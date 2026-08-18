use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::error::{ImouError, Result};

const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable";

#[derive(Deserialize)]
struct UploadedFile {
    id: String,
}

/// Uploads `local_path` to Google Drive using the resumable upload
/// protocol: a metadata-only POST to open a session, then a single PUT of
/// the whole file to the returned session URI. Preferred over a one-shot
/// multipart upload because it's what Google recommends for anything
/// beyond trivially small files, and it's more resilient to a flaky home
/// uplink (the session URI can in principle be resumed after a dropped
/// connection, even though this v1 doesn't yet implement resuming a partial
/// PUT — see CLAUDE.md).
pub async fn upload_file(
    http: &reqwest::Client,
    access_token: &str,
    folder_id: Option<&str>,
    local_path: &Path,
    drive_filename: &str,
) -> Result<String> {
    let mut metadata = json!({ "name": drive_filename });
    if let Some(folder_id) = folder_id {
        metadata["parents"] = json!([folder_id]);
    }

    let init_resp = http
        .post(UPLOAD_URL)
        .bearer_auth(access_token)
        .json(&metadata)
        .send()
        .await?;

    if !init_resp.status().is_success() {
        let body = init_resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("failed to start upload session: {body}")));
    }

    let session_url = init_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| ImouError::GDrive("upload session response missing Location header".into()))?;

    // Clips are tens of MB — buffering the whole file is a deliberate
    // simplification for v1, not a streaming upload; revisit if clip sizes
    // grow substantially.
    let bytes = tokio::fs::read(local_path).await?;

    let put_resp = http
        .put(&session_url)
        .header("Content-Type", "video/mp4")
        .body(bytes)
        .send()
        .await?;

    if !put_resp.status().is_success() {
        let body = put_resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("upload failed: {body}")));
    }

    let uploaded: UploadedFile = put_resp.json().await?;
    Ok(uploaded.id)
}
