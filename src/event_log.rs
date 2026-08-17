use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

use crate::error::Result;

/// Appends one JSON object per line to a file, flushing after every write.
/// Motion events are rare and important enough that durability beats
/// batching — no buffering that could lose an event on a crash.
pub struct EventLog {
    file: Mutex<File>,
}

impl EventLog {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn append(&self, value: &impl Serialize) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');

        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }
}
