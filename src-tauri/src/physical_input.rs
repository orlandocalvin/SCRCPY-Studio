//! Notices when someone uses the phone itself (power key, fingerprint sensor,
//! double-tap or touch) by streaming `getevent`. Kernel input events only come
//! from hardware, so input injected by scrcpy or adb never shows up here.

use crate::{commands::hidden_command, runtime::adb_path};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Stdio},
    thread,
};

#[derive(Debug)]
pub(crate) struct PhysicalInputWatch {
    child: Child,
}

impl PhysicalInputWatch {
    /// Calls `on_input` with each `getevent` line that shows someone using the
    /// phone. `-tt` runs getevent on a pseudo-terminal: writing to a pipe, it
    /// buffers its output and events would arrive late or not at all.
    pub(crate) fn start(
        serial: &str,
        on_input: impl Fn(&str) + Send + 'static,
    ) -> Result<Self, String> {
        let mut child = hidden_command(adb_path()?)
            .args(["-s", serial, "shell", "-tt", "getevent", "-lq"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "getevent produced no output stream.".to_string())?;
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let line = line.trim();
                if is_user_input(line) {
                    on_input(line);
                }
            }
        });
        Ok(Self { child })
    }

    /// The stream stops when adb loses the phone (for example a Wi-Fi drop).
    pub(crate) fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PhysicalInputWatch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Keys that do not mean someone wants the phone screen.
const IGNORED_KEYS: &[&str] = &[
    "KEY_VOLUMEUP",
    "KEY_VOLUMEDOWN",
    "KEY_MUTE",
    "KEY_MEDIA",
    "KEY_PLAYPAUSE",
    "KEY_NEXTSONG",
    "KEY_PREVIOUSSONG",
    "KEY_VOICECOMMAND",
];

/// A `getevent -lq` line that shows someone reaching for the phone: any key
/// press except volume and headset keys (fingerprint sensors report arbitrary
/// codes, such as KEY_RIGHT on HyperOS), or a finger touching the screen.
fn is_user_input(line: &str) -> bool {
    let mut fields = line.split_whitespace().skip(1);
    let (Some(kind), Some(code), Some(value)) = (fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    match kind {
        "EV_KEY" => {
            let headset_button = code.starts_with("BTN_") && code != "BTN_TOUCH";
            value == "DOWN" && !headset_button && !IGNORED_KEYS.contains(&code)
        }
        "EV_ABS" => code == "ABS_MT_TRACKING_ID" && value != "ffffffff",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_power_fingerprint_and_touch() {
        for line in [
            "/dev/input/event1: EV_KEY       KEY_POWER            DOWN",
            "/dev/input/event4: EV_KEY       KEY_RIGHT            DOWN",
            "/dev/input/event3: EV_KEY       KEY_WAKEUP           DOWN",
            "/dev/input/event3: EV_KEY       BTN_TOUCH            DOWN",
            "/dev/input/event4: EV_KEY       0000009c             DOWN",
            "/dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   00000012",
        ] {
            assert!(is_user_input(line), "{line}");
        }
    }

    #[test]
    fn ignores_releases_lifts_volume_and_headset() {
        for line in [
            "/dev/input/event1: EV_KEY       KEY_POWER            UP",
            "/dev/input/event3: EV_ABS       ABS_MT_TRACKING_ID   ffffffff",
            "/dev/input/event3: EV_ABS       ABS_MT_POSITION_X    000010a2",
            "/dev/input/event1: EV_KEY       KEY_VOLUMEUP         DOWN",
            "/dev/input/event0: EV_KEY       BTN_1                DOWN",
            "/dev/input/event3: EV_SYN       SYN_REPORT           00000000",
            "",
        ] {
            assert!(!is_user_input(line), "{line}");
        }
    }
}
