// ------------ Game Manager ------------
// Launches a game, watches its process while it runs, and keeps track of which games are running so the rest of the launcher can react.
// It also answers the update check for each game, runs the hourly auto update sweep, and exposes the launch, running
// game and Steam commands the UI calls.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::process::Child;

use super::manifest;
use super::validator::GAME_CONFIG_FILE;
use super::playtime::{local_day_key, PlaytimeTracker, ProcessIdentity};
use super::BackendState;
use super::{
    config, err_response, fps_unlock, fs_util, game_profiles, http, mods, ok_with,
    process_utils, steam, xxmi,
};

const PROCESS_MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const PID_RECONFIRM_INTERVAL: Duration = Duration::from_secs(4);
const LAUNCH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(90);
const STEAM_LAUNCH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const CUSTOM_LAUNCH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const STEAM_UPDATE_WATCH_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
const PASSIVE_WATCH_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const LAUNCH_FAILED_GRACE: Duration = Duration::from_secs(3);
const LAUNCH_HANDOFF_MAX_WAIT: Duration = Duration::from_secs(10 * 60);
const LAUNCH_NEVER_STARTED: &str = "The game never started. It may have been cancelled at the Windows permission prompt or blocked by security software.";
const STEAM_LAUNCH_NEVER_STARTED: &str =
    "Steam never started the game. Check Steam for an update or a prompt.";
const STEAM_LAUNCH_STILL_WAITING: &str = "Steam hasn't started the game yet. Check Steam for an update or a prompt. Peebify still tracks the game once Steam starts it.";
const FPS_PROMPT_DECLINED: &str = "You declined the Windows prompt.";
const UPDATE_CACHE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MISSING_GRACE: Duration = Duration::from_secs(10);

struct GameSession {
    proc_name: String,
    pid: Option<u32>,
    started_at: Option<i64>,
    last_pid_reconfirm: Instant,
    launch_deadline: Option<Instant>,
    passive: bool,
    via_steam: bool,
    handed_off: bool,
    last_seen_ms: i64,
    missing_since: Option<Instant>,
    next_probe: Instant,
}

impl GameSession {
    fn new(proc_name: String) -> Self {
        Self {
            proc_name,
            pid: None,
            started_at: None,
            last_pid_reconfirm: Instant::now(),
            launch_deadline: None,
            passive: false,
            via_steam: false,
            handed_off: true,
            last_seen_ms: 0,
            missing_since: None,
            next_probe: Instant::now(),
        }
    }

    fn is_running(&self) -> bool {
        self.started_at.is_some()
    }

    fn probe_due(&mut self, now: Instant) -> bool {
        if !self.passive || self.is_running() {
            return true;
        }
        if now < self.next_probe {
            return false;
        }
        self.next_probe = now + PASSIVE_WATCH_PROBE_INTERVAL;
        true
    }

    fn counts_as_active(&self) -> bool {
        !self.passive || self.is_running()
    }
}

enum Transition {
    None,
    Started,
    Stopped { duration_ms: i64, ended_ms: i64 },
    LaunchFailed,
    SteamStillStarting,
    WatchExpired,
}

#[derive(Default)]
struct Monitor {
    sessions: HashMap<String, GameSession>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

pub struct GameManager {
    app: AppHandle,
    pub playtime: PlaytimeTracker,
    monitor: Mutex<Monitor>,
    launching: Mutex<HashSet<String>>,
    update_cache: Mutex<HashMap<String, (Value, Instant)>>,
    update_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct LaunchingGuard {
    manager: Arc<GameManager>,
    game_id: String,
}

impl Drop for LaunchingGuard {
    fn drop(&mut self) {
        self.manager.launching.lock().remove(&self.game_id);
        if let Some(state) = self.manager.app.try_state::<BackendState>() {
            state.window.update_tray_menu();
        }
    }
}

impl GameManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            playtime: PlaytimeTracker::new(),
            monitor: Mutex::new(Monitor::default()),
            launching: Mutex::new(HashSet::new()),
            update_cache: Mutex::new(HashMap::new()),
            update_locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_game_active_id(&self, game_id: &str) -> bool {
        self.monitor
            .lock()
            .sessions
            .get(game_id)
            .is_some_and(GameSession::counts_as_active)
            || self.launching.lock().contains(game_id)
    }

    pub fn is_any_game_active(&self) -> bool {
        self.monitor
            .lock()
            .sessions
            .values()
            .any(GameSession::counts_as_active)
            || !self.launching.lock().is_empty()
    }

    fn has_other_session(&self, game_id: &str) -> bool {
        self.monitor
            .lock()
            .sessions
            .iter()
            .any(|(id, session)| id != game_id && session.counts_as_active())
            || self.launching.lock().iter().any(|id| id != game_id)
    }

    pub fn is_game_running(&self) -> bool {
        self.monitor
            .lock()
            .sessions
            .values()
            .any(GameSession::is_running)
    }

    pub fn is_game_running_id(&self, game_id: &str) -> bool {
        self.monitor
            .lock()
            .sessions
            .get(game_id)
            .is_some_and(GameSession::is_running)
    }

    pub fn running_pid(&self, game_id: &str) -> Option<u32> {
        self.monitor
            .lock()
            .sessions
            .get(game_id)
            .filter(|s| s.is_running())
            .and_then(|s| s.pid)
    }

    pub fn running_game(&self) -> Option<(String, Option<u32>)> {
        self.monitor
            .lock()
            .sessions
            .iter()
            .find(|(_, s)| s.is_running())
            .map(|(id, s)| (id.clone(), s.pid))
    }

    fn running_games(&self) -> Vec<String> {
        let mut games: Vec<String> = self
            .monitor
            .lock()
            .sessions
            .iter()
            .filter(|(_, s)| s.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        games.sort();
        games
    }

    fn config(&self) -> Arc<super::config::LauncherConfig> {
        self.app.state::<BackendState>().config.clone()
    }

    fn active_game_id(&self) -> String {
        self.config().active_game_id()
    }

    fn resolve_game_id(&self, target: Option<&str>) -> String {
        match target {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => self.active_game_id(),
        }
    }

    async fn emit_game_event(&self, channel: &str, payload: Value) {
        if let Err(e) = self.app.emit(channel, payload.clone()) {
            log::warn!("failed to emit '{channel}' to webview: {e}");
        }
    }

    fn graphics_api_args(&self, profile_id: &str, profile: &Value) -> Vec<String> {
        if mods::active_for(&self.app, profile_id, profile) {
            let forced = game_profiles::mod_force_dx11_args(profile);
            if !forced.is_empty() {
                log::info!("Mods are on for {profile_id} — forcing DX11 ({forced:?})");
                return forced;
            }
        }

        let Some(by_api) = profile.get("graphicsApiArgs") else {
            return Vec::new();
        };
        let choice = match self
            .config()
            .get(&format!("games.{profile_id}.graphicsApi"))
        {
            Value::String(s) if by_api.get(&s).is_some() => s,
            _ => profile
                .get("graphicsApiDefault")
                .and_then(Value::as_str)
                .unwrap_or("dx11")
                .to_string(),
        };
        by_api
            .get(&choice)
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn resource_quality_args(&self, profile: &Value, install_root: &Path) -> Vec<String> {
        let Some(by_quality) = profile.get("resourceQualityArgs") else {
            return Vec::new();
        };
        let Some(selected) = super::kuro::selected_quality(&self.app, profile) else {
            return Vec::new();
        };
        let choice = super::kuro::launch_quality(profile, install_root, &selected);
        log::info!(
            "{}: launching at {choice} quality (selected {selected}, on disk {:?})",
            game_profiles::profile_id(profile),
            super::kuro::qualities_on_disk(profile, install_root)
        );
        by_quality
            .get(&choice)
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn resolve_install_root(
        &self,
        profile_id: &str,
        profile: &'static Value,
    ) -> Option<PathBuf> {
        let game_path_str = match self.config().get(&format!("games.{profile_id}.gamePath")) {
            Value::String(s) if !s.is_empty() => s,
            _ => {
                log::debug!("{profile_id} has no configured game path");
                return None;
            }
        };

        let game_path_for_walk = game_path_str.clone();
        let install_root = tauri::async_runtime::spawn_blocking(move || {
            if let Some(marker) = game_profiles::install_root_marker(profile) {
                game_profiles::find_install_root_by_marker(Path::new(&game_path_for_walk), &marker, 4)
            } else {
                game_profiles::find_game_install_root(
                    Path::new(&game_path_for_walk),
                    game_profiles::executable_name(profile),
                    4,
                )
            }
        })
        .await
        .ok()
        .flatten();

        let Some(install_root) = install_root else {
            log::error!("Game executable not found under {game_path_str}");
            return None;
        };
        let install_root_str = install_root.to_string_lossy().to_string();
        if install_root_str != game_path_str {
            log::info!("Resolved {profile_id} install root: {install_root_str}");
            config::set_config_value(
                &self.app,
                &format!("games.{profile_id}.gamePath"),
                json!(install_root_str),
            );
        }
        Some(install_root)
    }

    pub async fn steam_app_id(&self, profile_id: &str, profile: &'static Value) -> Option<String> {
        steam::app_id(profile)?;
        let root = self.resolve_install_root(profile_id, profile).await?;
        steam::detect(&root, profile)
    }

    pub async fn steam_install_info(&self, target_game_id: Option<&str>) -> Value {
        let game_id = self.resolve_game_id(target_game_id);
        let profile = game_profiles::profile(&game_id);
        let profile_id = game_profiles::profile_id(profile).to_string();

        let app_id = self.steam_app_id(&profile_id, profile).await;

        ok_with(json!({
            "gameId": profile_id,
            "isSteamInstall": app_id.is_some(),
            "steamFound": app_id.is_some() && steam::steam_exe().is_some(),
        }))
    }

    // ------------ Launching Games ------------
    // Works out the install folder, launch arguments, FPS unlocker, mods and Steam or custom launcher hand off, then starts the game and begins watching it.
    pub async fn launch_game(self: &Arc<Self>, target_game_id: Option<&str>) -> Value {
        let game_id = self.resolve_game_id(target_game_id);
        let profile = game_profiles::profile(&game_id);
        let profile_id = game_profiles::profile_id(profile).to_string();
        let display_name = game_profiles::display_name(profile).to_string();

        match self.monitor.lock().sessions.get(&profile_id) {
            Some(session) if session.is_running() => {
                log::warn!("Launch attempted while {profile_id} is already running");
                return err_response(format!("{display_name} is already running."));
            }
            Some(session) if session.passive => {}
            Some(_) => {
                log::warn!("Launch attempted while {profile_id} is still starting");
                return err_response(format!("{display_name} is already starting."));
            }
            None => {}
        }
        if let Some(op) = self
            .app
            .state::<BackendState>()
            .engine
            .queue
            .launch_blocker(&profile_id)
        {
            log::warn!("Launch attempted while a {op} for {profile_id} is queued or running");
            return err_response(busy_launch_message(&display_name, &op));
        }
        if self.update_left_unfinished(&profile_id) {
            log::warn!("Launch refused: the last update of {profile_id} didn't finish");
            return err_response(format!(
                "{display_name}'s last update didn't finish. Finish the update before playing."
            ));
        }
        if !self.launching.lock().insert(profile_id.clone()) {
            log::warn!("Launch attempted while {profile_id} is still starting");
            return err_response(format!("{display_name} is already starting."));
        }
        let _launching = LaunchingGuard {
            manager: Arc::clone(self),
            game_id: profile_id.clone(),
        };
        self.app.state::<BackendState>().window.update_tray_menu();

        let Some(install_root) = self.resolve_install_root(&profile_id, profile).await else {
            return err_response("Game executable not found. Please verify game files.");
        };

        let proc_name = game_profiles::client_process_name(profile).to_string();
        if let Some(pid) = process_utils::find_by_name_under(&proc_name, &install_root).await {
            log::info!(
                "{profile_id} is already running outside Peebify (pid {pid}), tracking it instead of starting another copy"
            );
            if self.adopt_running(&profile_id, proc_name, pid) {
                self.handle_game_started(&profile_id).await;
            }
            return ok_with(json!({ "viaSteam": false }));
        }

        let (executable_path, mut launch_args) = game_profiles::resolve_launch(&install_root, profile);
        launch_args.extend(self.graphics_api_args(&profile_id, profile));
        launch_args.extend(self.resource_quality_args(profile, &install_root));
        if let Value::String(custom) = self.config().get(&format!("games.{profile_id}.launchArgs"))
        {
            launch_args.extend(split_launch_args(&custom));
        }

        if !executable_path.exists() {
            log::error!("Game executable not found at {}", executable_path.display());
            return err_response("Game executable not found. Please verify game files.");
        }

        let args_suffix = if launch_args.is_empty() {
            String::new()
        } else {
            format!(" {}", launch_args.join(" "))
        };

        let custom_launcher = self.custom_launcher(&profile_id, &display_name);
        let via_steam = match custom_launcher {
            Some(_) => None,
            None => self.steam_launch_target(&profile_id, &install_root, profile),
        };
        let steam_launch = via_steam.is_some();
        let loader_wait = if custom_launcher.is_some() {
            CUSTOM_LAUNCH_CONFIRM_TIMEOUT
        } else if via_steam.is_some() {
            STEAM_LAUNCH_CONFIRM_TIMEOUT
        } else {
            LAUNCH_CONFIRM_TIMEOUT
        };

        let mut mods_armed = false;
        if mods::active_for(&self.app, &profile_id, profile) {
            let state = self.app.state::<BackendState>();
            let root = xxmi::root(&state.config, &state.user_data);
            mods::log_launch_state(&self.app, &profile_id, profile);

            match xxmi::prepare_and_spawn(&self.app, &root, profile, loader_wait).await {
                Ok(()) => mods_armed = true,
                Err(e) => {
                    log::error!("Mod loader failed to start for {profile_id}: {e}");
                    super::platform::notify_if_backgrounded(
                        &self.app,
                        "Mods didn't load",
                        &format!("{display_name} is starting without mods. {e}"),
                    );
                    self.emit_game_event(
                        "mods-load-failed",
                        json!({ "gameId": profile_id, "error": e }),
                    )
                    .await;
                }
            }
        }

        let unlock = match via_steam {
            Some(_) => None,
            None => match fps_unlock::prepare(
                &self.app,
                &profile_id,
                profile,
                &executable_path,
                &launch_args,
                custom_launcher
                    .is_some()
                    .then(|| game_profiles::client_process_name(profile)),
            ) {
                Ok(prepared) => prepared,
                Err(e) => {
                    fps_unlock::report_failure(&self.app, &profile_id, &display_name, &e);
                    None
                }
            },
        };
        let mut unlocked = unlock.is_some();
        let via_helper = custom_launcher.is_none() && via_steam.is_none() && unlocked;

        let child = match custom_launcher {
            Some(launcher) => {
                if let Some(unlock) = &unlock {
                    log::info!(
                        "Starting the FPS unlocker first so it can attach once the game appears"
                    );
                    if let Err(e) = process_utils::launch_game_via_shell(
                        &unlock.helper,
                        &unlock.arguments,
                        unlock.helper.parent(),
                    )
                    .await
                    {
                        let reason = if e.declined {
                            FPS_PROMPT_DECLINED.to_string()
                        } else {
                            e.message
                        };
                        fps_unlock::end_session(&self.app, &profile_id);
                        fps_unlock::report_failure(&self.app, &profile_id, &display_name, &reason);
                        unlocked = false;
                    }
                }
                log::info!(
                    "Handing the launch to {}, which starts the game itself{}",
                    launcher.display(),
                    if launch_args.is_empty() {
                        String::new()
                    } else {
                        format!(" (leaving{args_suffix} behind)")
                    }
                );
                let launched =
                    process_utils::launch_game_via_shell(&launcher, &[], launcher.parent())
                        .await
                        .map(|()| None);
                launched
            }
            None => match via_steam {
                Some((steam_exe, app_id)) => {
                    log::info!("Launching game through Steam (app {app_id}):{args_suffix}");
                    steam::launch(&steam_exe, &app_id, &launch_args)
                        .await
                        .map(Some)
                        .map_err(|message| process_utils::SpawnError {
                            declined: false,
                            message,
                        })
                }
                None => match unlock {
                    Some(unlock) => {
                        log::info!(
                            "Launching game through the FPS unlocker: {}{args_suffix}",
                            executable_path.display()
                        );
                        process_utils::launch_game_via_shell(
                            &unlock.helper,
                            &unlock.arguments,
                            executable_path.parent(),
                        )
                        .await
                        .map(|()| None)
                    }
                    None => {
                        let hide_front_end =
                            game_profiles::launches_hidden_front_end(profile, &executable_path);
                        log::info!(
                            "Launching game from: {}{args_suffix}{}",
                            executable_path.display(),
                            if hide_front_end {
                                " (official front end kept hidden)"
                            } else {
                                ""
                            }
                        );
                        let launched = if hide_front_end {
                            process_utils::launch_game_minimized(
                                &executable_path,
                                &launch_args,
                                executable_path.parent(),
                            )
                            .await
                        } else {
                            process_utils::launch_game_via_shell(
                                &executable_path,
                                &launch_args,
                                executable_path.parent(),
                            )
                            .await
                        };
                        if hide_front_end && launched.is_ok() {
                            super::process_utils::front_end::conceal_until_client(
                                game_profiles::hidden_front_end(profile),
                                game_profiles::client_process_name(profile).to_string(),
                            );
                        }
                        launched.map(|()| None)
                    }
                },
            },
        };
        let child = match child {
            Ok(child) => child,
            Err(e) => {
                log::error!(
                    "Failed to launch game ({profile_id}): {}{}",
                    e.message,
                    if e.declined { " (permission prompt declined)" } else { "" }
                );
                if unlocked {
                    fps_unlock::end_session(&self.app, &profile_id);
                }
                if mods_armed && !self.has_other_session(&profile_id) {
                    xxmi::kill_orphan_loader().await;
                }
                return err_response(launch_error(&display_name, &e, via_helper));
            }
        };

        self.begin_launch(&profile_id, proc_name, child, loader_wait, steam_launch);
        if unlocked {
            fps_unlock::watch(&self.app, &profile_id);
        }
        ok_with(json!({ "viaSteam": steam_launch }))
    }

    fn custom_launcher(&self, profile_id: &str, display_name: &str) -> Option<PathBuf> {
        let Value::String(configured) = self
            .config()
            .get(&format!("games.{profile_id}.customLauncher"))
        else {
            return None;
        };
        let trimmed = configured.trim();
        if trimmed.is_empty() {
            return None;
        }

        let path = PathBuf::from(trimmed);
        if path.is_file() {
            return Some(path);
        }

        log::warn!(
            "{profile_id}: the custom launcher {trimmed} is missing, starting the game directly"
        );
        super::platform::notify_if_backgrounded(
            &self.app,
            "The custom launcher is missing",
            &format!("{trimmed} is no longer there, so {display_name} is starting on its own."),
        );
        let _ = self.app.emit(
            "custom-launcher-missing",
            json!({ "gameId": profile_id, "path": trimmed }),
        );
        None
    }

    fn steam_launch_target(
        &self,
        profile_id: &str,
        install_root: &Path,
        profile: &Value,
    ) -> Option<(PathBuf, String)> {
        if self
            .config()
            .get(&format!("games.{profile_id}.launchViaSteam"))
            == Value::Bool(false)
        {
            if let Some((app_id, signal)) = steam::detect_signal(install_root, profile) {
                log::info!(
                    "{profile_id}: Steam copy detected (app {app_id} via {signal}) but \
                     launchViaSteam is off, launching directly"
                );
            }
            return None;
        }
        let (app_id, signal) = steam::detect_signal(install_root, profile)?;
        log::info!("{profile_id}: Steam copy detected (app {app_id} via {signal})");
        match steam::steam_exe() {
            Some(exe) => Some((exe, app_id)),
            None => {
                log::warn!(
                    "{profile_id} is the Steam build (app {app_id}) but steam.exe could not be \
                     found — launching the game directly, without the Steam overlay."
                );
                None
            }
        }
    }

    fn begin_launch(
        self: &Arc<Self>,
        game_id: &str,
        proc_name: String,
        child: Option<Child>,
        confirm_timeout: Duration,
        via_steam: bool,
    ) {
        {
            let mut monitor = self.monitor.lock();
            if monitor
                .sessions
                .get(game_id)
                .is_some_and(GameSession::is_running)
            {
                log::info!("{game_id} is already being watched, keeping that session");
                return;
            }
            log::info!("Watching for {game_id} process ({proc_name})");
            monitor.sessions.insert(
                game_id.to_string(),
                GameSession {
                    launch_deadline: child.is_none().then(|| Instant::now() + confirm_timeout),
                    via_steam,
                    handed_off: child.is_none(),
                    ..GameSession::new(proc_name)
                },
            );
            self.ensure_ticker(&mut monitor);
        }
        if let Some(child) = child {
            self.watch_launch_handoff(game_id, child, confirm_timeout, via_steam);
        }
    }

    fn adopt_running(self: &Arc<Self>, game_id: &str, proc_name: String, pid: u32) -> bool {
        let mut monitor = self.monitor.lock();
        if monitor
            .sessions
            .get(game_id)
            .is_some_and(GameSession::is_running)
        {
            return false;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        monitor.sessions.insert(
            game_id.to_string(),
            GameSession {
                pid: Some(pid),
                started_at: Some(now_ms),
                last_seen_ms: now_ms,
                ..GameSession::new(proc_name)
            },
        );
        self.ensure_ticker(&mut monitor);
        true
    }

    pub fn update_left_unfinished(&self, game_id: &str) -> bool {
        self.config()
            .get(&super::file_channels::update_incomplete_key(game_id))
            .as_bool()
            == Some(true)
    }

    fn ensure_ticker(self: &Arc<Self>, monitor: &mut Monitor) {
        if monitor.task.is_some() {
            return;
        }
        log::info!("Starting game process monitoring");
        let me = Arc::clone(self);
        monitor.task = Some(tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(PROCESS_MONITOR_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if !me.check_game_processes().await {
                    log::info!("Stopped game process monitoring");
                    break;
                }
            }
        }));
    }

    fn watch_launch_handoff(
        self: &Arc<Self>,
        game_id: &str,
        mut child: Child,
        confirm_timeout: Duration,
        via_steam: bool,
    ) {
        let me = Arc::clone(self);
        let game_id = game_id.to_string();
        let spawned = Instant::now();
        tauri::async_runtime::spawn(async move {
            let handed_off = match tokio::time::timeout(LAUNCH_HANDOFF_MAX_WAIT, child.wait()).await
            {
                Ok(Ok(status)) if !status.success() => {
                    log::warn!("Shell hand-off for {game_id} failed ({status})");
                    false
                }
                _ => true,
            };
            let deadline =
                handoff_deadline(spawned, Instant::now(), handed_off, confirm_timeout, via_steam);
            if let Some(session) = me.monitor.lock().sessions.get_mut(&game_id) {
                if !session.is_running() && !session.passive {
                    session.launch_deadline = Some(deadline);
                    session.handed_off = handed_off;
                }
            }
        });
    }

    // ------------ Process Watching ------------
    // A once a second ticker that notices when a game starts, stops or restarts itself, and picks up games that were already running when Peebify opened.
    async fn check_game_processes(self: &Arc<Self>) -> bool {
        struct Probe {
            game_id: String,
            proc_name: String,
            by_name: bool,
            via_steam: bool,
            missing: bool,
        }

        let probes: Vec<Probe> = {
            let mut monitor = self.monitor.lock();
            if monitor.sessions.is_empty() {
                monitor.task = None;
                return false;
            }
            let now = Instant::now();
            monitor
                .sessions
                .iter_mut()
                .filter_map(|(game_id, session)| {
                    if !session.probe_due(now) {
                        return None;
                    }
                    let pid_alive = session.is_running()
                        && session.pid.is_some_and(process_utils::is_pid_alive);
                    let reconfirm_due =
                        session.last_pid_reconfirm.elapsed() >= PID_RECONFIRM_INTERVAL;
                    Some(Probe {
                        game_id: game_id.clone(),
                        proc_name: session.proc_name.clone(),
                        by_name: !pid_alive || reconfirm_due,
                        via_steam: session.via_steam,
                        missing: session.missing_since.is_some(),
                    })
                })
                .collect()
        };
        let held: Vec<String> = probes
            .iter()
            .filter(|probe| probe.missing)
            .map(|probe| probe.game_id.clone())
            .collect();
        self.playtime.tick_holding(&held);

        let names: Vec<String> = probes
            .iter()
            .filter(|probe| probe.by_name)
            .map(|probe| probe.proc_name.clone())
            .collect();
        let Some(found) = process_utils::pids_by_name(&names).await else {
            return true;
        };

        for probe in probes {
            let (seen, pid) = if probe.by_name {
                match found.get(&probe.proc_name.to_lowercase()) {
                    Some(&pid) => (true, Some(pid)),
                    None => (false, None),
                }
            } else {
                (true, None)
            };
            match self.apply_probe(&probe.game_id, seen, pid, probe.by_name) {
                Transition::Started => self.handle_game_started(&probe.game_id).await,
                Transition::Stopped {
                    duration_ms,
                    ended_ms,
                } => {
                    self.handle_game_stopped(&probe.game_id, duration_ms, ended_ms)
                        .await
                }
                Transition::LaunchFailed => {
                    self.handle_launch_failed(&probe.game_id, probe.via_steam)
                        .await
                }
                Transition::SteamStillStarting => {
                    self.handle_steam_still_starting(&probe.game_id).await
                }
                Transition::WatchExpired => {
                    log::info!("Stopped waiting for Steam to start {}", probe.game_id)
                }
                Transition::None => {}
            }
        }
        true
    }

    fn apply_probe(
        &self,
        game_id: &str,
        seen: bool,
        pid: Option<u32>,
        by_name: bool,
    ) -> Transition {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut monitor = self.monitor.lock();
        let Some(session) = monitor.sessions.get_mut(game_id) else {
            return Transition::None;
        };
        let before = session.pid;
        let transition = fold_probe(session, seen, pid, by_name, now_ms, Instant::now());
        let moved = session
            .pid
            .filter(|now| session.is_running() && before != Some(*now));
        if matches!(
            transition,
            Transition::Stopped { .. } | Transition::LaunchFailed | Transition::WatchExpired
        ) {
            monitor.sessions.remove(game_id);
        }
        drop(monitor);
        if let (Transition::None, Some(pid)) = (&transition, moved) {
            self.note_game_process(game_id, pid);
        }
        transition
    }

    fn note_game_process(&self, game_id: &str, pid: u32) {
        let created_ms = process_identity(pid).map(|id| id.created_ms);
        if created_ms.is_none() {
            log::debug!("Could not read when {game_id}'s process (pid {pid}) started");
        }
        self.playtime.set_process(game_id, pid, created_ms);
    }

    async fn handle_game_started(self: &Arc<Self>, game_id: &str) {
        let pid = self.running_pid(game_id);
        log::info!(
            "Game process started ({game_id}, pid {}, also running: {})",
            pid.map_or_else(|| "unknown".to_string(), |p| p.to_string()),
            describe_games(&self.other_running_games(game_id))
        );

        let proc_name = self
            .monitor
            .lock()
            .sessions
            .get(game_id)
            .map(|session| session.proc_name.clone())
            .unwrap_or_default();
        self.playtime.start_tracking(game_id, &proc_name);
        if let Some(pid) = pid {
            self.note_game_process(game_id, pid);
        }
        self.emit_game_event("game-started", json!({ "gameId": game_id, "pid": pid }))
            .await;
        let state = self.app.state::<BackendState>();
        super::http::shared_transfer().set_playing(true);
        state.overlay.on_game_started(game_id, pid);
        state.window.update_tray_menu();
        state.window.perform_launch_action();
    }

    async fn handle_game_stopped(
        self: &Arc<Self>,
        game_id: &str,
        session_duration: i64,
        ended_ms: i64,
    ) {
        let session_ms = self
            .playtime
            .stop_tracking_at(&self.app, game_id, ended_ms)
            .unwrap_or(session_duration)
            .max(0);
        log::info!(
            "Game process stopped ({game_id}). Session duration: {}s, active: {}s, still running: {}",
            session_duration / 1000,
            session_ms / 1000,
            describe_games(&self.other_running_games(game_id))
        );

        fps_unlock::end_session(&self.app, game_id);
        self.close_exit_companions(game_id).await;
        self.emit_game_event("game-stopped", json!({ "gameId": game_id }))
            .await;
        self.clear_update_cache(game_id);

        let state = self.app.state::<BackendState>();
        match state.overlay.owner() {
            Some(owner) if owner != game_id && self.is_game_running_id(&owner) => {
                log::info!("overlay: staying on {owner} after {game_id} stopped");
            }
            _ => {
                state.overlay.on_game_stopped();
                if let Some((next, _)) = self.latest_started_game() {
                    log::info!("overlay: moving from {game_id} to {next}, which is still running");
                    state.overlay.on_game_started(&next, self.running_pid(&next));
                }
            }
        }
        super::http::shared_transfer().set_playing(self.is_any_game_active());
        state.game_updater.on_game_stopped(game_id);
        state.window.update_tray_menu();
        if !self.is_any_game_active() {
            state.window.reopen_after_game();
        }

        if !self.is_any_game_active() {
            xxmi::kill_orphan_loader().await;
            super::xxmi::on_game_stopped(&self.app);
            self.app
                .state::<BackendState>()
                .window
                .exit_if_closed_for_game();
        }
    }

    pub fn resume_open_sessions(self: &Arc<Self>) {
        let leftovers = self.playtime.take_leftovers();
        if leftovers.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let names: Vec<String> = leftovers
                .iter()
                .filter(|session| !session.proc_name.is_empty())
                .map(|session| session.proc_name.clone())
                .collect();
            let found = process_utils::pids_by_name(&names).await.unwrap_or_default();
            let mut adopted = Vec::new();
            for session in leftovers {
                let by_name = found.get(&session.proc_name.to_lowercase()).copied();
                let pid = session.resumable_pid(by_name, process_identity);
                if pid.is_none() && by_name.is_some() {
                    log::info!(
                        "{} is running, but not as the process its open session tracked, so that session ends at its last checkpoint.",
                        session.game_id
                    );
                }
                let adoptable = pid.is_some() && {
                    let mut monitor = me.monitor.lock();
                    if monitor.sessions.contains_key(&session.game_id) {
                        false
                    } else {
                        monitor.sessions.insert(
                            session.game_id.clone(),
                            GameSession {
                                pid,
                                started_at: Some(session.start_ms),
                                last_seen_ms: chrono::Utc::now().timestamp_millis(),
                                ..GameSession::new(session.proc_name.clone())
                            },
                        );
                        me.ensure_ticker(&mut monitor);
                        true
                    }
                };
                if adoptable {
                    let game_id = session.game_id.clone();
                    me.playtime.adopt(session);
                    if let Some(pid) = pid {
                        me.note_game_process(&game_id, pid);
                    }
                    adopted.push(game_id);
                } else {
                    me.playtime.close_leftover(&me.app, &session);
                }
            }
            me.playtime.checkpoint();
            if !adopted.is_empty() {
                me.resume_started(&adopted).await;
            }
        });
    }

    async fn resume_started(self: &Arc<Self>, adopted: &[String]) {
        for game_id in adopted {
            if !self.is_game_running_id(game_id) {
                continue;
            }
            let pid = self.running_pid(game_id);
            log::info!(
                "Game process picked up again after a restart ({game_id}, pid {})",
                pid.map_or_else(|| "unknown".to_string(), |p| p.to_string())
            );
            self.emit_game_event("game-started", json!({ "gameId": game_id, "pid": pid }))
                .await;
        }
        let state = self.app.state::<BackendState>();
        if state.overlay.owner().is_none() {
            if let Some((id, _)) = self.latest_started_game() {
                state.overlay.on_game_started(&id, self.running_pid(&id));
            }
        }
        state.window.update_tray_menu();
    }

    fn other_running_games(&self, game_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .monitor
            .lock()
            .sessions
            .iter()
            .filter(|(id, session)| id.as_str() != game_id && session.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    fn latest_started_game(&self) -> Option<(String, i64)> {
        self.monitor
            .lock()
            .sessions
            .iter()
            .filter_map(|(id, s)| s.started_at.map(|t| (id.clone(), t)))
            .max_by_key(|(_, t)| *t)
    }

    async fn close_exit_companions(&self, game_id: &str) {
        let profile = game_profiles::profile(game_id);
        let client = game_profiles::client_process_name(profile).to_lowercase();
        let root = match self.config().get(&format!("games.{game_id}.gamePath")) {
            Value::String(s) if !s.is_empty() => PathBuf::from(s),
            _ => {
                log::debug!("{game_id} has no configured game path, so its exit companions stay");
                return;
            }
        };
        for name in game_profiles::exit_companions(profile) {
            if name.to_lowercase() == client {
                continue;
            }
            let closed = process_utils::terminate_by_name_under(&name, &root).await;
            if closed > 0 {
                log::info!("Closed {closed} leftover {name} process(es) after {game_id} exited.");
            }
        }
    }

    async fn handle_launch_failed(self: &Arc<Self>, game_id: &str, via_steam: bool) {
        log::warn!("Launch of {game_id} never produced a running process");
        let reason = if via_steam {
            STEAM_LAUNCH_NEVER_STARTED
        } else {
            LAUNCH_NEVER_STARTED
        };
        let name = game_profiles::display_name(game_profiles::profile(game_id));
        self.announce_stalled_launch(game_id, "game-launch-failed", reason, format!("{name} did not start"))
            .await;
    }

    async fn handle_steam_still_starting(self: &Arc<Self>, game_id: &str) {
        log::warn!(
            "Steam hasn't started {game_id} yet, watching for it for up to {} h",
            STEAM_UPDATE_WATCH_TIMEOUT.as_secs() / 3600
        );
        let name = game_profiles::display_name(game_profiles::profile(game_id));
        self.announce_stalled_launch(
            game_id,
            "game-launch-waiting",
            STEAM_LAUNCH_STILL_WAITING,
            format!("{name} hasn't started yet"),
        )
        .await;
    }

    async fn announce_stalled_launch(&self, game_id: &str, event: &str, reason: &str, title: String) {
        fps_unlock::end_session(&self.app, game_id);
        if !self.is_any_game_active() {
            xxmi::kill_orphan_loader().await;
        }
        self.emit_game_event(event, json!({ "gameId": game_id, "reason": reason }))
            .await;
        super::platform::notify_if_backgrounded(&self.app, &title, reason);
        self.app.state::<BackendState>().window.update_tray_menu();
    }

    // ------------ Update Checks ------------
    // Asks each game's server for its latest version and compares it with what is installed, caching answers briefly so the UI does not hammer the servers.
    pub async fn check_for_updates(
        &self,
        force_check: bool,
        target_game_id: Option<&str>,
        reason: &str,
    ) -> Value {
        let game_id = self.resolve_game_id(target_game_id);
        let profile = game_profiles::profile(&game_id);
        let profile_id = game_profiles::profile_id(profile).to_string();

        if !game_profiles::is_managed(profile) {
            return ok_with(json!({
                "gameId": profile_id,
                "updateAvailable": false,
                "currentVersion": null,
                "latestVersion": null,
            }));
        }

        if !force_check {
            if let Some((cached, at)) = self.cached_update_entry(&profile_id) {
                log::debug!(
                    "Using cached update check result for {profile_id} ({reason}, {}s old)",
                    at.elapsed().as_secs()
                );
                return ok_with(cached);
            }
        }

        if !http::is_online_cached() {
            log::info!("No internet connection, skipping game update check ({profile_id}, {reason}).");
            return err_response("No internet connection.");
        }

        let requested_at = Instant::now();
        let flight = self.update_lock(&profile_id);
        let _flight = flight.lock().await;
        if let Some((cached, at)) = self.cached_update_entry(&profile_id) {
            if reuse_cached_check(force_check, at, requested_at) {
                log::debug!(
                    "Reusing the update check for {profile_id} that finished while this one waited ({reason})"
                );
                return ok_with(cached);
            }
        }

        let started = Instant::now();
        let game_path = self.config().get(&format!("games.{profile_id}.gamePath"));
        let local_version = local_game_version_for(profile, game_path.as_str().unwrap_or(""));
        let local_label = local_version_label(
            local_version.as_deref(),
            game_path.as_str().is_some_and(|p| !p.is_empty()),
        );
        let feed = game_profiles::install_mode(profile).unwrap_or("game config");
        log::debug!("Checking for updates ({profile_id}, {reason}). Local version: {local_label}");

        let publisher = manifest::publisher(profile);
        let game_config = match publisher.latest(profile, force_check).await {
            Ok(config) => config,
            Err(e) => {
                log::warn!("Game update check failed ({game_id}, {reason}, via {feed}): {e}");
                return err_response(e);
            }
        };

        let raw_remote = game_config["default"]
            .get("version")
            .filter(|v| !v.is_null())
            .or_else(|| game_config.get("version"))
            .and_then(fs_util::json_version)
            .or_else(|| super::kuro::bundle_config_version(&game_config));
        let Some(remote_version) = raw_remote.as_deref() else {
            let msg = "No default game configuration found in remote.";
            log::error!("Game update check failed ({game_id}, {reason}, via {feed}): {msg}");
            return err_response(msg);
        };

        let update_available = local_version
            .as_deref()
            .map(|local| fs_util::is_version_newer(remote_version, local))
            .unwrap_or(false)
            || (local_version.is_some() && publisher.update_pending(&game_config, Path::new(game_path.as_str().unwrap_or(""))));

        let result = json!({
            "gameId": profile_id,
            "updateAvailable": update_available,
            "currentVersion": local_version,
            "latestVersion": remote_version,
        });
        self.update_cache
            .lock()
            .insert(profile_id.clone(), (result.clone(), Instant::now()));

        log::info!(
            "Update check ({profile_id}, {reason}): local {local_label}, remote {remote_version} via {feed}, update available: {update_available}, {} ms",
            started.elapsed().as_millis()
        );
        ok_with(result)
    }

    fn cached_update_entry(&self, game_id: &str) -> Option<(Value, Instant)> {
        let cache = self.update_cache.lock();
        let (result, at) = cache.get(game_id)?;
        (at.elapsed() < UPDATE_CACHE_TIMEOUT).then(|| (result.clone(), *at))
    }

    fn update_lock(&self, game_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        flight_lock(&self.update_locks, game_id)
    }

    pub fn clear_update_cache(&self, game_id: &str) {
        self.update_cache.lock().remove(game_id);
    }
}

fn fold_probe(
    session: &mut GameSession,
    seen: bool,
    pid: Option<u32>,
    by_name: bool,
    now_ms: i64,
    now: Instant,
) -> Transition {
    if by_name {
        session.last_pid_reconfirm = now;
        session.pid = pid;
    }
    match (session.started_at, seen) {
        (None, true) => {
            session.started_at = Some(now_ms);
            session.last_seen_ms = now_ms;
            Transition::Started
        }
        (Some(_), true) => {
            if session.missing_since.take().is_some() {
                log::info!("{} is back, keeping its session", session.proc_name);
            }
            session.last_seen_ms = now_ms;
            Transition::None
        }
        (Some(start), false) => {
            let since = *session.missing_since.get_or_insert_with(|| {
                log::info!(
                    "{} is gone, waiting {}s in case it restarts itself",
                    session.proc_name,
                    MISSING_GRACE.as_secs()
                );
                now
            });
            if now.saturating_duration_since(since) < MISSING_GRACE {
                return Transition::None;
            }
            let ended_ms = session.last_seen_ms.max(start);
            Transition::Stopped {
                duration_ms: ended_ms - start,
                ended_ms,
            }
        }
        (None, false)
            if session
                .launch_deadline
                .is_some_and(|deadline| now >= deadline) =>
        {
            if session.passive {
                Transition::WatchExpired
            } else if session.via_steam && session.handed_off {
                session.passive = true;
                session.launch_deadline = Some(now + STEAM_UPDATE_WATCH_TIMEOUT);
                Transition::SteamStillStarting
            } else {
                Transition::LaunchFailed
            }
        }
        _ => Transition::None,
    }
}

fn launch_error(display_name: &str, error: &process_utils::SpawnError, via_helper: bool) -> String {
    match (error.declined, via_helper) {
        (true, true) => format!(
            "You declined the Windows prompt for the FPS unlocker, so {display_name} didn't start."
        ),
        (true, false) => format!("You declined the Windows prompt, so {display_name} didn't start."),
        (false, _) => format!("Failed to launch: {}", error.message),
    }
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let image = process_utils::image_path(handle);
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let timed =
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        if !alive || !timed {
            return None;
        }
        let image = Path::new(&image?).file_name()?.to_string_lossy().to_string();
        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Some(ProcessIdentity {
            image,
            created_ms: (ticks / 10_000) as i64 - UNIX_EPOCH_MS,
        })
    }
}

fn handoff_deadline(
    spawned: Instant,
    now: Instant,
    handed_off: bool,
    confirm_timeout: Duration,
    via_steam: bool,
) -> Instant {
    let deadline = now
        + if handed_off {
            confirm_timeout
        } else {
            LAUNCH_FAILED_GRACE
        };
    if via_steam {
        deadline.min(spawned + LAUNCH_HANDOFF_MAX_WAIT)
    } else {
        deadline
    }
}

fn split_launch_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in input.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

// ------------ Version Helpers ------------
// Reads the installed version off disk, which differs by game (a config file, an ini, a manifest).
pub(super) fn local_game_version_at(install_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(install_path.join(GAME_CONFIG_FILE)).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    fs_util::json_version(&config["version"])
}

pub(super) fn local_game_version_for(profile: &Value, game_path: &str) -> Option<String> {
    (!game_path.is_empty()).then(|| manifest::publisher(profile).installed_version(Path::new(game_path))).flatten()
}

fn busy_launch_message(display_name: &str, op: &str) -> String {
    let doing = match op {
        "repair" => "being repaired",
        "move" => "being moved",
        _ => "being updated",
    };
    format!("{display_name} is {doing}. Launch it when that finishes.")
}

fn describe_games(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

fn flight_lock(
    locks: &Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    game_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    Arc::clone(locks.lock().entry(game_id.to_string()).or_default())
}

fn reuse_cached_check(force_check: bool, cached_at: Instant, requested_at: Instant) -> bool {
    !force_check || cached_at >= requested_at
}

fn local_version_label(local_version: Option<&str>, has_game_path: bool) -> String {
    match local_version {
        Some(version) => version.to_string(),
        None if has_game_path => "unknown (no version file)".to_string(),
        None => "not installed".to_string(),
    }
}

// ------------ Command Handlers ------------
// The thin functions the UI's launch, running game, Steam, update check and game link commands call into.
fn game(app: &AppHandle) -> Arc<GameManager> {
    app.state::<BackendState>().game.clone()
}

pub(super) async fn launch_game(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let target = args.first().and_then(|v| v.as_str()).map(str::to_string);
    Ok(game(app).launch_game(target.as_deref()).await)
}

pub(super) async fn get_running_game(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let manager = game(app);
    let Some(id) = super::arg_str(args, 0) else {
        return Ok(running_game_reply(&manager.running_games()));
    };
    Ok(super::ok_with(json!({ "running": manager.is_game_running_id(id) })))
}

fn running_game_reply(ids: &[String]) -> Value {
    super::ok_with(json!({
        "running": !ids.is_empty(),
        "runningIds": ids,
    }))
}

pub(super) async fn get_steam_install(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let target = args.first().and_then(|v| v.as_str()).map(str::to_string);
    Ok(game(app).steam_install_info(target.as_deref()).await)
}

pub(super) async fn check_for_updates(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let target = args.first().and_then(|v| v.as_str()).map(str::to_string);
    let force_check = args.get(1).and_then(Value::as_bool).unwrap_or(false);
    let reason =
        super::arg_str(args, 2).unwrap_or(if force_check { "manual" } else { "ui" });
    Ok(game(app)
        .check_for_updates(force_check, target.as_deref(), reason)
        .await)
}

pub(super) async fn get_game_links() -> Result<Value, String> {
    let mut links = serde_json::Map::new();
    for id in game_profiles::GAME_IDS {
        let profile = game_profiles::profile(id);
        let mut socials = Vec::new();
        if let Some(map) = profile.get("socialUrls").and_then(|v| v.as_object()) {
            for (platform, url) in map {
                if let Some(url) = url.as_str().filter(|u| !u.is_empty()) {
                    socials.push(json!({ "platform": platform, "url": url }));
                }
            }
        }

        let mut tools = Vec::new();
        if let Some(ct) = profile.get("communityTools").and_then(|v| v.as_object()) {
            for group in ["official", "community"] {
                if let Some(arr) = ct.get(group).and_then(|v| v.as_array()) {
                    for t in arr {
                        if let (Some(name), Some(url)) = (
                            t.get("name").and_then(|v| v.as_str()),
                            t.get("url").and_then(|v| v.as_str()),
                        ) {
                            tools.push(json!({ "name": name, "url": url }));
                        }
                    }
                }
            }
        }

        links.insert(
            id.to_string(),
            json!({ "socials": socials, "communityTools": tools }),
        );
    }
    Ok(ok_with(json!({ "links": links })))
}

pub(super) async fn open_game_folder(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = match super::game_file_ops::known_profile_or_error(app, super::arg_str(args, 0)) {
        Ok(profile) => profile,
        Err(refusal) => return Ok(refusal),
    };
    let game_path = app
        .state::<BackendState>()
        .config
        .get(&format!("games.{}.gamePath", game_profiles::profile_id(profile)));
    let game_path = game_path.as_str().unwrap_or("");
    if game_path.is_empty() {
        return Ok(err_response("Game path not configured."));
    }
    Ok(super::wrap_result(
        "open-game-folder",
        super::fs_util::dialog::open_folder(Path::new(game_path)),
    ))
}

// ------------ Game Auto Updater ------------
// Checks once an hour whether a game is due for its daily or weekly update check, and queues the update if auto update is on. If it is off, it only sends a notification.
// It waits for the game to be closed first, and does a first sweep shortly after Peebify starts.

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
const POST_CLOSE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
const PROCESS_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

const BUSY_RUNNING: &str = "running";
const BUSY_LAUNCHING: &str = "launching";
const BUSY_EXTERNAL: &str = "external";
const BUSY_OTHER_SESSION: &str = "session";

pub(super) async fn busy_reason(app: &AppHandle, game_id: &str) -> Option<&'static str> {
    let game = app.state::<BackendState>().game.clone();
    if game.is_game_running_id(game_id) {
        return Some(BUSY_RUNNING);
    }
    if game.is_game_active_id(game_id) {
        return Some(BUSY_LAUNCHING);
    }
    let proc_name = game_profiles::client_process_name(game_profiles::profile(game_id));
    if !proc_name.is_empty() && process_utils::is_process_running(proc_name).await {
        return Some(BUSY_EXTERNAL);
    }
    if game.is_any_game_active() {
        return Some(BUSY_OTHER_SESSION);
    }
    None
}

fn replay_reason(reason: &str) -> &'static str {
    if reason.starts_with("startup") {
        "startup-deferred"
    } else {
        "deferred"
    }
}

fn force_update_check(reason: &str) -> bool {
    !matches!(reason, "startup" | "startup-online")
}

pub struct GameUpdater {
    app: AppHandle,
    sweeping: AtomicBool,
    startup_skipped: AtomicBool,
    deferred: Mutex<HashMap<String, &'static str>>,
    watching: Mutex<HashSet<String>>,
}

impl GameUpdater {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            sweeping: AtomicBool::new(false),
            startup_skipped: AtomicBool::new(false),
            deferred: Mutex::new(HashMap::new()),
            watching: Mutex::new(HashSet::new()),
        })
    }

    fn config(&self) -> Arc<super::config::LauncherConfig> {
        self.app.state::<BackendState>().config.clone()
    }

    fn game_key(&self, game_id: &str, key: &str) -> Value {
        self.config().get(&format!("games.{game_id}.{key}"))
    }

    fn auto_update_enabled(&self, game_id: &str) -> bool {
        self.game_key(game_id, "autoUpdate") != Value::Bool(false)
    }

    fn any_enabled(&self) -> bool {
        game_profiles::GAME_IDS
            .iter()
            .any(|id| self.auto_update_enabled(id))
    }

    async fn already_queued(&self, game_id: &str) -> bool {
        let queue = self.app.state::<BackendState>().engine.queue.clone();
        queue
            .current_meta()
            .map(|m| m.game_id == game_id)
            .unwrap_or(false)
            || queue.has_pending(game_id, None)
            || queue.has_parked(game_id)
    }

    fn defer(self: &Arc<Self>, game_id: &str, busy: &str, reason: &str) {
        let name = game_profiles::display_name(game_profiles::profile(game_id));
        match busy {
            BUSY_RUNNING => log::info!(
                "Auto-update: {name} is running, the update will retry when it closes ({reason})."
            ),
            BUSY_EXTERNAL => {
                log::info!(
                    "Auto-update: {name} is running, the update will retry when it closes ({reason})."
                );
                self.watch_until_closed(game_id, reason);
            }
            BUSY_LAUNCHING => {
                log::info!("Auto-update: {name} launch in progress, deferring update ({reason}).");
                self.deferred
                    .lock()
                    .insert(game_id.to_string(), replay_reason(reason));
            }
            _ => {
                log::info!("Auto-update: a game session is running, deferring {name} ({reason}).");
                self.deferred
                    .lock()
                    .insert(game_id.to_string(), replay_reason(reason));
            }
        }
    }

    fn watch_until_closed(self: &Arc<Self>, game_id: &str, reason: &str) {
        if !self.watching.lock().insert(game_id.to_string()) {
            return;
        }
        let me = Arc::clone(self);
        let game_id = game_id.to_string();
        let replay = replay_reason(reason);
        tauri::async_runtime::spawn(async move {
            let proc_name = game_profiles::client_process_name(game_profiles::profile(&game_id));
            loop {
                tokio::time::sleep(PROCESS_WATCH_INTERVAL).await;
                if !process_utils::is_process_running(proc_name).await {
                    break;
                }
            }
            me.watching.lock().remove(&game_id);
            me.maybe_update_game(&game_id, replay).await;
        });
    }

    async fn maybe_update_game(self: &Arc<Self>, game_id: &str, reason: &str) {
        if !self.auto_update_enabled(game_id) {
            return;
        }
        if reason.starts_with("startup")
            && self.game_key(game_id, "autoUpdateOnStartup") == Value::Bool(false)
        {
            return;
        }
        let profile = game_profiles::profile(game_id);
        if !game_profiles::is_managed(profile) {
            return;
        }
        let game_path = self.game_key(game_id, "gamePath");
        if game_path.as_str().map(str::is_empty).unwrap_or(true) {
            return;
        }
        if self.already_queued(game_id).await {
            log::info!("Auto-update: {game_id} already downloading/queued — skipping ({reason}).");
            return;
        }
        if !http::is_online_cached() {
            return;
        }
        if let Some(busy) = busy_reason(&self.app, game_id).await {
            self.defer(game_id, busy, reason);
            return;
        }
        self.deferred.lock().remove(game_id);

        let state = self.app.state::<BackendState>();
        let result = state
            .game
            .check_for_updates(force_update_check(reason), Some(game_id), reason)
            .await;
        if result["success"] != Value::Bool(true) {
            return;
        }
        let stamp_key = format!("games.{game_id}.lastAutoUpdateCheck");
        let previous_stamp = self.config().get(&stamp_key);
        super::config::set_config_value(
            &self.app,
            &stamp_key,
            json!(chrono::Utc::now().timestamp_millis()),
        );
        if result["updateAvailable"] != Value::Bool(true) {
            return;
        }
        let _ = self.app.emit("update-available", result.clone());
        log::info!(
            "Auto-update: {} {} -> {}, queuing download ({reason}).",
            game_profiles::display_name(profile),
            result["currentVersion"].as_str().unwrap_or("?"),
            result["latestVersion"].as_str().unwrap_or("?")
        );
        let me = Arc::clone(self);
        let game_id = game_id.to_string();
        let reason = reason.to_string();
        tauri::async_runtime::spawn(async move {
            let args = [json!({ "gameId": game_id, "versionType": "default", "deferIfBusy": true })];
            match super::file_channels::start_download(&me.app, &args).await {
                Ok(outcome) => {
                    if let Some(busy) = outcome.get("deferredBusy").and_then(Value::as_str) {
                        super::config::set_config_value(
                            &me.app,
                            &stamp_key,
                            previous_stamp,
                        );
                        log::info!(
                            "Auto-update: {} became busy before its queued update ran.",
                            game_profiles::display_name(game_profiles::profile(&game_id))
                        );
                        me.defer(&game_id, busy, &reason);
                    } else if outcome.get("success") == Some(&Value::Bool(false)) {
                        log::warn!(
                            "Auto-update start-download failed for {game_id}: {}",
                            outcome.get("error").and_then(Value::as_str).unwrap_or("(no detail)")
                        );
                    }
                }
                Err(e) => log::warn!("Auto-update start-download failed for {game_id}: {e}"),
            }
        });
    }

    async fn notify_update(&self, game_id: &str) {
        let profile = game_profiles::profile(game_id);
        if !game_profiles::is_managed(profile) {
            return;
        }
        let game_path = self.game_key(game_id, "gamePath");
        if game_path.as_str().map(str::is_empty).unwrap_or(true) {
            return;
        }
        if !http::is_online_cached() {
            return;
        }
        let state = self.app.state::<BackendState>();
        let result = state.game.check_for_updates(true, Some(game_id), "notify").await;
        if result["success"] != Value::Bool(true) {
            return;
        }
        super::config::set_config_value(
            &self.app,
            &format!("games.{game_id}.lastUpdateNotifyCheck"),
            json!(chrono::Utc::now().timestamp_millis()),
        );
        if result["updateAvailable"] == Value::Bool(true) {
            log::info!(
                "Update available for {} with auto-update off, notifying only.",
                game_profiles::display_name(profile)
            );
            let _ = self.app.emit("update-available", result);
        }
    }

    async fn run_sweep(self: &Arc<Self>, reason: &str) {
        if self.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.any_enabled() {
            self.sweeping.store(false, Ordering::SeqCst);
            return;
        }
        if !http::is_online_cached() {
            log::info!("Auto-update sweep skipped ({reason}) — offline.");
            if reason == "startup" {
                self.startup_skipped.store(true, Ordering::SeqCst);
            }
            self.sweeping.store(false, Ordering::SeqCst);
            return;
        }

        log::info!("Running game auto-update sweep ({reason}).");
        for game_id in game_profiles::GAME_IDS {
            if !self.auto_update_enabled(game_id) {
                continue;
            }
            self.maybe_update_game(game_id, reason).await;
        }
        self.sweeping.store(false, Ordering::SeqCst);
    }

    pub fn resume_skipped_startup_sweep(self: &Arc<Self>) {
        if !self.startup_skipped.swap(false, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            me.run_sweep("startup-online").await;
        });
    }

    async fn replay_deferred(self: &Arc<Self>) {
        if self.app.state::<BackendState>().game.is_any_game_active() {
            return;
        }
        let pending: Vec<(String, &'static str)> = self.deferred.lock().drain().collect();
        for (game_id, reason) in pending {
            self.maybe_update_game(&game_id, reason).await;
        }
    }

    pub fn start(self: &Arc<Self>) {
        if self.any_enabled() {
            let me = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(STARTUP_DELAY).await;
                me.run_sweep("startup").await;
            });
        }

        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let now = chrono::Utc::now().timestamp_millis();
                for game_id in game_profiles::GAME_IDS {
                    if !me.auto_update_enabled(game_id) {
                        let last_ts = me
                            .game_key(game_id, "lastUpdateNotifyCheck")
                            .as_i64()
                            .unwrap_or(0);
                        if daily_weekly_due("daily", last_ts, now) {
                            me.notify_update(game_id).await;
                        }
                        continue;
                    }
                    let schedule = me.game_key(game_id, "autoUpdateSchedule");
                    let schedule = match schedule.as_str() {
                        Some("off") => "off",
                        Some("weekly") => "weekly",
                        _ => "daily",
                    };
                    if schedule == "off" {
                        continue;
                    }
                    let last_ts = me
                        .game_key(game_id, "lastAutoUpdateCheck")
                        .as_i64()
                        .unwrap_or(0);
                    if daily_weekly_due(schedule, last_ts, now) {
                        me.maybe_update_game(game_id, &format!("schedule:{schedule}"))
                            .await;
                    }
                }
            }
        });
    }

    pub fn on_game_stopped(self: &Arc<Self>, game_id: &str) {
        let me = Arc::clone(self);
        let game_id = game_id.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(POST_CLOSE_DELAY).await;
            if !game_id.is_empty() {
                me.maybe_update_game(&game_id, "game-stopped").await;
            }
            me.replay_deferred().await;
        });
    }
}

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(crate) fn daily_weekly_due(kind: &str, last_ts: i64, now: i64) -> bool {
    match kind {
        "daily" => last_ts == 0 || local_day_key(last_ts) != local_day_key(now),
        "weekly" => last_ts == 0 || last_ts > now || (now - last_ts) >= WEEK_MS,
        _ => false,
    }
}

// ------------ Game Manager Tests ------------
// Covers session tracking, launch hand offs, version checks and the auto update schedule.

#[cfg(test)]
mod tests {
    use super::*;

    fn session(passive: bool, deadline: Option<Instant>) -> GameSession {
        GameSession {
            launch_deadline: deadline,
            passive,
            ..GameSession::new("Client-Win64-Shipping.exe".to_string())
        }
    }

    #[test]
    fn only_an_idle_passive_watch_skips_ticks() {
        let now = Instant::now();
        let mut launch = session(false, Some(now + Duration::from_secs(60)));
        assert!(launch.probe_due(now));
        assert!(launch.probe_due(now));

        let mut watch = session(true, Some(now + STEAM_UPDATE_WATCH_TIMEOUT));
        watch.next_probe = now;
        assert!(watch.probe_due(now));
        assert!(!watch.probe_due(now + Duration::from_secs(1)));
        assert!(watch.probe_due(now + PASSIVE_WATCH_PROBE_INTERVAL));

        watch.started_at = Some(1_000);
        assert!(watch.probe_due(now + PASSIVE_WATCH_PROBE_INTERVAL + Duration::from_secs(1)));
    }

    #[test]
    fn a_game_that_restarts_itself_keeps_its_session() {
        let now = Instant::now();
        let mut game = session(false, None);
        assert!(matches!(
            fold_probe(&mut game, true, Some(7), true, 1_000, now),
            Transition::Started
        ));
        assert!(matches!(
            fold_probe(&mut game, true, None, false, 60_000, now),
            Transition::None
        ));
        let gone = now + Duration::from_secs(1);
        assert!(matches!(
            fold_probe(&mut game, false, None, true, 61_000, gone),
            Transition::None
        ));
        assert!(matches!(
            fold_probe(&mut game, false, None, true, 66_000, gone + Duration::from_secs(5)),
            Transition::None
        ));
        assert!(matches!(
            fold_probe(&mut game, true, Some(8), true, 67_000, gone + Duration::from_secs(6)),
            Transition::None
        ));
        assert!(game.missing_since.is_none());
        assert_eq!(game.pid, Some(8));
        assert_eq!(game.last_seen_ms, 67_000);
    }

    #[test]
    fn a_closed_game_ends_when_it_was_last_seen() {
        let now = Instant::now();
        let mut game = session(false, None);
        fold_probe(&mut game, true, Some(7), true, 1_000, now);
        fold_probe(&mut game, true, None, false, 60_000, now);
        let gone = now + Duration::from_secs(1);
        fold_probe(&mut game, false, None, true, 61_000, gone);
        assert!(matches!(
            fold_probe(&mut game, false, None, true, 71_000, gone + MISSING_GRACE),
            Transition::Stopped {
                duration_ms: 59_000,
                ended_ms: 60_000,
            }
        ));
    }

    #[test]
    fn a_slow_steam_start_becomes_a_background_watch() {
        let now = Instant::now();
        let mut launch = session(false, Some(now));
        launch.via_steam = true;
        assert!(matches!(
            fold_probe(&mut launch, false, None, true, 0, now),
            Transition::SteamStillStarting
        ));
        assert!(launch.passive);
        assert!(!launch.counts_as_active());
        assert_eq!(launch.launch_deadline, Some(now + STEAM_UPDATE_WATCH_TIMEOUT));
        assert!(matches!(
            fold_probe(&mut launch, true, Some(9), true, 1_000, now),
            Transition::Started
        ));
    }

    #[test]
    fn a_failed_steam_hand_off_still_fails() {
        let now = Instant::now();
        let mut launch = session(false, Some(now));
        launch.via_steam = true;
        launch.handed_off = false;
        assert!(matches!(
            fold_probe(&mut launch, false, None, true, 0, now),
            Transition::LaunchFailed
        ));
    }

    #[test]
    fn a_cold_steam_start_becomes_a_background_watch_at_the_hand_off_cap() {
        let spawned = Instant::now();
        let now = spawned + LAUNCH_HANDOFF_MAX_WAIT;
        let deadline = handoff_deadline(spawned, now, true, STEAM_LAUNCH_CONFIRM_TIMEOUT, true);
        assert_eq!(deadline, spawned + LAUNCH_HANDOFF_MAX_WAIT);
        let mut launch = session(false, Some(deadline));
        launch.via_steam = true;
        assert!(matches!(
            fold_probe(&mut launch, false, None, true, 0, now),
            Transition::SteamStillStarting
        ));
        assert!(launch.passive);
        let slow = spawned + Duration::from_secs(7 * 60);
        assert_eq!(
            handoff_deadline(spawned, slow, true, STEAM_LAUNCH_CONFIRM_TIMEOUT, true),
            spawned + LAUNCH_HANDOFF_MAX_WAIT
        );
    }

    #[test]
    fn passive_watch_expires_quietly_instead_of_failing() {
        let now = Instant::now();
        let mut watch = session(true, Some(now));
        assert!(matches!(
            fold_probe(&mut watch, false, None, true, 0, now),
            Transition::WatchExpired
        ));
        let mut launch = session(false, Some(now));
        assert!(matches!(
            fold_probe(&mut launch, false, None, true, 0, now),
            Transition::LaunchFailed
        ));
    }

    #[test]
    fn passive_watch_adopts_the_game_when_it_appears() {
        let now = Instant::now();
        let mut watch = session(true, Some(now + STEAM_UPDATE_WATCH_TIMEOUT));
        assert!(!watch.counts_as_active());
        assert!(matches!(
            fold_probe(&mut watch, false, None, true, 0, now),
            Transition::None
        ));
        assert!(matches!(
            fold_probe(&mut watch, true, Some(42), true, 1_000, now),
            Transition::Started
        ));
        assert!(watch.is_running());
        assert!(watch.counts_as_active());
        assert_eq!(watch.pid, Some(42));
    }

    #[test]
    fn launched_sessions_count_as_active_before_the_game_appears() {
        assert!(session(false, None).counts_as_active());
    }

    #[test]
    fn forced_checks_reuse_only_results_that_finished_while_waiting() {
        let requested = Instant::now();
        let earlier = requested - Duration::from_secs(30);
        let later = requested + Duration::from_millis(5);
        assert!(reuse_cached_check(false, earlier, requested));
        assert!(!reuse_cached_check(true, earlier, requested));
        assert!(reuse_cached_check(true, later, requested));
        assert!(reuse_cached_check(true, requested, requested));
    }

    #[tokio::test]
    async fn concurrent_update_checks_share_one_lock_per_game() {
        let locks = Mutex::new(HashMap::new());
        let get = |id: &str| flight_lock(&locks, id);
        let first = get("zzz");
        let held = first.lock().await;
        assert!(get("zzz").try_lock().is_err());
        assert!(get("hsr").try_lock().is_ok());
        drop(held);
        assert!(get("zzz").try_lock().is_ok());
    }

    const NOW: i64 = 1_790_000_000_000;

    #[test]
    fn weekly_due_after_a_week() {
        assert!(daily_weekly_due("weekly", 0, NOW));
        assert!(!daily_weekly_due("weekly", NOW - WEEK_MS + 1, NOW));
        assert!(daily_weekly_due("weekly", NOW - WEEK_MS, NOW));
    }

    #[test]
    fn future_stamp_counts_as_due() {
        assert!(daily_weekly_due("weekly", NOW + 365 * 24 * 60 * 60 * 1000, NOW));
        assert!(daily_weekly_due("daily", NOW + 365 * 24 * 60 * 60 * 1000, NOW));
    }

    #[test]
    fn unknown_schedule_is_never_due() {
        assert!(!daily_weekly_due("off", 0, NOW));
    }
}
