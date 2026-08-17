use md5::{Digest, Md5};

/// One `system` block worth of auth material: every Imou API call (not just
/// accessToken acquisition) is signed the same way, from `time`/`nonce`/`appSecret`
/// alone — the signature does not cover `params`, so a fresh nonce+time pair must
/// be generated per request even when replaying the same params.
pub struct Signature {
    pub time: i64,
    pub nonce: String,
    pub sign: String,
}

pub fn sign_request(app_secret: &str) -> Signature {
    let time = chrono::Utc::now().timestamp();
    let nonce = uuid::Uuid::new_v4().to_string();

    let raw = format!("time:{time},nonce:{nonce},appSecret:{app_secret}");
    let digest = Md5::digest(raw.as_bytes());
    let sign = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();

    Signature { time, nonce, sign }
}
