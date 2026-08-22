mod auth;
mod device_flow;
mod folders;
mod retention;
mod upload;

use std::collections::HashMap;
use std::path::Path;

use chrono::NaiveDate;
use tokio::sync::Mutex;

use crate::error::{ImouError, Result};

pub use device_flow::run_login;
pub use retention::start_retention_sweep;

/// OAuth client credentials (from a "TVs and Limited Input devices" OAuth
/// client, needed for the device-code login flow — see
/// `device_flow::run_login`) plus an optional target Drive folder.
/// Presence of `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` in the environment
/// is what gates the whole feature — same "optional, no-op if unset"
/// convention as `recorder::local_config_for` for per-camera RTSP config.
///
/// A plain service account was tried first and rejected live by the real
/// Drive API (`storageQuotaExceeded` — "Service Accounts do not have
/// storage quota. Leverage shared drives, or use OAuth delegation
/// instead."): a service account can only write into a Shared Drive
/// (Google Workspace only) or via domain-wide delegation (also Workspace
/// only), neither available for a personal Google account's regular "My
/// Drive" folder. Real user OAuth (this flow) uploads against the user's
/// own normal storage quota instead, so it works on any personal account.
#[derive(Clone)]
pub struct GDriveConfig {
    pub client_id: String,
    pub client_secret: String,
    pub folder_id: Option<String>,
}

/// Reads `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` (both required together)
/// and optional `GDRIVE_FOLDER_ID` from the environment. Returns `None` if
/// the required pair is missing — callers should treat that as "Drive
/// upload not configured," not as an error.
pub fn config_from_env() -> Option<GDriveConfig> {
    let client_id = std::env::var("GDRIVE_CLIENT_ID").ok()?;
    let client_secret = std::env::var("GDRIVE_CLIENT_SECRET").ok()?;
    let folder_id = std::env::var("GDRIVE_FOLDER_ID").ok();
    Some(GDriveConfig { client_id, client_secret, folder_id })
}

pub struct GDriveClient {
    pub(crate) http: reqwest::Client,
    pub(crate) config: GDriveConfig,
    /// Maps a `YYYY-MM-DD` day-folder name to its Drive file id — see
    /// `folders::ensure_day_folder`. In-memory only: losing it on restart
    /// just costs one extra list-or-create call per day, not correctness.
    pub(crate) folder_cache: Mutex<HashMap<String, String>>,
}

impl GDriveClient {
    pub fn new(config: GDriveConfig) -> Self {
        Self {
            // Clips are tens of MB over a possibly slow home uplink — a
            // longer timeout than ImouClient's 20s (which only ever moves
            // small JSON payloads), but still an explicit one; see
            // CLAUDE.md's note on why `client.rs` never omits this.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .expect("reqwest client with a timeout should always build"),
            config,
            folder_cache: Mutex::new(HashMap::new()),
        }
    }
}

/// Uploads `local_path` into the Drive folder for `date` (`YYYY-MM-DD`,
/// created under the configured root if it doesn't exist yet — see
/// `folders::ensure_day_folder`). Does not touch the local file either way
/// — local and Drive copies now have independent retention windows (see
/// `recorder::start_local_retention_sweep` vs `retention::start_retention_sweep`),
/// so deleting it here on success would cut the local copy's lifetime short.
pub async fn upload_clip(
    client: &GDriveClient,
    local_path: &Path,
    channel_name: &str,
    date: NaiveDate,
) -> Result<()> {
    let access_token = auth::get_access_token(client).await?;
    let day_folder_id = folders::ensure_day_folder(client, &access_token, date).await?;

    let drive_filename = local_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("{channel_name}_{n}"))
        .ok_or_else(|| ImouError::GDrive(format!("invalid clip path: {}", local_path.display())))?;

    upload::upload_file(&client.http, &access_token, Some(&day_folder_id), local_path, &drive_filename).await?;

    Ok(())
}
