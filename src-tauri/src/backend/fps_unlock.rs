// ------------ FPS Unlocker ------------
// Lets supported games run above their built in frame cap. The game is started through a small helper (peebify-fps-helper.exe) that Windows asks permission for, and this file tracks its status so the UI can show it.
// Only used for games whose profile has fpsUnlock turned on.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use peebify_helpers::fps::shared::{
    MAX_FPS, MIN_FPS, STATUS_ATTACHED, STATUS_FAILED, STATUS_INJECTING, STATUS_LAUNCHING,
    STATUS_READY,
};

use super::BackendState;
use super::{err_response, game_profiles, ok_with};

const HELPER_NAME: &str = "peebify-fps-helper.exe";

const DEFAULT_FPS: i64 = 120;
const DEFAULT_BACKGROUND_FPS: i64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    pub enabled: bool,
    pub target_fps: i32,
    pub background_fps: i32,
}

pub struct Launch {
    pub helper: PathBuf,
    pub arguments: Vec<String>,
}

pub fn supported(profile: &Value) -> bool {
    profile.get("fpsUnlock") == Some(&Value::Bool(true))
}

fn number(config: &super::config::LauncherConfig, key: &str, fallback: i64) -> i64 {
    match config.get(key) {
        Value::Number(n) => n.as_i64().unwrap_or(fallback),
        Value::String(s) => s.parse().unwrap_or(fallback),
        _ => fallback,
    }
}

pub fn settings(config: &super::config::LauncherConfig, game_id: &str) -> Settings {
    let enabled = config.get(&format!("games.{game_id}.fpsUnlock")) == Value::Bool(true);
    let target = number(
        config,
        &format!("games.{game_id}.fpsUnlockTarget"),
        DEFAULT_FPS,
    );
    let power_save =
        config.get(&format!("games.{game_id}.fpsUnlockPowerSave")) == Value::Bool(true);
    let background = number(
        config,
        &format!("games.{game_id}.fpsUnlockBackgroundFps"),
        DEFAULT_BACKGROUND_FPS,
    );
    Settings {
        enabled,
        target_fps: clamp_fps(target),
        background_fps: if power_save { clamp_fps(background) } else { 0 },
    }
}

fn clamp_fps(value: i64) -> i32 {
    value.clamp(MIN_FPS as i64, MAX_FPS as i64) as i32
}

fn cached_hint(config: &super::config::LauncherConfig, game_id: &str) -> (u64, u64) {
    let cache = config.get(&format!("games.{game_id}.fpsUnlockCache"));
    let read = |key: &str| -> u64 {
        match cache.get(key) {
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            _ => 0,
        }
    };
    (read("site"), read("fingerprint"))
}

fn remember_offset(app: &AppHandle, game_id: &str, site: u64, fingerprint: u64) {
    if site == 0 || fingerprint == 0 {
        return;
    }
    let config = app.state::<BackendState>().config.clone();
    if cached_hint(&config, game_id) == (site, fingerprint) {
        return;
    }
    config.set(
        &format!("games.{game_id}.fpsUnlockCache"),
        json!({ "site": site.to_string(), "fingerprint": fingerprint.to_string() }),
    );
}

fn status_label(status: u32) -> &'static str {
    match status {
        STATUS_LAUNCHING => "launching",
        STATUS_INJECTING => "attaching",
        STATUS_ATTACHED => "locating",
        STATUS_READY => "ready",
        STATUS_FAILED => "failed",
        _ => "idle",
    }
}

pub fn report_failure(app: &AppHandle, game_id: &str, display_name: &str, error: &str) {
    log::error!("[fps] could not start the unlocker for {game_id}: {error}");
    let _ = app.emit(
        "fps-unlock-status",
        json!({ "gameId": game_id, "state": "failed", "error": error }),
    );
    super::platform::notify_if_backgrounded(
        app,
        "FPS unlocker didn't start",
        &format!("{display_name} is starting at its normal frame rate. {error}"),
    );
}

fn emit_idle(app: &AppHandle, game_id: &str) {
    let _ = app.emit(
        "fps-unlock-status",
        json!({ "gameId": game_id, "state": "idle", "error": null }),
    );
}

mod live {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use serde_json::{json, Value};
    use tauri::{AppHandle, Emitter, Manager};

    use peebify_helpers::fps::section::{publish_settings, SectionView, Signal};
    use peebify_helpers::fps::shared::{
        SIGNAL_EVENT_NAME, STATUS_FAILED, STATUS_IDLE, STATUS_READY, STOP_ENDED, STOP_NONE,
        STOP_PAUSED, STUB_NAME,
    };

    use super::super::BackendState;
    use super::{cached_hint, remember_offset, settings, status_label, Launch, HELPER_NAME};

    const STATUS_POLL: Duration = Duration::from_millis(400);
    const STATUS_WATCH_LIMIT: Duration = Duration::from_secs(6 * 60);
    const STATUS_WATCH_HARD_LIMIT: Duration = Duration::from_secs(12 * 60);
    const NEVER_STARTED: &str =
        "The unlocker never started. The Windows prompt may have been declined.";
    const STALLED: &str = "The unlocker stopped responding before it could attach to the game.";

    struct Session {
        game_id: String,
        section: SectionView,
        signal: Option<Signal>,
    }

    fn session() -> &'static Mutex<Option<Session>> {
        static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();
        SESSION.get_or_init(|| Mutex::new(None))
    }

    pub fn prepare(
        app: &AppHandle,
        game_id: &str,
        executable: &std::path::Path,
        game_arguments: &[String],
        attach_to: Option<&str>,
    ) -> Result<Option<Launch>, String> {
        let config = app.state::<BackendState>().config.clone();
        let settings = settings(&config, game_id);
        if !settings.enabled {
            return Ok(None);
        }

        let helper = super::super::fs_util::resource(app, HELPER_NAME)
            .ok_or("The FPS unlocker's helper is missing from this install.")?;
        let helper = super::super::process_utils::resolve_subst(&helper);
        if !helper.with_file_name(STUB_NAME).is_file() {
            return Err("The FPS unlocker's stub is missing from this install.".to_string());
        }

        let section = SectionView::create().map_err(|code| {
            format!("Could not set up the unlocker's shared memory (error {code}).")
        })?;
        let shared = section.shared();
        shared.initialize();

        let (site, fingerprint) = cached_hint(&config, game_id);
        shared.hint_rva.store(site, Ordering::Relaxed);
        shared
            .hint_fingerprint
            .store(fingerprint, Ordering::Relaxed);
        publish_settings(&section, settings.target_fps, settings.background_fps);

        *session().lock() = Some(Session {
            game_id: game_id.to_string(),
            section,
            signal: Signal::open_or_create(SIGNAL_EVENT_NAME).ok(),
        });

        let mut arguments = match attach_to {
            Some(process) => vec!["--attach".to_string(), process.to_string()],
            None => vec![
                "--game".to_string(),
                super::super::process_utils::resolve_subst(executable)
                    .to_string_lossy()
                    .to_string(),
            ],
        };
        if attach_to.is_none() && !game_arguments.is_empty() {
            arguments.push("--".to_string());
            arguments.extend(game_arguments.iter().cloned());
        }

        log::info!(
            "[fps] {game_id} {} the unlocker at {} FPS{}, {}",
            match attach_to {
                Some(process) => format!("waiting for {process} to attach"),
                None => "launching through".to_string(),
            },
            settings.target_fps,
            if settings.background_fps > 0 {
                format!(" (background {})", settings.background_fps)
            } else {
                String::new()
            },
            if site != 0 {
                "reusing the cached offset"
            } else {
                "scanning for the offset"
            }
        );
        Ok(Some(Launch { helper, arguments }))
    }

    pub fn refresh(config: &super::super::config::LauncherConfig, game_id: &str) {
        let session = session().lock();
        let Some(session) = session.as_ref().filter(|s| s.game_id == game_id) else {
            return;
        };
        let settings = settings(config, game_id);
        publish_settings(
            &session.section,
            settings.target_fps,
            settings.background_fps,
        );
        if let Some(signal) = &session.signal {
            signal.raise();
        }
    }

    pub fn stop(game_id: &str) -> bool {
        let session = session().lock();
        let Some(session) = session.as_ref().filter(|s| s.game_id == game_id) else {
            return false;
        };
        let shared = session.section.shared();
        let holding = shared.status.load(Ordering::Acquire) == STATUS_READY;
        let next = if holding { STOP_PAUSED } else { STOP_ENDED };
        if shared
            .stop
            .compare_exchange(STOP_NONE, next, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if let Some(signal) = &session.signal {
            signal.raise();
        }
        if holding {
            log::info!(
                "[fps] {game_id} unlocker turned off while the game runs; its own cap is back"
            );
        } else {
            log::info!(
                "[fps] {game_id} unlocker turned off before it attached; it applies from the next launch"
            );
        }
        true
    }

    pub fn resume(game_id: &str) -> bool {
        let session = session().lock();
        let Some(session) = session.as_ref().filter(|s| s.game_id == game_id) else {
            return false;
        };
        let shared = session.section.shared();
        if shared
            .stop
            .compare_exchange(STOP_PAUSED, STOP_NONE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if let Some(signal) = &session.signal {
            signal.raise();
        }
        log::info!("[fps] {game_id} unlocker turned back on while the game runs");
        true
    }

    pub fn active(game_id: &str) -> bool {
        let session = session().lock();
        session.as_ref().is_some_and(|s| {
            s.game_id == game_id && s.section.shared().stop.load(Ordering::Acquire) == STOP_NONE
        })
    }

    pub fn end(game_id: &str) -> bool {
        let mut slot = session().lock();
        let Some(session) = slot.as_ref().filter(|s| s.game_id == game_id) else {
            return false;
        };
        let shared = session.section.shared();
        let resets = shared.resets.load(Ordering::Relaxed);
        shared.stop.store(STOP_ENDED, Ordering::Release);
        if let Some(signal) = &session.signal {
            signal.raise();
        }
        *slot = None;
        log::info!("[fps] {game_id} unlock session ended; the game reset the cap {resets} time(s)");
        true
    }

    pub fn snapshot() -> Value {
        let session = session().lock();
        let Some(session) = session.as_ref() else {
            return json!({ "state": "idle", "error": null });
        };
        let shared = session.section.shared();
        if shared.stop.load(Ordering::Acquire) != STOP_NONE {
            return json!({ "state": "idle", "error": null });
        }
        let status = shared.status.load(Ordering::Acquire);
        json!({
            "state": status_label(status),
            "error": (status == STATUS_FAILED).then(|| shared.error_message()),
        })
    }

    pub fn watch(app: &AppHandle, game_id: &str) {
        let app = app.clone();
        let game_id = game_id.to_string();
        tauri::async_runtime::spawn(async move {
            let started = Instant::now();
            let mut previous = STATUS_IDLE;
            loop {
                let Some(reading) = read(&game_id) else {
                    return;
                };
                if reading.stopped {
                    return;
                }

                if reading.status != previous {
                    previous = reading.status;
                    let _ = app.emit(
                        "fps-unlock-status",
                        json!({
                            "gameId": game_id,
                            "state": status_label(reading.status),
                            "error": reading.error.clone(),
                        }),
                    );
                    match reading.status {
                        STATUS_READY => {
                            remember_offset(&app, &game_id, reading.site, reading.fingerprint);
                            match reading.scan_micros {
                                0 => log::info!(
                                    "[fps] {game_id} reused the cached frame-rate offset"
                                ),
                                micros => log::info!(
                                    "[fps] {game_id} found the frame-rate field in {} ms{}",
                                    micros / 1000,
                                    if reading.site != 0 { "; caching it" } else { "" }
                                ),
                            }
                        }
                        STATUS_FAILED => {
                            let error = reading.error.unwrap_or_default();
                            log::warn!("[fps] {game_id} unlock failed: {error}");
                            super::super::platform::notify_if_backgrounded(
                                &app,
                                "FPS unlocker didn't start",
                                &error,
                            );
                            return;
                        }
                        _ => {}
                    }
                }

                if reading.status == STATUS_READY {
                    return;
                }
                if let Some(message) = expired(reading.status, started.elapsed()) {
                    if !give_up(&game_id, message) {
                        return;
                    }
                    continue;
                }
                tokio::time::sleep(STATUS_POLL).await;
            }
        });
    }

    fn expired(status: u32, elapsed: Duration) -> Option<&'static str> {
        match status {
            STATUS_READY | STATUS_FAILED => None,
            STATUS_IDLE if elapsed >= STATUS_WATCH_LIMIT => Some(NEVER_STARTED),
            _ if elapsed >= STATUS_WATCH_HARD_LIMIT => Some(STALLED),
            _ => None,
        }
    }

    fn give_up(game_id: &str, message: &str) -> bool {
        let session = session().lock();
        let Some(session) = session.as_ref().filter(|s| s.game_id == game_id) else {
            return false;
        };
        session.section.shared().fail(message);
        true
    }

    struct Reading {
        stopped: bool,
        status: u32,
        error: Option<String>,
        scan_micros: u32,
        site: u64,
        fingerprint: u64,
    }

    fn read(game_id: &str) -> Option<Reading> {
        let session = session().lock();
        let session = session.as_ref().filter(|s| s.game_id == game_id)?;
        let shared = session.section.shared();
        let status = shared.status.load(Ordering::Acquire);
        Some(Reading {
            stopped: shared.stop.load(Ordering::Acquire) != STOP_NONE,
            status,
            error: (status == STATUS_FAILED).then(|| shared.error_message()),
            scan_micros: shared.scan_micros.load(Ordering::Relaxed),
            site: shared.found_rva.load(Ordering::Relaxed),
            fingerprint: shared.found_fingerprint.load(Ordering::Acquire),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use peebify_helpers::fps::shared::{STATUS_ATTACHED, STATUS_LAUNCHING};

        #[test]
        fn idle_helper_is_reported_only_after_the_watch_limit() {
            assert_eq!(expired(STATUS_IDLE, Duration::from_secs(30)), None);
            assert_eq!(expired(STATUS_IDLE, STATUS_WATCH_LIMIT), Some(NEVER_STARTED));
        }

        #[test]
        fn helper_in_progress_gets_past_its_own_attach_wait() {
            assert_eq!(expired(STATUS_LAUNCHING, STATUS_WATCH_LIMIT), None);
            assert_eq!(expired(STATUS_ATTACHED, Duration::from_secs(11 * 60)), None);
            assert_eq!(
                expired(STATUS_LAUNCHING, STATUS_WATCH_HARD_LIMIT),
                Some(STALLED)
            );
        }

        #[test]
        fn settled_states_never_expire() {
            assert_eq!(expired(STATUS_READY, STATUS_WATCH_HARD_LIMIT), None);
            assert_eq!(expired(STATUS_FAILED, STATUS_WATCH_HARD_LIMIT), None);
        }
    }
}

pub fn prepare(
    app: &AppHandle,
    game_id: &str,
    profile: &Value,
    executable: &Path,
    game_arguments: &[String],
    attach_to: Option<&str>,
) -> Result<Option<Launch>, String> {
    if !supported(profile) {
        return Ok(None);
    }
    live::prepare(app, game_id, executable, game_arguments, attach_to)
}

pub use live::watch;

pub fn end_session(app: &AppHandle, game_id: &str) {
    if live::end(game_id) {
        emit_idle(app, game_id);
    }
}

pub(super) async fn get_fps_unlock(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let state = app.state::<BackendState>();
    let game_id = args
        .first()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| state.config.active_game_id());
    let profile = game_profiles::profile(&game_id);
    let settings = settings(&state.config, &game_id);
    let power_save = state
        .config
        .get(&format!("games.{game_id}.fpsUnlockPowerSave"))
        == Value::Bool(true);

    Ok(ok_with(json!({
        "gameId": game_id,
        "supported": supported(profile),
        "enabled": settings.enabled,
        "targetFps": settings.target_fps,
        "powerSave": power_save,
        "backgroundFps": number(
            &state.config,
            &format!("games.{game_id}.fpsUnlockBackgroundFps"),
            DEFAULT_BACKGROUND_FPS,
        ),
        "session": live::snapshot(),
        "appliesNextLaunch": applies_next_launch(&state, &game_id),
    })))
}

pub(crate) async fn set_fps_unlock(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(game_id) = args.first().and_then(|v| v.as_str()) else {
        return Ok(err_response("no game id"));
    };
    if !game_profiles::is_known_game_id(game_id) {
        return Ok(err_response("unknown game id"));
    }
    if !supported(game_profiles::profile(game_id)) {
        return Ok(err_response(
            "The FPS unlocker isn't available for this game.",
        ));
    }
    let Some(patch) = args.get(1).and_then(Value::as_object) else {
        return Ok(err_response("no settings given"));
    };

    let state = app.state::<BackendState>();
    for (key, value) in patch {
        let (setting, stored) = match (key.as_str(), value) {
            ("enabled", Value::Bool(on)) => ("fpsUnlock", json!(on)),
            ("powerSave", Value::Bool(on)) => ("fpsUnlockPowerSave", json!(on)),
            ("targetFps", value) => (
                "fpsUnlockTarget",
                json!(clamp_fps(value.as_i64().unwrap_or(DEFAULT_FPS))),
            ),
            ("backgroundFps", value) => (
                "fpsUnlockBackgroundFps",
                json!(clamp_fps(value.as_i64().unwrap_or(DEFAULT_BACKGROUND_FPS))),
            ),
            _ => continue,
        };
        state
            .config
            .set(&format!("games.{game_id}.{setting}"), stored);
    }

    let enabled = patch.get("enabled").and_then(Value::as_bool);
    if enabled == Some(false) && live::stop(game_id) {
        emit_idle(app, game_id);
    } else {
        live::refresh(&state.config, game_id);
        if enabled == Some(true) && live::resume(game_id) {
            let _ = app.emit(
                "fps-unlock-status",
                json!({ "gameId": game_id, "state": "ready", "error": null }),
            );
        }
    }
    Ok(ok_with(json!({
        "appliesNextLaunch": applies_next_launch(&state, game_id),
    })))
}

fn applies_next_launch(state: &BackendState, game_id: &str) -> bool {
    settings(&state.config, game_id).enabled
        && state.game.is_game_active_id(game_id)
        && !live::active(game_id)
}
