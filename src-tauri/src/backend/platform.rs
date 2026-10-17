// ------------ Windows Platform ------------
// Thin wrappers over Windows: toast notifications and the app identity behind them, the Run registry entry that starts
// the launcher at sign in, and the power settings that keep transfers fast and the PC awake.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Once, OnceLock};

use parking_lot::Mutex;
use serde_json::Value;
use tauri::{AppHandle, Manager};
use windows_sys::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
};

use super::{now_ms, BackendState};

// ------------ System Notifications ------------
// Shows Windows toast notifications, but only when the launcher is in the background and the player has turned
// them on. Also registers the app's identity with Windows so the toast shows the Peebify name and icon.

pub fn notify_if_backgrounded(app: &AppHandle, title: &str, body: &str) {
    let enabled = matches!(
        app.state::<BackendState>()
            .config
            .get("behavior.osNotifications"),
        Value::Bool(true)
    );
    if !enabled {
        log::debug!("[os-notify] Skipped \"{title}\": system notifications are off");
        return;
    }

    if let Some(window) = app.get_webview_window("main") {
        let visible = window.is_visible().unwrap_or(true);
        let minimized = window.is_minimized().unwrap_or(false);
        let focused = window.is_focused().unwrap_or(false);
        if visible && !minimized && focused {
            log::debug!("[os-notify] Skipped \"{title}\": the launcher window is focused");
            return;
        }
    }

    if let Err(e) = show(app, title, body) {
        log::warn!("[os-notify] Failed to show system notification \"{title}\": {e}");
    }
}

fn show(app: &AppHandle, title: &str, body: &str) -> Result<(), String> {
    use tauri_winrt_notification::{Duration, Toast};

    let app = app.clone();
    Toast::new(AUMID)
        .title(title)
        .text1(body)
        .duration(Duration::Short)
        .on_activated(move |_| {
            if let Some(state) = app.try_state::<BackendState>() {
                state.window.show_window();
            }
            Ok(())
        })
        .show()
        .map_err(|e| e.to_string())
}

pub const AUMID: &str = "com.peebify.launcher";

const DISPLAY_NAME: &str = "Peebify Launcher";

pub fn icon_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    let installed = dir.join("icons").join("app.png");
    if installed.exists() {
        return Some(installed);
    }

    let dev = dir.parent()?.parent()?.join("webui/public/icons/app.png");
    dev.exists().then_some(dev)
}

fn describe_to_shell() -> std::io::Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))?;
    key.set_value("DisplayName", &DISPLAY_NAME)?;
    key.set_value("ShowInSettings", &1u32)?;

    match icon_path() {
        Some(icon) => key.set_value("IconUri", &icon.as_os_str())?,
        None => log::warn!("[aumid] app.png not found; toasts will show no icon"),
    }
    Ok(())
}

pub fn register_app_id() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    if let Err(e) = describe_to_shell() {
        log::warn!("[aumid] could not describe {AUMID} to the shell: {e}");
    }

    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(peebify_helpers::common::wide(AUMID).as_ptr()) };
    if hr < 0 {
        log::warn!("[aumid] SetCurrentProcessExplicitAppUserModelID failed: 0x{hr:08X}");
    } else {
        log::info!("[aumid] process identity set to {AUMID}");
    }
}

// ------------ Windows Startup ------------
// Adds or removes the launcher from Windows sign in through the Run registry key, and clears the flag Windows keeps
// when someone turned it off in Startup Apps. Boot launches are marked with --from-boot.

pub(crate) const APP_NAME: &str = "PeebifyLauncher";
pub(crate) const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

static BOOT_LAUNCH: OnceLock<bool> = OnceLock::new();

pub fn is_boot_launch() -> bool {
    *BOOT_LAUNCH.get_or_init(|| std::env::args().any(|a| a == "--from-boot"))
}

pub type StartupNote = Option<String>;

fn approval_disables(bytes: &[u8]) -> bool {
    bytes.first().is_some_and(|b| b & 1 == 1)
}

fn hkcu() -> winreg::RegKey {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
}

fn delete_app_value(subkey: &str) -> std::io::Result<bool> {
    use std::io::ErrorKind::NotFound;

    let key = match hkcu().open_subkey_with_flags(subkey, winreg::enums::KEY_SET_VALUE) {
        Ok(key) => key,
        Err(e) if e.kind() == NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match key.delete_value(APP_NAME) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) fn disabled_in_startup_apps() -> bool {
    hkcu()
        .open_subkey(STARTUP_APPROVED_SUBKEY)
        .and_then(|key| key.get_raw_value(APP_NAME))
        .is_ok_and(|value| approval_disables(&value.bytes))
}

pub(crate) fn boot_command() -> Result<String, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not resolve launcher path: {e}"))?;
    Ok(format!("\"{}\" --from-boot", exe.to_string_lossy()))
}

pub(crate) fn write_run_value(command: &str) -> Result<(), String> {
    hkcu()
        .create_subkey(RUN_SUBKEY)
        .and_then(|(key, _)| key.set_value(APP_NAME, &command))
        .map_err(|e| format!("could not write the Windows sign in entry: {e}"))
}

pub async fn manage_windows_startup(enable: bool) -> Result<StartupNote, String> {
    if enable {
        write_run_value(&boot_command()?)?;
        if !disabled_in_startup_apps() {
            return Ok(None);
        }
        return Ok(match delete_app_value(STARTUP_APPROVED_SUBKEY) {
            Ok(_) => {
                log::info!(
                    "[startup] cleared the disabled flag Windows startup apps kept for {APP_NAME}"
                );
                None
            }
            Err(e) => {
                log::warn!("[startup] could not clear the Windows startup apps flag: {e}");
                Some(
                    "Peebify was added to Windows sign in, but Windows startup apps still has it \
                     turned off. Turn it on in Settings > Apps > Startup."
                        .to_string(),
                )
            }
        });
    }

    delete_app_value(RUN_SUBKEY)
        .map_err(|e| format!("could not remove the Windows sign in entry: {e}"))?;
    Ok(None)
}

// ------------ Transfer Performance ------------
// How many threads verify files. While a transfer is running it asks Windows not to throttle us or put the PC to
// sleep, and after the PC wakes up it rechecks the network.

pub fn validation_workers() -> usize {
    (peebify_transfer::cores() / 2).clamp(4, 8)
}

static TRANSFERS_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static TRANSFERS_IDLE: AtomicUsize = AtomicUsize::new(0);
static HOLDING_AWAKE: Mutex<bool> = Mutex::new(false);

const IDLE_PAUSED: u8 = 1;
const IDLE_OFFLINE: u8 = 2;

pub fn allow_full_speed() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE,
    };

    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    let ok = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::addr_of!(state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if ok == 0 {
        log::warn!(
            "[perf] Could not opt out of processor power throttling ({}). Downloads may run at reduced speed on laptops.",
            std::io::Error::last_os_error()
        );
    } else {
        log::info!("[perf] Opted out of processor power throttling for this process.");
    }
}

fn awake_channel() -> &'static std::sync::mpsc::Sender<bool> {
    use std::sync::mpsc;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };

    static TX: OnceLock<mpsc::Sender<bool>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("peebify-power".into())
            .spawn(move || {
                while let Ok(keep_awake) = rx.recv() {
                    let flags = if keep_awake {
                        ES_CONTINUOUS | ES_SYSTEM_REQUIRED
                    } else {
                        ES_CONTINUOUS
                    };
                    if unsafe { SetThreadExecutionState(flags) } == 0 {
                        log::warn!("[perf] Could not update the system sleep request.");
                    }
                }
            })
            .expect("the power thread cannot fail to spawn");
        tx
    })
}

fn set_awake(keep_awake: bool) {
    let _ = awake_channel().send(keep_awake);
}

pub fn transfers_active() -> usize {
    TRANSFERS_ACTIVE.load(Ordering::SeqCst)
}

fn refresh_awake() -> bool {
    let mut holding = HOLDING_AWAKE.lock();
    let want = TRANSFERS_ACTIVE.load(Ordering::SeqCst) > TRANSFERS_IDLE.load(Ordering::SeqCst);
    if want == *holding {
        return false;
    }
    *holding = want;
    set_awake(want);
    true
}

fn idle_change(before: u8, reason: u8, on: bool) -> Option<bool> {
    let after = if on { before | reason } else { before & !reason };
    match (before != 0, after != 0) {
        (false, true) => Some(true),
        (true, false) => Some(false),
        _ => None,
    }
}

pub struct TransferGuard {
    idle: AtomicU8,
}

impl TransferGuard {
    pub fn acquire() -> Self {
        if TRANSFERS_ACTIVE.fetch_add(1, Ordering::SeqCst) == 0 {
            log::info!("[perf] Transfer started — holding the system awake and unthrottled.");
        }
        refresh_awake();
        Self {
            idle: AtomicU8::new(0),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.set_idle_reason(IDLE_PAUSED, paused);
    }

    pub fn set_offline(&self, offline: bool) {
        self.set_idle_reason(IDLE_OFFLINE, offline);
    }

    fn set_idle_reason(&self, reason: u8, on: bool) {
        let before = if on {
            self.idle.fetch_or(reason, Ordering::SeqCst)
        } else {
            self.idle.fetch_and(!reason, Ordering::SeqCst)
        };
        match idle_change(before, reason, on) {
            Some(true) => {
                TRANSFERS_IDLE.fetch_add(1, Ordering::SeqCst);
                if refresh_awake() {
                    log::info!("[perf] Transfer paused or waiting for the network, released the sleep request.");
                }
            }
            Some(false) => {
                TRANSFERS_IDLE.fetch_sub(1, Ordering::SeqCst);
                if refresh_awake() {
                    log::info!("[perf] Transfer moving again, holding the system awake.");
                }
            }
            None => {}
        }
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        if *self.idle.get_mut() != 0 {
            TRANSFERS_IDLE.fetch_sub(1, Ordering::SeqCst);
        }
        if TRANSFERS_ACTIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
            log::info!("[perf] Transfers finished — released the system sleep request.");
        }
        refresh_awake();
    }
}

static SUSPENDED_AT_MS: AtomicI64 = AtomicI64::new(0);
static REGISTER: Once = Once::new();

fn suspend_message(transfers_active: usize) -> String {
    format!("[power] System suspending (transfers active: {transfers_active}).")
}

fn resume_message(suspended_at_ms: i64, now_ms: i64) -> String {
    if suspended_at_ms <= 0 {
        return "[power] System resumed.".to_string();
    }
    let secs = (now_ms - suspended_at_ms).max(0) / 1000;
    format!("[power] Resumed after {secs}s.")
}

unsafe extern "system" fn on_power_event(
    _context: *const core::ffi::c_void,
    kind: u32,
    _setting: *const core::ffi::c_void,
) -> u32 {
    match kind {
        PBT_APMSUSPEND => {
            SUSPENDED_AT_MS.store(now_ms(), Ordering::SeqCst);
            log::info!("{}", suspend_message(transfers_active()));
        }
        PBT_APMRESUMEAUTOMATIC => {
            let suspended_at = SUSPENDED_AT_MS.swap(0, Ordering::SeqCst);
            log::info!("{}", resume_message(suspended_at, now_ms()));
            super::http::recheck_after_resume();
        }
        _ => {}
    }
    0
}

pub fn watch_suspend_resume() {
    REGISTER.call_once(|| {
        let params: &'static DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
            Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power_event),
                Context: std::ptr::null_mut(),
            }));
        let mut handle: *mut core::ffi::c_void = std::ptr::null_mut();
        let status = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                (params as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS)
                    .cast_mut()
                    .cast(),
                &mut handle,
            )
        };
        if status != 0 {
            log::warn!(
                "[power] Could not subscribe to suspend and resume notifications ({}).",
                std::io::Error::from_raw_os_error(status as i32)
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_approval_odd_first_byte_means_disabled() {
        assert!(approval_disables(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(approval_disables(&[0x07, 0, 0, 0]));
        assert!(approval_disables(&[0x01]));
    }

    #[test]
    fn startup_approval_even_or_missing_first_byte_means_enabled() {
        assert!(!approval_disables(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!approval_disables(&[0x06]));
        assert!(!approval_disables(&[]));
    }

    #[test]
    fn a_job_counts_as_idle_once_whatever_the_reasons() {
        assert_eq!(idle_change(0, IDLE_PAUSED, true), Some(true));
        assert_eq!(idle_change(IDLE_PAUSED, IDLE_PAUSED, true), None);
        assert_eq!(idle_change(IDLE_PAUSED, IDLE_OFFLINE, true), None);
        assert_eq!(idle_change(IDLE_PAUSED | IDLE_OFFLINE, IDLE_OFFLINE, false), None);
        assert_eq!(idle_change(IDLE_PAUSED, IDLE_PAUSED, false), Some(false));
        assert_eq!(idle_change(0, IDLE_OFFLINE, false), None);
    }

    #[test]
    fn resume_message_reports_whole_seconds_asleep() {
        assert_eq!(
            resume_message(1_000_000, 1_125_900),
            "[power] Resumed after 125s."
        );
    }

    #[test]
    fn resume_message_without_a_recorded_suspend() {
        assert_eq!(resume_message(0, 1_125_900), "[power] System resumed.");
    }

    #[test]
    fn resume_message_clamps_a_backwards_clock() {
        assert_eq!(
            resume_message(2_000_000, 1_000_000),
            "[power] Resumed after 0s."
        );
    }
}
