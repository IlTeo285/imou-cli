use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ImouError, Result};

use super::GDriveClient;

const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

#[derive(Deserialize)]
struct FileListResponse {
    files: Vec<FileRef>,
}

#[derive(Deserialize)]
struct FileRef {
    id: String,
}

/// Drive's query language uses single-quoted string literals; a literal `'`
/// inside one must be backslash-escaped (see
/// https://developers.google.com/drive/api/guides/ref-search-terms). Our
/// own inputs (a `YYYY-MM-DD` name, an env-configured folder id) never
/// contain one, but escaping defensively costs nothing.
fn escape_query_value(value: &str) -> String {
    value.replace('\'', "\\'")
}

/// Returns the Drive folder id for `date` (named `YYYY-MM-DD`) directly
/// under `client`'s configured root folder (or Drive root if unset),
/// creating it if it doesn't exist yet. Cached in-memory on `client` so
/// repeat uploads on the same day don't re-list/re-create — the cache also
/// doubles as a lock (held across the whole check-then-create sequence) so
/// two clips finishing at nearly the same moment can't race into creating
/// two folders for the same day.
pub(crate) async fn ensure_day_folder(client: &GDriveClient, access_token: &str, date: NaiveDate) -> Result<String> {
    let name = date.format("%Y-%m-%d").to_string();

    let mut cache = client.folder_cache.lock().await;
    if let Some(id) = cache.get(&name) {
        return Ok(id.clone());
    }

    let parent = client.config.folder_id.as_deref();
    let id = find_or_create_folder(&client.http, access_token, &name, parent).await?;
    cache.insert(name, id.clone());
    Ok(id)
}

async fn find_or_create_folder(
    http: &reqwest::Client,
    access_token: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<String> {
    let mut query = format!(
        "name = '{}' and mimeType = '{FOLDER_MIME}' and trashed = false",
        escape_query_value(name)
    );
    if let Some(parent) = parent {
        query.push_str(&format!(" and '{}' in parents", escape_query_value(parent)));
    }

    let list_resp = http
        .get(FILES_URL)
        .bearer_auth(access_token)
        .query(&[("q", query.as_str()), ("fields", "files(id)"), ("spaces", "drive")])
        .send()
        .await?;

    if !list_resp.status().is_success() {
        let body = list_resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("failed to list Drive folders: {body}")));
    }

    let listed: FileListResponse = list_resp.json().await?;
    if let Some(existing) = listed.files.into_iter().next() {
        return Ok(existing.id);
    }

    let mut metadata = json!({ "name": name, "mimeType": FOLDER_MIME });
    if let Some(parent) = parent {
        metadata["parents"] = json!([parent]);
    }

    let create_resp = http
        .post(FILES_URL)
        .bearer_auth(access_token)
        .json(&metadata)
        .send()
        .await?;

    if !create_resp.status().is_success() {
        let body = create_resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("failed to create Drive folder '{name}': {body}")));
    }

    let created: FileRef = create_resp.json().await?;
    Ok(created.id)
}
