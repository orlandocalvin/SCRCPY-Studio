use crate::{
    creator::recordings_root,
    desktop::{app_data_dir, compact_error, launch_desktop_and_watch},
    devices::{is_wireless_serial, list_devices},
    event_log::EventLog,
    models::{DesktopDiagnostics, LaunchConfig, LaunchResult, SessionStatus},
    physical_input::PhysicalInputWatch,
    preferences::remember_successful_profile,
    runtime::{adb_path, scrcpy_command, scrcpy_path},
};
use chrono::Local;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
enum SettingBackup {
    Missing,
    Value(String),
}

#[derive(Debug)]
struct ManagedSession {
    config: LaunchConfig,
    show_touches_backup: Option<SettingBackup>,
    scrcpy_manages_show_touches: bool,
    started_at: Instant,
    power: PowerWatch,
    phone: Arc<PhoneControl>,
    /// Notices hands on the phone while mirroring; see `physical_input`.
    input_watch: Option<PhysicalInputWatch>,
    /// F11 is handled asynchronously by scrcpy; do not read the window state
    /// back before it has had time to switch.
    fullscreen_settles_at: Instant,
    /// The phone screen may briefly be on while scrcpy starts or while a
    /// screen-off change is applied; do not read that as the user's doing.
    panel_settles_at: Instant,
}

/// How often the phone state is read while mirroring.
const POWER_CHECK_INTERVAL: Duration = Duration::from_secs(3);
/// scrcpy turns the phone on itself when a session starts.
const PANEL_SETTLE: Duration = Duration::from_secs(5);
const SCREEN_TURNED_ON_NOTICE: &str = "Phone screen was turned on from the phone.";
/// One adb round trip for everything the watchdog needs. `; true` keeps the
/// exit status successful when a grep finds nothing.
const PHONE_STATE_QUERY: &str =
    "dumpsys power | grep -E 'mWakefulness=|mLastSleepTime=|mLastSleepReason='; \
    dumpsys SurfaceFlinger 2>/dev/null | grep -m1 powerMode; \
    dumpsys window 2>/dev/null | grep -m1 isKeyguardShowing; true";

/// What the phone reports about itself. `screen_on` is the panel power mode
/// from SurfaceFlinger: unlike `dumpsys power`, it also sees a panel that
/// scrcpy switched off while the phone stays awake.
#[derive(Debug, Clone, Default, PartialEq)]
struct PhoneState {
    awake: bool,
    last_sleep_time: Option<u64>,
    last_sleep_reason: Option<String>,
    screen_on: Option<bool>,
    locked: bool,
}

fn parse_phone_state(text: &str) -> Option<PhoneState> {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(str::trim)
    };
    let wakefulness = value("mWakefulness=")?;
    Some(PhoneState {
        awake: matches!(wakefulness, "Awake" | "Dreaming"),
        last_sleep_time: value("mLastSleepTime=")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok()),
        last_sleep_reason: value("mLastSleepReason=").map(str::to_string),
        screen_on: value("powerMode=").map(|mode| mode == "On"),
        locked: value("isKeyguardShowing=") == Some("true"),
    })
}

#[derive(Debug, Default)]
struct PowerWatch {
    checked_at: Option<Instant>,
    previous: Option<PhoneState>,
    asleep: bool,
    screen_on: Option<bool>,
    locked: bool,
    notice: Option<String>,
}

impl PowerWatch {
    /// Updates the known phone state. Returns true when the phone screen is
    /// kept off but its power button was pressed, or the screen came back on
    /// some other way once `settled`. Hands on the phone are usually noticed
    /// sooner by `PhysicalInputWatch`; this is the fallback.
    fn observe(&mut self, state: Option<PhoneState>, screen_off: bool, settled: bool) -> bool {
        let mut slept = false;
        if let Some(state) = state {
            self.asleep = !state.awake;
            self.screen_on = state.screen_on;
            self.locked = state.locked;
            slept = self
                .previous
                .replace(state.clone())
                .is_some_and(|previous| previous.last_sleep_time != state.last_sleep_time);
        }
        let reason = self
            .previous
            .as_ref()
            .and_then(|state| state.last_sleep_reason.clone())
            .unwrap_or_else(|| "unknown".into());
        let screen_came_on = settled && !self.asleep && self.screen_on == Some(true);

        if screen_off && ((slept && reason == "power_button") || screen_came_on) {
            self.notice = Some(SCREEN_TURNED_ON_NOTICE.into());
            return true;
        }
        if slept && self.asleep {
            self.notice = Some(format!(
                "The phone went to sleep ({reason}). Use Wake Phone to continue mirroring."
            ));
        }
        false
    }
}

/// Screen-off state shared between the session and the threads that watch the
/// phone. `keep_screen_off` is the source of truth; the session config follows it.
#[derive(Debug)]
struct PhoneControl {
    serial: String,
    keep_screen_off: AtomicBool,
    power_presses: AtomicU64,
    restoring: AtomicBool,
    events: EventLog,
}

impl PhoneControl {
    /// Called for each physical input on the phone (see `PhysicalInputWatch`).
    fn on_physical_input(self: &Arc<Self>, line: &str) {
        let power_key = line.contains("KEY_POWER");
        if power_key {
            self.power_presses.fetch_add(1, Ordering::SeqCst);
        }
        if line.contains("EV_KEY") {
            self.events.write(format!("phone key: {line}"));
        }
        if self.keep_screen_off.swap(false, Ordering::SeqCst) {
            self.events.write(format!(
                "hands on the phone ({line}); turning its screen on"
            ));
            self.turn_screen_on(power_key);
        }
    }

    /// Turns the phone screen back on in the background.
    ///
    /// MOD+Shift+o makes scrcpy power the panel on and stop keeping it off,
    /// but on some phones (seen on HyperOS) the backlight stays dark, so the
    /// backlight is relit too. A power key press also puts an awake phone to
    /// sleep; waking it then lights the screen through Android itself, unless
    /// the user pressed the key again meanwhile.
    fn turn_screen_on(self: &Arc<Self>, after_power_key: bool) {
        if self.restoring.swap(true, Ordering::SeqCst) {
            return;
        }
        let phone = self.clone();
        thread::spawn(move || {
            let presses = phone.power_presses.load(Ordering::SeqCst);
            phone.send_screen_on_shortcut();
            let mut left_to_user = false;
            if after_power_key {
                thread::sleep(Duration::from_millis(600));
                if phone.power_presses.load(Ordering::SeqCst) == presses {
                    let woke = adb_shell(&phone.serial, &["input", "keyevent", "KEYCODE_WAKEUP"]);
                    phone.events.write(format!("sent KEYCODE_WAKEUP: {woke:?}"));
                } else {
                    left_to_user = true;
                    phone
                        .events
                        .write("power key pressed again; leaving the phone as the user set it");
                }
            }
            if !left_to_user {
                relight_backlight(&phone.serial, &phone.events);
            }
            let state = adb_shell(&phone.serial, &[PHONE_STATE_QUERY])
                .ok()
                .and_then(|text| parse_phone_state(&text));
            phone
                .events
                .write(format!("phone after turning its screen on: {state:?}"));
            phone.restoring.store(false, Ordering::SeqCst);
        });
    }

    fn send_screen_on_shortcut(&self) {
        let sent = native_window::press_mod_key(&self.serial, 'o', true);
        self.events.write(format!("sent MOD+Shift+o: {sent:?}"));
    }
}

/// scrcpy can power the panel back on without its backlight on some phones
/// (seen on HyperOS). Nudging the brightness setting makes Android write the
/// backlight again; the original values are put back straight away.
fn relight_backlight(serial: &str, events: &EventLog) {
    let setting = |key: &str| {
        adb_shell(serial, &["settings", "get", "system", key])
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
    };
    let (Some(mode), Some(level)) = (
        setting("screen_brightness_mode"),
        setting("screen_brightness"),
    ) else {
        events.write("could not read the brightness settings to relight the screen");
        return;
    };
    let nudged = if level >= 255 { level - 1 } else { level + 1 };
    let script = format!(
        "settings put system screen_brightness_mode 0; \
         settings put system screen_brightness {nudged}; sleep 1; \
         settings put system screen_brightness {level}; \
         settings put system screen_brightness_mode {mode}"
    );
    let result = adb_shell(serial, &[&script]);
    events.write(format!(
        "relit the backlight (brightness {level}, mode {mode}): {result:?}"
    ));
}

fn watches_phone_power(config: &LaunchConfig) -> bool {
    matches!(config.mode.as_str(), "creator" | "mirror")
}

fn status_of(session: &ManagedSession) -> SessionStatus {
    SessionStatus {
        active: true,
        serial: Some(session.config.serial.clone()),
        mode: Some(session.config.mode.clone()),
        applied_config: Some(session.config.clone()),
        phone_asleep: session.power.asleep,
        phone_locked: session.power.locked,
        notice: None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionManager {
    active: Mutex<Option<ManagedSession>>,
}

fn session_window_title(serial: &str) -> String {
    format!("SCRCPY Studio · {serial}")
}

#[cfg(target_os = "windows")]
mod native_window {
    use super::session_window_title;
    use std::{thread, time::Duration};

    const WM_CLOSE: u32 = 0x0010;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_SYSKEYDOWN: u32 = 0x0104;
    const WM_SYSKEYUP: u32 = 0x0105;
    const VK_MENU: usize = 0x12;
    const VK_SHIFT: usize = 0x10;
    const VK_F11: usize = 0x7a;

    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    struct WindowSearch {
        title: String,
        found: isize,
    }

    #[repr(C)]
    #[derive(Default, PartialEq)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(isize, isize) -> i32>,
            data: isize,
        ) -> i32;
        fn GetWindowTextLengthW(window: isize) -> i32;
        fn GetWindowTextW(window: isize, text: *mut u16, max_count: i32) -> i32;
        fn IsWindow(window: isize) -> i32;
        fn IsIconic(window: isize) -> i32;
        fn GetWindowRect(window: isize, rect: *mut Rect) -> i32;
        fn MonitorFromWindow(window: isize, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
        fn PostMessageW(window: isize, message: u32, wparam: usize, lparam: isize) -> i32;
        fn SetForegroundWindow(window: isize) -> i32;
    }

    unsafe extern "system" fn find_window_callback(window: isize, data: isize) -> i32 {
        let search = &mut *(data as *mut WindowSearch);
        let length = GetWindowTextLengthW(window);
        if length <= 0 {
            return 1;
        }
        let mut text = vec![0_u16; length as usize + 1];
        let copied = GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32);
        if copied > 0 && String::from_utf16_lossy(&text[..copied as usize]) == search.title {
            search.found = window;
            return 0;
        }
        1
    }

    fn find(serial: &str) -> Option<isize> {
        let mut search = WindowSearch {
            title: session_window_title(serial),
            found: 0,
        };
        unsafe {
            EnumWindows(
                Some(find_window_callback),
                &mut search as *mut WindowSearch as isize,
            );
        }
        (search.found != 0).then_some(search.found)
    }

    pub(super) fn exists(serial: &str) -> bool {
        find(serial)
            .map(|window| unsafe { IsWindow(window) != 0 })
            .unwrap_or(false)
    }

    pub(super) fn close(serial: &str) {
        let Some(window) = find(serial) else { return };
        unsafe {
            PostMessageW(window, WM_CLOSE, 0, 0);
        }
        for _ in 0..30 {
            if unsafe { IsWindow(window) } == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// scrcpy uses borderless desktop fullscreen, so a fullscreen window covers
    /// its whole monitor. None when the window is missing or minimized.
    pub(super) fn is_fullscreen(serial: &str) -> Option<bool> {
        let window = find(serial)?;
        unsafe {
            if IsIconic(window) != 0 {
                return None;
            }
            let mut rect = Rect::default();
            if GetWindowRect(window, &mut rect) == 0 {
                return None;
            }
            let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
            let mut info = MonitorInfo {
                size: std::mem::size_of::<MonitorInfo>() as u32,
                ..MonitorInfo::default()
            };
            if monitor == 0 || GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            Some(rect == info.monitor)
        }
    }

    pub(super) fn press_f11(serial: &str) -> Result<(), String> {
        let window =
            find(serial).ok_or_else(|| "The active scrcpy window was not found.".to_string())?;
        unsafe {
            SetForegroundWindow(window);
            PostMessageW(window, WM_KEYDOWN, VK_F11, 0);
            PostMessageW(window, WM_KEYUP, VK_F11, 0);
        }
        Ok(())
    }

    /// Posts MOD+key to the scrcpy window. SDL handles posted key messages
    /// without keyboard focus, so this works while the user is in another app
    /// (Windows would often refuse to bring the window to the front anyway).
    pub(super) fn press_mod_key(serial: &str, key: char, shift: bool) -> Result<(), String> {
        let window =
            find(serial).ok_or_else(|| "The active scrcpy window was not found.".to_string())?;
        let key = key.to_ascii_uppercase() as usize;
        unsafe {
            PostMessageW(window, WM_SYSKEYDOWN, VK_MENU, 0);
            if shift {
                PostMessageW(window, WM_KEYDOWN, VK_SHIFT, 0);
            }
            PostMessageW(window, WM_SYSKEYDOWN, key, 1 << 29);
            PostMessageW(window, WM_SYSKEYUP, key, 1 << 29);
            if shift {
                PostMessageW(window, WM_KEYUP, VK_SHIFT, 0);
            }
            PostMessageW(window, WM_SYSKEYUP, VK_MENU, 0);
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
mod native_window {
    pub(super) fn exists(_serial: &str) -> bool {
        true
    }
    pub(super) fn close(_serial: &str) {}
    pub(super) fn is_fullscreen(_serial: &str) -> Option<bool> {
        None
    }
    pub(super) fn press_f11(_serial: &str) -> Result<(), String> {
        Err("Live window controls are currently available on Windows only.".into())
    }
    pub(super) fn press_mod_key(_serial: &str, _key: char, _shift: bool) -> Result<(), String> {
        Err("Live window controls are currently available on Windows only.".into())
    }
}

fn adb_shell(serial: &str, args: &[&str]) -> Result<String, String> {
    let adb = adb_path()?;
    let output = crate::commands::hidden_command(adb)
        .arg("-s")
        .arg(serial)
        .arg("shell")
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn read_setting(serial: &str, namespace: &str, key: &str) -> Result<SettingBackup, String> {
    let value = adb_shell(serial, &["settings", "get", namespace, key])?;
    if value.is_empty() || value == "null" {
        Ok(SettingBackup::Missing)
    } else {
        Ok(SettingBackup::Value(value))
    }
}

fn write_setting(serial: &str, namespace: &str, key: &str, value: &str) -> Result<(), String> {
    adb_shell(serial, &["settings", "put", namespace, key, value]).map(|_| ())
}

fn restore_setting(serial: &str, namespace: &str, key: &str, backup: SettingBackup) {
    let args = match backup {
        SettingBackup::Missing => vec!["settings", "delete", namespace, key],
        SettingBackup::Value(ref value) => vec!["settings", "put", namespace, key, value],
    };
    let _ = adb_shell(serial, &args);
}

fn restore_live_settings(session: ManagedSession) {
    if let Some(backup) = session.show_touches_backup {
        restore_setting(&session.config.serial, "system", "show_touches", backup);
    }
    // scrcpy powers the panel back on when it exits, which can leave the
    // backlight dark (see `relight_backlight`).
    if session.phone.keep_screen_off.load(Ordering::SeqCst) {
        let phone = session.phone.clone();
        phone
            .events
            .write("session ended with the phone screen off");
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(1500));
            relight_backlight(&phone.serial, &phone.events);
        });
    }
}

pub(crate) fn stop_managed_session(manager: &SessionManager) {
    let session = manager
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take());
    if let Some(session) = session {
        native_window::close(&session.config.serial);
        restore_live_settings(session);
    }
}

fn recording_path() -> Result<PathBuf, String> {
    let folder = recordings_root()?.join(Local::now().format("%Y-%m-%d").to_string());
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    Ok(folder.join(format!(
        "SCRCPY-Studio-{}.mp4",
        Local::now().format("%H-%M-%S")
    )))
}

fn valid_camera_facing(value: &str) -> bool {
    matches!(value, "front" | "back" | "external")
}

fn validate_launch_mode(config: &LaunchConfig, requested_mode: &str) -> Result<(), String> {
    if !matches!(requested_mode, "mirror" | "creator" | "camera" | "desktop") {
        return Err("Unknown session mode.".into());
    }
    if config.mode != requested_mode {
        return Err(
            "The selected mode changed while its settings were loading. Wait a moment and try again."
                .into(),
        );
    }
    Ok(())
}

fn safe_desktop_dimension(value: Option<u32>, fallback: u32) -> u32 {
    value
        .filter(|value| (480..=7680).contains(value))
        .unwrap_or(fallback)
}

fn safe_desktop_density(value: Option<u32>) -> u32 {
    value
        .filter(|value| (120..=640).contains(value))
        .unwrap_or(240)
}

fn safe_video_codec(value: &str) -> &str {
    if matches!(value, "h264" | "h265" | "av1") {
        value
    } else {
        "h264"
    }
}

fn safe_video_encoder(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| {
        !value.is_empty()
            && value.len() <= 160
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
    })
}

fn safe_camera_size(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| {
        let Some((width, height)) = value.split_once('x') else {
            return false;
        };
        matches!(width.parse::<u32>(), Ok(1..=7680))
            && matches!(height.parse::<u32>(), Ok(1..=7680))
    })
}

fn safe_crop(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| {
        let parts = value
            .split(':')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>();
        matches!(parts, Ok(ref values) if values.len() == 4 && values[0] > 0 && values[1] > 0)
    })
}

fn capture_orientation_arg(value: Option<&str>) -> Option<String> {
    match value {
        Some("initial") => Some("--capture-orientation=@".into()),
        Some(value @ ("0" | "90" | "180" | "270")) => {
            Some(format!("--capture-orientation=@{value}"))
        }
        _ => None,
    }
}

fn build_args(config: &LaunchConfig, recording: Option<&Path>) -> Vec<String> {
    let mut args = vec!["-s".into(), config.serial.clone()];

    match config.mode.as_str() {
        "camera" => {
            args.push("--video-source=camera".into());
            if let Some(id) = config
                .camera_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
            {
                args.push(format!("--camera-id={}", id.trim()));
            } else if let Some(facing) = config
                .camera_facing
                .as_deref()
                .filter(|facing| valid_camera_facing(facing))
            {
                args.push(format!("--camera-facing={facing}"));
            }
            let camera_size = safe_camera_size(config.camera_size.as_deref());
            if let Some(size) = camera_size {
                args.push(format!("--camera-size={size}"));
            } else if config.max_size > 0 {
                args.push(format!("--max-size={}", config.max_size));
            }
            if camera_size.is_none() {
                if let Some(aspect_ratio) = config
                    .camera_aspect_ratio
                    .as_deref()
                    .filter(|value| matches!(*value, "sensor" | "16:9" | "4:3"))
                {
                    args.push(format!("--camera-ar={aspect_ratio}"));
                }
            }
            if config.max_fps > 0 {
                args.push(format!("--camera-fps={}", config.max_fps));
            }
            // scrcpy rejects high-speed capture without an explicit --camera-fps.
            if config.camera_high_speed && camera_size.is_some() && config.max_fps > 0 {
                args.push("--camera-high-speed".into());
            }
            if let Some(zoom) = config
                .camera_zoom
                .filter(|zoom| zoom.is_finite() && *zoom > 0.0)
            {
                args.push(format!("--camera-zoom={zoom:.2}"));
            }
            if config.camera_torch {
                args.push("--camera-torch".into());
            }
        }
        "desktop" => {
            if config.desktop_environment.as_deref() == Some("samsung_dex") {
                if let Some(display_id) = config.desktop_display_id {
                    args.push(format!("--display-id={display_id}"));
                }
            } else {
                let width = safe_desktop_dimension(
                    config.desktop_width,
                    if config.max_size >= 1920 { 1920 } else { 1280 },
                );
                let height = safe_desktop_dimension(
                    config.desktop_height,
                    if width >= 1920 { 1080 } else { 720 },
                );
                let density = safe_desktop_density(config.desktop_density);
                args.push(format!("--new-display={width}x{height}/{density}"));
                if config.desktop_flex {
                    args.push("--flex-display".into());
                }
                if config.desktop_no_decorations {
                    args.push("--no-vd-system-decorations".into());
                }
                if config.desktop_keep_content {
                    args.push("--no-vd-destroy-content".into());
                }
                if let Some(package) = config
                    .desktop_start_app
                    .as_deref()
                    .map(str::trim)
                    .filter(|package| !package.is_empty())
                {
                    args.push(format!("--start-app={package}"));
                }
                args.push("--display-ime-policy=local".into());
            }
            if config.max_fps > 0 {
                args.push(format!("--max-fps={}", config.max_fps));
            }
        }
        _ => {
            if config.max_size > 0 {
                args.push(format!("--max-size={}", config.max_size));
            }
            if config.max_fps > 0 {
                args.push(format!("--max-fps={}", config.max_fps));
            }
        }
    }

    args.push(format!("--video-codec={}", safe_video_codec(&config.codec)));
    let bit_rate = if (1..=200).contains(&config.video_bit_rate) {
        config.video_bit_rate
    } else {
        8
    };
    args.push(format!("--video-bit-rate={bit_rate}M"));
    if let Some(encoder) = safe_video_encoder(config.video_encoder.as_deref()) {
        args.push(format!("--video-encoder={encoder}"));
    }
    if !config.audio || config.audio_source.as_deref() == Some("off") {
        args.push("--no-audio".into());
    } else {
        if let Some(source) = config
            .audio_source
            .as_deref()
            .filter(|source| matches!(*source, "output" | "mic"))
        {
            args.push(format!("--audio-source={source}"));
        }
        // Wi-Fi jitter underruns scrcpy's default 50 ms audio buffer, which is
        // heard as choppy audio. Trade a little latency for smooth playback.
        if is_wireless_serial(&config.serial) {
            args.push(format!("--audio-buffer={WIRELESS_AUDIO_BUFFER_MS}"));
        }
    }
    // --stay-awake only works while the phone is charging, so over Wi-Fi on
    // battery it did nothing. --keep-active simulates user activity and works
    // on battery. A screen turned off by scrcpy needs it too: only the panel is
    // off, so Android's screen timeout would still put the phone to sleep, and
    // lock it, whenever the PC stops sending input.
    if (config.stay_awake || config.turn_screen_off) && config.mode != "camera" {
        args.push("--keep-active".into());
    }
    if config.turn_screen_off && config.mode != "camera" {
        args.push("--turn-screen-off".into());
    }
    if config.show_touches && config.mode != "camera" {
        args.push("--show-touches".into());
    }
    if config.fullscreen {
        args.push("--fullscreen".into());
    }
    if let Some(orientation) = capture_orientation_arg(config.capture_orientation.as_deref()) {
        args.push(orientation);
    }
    // scrcpy rejects --crop together with --flex-display.
    let flex_display = config.mode == "desktop"
        && config.desktop_environment.as_deref() != Some("samsung_dex")
        && config.desktop_flex;
    if let Some(crop) = safe_crop(config.crop.as_deref()).filter(|_| !flex_display) {
        args.push(format!("--crop={crop}"));
    }
    if let Some(path) = recording {
        args.push(format!("--record={}", path.display()));
    }
    args.push(format!("--window-title=SCRCPY Studio · {}", config.serial));
    args
}

fn shell_preview(path: &Path, args: &[String]) -> String {
    let quoted = args
        .iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {}", path.display(), quoted)
}

const WIRELESS_AUDIO_BUFFER_MS: u32 = 150;
const FULLSCREEN_SETTLE: Duration = Duration::from_millis(1500);
const KEPT_SESSION_LOGS: usize = 40;

fn session_logs_dir() -> Result<PathBuf, String> {
    let folder = app_data_dir()?.join("Session Logs");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    Ok(folder)
}

/// Creates a log file for one scrcpy launch and removes the oldest logs so the
/// folder does not grow without bound.
fn new_session_log(config: &LaunchConfig) -> Result<PathBuf, String> {
    let folder = session_logs_dir()?;
    let mut logs = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .collect::<Vec<_>>();
    logs.sort();
    let excess = (logs.len() + 1).saturating_sub(KEPT_SESSION_LOGS);
    for old in logs.iter().take(excess) {
        let _ = fs::remove_file(old);
    }
    let serial = config
        .serial
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    Ok(folder.join(format!(
        "{}-{}-{serial}.log",
        Local::now().format("%Y%m%d-%H%M%S%3f"),
        config.mode
    )))
}

/// Starts scrcpy with its output written to `log`, so failures and runtime
/// warnings (audio underruns, disconnects) can be inspected afterwards.
fn launch_and_watch(path: &Path, args: &[String], log: &Path) -> Result<bool, String> {
    let file = fs::File::create(log).map_err(|e| e.to_string())?;
    let stderr = file.try_clone().map_err(|e| e.to_string())?;
    let mut child = scrcpy_command(path)
        .args(args)
        .stdout(file)
        .stderr(stderr)
        .spawn()
        .map_err(|e| e.to_string())?;

    for _ in 0..6 {
        thread::sleep(Duration::from_millis(150));
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => return Ok(false),
            None => continue,
        }
    }
    Ok(true)
}

fn push_variant(variants: &mut Vec<LaunchConfig>, mutator: impl FnOnce(&mut LaunchConfig)) {
    let mut next = variants
        .last()
        .cloned()
        .expect("at least one launch variant");
    mutator(&mut next);
    variants.push(next);
}

pub(crate) fn fallback_configs(original: &LaunchConfig) -> Vec<LaunchConfig> {
    let mut variants = vec![original.clone()];
    // A manual crop outside the captured area kills every attempt, so it is
    // the first option to drop.
    if safe_crop(original.crop.as_deref()).is_some() {
        push_variant(&mut variants, |next| next.crop = None);
    }
    variants = mode_fallbacks(variants);
    // Skip retries that would launch the exact same command, e.g. dropping a
    // crop that --flex-display already suppressed.
    variants.dedup_by(|next, previous| build_args(next, None) == build_args(previous, None));
    variants
}

fn mode_fallbacks(mut variants: Vec<LaunchConfig>) -> Vec<LaunchConfig> {
    let original = variants[0].clone();
    let original = &original;

    if original.mode == "camera" {
        if original.camera_high_speed {
            push_variant(&mut variants, |next| {
                next.camera_high_speed = false;
                next.camera_size = None;
                if next.max_fps > 60 {
                    next.max_fps = 30;
                }
            });
        }
        if original.camera_torch {
            push_variant(&mut variants, |next| next.camera_torch = false);
        }
        if original.camera_zoom.unwrap_or(1.0) > 1.01 {
            push_variant(&mut variants, |next| next.camera_zoom = None);
        }
        if original.camera_aspect_ratio.is_some() {
            push_variant(&mut variants, |next| next.camera_aspect_ratio = None);
        }
        if original.video_encoder.is_some() {
            push_variant(&mut variants, |next| next.video_encoder = None);
        }
        if original.codec != "h264" {
            push_variant(&mut variants, |next| next.codec = "h264".into());
        }
        if original.max_fps > 30 {
            push_variant(&mut variants, |next| next.max_fps = 30);
        }
        if original.max_size > 1280 {
            push_variant(&mut variants, |next| next.max_size = 1280);
        }
        if original.camera_id.is_some() && original.camera_facing.is_some() {
            push_variant(&mut variants, |next| next.camera_id = None);
        }
        return variants;
    }

    if original.mode == "desktop" {
        if original.desktop_environment.as_deref() != Some("samsung_dex") {
            if original.desktop_no_decorations {
                push_variant(&mut variants, |next| next.desktop_no_decorations = false);
            }
            if original.desktop_flex {
                push_variant(&mut variants, |next| next.desktop_flex = false);
            }
            if original.desktop_width.unwrap_or(1920) > 1280
                || original.desktop_height.unwrap_or(1080) > 720
            {
                push_variant(&mut variants, |next| {
                    next.desktop_width = Some(1280);
                    next.desktop_height = Some(720);
                    next.desktop_density = Some(200);
                });
            }
        }
        if original.video_encoder.is_some() {
            push_variant(&mut variants, |next| next.video_encoder = None);
        }
        if original.codec != "h264" {
            push_variant(&mut variants, |next| next.codec = "h264".into());
        }
        if original.max_fps > 30 {
            push_variant(&mut variants, |next| next.max_fps = 30);
        }
        return variants;
    }

    if original.video_encoder.is_some() {
        push_variant(&mut variants, |next| next.video_encoder = None);
    }
    if original.codec != "h264" {
        push_variant(&mut variants, |next| next.codec = "h264".into());
    }
    if original.max_size > 1280 {
        push_variant(&mut variants, |next| next.max_size = 1280);
    }
    if original.max_fps > 30 {
        push_variant(&mut variants, |next| next.max_fps = 30);
    }
    variants
}

#[tauri::command(async)]
pub(crate) fn launch_session(
    manager: tauri::State<'_, SessionManager>,
    config: LaunchConfig,
    requested_mode: String,
) -> Result<LaunchResult, String> {
    validate_launch_mode(&config, &requested_mode)?;
    let scrcpy = scrcpy_path()?;
    let devices = list_devices()?;
    let device = devices
        .iter()
        .find(|d| d.serial == config.serial)
        .ok_or_else(|| "Selected device is no longer connected.".to_string())?;
    if device.state != "device" {
        return Err(format!(
            "Device is '{}', not ready. Run Connection Doctor for the next step.",
            device.state
        ));
    }

    stop_managed_session(&manager);
    // Also clean up a matching window left behind by an older build which did
    // not yet participate in managed-session tracking.
    native_window::close(&config.serial);

    let recording = if config.record {
        Some(recording_path()?)
    } else {
        None
    };
    let variants = fallback_configs(&config);
    let total = variants.len();
    let mut last_desktop_diagnostics: Option<DesktopDiagnostics> = None;
    let mut last_log: Option<PathBuf> = None;

    for (index, variant) in variants.iter().enumerate() {
        let args = build_args(variant, recording.as_deref());
        let started = if config.mode == "desktop" {
            let outcome = launch_desktop_and_watch(&scrcpy, &args, &config.serial)?;
            last_desktop_diagnostics = Some(outcome.diagnostics);
            outcome.started
        } else {
            let log = new_session_log(variant)?;
            let started = launch_and_watch(&scrcpy, &args, &log)?;
            last_log = Some(log);
            started
        };
        if started {
            let fallback_used = index > 0;
            let remembered = remember_successful_profile(variant).is_ok();
            let events = last_log
                .as_ref()
                .map(|log| EventLog::create(&log.with_extension("events.log")))
                .unwrap_or_default();
            events.write(format!(
                "session started: mode {}, turn screen off {}, keep awake {}",
                variant.mode, variant.turn_screen_off, variant.stay_awake
            ));
            let phone = Arc::new(PhoneControl {
                serial: variant.serial.clone(),
                keep_screen_off: AtomicBool::new(
                    variant.turn_screen_off && watches_phone_power(variant),
                ),
                power_presses: AtomicU64::new(0),
                restoring: AtomicBool::new(false),
                events,
            });
            if let Ok(mut active) = manager.active.lock() {
                let now = Instant::now();
                *active = Some(ManagedSession {
                    config: variant.clone(),
                    show_touches_backup: None,
                    scrcpy_manages_show_touches: variant.show_touches,
                    started_at: now,
                    power: PowerWatch::default(),
                    phone,
                    input_watch: None,
                    fullscreen_settles_at: now + FULLSCREEN_SETTLE,
                    panel_settles_at: now + PANEL_SETTLE,
                });
            }
            return Ok(LaunchResult {
                started: true,
                fallback_used,
                attempts: index + 1,
                command_preview: shell_preview(&scrcpy, &args),
                recording_path: recording.as_ref().map(|p| p.display().to_string()),
                desktop_diagnostics: last_desktop_diagnostics,
                message: if fallback_used {
                    if config.mode == "camera" {
                        format!(
                            "Camera opened after SCRCPY Studio automatically found a safer working combination on attempt {} of {}.",
                            index + 1,
                            total
                        )
                    } else if config.mode == "desktop" {
                        format!(
                            "{} recovered automatically on attempt {} of {} using a safer capture configuration.",
                            desktop_launch_name(&config), index + 1, total
                        )
                    } else if remembered {
                        format!(
                            "Session recovered on attempt {} of {}. This working profile is now remembered for this device.",
                            index + 1,
                            total
                        )
                    } else {
                        format!(
                            "Session started after SCRCPY Studio automatically recovered on attempt {} of {}.",
                            index + 1,
                            total
                        )
                    }
                } else if config.mode == "camera" {
                    "Camera opened with the selected smart camera profile.".into()
                } else if config.mode == "desktop" {
                    format!(
                        "{} opened. The Desktop Diagnostics log records the exact command and observed Android display state.",
                        desktop_launch_name(&config)
                    )
                } else {
                    "Session started with the selected smart profile.".into()
                },
            });
        }
    }

    if config.mode == "desktop" {
        let diagnostics = last_desktop_diagnostics.unwrap_or_default();
        let detail = if diagnostics.scrcpy_output.trim().is_empty() {
            diagnostics.exit_result.clone()
        } else {
            compact_error(&diagnostics.scrcpy_output)
        };
        return Ok(LaunchResult {
            started: false,
            fallback_used: total > 1,
            attempts: total,
            command_preview: diagnostics.command.clone(),
            recording_path: recording.as_ref().map(|p| p.display().to_string()),
            message: format!(
                "{} did not stay running after {} attempts: {}. Open Desktop Diagnostics for the complete command, output, and device evidence.",
                desktop_launch_name(&config), total, detail
            ),
            desktop_diagnostics: Some(diagnostics),
        });
    }

    let detail = last_log
        .and_then(|log| fs::read_to_string(log).ok())
        .filter(|output| !output.trim().is_empty())
        .map(|output| format!(" Last error: {}.", compact_error(&output)))
        .unwrap_or_default();
    Err(format!(
        "scrcpy exited immediately after {} smart attempts.{} Open Connection Doctor and verify the device/runtime.",
        total, detail
    ))
}

#[tauri::command(async)]
pub(crate) fn session_status(manager: tauri::State<'_, SessionManager>) -> SessionStatus {
    let ended = if let Ok(mut active) = manager.active.lock() {
        if active.as_ref().is_some_and(|session| {
            session.started_at.elapsed() > Duration::from_secs(3)
                && !native_window::exists(&session.config.serial)
        }) {
            active.take()
        } else {
            None
        }
    } else {
        None
    };
    if let Some(session) = ended {
        restore_live_settings(session);
    }

    // Read the phone state at most every few seconds, outside the lock: adb
    // over Wi-Fi can take a while. Hands on the phone are handled as they
    // happen by the input watch.
    let power_check = manager.active.lock().ok().and_then(|mut active| {
        let session = active.as_mut()?;
        let due = session
            .power
            .checked_at
            .is_none_or(|at| at.elapsed() >= POWER_CHECK_INTERVAL);
        if !due || !watches_phone_power(&session.config) {
            return None;
        }
        session.power.checked_at = Some(Instant::now());
        let restart_input_watch = !session
            .input_watch
            .as_mut()
            .is_some_and(PhysicalInputWatch::is_running);
        Some((session.phone.clone(), restart_input_watch))
    });
    let mut new_input_watch = None;
    let phone_state = power_check.and_then(|(phone, restart_input_watch)| {
        if restart_input_watch {
            let watched = phone.clone();
            new_input_watch = PhysicalInputWatch::start(&phone.serial, move |line| {
                watched.on_physical_input(line)
            })
            .inspect_err(|error| phone.events.write(format!("input watch failed: {error}")))
            .ok();
        }
        adb_shell(&phone.serial, &[PHONE_STATE_QUERY])
            .ok()
            .and_then(|text| parse_phone_state(&text))
    });

    match manager.active.lock() {
        Ok(mut active) => match active.as_mut() {
            Some(session) => {
                if new_input_watch.is_some() {
                    session.input_watch = new_input_watch;
                }
                let phone = session.phone.clone();
                if phone_state.is_some() && phone_state != session.power.previous {
                    phone.events.write(format!("phone state: {phone_state:?}"));
                }
                let settled = Instant::now() >= session.panel_settles_at;
                let keep_off = phone.keep_screen_off.load(Ordering::SeqCst);
                if session.power.observe(phone_state, keep_off, settled)
                    && phone.keep_screen_off.swap(false, Ordering::SeqCst)
                {
                    phone
                        .events
                        .write("phone screen came on or its power key was pressed");
                    phone.turn_screen_on(session.power.asleep);
                }
                // The input watch and the checks above may have handed the
                // phone back to the user; make the settings follow.
                if watches_phone_power(&session.config)
                    && session.config.turn_screen_off
                    && !phone.keep_screen_off.load(Ordering::SeqCst)
                {
                    session.config.turn_screen_off = false;
                    session.panel_settles_at = Instant::now() + PANEL_SETTLE;
                    session.power.notice = Some(SCREEN_TURNED_ON_NOTICE.into());
                }
                if Instant::now() >= session.fullscreen_settles_at {
                    if let Some(fullscreen) = native_window::is_fullscreen(&session.config.serial) {
                        session.config.fullscreen = fullscreen;
                    }
                }
                let mut status = status_of(session);
                status.notice = session.power.notice.take();
                status
            }
            None => SessionStatus::default(),
        },
        Err(_) => SessionStatus::default(),
    }
}

/// Wakes the phone without the toggle behaviour of POWER. If SCRCPY Studio is
/// keeping the phone screen off, turn the panel off again once it is awake.
#[tauri::command(async)]
pub(crate) fn wake_phone(
    manager: tauri::State<'_, SessionManager>,
    serial: String,
) -> Result<String, String> {
    let phone = manager.active.lock().ok().and_then(|mut active| {
        let session = active
            .as_mut()
            .filter(|session| session.config.serial == serial)?;
        session.panel_settles_at = Instant::now() + PANEL_SETTLE;
        session.power.asleep = false;
        Some(session.phone.clone())
    });
    adb_shell(&serial, &["input", "keyevent", "KEYCODE_WAKEUP"])?;
    let Some(phone) = phone else {
        return Ok("Phone woken up.".into());
    };
    phone.events.write("Wake Phone: sent KEYCODE_WAKEUP");
    if phone.keep_screen_off.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(700));
        native_window::press_mod_key(&serial, 'o', false)?;
        phone
            .events
            .write("Wake Phone: sent MOD+o to keep the screen off");
        return Ok("Phone woken up; its screen stays off while mirroring.".into());
    }
    Ok("Phone woken up.".into())
}

#[tauri::command(async)]
pub(crate) fn apply_live_setting(
    manager: tauri::State<'_, SessionManager>,
    config: LaunchConfig,
    setting: String,
) -> Result<SessionStatus, String> {
    let mut active = manager
        .active
        .lock()
        .map_err(|_| "The active session state is unavailable.".to_string())?;
    let session = active
        .as_mut()
        .ok_or_else(|| "No active scrcpy session is running.".to_string())?;
    if session.config.serial != config.serial || session.config.mode != config.mode {
        return Err("The active scrcpy window belongs to another device or mode.".into());
    }
    if !native_window::exists(&session.config.serial) {
        return Err("The active scrcpy window has already closed.".into());
    }

    match setting.as_str() {
        "fullscreen" => {
            native_window::press_f11(&config.serial)?;
            session.config.fullscreen = config.fullscreen;
            session.fullscreen_settles_at = Instant::now() + FULLSCREEN_SETTLE;
        }
        "turnScreenOff" if config.mode != "camera" => {
            native_window::press_mod_key(&config.serial, 'o', !config.turn_screen_off)?;
            session.config.turn_screen_off = config.turn_screen_off;
            session.panel_settles_at = Instant::now() + PANEL_SETTLE;
            let phone = session.phone.clone();
            phone
                .keep_screen_off
                .store(config.turn_screen_off, Ordering::SeqCst);
            phone.events.write(format!(
                "Turn screen off switched {} in the app",
                if config.turn_screen_off { "on" } else { "off" }
            ));
            if !config.turn_screen_off {
                thread::spawn(move || relight_backlight(&phone.serial, &phone.events));
            }
        }
        "showTouches" if config.mode != "camera" => {
            if session.show_touches_backup.is_none() && !session.scrcpy_manages_show_touches {
                session.show_touches_backup =
                    Some(read_setting(&config.serial, "system", "show_touches")?);
            }
            write_setting(
                &config.serial,
                "system",
                "show_touches",
                if config.show_touches { "1" } else { "0" },
            )?;
            session.config.show_touches = config.show_touches;
        }
        "cameraTorch" if config.mode == "camera" => {
            native_window::press_mod_key(&config.serial, 't', !config.camera_torch)?;
            session.config.camera_torch = config.camera_torch;
        }
        _ => return Err("This setting requires restarting the current scrcpy session.".into()),
    }

    Ok(status_of(session))
}

fn desktop_launch_name(config: &LaunchConfig) -> &'static str {
    match config.desktop_environment.as_deref() {
        Some("samsung_dex") => "Samsung DeX display",
        Some("android_desktop_windowing") => "Android Desktop Windowing",
        _ => "Virtual Display",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config(mode: &str) -> LaunchConfig {
        LaunchConfig {
            serial: "ABC".into(),
            mode: mode.into(),
            max_size: 1920,
            max_fps: 60,
            codec: "h265".into(),
            video_bit_rate: 8,
            video_encoder: None,
            audio: true,
            audio_source: Some(if mode == "camera" { "mic" } else { "output" }.into()),
            stay_awake: true,
            turn_screen_off: false,
            show_touches: false,
            record: false,
            fullscreen: false,
            capture_orientation: None,
            crop: None,
            camera_id: None,
            camera_facing: None,
            camera_zoom: None,
            camera_torch: false,
            camera_size: None,
            camera_aspect_ratio: None,
            camera_high_speed: false,
            desktop_width: None,
            desktop_height: None,
            desktop_density: None,
            desktop_flex: false,
            desktop_no_decorations: false,
            desktop_keep_content: false,
            desktop_start_app: None,
            desktop_environment: None,
            desktop_display_id: None,
        }
    }

    #[test]
    fn generates_progressively_safer_fallbacks() {
        let config = sample_config("mirror");
        let variants = fallback_configs(&config);
        assert_eq!(variants.len(), 4);
        assert_eq!(variants[1].codec, "h264");
        assert_eq!(variants[2].max_size, 1280);
        assert_eq!(variants[3].max_fps, 30);
    }

    #[test]
    fn camera_args_use_camera_specific_fps_and_controls() {
        let mut config = sample_config("camera");
        config.camera_id = Some("2".into());
        config.camera_facing = Some("back".into());
        config.camera_zoom = Some(2.0);
        config.camera_torch = true;
        let args = build_args(&config, None);
        assert!(args.contains(&"--video-source=camera".to_string()));
        assert!(args.contains(&"--camera-id=2".to_string()));
        assert!(args.contains(&"--camera-fps=60".to_string()));
        assert!(args.contains(&"--camera-zoom=2.00".to_string()));
        assert!(args.contains(&"--camera-torch".to_string()));
        assert!(args.contains(&"--audio-source=mic".to_string()));
        assert!(args.contains(&"--video-bit-rate=8M".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--max-fps=")));
    }

    #[test]
    fn advanced_video_options_are_safely_forwarded() {
        let mut config = sample_config("creator");
        config.codec = "av1".into();
        config.video_bit_rate = 24;
        config.video_encoder = Some("c2.android.av1.encoder".into());
        config.capture_orientation = Some("90".into());
        config.crop = Some("1080:1920:0:0".into());
        let args = build_args(&config, None);
        assert!(args.contains(&"--video-codec=av1".to_string()));
        assert!(args.contains(&"--video-bit-rate=24M".to_string()));
        assert!(args.contains(&"--video-encoder=c2.android.av1.encoder".to_string()));
        assert!(args.contains(&"--capture-orientation=@90".to_string()));
        assert!(args.contains(&"--crop=1080:1920:0:0".to_string()));
    }

    #[test]
    fn screen_off_keeps_the_phone_active_on_battery() {
        let mut config = sample_config("mirror");
        config.stay_awake = false;
        config.turn_screen_off = true;
        let args = build_args(&config, None);
        assert!(args.contains(&"--turn-screen-off".to_string()));
        assert!(args.contains(&"--keep-active".to_string()));
        assert!(!args.contains(&"--stay-awake".to_string()));

        config.turn_screen_off = false;
        let args = build_args(&config, None);
        assert!(!args.contains(&"--keep-active".to_string()));
    }

    #[test]
    fn wireless_audio_gets_a_larger_buffer() {
        let mut config = sample_config("mirror");
        assert!(!build_args(&config, None)
            .iter()
            .any(|arg| arg.starts_with("--audio-buffer=")));
        config.serial = "192.168.0.101:44741".into();
        assert!(build_args(&config, None).contains(&"--audio-buffer=150".to_string()));
        config.audio = false;
        assert!(!build_args(&config, None)
            .iter()
            .any(|arg| arg.starts_with("--audio-buffer=")));
    }

    const PHONE_DUMP: &str = "  mWakefulness=Awake\n  mLastSleepTime=9414070 (387785 ms ago)\n  mLastSleepReason=timeout\n   powerMode=Off\n    isKeyguardShowing=false\n";

    fn phone(awake: bool, sleep: u64, reason: &str, screen_on: bool) -> PhoneState {
        PhoneState {
            awake,
            last_sleep_time: Some(sleep),
            last_sleep_reason: Some(reason.into()),
            screen_on: Some(screen_on),
            locked: false,
        }
    }

    #[test]
    fn parses_the_phone_state_query() {
        assert_eq!(
            parse_phone_state(PHONE_DUMP),
            Some(phone(true, 9414070, "timeout", false))
        );
        let locked = PHONE_DUMP.replace("isKeyguardShowing=false", "isKeyguardShowing=true");
        assert!(parse_phone_state(&locked).is_some_and(|state| state.locked));
        assert_eq!(parse_phone_state("nothing useful"), None);
    }

    /// A watch that has already seen an awake phone with its screen kept off.
    fn watching() -> PowerWatch {
        let mut watch = PowerWatch::default();
        watch.observe(Some(phone(true, 10, "timeout", false)), true, true);
        watch
    }

    fn phone_control(keep_screen_off: bool) -> Arc<PhoneControl> {
        Arc::new(PhoneControl {
            serial: "ABC".into(),
            keep_screen_off: AtomicBool::new(keep_screen_off),
            power_presses: AtomicU64::new(0),
            // Pretend a restore is running so the test does not call adb.
            restoring: AtomicBool::new(true),
            events: EventLog::default(),
        })
    }

    #[test]
    fn hands_on_the_phone_end_screen_off() {
        let phone = phone_control(true);
        phone.on_physical_input("/dev/input/event4: EV_KEY KEY_RIGHT DOWN");
        assert!(!phone.keep_screen_off.load(Ordering::SeqCst));
        assert_eq!(phone.power_presses.load(Ordering::SeqCst), 0);

        let phone = phone_control(false);
        phone.on_physical_input("/dev/input/event1: EV_KEY KEY_POWER DOWN");
        assert_eq!(phone.power_presses.load(Ordering::SeqCst), 1);
        assert!(!phone.keep_screen_off.load(Ordering::SeqCst));
    }

    #[test]
    fn power_button_sleep_ends_screen_off() {
        let mut watch = watching();
        let pressed = phone(false, 30, "power_button", false);
        assert!(watch.observe(Some(pressed), true, true));
    }

    #[test]
    fn a_lit_screen_ends_screen_off_once_settled() {
        let lit = phone(true, 10, "timeout", true);
        assert!(!watching().observe(Some(lit.clone()), true, false));
        assert!(watching().observe(Some(lit), true, true));
    }

    #[test]
    fn timeout_sleep_keeps_screen_off_and_reports_it() {
        let mut watch = watching();
        let slept = phone(false, 30, "timeout", false);
        assert!(!watch.observe(Some(slept), true, true));
        assert!(watch.asleep);
        let notice = watch.notice.as_deref().unwrap_or_default();
        assert!(notice.contains("timeout"));
    }

    #[test]
    fn invalid_advanced_values_fall_back_or_are_ignored() {
        let mut config = sample_config("creator");
        config.codec = "made-up".into();
        config.video_bit_rate = 999;
        config.video_encoder = Some("invalid encoder name".into());
        config.capture_orientation = Some("45".into());
        config.crop = Some("invalid".into());
        let args = build_args(&config, None);
        assert!(args.contains(&"--video-codec=h264".to_string()));
        assert!(args.contains(&"--video-bit-rate=8M".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--video-encoder=")));
        assert!(!args
            .iter()
            .any(|arg| arg.starts_with("--capture-orientation=")));
        assert!(!args.iter().any(|arg| arg.starts_with("--crop=")));
    }

    #[test]
    fn high_speed_camera_uses_an_explicit_supported_size() {
        let mut config = sample_config("camera");
        config.camera_high_speed = true;
        config.camera_size = Some("1280x720".into());
        config.max_fps = 120;
        let args = build_args(&config, None);
        assert!(args.contains(&"--camera-size=1280x720".to_string()));
        assert!(args.contains(&"--camera-fps=120".to_string()));
        assert!(args.contains(&"--camera-high-speed".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--max-size=")));
    }

    #[test]
    fn high_speed_camera_is_dropped_without_an_explicit_fps() {
        let mut config = sample_config("camera");
        config.camera_high_speed = true;
        config.camera_size = Some("1280x720".into());
        config.max_fps = 0;
        let args = build_args(&config, None);
        assert!(!args.contains(&"--camera-high-speed".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--camera-fps=")));
    }

    #[test]
    fn crop_is_skipped_with_flex_display_only() {
        let mut config = sample_config("desktop");
        config.crop = Some("1080:1920:0:0".into());
        config.desktop_flex = true;
        let args = build_args(&config, None);
        assert!(args.contains(&"--flex-display".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--crop=")));

        config.desktop_flex = false;
        let args = build_args(&config, None);
        assert!(args.contains(&"--crop=1080:1920:0:0".to_string()));
    }

    #[test]
    fn crop_is_the_first_fallback_to_drop() {
        let mut config = sample_config("mirror");
        config.crop = Some("1080:1920:0:0".into());
        let variants = fallback_configs(&config);
        assert_eq!(variants[0].crop.as_deref(), Some("1080:1920:0:0"));
        assert_eq!(variants[1].crop, None);
        assert_eq!(variants[1].codec, config.codec);
        assert!(variants[1..].iter().all(|variant| variant.crop.is_none()));
    }

    #[test]
    fn fallbacks_never_repeat_the_same_command() {
        let mut config = sample_config("desktop");
        config.crop = Some("1080:1920:0:0".into());
        config.desktop_flex = true;
        let variants = fallback_configs(&config);
        // Flex already suppresses the crop, so dropping it alone is skipped.
        assert!(variants[1].crop.is_none() && !variants[1].desktop_flex);
        for pair in variants.windows(2) {
            assert_ne!(build_args(&pair[0], None), build_args(&pair[1], None));
        }
    }

    #[test]
    fn rejects_a_stale_config_from_another_mode() {
        let config = sample_config("mirror");
        let error = validate_launch_mode(&config, "camera").unwrap_err();
        assert!(error.contains("changed while its settings were loading"));
    }

    #[test]
    fn accepts_a_config_for_the_requested_mode() {
        let config = sample_config("camera");
        assert!(validate_launch_mode(&config, "camera").is_ok());
    }

    #[test]
    fn camera_fallbacks_remove_risky_options() {
        let mut config = sample_config("camera");
        config.camera_id = Some("0".into());
        config.camera_facing = Some("back".into());
        config.camera_zoom = Some(2.0);
        config.camera_torch = true;
        config.camera_high_speed = true;
        config.camera_size = Some("1280x720".into());
        config.max_fps = 120;
        let variants = fallback_configs(&config);
        assert!(variants.iter().any(|item| !item.camera_torch));
        assert!(variants.iter().any(|item| !item.camera_high_speed));
        assert!(variants.iter().any(|item| item.camera_zoom.is_none()));
        assert!(variants.iter().any(|item| item.codec == "h264"));
        assert!(variants.iter().any(|item| item.max_fps == 30));
        assert!(variants.iter().any(|item| item.max_size == 1280));
        assert!(variants.iter().any(|item| item.camera_id.is_none()));
    }

    #[test]
    fn desktop_args_use_verified_virtual_display_controls() {
        let mut config = sample_config("desktop");
        config.desktop_width = Some(1920);
        config.desktop_height = Some(1080);
        config.desktop_density = Some(240);
        config.desktop_flex = true;
        config.desktop_keep_content = true;
        config.desktop_start_app = Some("com.android.settings".into());
        let args = build_args(&config, None);
        assert!(args.contains(&"--new-display=1920x1080/240".to_string()));
        assert!(args.contains(&"--flex-display".to_string()));
        assert!(args.contains(&"--no-vd-destroy-content".to_string()));
        assert!(args.contains(&"--start-app=com.android.settings".to_string()));
        assert!(args.contains(&"--display-ime-policy=local".to_string()));
        assert!(args.contains(&"--keep-active".to_string()));
    }

    #[test]
    fn desktop_does_not_move_apps_to_the_phone_by_default() {
        let config = sample_config("desktop");
        let args = build_args(&config, None);
        assert!(!args.contains(&"--no-vd-destroy-content".to_string()));
    }

    #[test]
    fn desktop_fallbacks_reduce_risky_options() {
        let mut config = sample_config("desktop");
        config.desktop_width = Some(1920);
        config.desktop_height = Some(1080);
        config.desktop_density = Some(240);
        config.desktop_flex = true;
        config.desktop_start_app = Some("com.example.launcher".into());
        let variants = fallback_configs(&config);
        assert!(variants.iter().any(|item| !item.desktop_flex));
        assert!(variants
            .iter()
            .all(|item| item.desktop_start_app.as_deref() != Some("com.android.settings")));
        assert!(variants.iter().any(|item| item.desktop_width == Some(1280)));
        assert!(variants.iter().any(|item| item.max_fps == 30));
    }

    #[test]
    fn samsung_dex_captures_an_existing_display() {
        let mut config = sample_config("desktop");
        config.desktop_environment = Some("samsung_dex".into());
        config.desktop_display_id = Some(2);
        let args = build_args(&config, None);
        assert!(args.contains(&"--display-id=2".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--new-display=")));
    }
}
