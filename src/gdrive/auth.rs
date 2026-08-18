use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ImouError, Result};
use crate::token_cache;

use super::GDriveClient;

const TOKEN_CACHE_PATH: &str = ".gdrive_token_cache.json";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Unlike Imou's per-client access-token cache (client.rs), this also
/// carries the long-lived `refresh_token` obtained once via
/// `device_flow::run_login` — there's no equivalent "re-fetch
/// automatically" path for a user OAuth flow, so losing this file means the
/// user has to redo the manual OAuth consent (hence it must be
/// bind-mounted in the Docker deploy — see CLAUDE.md/deploy/README.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GDriveTokenCache {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub expires_at: Option<u64>,
}

fn cache_path() -> &'static Path {
    Path::new(TOKEN_CACHE_PATH)
}

pub(crate) fn load_cache() -> Option<GDriveTokenCache> {
    token_cache::load(cache_path())
}

pub(crate) fn save_cache(cache: &GDriveTokenCache) -> Result<()> {
    token_cache::save(cache_path(), cache)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Returns a valid access token, refreshing it via the cached refresh token
/// if missing/expired. Errors clearly if no login has ever been done.
pub async fn get_access_token(client: &GDriveClient) -> Result<String> {
    let mut cache = load_cache().ok_or_else(|| {
        ImouError::GDrive("not logged in to Google Drive — run `imou gdrive-login` first".into())
    })?;

    if let (Some(token), Some(expires_at)) = (&cache.access_token, cache.expires_at)
        && now_secs() + 60 < expires_at
    {
        return Ok(token.clone());
    }

    #[derive(Deserialize)]
    struct RefreshResponse {
        access_token: String,
        expires_in: u64,
    }

    let resp = client
        .http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client.config.client_id.as_str()),
            ("client_secret", client.config.client_secret.as_str()),
            ("refresh_token", cache.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ImouError::GDrive(format!("token refresh failed: {body}")));
    }

    let data: RefreshResponse = resp.json().await?;
    let expires_at = now_secs() + data.expires_in;

    cache.access_token = Some(data.access_token.clone());
    cache.expires_at = Some(expires_at);
    save_cache(&cache)?;

    Ok(data.access_token)
}
