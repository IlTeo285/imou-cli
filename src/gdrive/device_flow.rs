use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::{ImouError, Result};

use super::auth::{save_cache, GDriveTokenCache};
use super::{GDriveClient, GDriveConfig};

const DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// `drive.file` scopes access to only the files this app creates/opens —
/// deliberately narrower than full Drive access.
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One-time OAuth setup for a headless deployment: Google's device-code
/// flow needs no local redirect URI or browser on this machine — the user
/// opens `verification_url` and enters `user_code` on any other device
/// (phone, laptop), while this process polls for authorization. On success,
/// the refresh token is persisted so this only has to be done once (unless
/// the token cache file is lost — see CLAUDE.md's note on why it must be
/// bind-mounted in the Docker deploy). Uploads made with the resulting
/// token count against the authorizing user's own Drive storage quota —
/// unlike a service account, which Google rejects for writes into a
/// personal "My Drive" folder (`storageQuotaExceeded`, confirmed live).
pub async fn run_login(config: &GDriveConfig) -> Result<()> {
    let client = GDriveClient::new(config.clone());

    let device: DeviceCodeResponse = client
        .http
        .post(DEVICE_CODE_URL)
        .form(&[("client_id", config.client_id.as_str()), ("scope", SCOPE)])
        .send()
        .await?
        .json()
        .await?;

    println!(
        "Go to {} and enter this code: {}",
        device.verification_url, device.user_code
    );
    println!("Waiting for authorization...");

    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));

    loop {
        if Instant::now() >= deadline {
            return Err(ImouError::GDrive(
                "device code expired before authorization completed".into(),
            ));
        }
        tokio::time::sleep(interval).await;

        let resp = client
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("client_secret", config.client_secret.as_str()),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        if resp.status().is_success() {
            let token: TokenResponse = resp.json().await?;
            let refresh_token = token.refresh_token.ok_or_else(|| {
                ImouError::GDrive(
                    "Google did not return a refresh_token — revoke this app's prior access at \
                     https://myaccount.google.com/permissions and retry (Google only returns a \
                     refresh_token on the first consent for a given client/account pair)"
                        .into(),
                )
            })?;
            let expires_at = now_secs() + token.expires_in;
            save_cache(&GDriveTokenCache {
                refresh_token,
                access_token: Some(token.access_token),
                expires_at: Some(expires_at),
            })?;
            println!("Login complete — token cached.");
            return Ok(());
        }

        let body: TokenErrorResponse = resp
            .json()
            .await
            .map_err(|_| ImouError::GDrive("unexpected error response during device authorization".into()))?;

        match body.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => interval += Duration::from_secs(5),
            other => return Err(ImouError::GDrive(format!("device authorization failed: {other}"))),
        }
    }
}
