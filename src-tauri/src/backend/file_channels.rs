// ------------ Game Install Commands ------------
// The commands behind a game's Install, Repair, Verify, Move and Uninstall buttons, plus the disk space and default install folder lookups.
// Each one resolves the game profile, refuses if something else is already using the game, and hands the real work to the operation queue.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use super::fs_util::dialog::{pick_folder, Pick};
use super::engine::{self, verify_update, GameDownloadManager, GameRepairManager, JobCore, VerifyError};
use super::manifest;
use super::queue::{OperationQueue, Phase, Update};
use super::BackendState;
use super::{
    arg_str, err_response, game_file_ops, game_profiles, ok_response, ok_with,
    resolve_profile, steam,
};

// ------------ Engine Pools ------------
// Keeps one download manager and one repair manager per game, along with the shared operation queue, so every command here talks to the same running jobs.
pub struct EnginePools {
    pub queue: Arc<OperationQueue>,
    downloads: Mutex<HashMap<String, Arc<GameDownloadManager>>>,
    repairs: Mutex<HashMap<String, Arc<GameRepairManager>>>,
    verify_cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub verify_cache: Mutex<Option<engine::VerifyCache>>,
}

impl EnginePools {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let pools = Arc::new(Self {
            queue: OperationQueue::new(app),
            downloads: Mutex::new(HashMap::new()),
            repairs: Mutex::new(HashMap::new()),
            verify_cancels: Mutex::new(HashMap::new()),
            verify_cache: Mutex::new(None),
        });
        let weak: Weak<EnginePools> = Arc::downgrade(&pools);
        pools.queue.set_resume_hook(move |meta| {
            let Some(pools) = weak.upgrade() else {
                return;
            };
            if meta.op_type == "repair" {
                if let Some(mgr) = pools.repairs.lock().get(&meta.game_id) {
                    mgr.resume_repair();
                }
            } else if let Some(mgr) = pools.downloads.lock().get(&meta.game_id) {
                mgr.resume_download();
            }
        });
        pools
    }

    fn download_manager(
        &self,
        app: &AppHandle,
        profile: &'static Value,
    ) -> Arc<GameDownloadManager> {
        let id = game_profiles::profile_id(profile).to_string();
        let mut pool = self.downloads.lock();
        let mgr = pool
            .entry(id)
            .or_insert_with(|| GameDownloadManager::new(app.clone(), profile));
        mgr.set_profile(profile);
        Arc::clone(mgr)
    }

    fn existing_download_manager(&self, game_id: &str) -> Option<Arc<GameDownloadManager>> {
        self.downloads.lock().get(game_id).cloned()
    }

    fn existing_repair_manager(&self, game_id: &str) -> Option<Arc<GameRepairManager>> {
        self.repairs.lock().get(game_id).cloned()
    }

    fn repair_manager(&self, app: &AppHandle, profile: &'static Value) -> Arc<GameRepairManager> {
        let id = game_profiles::profile_id(profile).to_string();
        let mut pool = self.repairs.lock();
        let mgr = pool
            .entry(id)
            .or_insert_with(|| GameRepairManager::new(app.clone(), profile));
        mgr.set_profile(profile);
        Arc::clone(mgr)
    }
}

fn pools(app: &AppHandle) -> Arc<EnginePools> {
    app.state::<BackendState>().engine.clone()
}

fn configured_game_path(app: &AppHandle, profile_id: &str) -> String {
    app.state::<BackendState>()
        .config
        .get(&format!("games.{profile_id}.gamePath"))
        .as_str()
        .unwrap_or("")
        .to_string()
}

pub(super) async fn steam_copy_at(path: &str, profile: &'static Value) -> Option<String> {
    steam::app_id(profile)?;
    let path = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || steam::detect(&path, profile))
        .await
        .ok()
        .flatten()
}

// ------------ Pending Install Tracking ------------
// Remembers in the config where a fresh install is going, so an install that got interrupted can be picked up or cleaned up on the next start.
fn pending_install_key(profile_id: &str) -> String {
    format!("games.{profile_id}.pendingInstall")
}

fn record_pending_install(app: &AppHandle, profile_id: &str, path: &str, version_type: &str) {
    super::config::set_config_value(
        app,
        &pending_install_key(profile_id),
        json!({
            "path": path,
            "versionType": version_type,
            "startedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );
    app.state::<BackendState>().config.flush();
}

pub(super) fn clear_pending_install(app: &AppHandle, profile_id: &str) {
    forget_pending_install(app, profile_id);
    clear_update_incomplete(app, profile_id);
}

fn forget_pending_install(app: &AppHandle, profile_id: &str) {
    super::config::set_config_value(app, &pending_install_key(profile_id), Value::Null);
    app.state::<BackendState>().config.flush();
}

pub(super) fn update_incomplete_key(profile_id: &str) -> String {
    format!("games.{profile_id}.updateIncomplete")
}

pub(super) fn mark_update_incomplete(app: &AppHandle, profile_id: &str) {
    super::config::set_config_value(app, &update_incomplete_key(profile_id), json!(true));
    app.state::<BackendState>().config.flush();
}

pub(super) fn clear_update_incomplete(app: &AppHandle, profile_id: &str) {
    let state = app.state::<BackendState>();
    let key = update_incomplete_key(profile_id);
    if state.config.get(&key).is_null() {
        return;
    }
    super::config::set_config_value(app, &key, Value::Null);
    state.config.flush();
    log::info!("{profile_id}: the installed files are complete again, so it can be played.");
    state.window.update_tray_menu();
}

pub(super) fn follow_moved_pending_install(
    app: &AppHandle,
    profile_id: &str,
    old_path: &str,
    new_path: &str,
) {
    let key = format!("{}.path", pending_install_key(profile_id));
    let recorded = app.state::<BackendState>().config.get(&key);
    if recorded.as_str().is_some_and(|p| super::fs_util::same_path(p, old_path)) {
        super::config::set_config_value(app, &key, json!(new_path));
        app.state::<BackendState>().config.flush();
    }
}

pub(super) async fn discard_partial_install(install_path: &Path, profile_id: &str) -> (u64, u64) {
    let (path, id) = (install_path.to_path_buf(), profile_id.to_string());
    tauri::async_runtime::spawn_blocking(move || super::validator::discard_partial_install_for(&path, &id))
        .await
        .unwrap_or((0, 0))
}

async fn discard_cancelled_install(profile_id: &str, install_path: &Path) {
    let (files, bytes) = discard_partial_install(install_path, profile_id).await;
    if files > 0 {
        log::info!(
            "Cancelled {profile_id} install: discarded {files} file(s), {:.2} GB.",
            bytes as f64 / 1e9
        );
    }
}

fn renderer_install_path_refusal(profile: &Value, path: &str) -> Option<String> {
    use std::path::Component;
    let target = Path::new(path);
    let plain = target.is_absolute()
        && !target
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir));
    if !plain {
        return Some("The install folder must be a full path.".to_string());
    }
    let folder = game_profiles::install_folder_name(profile);
    let leaf = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if folder.is_empty() || !leaf.eq_ignore_ascii_case(&folder) {
        return Some(format!(
            "Pick the install location again. Peebify installs into a \"{folder}\" folder."
        ));
    }
    None
}

fn with_game_folder(picked: &str, folder: &str) -> String {
    let trimmed = picked.trim_end_matches(['\\', '/']);
    if folder.is_empty() {
        return picked.to_string();
    }
    let leaf = trimmed.rsplit(['\\', '/']).next().unwrap_or("");
    if leaf.eq_ignore_ascii_case(folder) {
        return trimmed.to_string();
    }
    format!("{trimmed}\\{folder}")
}

fn nested_install_dir(profile: &Value, base: &Path) -> Option<PathBuf> {
    if !manifest::publisher(profile).policy(base).nested {
        return None;
    }
    let exe_name = game_profiles::executable_name(profile);
    if base.join(exe_name).exists() {
        return Some(base.to_path_buf());
    }
    std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .find(|nested| nested.join(exe_name).exists())
}

struct PreparedStart<F: FnMut()>(F);

impl<F: FnMut()> Drop for PreparedStart<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn cancelled_before_start(app: &AppHandle, game_id: &str) -> Value {
    log::info!("Download for {game_id} was cancelled before it started.");
    super::queue::publish(
        app,
        "download-progress",
        Phase::Cancelled,
        json!({
            "status": super::engine::status::CANCELLED,
            "gameId": game_id,
            "percentage": 0,
        }),
    );
    json!({
        "success": false,
        "cancelled": true,
        "gameId": game_id,
        "error": "Download aborted by user.",
    })
}

fn refuse_beside_download(app: &AppHandle, game_id: &str, op_type: &str) -> Option<Value> {
    if !matches!(op_type, "move" | "repair" | "verify") {
        return None;
    }
    let pools = pools(app);
    let name = game_profiles::display_name(game_profiles::profile(game_id));
    if pools.queue.download_in_flight(game_id) {
        log::info!("Refusing {op_type} for {game_id} because its download is still in progress.");
        return Some(err_response(format!(
            "{name} has a download in progress. Let it finish or cancel it on the Downloads page, then try again."
        )));
    }
    if pools.queue.has_job_for(game_id) {
        log::info!(
            "Refusing {op_type} for {game_id} because another operation for it is running or queued."
        );
        return Some(err_response(format!(
            "{name} already has an operation in progress. Let it finish or cancel it on the Downloads page, then try again."
        )));
    }
    None
}

type QueuedRun = std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'static>>;

async fn run_queued(
    app: &AppHandle,
    game_id: &str,
    op_type: &str,
    kind: &str,
    status_event: &str,
    run: QueuedRun,
) -> Value {
    match enqueue_or_refuse(app, game_id, op_type, kind, status_event, run) {
        Ok(rx) => rx
            .await
            .unwrap_or_else(|_| err_response("Queued operation was dropped.")),
        Err(reply) => reply,
    }
}

fn enqueue_or_refuse(
    app: &AppHandle,
    game_id: &str,
    op_type: &str,
    kind: &str,
    status_event: &str,
    run: QueuedRun,
) -> Result<tokio::sync::oneshot::Receiver<Value>, Value> {
    if game_file_ops::is_uninstalling(game_id) {
        return Err(err_response(format!(
            "{} is being uninstalled. Try again when it finishes.",
            game_profiles::display_name(game_profiles::profile(game_id))
        )));
    }
    if let Some(refusal) = refuse_beside_download(app, game_id, op_type) {
        return Err(refusal);
    }
    let pools = pools(app);
    if op_type == "download" && pools.queue.has_download_for(game_id) {
        log::info!("A download for {game_id} is already running or queued, so the new request joins it.");
        return Err(super::queue::already_queued(game_id));
    }
    if pools.queue.is_busy() || pools.queue.has_pending(game_id, None) {
        super::queue::publish(
            app,
            status_event,
            Update::new(Phase::Queued).kind(kind),
            json!({
                "status": "Queued",
                "percentage": 0,
                "gameId": game_id,
                "kind": kind,
            }),
        );
    }
    Ok(pools.queue.enqueue(game_id, op_type, kind, run))
}

// ------------ Download Commands ------------
// Start, pause, resume, cancel and prioritize a game download or update.
pub(super) async fn start_download(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let opts = args.first().cloned().unwrap_or(Value::Null);
    let requested_game_id = opts
        .get("gameId")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let version_type = opts
        .get("versionType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    if let Some(refusal) = super::engine::unsupported_channel(&version_type) {
        return Ok(err_response(refusal));
    }
    let profile = resolve_profile(app, requested_game_id.as_deref());
    let profile_id = game_profiles::profile_id(profile).to_string();

    if !game_profiles::is_managed(profile) {
        return Ok(err_response(format!(
            "{} install/update isn't managed by Peebify Launcher yet. Install it with the official launcher, then use \"Locate existing install\".",
            game_profiles::display_name(profile)
        )));
    }

    if pools(app).queue.has_download_for(&profile_id) {
        log::info!("A download for {profile_id} is already running or queued, so the new request joins it.");
        return Ok(super::queue::already_queued(&profile_id));
    }

    let state = app.state::<BackendState>();
    let previous_path = configured_game_path(app, &profile_id);
    let was_installed = if previous_path.is_empty() {
        false
    } else {
        let prev = previous_path.clone();
        tauri::async_runtime::spawn_blocking(move || {
            game_profiles::validate_game_path_for_profile(&prev, profile).is_valid
        })
        .await
        .unwrap_or(false)
    };

    let mut selected_path = opts
        .get("installPath")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| previous_path.clone());

    let recorded_pending = state
        .config
        .get(&format!("{}.path", pending_install_key(&profile_id)))
        .as_str()
        .map(str::to_string);
    let known_path = super::fs_util::same_path(&selected_path, &previous_path)
        || recorded_pending
            .as_deref()
            .is_some_and(|p| super::fs_util::same_path(p, &selected_path));
    if !selected_path.is_empty() && !known_path {
        if let Some(refusal) = renderer_install_path_refusal(profile, &selected_path) {
            log::warn!("Refusing to download {profile_id} into {selected_path}: {refusal}");
            return Ok(err_response(refusal));
        }
    }

    if was_installed && !super::fs_util::same_path(&selected_path, &previous_path) {
        let pending_path = state
            .config
            .get(&format!("{}.path", pending_install_key(&profile_id)));
        if pending_path.as_str().is_some_and(|p| super::fs_util::same_path(p, &selected_path)) {
            log::info!(
                "{profile_id} is installed at {previous_path}, so its stale unfinished-install path {selected_path} is not used."
            );
            selected_path = previous_path.clone();
        }
    }

    if selected_path.is_empty() {
        let picked = pick_folder(
            app,
            Pick {
                title: "Select Installation Folder".into(),
                default_path: super::config::browse_start_dir(app, profile),
                ..Pick::default()
            },
        )
        .await?;
        match picked {
            Some(p) => {
                selected_path = with_game_folder(&p, &game_profiles::install_folder_name(profile));
                let target = Path::new(&selected_path);
                if !target.exists() {
                    if let Some(too_deep) = game_profiles::path_budget_error(target, profile) {
                        return Ok(err_response(too_deep));
                    }
                }
            }
            None => {
                return Ok(json!({
                    "success": false,
                    "cancelled": true,
                    "error": "Installation path not selected.",
                }))
            }
        }
    }

    if let Some(offline) = drive_offline_error(game_profiles::display_name(profile), &selected_path)
    {
        log::warn!("Refusing to download {profile_id}: {offline}");
        return Ok(err_response(offline));
    }

    if !was_installed && game_file_ops::inside_launcher_payload(Path::new(&selected_path)) {
        log::warn!("Refusing to download {profile_id} into the launcher's own folder: {selected_path}");
        return Ok(err_response(game_file_ops::LAUNCHER_PAYLOAD));
    }

    let defer_if_busy = opts.get("deferIfBusy") == Some(&Value::Bool(true));
    if was_installed && !defer_if_busy {
        if let Some(refusal) = game_file_ops::refuse_if_running(app, profile).await {
            return Ok(refusal);
        }
    }

    let previous_pending = state.config.get(&pending_install_key(&profile_id));
    let had_pending = !previous_pending.is_null();
    record_pending_install(app, &profile_id, &selected_path, &version_type);

    let manager = pools(app).download_manager(app, profile);
    let install_path = PathBuf::from(&selected_path);
    let path_was_empty = previous_path.is_empty();
    let in_place = was_installed && super::fs_util::same_path(&selected_path, &previous_path);
    let ran = Arc::new(AtomicBool::new(false));
    let run = Box::pin({
        let manager = Arc::clone(&manager);
        let install_path = install_path.clone();
        let app = app.clone();
        let game_id = profile_id.clone();
        let ran = Arc::clone(&ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            manager.prepare_start();
            let _prepared = PreparedStart(|| manager.abandon_start());
            if defer_if_busy {
                if let Some(busy) = super::game_manager::busy_reason(&app, &game_id).await {
                    forget_pending_install(&app, &game_id);
                    if manager.start_cancelled() {
                        return cancelled_before_start(&app, &game_id);
                    }
                    super::queue::publish(
                        &app,
                        "download-progress",
                        Phase::Deferred,
                        json!({ "status": "Deferred", "gameId": game_id, "percentage": 0 }),
                    );
                    return json!({
                        "success": false,
                        "deferredBusy": busy,
                        "gameId": game_id,
                        "error": "Update deferred while a game is running.",
                    });
                }
            }
            if was_installed {
                if let Some(message) =
                    game_file_ops::running_refusal(&app, game_profiles::profile(&game_id)).await
                {
                    if !had_pending {
                        forget_pending_install(&app, &game_id);
                    }
                    if manager.start_cancelled() {
                        return cancelled_before_start(&app, &game_id);
                    }
                    super::queue::publish(
                        &app,
                        "download-progress",
                        Phase::Error,
                        json!({
                            "status": "Error",
                            "gameId": game_id,
                            "percentage": 0,
                            "error": message,
                        }),
                    );
                    return json!({
                        "success": false,
                        "gameRunning": true,
                        "gameId": game_id,
                        "error": message,
                    });
                }
            } else {
                super::fs_util::create_shared_root_for(&install_path, &game_profiles::system_drive_games_root());
            }
            let on_complete: super::engine::CompletionHook = {
                let app = app.clone();
                let game_id = game_id.clone();
                Box::new(move |path: PathBuf| {
                    let final_path = nested_install_dir(profile, &path).unwrap_or(path);
                    super::config::set_config_value(
                        &app,
                        &format!("games.{game_id}.gamePath"),
                        json!(final_path.to_string_lossy()),
                    );
                    forget_pending_install(&app, &game_id);
                })
            };
            let result = manager
                .download_game_with(&install_path, in_place, Some(on_complete))
                .await;
            if result.get("cancelled") == Some(&Value::Bool(true)) {
                if !was_installed {
                    discard_cancelled_install(&game_id, &install_path).await;
                    if path_was_empty {
                        super::config::set_config_value(
                            &app,
                            &format!("games.{game_id}.gamePath"),
                            json!(""),
                        );
                    }
                }
                forget_pending_install(&app, &game_id);
            }
            result
        }
    });
    let kind = if was_installed { "update" } else { "install" };
    let result = run_queued(app, &profile_id, "download", kind, "download-progress", run).await;

    if result.get("alreadyQueued") == Some(&Value::Bool(true)) {
        return Ok(result);
    }

    if result.get("deferredBusy").is_some_and(Value::is_string)
        || result.get("gameRunning") == Some(&Value::Bool(true))
    {
        return Ok(result);
    }

    if result["success"] == Value::Bool(true) {
        forget_pending_install(app, &profile_id);
        let mut final_path = result
            .get("installPath")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| selected_path.clone());

        let base = PathBuf::from(&selected_path);
        let resolved =
            tauri::async_runtime::spawn_blocking(move || nested_install_dir(profile, &base))
                .await
                .ok()
                .flatten();
        if let Some(resolved) = resolved {
            final_path = resolved.to_string_lossy().to_string();
        }

        super::config::set_config_value(
            app,
            &format!("games.{profile_id}.gamePath"),
            json!(final_path),
        );
        let _ = app.emit(
            "installation-complete",
            json!({
                "gamePath": final_path,
                "version": result.get("version").cloned().unwrap_or(Value::Null),
                "gameId": profile_id,
            }),
        );
        super::platform::notify_if_backgrounded(
            app,
            &format!("{} is ready", game_profiles::display_name(profile)),
            "Download complete. The game is ready to play.",
        );
    } else {
        let error = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cancelled = result.get("cancelled") == Some(&Value::Bool(true));
        if !cancelled && !error.is_empty() {
            super::platform::notify_if_backgrounded(
                app,
                &format!("{} download failed", game_profiles::display_name(profile)),
                &error,
            );
        }
        if cancelled && !ran.load(Ordering::SeqCst) {
            if !was_installed {
                if let Some(path) = previous_pending.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                    discard_cancelled_install(&profile_id, Path::new(path)).await;
                }
                if path_was_empty {
                    super::config::set_config_value(
                        app,
                        &format!("games.{profile_id}.gamePath"),
                        json!(""),
                    );
                }
            }
            forget_pending_install(app, &profile_id);
        }
        if !cancelled && !was_installed && path_was_empty {
            super::config::set_config_value(
                app,
                &format!("games.{profile_id}.gamePath"),
                json!(""),
            );
        }
    }

    Ok(result)
}

fn profile_arg(app: &AppHandle, args: &[Value]) -> (&'static Value, &'static str) {
    let profile = resolve_profile(app, arg_str(args, 0));
    (profile, game_profiles::profile_id(profile))
}

fn cancel_queued(
    app: &AppHandle,
    profile_id: &str,
    kind: &str,
    event: &str,
    percentage: bool,
) -> bool {
    let pools = pools(app);
    if !pools.queue.cancel_pending(profile_id, Some(kind)) {
        return false;
    }
    let running = pools
        .queue
        .current_meta()
        .is_some_and(|m| m.game_id == profile_id)
        || pools.queue.has_parked(profile_id);
    if running {
        log::info!(
            "Removed a queued {kind} for {profile_id} while another of its operations runs."
        );
        return true;
    }
    let mut payload = json!({ "status": "Cancelled", "gameId": profile_id });
    if percentage {
        if let Some(map) = payload.as_object_mut() {
            map.insert("percentage".to_string(), json!(0));
        }
    }
    super::queue::publish(app, event, Phase::Cancelled, payload);
    true
}

pub(super) async fn pause_download(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    if let Some(mgr) = pools(app).existing_download_manager(profile_id) {
        mgr.pause_download();
    }
    Ok(ok_response())
}

pub(super) async fn keep_going_while_playing(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    let pools = pools(app);
    if let Some(mgr) = pools.existing_download_manager(profile_id) {
        mgr.keep_downloading_while_playing();
    }
    if let Some(mgr) = pools.existing_repair_manager(profile_id) {
        mgr.keep_repairing_while_playing();
    }
    Ok(ok_response())
}

pub(super) async fn resume_download(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    let pools = pools(app);
    if pools.queue.has_parked(profile_id) {
        log::info!(
            "resume-download for {profile_id}: parked behind a prioritized op, it resumes when that finishes."
        );
        return Ok(ok_response());
    }
    if let Some(mgr) = pools.existing_download_manager(profile_id) {
        mgr.resume_download();
    }
    Ok(ok_response())
}

pub(super) async fn cancel_download(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    super::kuro::forget_quality_switch(app, profile_id);
    let pools = pools(app);
    if pools.queue.has_pending(profile_id, Some("download")) || pools.queue.download_in_flight(profile_id) {
        forget_pending_install(app, profile_id);
    }
    if cancel_queued(app, profile_id, "download", "download-progress", true) {
        return Ok(ok_response());
    }
    match pools.existing_download_manager(profile_id) {
        Some(mgr) => mgr.cancel_download(),
        None => log::info!("cancel-download for {profile_id}: nothing to cancel."),
    }
    Ok(ok_response())
}

pub(super) async fn prioritize_download(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile);
    let pools = pools(app);
    if pools
        .queue
        .current_meta()
        .map(|m| m.game_id == profile_id)
        .unwrap_or(false)
    {
        return Ok(ok_response());
    }
    if !pools.queue.prioritize(profile_id) {
        return Ok(ok_response());
    }
    let Some(active) = pools.queue.current_meta() else {
        return Ok(ok_response());
    };
    if active.game_id == profile_id {
        return Ok(ok_response());
    }
    let defer = |other: &str| -> Result<Value, String> {
        log::info!(
            "Moved {profile_id} to the front of the queue; it starts after the current {other} for {} finishes.",
            active.game_id
        );
        Ok(ok_with(json!({
            "deferred": format!(
                "{} starts after the current {other} finishes.",
                game_profiles::display_name(profile)
            ),
        })))
    };
    match active.op_type.as_str() {
        "download" => {
            let Some(mgr) = pools.existing_download_manager(&active.game_id) else {
                return defer("download");
            };
            if mgr.is_extracting() {
                log::info!(
                    "Moved {profile_id} to the front of the queue; it starts after {} finishes unpacking.",
                    active.game_id
                );
                return Ok(ok_with(json!({
                    "deferred": format!(
                        "{} starts after the current install finishes unpacking.",
                        game_profiles::display_name(profile)
                    ),
                })));
            }
            let was_paused = mgr.is_paused();
            mgr.pause_download();
            if !mgr.is_paused() {
                return defer("download");
            }
            if !pools.queue.park_current(active.id, was_paused) && !was_paused {
                mgr.resume_download();
            }
        }
        "repair" => {
            let mgr = pools.repairs.lock().get(&active.game_id).cloned();
            let Some(mgr) = mgr else {
                return defer("repair");
            };
            let was_paused = mgr.is_paused();
            mgr.pause_repair();
            if !mgr.is_paused() {
                return defer("repair");
            }
            if !pools.queue.park_current(active.id, was_paused) && !was_paused {
                mgr.resume_repair();
            }
        }
        other => return defer(other),
    }
    Ok(ok_response())
}

// ------------ Repair Commands ------------
// Full and quick repair, plus pause, resume and cancel for repairs, moves and verifies.
async fn handle_repair_request(app: &AppHandle, mode: &str, game_id: Option<&str>) -> Value {
    let profile = resolve_profile(app, game_id);
    let profile_id = game_profiles::profile_id(profile).to_string();
    let game_path = configured_game_path(app, &profile_id);
    if game_path.is_empty() {
        log::warn!(
            "Repair attempted without a configured game path (mode: {mode}, game: {profile_id})."
        );
        return err_response("Game path is not configured.");
    }
    if !game_profiles::is_managed(profile) {
        return err_response(format!(
            "{} repair is not managed by Peebify Launcher yet.",
            game_profiles::display_name(profile)
        ));
    }
    if let Some(missing) = missing_install(profile, &game_path).await {
        log::warn!("Refusing to repair {profile_id}: {missing}");
        return err_response(missing);
    }

    let pools = pools(app);
    let repair_active = pools
        .queue
        .current_meta()
        .is_some_and(|m| m.game_id == profile_id && m.op_type == "repair");
    if repair_active || pools.queue.has_pending(&profile_id, Some("repair")) {
        log::info!("A repair for {profile_id} is already running or queued; ignoring the new {mode} request.");
        return ok_response();
    }
    if let Some(refusal) = refuse_beside_download(app, &profile_id, "repair") {
        return refusal;
    }

    let manager = pools.repair_manager(app, profile);
    let mode = mode.to_string();
    let run = Box::pin({
        let manager = Arc::clone(&manager);
        let app = app.clone();
        let profile_id = profile_id.clone();
        async move {
            manager.prepare_start();
            let _prepared = PreparedStart(|| manager.abandon_start());
            let game_path = configured_game_path(&app, &profile_id);
            if let Some(problem) = install_blocker(profile, &game_path, "Game path is not configured.").await {
                log::warn!("Repair of {profile_id} stopped before it began: {problem}");
                super::queue::publish(
                    &app,
                    "repair-progress",
                    Phase::Error,
                    json!({
                        "status": super::engine::status::ERROR,
                        "gameId": profile_id,
                        "error": problem,
                    }),
                );
                return err_response(problem);
            }
            manager.repair_game(Path::new(&game_path), &mode).await;
            Value::Null
        }
    });
    match enqueue_or_refuse(app, &profile_id, "repair", "repair", "repair-progress", run) {
        Ok(_) => ok_response(),
        Err(refusal) => {
            log::info!("Repair of {profile_id} was refused before it was queued.");
            refusal
        }
    }
}

pub(super) async fn start_repair(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    Ok(handle_repair_request(app, "full", arg_str(args, 0)).await)
}

pub(super) async fn start_quick_repair(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    Ok(handle_repair_request(app, "quick", arg_str(args, 0)).await)
}

pub(super) async fn cancel_repair(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    if cancel_queued(app, profile_id, "repair", "repair-progress", false) {
        return Ok(ok_response());
    }
    let mgr = pools(app).repairs.lock().get(profile_id).cloned();
    match mgr {
        Some(mgr) => mgr.cancel_repair(),
        None => log::info!("cancel-repair for {profile_id}: nothing to cancel."),
    }
    Ok(ok_response())
}

pub(super) async fn cancel_move(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    if cancel_queued(app, profile_id, "move", "move-progress", true) {
        return Ok(ok_response());
    }
    if !game_file_ops::cancel_running_move(profile_id) {
        log::info!("cancel-move for {profile_id}: no move to cancel.");
    }
    Ok(ok_response())
}

pub(super) async fn pause_repair(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    if let Some(mgr) = pools(app).repairs.lock().get(profile_id) {
        mgr.pause_repair();
    }
    Ok(ok_response())
}

pub(super) async fn resume_repair(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    let pools = pools(app);
    if pools.queue.has_parked(profile_id) {
        log::info!(
            "resume-repair for {profile_id}: parked behind a prioritized op, it resumes when that finishes."
        );
        return Ok(ok_response());
    }
    if let Some(mgr) = pools.repairs.lock().get(profile_id) {
        mgr.resume_repair();
    }
    Ok(ok_response())
}

pub(super) async fn cancel_verify(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (_, profile_id) = profile_arg(app, args);
    if cancel_queued(app, profile_id, "verify", "download-progress", true) {
        return Ok(ok_response());
    }
    let pools = pools(app);
    let flag = pools.verify_cancels.lock().get(profile_id).cloned();
    if let Some(flag) = flag {
        flag.store(true, Ordering::SeqCst);
        return Ok(ok_response());
    }
    if pools.queue.download_in_flight(profile_id) {
        if let Some(mgr) = pools.existing_download_manager(profile_id) {
            log::info!(
                "cancel-verify for {profile_id}: no verify running, forwarded to the download."
            );
            forget_pending_install(app, profile_id);
            mgr.cancel_download();
            return Ok(ok_response());
        }
    }
    log::info!("cancel-verify for {profile_id}: nothing to cancel.");
    Ok(ok_response())
}

// ------------ File Verification ------------
// Queues a verify of the installed files (the check itself lives with repair) and reports how it ended.
fn drive_offline_error(name: &str, game_path: &str) -> Option<String> {
    let root = super::fs_util::drive_root(game_path)?;
    if Path::new(&root).is_dir() {
        return None;
    }
    Some(format!(
        "The drive that holds {name} ({root}) is not connected. Reconnect it and try again."
    ))
}

async fn install_blocker(profile: &'static Value, game_path: &str, empty: &str) -> Option<String> {
    if game_path.is_empty() {
        return Some(empty.to_string());
    }
    missing_install(profile, game_path).await
}

async fn missing_install(profile: &'static Value, game_path: &str) -> Option<String> {
    let path = game_path.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        missing_install_error(game_profiles::display_name(profile), &path)
    })
    .await
    .unwrap_or(None)
}

fn missing_install_error(name: &str, game_path: &str) -> Option<String> {
    if Path::new(game_path).is_dir() {
        return None;
    }
    Some(drive_offline_error(name, game_path).unwrap_or_else(|| {
        format!(
            "The {name} folder {game_path} is missing. Reconnect the drive, use Locate existing install, or reinstall the game."
        )
    }))
}

pub(super) fn verify_cancelled_response() -> Value {
    json!({ "success": false, "cancelled": true, "error": "Cancelled." })
}

pub(super) async fn verify_game_integrity(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let game_path = configured_game_path(app, &profile_id);
    if let Some(problem) = install_blocker(profile, &game_path, "Game path not set.").await {
        log::warn!("Refusing to verify {profile_id}: {problem}");
        return Ok(err_response(problem));
    }

    let app_for_job = app.clone();
    let profile_id_for_job = profile_id.clone();
    let run = Box::pin(async move {
        let game_path = configured_game_path(&app_for_job, &profile_id_for_job);
        if let Some(problem) = install_blocker(profile, &game_path, "Game path not set.").await {
            log::warn!("Verify of {profile_id_for_job} stopped before it began: {problem}");
            super::queue::publish(
                &app_for_job,
                "download-progress",
                verify_update(Phase::Error),
                json!({
                    "status": "Verification Failed",
                    "gameId": profile_id_for_job,
                    "percentage": 0,
                    "error": problem,
                }),
            );
            return err_response(problem);
        }
        run_verify(&app_for_job, profile, &profile_id_for_job, &game_path).await
    });
    Ok(run_queued(app, &profile_id, "verify", "verify", "download-progress", run).await)
}

async fn run_verify(
    app: &AppHandle,
    profile: &'static Value,
    profile_id: &str,
    game_path: &str,
) -> Value {
    let _power = super::platform::TransferGuard::acquire();
    let pools = pools(app);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    pools
        .verify_cancels
        .lock()
        .insert(profile_id.to_string(), Arc::clone(&cancel_flag));

    let job = JobCore::verify(app, profile, &cancel_flag);
    let result = engine::verify(&job, game_path).await;

    pools.verify_cancels.lock().remove(profile_id);

    match result {
        Ok(mut response) => {
            if response["success"] == Value::Bool(true) && response.get("updatePending").is_none()
            {
                response["updatePending"] = json!(false);
            }
            if verify_left_no_broken_files(manifest::publisher(profile).policy(Path::new(game_path)).partial_verify, &response) {
                clear_update_incomplete(app, profile_id);
            }
            response
        }
        Err(VerifyError::Cancelled) => {
            log::info!("Integrity verification cancelled for {profile_id}.");
            job.publish(
                verify_update(Phase::Cancelled),
                json!({ "status": "Verification Cancelled", "percentage": 0 }),
            );
            verify_cancelled_response()
        }
        Err(VerifyError::Other(e)) => {
            log::error!("Game integrity verification failed for {profile_id}: {e}");
            job.publish(
                verify_update(Phase::Error),
                json!({ "status": "Verification Failed", "error": e }),
            );
            err_response(e)
        }
    }
}
fn verify_left_no_broken_files(partial_verify: bool, response: &Value) -> bool {
    !partial_verify
        && response["success"] == Value::Bool(true)
        && response["updatePending"] != Value::Bool(true)
        && response["invalidFiles"].as_array().is_some_and(Vec::is_empty)
}

pub(super) async fn move_game_location(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = match game_file_ops::named_known_profile(arg_str(args, 0)) {
        Ok(profile) => profile,
        Err(refusal) => return Ok(refusal),
    };
    if let Some(refusal) = game_file_ops::refuse_if_running(app, profile).await {
        return Ok(refusal);
    }
    let profile_id = game_profiles::profile_id(profile).to_string();
    if let Some(refusal) = refuse_beside_download(app, &profile_id, "move") {
        return Ok(refusal);
    }
    let old_path = configured_game_path(app, &profile_id);
    if old_path.is_empty() {
        return Ok(err_response("Current game path is not set."));
    }
    if let Some(app_id) = steam_copy_at(&old_path, profile).await {
        log::info!("Refusing to move {profile_id} because it is the Steam copy (app {app_id}).");
        return Ok(err_response(format!(
            "{} is installed through Steam. Move it from Steam under Properties, Installed Files, then use Locate to point Peebify at the new folder.",
            game_profiles::display_name(profile)
        )));
    }

    let new_path = match game_file_ops::pick_move_destination(app, profile, &old_path).await? {
        Ok(path) => path,
        Err(early_response) => return Ok(early_response),
    };

    let app_for_job = app.clone();
    let run = Box::pin(async move {
        game_file_ops::run_move(&app_for_job, profile, PathBuf::from(old_path), new_path).await
    });
    Ok(run_queued(app, &profile_id, "move", "move", "move-progress", run).await)
}

pub(super) async fn uninstall_game(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let target = arg_str(args, 0).map(str::to_string);
    Ok(game_file_ops::uninstall_game(app, target.as_deref()).await)
}

pub(super) async fn open_leftover_folder(args: &[Value]) -> Result<Value, String> {
    Ok(game_file_ops::open_leftover_folder(arg_str(args, 0)).await)
}

pub(super) async fn select_install_directory(app: &AppHandle) -> Result<Value, String> {
    let path = pick_folder(
        app,
        Pick {
            title: "Select Custom Installation Folder".into(),
            ..Pick::default()
        },
    )
    .await?;
    Ok(json!({ "canceled": path.is_none(), "path": path }))
}

// ------------ Game Storage ------------
// Measures how much disk a game uses, for the Storage section on the Games settings tab: the install folder, the
// game's mods folder and the screenshots and clips Peebify saved for it, plus free space on the drive.
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "jxr"];
const VIDEO_EXT: &[&str] = &["mp4", "webm", "mkv", "mov", "m4v"];

#[derive(Default, Debug, PartialEq, Eq)]
struct MediaTally {
    bytes: u64,
    screenshots: u64,
    clips: u64,
}

pub(super) async fn get_game_storage(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(game_id) = arg_str(args, 0).filter(|id| game_profiles::is_known_game_id(id)) else {
        return Ok(err_response("Unknown game."));
    };
    let game_id = game_id.to_string();
    let profile = game_profiles::profile(&game_id);
    let game_path = configured_game_path(app, game_profiles::profile_id(profile));
    let mods = super::mods::storage_dir(app, &game_id);
    let capture_root = super::overlay::capture_dir(app);
    let media_dir = capture_root.join(super::capture::game_folder(&game_id));

    let measured = tauri::async_runtime::spawn_blocking(move || {
        let game_dir = PathBuf::from(&game_path);
        let has_game = !game_path.is_empty() && game_dir.is_dir();
        let size = |dir: &Path| super::fs_util::directory_size(dir);
        let mut game_bytes = if has_game { size(&game_dir) } else { 0 };

        let mods = mods.map(|(dir, installed, enabled)| {
            let bytes = if dir.is_dir() { size(&dir) } else { 0 };
            if has_game && dir.starts_with(&game_dir) {
                game_bytes = game_bytes.saturating_sub(bytes);
            }
            json!({ "bytes": bytes, "installed": installed, "enabled": enabled })
        });

        let media = if media_dir.is_dir() {
            tally_media(&media_dir)
        } else {
            MediaTally::default()
        };

        let probe = if has_game { game_dir.clone() } else { capture_root };
        let disk = super::fs_util::disk_space_for(&probe);
        let drive = super::fs_util::drive_root(&probe.to_string_lossy()).unwrap_or_default();

        json!({
            "gamePath": if has_game { game_path } else { String::new() },
            "gameBytes": game_bytes,
            "mods": mods,
            "media": {
                "bytes": media.bytes,
                "screenshots": media.screenshots,
                "clips": media.clips,
            },
            "drive": drive.trim_end_matches('\\'),
            "free": disk.map(|(free, _)| free),
            "total": disk.map(|(_, total)| total),
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(ok_with(measured))
}

fn tally_media(dir: &Path) -> MediaTally {
    let mut tally = MediaTally::default();
    super::fs_util::walk_files(dir, &AtomicBool::new(false), |path, len| {
        let hidden = path
            .strip_prefix(dir)
            .is_ok_and(|rel| rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')));
        if hidden {
            return;
        }
        tally.bytes = tally.bytes.saturating_add(len);
        match media_kind(path) {
            Some(MediaKind::Image) => tally.screenshots += 1,
            Some(MediaKind::Video) => tally.clips += 1,
            None => {}
        }
    });
    tally
}

#[derive(Debug, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

// ------------ Disk Space And Default Folders ------------
// The free space and default install folder commands, and the startup scan that adopts a game it finds already sitting in one of the default folders.
pub(super) async fn get_disk_space(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let target = arg_str(args, 0).unwrap_or("");
    let probe = super::fs_util::existing_probe_dir(Path::new(target))
        .or_else(|| super::fs_util::existing_probe_dir(&app.state::<BackendState>().user_data))
        .or_else(|| std::env::var("USERPROFILE").ok());
    let Some(probe) = probe else {
        return Ok(err_response("Could not resolve a probe path."));
    };

    match super::fs_util::disk_free_total(&probe) {
        Ok((free, total)) => Ok(ok_with(
            json!({ "free": free, "total": total, "path": probe }),
        )),
        Err(e) => {
            log::error!("Failed to read disk space: {e}");
            Ok(err_response(e))
        }
    }
}

fn has_pending_install(config: &super::config::LauncherConfig, profile_id: &str) -> bool {
    config
        .get(&pending_install_key(profile_id))
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty())
}

pub(crate) async fn adopt_default_location_installs(app: &AppHandle) -> Vec<String> {
    game_file_ops::recover_interrupted_moves(app).await;
    let config = app.state::<BackendState>().config.clone();
    let mut adopted = Vec::new();

    for id in game_profiles::GAME_IDS {
        let configured = config.get(&format!("games.{id}.gamePath"));
        if !configured.as_str().unwrap_or("").is_empty() {
            continue;
        }
        if has_pending_install(&config, id) {
            log::debug!("Startup scan: {id} has an unfinished install, so it is not adopted.");
            continue;
        }
        let profile = game_profiles::profile(id);
        let found = tauri::async_runtime::spawn_blocking(move || game_profiles::install_in_default_dirs(profile))
            .await
            .unwrap_or(None);
        let Some(path) = found else {
            continue;
        };
        log::info!(
            "Startup scan: {} is already installed at {path}, adding it to the library.",
            game_profiles::display_name(profile)
        );
        super::config::set_config_value(app, &format!("games.{id}.gamePath"), json!(path));
        game_profiles::resolve_audio_language(app, profile, Path::new(&path), None).await;
        adopted.push(id.to_string());
    }

    if adopted.is_empty() {
        log::debug!("Startup scan: no unregistered games in Peebify's default install folders.");
    }
    adopted
}

pub(super) async fn get_install_path_health(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let (profile, profile_id) = profile_arg(app, args);
    let game_path = configured_game_path(app, profile_id);

    let too_deep = (!game_path.is_empty())
        .then(|| game_profiles::path_budget_error(Path::new(&game_path), profile))
        .flatten();
    let (not_writable, missing) = if game_path.is_empty() {
        (false, None)
    } else {
        let path = game_path.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let not_writable = super::fs_util::dir_denies_writes(Path::new(&path));
            let missing = missing_install_error(game_profiles::display_name(profile), &path);
            (not_writable, missing)
        })
        .await
        .unwrap_or((false, None))
    };
    let message = missing.clone().or_else(|| too_deep.clone()).or_else(|| {
        not_writable.then(|| super::fs_util::not_writable_message(Path::new(&game_path)))
    });

    Ok(ok_with(json!({
        "gameId": profile_id,
        "path": game_path,
        "tooDeep": too_deep.is_some(),
        "notWritable": not_writable,
        "missing": missing.is_some(),
        "message": message,
    })))
}

pub(super) async fn get_default_install_path(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let opts = args.first().cloned().unwrap_or(Value::Null);
    let profile = resolve_profile(app, opts.get("gameId").and_then(|v| v.as_str()));

    let attempts = game_profiles::default_install_dirs(profile);
    let mut last_error: Option<String> = None;
    for (_, dir) in &attempts {
        match super::fs_util::preview_writable(dir) {
            Ok(()) => {
                return Ok(ok_with(json!({
                    "path": dir.to_string_lossy(),
                    "maxRootLength": game_profiles::max_install_root_len(profile),
                    "folderName": game_profiles::install_folder_name(profile),
                })));
            }
            Err(e) => {
                log::warn!(
                    "Default install path candidate {} unwritable: {e}",
                    dir.display()
                );
                last_error = Some(e);
            }
        }
    }

    let message = match &last_error {
        Some(e) if e.to_lowercase().contains("denied") || e.to_lowercase().contains("access") => {
            "Could not create the default install folder. Pick a folder you have write access to."
                .to_string()
        }
        Some(e) => e.clone(),
        None => "Could not resolve a writable default install path.".to_string(),
    };
    Ok(err_response(message))
}

// ------------ Tests ------------
// Where a renderer may install, how a verify reports its result, missing installs, and measuring a game's storage.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;

    #[test]
    fn renderer_install_paths_must_name_the_games_own_folder() {
        let profile = game_profiles::profile("wuwa");
        assert_eq!(
            renderer_install_path_refusal(profile, r"D:\Games\Wuthering Waves"),
            None
        );
        assert_eq!(
            renderer_install_path_refusal(profile, r"D:\Games\wuthering waves\"),
            None
        );
        for bad in [
            r"D:\Projects",
            r"C:\",
            r"C:\Users\me",
            r"Games\Wuthering Waves",
            r"D:\x\..\Wuthering Waves",
            r"D:\Games\Wuthering Waves\..",
        ] {
            assert!(
                renderer_install_path_refusal(profile, bad).is_some(),
                "{bad} was accepted"
            );
        }
        for dir in game_profiles::default_install_dirs(profile) {
            assert_eq!(renderer_install_path_refusal(profile, &dir.1.to_string_lossy()), None);
        }
    }

    #[test]
    fn only_a_verify_that_checked_every_file_clears_an_unfinished_update() {
        let clean = json!({ "success": true, "invalidFiles": [], "updatePending": false });
        assert!(verify_left_no_broken_files(false, &clean));
        let waiting = json!({ "success": true, "invalidFiles": [], "updatePending": true });
        assert!(!verify_left_no_broken_files(false, &waiting));
        let broken = json!({
            "success": true,
            "invalidFiles": [{ "dest": "a.pak" }],
            "updatePending": false,
        });
        assert!(!verify_left_no_broken_files(false, &broken));
        let failed = json!({ "success": false, "error": "offline" });
        assert!(!verify_left_no_broken_files(false, &failed));
        assert!(!verify_left_no_broken_files(true, &clean));
    }

    #[test]
    fn a_missing_install_folder_is_named_before_any_work_starts() {
        let tmp = TempDir::new("present");
        let dir = tmp.path().to_path_buf();
        assert_eq!(missing_install_error("Wuthering Waves", &dir.to_string_lossy()), None);
        let gone = dir.join("Wuthering Waves");
        let message = missing_install_error("Wuthering Waves", &gone.to_string_lossy()).unwrap();
        assert!(message.starts_with("The Wuthering Waves folder "));
        assert!(message.contains("is missing"));
    }

    #[cfg(windows)]
    #[test]
    fn an_unplugged_drive_is_told_apart_from_a_missing_folder() {
        let Some(letter) = ('D'..='Z')
            .rev()
            .find(|l| !Path::new(&format!("{l}:\\")).exists())
        else {
            return;
        };
        let path = format!("{letter}:\\Games\\Wuthering Waves");
        let message = missing_install_error("Wuthering Waves", &path).unwrap();
        assert_eq!(
            message,
            format!("The drive that holds Wuthering Waves ({letter}:\\) is not connected. Reconnect it and try again.")
        );
        assert_eq!(drive_offline_error("Wuthering Waves", &path), Some(message));
    }

    #[test]
    fn media_tally_counts_every_file_but_skips_hidden_folders() {
        let tmp = TempDir::new("tally");
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join("one.png"), [0u8; 10]).unwrap();
        std::fs::write(dir.join("two.mp4"), [0u8; 30]).unwrap();
        std::fs::create_dir_all(dir.join(".thumbs")).unwrap();
        std::fs::write(dir.join(".thumbs").join("shot.jpg"), [0u8; 100]).unwrap();
        std::fs::create_dir_all(dir.join("Session")).unwrap();
        std::fs::write(dir.join("Session").join("three.jpg"), [0u8; 5]).unwrap();

        let tally = tally_media(&dir);
        assert_eq!(tally, MediaTally { bytes: 45, screenshots: 2, clips: 1 });
    }
}
