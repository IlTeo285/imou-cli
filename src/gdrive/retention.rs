use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::NaiveDate;
use serde::Deserialize;

use crate::error::{ImouError, Result};

use super::{auth, GDriveClient};

const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(24 * 60 * 60);

fn escape_query_value(value: &str) -> String {
    value.replace('\'', "\\'")
}

#[derive(Deserialize)]
struct FileListResponse {
    files: Vec<FileRef>,
}

#[derive(Deserialize)]
struct FileRef {
    id: String,
    name: String,
}

/// Lists the day folders directly under `client`'s configured root (or
/// Drive root if unset) and permanently deletes (not trashes — same
/// semantics as `recorder::cleanup_old_segments` for local segments) any
/// whose name parses as `YYYY-MM-DD` older than `retention_days`. Deleting
/// a folder via the Drive API cascades to everything inside it that has no
/// other parent, so this is enough to reclaim the clips too — no need to
/// list/delete files individually. Folders that don't match the exact date
/// format are left alone, so this only ever touches folders this app could
/// plausibly have created itself.
async fn sweep_once(client: &GDriveClient, access_token: &str, retention_days: u32) -> Result<()> {
    let parent = client.config.folder_id.clone().unwrap_or_else(|| "root".to_string());
    let query = format!(
        "'{}' in parents and mimeType = '{FOLDER_MIME}' and trashed = false",
        escape_query_value(&parent)
    );

    let list_resp = client
        .http
        .get(FILES_URL)
        .bearer_auth(access_token)
        .query(&[("q", query.as_str()), ("fields", "files(id,name)"), ("spaces", "drive")])
        .send()
        .await?;

    if !list_resp.status().is_success() {
        let body = list_resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("failed to list Drive folders for retention sweep: {body}")));
    }

    let listed: FileListResponse = list_resp.json().await?;
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(retention_days as i64);

    for folder in listed.files {
        let Ok(date) = NaiveDate::parse_from_str(&folder.name, "%Y-%m-%d") else {
            continue;
        };
        if date >= cutoff {
            continue;
        }

        let del_resp = client
            .http
            .delete(format!("{FILES_URL}/{}", folder.id))
            .bearer_auth(access_token)
            .send()
            .await?;

        if del_resp.status().is_success() {
            println!("Google Drive: deleted clip folder {} (older than {retention_days}d)", folder.name);
        } else {
            let body = del_resp.text().await.unwrap_or_default();
            eprintln!("warning: failed to delete old Drive folder {}: {body}", folder.name);
        }
    }

    Ok(())
}

/// Runs `sweep_once` immediately, then every 24h, for as long as the
/// process runs. Detached: unlike the ring-buffer recorders
/// (`recorder::start_ring_buffer`), nothing here manages an OS resource, so
/// there's no orphan risk in just letting tokio drop this task at process
/// exit — no shutdown signal needed. `retention_days == 0` disables the
/// sweep entirely (kept forever).
pub fn start_retention_sweep(client: Arc<GDriveClient>, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            match auth::get_access_token(&client).await {
                Ok(token) => {
                    if let Err(e) = sweep_once(&client, &token, retention_days).await {
                        eprintln!("warning: Google Drive retention sweep failed: {e}");
                    }
                }
                Err(e) => eprintln!("warning: Google Drive retention sweep skipped (auth failed): {e}"),
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    });
}
