# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

SCRCPY Studio is a Windows-first Tauri v2 + React 19 desktop frontend for Genymobile's scrcpy. The Rust backend (`src-tauri/`) drives `adb` and `scrcpy` as child processes. The React frontend (`src/`) is a compact, fixed-size (900×620) UI with three modes: Mirror Phone (internal id `creator`), Camera, and Desktop. Product principle from CONTRIBUTING.md: favor simple workflows over exposing every scrcpy flag. Rare options belong in Advanced settings (`src/AdvancedSettings.tsx`), not the primary UI.

## Commands

```bash
npm install
npm run check                                   # tsc --noEmit (frontend type check; there is no linter)
npm run build                                   # tsc + vite build -> dist/
npm run tauri dev                               # run the app (Vite dev server on :1420, strictPort)
npm run tauri build                             # production build
npm run tauri build -- --no-bundle              # just src-tauri/target/release/scrcpy-studio.exe
cargo test --manifest-path src-tauri/Cargo.toml                   # all Rust tests
cargo test --manifest-path src-tauri/Cargo.toml fallback           # tests whose name matches a filter
cargo test --manifest-path src-tauri/Cargo.toml session::tests     # one module's tests
```

There are no frontend tests. Rust unit tests live in `#[cfg(test)] mod tests` at the bottom of each module and mostly cover the pure parsers of `adb`/`dumpsys` output and the arg/fallback builders.

## Architecture

**IPC boundary.** Every backend entry point is registered in `src-tauri/src/lib.rs` (`generate_handler!`) and called from the frontend with `invoke("snake_case_name", { camelCaseArgs })`, mostly in `src/App.tsx`. Shared data types are defined twice and must be kept in sync by hand: in `src-tauri/src/models.rs` (serde, `rename_all = "camelCase"`) and in `src/types.ts`.

**All commands are `#[tauri::command(async)]` sync functions.** Blocking adb/scrcpy calls then run off the UI thread. Keep new commands the same way.

**Child processes.** Always create them with `commands::hidden_command(...)`, never `Command::new` directly. It sets `CREATE_NO_WINDOW` so no console window flashes on Windows, and CI fails if `Command::new` appears in any other `.rs` file. `runtime::resolve_binary` finds `adb`/`scrcpy` by searching in order: the managed runtime (`%LOCALAPPDATA%\SCRCPY Studio\runtime`), next to the exe (and `runtime/` and `scrcpy/` subfolders), then `PATH`. `adb_path()` checks the `ADB` env var before that, which is the same override scrcpy reads. Launch scrcpy only through `runtime::scrcpy_command`, which passes the resolved adb to scrcpy via `ADB`; otherwise scrcpy uses the adb next to its own exe, and a second adb version can restart the shared adb server. `install_official_runtime` downloads the latest official scrcpy Windows zip with an embedded PowerShell script, checks it against the release's `SHA256SUMS.txt`, and installs it there. User-supplied values (addresses, pairing codes) go in as process args, never interpolated into a shell string.

**Session lifecycle (`session.rs`).** `SessionManager` is Tauri managed state that holds at most one active scrcpy session, and it is stopped when the window is destroyed. `launch_session` works like this:
- It checks that `config.mode` matches `requestedMode` (this guards against a stale config from a previous mode).
- It stops any existing session.
- It builds a list of progressively safer configs with `fallback_configs` (drop high-speed/torch/zoom/encoder, then h264, lower fps, smaller size, and so on).
- It launches each one with `build_args` and treats it as started if scrcpy is still alive after about 900ms. scrcpy output goes to a per-session file in `<app data>/Session Logs` (the newest 40 are kept), and a failure message quotes the relevant error line from it.
- The first success is saved per device+mode via `preferences::remember_successful_profile`.

show_touches is a device setting, so it is backed up and restored when the session ends. Keep awake uses `--keep-active` rather than `--stay-awake`, because `--stay-awake` only works while the phone is charging. On Windows, the scrcpy window is found by the title `SCRCPY Studio · <serial>` and controlled through Win32 messages. `PostMessage` key presses (Alt shortcuts) work without focusing the window, but F11 needs it in the foreground.

**Phone state watch (Mirror only, `watches_phone_power`).** The frontend polls `session_status` every 900ms, and that call is also where the backend syncs session state:
- Every 3s it reads the phone state with one adb shell call (`PHONE_STATE_QUERY`: `dumpsys power`, SurfaceFlinger `powerMode`, keyguard).
- Presses on the phone are caught as they happen by `physical_input.rs`, which runs `adb shell -tt getevent -lq`. The `-tt` pty is required, because without it the output is buffered and nothing arrives.
- If the user wakes the phone while "Turn screen off" is on, `PhoneControl` stops keeping it off, the toggle goes OFF, and a `notice` is returned.
- The Fullscreen toggle follows the scrcpy window's real size.
- Watch events go to `<session>.events.log` via `event_log.rs`. Read that file first when debugging power behavior.

On the HyperOS test phone, turning the panel back on with MOD+Shift+o leaves the backlight dark. `relight_backlight` fixes that by nudging the screen brightness.

**Device intelligence.** `devices.rs` (`inspect_device`, `recommend_settings` per mode), `camera.rs` (parses `scrcpy --list-cameras`/sizes), and `desktop.rs` (the largest module) all turn raw adb/dumpsys/scrcpy text into typed capabilities.

**Desktop Mode (`desktop.rs`).** It separates three cases and must not treat them as the same thing: a generic `--new-display` virtual display, Android desktop windowing (freeform), and Samsung DeX (only captured when firmware already exposes an active DeX display). Details:
- `probe_desktop_capabilities` is the single source of truth. The frontend launch button is gated on `desktopProbe.capabilities?.supported`. Only one probe runs at a time (`PROBE_LOCK` in the backend, a shared in-flight promise in `DesktopControls.tsx`), and a finished probe must not overwrite the user's choices.
- A desktop launch counts as started only after scrcpy prints "New display" (up to 10s). The virtual display is kept from rotating with portrait-only apps via `wm set-ignore-orientation-request`.
- Probes must not disturb the phone's own screen.
- `enable_desktop_experience` backs up global settings to `desktop-settings-backup.json` before changing them, then reboots and reconnects. `restore_desktop_experience` puts them back.
- Detailed diagnostics go to log files under the app data dir ("Desktop Diagnostics"), opened via "Open Logs". They are not shown in the UI.

**Local persistence.** Profiles, remembered wireless devices, and desktop backups are JSON files under `dirs::config_local_dir()`. Screenshots and recordings live under the paths from `creator.rs`.

## CI regression guards (`.github/workflows/windows-test-build.yml`)

This workflow uses string matching on the source to enforce UI and behavior invariants. If you rename or remove any of these, the build fails, so update the workflow on purpose in the same change:
- UI labels/classNames in `src/App.tsx`, `src/AdvancedSettings.tsx`, `src/DesktopControls.tsx`, e.g. `{ id: "creator", label: "Mirror Phone"`, `className="device-actions"`, `className="capture-toolbar"`, wireless strings like "Saved phones"/"Manual setup", and loading strings like "Checking desktop support…".
- Things that must *not* come back: a separate `{ id: "mirror"` mode, "CONNECTION DOCTOR" in the UI, diagnostic panels in DesktopControls, "Record session" in Advanced settings, and a scrollable `.main-content`.
- Specific CSS rules in `src/styles.css` that keep the window from needing to scroll.
- The window is 900×620 in `tauri.conf.json`. `main.rs` must keep `cfg_attr(not(debug_assertions), windows_subsystem = "windows")`, and the built exe is checked for the PE GUI subsystem.
- `creator.rs` must contain `screenshot_display_id`, `SurfaceFlinger`, `display_id: Option<u32>`.

## Releases

The version appears in three places that must match: `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`. Update `RELEASE_NOTES.md` too, since it becomes the GitHub release body. Pushing tag `v<version>` runs `release-windows.yml`. That workflow checks that the tag matches the config version and that the NSIS hooks contain `SetAutoClose true`, then builds unsigned NSIS setup + portable exe + `SHA256SUMS-Windows-x64.txt` and publishes them. Don't bundle third-party binaries without documenting their source, version, license, and integrity check.
