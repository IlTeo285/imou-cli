use crate::error::{ImouError, Result};

/// Imou Open Platform data centers. The appId/appSecret pair is tied to the
/// data center it was registered in (see https://open.imoulife.com/book/http/develop.html) —
/// calling the wrong region returns an auth error even with valid credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataCenter {
    /// East Asia / Singapore
    Singapore,
    /// Central Europe / Frankfurt
    Frankfurt,
    /// Western America / Oregon
    Oregon,
}

impl DataCenter {
    pub fn base_url(self) -> &'static str {
        match self {
            DataCenter::Singapore => "https://openapi-sg.easy4ip.com:443/openapi",
            DataCenter::Frankfurt => "https://openapi-fk.easy4ip.com:443/openapi",
            DataCenter::Oregon => "https://openapi-or.easy4ip.com:443/openapi",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sg" | "singapore" | "east-asia" => Ok(DataCenter::Singapore),
            "fk" | "frankfurt" | "central-europe" => Ok(DataCenter::Frankfurt),
            "or" | "oregon" | "western-america" => Ok(DataCenter::Oregon),
            other => Err(ImouError::Config(format!(
                "unknown DATA_CENTER '{other}', expected one of: sg, fk, or"
            ))),
        }
    }
}

pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub data_center: DataCenter,
}

impl Config {
    /// Loads credentials from the environment (populated from `.env` by `dotenvy` in `main`).
    pub fn from_env() -> Result<Self> {
        let app_id = std::env::var("APP_ID")
            .map_err(|_| ImouError::Config("APP_ID is not set in .env".into()))?;
        let app_secret = std::env::var("APP_SECRET")
            .map_err(|_| ImouError::Config("APP_SECRET is not set in .env".into()))?;
        let data_center = std::env::var("DATA_CENTER").map_err(|_| {
            ImouError::Config(
                "DATA_CENTER is not set in .env (use one of: sg, fk, or)".into(),
            )
        })?;
        let data_center = DataCenter::parse(&data_center)?;

        Ok(Config {
            app_id,
            app_secret,
            data_center,
        })
    }
}
