//! Timestamped per-session log of what SCRCPY Studio saw on the phone and did
//! about it. Each line is written straight to the file, so it can be read
//! while the session runs (scrcpy's own output is buffered until it exits).

use chrono::Local;
use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct EventLog(Option<Arc<Mutex<File>>>);

impl EventLog {
    /// Logging is best effort: a log that cannot be created is silently off.
    pub(crate) fn create(path: &Path) -> Self {
        Self(
            File::create(path)
                .ok()
                .map(|file| Arc::new(Mutex::new(file))),
        )
    }

    pub(crate) fn write(&self, message: impl AsRef<str>) {
        let Some(file) = &self.0 else { return };
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(
                file,
                "{} {}",
                Local::now().format("%H:%M:%S%.3f"),
                message.as_ref()
            );
        }
    }
}
