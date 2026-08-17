use serde::Serialize;

use crate::client::ImouClient;
use crate::error::Result;

#[derive(Serialize)]
struct SetMessageCallbackParams<'a> {
    #[serde(rename = "callbackUrl")]
    callback_url: &'a str,
    #[serde(rename = "callbackFlag")]
    callback_flag: &'a str,
    #[serde(rename = "basePush")]
    base_push: &'a str,
    status: &'a str,
}

/// Registers (or unregisters) the account-wide push callback URL. Imou
/// requires this to be an HTTPS, publicly reachable address — see
/// https://open.imoulife.com/book/http/push/setMessageCallback.html
///
/// Verified live in this project that `OP1003` rejections during
/// registration were caused by the `callbackUrl` itself (an unreachable/
/// throwaway domain), not `basePush` — both `"1"` and `"2"` were accepted
/// for registration in testing. Uses `"1"` (push enabled) per the doc's
/// stated meaning; whether this field has any real effect on delivery is
/// unconfirmed either way (the one registration that did successfully
/// deliver a real push was done manually, outside this code, so its
/// `basePush` value was never captured for comparison).
///
/// `status` is `"on"` or `"off"`. `callback_flag` is a comma-separated list
/// (`"alarm"`, `"deviceStatus"`, `"numberstat"`, `"faceAnalysis"`) — only
/// used when `status` is `"on"`.
pub async fn set_callback(
    client: &ImouClient,
    callback_url: &str,
    callback_flag: &str,
    status: &str,
) -> Result<()> {
    client
        .call_action(
            "setMessageCallback",
            SetMessageCallbackParams {
                callback_url,
                callback_flag,
                base_push: "1",
                status,
            },
        )
        .await
}
