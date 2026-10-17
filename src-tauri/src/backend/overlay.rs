// ------------ Game Overlay ------------
// Runs the in-game overlay. It starts the separate overlay helper exe next to the game, registers the hotkeys,
// drives screenshots and recordings, and keeps the capture folder and audio track settings. It also creates the overlay
// drawer and recording readout windows and keeps them lined up on top of the game window.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use peebify_helpers::overlay::section::{SectionView, Signal};
use peebify_helpers::overlay::shared::{
    HELPER_PROTOCOL, HOTKEY_COUNT, HOTKEY_RECORD, HOTKEY_SHOT, HOTKEY_TOGGLE, SIGNAL_TO_HELPER,
    SIGNAL_TO_LAUNCHER, STATUS_RUNNING,
};
use tokio::process::Child;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ClipCursor, DispatchMessageW, GetClientRect, GetForegroundWindow, GetWindow, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsIconic, MsgWaitForMultipleObjectsEx, PeekMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, TranslateMessage, CHILDID_SELF, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GWL_EXSTYLE,
    GW_OWNER, HWND_NOTOPMOST, HWND_TOPMOST, MSG, MWMO_INPUTAVAILABLE, OBJID_WINDOW, PM_REMOVE,
    QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

use super::fs_util::dialog::Pick;
use super::window_manager::WEBVIEW_BROWSER_ARGS;
use super::{arg_str, err_response, now_ms, ok_with, BackendState};

const HELPER_NAME: &str = "peebify-overlay-helper.exe";
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
const STALE_TICKS: u32 = 6;
const HELPER_EXIT_WAIT: Duration = Duration::from_secs(5);
const HOTKEYS_REGISTERED_EVENT: &str = "overlay-hotkeys-registered";
const WINDOW_WAIT_TICKS: u32 = 120;
const RETIRED_WINDOW_WAIT_TICKS: u32 = 40;

pub const AUDIO_TRACKS: &[(&str, &str, bool)] = &[
    ("desktop", "Desktop", false),
    ("mic", "Microphone", false),
    ("game", "Game only", true),
    ("discord", "Discord only", true),
];

pub const HOTKEYS: &[(&str, &str, &str)] = &[
    ("overlayHotkey", "Open the overlay", "Alt+P"),
    ("overlayShotHotkey", "Screenshot", "Alt+S"),
    ("overlayRecHotkey", "Start or stop recording", "Alt+R"),
];

fn hotkey_text(read: impl Fn(&str) -> Value, key: &str, default: &str) -> String {
    let stored = read(&format!("behavior.{key}"));
    let text = stored.as_str().filter(|s| !s.is_empty());
    text.unwrap_or(default).to_string()
}

const BARE_KEY_WARNING: &str = "A key on its own does not reach the overlay while a game that runs as administrator is in front, and every supported game does. Combine it with Alt, Ctrl or Shift to be safe.";

const DEFAULT_AUDIO_TRACKS: &str = "desktop,mic";

pub fn supports_per_app_audio() -> bool {
    let build: u32 = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()
        .and_then(|key| key.get_value::<String, _>("CurrentBuildNumber").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    build >= 20348
}

pub fn default_capture_dir(app: &AppHandle) -> PathBuf {
    use tauri::Manager;
    app.path()
        .video_dir()
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Videos")
        })
        .join("Peebify")
}

pub fn capture_dir(app: &AppHandle) -> PathBuf {
    capture_dir_in(app, &app.state::<BackendState>().config)
}

pub(super) fn capture_dir_in(app: &AppHandle, config: &super::config::LauncherConfig) -> PathBuf {
    let configured = config.get("behavior.overlayCaptureFolder");
    match configured.as_str().filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => default_capture_dir(app),
    }
}

fn csv_setting(app: &AppHandle, key: &str, fallback: &str) -> Vec<String> {
    let raw = app.state::<BackendState>().config.get(key);
    let text = raw.as_str().unwrap_or(fallback);
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) async fn get_overlay_config(app: &AppHandle) -> Result<Value, String> {
    let config = &app.state::<BackendState>().config;

    let hotkeys: Vec<Value> = HOTKEYS
        .iter()
        .map(|(key, label, default)| {
            json!({
                "id": key,
                "label": label,
                "accelerator": hotkey_text(|k| config.get(k), key, default),
                "default": default,
            })
        })
        .collect();

    let per_app_audio = supports_per_app_audio();
    let tracks: Vec<Value> = AUDIO_TRACKS
        .iter()
        .map(|(id, label, needs_win11)| {
            json!({
                "id": id,
                "label": label,
                "available": !needs_win11 || per_app_audio,
            })
        })
        .collect();

    let folder = capture_dir(app);
    Ok(ok_with(json!({
        "hotkeys": hotkeys,
        "audioTracks": tracks,
        "selectedAudioTracks": csv_setting(app, "behavior.overlayAudioTracks", DEFAULT_AUDIO_TRACKS),
        "perAppAudio": per_app_audio,
        "captureFolder": folder.to_string_lossy(),
        "captureFolderIsDefault": folder == default_capture_dir(app),
        "launchAction": config.get("behavior.launchAction"),
        "recEstimate": rec_estimate(),
    })))
}

fn rec_estimate() -> Value {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let (width, height) = game_hwnd()
        .and_then(client_rect)
        .map(|rect| (rect.width, rect.height))
        .unwrap_or_else(|| unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) });
    let mut model = super::recorder::bitrate_model();
    model["sourceWidth"] = json!(width.max(0));
    model["sourceHeight"] = json!(height.max(0));
    model
}

pub(super) async fn set_overlay_hotkey(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(id) = arg_str(args, 0) else {
        return Ok(err_response("No hotkey was specified."));
    };
    let Some((key, _, default)) = HOTKEYS.iter().find(|(k, _, _)| *k == id) else {
        return Ok(err_response("That isn't an overlay hotkey."));
    };

    let accelerator = arg_str(args, 1).unwrap_or(default).trim().to_string();
    let Some(parsed) = parse_accelerator(&accelerator) else {
        return Ok(err_response(
            "That shortcut won't work. Use a function key, or a modifier plus another key.",
        ));
    };
    let warning = (!accelerator.contains('+')).then_some(BARE_KEY_WARNING);

    let clash = HOTKEYS.iter().find_map(|(other, label, other_default)| {
        if other == key {
            return None;
        }
        let config = &app.state::<BackendState>().config;
        let current = hotkey_text(|k| config.get(k), other, other_default);
        (parse_accelerator(&current) == Some(parsed)).then_some(label)
    });
    if let Some(label) = clash {
        return Ok(err_response(format!(
            "\"{label}\" already uses that shortcut."
        )));
    }

    app.state::<BackendState>().config.set(
        &format!("behavior.{key}"),
        Value::String(accelerator.clone()),
    );
    log::info!("overlay: {key} bound to {accelerator}");
    app.state::<BackendState>().overlay.publish_hotkeys();
    let _ = app.emit("settings-changed", json!({ "key": key, "value": accelerator }));
    Ok(ok_with(
        json!({ "id": key, "accelerator": accelerator, "warning": warning }),
    ))
}

pub(super) async fn set_overlay_capture_folder(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let state = app.state::<BackendState>();

    if args.first() == Some(&Value::Null) || args.first().and_then(Value::as_str) == Some("") {
        let default_dir = default_capture_dir(app);
        super::fs_util::allow_asset_dir(app, &default_dir);
        state.config.set(
            "behavior.overlayCaptureFolder",
            Value::String(String::new()),
        );
        let _ = app.emit("settings-changed", json!({ "key": "overlayCaptureFolder" }));
        return Ok(ok_with(json!({
            "path": default_dir.to_string_lossy(),
            "isDefault": true,
        })));
    }

    let owner = if drawer_is_open() {
        DRAWER_LABEL
    } else {
        "main"
    };
    let picked = crate::backend::fs_util::dialog::pick_folder(
        app,
        Pick {
            title: "Choose where captures are saved".into(),
            owner,
            ..Pick::default()
        },
    )
    .await?;
    let Some(path) = picked.as_deref() else {
        return Ok(json!({ "success": false, "cancelled": true }));
    };

    super::fs_util::allow_asset_dir(app, std::path::Path::new(path));
    state.config.set(
        "behavior.overlayCaptureFolder",
        Value::String(path.to_string()),
    );
    log::info!("overlay: captures will be saved to {path}");
    let _ = app.emit("settings-changed", json!({ "key": "overlayCaptureFolder" }));
    Ok(ok_with(json!({ "path": path, "isDefault": false })))
}

pub(super) async fn open_launcher(app: &AppHandle) -> Result<Value, String> {
    let state = app.state::<BackendState>();
    state.overlay.close_drawer(false);
    state.window.show_window();
    Ok(super::ok_response())
}

pub(super) async fn open_capture_folder(app: &AppHandle) -> Result<Value, String> {
    let dir = capture_dir(app);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Ok(err_response(format!("Could not open that folder: {e}")));
    }
    crate::backend::fs_util::dialog::open_folder(&dir)?;
    Ok(super::ok_response())
}

#[derive(Debug, Default, PartialEq)]
struct HotkeyReport {
    registered: Vec<String>,
    unregistered: Vec<String>,
    invalid: Vec<String>,
}

impl HotkeyReport {
    fn describe(&self) -> String {
        let list = |keys: &[String]| {
            if keys.is_empty() {
                "none".to_string()
            } else {
                keys.join(", ")
            }
        };
        let mut text = format!(
            "hotkeys registered: {}; not registered: {}",
            list(&self.registered),
            list(&self.unregistered)
        );
        if !self.invalid.is_empty() {
            text.push_str(&format!("; invalid: {}", list(&self.invalid)));
        }
        text
    }
}

fn hotkey_report(stored: impl Fn(&str) -> Value, bits: u32) -> HotkeyReport {
    let mut report = HotkeyReport::default();
    for (index, (key, _, default)) in HOTKEYS.iter().enumerate() {
        let text = hotkey_text(&stored, key, default);
        if parse_accelerator(&text).is_none() {
            report.invalid.push(text);
        } else if bits & (1 << index) != 0 {
            report.registered.push(text);
        } else {
            report.unregistered.push(text);
        }
    }
    report
}

async fn retire_helper(mut child: Child) {
    let deadline = Instant::now() + HELPER_EXIT_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                log::info!("overlay: the previous helper has exited ({status})");
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(None) => {
                log::warn!(
                    "overlay: the previous helper did not exit within {}s, so it is being ended",
                    HELPER_EXIT_WAIT.as_secs()
                );
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                return;
            }
            Err(e) => {
                log::warn!("overlay: could not check on the previous helper ({e})");
                return;
            }
        }
    }
}

fn process_started_ms(pid: u32) -> Option<i64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

    if pid == 0 {
        return None;
    }
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let timed = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let timed = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        timed
    };
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    (timed && ticks != 0).then(|| (ticks / 10_000) as i64 - UNIX_EPOCH_MS)
}

fn parse_accelerator(text: &str) -> Option<(u32, u32)> {
    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;

    let mut modifiers = 0u32;
    let mut key: Option<u32> = None;

    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" => modifiers |= MOD_WIN,
            other => {
                if key.is_some() {
                    return None;
                }
                key = virtual_key(other);
            }
        }
    }

    let vk = key?;
    let bare_ok = (0x70..=0x87).contains(&vk);
    (modifiers != 0 || bare_ok).then_some((modifiers, vk))
}

fn virtual_key(lowercase: &str) -> Option<u32> {
    if let Some(number) = lowercase.strip_prefix('f') {
        if let Ok(n) = number.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x6F + n);
            }
        }
    }
    let mut chars = lowercase.chars();
    let (first, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return match lowercase {
            "space" => Some(0x20),
            "tab" => Some(0x09),
            "insert" => Some(0x2D),
            "delete" => Some(0x2E),
            "home" => Some(0x24),
            "end" => Some(0x23),
            "pageup" => Some(0x21),
            "pagedown" => Some(0x22),
            "arrowleft" => Some(0x25),
            "arrowup" => Some(0x26),
            "arrowright" => Some(0x27),
            "arrowdown" => Some(0x28),
            "enter" => Some(0x0D),
            "backspace" => Some(0x08),
            _ => None,
        };
    }
    match first {
        'a'..='z' => Some(first.to_ascii_uppercase() as u32),
        '0'..='9' => Some(first as u32),
        _ => None,
    }
}

// ------------ Overlay Manager ------------
// Owns the helper process for the current game: spawning it, watching its status, restarting it if it dies,
// and handling hotkey presses and the drawer.
enum HelperCheck {
    NotReady,
    Mismatch { theirs: u32 },
    Live,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    Idle,
    Starting,
    Running,
    Mismatched,
    Failed,
}

impl HelperState {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Mismatched => "mismatched",
            Self::Failed => "failed",
        }
    }
}

struct Link {
    section: SectionView,
    to_helper: Option<Signal>,
}

pub struct OverlayManager {
    app: AppHandle,
    link: Mutex<Option<Link>>,
    state: Mutex<HelperState>,
    attached_game: Mutex<Option<String>>,
    owner_game: Mutex<Option<String>>,
    generation: AtomicU64,
    helper: tokio::sync::Mutex<Option<Child>>,
    respawned: AtomicU64,
    window_error: Mutex<Option<String>>,
    suspend_token: AtomicU64,
    recorder: std::sync::OnceLock<super::recorder::RecorderHandle>,
}

impl OverlayManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            link: Mutex::new(None),
            state: Mutex::new(HelperState::Idle),
            attached_game: Mutex::new(None),
            owner_game: Mutex::new(None),
            generation: AtomicU64::new(0),
            helper: tokio::sync::Mutex::new(None),
            respawned: AtomicU64::new(0),
            window_error: Mutex::new(None),
            suspend_token: AtomicU64::new(0),
            recorder: std::sync::OnceLock::new(),
        })
    }

    fn recorder(&self) -> &super::recorder::RecorderHandle {
        self.recorder
            .get_or_init(|| super::recorder::start(self.app.clone()))
    }

    pub fn is_recording(&self) -> bool {
        self.recorder.get().is_some_and(|r| r.status.is_recording())
    }

    pub fn stop_recording_and_wait(&self, timeout: Duration) -> bool {
        self.recorder
            .get()
            .is_none_or(|recorder| recorder.stop_and_wait(timeout))
    }

    pub fn active_recording_path(&self) -> Option<PathBuf> {
        let recorder = self.recorder.get()?;
        (!recorder.status.is_idle())
            .then(|| recorder.status.active_path())
            .flatten()
    }

    fn rec_settings(&self) -> super::recorder::RecSettings {
        let config = self.config();
        let number = |key: &str, fallback: u32| {
            config
                .get(&format!("behavior.{key}"))
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(fallback)
        };
        let (audio, warnings) = self.audio_sources();
        super::recorder::RecSettings {
            height: match config.get("behavior.overlayRecRes").as_str() {
                Some("720") => 720,
                Some("1080") => 1080,
                Some("1440") => 1440,
                _ => 0,
            },
            codec: config
                .get("behavior.overlayRecCodec")
                .as_str()
                .unwrap_or("h264")
                .to_string(),
            fps: number("overlayRecFps", 60).clamp(24, 240),
            quality: config
                .get("behavior.overlayRecQuality")
                .as_str()
                .unwrap_or("balanced")
                .to_string(),
            directory: capture_dir(&self.app),
            audio,
            warnings,
        }
    }

    fn game_pid(&self) -> u32 {
        self.link
            .lock()
            .as_ref()
            .map(|link| link.section.shared().game_pid.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn audio_sources(&self) -> (Vec<super::recorder::AudioSource>, Vec<String>) {
        use super::recorder::AudioSource;
        use super::recorder::audio::SourceKind;

        let chosen = csv_setting(
            &self.app,
            "behavior.overlayAudioTracks",
            DEFAULT_AUDIO_TRACKS,
        );
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let source = |kind: SourceKind, name: &str| AudioSource {
            kind,
            name: name.to_string(),
        };
        let desktop = chosen.iter().any(|t| t == "desktop");

        if desktop {
            sources.push(source(SourceKind::Desktop, "desktop"));
        }
        if chosen.iter().any(|t| t == "mic") {
            sources.push(source(SourceKind::Microphone, "microphone"));
        }

        if desktop {
            if chosen.iter().any(|t| t == "game" || t == "discord") {
                log::info!(
                    "recorder: desktop audio already carries the game and Discord, so those tracks are not captured separately"
                );
            }
            return (sources, warnings);
        }

        if !supports_per_app_audio() {
            if chosen.iter().any(|t| t == "game" || t == "discord") {
                log::warn!(
                    "recorder: this build of Windows cannot capture one app's audio, so the game and Discord tracks are skipped"
                );
                warnings.push(
                    "No game or Discord audio in this clip: this version of Windows cannot capture one app's audio."
                        .to_string(),
                );
            }
            return (sources, warnings);
        }

        if chosen.iter().any(|t| t == "game") {
            match self.game_pid() {
                0 => {
                    log::warn!("recorder: no game process to capture audio from");
                    warnings.push(
                        "No game audio in this clip: the game process was not found.".to_string(),
                    );
                }
                pid => sources.push(source(SourceKind::Process(pid), "game")),
            }
        }

        if chosen.iter().any(|t| t == "discord") {
            let wanted = csv_setting(&self.app, "behavior.overlayAudioDiscord", "");
            let running = super::discord_audio::running_audio_clients();
            if running.is_empty() {
                log::warn!("recorder: no Discord client is running, so its audio is not captured");
                warnings.push("No Discord audio in this clip: Discord is not running.".to_string());
            }
            let chosen_client = |id: &String| wanted.is_empty() || wanted.contains(id);
            let take_all = !running.is_empty() && !running.iter().any(|(id, _, _)| chosen_client(id));
            if take_all {
                log::warn!(
                    "recorder: none of the chosen Discord clients ({}) is running, so every running Discord client is captured instead",
                    wanted.join(", ")
                );
            }
            for (id, label, pid) in running {
                if take_all || chosen_client(&id) {
                    log::info!("recorder: capturing audio from {label} (pid {pid})");
                    sources.push(source(SourceKind::Process(pid), &label));
                }
            }
        }

        (sources, warnings)
    }

    pub fn toggle_recording(&self) -> Result<(), String> {
        let idle = self.recorder.get().is_none_or(|r| r.status.toggle_starts());
        if idle && !super::recorder::recording_supported() {
            return self.refuse_recording(
                "Recording needs Windows Media Foundation. Install the Media Feature Pack from Optional features.",
            );
        }
        let hwnd = match game_hwnd() {
            Some(hwnd) => hwnd as i64,
            None if idle => return self.refuse_recording("No game is running."),
            None => 0,
        };
        let game_id = self.capture_game_id();
        let settings = self.rec_settings();
        self.recorder().toggle(hwnd, &game_id, settings);
        Ok(())
    }

    fn refuse_recording(&self, error: &str) -> Result<(), String> {
        let _ = self.app.emit("overlay-record-failed", json!({ "error": error }));
        Err(error.to_string())
    }

    fn config(&self) -> Arc<super::config::LauncherConfig> {
        self.app.state::<BackendState>().config.clone()
    }

    fn enabled(&self) -> bool {
        self.config().get("behavior.overlayEnabled") == Value::Bool(true)
    }

    fn set_state(&self, state: HelperState) {
        *self.state.lock() = state;
        let _ = self.app.emit(
            "overlay-status",
            json!({ "state": state.label(), "error": self.error_message() }),
        );
    }

    fn error_message(&self) -> String {
        self.link
            .lock()
            .as_ref()
            .map(|l| l.section.shared().error_message())
            .unwrap_or_default()
    }

    pub fn publish_hotkeys(&self) {
        let link = self.link.lock();
        let Some(link) = link.as_ref() else {
            return;
        };
        let shared = link.section.shared();
        let config = self.config();

        for (index, (key, label, default)) in HOTKEYS.iter().enumerate() {
            let text = hotkey_text(|k| config.get(k), key, default);
            match parse_accelerator(&text) {
                Some((modifiers, vk)) => {
                    shared.hotkey_mods[index].store(modifiers, Ordering::Relaxed);
                    shared.hotkey_vk[index].store(vk, Ordering::Relaxed);
                    log::debug!("overlay: hotkey {index} is {text} (vk {vk}, mods {modifiers})");
                    if modifiers == 0 {
                        log::warn!("overlay: {label} is bound to the bare key {text}, which Windows will not deliver while an elevated game is in front");
                    }
                }
                None => {
                    log::warn!("overlay: \"{text}\" is not a usable shortcut for {label}");
                    shared.hotkey_vk[index].store(0, Ordering::Relaxed);
                }
            }
        }
        shared.hotkey_generation.fetch_add(1, Ordering::Release);
        if let Some(signal) = link.to_helper.as_ref() {
            signal.raise();
        }
    }

    pub fn suspend_hotkeys(self: &Arc<Self>, on: bool) {
        let token = self.suspend_token.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let link = self.link.lock();
            let Some(link) = link.as_ref() else {
                return;
            };
            link.section
                .shared()
                .hotkey_suspend
                .store(u32::from(on), Ordering::Release);
            if let Some(signal) = link.to_helper.as_ref() {
                signal.raise();
            }
        }
        if !on {
            return;
        }
        let manager = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(12)).await;
            if manager.suspend_token.load(Ordering::SeqCst) != token {
                return;
            }
            log::info!("overlay: the hotkey editor never finished, putting the shortcuts back");
            manager.suspend_hotkeys(false);
        });
    }

    pub fn set_game_in_front(&self, in_front: bool) {
        let link = self.link.lock();
        let Some(link) = link.as_ref() else {
            return;
        };
        let wanted = u32::from(in_front);
        if link.section.shared().game_in_front.swap(wanted, Ordering::AcqRel) == wanted {
            return;
        }
        log::info!(
            "overlay: {}",
            if in_front {
                "the game is in front again, taking the shortcuts back"
            } else {
                "another app is in front, handing the shortcuts back to it"
            }
        );
        if let Some(signal) = link.to_helper.as_ref() {
            signal.raise();
        }
    }

    fn hotkey_target_in_front(&self) -> bool {
        foreground_is_game_or_overlay(self.game_pid())
    }

    pub fn on_game_started(self: &Arc<Self>, game_id: &str, pid: Option<u32>) {
        if !self.enabled() {
            return;
        }
        let previous = self.owner_game.lock().replace(game_id.to_string());
        if let Some(previous) = previous.filter(|p| p != game_id) {
            log::info!("overlay: moving from {previous} to {game_id}");
        }
        let live = self.link.lock().is_some();
        if live {
            stop_tracking();
            self.end_session();
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let manager = self.clone();
        let game = game_id.to_string();
        tauri::async_runtime::spawn(async move {
            manager.attach_windows(game, pid, generation).await;
        });

        let section = match SectionView::create() {
            Ok(section) => section,
            Err(code) => {
                log::error!("overlay: could not create the shared section (error {code})");
                self.set_state(HelperState::Failed);
                return;
            }
        };
        section
            .shared()
            .game_pid
            .store(pid.unwrap_or(0), Ordering::Relaxed);
        section.shared().game_in_front.store(0, Ordering::Relaxed);

        *self.link.lock() = Some(Link {
            section,
            to_helper: Signal::open_or_create(SIGNAL_TO_HELPER).ok(),
        });
        self.publish_hotkeys();
        self.set_state(HelperState::Starting);

        let manager = self.clone();
        let helper_game_id = game_id.to_string();
        tauri::async_runtime::spawn(async move {
            manager.spawn_helper(generation, &helper_game_id).await;
        });
    }

    async fn spawn_helper(self: &Arc<Self>, generation: u64, game_id: &str) {
        let Some(helper) = super::fs_util::resource(&self.app, HELPER_NAME) else {
            log::error!("overlay: {HELPER_NAME} is missing from this install");
            self.set_state(HelperState::Failed);
            return;
        };
        let cwd = helper
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let launcher_pid = std::process::id().to_string();

        {
            let mut slot = self.helper.lock().await;
            if let Some(previous) = slot.take() {
                retire_helper(previous).await;
            }
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match self.link.lock().as_ref() {
                Some(link) => link.section.shared().reset_session(),
                None => return,
            }
            match super::process_utils::spawn_tool(&helper, &[launcher_pid], &[], &cwd).await {
                Ok(child) => *slot = Some(child),
                Err(e) => {
                    log::warn!("overlay: the helper was not started ({e})");
                    self.set_state(HelperState::Failed);
                    return;
                }
            }
        }

        for _ in 0..40 {
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match self.helper_check() {
                HelperCheck::Live => {
                    let bits = self
                        .link
                        .lock()
                        .as_ref()
                        .map(|l| l.section.shared().hotkey_registered.load(Ordering::Acquire))
                        .unwrap_or(0);
                    let config = self.config();
                    let report = hotkey_report(|key| config.get(key), bits);
                    log::info!(
                        "overlay: helper is running for {game_id} (protocol {HELPER_PROTOCOL:#x}; {})",
                        report.describe()
                    );
                    self.set_state(HelperState::Running);
                    self.clone().start_status_loop(generation);
                    return;
                }
                HelperCheck::Mismatch { theirs } => {
                    log::error!(
                        "overlay: the attached helper reports protocol {theirs:#x} but this launcher speaks {HELPER_PROTOCOL:#x}. It is a different build, so its readings are not being used. Helper: {}",
                        helper.display()
                    );
                    self.set_state(HelperState::Mismatched);
                    return;
                }
                HelperCheck::NotReady => {}
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let status = self
            .link
            .lock()
            .as_ref()
            .map(|l| l.section.shared().status.load(Ordering::Acquire));
        let error = self.error_message();
        log::warn!(
            "overlay: the helper did not report in within 10s (status {}, error: {})",
            status.map_or_else(|| "unknown".to_string(), |s| s.to_string()),
            if error.is_empty() { "none" } else { error.as_str() }
        );
        self.set_state(HelperState::Failed);
    }

    async fn attach_windows(self: Arc<Self>, game_id: String, pid: Option<u32>, generation: u64) {
        if pid.filter(|p| *p != 0).is_none() {
            log::warn!("overlay: no pid for {game_id} — cannot attach");
            return;
        }
        let games = { self.app.state::<BackendState>().game.clone() };
        let mut watching = false;

        loop {
            let mut found = None;
            for _ in 0..WINDOW_WAIT_TICKS {
                if self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                let Some(probe) = games.running_pid(&game_id).or(pid).filter(|p| *p != 0) else {
                    self.set_game_in_front(false);
                    return;
                };
                if let Some(hwnd) = tauri::async_runtime::spawn_blocking(move || {
                    find_game_window(probe).map(|h| h as i64)
                })
                .await
                .ok()
                .flatten()
                {
                    found = Some((hwnd, probe));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            let Some((raw, probe)) = found else {
                log::warn!(
                    "overlay: {game_id} never showed a window — the overlay will not attach"
                );
                self.set_game_in_front(false);
                return;
            };

            for _ in 0..RETIRED_WINDOW_WAIT_TICKS {
                if !windows_retiring(&self.app) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            *self.attached_game.lock() = Some(game_id.clone());
            let handle = raw as windows_sys::Win32::Foundation::HWND;
            let rect = client_rect(handle).unwrap_or_default();
            if let Err(e) = create_windows(&self.app, rect) {
                log::error!("overlay: {e}");
                *self.window_error.lock() = Some(e.clone());
                self.set_game_in_front(false);
                let state = *self.state.lock();
                let _ = self.app.emit(
                    "overlay-status",
                    json!({ "state": state.label(), "error": e, "windowError": e }),
                );
                super::platform::notify_if_backgrounded(
                    &self.app,
                    "Peebify overlay",
                    "The overlay window could not be created. Restart the game to try again.",
                );
                return;
            }
            *self.window_error.lock() = None;
            if let Some(link) = self.link.lock().as_ref() {
                link.section.shared().game_pid.store(probe, Ordering::Relaxed);
            }
            start_tracking(self.app.clone(), handle, probe);
            log::info!("overlay: attached to {game_id} (pid {probe})");

            if !watching {
                watching = true;
                self.clone().start_hotkey_watcher(generation);
            }

            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                if tracking_ended() {
                    break;
                }
            }
            self.set_game_in_front(false);

            if !games.is_game_running_id(&game_id) {
                return;
            }
            log::info!(
                "overlay: {game_id} closed its window (pid {probe}), waiting for a new one or for the game to exit"
            );
        }
    }

    fn start_hotkey_watcher(self: Arc<Self>, generation: u64) {
        std::thread::Builder::new()
            .name("overlay-ipc".into())
            .spawn(move || {
                let Ok(signal) = Signal::open_or_create(SIGNAL_TO_LAUNCHER) else {
                    log::warn!("overlay: could not open the hotkey signal");
                    return;
                };
                let mut seen = [0u32; HOTKEY_COUNT];
                {
                    let link = self.link.lock();
                    if let Some(link) = link.as_ref() {
                        let shared = link.section.shared();
                        for (index, slot) in seen.iter_mut().enumerate() {
                            *slot = shared.hotkey_epoch[index].load(Ordering::Acquire);
                        }
                    }
                }

                loop {
                    signal.wait(500);
                    if self.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let fired: Vec<usize> = {
                        let link = self.link.lock();
                        let Some(link) = link.as_ref() else { return };
                        let shared = link.section.shared();
                        (0..HOTKEY_COUNT)
                            .filter(|index| {
                                let now = shared.hotkey_epoch[*index].load(Ordering::Acquire);
                                let changed = now != seen[*index];
                                seen[*index] = now;
                                changed
                            })
                            .collect()
                    };
                    for index in fired {
                        self.on_hotkey(index);
                    }
                }
            })
            .map(|_| ())
            .unwrap_or_else(|e| log::error!("overlay: could not start the hotkey watcher: {e}"));
    }

    fn on_hotkey(&self, index: usize) {
        let name = HOTKEYS
            .get(index)
            .map(|(_, label, _)| *label)
            .unwrap_or("unknown");
        if !self.hotkey_target_in_front() {
            log::info!("overlay: hotkey {index} ({name}) ignored because the game is not in front");
            return;
        }
        log::info!("overlay: hotkey {index} fired ({name})");
        match index {
            HOTKEY_TOGGLE => self.toggle_drawer(),
            HOTKEY_SHOT => {
                ensure_hud(&self.app);
                let app = self.app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = super::capture::take_screenshot(&app, &[]).await;
                });
            }
            HOTKEY_RECORD => {
                ensure_hud(&self.app);
                let _ = self.toggle_recording();
            }
            _ => {}
        }
    }

    pub fn on_setting_changed(self: &Arc<Self>, key: &str) {
        if HOTKEYS.iter().any(|(hotkey, _, _)| *hotkey == key) {
            self.publish_hotkeys();
            return;
        }
        if key == "overlayEnabled" {
            self.apply_enabled();
        }
    }

    fn apply_enabled(self: &Arc<Self>) {
        let live = self.link.lock().is_some();
        if !self.enabled() {
            if live || self.owner_game.lock().is_some() {
                log::info!("overlay: turned off while a game runs, so the session ends now");
                self.on_game_stopped();
            }
            return;
        }
        if live {
            return;
        }
        let running = self.app.state::<BackendState>().game.running_game();
        if let Some((game_id, pid)) = running {
            log::info!("overlay: turned on while {game_id} runs, so the session starts now");
            self.on_game_started(&game_id, pid);
        }
    }

    pub fn toggle_drawer(&self) {
        let Some(window) = self
            .app
            .get_webview_window(DRAWER_LABEL)
        else {
            log::warn!("overlay: the drawer window does not exist, so there is nothing to open");
            return;
        };
        if drawer_is_open() {
            log::info!("overlay: closing the drawer");
            self.close_drawer(true);
        } else if !drawer_blocked_by_fullscreen(&self.app) {
            log::info!("overlay: opening the drawer");
            set_drawer_open(true);
            let _ = window.show();
            activate_drawer(&self.app);
            let _ = self.app.emit("overlay-opened", Value::Null);
        }
    }

    pub fn close_drawer(&self, refocus_game: bool) {
        let held_foreground = refocus_game && we_hold_foreground();
        set_drawer_open(false);
        if let Some(window) = self
            .app
            .get_webview_window(DRAWER_LABEL)
        {
            let _ = window.hide();
        }
        if held_foreground {
            focus_game(&self.app);
        }
        let _ = self.app.emit("overlay-closed", Value::Null);
    }

    fn helper_check(&self) -> HelperCheck {
        let link = self.link.lock();
        let Some(link) = link.as_ref() else {
            return HelperCheck::NotReady;
        };
        let shared = link.section.shared();
        if !shared.is_valid() || shared.status.load(Ordering::Acquire) != STATUS_RUNNING {
            return HelperCheck::NotReady;
        }
        let theirs = shared.helper_protocol.load(Ordering::Acquire);
        if theirs != HELPER_PROTOCOL {
            return HelperCheck::Mismatch { theirs };
        }
        HelperCheck::Live
    }

    fn start_status_loop(self: Arc<Self>, generation: u64) {
        tauri::async_runtime::spawn(async move {
            let mut last_sample = 0u32;
            let mut stale_ticks = 0u32;
            let mut last_registered: Option<u32> = None;
            loop {
                tokio::time::sleep(STATUS_INTERVAL).await;
                if self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }

                let registered = {
                    let link = self.link.lock();
                    let Some(link) = link.as_ref() else { return };
                    let shared = link.section.shared();
                    if !shared.is_valid() {
                        return;
                    }

                    let sample = shared.sample_seq.load(Ordering::Acquire);
                    if sample == last_sample {
                        stale_ticks += 1;
                    } else {
                        stale_ticks = 0;
                        last_sample = sample;
                    }

                    shared.hotkey_registered.load(Ordering::Acquire)
                };

                if last_registered.replace(registered) != Some(registered) {
                    let _ = self.app.emit(HOTKEYS_REGISTERED_EVENT, ());
                }
                if stale_ticks >= STALE_TICKS && self.restart_exited_helper(generation) {
                    return;
                }
                if stale_ticks == STALE_TICKS {
                    log::warn!("overlay: the helper stopped responding");
                    self.set_state(HelperState::Failed);
                } else if stale_ticks == 0 && *self.state.lock() == HelperState::Failed {
                    log::info!("overlay: the helper is responding again");
                    self.set_state(HelperState::Running);
                }
            }
        });
    }

    fn restart_exited_helper(self: &Arc<Self>, generation: u64) -> bool {
        let status = {
            let Ok(mut slot) = self.helper.try_lock() else {
                return false;
            };
            match slot.as_mut().map(Child::try_wait) {
                Some(Ok(Some(status))) => status,
                _ => return false,
            }
        };
        if self.respawned.swap(generation, Ordering::SeqCst) == generation {
            return false;
        }
        log::warn!("overlay: the helper exited ({status}), starting it again");
        self.set_state(HelperState::Starting);
        let manager = self.clone();
        let game_id = self.owner().unwrap_or_default();
        tauri::async_runtime::spawn(async move {
            manager.spawn_helper(generation, &game_id).await;
        });
        true
    }

    pub fn owner(&self) -> Option<String> {
        self.owner_game.lock().clone()
    }

    pub fn capture_game_id(&self) -> String {
        let state = self.app.state::<BackendState>();
        self.owner()
            .or_else(|| state.game.running_game().map(|(id, _)| id))
            .unwrap_or_else(|| state.config.active_game_id())
    }

    pub fn on_game_stopped(self: &Arc<Self>) {
        if let Some(recorder) = self.recorder.get() {
            recorder.stop();
        }
        stop_tracking();
        destroy_windows(&self.app);
        self.end_session();
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some(game_id) = self.owner_game.lock().take() {
            log::info!("overlay: session ended for {game_id}");
        }
        self.set_state(HelperState::Idle);

        let manager = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut slot = manager.helper.lock().await;
            if manager.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Some(child) = slot.take() {
                retire_helper(child).await;
            }
        });
    }

    fn end_session(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.attached_game.lock() = None;
        *self.window_error.lock() = None;

        let link = self.link.lock().take();
        if let Some(link) = link {
            link.section.shared().stop.store(1, Ordering::Release);
            if let Some(signal) = link.to_helper.as_ref() {
                signal.raise();
            }
        }
    }

    pub fn status(&self) -> Value {
        let state = *self.state.lock();
        let config = self.config();
        let (error, bits, game_pid) = {
            let link = self.link.lock();
            link.as_ref()
                .map(|l| {
                    let shared = l.section.shared();
                    (
                        shared.error_message(),
                        shared.hotkey_registered.load(Ordering::Acquire),
                        shared.game_pid.load(Ordering::Relaxed),
                    )
                })
                .unwrap_or((String::new(), 0, 0))
        };

        let live = matches!(state, HelperState::Running | HelperState::Mismatched);
        let mut report = hotkey_report(|key| config.get(key), bits);
        if !live {
            report.unregistered.clear();
        }

        json!({
            "state": state.label(),
            "gameId": self.attached_game.lock().clone(),
            "error": error,
            "unregistered": report.unregistered,
            "invalid": report.invalid,
            "windowError": self.window_error.lock().clone(),
            "recording": self.is_recording(),
            "recordingSinceMs": self.recording_since_ms(),
            "sessionStartedMs": process_started_ms(game_pid),
            "hudFeedback": pending_feedback(),
        })
    }

    fn recording_since_ms(&self) -> u64 {
        self.recorder
            .get()
            .filter(|r| r.status.is_recording())
            .map(|r| r.status.started_ms())
            .unwrap_or(0)
    }
}

pub(super) async fn get_overlay_status(app: &AppHandle) -> Result<Value, String> {
    Ok(ok_with(app.state::<BackendState>().overlay.status()))
}

pub(super) async fn suspend_overlay_hotkeys(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let on = args.first().and_then(Value::as_bool).unwrap_or(false);
    app.state::<BackendState>().overlay.suspend_hotkeys(on);
    Ok(ok_with(json!({ "suspended": on })))
}

pub(super) async fn overlay_record(app: &AppHandle) -> Result<Value, String> {
    let overlay = app.state::<BackendState>().overlay.clone();
    Ok(match overlay.toggle_recording() {
        Ok(()) => ok_with(json!({ "pending": true })),
        Err(e) => err_response(e),
    })
}

pub(super) async fn overlay_toggle(app: &AppHandle, _args: &[Value]) -> Result<Value, String> {
    app.state::<BackendState>().overlay.close_drawer(true);
    Ok(ok_with(json!({})))
}

// ------------ Overlay Windows ------------
// Creates the overlay drawer and the recording readout window and keeps them lined up on top of the game window,
// following it as it moves, resizes or loses focus. Windows only.

pub const DRAWER_LABEL: &str = "overlay";
pub const HUD_LABEL: &str = "overlay-hud";

pub const FULLSCREEN_BLOCKED_EVENT: &str = "overlay-fullscreen-blocked";

const RECONCILE_MS: u128 = 500;
const DEBOUNCE_MS: u128 = 16;
const GAME_EVENT_RANGES: [(u32, u32); 3] = [
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
    (EVENT_OBJECT_DESTROY, EVENT_OBJECT_DESTROY),
    (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
];
const FEEDBACK_HOLD_MS: i64 = 5_000;
const RETIRE_WAIT_STEPS: u32 = 40;
const FEEDBACK_EVENTS: [&str; 5] = [
    "overlay-capture",
    "overlay-capture-failed",
    "overlay-record-started",
    "overlay-record-stopped",
    "overlay-record-failed",
];

static FEEDBACK_UNTIL_MS: AtomicI64 = AtomicI64::new(0);
static HUD_CREATING: AtomicBool = AtomicBool::new(false);

struct HookState {
    dirty: AtomicBool,
    foreground_changed: AtomicBool,
    windows_changed: AtomicBool,
    destroyed: AtomicBool,
    game_hwnd: AtomicI64,
}

fn hook_state() -> &'static HookState {
    static STATE: OnceLock<HookState> = OnceLock::new();
    STATE.get_or_init(|| HookState {
        dirty: AtomicBool::new(false),
        foreground_changed: AtomicBool::new(false),
        windows_changed: AtomicBool::new(false),
        destroyed: AtomicBool::new(false),
        game_hwnd: AtomicI64::new(0),
    })
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _thread: u32,
    _time: u32,
) {
    let state = hook_state();
    if event == EVENT_OBJECT_LOCATIONCHANGE && object_id != OBJID_WINDOW {
        return;
    }
    let tracked = state.game_hwnd.load(Ordering::Relaxed);
    if tracked != 0
        && hwnd as i64 != tracked
        && (event == EVENT_OBJECT_LOCATIONCHANGE || event == EVENT_OBJECT_DESTROY)
    {
        return;
    }

    match event {
        EVENT_OBJECT_DESTROY if object_id == OBJID_WINDOW && child_id == CHILDID_SELF as i32 => {
            state.destroyed.store(true, Ordering::Relaxed)
        }
        EVENT_OBJECT_DESTROY => {}
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND => {
            state.foreground_changed.store(true, Ordering::Relaxed);
            state.dirty.store(true, Ordering::Relaxed);
        }
        _ => state.dirty.store(true, Ordering::Relaxed),
    }
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object_id: i32,
    _child_id: i32,
    _thread: u32,
    _time: u32,
) {
    let state = hook_state();
    state.foreground_changed.store(true, Ordering::Relaxed);
    state.dirty.store(true, Ordering::Relaxed);
}

pub fn find_game_window(pid: u32) -> Option<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    if pid == 0 {
        return None;
    }
    peebify_helpers::common::process::visible_windows_of(pid)
        .into_iter()
        .filter(|&hwnd| unsafe { GetWindow(hwnd, GW_OWNER) }.is_null())
        .filter_map(|hwnd| {
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let read = unsafe { GetWindowRect(hwnd, &mut rect) } != 0;
            read.then(|| ((rect.right - rect.left) as i64 * (rect.bottom - rect.top) as i64, hwnd))
        })
        .filter(|(area, _)| *area > 0)
        .rev()
        .max_by_key(|(area, _)| *area)
        .map(|(_, hwnd)| hwnd)
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn client_rect(hwnd: HWND) -> Option<Rect> {
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetClientRect(hwnd, &mut client) } == 0 {
        return None;
    }
    let mut origin = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        return None;
    }
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    (width > 0 && height > 0).then_some(Rect {
        x: origin.x,
        y: origin.y,
        width,
        height,
    })
}

pub fn is_exclusive_fullscreen(hwnd: HWND) -> bool {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN,
    };

    let mut state = 0;
    let shell_says_fullscreen = unsafe { SHQueryUserNotificationState(&mut state) } >= 0
        && state == QUNS_RUNNING_D3D_FULL_SCREEN;
    if !shell_says_fullscreen {
        return false;
    }

    let mut window = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut window) } == 0
    {
        return false;
    }

    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info: MONITORINFO = unsafe { core::mem::zeroed() };
    info.cbSize = core::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return false;
    }

    window.left == info.rcMonitor.left
        && window.top == info.rcMonitor.top
        && window.right == info.rcMonitor.right
        && window.bottom == info.rcMonitor.bottom
}

pub fn drawer_blocked_by_fullscreen(app: &AppHandle) -> bool {
    let Some(hwnd) = game_hwnd() else {
        return false;
    };
    if !is_exclusive_fullscreen(hwnd) {
        return false;
    }
    log::info!("overlay: not opening the drawer because the game is in exclusive fullscreen");
    let _ = app.emit(FULLSCREEN_BLOCKED_EVENT, serde_json::Value::Null);
    true
}

struct Feedback {
    at_ms: i64,
    event: &'static str,
    payload: String,
}

fn last_feedback() -> &'static Mutex<Option<Feedback>> {
    static LAST: OnceLock<Mutex<Option<Feedback>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn note_capture_feedback(event: &'static str, payload: &str) {
    let now = now_ms();
    FEEDBACK_UNTIL_MS.store(now + FEEDBACK_HOLD_MS, Ordering::Relaxed);
    *last_feedback().lock() = Some(Feedback {
        at_ms: now,
        event,
        payload: payload.to_string(),
    });
    hook_state().dirty.store(true, Ordering::Relaxed);
    wake_tracker();
}

fn feedback_json(feedback: &Feedback, now: i64) -> Option<serde_json::Value> {
    if now - feedback.at_ms > FEEDBACK_HOLD_MS {
        return None;
    }
    let payload =
        serde_json::from_str(&feedback.payload).unwrap_or(serde_json::Value::Null);
    Some(serde_json::json!({ "event": feedback.event, "payload": payload }))
}

pub fn pending_feedback() -> Option<serde_json::Value> {
    last_feedback()
        .lock()
        .as_ref()
        .and_then(|feedback| feedback_json(feedback, now_ms()))
}

fn install_feedback_listeners(app: &AppHandle) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        for event in FEEDBACK_EVENTS {
            let handle = app.clone();
            app.listen_any(event, move |e| {
                note_capture_feedback(event, e.payload());
                ensure_hud(&handle);
            });
        }
    });
}

fn hwnd_of(window: &tauri::WebviewWindow) -> Option<HWND> {
    window.hwnd().ok().map(|h| h.0 as HWND)
}

static DRAWER_HWND: AtomicI64 = AtomicI64::new(0);
static HUD_HWND: AtomicI64 = AtomicI64::new(0);

fn hwnd_slot(label: &str) -> &'static AtomicI64 {
    if label == DRAWER_LABEL {
        &DRAWER_HWND
    } else {
        &HUD_HWND
    }
}

fn cached_hwnd(label: &str) -> Option<HWND> {
    let raw = hwnd_slot(label).load(Ordering::Relaxed);
    (raw != 0).then_some(raw as HWND)
}

fn remember_hwnd(label: &str, hwnd: HWND) {
    hwnd_slot(label).store(hwnd as i64, Ordering::Relaxed);
}

fn retiring(app: &AppHandle, label: &str) -> bool {
    cached_hwnd(label).is_none() && app.get_webview_window(label).is_some()
}

pub fn windows_retiring(app: &AppHandle) -> bool {
    retiring(app, DRAWER_LABEL) || retiring(app, HUD_LABEL)
}

fn wait_until_retired(app: &AppHandle, label: &str) {
    for _ in 0..RETIRE_WAIT_STEPS {
        if !retiring(app, label) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    log::warn!("overlay: the previous {label} window is still closing, reusing it");
}

fn our_hwnds() -> Vec<HWND> {
    [DRAWER_LABEL, HUD_LABEL]
        .iter()
        .filter_map(|label| cached_hwnd(label))
        .collect()
}

fn apply_styles(hwnd: HWND, label: &str) {
    let extra = if label == DRAWER_LABEL {
        WS_EX_TOOLWINDOW
    } else {
        WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_LAYERED
    };
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (current as u32 | extra) as isize);
    }
}

fn set_topmost(hwnd: HWND, topmost: bool) {
    let order = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe { SetWindowPos(hwnd, order, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
}

fn is_covered_by(ours: HWND, game: HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, GW_HWNDPREV};
    let mut cursor = ours;
    for _ in 0..64 {
        cursor = unsafe { GetWindow(cursor, GW_HWNDPREV) };
        if cursor.is_null() {
            return false;
        }
        if cursor == game {
            return true;
        }
    }
    false
}

pub fn focus_game(app: &AppHandle) {
    let Some(hwnd) = game_hwnd() else { return };
    let raw = hwnd as i64;
    let _ = app.run_on_main_thread(move || {
        if unsafe { SetForegroundWindow(raw as HWND) } == 0 {
            log::debug!("overlay: the game did not take the foreground back");
        }
    });
}

pub fn activate_drawer(app: &AppHandle) {
    let Some(hwnd) = cached_hwnd(DRAWER_LABEL) else {
        log::warn!("overlay: no drawer handle yet, cannot give it the foreground");
        return;
    };
    let raw = hwnd as i64;
    let _ = app.run_on_main_thread(move || {
        let hwnd = raw as HWND;
        unsafe {
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
            ClipCursor(std::ptr::null());
            if GetForegroundWindow() != hwnd {
                log::warn!(
                    "overlay: the drawer did not take the foreground, the game may keep the cursor"
                );
            }
        }
    });
}

pub fn we_hold_foreground() -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    !foreground.is_null() && our_hwnds().contains(&foreground)
}

fn is_game_or_overlay(foreground: HWND, game_hwnd: Option<HWND>, game_pid: u32, ours: &[HWND]) -> bool {
    if foreground.is_null() {
        return false;
    }
    if Some(foreground) == game_hwnd || ours.contains(&foreground) {
        return true;
    }
    if game_pid == 0 {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(foreground, &mut pid) };
    pid == game_pid
}

pub fn foreground_is_game_or_overlay(game_pid: u32) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    is_game_or_overlay(foreground, game_hwnd(), game_pid, &our_hwnds())
}

pub fn game_hwnd() -> Option<HWND> {
    session()
        .lock()
        .as_ref()
        .map(|s| s.game_hwnd as HWND)
        .filter(|h| !h.is_null())
}

struct Wake(HANDLE);

unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

impl Wake {
    fn new() -> Self {
        Self(unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) })
    }

    fn raise(&self) {
        if !self.0.is_null() {
            unsafe { SetEvent(self.0) };
        }
    }

    fn wait_or_message(&self, timeout_ms: u32) {
        let (count, handles) = if self.0.is_null() {
            (0, std::ptr::null())
        } else {
            (1, &self.0 as *const HANDLE)
        };
        unsafe {
            MsgWaitForMultipleObjectsEx(count, handles, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    pub drawer_open: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    wake: Arc<Wake>,
    pub game_hwnd: i64,
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

fn session() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

fn wake_tracker() {
    if let Some(session) = session().lock().as_ref() {
        session.wake.raise();
    }
}

pub fn create_windows(app: &AppHandle, rect: Rect) -> Result<(), String> {
    install_feedback_listeners(app);
    ensure_window(app, DRAWER_LABEL, ("overlay.html", "Peebify Overlay", "overlay"), rect)?;
    create_hud(app, rect)
}

fn create_hud(app: &AppHandle, rect: Rect) -> Result<(), String> {
    ensure_window(app, HUD_LABEL, ("hud.html", "Peebify Readout", "readout"), rect)
}

fn ensure_window(
    app: &AppHandle,
    label: &str,
    (page, title, what): (&str, &str, &str),
    rect: Rect,
) -> Result<(), String> {
    wait_until_retired(app, label);
    if let Some(window) = app.get_webview_window(label) {
        let Some(hwnd) = hwnd_of(&window) else {
            return Err(format!("the {what} window from the previous session has no handle"));
        };
        remember_hwnd(label, hwnd);
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(rect.width as f64, rect.height as f64)
        .position(rect.x as f64, rect.y as f64)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .focused(false)
        .visible(false)
        .additional_browser_args(WEBVIEW_BROWSER_ARGS)
        .build()
        .map_err(|e| format!("could not create the {what} window: {e}"))?;
    if label == HUD_LABEL {
        let _ = window.set_ignore_cursor_events(true);
    }
    let Some(hwnd) = hwnd_of(&window) else {
        let _ = window.destroy();
        return Err(format!(
            "the {what} window was registered but never came up. This happens when its browser arguments differ from the main window's, which WebView2 refuses."
        ));
    };
    remember_hwnd(label, hwnd);
    apply_styles(hwnd, label);
    Ok(())
}

pub fn ensure_hud(app: &AppHandle) {
    if HUD_HWND.load(Ordering::Relaxed) != 0 || HUD_CREATING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(target) = game_hwnd() else {
        HUD_CREATING.store(false, Ordering::SeqCst);
        return;
    };
    let app = app.clone();
    let raw = target as i64;
    tauri::async_runtime::spawn_blocking(move || {
        let rect = client_rect(raw as HWND).unwrap_or_default();
        match create_hud(&app, rect) {
            Ok(()) if game_hwnd() == Some(raw as HWND) => {
                log::info!("overlay: created the readout window on demand");
                let state = hook_state();
                state.windows_changed.store(true, Ordering::Relaxed);
                state.dirty.store(true, Ordering::Relaxed);
                wake_tracker();
            }
            Ok(()) => {
                HUD_HWND.store(0, Ordering::Relaxed);
                if let Some(window) = app.get_webview_window(HUD_LABEL) {
                    let _ = window.destroy();
                }
            }
            Err(e) => log::error!("overlay: {e}"),
        }
        HUD_CREATING.store(false, Ordering::SeqCst);
    });
}

pub fn destroy_windows(app: &AppHandle) {
    DRAWER_HWND.store(0, Ordering::Relaxed);
    HUD_HWND.store(0, Ordering::Relaxed);
    for label in [DRAWER_LABEL, HUD_LABEL] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.destroy();
        }
    }
}

pub fn start_tracking(app: AppHandle, game_hwnd: HWND, game_pid: u32) {
    stop_tracking();

    let stop = Arc::new(AtomicBool::new(false));
    let drawer_open = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Wake::new());
    if wake.0.is_null() {
        log::warn!("overlay: could not create the tracker's wake event, so it falls back to the reconciliation tick");
    }

    *session().lock() = Some(Session {
        stop: stop.clone(),
        drawer_open: drawer_open.clone(),
        finished: finished.clone(),
        wake: wake.clone(),
        game_hwnd: game_hwnd as i64,
    });

    let state = hook_state();
    state.game_hwnd.store(game_hwnd as i64, Ordering::Relaxed);
    state.destroyed.store(false, Ordering::Relaxed);
    state.dirty.store(true, Ordering::Relaxed);
    state.foreground_changed.store(true, Ordering::Relaxed);

    let raw_hwnd = game_hwnd as i64;
    std::thread::Builder::new()
        .name("overlay-winevent".into())
        .spawn(move || {
            track_loop(app, raw_hwnd, game_pid, stop, &wake, drawer_open);
            finished.store(true, Ordering::Relaxed);
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::error!("overlay: could not start the tracker thread: {e}"));
}

pub fn tracking_ended() -> bool {
    session()
        .lock()
        .as_ref()
        .map(|s| s.finished.load(Ordering::Relaxed))
        .unwrap_or(true)
}

pub fn stop_tracking() {
    if let Some(session) = session().lock().take() {
        session.stop.store(true, Ordering::Relaxed);
        session.wake.raise();
    }
}

pub fn set_drawer_open(open: bool) {
    if let Some(session) = session().lock().as_ref() {
        session.drawer_open.store(open, Ordering::Relaxed);
    }
}

pub fn drawer_is_open() -> bool {
    session()
        .lock()
        .as_ref()
        .is_some_and(|s| s.drawer_open.load(Ordering::Relaxed))
}

fn track_loop(
    app: AppHandle,
    raw_hwnd: i64,
    game_pid: u32,
    stop: Arc<AtomicBool>,
    wake: &Wake,
    drawer_open: Arc<AtomicBool>,
) {
    let game_hwnd = raw_hwnd as HWND;
    let hooks: Vec<HWINEVENTHOOK> = GAME_EVENT_RANGES
        .iter()
        .map(|&(first, last)| unsafe {
            SetWinEventHook(
                first,
                last,
                std::ptr::null_mut(),
                Some(win_event_proc),
                game_pid,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        })
        .filter(|hook| !hook.is_null())
        .collect();
    if hooks.len() < GAME_EVENT_RANGES.len() {
        log::warn!("overlay: SetWinEventHook failed — falling back to the reconciliation tick");
    }

    let foreground_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(foreground_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if foreground_hook.is_null() {
        log::warn!("overlay: the desktop foreground hook failed — leaving another app will take up to half a second to register");
    }

    let state = hook_state();
    let mut last_rect = Rect::default();
    let mut last_visible: Option<(bool, bool)> = None;
    let mut last_reconcile = std::time::Instant::now();
    let mut warned_fullscreen = false;
    let mut dirty_since: Option<std::time::Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        pump_messages();

        if state.destroyed.load(Ordering::Relaxed) {
            log::debug!("overlay: the game window went away");
            break;
        }

        if state.windows_changed.swap(false, Ordering::Relaxed) {
            last_rect = Rect::default();
            last_visible = None;
        }

        if state.dirty.swap(false, Ordering::Relaxed) {
            dirty_since = Some(std::time::Instant::now());
        }

        let now = std::time::Instant::now();
        let debounced =
            dirty_since.is_some_and(|t| now.duration_since(t).as_millis() >= DEBOUNCE_MS);
        let due = now.duration_since(last_reconcile).as_millis() >= RECONCILE_MS;

        if debounced || due {
            dirty_since = None;
            last_reconcile = now;
            reconcile(
                &app,
                game_hwnd,
                game_pid,
                &mut last_rect,
                &mut last_visible,
                &drawer_open,
                &mut warned_fullscreen,
                state.foreground_changed.swap(false, Ordering::Relaxed),
            );
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }
        wake.wait_or_message(wait_ms(
            dirty_since,
            last_reconcile,
            std::time::Instant::now(),
        ));
    }

    for hook in hooks {
        unsafe { UnhookWinEvent(hook) };
    }
    if !foreground_hook.is_null() {
        unsafe { UnhookWinEvent(foreground_hook) };
    }
    if drawer_open.swap(false, Ordering::Relaxed) {
        let _ = app.emit("overlay-closed", serde_json::Value::Null);
    }
    for label in [DRAWER_LABEL, HUD_LABEL] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.hide();
        }
    }
}

fn wait_ms(
    dirty_since: Option<std::time::Instant>,
    last_reconcile: std::time::Instant,
    now: std::time::Instant,
) -> u32 {
    let left = |since: std::time::Instant, span: u128| {
        span.saturating_sub(now.saturating_duration_since(since).as_millis())
    };
    let reconcile = left(last_reconcile, RECONCILE_MS);
    let settle = dirty_since.map_or(reconcile, |since| left(since, DEBOUNCE_MS));
    settle.min(reconcile) as u32
}

fn pump_messages() {
    let mut message: MSG = unsafe { core::mem::zeroed() };
    while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile(
    app: &AppHandle,
    game_hwnd: HWND,
    game_pid: u32,
    last_rect: &mut Rect,
    last_visible: &mut Option<(bool, bool)>,
    drawer_open: &Arc<AtomicBool>,
    warned_fullscreen: &mut bool,
    foreground_changed: bool,
) {
    let minimized = unsafe { IsIconic(game_hwnd) } != 0;
    let foreground = unsafe { GetForegroundWindow() };

    let ours: Vec<HWND> = our_hwnds();
    let foreground_is_relevant = foreground == game_hwnd || ours.contains(&foreground);
    if !foreground.is_null() {
        if let Some(state) = app.try_state::<super::BackendState>() {
            state.overlay.set_game_in_front(is_game_or_overlay(
                foreground,
                Some(game_hwnd),
                game_pid,
                &ours,
            ));
        }
    }

    let should_show = !minimized && foreground_is_relevant;
    let recording = app
        .try_state::<super::BackendState>()
        .is_some_and(|state| state.overlay.is_recording());
    let feedback = FEEDBACK_UNTIL_MS.load(Ordering::Relaxed) > now_ms();
    let hud_wanted = should_show && (recording || feedback);

    if !should_show && drawer_open.swap(false, Ordering::Relaxed) {
        log::info!("overlay: the foreground left the game, closing the drawer");
        let _ = app.emit("overlay-closed", serde_json::Value::Null);
        *last_visible = None;
    }
    let drawer_wanted = should_show && drawer_open.load(Ordering::Relaxed);

    if should_show {
        follow_client_rect(app, game_hwnd, last_rect);
    }

    if Some((drawer_wanted, hud_wanted)) != *last_visible {
        *last_visible = Some((drawer_wanted, hud_wanted));
        for label in [DRAWER_LABEL, HUD_LABEL] {
            let Some(window) = app.get_webview_window(label) else {
                continue;
            };
            let wanted = if label == DRAWER_LABEL {
                drawer_wanted
            } else {
                hud_wanted
            };
            let Some(hwnd) = cached_hwnd(label) else {
                continue;
            };
            let raw = hwnd as i64;
            let _ = app.run_on_main_thread(move || {
                let hwnd = raw as HWND;
                set_topmost(hwnd, wanted);
                apply_styles(hwnd, label);
            });
            let _ = if wanted { window.show() } else { window.hide() };
        }
    }

    if !should_show {
        return;
    }

    if foreground_changed {
        let covered: Vec<i64> = ours
            .iter()
            .filter(|hwnd| is_covered_by(**hwnd, game_hwnd))
            .map(|hwnd| *hwnd as i64)
            .collect();
        if !covered.is_empty() {
            let _ = app.run_on_main_thread(move || {
                for raw in covered {
                    set_topmost(raw as HWND, true);
                }
            });
        }

        if !*warned_fullscreen && is_exclusive_fullscreen(game_hwnd) {
            *warned_fullscreen = true;
            log::info!(
                "overlay: the game is in exclusive fullscreen, so the overlay cannot draw over it"
            );
            let _ = app.emit(FULLSCREEN_BLOCKED_EVENT, serde_json::Value::Null);
            super::platform::notify_if_backgrounded(
                app,
                "Peebify overlay",
                "Switch the game to Borderless or Windowed for the overlay to appear.",
            );
        }
    }
}

fn follow_client_rect(app: &AppHandle, game_hwnd: HWND, last_rect: &mut Rect) {
    if let Some(rect) = client_rect(game_hwnd) {
        if rect != *last_rect {
            let level = if *last_rect == Rect::default() {
                log::Level::Info
            } else {
                log::Level::Debug
            };
            log::log!(
                level,
                "overlay: game client area at {},{} size {}x{}",
                rect.x,
                rect.y,
                rect.width,
                rect.height
            );
            *last_rect = rect;
            for label in [DRAWER_LABEL, HUD_LABEL] {
                let Some(window) = app.get_webview_window(label) else {
                    continue;
                };
                let _ = window.set_position(tauri::PhysicalPosition::new(rect.x, rect.y));
                let _ = window.set_size(tauri::PhysicalSize::new(
                    rect.width as u32,
                    rect.height as u32,
                ));
            }
        }
    }
}

// ------------ Overlay Tests ------------
// Covers hotkey parsing and the clash report.

#[cfg(test)]
mod tests {
    use super::*;

    fn storable(text: &str) -> bool {
        parse_accelerator(text).is_some()
    }

    #[test]
    fn hotkey_report_splits_by_registration_bits() {
        let report = hotkey_report(|_| Value::Null, 0b011);
        assert_eq!(report.registered, vec!["Alt+P", "Alt+S"]);
        assert_eq!(report.unregistered, vec!["Alt+R"]);
        assert!(report.invalid.is_empty());
        assert!(!report.describe().contains("invalid"));

        let custom = hotkey_report(
            |key| {
                if key == "behavior.overlayShotHotkey" {
                    json!("Alt+Nope")
                } else {
                    json!("")
                }
            },
            0,
        );
        assert_eq!(custom.invalid, vec!["Alt+Nope"]);
        assert!(custom.registered.is_empty());
        assert!(custom.describe().starts_with("hotkeys registered: none;"));
        assert!(custom.describe().ends_with("; invalid: Alt+Nope"));
    }

    #[test]
    fn arrows_enter_and_backspace_map_to_their_virtual_keys() {
        assert_eq!(parse_accelerator("Alt+ArrowLeft"), Some((0x0001, 0x25)));
        assert_eq!(parse_accelerator("Alt+ArrowUp"), Some((0x0001, 0x26)));
        assert_eq!(parse_accelerator("Ctrl+ArrowRight"), Some((0x0002, 0x27)));
        assert_eq!(parse_accelerator("Shift+ArrowDown"), Some((0x0004, 0x28)));
        assert_eq!(parse_accelerator("Alt+Enter"), Some((0x0001, 0x0D)));
        assert_eq!(parse_accelerator("Alt+Backspace"), Some((0x0001, 0x08)));
    }

    #[test]
    fn modifier_order_and_case_do_not_hide_a_clash() {
        assert_eq!(parse_accelerator("Ctrl+Alt+S"), parse_accelerator("alt+ctrl+s"));
        assert_ne!(parse_accelerator("Alt+S"), parse_accelerator("Alt+Shift+S"));
    }

    #[test]
    fn rejects_shortcuts_the_helper_cannot_register() {
        assert!(!storable("Shift+!"));
        assert!(!storable("Alt+ScrollLock"));
        assert!(!storable("Alt+&"));
        assert!(!storable("Alt+Ы"));
        assert!(!storable("S"));
        assert!(!storable("F25"));
    }

    #[test]
    fn every_hotkey_has_a_matching_default() {
        let mut merged = json!({});
        super::super::config::sanitize_behavior(&mut merged);
        for (key, _, default) in HOTKEYS {
            assert_eq!(merged["behavior"][key], json!(default), "{key}");
            assert!(storable(default), "{key}");
        }
    }

    #[test]
    fn a_late_readout_replays_only_recent_capture_feedback() {
        let feedback = Feedback {
            at_ms: 10_000,
            event: "overlay-capture",
            payload: r#"{"path":"C:\\shots\\a.png"}"#.to_string(),
        };
        let replay = feedback_json(&feedback, 10_000 + FEEDBACK_HOLD_MS).unwrap();
        assert_eq!(replay["event"], "overlay-capture");
        assert_eq!(replay["payload"]["path"], "C:\\shots\\a.png");
        assert!(feedback_json(&feedback, 10_001 + FEEDBACK_HOLD_MS).is_none());

        let empty = Feedback {
            at_ms: 0,
            event: "overlay-record-failed",
            payload: String::new(),
        };
        assert_eq!(feedback_json(&empty, 0).unwrap()["payload"], serde_json::Value::Null);
    }

    #[test]
    fn the_tracker_sleeps_until_a_change_settles_or_the_next_tick() {
        use std::time::{Duration, Instant};
        let now = Instant::now() + Duration::from_secs(1);
        let ago = |ms: u64| now - Duration::from_millis(ms);
        assert_eq!(wait_ms(None, now, now), RECONCILE_MS as u32);
        assert_eq!(wait_ms(None, ago(200), now), 300);
        assert_eq!(wait_ms(None, ago(900), now), 0);
        assert_eq!(wait_ms(Some(now), now, now), DEBOUNCE_MS as u32);
        assert_eq!(wait_ms(Some(ago(10)), ago(100), now), 6);
        assert_eq!(wait_ms(Some(ago(10)), ago(495), now), 5);
        assert_eq!(wait_ms(Some(ago(40)), ago(100), now), 0);
    }

    #[test]
    fn shortcuts_belong_to_the_game_only_while_it_or_the_overlay_is_in_front() {
        let game = 0x1000 as HWND;
        let drawer = 0x2000 as HWND;
        let other = 0x3000 as HWND;
        let ours = [drawer];
        assert!(is_game_or_overlay(game, Some(game), 0, &ours));
        assert!(is_game_or_overlay(drawer, Some(game), 0, &ours));
        assert!(!is_game_or_overlay(other, Some(game), 0, &ours));
        assert!(!is_game_or_overlay(other, None, u32::MAX, &ours));
        assert!(!is_game_or_overlay(std::ptr::null_mut(), Some(game), 0, &ours));
    }
}
