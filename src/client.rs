use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{ImouError, Result};
use crate::signing::sign_request;
use crate::token_cache::{self, CachedToken};

pub struct ImouClient {
    http: reqwest::Client,
    config: Config,
}

#[derive(serde::Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct Envelope<T> {
    result: EnvelopeResult<T>,
}

#[derive(serde::Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct EnvelopeResult<T> {
    code: String,
    msg: String,
    #[serde(default = "Option::default")]
    data: Option<T>,
}

impl ImouClient {
    pub fn new(config: Config) -> Self {
        Self {
            // No default timeout on reqwest::Client — a stalled connection
            // would hang forever, freezing `watch`'s sequential poll loop on
            // whichever channel it stuck on (observed live: the process
            // stayed alive but stopped making progress entirely).
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("reqwest client with a timeout should always build"),
            config,
        }
    }

    /// Calls an unauthenticated method (currently only `accessToken`), signing
    /// the request with appId/appSecret but no token in `params`.
    pub async fn call_unauthenticated<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let (_, data) = self.dispatch(method, json!(params)).await?;
        data.ok_or_else(|| ImouError::Api {
            code: "0".into(),
            msg: "response had no data payload".into(),
        })
    }

    /// Calls an authenticated method that returns a `data` payload: fetches/
    /// refreshes the cached access token, then merges `token` into whatever
    /// params the caller supplied (per deviceBaseList.html, the token lives
    /// in `params`, not `system`).
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let params_value = self.with_token(params).await?;
        let (_, data) = self.dispatch::<R>(method, params_value).await?;
        data.ok_or_else(|| ImouError::Api {
            code: "0".into(),
            msg: "response had no data payload".into(),
        })
    }

    /// Calls an authenticated method that returns no `data` payload (e.g.
    /// PTZ control) — success is just `result.code == "0"`.
    pub async fn call_action<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        let params_value = self.with_token(params).await?;
        self.dispatch::<Value>(method, params_value).await?;
        Ok(())
    }

    async fn with_token<P: Serialize>(&self, params: P) -> Result<Value> {
        let token = self.get_access_token().await?;
        let mut params_value = serde_json::to_value(params)?;
        if let Value::Object(ref mut map) = params_value {
            map.insert("token".into(), json!(token));
        }
        Ok(params_value)
    }

    async fn dispatch<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(String, Option<R>)> {
        let sig = sign_request(&self.config.app_secret);
        let body = json!({
            "system": {
                "ver": "1.0",
                "appId": self.config.app_id,
                "sign": sig.sign,
                "time": sig.time,
                "nonce": sig.nonce,
            },
            "id": uuid::Uuid::new_v4().to_string(),
            "params": params,
        });

        let url = format!("{}/{}", self.config.data_center.base_url(), method);
        let resp: Envelope<R> = self
            .http
            .post(url)
            .header("Content-Type", "application/json;charset=UTF-8")
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.result.code != "0" {
            return Err(ImouError::Api {
                code: resp.result.code,
                msg: resp.result.msg,
            });
        }

        Ok((resp.result.msg, resp.result.data))
    }

    pub fn app_id(&self) -> &str {
        &self.config.app_id
    }

    /// Returns the cached access token, fetching a fresh one if missing/expired.
    pub async fn access_token(&self) -> Result<String> {
        self.get_access_token().await
    }

    async fn get_access_token(&self) -> Result<String> {
        if let Some(cached) = token_cache::load()
            && cached.is_valid()
        {
            return Ok(cached.access_token);
        }

        #[derive(serde::Deserialize)]
        struct TokenData {
            #[serde(rename = "accessToken")]
            access_token: String,
            #[serde(rename = "expireTime")]
            expire_time: u64,
        }

        let data: TokenData = self
            .call_unauthenticated("accessToken", json!({}))
            .await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let cached = CachedToken {
            access_token: data.access_token.clone(),
            expires_at: now + data.expire_time,
        };
        token_cache::save(&cached)?;

        Ok(data.access_token)
    }
}
