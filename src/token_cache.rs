use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

use crate::error::Result;

pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    std::fs::write(path, data)?;
    Ok(())
}
