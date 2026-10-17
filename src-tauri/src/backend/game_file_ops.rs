// ------------ Game Move And Uninstall ------------
// Moves a game to another folder or drive and uninstalls it, with progress, cancelling, retries for locked files, and recovery if Peebify was closed halfway through.
// Refuses to touch a folder shared with another game or Peebify's own install.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::fs_util::{
    fmt_io, is_same_or_inside, list_files, nearest_existing_dir, normalize_path as normalized,
};
use super::fs_util::dialog::Pick;
use super::queue::{Phase, Update};
use super::BackendState;
use super::{err_response, game_profiles, ok_response, ok_with, process_utils};

const STREAM_THRESHOLD: u64 = 64 * 1024 * 1024;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_NOT_SAME_DEVICE: i32 = 17;
const ERROR_SHARING_VIOLATION: i32 = 32;
const MOVE_NESTED: &str = "Pick a folder outside the current install.";
pub(super) const LAUNCHER_PAYLOAD: &str =
    "That folder is part of Peebify's own install. Pick a folder outside it, or use its games folder.";
const REMOVE_RETRY_BUDGET_MS: u64 = 5_000;
const REMOVE_RETRY_DELAY_MS: u64 = 100;
const REMOVE_RETRIES_PER_FILE: u32 = 3;
const MOVE_CANCELLED: &str = "Move cancelled.";
const REMOVING_OLD_COPY: &str = "Removing the old copy";
const MOVE_REPORT_INTERVAL: Duration = Duration::from_millis(120);
const MOVE_RATE_WINDOW: Duration = Duration::from_millis(500);

static UNINSTALLING: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());
static MOVE_CANCELS: parking_lot::Mutex<Vec<(String, Arc<AtomicBool>)>> =
    parking_lot::Mutex::new(Vec::new());

struct MoveCancel {
    profile_id: String,
    flag: Arc<AtomicBool>,
}

impl MoveCancel {
    fn register(profile_id: &str) -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        let mut cancels = MOVE_CANCELS.lock();
        cancels.retain(|(id, _)| id != profile_id);
        cancels.push((profile_id.to_string(), Arc::clone(&flag)));
        Self {
            profile_id: profile_id.to_string(),
            flag,
        }
    }
}

impl Drop for MoveCancel {
    fn drop(&mut self) {
        MOVE_CANCELS
            .lock()
            .retain(|(id, flag)| !(id == &self.profile_id && Arc::ptr_eq(flag, &self.flag)));
    }
}

pub(super) fn cancel_running_move(profile_id: &str) -> bool {
    let cancels = MOVE_CANCELS.lock();
    let Some((_, flag)) = cancels.iter().find(|(id, _)| id == profile_id) else {
        return false;
    };
    log::info!("cancel-move for {profile_id}: stopping the copy.");
    flag.store(true, Ordering::SeqCst);
    true
}

struct CopyRate {
    at: Instant,
    bytes: u64,
    speed: f64,
}

impl CopyRate {
    fn new(now: Instant) -> Self {
        Self {
            at: now,
            bytes: 0,
            speed: 0.0,
        }
    }

    fn sample(&mut self, now: Instant, bytes: u64) -> f64 {
        let elapsed = now.saturating_duration_since(self.at);
        if elapsed >= MOVE_RATE_WINDOW {
            let rate = bytes.saturating_sub(self.bytes) as f64 / elapsed.as_secs_f64();
            self.speed = if self.speed > 0.0 {
                0.6 * self.speed + 0.4 * rate
            } else {
                rate
            };
            self.at = now;
            self.bytes = bytes;
        }
        self.speed
    }
}

fn eta_secs(speed: f64, done: u64, total: u64) -> f64 {
    if speed > 0.0 {
        total.saturating_sub(done) as f64 / speed
    } else {
        0.0
    }
}
static LEFTOVER_FOLDERS: parking_lot::Mutex<Vec<(String, PathBuf)>> =
    parking_lot::Mutex::new(Vec::new());

fn remember_leftovers(profile_id: &str, dir: &Path) {
    let mut folders = LEFTOVER_FOLDERS.lock();
    folders.retain(|(id, _)| id != profile_id);
    folders.push((profile_id.to_string(), dir.to_path_buf()));
}

pub async fn open_leftover_folder(target_game_id: Option<&str>) -> Value {
    let profile = match named_known_profile(target_game_id) {
        Ok(profile) => profile,
        Err(refusal) => return refusal,
    };
    let profile_id = game_profiles::profile_id(profile);
    let recorded = LEFTOVER_FOLDERS
        .lock()
        .iter()
        .find(|(id, _)| id == profile_id)
        .map(|(_, dir)| dir.clone());
    let Some(dir) = recorded.as_deref().and_then(nearest_existing_dir) else {
        if let Some(root) = recorded
            .as_deref()
            .and_then(|dir| super::fs_util::drive_root(&dir.to_string_lossy()))
            .filter(|root| !Path::new(root).is_dir())
        {
            return err_response(format!(
                "The drive that holds the leftover folder ({root}) isn't connected. Reconnect it and try again."
            ));
        }
        return err_response("The leftover folder is gone.");
    };
    match super::fs_util::dialog::open_folder(dir) {
        Ok(()) => ok_response(),
        Err(e) => err_response(e),
    }
}

fn resolved(path: &Path) -> String {
    let real = std::fs::canonicalize(path)
        .map(|p| super::fs_util::plain_path(&p))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    normalized(&real)
}

fn is_nested(a: &str, b: &str) -> bool {
    is_same_or_inside(a, b) || is_same_or_inside(b, a)
}

fn in_launcher_payload(path: &str, launcher: &str) -> bool {
    is_same_or_inside(path, launcher)
        && !is_same_or_inside(
            path,
            &format!("{launcher}\\{}", super::game_profiles::GAMES_DIR_NAME),
        )
}

pub(super) fn inside_launcher_payload(path: &Path) -> bool {
    let launcher = super::game_profiles::launcher_dir();
    if launcher.as_os_str().is_empty() {
        return false;
    }
    let real = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !path.exists() => {
            format!("{}\\{}", resolved(parent), name.to_string_lossy())
        }
        _ => resolved(path),
    };
    in_launcher_payload(&path.to_string_lossy(), &launcher.to_string_lossy())
        || in_launcher_payload(&real, &resolved(&launcher))
}

struct Protected {
    exact: Vec<String>,
    tree: Vec<String>,
    keep: Vec<String>,
}

impl Protected {
    fn current() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut exact: Vec<String> = [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "PUBLIC",
        ]
        .iter()
        .filter_map(|name| env(name))
        .collect();
        if let Some(home) = env("USERPROFILE") {
            for sub in [
                "Desktop",
                "Documents",
                "Downloads",
                "Pictures",
                "Videos",
                "Music",
                "Saved Games",
                "Peebify Games",
            ] {
                exact.push(Path::new(&home).join(sub).to_string_lossy().into_owned());
            }
        }
        if let Some(one_drive) = env("OneDrive") {
            for sub in ["Desktop", "Documents", "Pictures"] {
                exact.push(Path::new(&one_drive).join(sub).to_string_lossy().into_owned());
            }
        }
        let system_drive = env("SystemDrive").unwrap_or_else(|| "C:".to_string());
        exact.push(format!("{system_drive}\\Peebify Games"));

        let tree: Vec<String> = ["SystemRoot", "windir"]
            .iter()
            .filter_map(|name| env(name))
            .collect();
        let mut keep: Vec<String> = ["USERPROFILE", "APPDATA", "LOCALAPPDATA", "OneDrive"]
            .iter()
            .filter_map(|name| env(name))
            .collect();

        let launcher = super::game_profiles::launcher_dir();
        if !launcher.as_os_str().is_empty() {
            exact.push(launcher.join("games").to_string_lossy().into_owned());
            keep.push(launcher.to_string_lossy().into_owned());
        }
        Self { exact, tree, keep }
    }

    fn refuses(&self, target: &str) -> bool {
        let key = normalized(target);
        if key.is_empty() || Path::new(&key).parent().is_none() {
            return true;
        }
        if key.ends_with("\\steamapps") || key.ends_with("\\steamapps\\common") {
            return true;
        }
        self.exact.iter().any(|p| normalized(p) == key)
            || self.tree.iter().any(|p| is_same_or_inside(&key, p))
            || self.keep.iter().any(|p| is_same_or_inside(p, &key))
    }
}

fn is_shared_folder(app: &AppHandle, profile_id: &str, path: &str) -> bool {
    let config = app.state::<BackendState>().config.clone();
    let mut protected = Protected::current();
    protected.keep.extend(other_game_paths(&config, profile_id));
    protected.refuses(&resolved(Path::new(path))) || protected.refuses(path)
}

pub(super) fn known_profile_or_error(
    app: &AppHandle,
    game_id: Option<&str>,
) -> Result<&'static Value, Value> {
    match game_id {
        Some(id) if !id.is_empty() => named_known_profile(Some(id)),
        _ => named_known_profile(Some(&app.state::<BackendState>().config.active_game_id())),
    }
}

pub(super) fn named_known_profile(game_id: Option<&str>) -> Result<&'static Value, Value> {
    let Some(id) = game_id.filter(|id| !id.is_empty()) else {
        log::warn!("Refusing a file operation that named no game.");
        return Err(err_response(
            "No game was given. Select the game again and retry.",
        ));
    };
    game_profiles::known_profile(id).ok_or_else(|| {
        log::warn!("Refusing a file operation for unknown game id \"{id}\".");
        err_response(format!(
            "Unknown game \"{id}\". Select the game again and retry."
        ))
    })
}

pub(super) async fn running_refusal(app: &AppHandle, profile: &Value) -> Option<String> {
    let profile_id = game_profiles::profile_id(profile);
    let proc_name = game_profiles::client_process_name(profile);
    let tracked = app.state::<BackendState>().game.is_game_running_id(profile_id);
    let live = tracked || (!proc_name.is_empty() && process_utils::is_process_running(proc_name).await);
    live.then(|| {
        format!(
            "{} is currently running. Close the game and try again.",
            game_profiles::display_name(profile)
        )
    })
}

pub(super) async fn refuse_if_running(app: &AppHandle, profile: &Value) -> Option<Value> {
    running_refusal(app, profile).await.map(err_response)
}

struct UninstallMark(String);

impl Drop for UninstallMark {
    fn drop(&mut self) {
        UNINSTALLING.lock().retain(|id| id != &self.0);
    }
}

fn mark_uninstalling(profile_id: &str) -> Option<UninstallMark> {
    let mut active = UNINSTALLING.lock();
    if active.iter().any(|id| id == profile_id) {
        return None;
    }
    active.push(profile_id.to_string());
    Some(UninstallMark(profile_id.to_string()))
}

pub(super) fn is_uninstalling(game_id: &str) -> bool {
    UNINSTALLING.lock().iter().any(|id| id == game_id)
}

pub(super) fn log_unfinished_work(app: &AppHandle) {
    if let Some(meta) = app.state::<BackendState>().engine.queue.current_meta() {
        log::warn!(
            "Quitting while a {} for {} is still running.",
            meta.op_type,
            meta.game_id
        );
    }
    for id in UNINSTALLING.lock().iter() {
        log::warn!("Quitting while {id} is still being uninstalled.");
    }
}

// ------------ Interrupted Move Recovery ------------
// Writes down a move before it starts so the next launch can finish or undo it, and cleans up old copies that were left behind.
fn pending_move_key(profile_id: &str) -> String {
    format!("games.{profile_id}.pendingMove")
}

fn record_pending_move(app: &AppHandle, profile_id: &str, from: &Path, to: &Path) {
    super::config::set_config_value(
        app,
        &pending_move_key(profile_id),
        json!({
            "from": from.to_string_lossy(),
            "to": to.to_string_lossy(),
            "startedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );
    app.state::<BackendState>().config.flush();
}

fn clear_pending_move(app: &AppHandle, profile_id: &str) {
    super::config::set_config_value(app, &pending_move_key(profile_id), Value::Null);
    app.state::<BackendState>().config.flush();
}

fn old_copy_key(profile_id: &str) -> String {
    format!("games.{profile_id}.leftoverFolder")
}

fn other_game_paths(config: &super::config::LauncherConfig, profile_id: &str) -> Vec<String> {
    game_profiles::GAME_IDS
        .iter()
        .filter(|other| **other != profile_id)
        .filter_map(|other| {
            config
                .get(&format!("games.{other}.gamePath"))
                .as_str()
                .filter(|p| !p.is_empty())
                .map(str::to_string)
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Recovery {
    Keep,
    Adopt(String),
    OldCopy(PathBuf),
}

fn recover_move(profile_id: &str, game_path: &str, from: &Path, to: &Path, others: &[String]) -> Recovery {
    let from_text = from.to_string_lossy().into_owned();
    let to_text = to.to_string_lossy().into_owned();
    if is_nested(&to_text, &from_text) {
        return Recovery::Keep;
    }
    if normalized(game_path) == normalized(&to_text) {
        if from.exists() {
            log::warn!(
                "Startup: an interrupted move of {profile_id} left the old copy at {from_text}."
            );
            return Recovery::OldCopy(from.to_path_buf());
        }
        return Recovery::Keep;
    }
    if normalized(game_path) != normalized(&from_text) || !to.is_dir() {
        return Recovery::Keep;
    }
    if !from.exists() {
        log::info!(
            "Startup: the interrupted move of {profile_id} had already finished, so the game now points at {to_text}."
        );
        return Recovery::Adopt(to_text);
    }
    if Protected::current().refuses(&resolved(to)) || others.iter().any(|o| is_nested(o, &to_text)) {
        log::warn!(
            "Startup: kept {to_text} from an interrupted move of {profile_id} because it is a shared folder."
        );
        return Recovery::Keep;
    }
    match std::fs::remove_dir_all(to) {
        Ok(()) => log::info!(
            "Startup: removed the partial copy at {to_text} left by an interrupted move of {profile_id}."
        ),
        Err(e) => log::warn!(
            "Startup: {}",
            fmt_io(&format!("Could not remove the partial copy {to_text}"), &e)
        ),
    }
    Recovery::Keep
}

fn old_copy_deletable(game_path: &str, from: &Path, others: &[String]) -> bool {
    let from_text = from.to_string_lossy().into_owned();
    let from_real = resolved(from);
    if !Path::new(game_path).is_dir()
        || is_nested(game_path, &from_text)
        || is_nested(&resolved(Path::new(game_path)), &from_real)
    {
        return false;
    }
    if !std::fs::symlink_metadata(from).is_ok_and(|m| !m.file_type().is_symlink()) {
        return false;
    }
    let protected = Protected::current();
    !protected.refuses(&from_text)
        && !protected.refuses(&from_real)
        && !others
            .iter()
            .any(|o| is_nested(o, &from_text) || is_nested(o, &from_real))
}

pub(super) async fn recover_interrupted_moves(app: &AppHandle) {
    let config = app.state::<BackendState>().config.clone();
    for id in game_profiles::GAME_IDS {
        let record = config.get(&pending_move_key(id));
        if record.is_null() {
            continue;
        }
        let (Some(from), Some(to)) = (record["from"].as_str(), record["to"].as_str()) else {
            clear_pending_move(app, id);
            continue;
        };
        let from = PathBuf::from(from);
        let to = PathBuf::from(to);
        let game_path = config
            .get(&format!("games.{id}.gamePath"))
            .as_str()
            .unwrap_or("")
            .to_string();
        let others = other_game_paths(&config, id);
        let recovery = tauri::async_runtime::spawn_blocking(move || {
            recover_move(id, &game_path, &from, &to, &others)
        })
        .await
        .unwrap_or(Recovery::Keep);
        match recovery {
            Recovery::Adopt(path) => {
                super::config::set_config_value(app, &format!("games.{id}.gamePath"), json!(path));
            }
            Recovery::OldCopy(dir) => {
                super::config::set_config_value(app, &old_copy_key(id), json!(dir.to_string_lossy()));
            }
            Recovery::Keep => {}
        }
        clear_pending_move(app, id);
    }
    let unfinished = game_profiles::GAME_IDS
        .iter()
        .any(|id| config.get(&old_copy_key(id)).as_str().is_some_and(|p| !p.is_empty()));
    if unfinished {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { finish_old_copy_cleanup(&app).await });
    }
}

async fn finish_old_copy_cleanup(app: &AppHandle) {
    super::window_manager::wait_for_renderer(app).await;
    let config = app.state::<BackendState>().config.clone();
    for id in game_profiles::GAME_IDS {
        let key = old_copy_key(id);
        let Some(from) = config
            .get(&key)
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
        else {
            continue;
        };
        let Some(_mark) = mark_uninstalling(id) else {
            continue;
        };
        if app.state::<BackendState>().engine.queue.has_job_for(id) {
            log::info!("Startup: left the old copy of {id} for later because it has work queued.");
            continue;
        }
        let profile = game_profiles::profile(id);
        let name = game_profiles::display_name(profile);
        let game_path = config
            .get(&format!("games.{id}.gamePath"))
            .as_str()
            .unwrap_or("")
            .to_string();
        let others = other_game_paths(&config, id);
        let check = from.clone();
        let deletable = tauri::async_runtime::spawn_blocking(move || {
            check.exists().then(|| old_copy_deletable(&game_path, &check, &others))
        })
        .await
        .unwrap_or(Some(false));
        let Some(deletable) = deletable else {
            log::info!("Startup: the old copy of {id} at {} is already gone.", from.display());
            super::config::set_config_value(app, &key, Value::Null);
            app.state::<BackendState>().config.flush();
            continue;
        };

        super::queue::publish(
            app,
            "move-progress",
            Phase::Moving,
            json!({
                "status": REMOVING_OLD_COPY,
                "percentage": 100,
                "speed": 0,
                "eta": 0,
                "gameId": id,
            }),
        );
        let failures = if deletable {
            log::info!("Startup: finishing the removal of the old copy of {id} at {}.", from.display());
            let target = from.clone();
            Some(
                tauri::async_runtime::spawn_blocking(move || remove_old_copy(&target))
                    .await
                    .unwrap_or_else(|e| {
                        vec![json!({ "file": from.to_string_lossy(), "error": e.to_string() })]
                    }),
            )
        } else {
            log::warn!(
                "Startup: kept the old copy of {id} at {} because it is shared, linked or the new copy is not reachable.",
                from.display()
            );
            None
        };
        super::config::set_config_value(app, &key, Value::Null);
        app.state::<BackendState>().config.flush();

        let warning = match &failures {
            Some(failures) if failures.is_empty() => None,
            Some(failures) => Some(format!(
                "Peebify closed before it finished removing the old copy of {name}. {} item{} in {} could not be removed. Delete them manually if needed.",
                failures.len(),
                if failures.len() == 1 { "" } else { "s" },
                from.display()
            )),
            None => Some(format!(
                "Peebify closed before it finished removing the old copy of {name}. The old files are still in {}.",
                from.display()
            )),
        };
        if let Some(warning) = &warning {
            log::warn!("Move ({id}): {warning}");
            remember_leftovers(id, &from);
        }
        let message = warning.clone().unwrap_or_else(|| {
            format!("Finished removing the old copy of {name} from {}.", from.display())
        });
        super::queue::publish(
            app,
            "move-progress",
            Update::new(Phase::Done).warning(warning.is_some()),
            json!({
                "percentage": 100,
                "gameId": id,
                "status": if warning.is_some() { "completed-with-warnings" } else { "completed" },
                "message": message,
            }),
        );
    }
}

// ------------ Move Game ------------
// Asks where to move to, then copies the files across (or just renames on the same drive) and removes the old copy.
pub async fn pick_move_destination(
    app: &AppHandle,
    profile: &'static Value,
    old_path: &str,
) -> Result<Result<PathBuf, Value>, String> {
    let picked = super::fs_util::dialog::pick_folder(
        app,
        Pick {
            title: format!("Select New {} Location", game_profiles::display_name(profile)),
            ..Pick::default()
        },
    )
    .await?;
    let Some(picked) = picked else {
        return Ok(Err(json!({
            "success": false,
            "cancelled": true,
            "error": "No new location selected."
        })));
    };

    let folder_name = game_profiles::install_folder_name(profile);
    let new_path = Path::new(&picked).join(&folder_name);

    if normalized(&new_path.to_string_lossy()) == normalized(old_path) {
        return Ok(Err(err_response(
            "The new location is the same as the current one.",
        )));
    }
    let old_real = resolved(Path::new(old_path));
    let new_real = format!("{}\\{}", resolved(Path::new(&picked)), folder_name);
    if is_nested(&new_real, &old_real) || is_nested(&new_path.to_string_lossy(), old_path) {
        return Ok(Err(err_response(MOVE_NESTED)));
    }
    if inside_launcher_payload(&new_path) {
        return Ok(Err(err_response(LAUNCHER_PAYLOAD)));
    }
    if new_path.exists() {
        return Ok(Err(err_response(format!(
            "A \"{folder_name}\" folder already exists at that location."
        ))));
    }
    if let Some(too_deep) = game_profiles::path_budget_error(&new_path, profile) {
        return Ok(Err(err_response(too_deep)));
    }
    Ok(Ok(new_path))
}

struct Transfer {
    renamed: bool,
    files: usize,
    bytes: u64,
    skipped_links: Vec<PathBuf>,
}

#[derive(Default)]
struct Scan {
    files: Vec<(PathBuf, u64)>,
    links: Vec<PathBuf>,
    total: u64,
}

fn scan_tree(dir: &Path, base: &Path, scan: &mut Scan) -> Result<(), String> {
    let list_error = |e: std::io::Error| fmt_io(&format!("Could not list {}", dir.display()), &e);
    for entry in std::fs::read_dir(dir).map_err(list_error)? {
        let entry = entry.map_err(list_error)?;
        let path = entry.path();
        let read_error = |e: std::io::Error| fmt_io(&format!("Could not read {}", path.display()), &e);
        let file_type = entry.file_type().map_err(read_error)?;
        if file_type.is_symlink() {
            let rel = path.strip_prefix(base).map_err(|e| e.to_string())?.to_path_buf();
            scan.links.push(rel);
        } else if file_type.is_dir() {
            scan_tree(&path, base, scan)?;
        } else {
            let size = entry.metadata().map_err(read_error)?.len();
            let rel = path.strip_prefix(base).map_err(|e| e.to_string())?.to_path_buf();
            scan.files.push((rel, size));
            scan.total += size;
        }
    }
    Ok(())
}

fn link_target_in(new_root: &Path, old_root: &Path, target: &Path) -> PathBuf {
    let target = PathBuf::from(super::fs_util::plain_path(target));
    let target_text = target.to_string_lossy();
    let old_text = old_root.to_string_lossy();
    let old_text = old_text.trim_end_matches(['\\', '/']);
    if !is_same_or_inside(&target_text, old_text) {
        return target;
    }
    match target_text.get(old_text.len()..) {
        Some(rest) => new_root.join(rest.trim_start_matches(['\\', '/'])),
        None => target,
    }
}

fn recreate_link(source: &Path, dest: &Path, old_root: &Path, new_root: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    let target = std::fs::read_link(source)?;
    let target = if target.is_absolute() {
        link_target_in(new_root, old_root, &target)
    } else {
        target
    };
    if std::fs::symlink_metadata(source)?.file_type().is_symlink_dir() {
        std::os::windows::fs::symlink_dir(&target, dest)
    } else {
        std::os::windows::fs::symlink_file(&target, dest)
    }
}

fn sync_copied(dest: &Path) -> std::io::Result<()> {
    let open = || std::fs::OpenOptions::new().write(true).open(dest);
    match open() {
        Ok(file) => file.sync_all(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let perms = std::fs::metadata(dest)?.permissions();
            if !perms.readonly() {
                return Err(e);
            }
            let mut writable = perms.clone();
            #[allow(clippy::permissions_set_readonly_false)]
            writable.set_readonly(false);
            std::fs::set_permissions(dest, writable)?;
            let synced = open().and_then(|file| file.sync_all());
            std::fs::set_permissions(dest, perms)?;
            synced
        }
        Err(e) => Err(e),
    }
}

fn same_volume(a: &Path, b: &Path) -> bool {
    match (
        super::fs_util::drive_root(&a.to_string_lossy()),
        super::fs_util::drive_root(&b.to_string_lossy()),
    ) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn stream_copy(
    source: &Path,
    dest: &Path,
    check_cancel: &dyn Fn() -> Result<(), String>,
    on_chunk: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let read_error = |e: std::io::Error| fmt_io(&format!("Could not read {}", source.display()), &e);
    let write_error = |e: std::io::Error| fmt_io(&format!("Could not write {}", dest.display()), &e);
    let mut src = std::fs::File::open(source).map_err(read_error)?;
    let mut dst = std::fs::File::create(dest).map_err(write_error)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        check_cancel()?;
        let n = src.read(&mut buf).map_err(read_error)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).map_err(write_error)?;
        on_chunk(n as u64);
    }
    let modified = src.metadata().and_then(|m| m.modified()).map_err(read_error)?;
    dst.set_modified(modified).map_err(write_error)?;
    dst.sync_all().map_err(write_error)
}

type MoveEmit<'a> = &'a dyn Fn(f64, u64, u64, f64, f64);

fn transfer(
    emit: MoveEmit,
    cancel: &AtomicBool,
    old_path: &Path,
    new_path: &Path,
) -> Result<Transfer, String> {
    let check_cancel = || {
        if cancel.load(Ordering::SeqCst) {
            Err(MOVE_CANCELLED.to_string())
        } else {
            Ok(())
        }
    };
    emit(0.0, 0, 0, 0.0, 0.0);
    check_cancel()?;

    if same_volume(old_path, new_path) {
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| fmt_io(&format!("Could not create {}", parent.display()), &e))?;
        }
        match std::fs::rename(old_path, new_path) {
            Ok(()) => {
                emit(100.0, 0, 0, 0.0, 0.0);
                return Ok(Transfer {
                    renamed: true,
                    files: 0,
                    bytes: 0,
                    skipped_links: Vec::new(),
                });
            }
            Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) => {
                log::info!(
                    "Move: {} is on another volume, copying the files instead.",
                    new_path.display()
                );
            }
            Err(e) if matches!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)) => {
                return Err(format!(
                    "{}. Close the game and any program using its folder, then try again.",
                    fmt_io(&format!("Could not move {}", old_path.display()), &e)
                ));
            }
            Err(e) => {
                return Err(fmt_io(
                    &format!("Could not move {} to {}", old_path.display(), new_path.display()),
                    &e,
                ));
            }
        }
    }

    let mut scan = Scan::default();
    scan_tree(old_path, old_path, &mut scan)?;
    let Scan { files, links, total: total_bytes } = scan;
    check_cancel()?;

    super::engine::ensure_disk_space(
        new_path,
        total_bytes,
        1.0,
        super::engine::HEADROOM_INSTALL,
    )?;
    let largest = files.iter().map(|(_, size)| *size).max().unwrap_or(0);
    if let Some(hint) = super::fs_util::fat_limit_message(new_path, largest) {
        return Err(hint);
    }

    let mut bytes_copied: u64 = 0;
    let mut last_emit: Option<Instant> = None;
    let mut rate = CopyRate::new(Instant::now());
    let mut report = |bytes_copied: u64| {
        let now = Instant::now();
        if last_emit.is_some_and(|at| now.saturating_duration_since(at) < MOVE_REPORT_INTERVAL) {
            return;
        }
        last_emit = Some(now);
        let pct = if total_bytes > 0 {
            (bytes_copied as f64 / total_bytes as f64) * 100.0
        } else {
            0.0
        };
        let speed = rate.sample(now, bytes_copied);
        emit(
            pct,
            bytes_copied,
            total_bytes,
            speed,
            eta_secs(speed, bytes_copied, total_bytes),
        );
    };
    for (rel, size) in &files {
        check_cancel()?;
        let source = old_path.join(rel);
        let dest = new_path.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| fmt_io(&format!("Could not create {}", parent.display()), &e))?;
        }

        if *size > STREAM_THRESHOLD {
            stream_copy(&source, &dest, &check_cancel, &mut |n| {
                bytes_copied += n;
                report(bytes_copied);
            })?;
        } else {
            let copied = std::fs::copy(&source, &dest).map_err(|e| {
                fmt_io(
                    &format!("Could not copy {} to {}", source.display(), dest.display()),
                    &e,
                )
            })?;
            sync_copied(&dest)
                .map_err(|e| fmt_io(&format!("Could not write {}", dest.display()), &e))?;
            bytes_copied += copied;
            report(bytes_copied);
        }
    }

    let mut skipped_links = Vec::new();
    for rel in &links {
        check_cancel()?;
        let source = old_path.join(rel);
        let dest = new_path.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| fmt_io(&format!("Could not create {}", parent.display()), &e))?;
        }
        let Err(e) = recreate_link(&source, &dest, old_path, new_path) else {
            continue;
        };
        let copied = std::fs::metadata(&source).is_ok_and(|m| m.is_file())
            && std::fs::copy(&source, &dest).and_then(|_| sync_copied(&dest)).is_ok();
        if !copied {
            log::warn!(
                "Move: {}",
                fmt_io(&format!("could not recreate the link {} in the new folder", rel.display()), &e)
            );
            skipped_links.push(rel.clone());
        }
    }

    emit(100.0, bytes_copied, total_bytes, 0.0, 0.0);
    Ok(Transfer {
        renamed: false,
        files: files.len(),
        bytes: bytes_copied,
        skipped_links,
    })
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink_dir()) {
        return std::fs::remove_dir(path);
    }
    std::fs::remove_file(path)
}

fn remove_with_retry(path: &Path, budget_ms: &mut u64) -> Result<(), String> {
    let mut retries = 0u32;
    let mut cleared_readonly = false;
    loop {
        let e = match remove_entry(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => e,
        };
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
            let readonly = std::fs::symlink_metadata(path)
                .ok()
                .filter(|m| !m.file_type().is_symlink() && m.permissions().readonly());
            let Some(meta) = readonly.filter(|_| !cleared_readonly) else {
                return Err(e.to_string());
            };
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(path, perms).map_err(|_| e.to_string())?;
            cleared_readonly = true;
            continue;
        }
        if retries >= REMOVE_RETRIES_PER_FILE || *budget_ms < REMOVE_RETRY_DELAY_MS {
            return Err(e.to_string());
        }
        retries += 1;
        *budget_ms -= REMOVE_RETRY_DELAY_MS;
        std::thread::sleep(std::time::Duration::from_millis(REMOVE_RETRY_DELAY_MS));
    }
}

fn remove_root(root: &Path) -> Result<(), String> {
    for i in 0..5 {
        match std::fs::remove_dir_all(root) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                if i == 4 {
                    return Err(e.to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    Ok(())
}

fn remove_old_copy(root: &Path) -> Vec<Value> {
    let files = list_files(root);
    let mut failures: Vec<Value> = Vec::new();
    let mut budget_ms = REMOVE_RETRY_BUDGET_MS;
    for (file, _) in &files {
        if let Err(e) = remove_with_retry(file, &mut budget_ms) {
            log::warn!("Move: could not remove the old copy {}: {e}", file.display());
            failures.push(json!({ "file": file.to_string_lossy(), "error": e }));
        }
    }
    if let Err(e) = remove_root(root) {
        log::warn!("Move: could not remove the old folder {}: {e}", root.display());
        failures.push(json!({ "file": root.to_string_lossy(), "error": e }));
    }
    failures
}

pub async fn run_move(
    app: &AppHandle,
    profile: &'static Value,
    old_path: PathBuf,
    new_path: PathBuf,
) -> Value {
    let profile_id = game_profiles::profile_id(profile).to_string();
    let fail = |message: String| -> Value {
        super::queue::publish(
            app,
            "move-progress",
            Phase::Error,
            json!({
                "percentage": 0,
                "processedBytes": 0,
                "totalBytes": 0,
                "speed": 0,
                "eta": 0,
                "gameId": profile_id,
                "status": "error",
                "error": message,
            }),
        );
        err_response(format!("Move failed: {message}"))
    };
    let cancel = MoveCancel::register(&profile_id);

    if let Some(message) = running_refusal(app, profile).await {
        return fail(message);
    }
    let configured = app
        .state::<BackendState>()
        .config
        .get(&format!("games.{profile_id}.gamePath"));
    if normalized(configured.as_str().unwrap_or("")) != normalized(&old_path.to_string_lossy()) {
        return fail(format!(
            "{} is no longer at {}. Nothing was moved.",
            game_profiles::display_name(profile),
            old_path.display()
        ));
    }
    if is_nested(&new_path.to_string_lossy(), &old_path.to_string_lossy()) {
        return fail(MOVE_NESTED.to_string());
    }
    if !old_path.is_dir() {
        return fail(format!("{} was not found.", old_path.display()));
    }
    if is_shared_folder(app, &profile_id, &old_path.to_string_lossy()) {
        return fail(format!(
            "{} is not used by this game alone, so Peebify will not move it. Locate the game's own folder first.",
            old_path.display()
        ));
    }
    if new_path.exists() {
        return fail(format!(
            "{} already exists. Pick another location.",
            new_path.display()
        ));
    }

    log::info!(
        "Moving {profile_id} from {} to {}...",
        old_path.display(),
        new_path.display()
    );
    let _power = super::platform::TransferGuard::acquire();
    let started = Instant::now();
    record_pending_move(app, &profile_id, &old_path, &new_path);

    let app_for_task = app.clone();
    let profile_id_task = profile_id.clone();
    let old_for_task = old_path.clone();
    let new_for_task = new_path.clone();
    let cancel_for_task = Arc::clone(&cancel.flag);
    let result = tauri::async_runtime::spawn_blocking(move || -> Result<Transfer, String> {
        let emit = |percentage: f64, bytes_copied: u64, total_bytes: u64, speed: f64, eta: f64| {
            super::queue::publish(
                &app_for_task,
                "move-progress",
                Phase::Downloading,
                json!({
                    "percentage": percentage,
                    "processedBytes": bytes_copied,
                    "totalBytes": total_bytes,
                    "speed": speed,
                    "eta": eta,
                    "gameId": profile_id_task,
                }),
            );
        };
        transfer(&emit, &cancel_for_task, &old_for_task, &new_for_task)
    })
    .await
    .map_err(|e| format!("move task panicked: {e}"))
    .and_then(|r| r);
    drop(cancel);

    let moved = match result {
        Ok(moved) => moved,
        Err(e) => {
            let cancelled = e == MOVE_CANCELLED;
            if cancelled {
                log::info!(
                    "Move of {profile_id} cancelled after {:.1} s; removing the partial copy at {}.",
                    started.elapsed().as_secs_f64(),
                    new_path.display()
                );
            } else {
                log::error!(
                    "Failed to move {profile_id} after {:.1} s: {e}",
                    started.elapsed().as_secs_f64()
                );
            }
            if old_path.is_dir() {
                let partial = new_path.clone();
                let removed = tauri::async_runtime::spawn_blocking(move || std::fs::remove_dir_all(&partial))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
                if let Err(rm_err) = removed {
                    if rm_err.kind() != std::io::ErrorKind::NotFound {
                        log::warn!(
                            "Move rollback: {}",
                            fmt_io(
                                &format!("could not remove partial copy at {}", new_path.display()),
                                &rm_err
                            )
                        );
                    }
                }
            } else {
                log::warn!(
                    "Move rollback skipped: {} is gone, so {} was kept.",
                    old_path.display(),
                    new_path.display()
                );
            }
            clear_pending_move(app, &profile_id);
            if cancelled {
                super::queue::publish(
                    app,
                    "move-progress",
                    Phase::Cancelled,
                    json!({
                        "percentage": 0,
                        "speed": 0,
                        "eta": 0,
                        "gameId": profile_id,
                        "status": "cancelled",
                    }),
                );
                return json!({ "success": false, "cancelled": true, "error": MOVE_CANCELLED });
            }
            return fail(e);
        }
    };

    super::config::set_config_value(
        app,
        &format!("games.{profile_id}.gamePath"),
        json!(new_path.to_string_lossy()),
    );
    super::file_channels::follow_moved_pending_install(
        app,
        &profile_id,
        &old_path.to_string_lossy(),
        &new_path.to_string_lossy(),
    );
    app.state::<BackendState>().config.flush();

    let failures = if moved.renamed {
        Vec::new()
    } else {
        super::queue::publish(
            app,
            "move-progress",
            Phase::Moving,
            json!({
                "status": REMOVING_OLD_COPY,
                "percentage": 100,
                "processedBytes": moved.bytes,
                "totalBytes": moved.bytes,
                "speed": 0,
                "eta": 0,
                "gameId": profile_id,
            }),
        );
        let old_for_cleanup = old_path.clone();
        tauri::async_runtime::spawn_blocking(move || remove_old_copy(&old_for_cleanup))
            .await
            .unwrap_or_else(|e| {
                vec![json!({ "file": old_path.to_string_lossy(), "error": e.to_string() })]
            })
    };
    clear_pending_move(app, &profile_id);

    if moved.renamed {
        log::info!(
            "Moved {profile_id} to {} by rename in {:.1} s.",
            new_path.display(),
            started.elapsed().as_secs_f64(),
        );
    } else {
        log::info!(
            "Moved {profile_id} to {} by copy in {:.1} s: {} files, {:.2} GB, {} left in the old folder.",
            new_path.display(),
            started.elapsed().as_secs_f64(),
            moved.files,
            peebify_transfer::progress::gib(moved.bytes as f64),
            failures.len()
        );
    }

    let mut warnings: Vec<String> = Vec::new();
    if !failures.is_empty() {
        warnings.push(format!(
            "Moved {} to the new folder. {} item{} in {} could not be removed. Delete them manually if needed.",
            game_profiles::display_name(profile),
            failures.len(),
            if failures.len() == 1 { "" } else { "s" },
            old_path.display()
        ));
        remember_leftovers(&profile_id, &old_path);
    } else if !moved.skipped_links.is_empty() {
        remember_leftovers(&profile_id, &new_path);
    }
    if !moved.skipped_links.is_empty() {
        let names: Vec<String> = moved
            .skipped_links
            .iter()
            .take(3)
            .map(|rel| rel.display().to_string())
            .collect();
        warnings.push(format!(
            "Peebify could not recreate {} linked item{} in the new folder ({}{}). Recreate {} if you still need {}.",
            moved.skipped_links.len(),
            if moved.skipped_links.len() == 1 { "" } else { "s" },
            names.join(", "),
            if moved.skipped_links.len() > names.len() { ", …" } else { "" },
            if moved.skipped_links.len() == 1 { "it" } else { "them" },
            if moved.skipped_links.len() == 1 { "it" } else { "them" },
        ));
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join(" "));
    super::queue::publish(
        app,
        "move-progress",
        Update::new(Phase::Done).warning(warning.is_some()),
        json!({
            "percentage": 100,
            "processedBytes": moved.bytes,
            "totalBytes": moved.bytes,
            "gameId": profile_id,
            "status": if warning.is_some() { "completed-with-warnings" } else { "completed" },
            "message": warning,
        }),
    );

    let Some(warning) = warning else {
        return ok_with(json!({ "newPath": new_path.to_string_lossy() }));
    };
    log::warn!("Move ({profile_id}): {warning}");
    ok_with(json!({
        "newPath": new_path.to_string_lossy(),
        "warning": warning,
    }))
}

// ------------ Uninstall Game ------------
// Deletes the game's files, forgets its saved install path and removes its mods, and reports anything that could not be deleted.
fn publish_uninstall_progress(app: &AppHandle, game_id: &str, update: impl Into<Update>, payload: Value) {
    let mut merged = json!({ "gameId": game_id });
    if let (Some(map), Value::Object(extra)) = (merged.as_object_mut(), payload) {
        map.extend(extra);
    }
    super::queue::publish(app, "uninstall-progress", update, merged);
}

fn clear_install_record(app: &AppHandle, profile_id: &str) {
    super::config::set_config_value(app, &format!("games.{profile_id}.gamePath"), json!(""));
    super::file_channels::clear_pending_install(app, profile_id);
}

pub async fn uninstall_game(app: &AppHandle, target_game_id: Option<&str>) -> Value {
    let profile = match named_known_profile(target_game_id) {
        Ok(profile) => profile,
        Err(refusal) => return refusal,
    };
    let state = app.state::<BackendState>();
    let profile_id = game_profiles::profile_id(profile).to_string();
    let game_path = state.config.get(&format!("games.{profile_id}.gamePath"));
    let game_path = game_path.as_str().unwrap_or("").to_string();

    let send_progress = |update: Update, payload: Value| {
        publish_uninstall_progress(app, &profile_id, update, payload)
    };

    if game_path.is_empty() {
        let pending_path = state
            .config
            .get(&format!("games.{profile_id}.pendingInstall.path"))
            .as_str()
            .unwrap_or("")
            .to_string();
        if !pending_path.is_empty() {
            let Some(_mark) = mark_uninstalling(&profile_id) else {
                return err_response(format!(
                    "{} is already being uninstalled.",
                    game_profiles::display_name(profile)
                ));
            };
            if state.engine.queue.has_job_for(&profile_id) {
                return err_response(format!(
                    "{} has a download in progress. Cancel it on the Downloads page, then discard it.",
                    game_profiles::display_name(profile)
                ));
            }
            log::info!("Uninstall ({profile_id}): discarding the unfinished install at {pending_path}.");
            let (files, bytes) =
                super::file_channels::discard_partial_install(Path::new(&pending_path), &profile_id).await;
            log::info!(
                "Uninstall ({profile_id}): discarded {files} unfinished download file(s), {:.2} GB.",
                bytes as f64 / 1e9
            );
            super::file_channels::clear_pending_install(app, &profile_id);
        }
        send_progress(
            Phase::Done.into(),
            json!({ "percentage": 100, "status": "completed" }),
        );
        return ok_response();
    }
    if !Path::new(&game_path).exists() {
        let name = game_profiles::display_name(profile);
        let offline_root = super::fs_util::drive_root(&game_path)
            .filter(|root| !Path::new(root).is_dir());
        clear_install_record(app, &profile_id);
        send_progress(
            Phase::Downloading.into(),
            json!({ "percentage": 0 }),
        );
        if let Some(root) = offline_root {
            let message = format!(
                "The drive that holds {name} ({root}) isn't connected, so its files weren't deleted. {name} was removed from your library; delete {game_path} yourself after reconnecting the drive."
            );
            log::warn!("Uninstall ({profile_id}): {root} is not connected; cleared the record only ({game_path}).");
            remember_leftovers(&profile_id, Path::new(&game_path));
            send_progress(
                Update::new(Phase::Done).warning(true),
                json!({
                    "percentage": 100,
                    "status": "completed-with-warnings",
                    "message": message,
                }),
            );
            return ok_with(json!({ "warning": message, "path": game_path }));
        }
        log::warn!(
            "Uninstall: {profile_id} path missing on disk; clearing launcher record ({game_path})."
        );
        super::mods::remove_game_mods(app, profile).await;
        send_progress(
            Phase::Done.into(),
            json!({
                "percentage": 100,
                "status": "completed",
                "message": format!("{name}'s folder was already gone, so it was only removed from your library."),
            }),
        );
        return ok_response();
    }

    if let Some(app_id) = super::file_channels::steam_copy_at(&game_path, profile).await {
        let name = game_profiles::display_name(profile);
        let Some(steam_exe) = super::steam::steam_exe() else {
            let message = format!(
                "{name} is installed through Steam, and Steam could not be found on this PC. Uninstall it from your Steam library. Nothing was deleted."
            );
            log::warn!("Uninstall ({profile_id}): Steam copy (app {app_id}) but no steam.exe; deleted nothing.");
            send_progress(
                Phase::Error.into(),
                json!({
                    "percentage": 100,
                    "status": "error",
                    "error": message,
                }),
            );
            return err_response(message);
        };
        if let Err(e) = super::steam::request_uninstall(&steam_exe, &app_id).await {
            log::error!("Uninstall ({profile_id}): could not hand the uninstall to Steam: {e}");
            let message = format!("Couldn't start Steam: {e}");
            send_progress(
                Phase::Error.into(),
                json!({
                    "percentage": 100,
                    "status": "error",
                    "error": message,
                }),
            );
            return err_response(message);
        }
        log::info!("Uninstall ({profile_id}): handed the Steam copy (app {app_id}) to Steam and removed it from the library.");
        clear_install_record(app, &profile_id);
        super::mods::remove_game_mods(app, profile).await;
        send_progress(
            Phase::Done.into(),
            json!({ "percentage": 100, "status": "completed" }),
        );
        return ok_with(json!({ "steamManaged": true, "appId": app_id }));
    }

    let Some(_mark) = mark_uninstalling(&profile_id) else {
        return err_response(format!(
            "{} is already being uninstalled.",
            game_profiles::display_name(profile)
        ));
    };
    if state.engine.queue.has_job_for(&profile_id) {
        return err_response(format!(
            "{} has a download, repair or move in progress. Cancel it on the Downloads page, then uninstall.",
            game_profiles::display_name(profile)
        ));
    }
    if let Some(message) = running_refusal(app, profile).await {
        return err_response(message);
    }

    let real_path = resolved(Path::new(&game_path));
    if is_shared_folder(app, &profile_id, &game_path) {
        let message = format!(
            "Peebify removed {} from the launcher but did not delete {game_path}, because that folder is not used by this game alone. Delete the game files there yourself if needed.",
            game_profiles::display_name(profile)
        );
        log::warn!("Uninstall ({profile_id}): refused to delete {game_path} ({real_path}).");
        clear_install_record(app, &profile_id);
        super::mods::remove_game_mods(app, profile).await;
        remember_leftovers(&profile_id, Path::new(&game_path));
        send_progress(
            Phase::Downloading.into(),
            json!({ "percentage": 0 }),
        );
        send_progress(
            Update::new(Phase::Done).warning(true),
            json!({
                "percentage": 100,
                "status": "completed-with-warnings",
                "message": message,
            }),
        );
        return ok_with(json!({ "warning": message, "path": game_path }));
    }

    let probe = PathBuf::from(&game_path);
    let protected_by_windows =
        tauri::async_runtime::spawn_blocking(move || super::fs_util::dir_denies_writes(&probe))
            .await
            .unwrap_or(false);
    if protected_by_windows {
        let name = game_profiles::display_name(profile);
        log::warn!("Uninstall ({profile_id}): {game_path} refuses writes, so nothing was deleted.");
        return err_response(format!(
            "Windows protects {game_path}, so Peebify can't delete it. Uninstall {name} with its official launcher."
        ));
    }

    log::info!("Uninstalling {profile_id} from {game_path}...");
    let started = Instant::now();
    let app_for_task = app.clone();
    let profile_id_task = profile_id.clone();
    let game_path_buf = PathBuf::from(&game_path);

    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let app = app_for_task;
        let profile_id = profile_id_task;
        let send = |payload: Value| {
            publish_uninstall_progress(&app, &profile_id, Phase::Downloading, payload)
        };

        send(json!({ "percentage": 0 }));
        let files = list_files(&game_path_buf);
        let total_files = files.len();

        let mut failures: Vec<Value> = Vec::new();
        let mut deleted: usize = 0;
        let mut removed_bytes: u64 = 0;
        let mut last_emit: i64 = 0;
        let mut budget_ms = REMOVE_RETRY_BUDGET_MS;

        for (file, size) in &files {
            match remove_with_retry(file, &mut budget_ms) {
                Ok(()) => {
                    deleted += 1;
                    removed_bytes += size;
                }
                Err(e) => {
                    log::warn!("Uninstall: failed to remove {}: {e}", file.display());
                    failures.push(json!({ "file": file.to_string_lossy(), "error": e }));
                }
            }
            let now = chrono::Utc::now().timestamp_millis();
            if now - last_emit >= 100 || deleted == total_files {
                last_emit = now;
                send(json!({
                    "percentage": if total_files > 0 { (deleted as f64 / total_files as f64) * 100.0 } else { 100.0 },
                }));
            }
        }

        if let Err(e) = remove_root(&game_path_buf) {
            log::warn!(
                "Uninstall: root removal failed for {}: {e}",
                game_path_buf.display()
            );
            failures.push(json!({ "file": game_path_buf.to_string_lossy(), "error": e }));
        }

        (deleted, total_files, removed_bytes, failures)
    })
    .await;

    let (deleted, total_files, removed_bytes, failures) = match outcome {
        Ok(v) => v,
        Err(e) => {
            log::error!(
                "Uninstall ({profile_id}) failed after {:.1} s: {e}",
                started.elapsed().as_secs_f64()
            );
            super::config::set_config_value(app, &format!("games.{profile_id}.gamePath"), json!(""));
            send_progress(
                Phase::Error.into(),
                json!({ "percentage": 100, "status": "error", "error": e.to_string() }),
            );
            return err_response(format!("Uninstall failed: {e}"));
        }
    };

    clear_install_record(app, &profile_id);
    super::mods::remove_game_mods(app, profile).await;

    log::info!(
        "Uninstall {profile_id}: removed {deleted} of {total_files} files ({:.2} GB) in {:.1} s, {} failures.",
        peebify_transfer::progress::gib(removed_bytes as f64),
        started.elapsed().as_secs_f64(),
        failures.len()
    );

    let summary = (!failures.is_empty()).then(|| {
        format!(
            "Removed {deleted} of {total_files} files. {} item{} could not be removed (likely held by another program or antivirus). Delete the leftovers manually if needed.",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        )
    });
    if summary.is_some() {
        remember_leftovers(&profile_id, Path::new(&game_path));
    }
    send_progress(
        Update::new(Phase::Done).warning(summary.is_some()),
        json!({
            "percentage": 100,
            "status": if summary.is_some() { "completed-with-warnings" } else { "completed" },
            "message": summary,
        }),
    );

    if let Some(summary) = summary {
        log::warn!("Uninstall ({profile_id}): {summary}");
        return ok_with(json!({ "warning": summary, "path": game_path }));
    }

    log::info!("Game ({profile_id}) uninstalled successfully ({deleted} files).");
    ok_response()
}

// ------------ Move And Uninstall Tests ------------
// Covers the move and uninstall rules.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftovers_open_the_nearest_folder_that_still_exists() {
        let root = std::env::temp_dir().join(format!("peebify-leftovers-{}", std::process::id()));
        let kept = root.join("Wuthering Waves");
        std::fs::create_dir_all(&kept).unwrap();
        assert_eq!(nearest_existing_dir(&kept), Some(kept.as_path()));
        assert_eq!(
            nearest_existing_dir(&kept.join("Client").join("Binaries")),
            Some(kept.as_path())
        );
        std::fs::remove_dir_all(&kept).unwrap();
        assert_eq!(nearest_existing_dir(&kept), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_folder_name_keeps_the_existing_default_folders() {
        assert_eq!(
            game_profiles::install_folder_name(game_profiles::profile("hsr")),
            "Honkai Star Rail"
        );
        assert_eq!(
            game_profiles::install_folder_name(game_profiles::profile("gf2")),
            "Girls' Frontline 2 Exilium"
        );
        assert_eq!(
            game_profiles::install_folder_name(game_profiles::profile("wuwa")),
            "Wuthering Waves"
        );
    }

    #[test]
    fn install_folder_name_strips_trailing_dots_and_falls_back_to_the_id() {
        let dotted = json!({ "id": "x", "displayName": "Game: Name. . " });
        assert_eq!(game_profiles::install_folder_name(&dotted), "Game Name");
        let unnamed = json!({ "id": "nameless", "displayName": "" });
        assert_eq!(game_profiles::install_folder_name(&unnamed), "nameless");
    }

    #[test]
    fn destructive_operations_refuse_a_missing_game_id() {
        assert!(named_known_profile(None).is_err());
        assert!(named_known_profile(Some("")).is_err());
    }

    #[test]
    fn destructive_operations_refuse_an_unknown_game_id() {
        let refusal = named_known_profile(Some("not-a-game")).unwrap_err();
        assert_eq!(refusal["success"], json!(false));
    }

    #[test]
    fn destructive_operations_resolve_the_named_game_not_the_default() {
        let profile = named_known_profile(Some("zzz")).unwrap();
        assert_eq!(game_profiles::profile_id(profile), "zzz");
    }

    fn protected() -> Protected {
        Protected {
            exact: vec![
                r"C:\Program Files".to_string(),
                r"C:\Users\me\Desktop".to_string(),
                r"D:\Peebify\games".to_string(),
            ],
            tree: vec![r"C:\Windows".to_string()],
            keep: vec![
                r"C:\Users\me".to_string(),
                r"D:\Peebify".to_string(),
                r"E:\Games\Other".to_string(),
            ],
        }
    }

    #[test]
    fn nested_same_folder() {
        assert!(is_nested(r"D:\Games\WW", r"D:\Games\WW"));
    }

    #[test]
    fn nested_child_folder() {
        assert!(is_nested(r"D:\Games\WW\Wuthering Waves", r"D:\Games\WW"));
        assert!(is_nested(r"D:\Games\WW", r"D:\Games\WW\Client"));
    }

    #[test]
    fn nested_ignores_case_slashes_and_trailing_separator() {
        assert!(is_nested(r"d:\games\ww\sub", r"D:\Games\WW\"));
        assert!(is_nested("D:/Games/WW/sub", r"D:\Games\WW"));
    }

    #[test]
    fn sibling_with_shared_prefix_is_not_nested() {
        assert!(!is_nested(r"D:\Games\WW2", r"D:\Games\WW"));
        assert!(!is_nested(r"D:\Games\WW", r"D:\Games\WW2\Wuthering Waves"));
    }

    #[test]
    fn drive_root_nests_everything_on_it() {
        assert!(is_nested(r"D:\Games\WW", r"D:\"));
        assert!(!is_nested(r"E:\Games\WW", r"D:\"));
    }

    #[test]
    fn uninstall_refuses_drive_and_share_roots() {
        let p = protected();
        assert!(p.refuses(r"E:\"));
        assert!(p.refuses("E:"));
        assert!(p.refuses(r"\\server\share"));
        assert!(p.refuses(""));
    }

    #[test]
    fn uninstall_refuses_known_and_system_folders() {
        let p = protected();
        assert!(p.refuses(r"C:\Program Files"));
        assert!(p.refuses(r"c:\users\me\desktop\"));
        assert!(p.refuses(r"C:\Windows\System32"));
        assert!(p.refuses(r"D:\Peebify\games"));
        assert!(p.refuses(r"F:\SteamLibrary\steamapps\common"));
        assert!(p.refuses(r"F:\SteamLibrary\steamapps"));
    }

    #[test]
    fn uninstall_refuses_ancestors_of_kept_folders() {
        let p = protected();
        assert!(p.refuses(r"C:\Users"));
        assert!(p.refuses(r"C:\Users\me"));
        assert!(p.refuses(r"D:\Peebify"));
        assert!(p.refuses(r"E:\Games"));
    }

    #[test]
    fn uninstall_allows_real_game_folders() {
        let p = protected();
        assert!(!p.refuses(r"C:\Program Files\HoYoPlay\games\Genshin Impact game"));
        assert!(!p.refuses(r"D:\Peebify\games\Wuthering Waves"));
        assert!(!p.refuses(r"C:\Users\me\Peebify Games\Zenless Zone Zero"));
        assert!(!p.refuses(r"F:\SteamLibrary\steamapps\common\Wuthering Waves"));
        assert!(!p.refuses(r"E:\Games\Other2"));
    }

    #[test]
    fn uninstall_mark_is_exclusive_and_released_on_drop() {
        let first = mark_uninstalling("test-mark-game");
        assert!(first.is_some());
        assert!(is_uninstalling("test-mark-game"));
        assert!(mark_uninstalling("test-mark-game").is_none());
        drop(first);
        assert!(!is_uninstalling("test-mark-game"));
    }

    #[test]
    fn recover_move_deletes_only_a_partial_destination() {
        let root = std::env::temp_dir().join(format!("peebify-move-test-{}", std::process::id()));
        let from = root.join("old");
        let to = root.join("new");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("partial.bin"), b"x").unwrap();
        let from_text = from.to_string_lossy().into_owned();

        assert_eq!(recover_move("wuwa", &from_text, &from, &to, &[]), Recovery::Keep);
        assert!(!to.exists());
        assert!(from.exists());

        std::fs::create_dir_all(&to).unwrap();
        std::fs::remove_dir_all(&from).unwrap();
        assert_eq!(
            recover_move("wuwa", &from_text, &from, &to, &[]),
            Recovery::Adopt(to.to_string_lossy().into_owned())
        );
        assert!(to.exists());

        let to_text = to.to_string_lossy().into_owned();
        assert_eq!(recover_move("wuwa", &to_text, &from, &to, &[]), Recovery::Keep);
        assert!(to.exists());

        std::fs::create_dir_all(&from).unwrap();
        assert_eq!(
            recover_move("wuwa", &from_text, &from, &to, std::slice::from_ref(&to_text)),
            Recovery::Keep
        );
        assert!(to.exists());

        assert_eq!(
            recover_move("wuwa", &to_text, &from, &to, &[]),
            Recovery::OldCopy(from.clone())
        );
        assert!(from.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_old_copy_is_only_deleted_when_it_is_the_games_own_folder() {
        let root = std::env::temp_dir().join(format!("peebify-old-copy-{}", uuid::Uuid::new_v4()));
        let from = root.join("old");
        let to = root.join("new");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let to_text = to.to_string_lossy().into_owned();
        let missing = root.join("gone").to_string_lossy().into_owned();
        let other = from.join("Other Game").to_string_lossy().into_owned();

        let own = old_copy_deletable(&to_text, &from, &[]);
        let new_copy_missing = old_copy_deletable(&missing, &from, &[]);
        let holds_other_game = old_copy_deletable(&to_text, &from, &[other]);
        let holds_new_copy = old_copy_deletable(&to_text, &root, &[]);
        let _ = std::fs::remove_dir_all(&root);

        assert!(own);
        assert!(!new_copy_missing);
        assert!(!holds_other_game);
        assert!(!holds_new_copy);
    }

    #[test]
    fn the_launcher_folder_is_refused_except_its_games_folder() {
        let launcher = r"D:\Peebify";
        assert!(in_launcher_payload(r"D:\Peebify\Wuthering Waves", launcher));
        assert!(in_launcher_payload(r"d:\peebify\resources\Wuthering Waves", launcher));
        assert!(in_launcher_payload(r"D:\Peebify", launcher));
        assert!(!in_launcher_payload(r"D:\Peebify\games\Wuthering Waves", launcher));
        assert!(!in_launcher_payload(r"D:\Peebify Games\Wuthering Waves", launcher));
        assert!(!in_launcher_payload(r"E:\Wuthering Waves", launcher));
        assert!(!in_launcher_payload(r"E:\Wuthering Waves", ""));
    }

    #[test]
    fn links_into_the_old_folder_follow_the_move() {
        let old_root = Path::new(r"D:\Games\WW");
        let new_root = Path::new(r"E:\WW");
        assert_eq!(
            link_target_in(new_root, old_root, Path::new(r"D:\Games\WW\Client\Mods")),
            PathBuf::from(r"E:\WW\Client\Mods")
        );
        assert_eq!(
            link_target_in(new_root, old_root, Path::new(r"\\?\d:\games\ww\Shots")),
            PathBuf::from(r"E:\WW\Shots")
        );
        assert_eq!(
            link_target_in(new_root, old_root, Path::new(r"F:\Mods")),
            PathBuf::from(r"F:\Mods")
        );
        assert_eq!(
            link_target_in(new_root, old_root, Path::new(r"D:\Games\WW2\Mods")),
            PathBuf::from(r"D:\Games\WW2\Mods")
        );
    }

    #[test]
    fn deletes_clear_read_only_and_spend_no_budget_on_success() {
        let dir = std::env::temp_dir().join(format!("peebify-remove-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let locked = dir.join("readonly.bin");
        std::fs::write(&locked, b"x").unwrap();
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&locked, perms).unwrap();

        let mut budget = REMOVE_RETRY_BUDGET_MS;
        let removed = remove_with_retry(&locked, &mut budget);
        let gone = !locked.exists();
        let missing = remove_with_retry(&dir.join("absent.bin"), &mut budget);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(removed.is_ok());
        assert!(gone);
        assert!(missing.is_ok());
        assert_eq!(budget, REMOVE_RETRY_BUDGET_MS);
    }

    #[test]
    fn a_read_only_copy_is_flushed_and_stays_read_only() {
        let dir = std::env::temp_dir().join(format!("peebify-sync-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.ini");
        std::fs::write(&file, b"x").unwrap();
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();

        let synced = sync_copied(&file);
        let still_readonly = std::fs::metadata(&file).unwrap().permissions().readonly();
        let mut writable = std::fs::metadata(&file).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        let _ = std::fs::set_permissions(&file, writable);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(synced.is_ok());
        assert!(still_readonly);
    }

    #[test]
    fn a_cancelled_move_stops_before_it_changes_anything() {
        let root = std::env::temp_dir().join(format!("peebify-move-cancel-{}", uuid::Uuid::new_v4()));
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("game.exe"), b"x").unwrap();
        let cancel = AtomicBool::new(true);
        let result = transfer(&|_, _, _, _, _| {}, &cancel, &old, &new);
        let kept = old.join("game.exe").is_file();
        let created = new.exists();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(result.err().as_deref(), Some(MOVE_CANCELLED));
        assert!(kept && !created);
    }

    #[test]
    fn a_same_drive_move_is_a_rename_without_a_walk() {
        let root = std::env::temp_dir().join(format!("peebify-move-rename-{}", uuid::Uuid::new_v4()));
        let old = root.join("old");
        let new = root.join("nested").join("new");
        std::fs::create_dir_all(old.join("Data")).unwrap();
        std::fs::write(old.join("Data").join("a.pak"), b"abc").unwrap();
        let cancel = AtomicBool::new(false);
        let result = transfer(&|_, _, _, _, _| {}, &cancel, &old, &new);
        let moved = new.join("Data").join("a.pak").is_file();
        let old_gone = !old.exists();
        let _ = std::fs::remove_dir_all(&root);
        let result = result.unwrap();
        assert!(result.renamed);
        assert_eq!((result.files, result.bytes), (0, 0));
        assert!(moved && old_gone);
    }

    #[test]
    fn a_streamed_copy_keeps_the_modified_time() {
        let root = std::env::temp_dir().join(format!("peebify-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("big.pak");
        let dest = root.join("copy.pak");
        std::fs::write(&source, vec![7u8; 5 * 1024 * 1024 + 3]).unwrap();
        let stamp = std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(stamp)
            .unwrap();

        let mut copied = 0u64;
        let result = stream_copy(&source, &dest, &|| Ok(()), &mut |n| copied += n);
        let source_mtime = std::fs::metadata(&source).and_then(|m| m.modified()).unwrap();
        let dest_meta = std::fs::metadata(&dest).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert!(result.is_ok());
        assert_eq!(copied, 5 * 1024 * 1024 + 3);
        assert_eq!(dest_meta.len(), copied);
        assert_eq!(dest_meta.modified().unwrap(), source_mtime);
    }

    #[test]
    fn only_a_move_still_copying_can_be_cancelled() {
        let id = "test-move-cancel-game";
        assert!(!cancel_running_move(id));
        let running = MoveCancel::register(id);
        assert!(cancel_running_move(id));
        assert!(running.flag.load(Ordering::SeqCst));
        drop(running);
        assert!(!cancel_running_move(id), "the copy finished, so nothing is left to stop");
    }

    #[test]
    fn move_speed_is_smoothed_over_a_window_and_gives_the_time_left() {
        let t0 = Instant::now();
        let mut rate = CopyRate::new(t0);
        assert_eq!(rate.sample(t0 + Duration::from_millis(100), 50), 0.0);
        let first = rate.sample(t0 + Duration::from_secs(1), 100_000_000);
        assert!((first - 100_000_000.0).abs() < 1.0);
        let second = rate.sample(t0 + Duration::from_secs(2), 100_000_000);
        assert!((second - 60_000_000.0).abs() < 1.0, "a stall only pulls it down part way");
        assert_eq!(eta_secs(50.0, 100, 600), 10.0);
        assert_eq!(eta_secs(0.0, 100, 600), 0.0);
        assert_eq!(eta_secs(50.0, 700, 600), 0.0);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_scanned_as_a_link_and_deleted_without_its_target() {
        let root = std::env::temp_dir().join(format!("peebify-junction-{}", uuid::Uuid::new_v4()));
        let game = root.join("game");
        let outside = root.join("outside");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(game.join("a.pak"), b"abc").unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let junction = game.join("Mods");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let mut scan = Scan::default();
        let scanned = scan_tree(&game, &game, &mut scan);
        let mut budget = REMOVE_RETRY_BUDGET_MS;
        let removed = remove_with_retry(&junction, &mut budget);
        let target_kept = outside.join("keep.txt").exists();
        let link_gone = std::fs::symlink_metadata(&junction).is_err();
        let _ = std::fs::remove_dir_all(&root);

        assert!(scanned.is_ok());
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.links, vec![PathBuf::from("Mods")]);
        assert_eq!(scan.total, 3);
        assert!(removed.is_ok());
        assert!(target_kept);
        assert!(link_gone);
    }
}
