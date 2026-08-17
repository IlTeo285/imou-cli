use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Result;

/// Access tokens are valid for ~3 days (see accessToken.html), so we persist
/// the token to disk between CLI invocations instead of fetching a new one
/// on every run — repeated fetches are wasteful and count against API quota.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedToken {
    pub access_token: String,
    /// Unix timestamp (seconds) after which the token should be treated as expired.
    pub expires_at: u64,
}

impl CachedToken {
    pub fn is_valid(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Refresh a bit early to avoid racing expiry mid-request.
        now + 60 < self.expires_at
    }
}

fn cache_path() -> PathBuf {
    PathBuf::from(".imou_token_cache.json")
}

pub fn load() -> Option<CachedToken> {
    let data = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save(token: &CachedToken) -> Result<()> {
    let data = serde_json::to_string_pretty(token)?;
    std::fs::write(cache_path(), data)?;
    Ok(())
}
