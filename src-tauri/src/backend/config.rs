// ------------ Launcher Config ------------
// Owns launcher-config.json in the app data folder: every setting, each game's path and playtime, and the library.
// It fills in defaults, repairs bad or old values, saves with a small delay, and keeps backups so a damaged file can be restored.
// It also holds the commands the window uses to read and change settings.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::{json, Map, Value};
use tauri::AppHandle;

use super::fs_util::dialog::{open_folder, open_url, pick_file, pick_folder, Pick};
use super::game_profiles::{self, GAME_IDS};
use super::{
    active_game_id, arg_str, backend, err_response, fs_util, game_file_ops,
    ok_response, ok_with, platform, wrap_result,
};

const CONFIG_FILE: &str = "launcher-config.json";
const SAVE_DEBOUNCE_MS: u64 = 100;
const EMPTY_OK: [&str; 1] = ["overlayAudioTracks"];
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_MS: u64 = 200;
const SAVE_ATTEMPTS: u32 = 3;
const SAVE_RETRY_MS: u64 = 100;

// ------------ Default Settings ------------
// What a fresh config looks like, for the launcher as a whole and for each game.
fn default_playtime_full() -> Value {
    json!({
        "totalPlaytime": 0,
        "dailyPlaytime": {},
        "sessionCount": 0,
        "mostRecentSession": null,
        "sessions": []
    })
}

fn default_playtime_slice() -> Value {
    let mut slice = default_playtime_full();
    slice.as_object_mut().expect("the playtime default is an object").remove("sessions");
    slice
}

fn default_media_slice() -> Value {
    json!({ "type": "default", "path": null })
}

fn per_game_media() -> Value {
    Value::Object(
        GAME_IDS
            .iter()
            .map(|id| (id.to_string(), default_media_slice()))
            .collect(),
    )
}

fn graphics_api_default(profile: &Value) -> Option<&str> {
    profile.get("graphicsApiArgs")?;
    Some(
        profile
            .get("graphicsApiDefault")
            .and_then(Value::as_str)
            .unwrap_or("dx11"),
    )
}

fn default_game(id: &str) -> Value {
    let profile = game_profiles::profile(id);
    let mut game = Map::new();
    game.insert("gamePath".into(), json!(""));
    if let Some(api) = graphics_api_default(profile) {
        game.insert("graphicsApi".into(), json!(api));
    }
    if let Some(quality) = game_profiles::resource_quality_default(profile) {
        game.insert("resourceQuality".into(), json!(quality));
    }
    if profile.get("steamAppId").is_some() {
        game.insert("launchViaSteam".into(), json!(true));
    }
    if game_profiles::mod_config(profile).is_some() {
        game.insert("modsEnabled".into(), json!(false));
    }
    for (key, value) in super::mod_game_settings::defaults(profile) {
        game.insert(key.into(), value);
    }
    if profile.get("fpsUnlock") == Some(&Value::Bool(true)) {
        game.insert("fpsUnlock".into(), json!(false));
        game.insert("fpsUnlockTarget".into(), json!(120));
        game.insert("fpsUnlockPowerSave".into(), json!(false));
        game.insert("fpsUnlockBackgroundFps".into(), json!(30));
    }
    game.insert("playtime".into(), default_playtime_full());
    Value::Object(game)
}

fn default_config() -> Value {
    let games: Map<String, Value> = GAME_IDS
        .iter()
        .map(|id| (id.to_string(), default_game(id)))
        .collect();
    json!({
        "games": games,
        "window": {
            "width": 1280,
            "height": 720,
            "maximized": false
        },
        "behavior": default_behavior(),
        "wallpaper": per_game_media(),
        "gameIcons": per_game_media(),
        "library": {
            "visible": [],
            "setupComplete": false
        }
    })
}

fn default_behavior() -> Value {
    json!({
        "closeAction": "close",
        "minimizeAction": "minimize",
        "startOnBoot": false,
        "startOnBootAction": "open",
        "launchAction": "minimize",
        "reopenAfterGameClose": true,
        "rememberWindowState": true,
        "hideSocials": false,
        "hideBottomRightButtons": false,
        "hideSidebarPlaytime": false,
        "hideSidebarGallery": false,
        "hideSidebarDownloads": false,
        "hideSidebarMods": false,
        "hideSidebarVersion": false,
        "hidePlaytime": false,
        "hideNewsPanel": false,
        "disableAnimations": false,
        "glassEffects": "full",
        "osNotifications": true,
        "uiScale": "100",
        "activeGameId": "wuwa",

        "downloadSpeedLimit": "0",
        "downloadConnections": "0",
        "downloadWhileGaming": "off",

        "modsEnabled": false,
        "modsPath": "",
        "modsPerPage": "16",
        "showNsfwMods": false,

        "overlayEnabled": false,
        "overlayHotkey": "Alt+P",
        "overlayShotHotkey": "Alt+S",
        "overlayRecHotkey": "Alt+R",
        "overlayCaptureFolder": "",
        "overlayShotFormat": "png",
        "overlayShotQuality": "90",
        "overlayShotToast": true,
        "overlayRecRes": "native",
        "overlayRecFps": "60",
        "overlayRecCodec": "h264",
        "overlayRecQuality": "balanced",
        "overlayAudioTracks": "desktop,mic",
        "overlayAudioDiscord": "",

        "peebifyContent": true
    })
}

// ------------ Repairing Settings ------------
// Replaces anything missing or the wrong type with its default.
fn heal_library(merged: &mut Value) {
    if !merged["library"].is_object() {
        merged["library"] = json!({});
    }
    let mut seen = std::collections::HashSet::new();
    let known: Vec<String> = merged["library"]["visible"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| GAME_IDS.contains(id) && seen.insert(*id))
        .map(str::to_string)
        .collect();
    merged["library"]["visible"] = json!(known);
    if !merged["library"]["setupComplete"].is_boolean() {
        merged["library"]["setupComplete"] = json!(false);
    }
}

fn prune_corrupt_backups(user_data: &Path, keep: Option<&Path>) {
    let prefix = format!("{CONFIG_FILE}.corrupt-");
    let keep = keep.and_then(Path::file_name);
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return;
    };
    for entry in entries.flatten() {
        if keep == Some(entry.file_name().as_os_str()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => log::info!("[config] Removed stale corrupt-config backup {name}"),
            Err(e) => log::warn!("[config] Could not remove {name}: {e}"),
        }
    }
}

fn deep_merge(target: &mut Value, source: &Value) {
    let Some(source_map) = source.as_object() else {
        return;
    };
    let Some(target_map) = target.as_object_mut() else {
        return;
    };
    for (key, sv) in source_map {
        match target_map.get_mut(key) {
            Some(tv) if tv.is_object() && sv.is_object() => deep_merge(tv, sv),
            _ => {
                target_map.insert(key.clone(), sv.clone());
            }
        }
    }
}

pub fn sanitize_behavior(merged: &mut Value) {
    let defaults = default_behavior();
    let defaults_map = defaults
        .as_object()
        .expect("behavior defaults are an object");

    if !merged["behavior"].is_object() {
        merged["behavior"] = defaults.clone();
        return;
    }
    let behavior = merged["behavior"].as_object_mut().expect("checked above");

    for (key, default_value) in defaults_map {
        let value = behavior.get(key);
        let heal = match default_value {
            Value::Bool(_) => !matches!(value, Some(Value::Bool(_))),
            Value::String(_) => !matches!(
                value,
                Some(Value::String(s))
                    if (!s.is_empty() || EMPTY_OK.contains(&key.as_str()))
                        && s != "null"
                        && s != "undefined"
            ),
            Value::Number(_) => !matches!(value, Some(Value::Number(_))),
            _ => matches!(value, None | Some(Value::Null)),
        };
        if heal {
            behavior.insert(key.clone(), default_value.clone());
        }
    }

    heal_download_settings(behavior);

    if behavior
        .get("activeGameId")
        .and_then(Value::as_str)
        .is_some_and(|id| !game_profiles::is_known_game_id(id))
    {
        behavior.insert(
            "activeGameId".to_string(),
            json!(game_profiles::DEFAULT_GAME_ID),
        );
    }
}

const MAX_SPEED_LIMIT_MB: u64 = 10_000;

fn heal_download_settings(behavior: &mut serde_json::Map<String, Value>) {
    let mut clamp = |key: &str, max: u64| {
        let value = behavior
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(0, |n| n.min(max));
        behavior.insert(key.to_string(), json!(value.to_string()));
    };
    clamp("downloadSpeedLimit", MAX_SPEED_LIMIT_MB);
    clamp("downloadConnections", peebify_transfer::runtime::MAX_CONNECTIONS as u64);
    if !matches!(
        behavior.get("downloadWhileGaming").and_then(Value::as_str),
        Some("off" | "slow" | "pause")
    ) {
        behavior.insert("downloadWhileGaming".to_string(), json!("off"));
    }
}

pub fn transfer_settings(behavior: &Value) -> peebify_transfer::Settings {
    let number = |key: &str| {
        behavior
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    peebify_transfer::Settings {
        speed_limit: number("downloadSpeedLimit").saturating_mul(1_000_000),
        connections: number("downloadConnections") as usize,
        while_gaming: peebify_transfer::WhileGaming::parse(
            behavior
                .get("downloadWhileGaming")
                .and_then(Value::as_str)
                .unwrap_or("off"),
        ),
    }
}

fn heal_games(merged: &mut Value) {
    if !merged["games"].is_object() {
        merged["games"] = json!({});
    }
    for id in GAME_IDS {
        if !merged["games"][id].is_object() {
            merged["games"][id] = json!({ "gamePath": "" });
        }
        if !merged["games"][id]["playtime"].is_object() {
            merged["games"][id]["playtime"] = default_playtime_slice();
        }
    }
}

fn heal_game_settings(merged: &mut Value) {
    type Rule = (&'static str, fn(&Value) -> Option<Value>, fn(&Value, &Value) -> bool);
    const RULES: [Rule; 3] = [
        (
            "graphicsApi",
            |profile| graphics_api_default(profile).map(|api| json!(api)),
            |_, saved| matches!(saved.as_str(), Some("dx11" | "dx12")),
        ),
        (
            "resourceQuality",
            |profile| game_profiles::resource_quality_default(profile).map(|quality| json!(quality)),
            |profile, saved| saved.as_str().is_some_and(|q| profile["resourceQualityArgs"].get(q).is_some()),
        ),
        (
            "modsEnabled",
            |profile| game_profiles::mod_config(profile).map(|_| json!(false)),
            |_, saved| saved.is_boolean(),
        ),
    ];
    for id in GAME_IDS {
        let Some(game) = merged["games"].get_mut(id).and_then(Value::as_object_mut) else {
            continue;
        };
        let profile = game_profiles::profile(id);
        for (key, default, valid) in RULES {
            match default(profile) {
                None => {
                    game.remove(key);
                }
                Some(default) if !game.get(key).is_some_and(|saved| valid(profile, saved)) => {
                    game.insert(key.to_string(), default);
                }
                Some(_) => {}
            }
        }
    }
}

fn heal_media(merged: &mut Value, section: &str) {
    if !merged[section].is_object() {
        merged[section] = per_game_media();
        return;
    }
    for id in GAME_IDS {
        heal_media_slice(&mut merged[section], id);
    }
}

fn heal_media_slice(section: &mut Value, id: &str) {
    if !section[id].is_object() {
        section[id] = default_media_slice();
        return;
    }
    let slice = section[id].as_object_mut().expect("checked above");
    let type_ok = matches!(slice.get("type"), Some(Value::String(s)) if !s.is_empty());
    if !type_ok {
        slice.insert("type".to_string(), json!("default"));
    }
    if !slice.contains_key("path") {
        slice.insert("path".to_string(), Value::Null);
    }
}

// ------------ Safe Reading and Writing ------------
// Reads the file with retries, sets a broken one aside, restores the newest good copy, and writes through temp files so a crash cannot leave half a config.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{suffix}", path.display()))
}

fn read_with_retry(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut attempt = 1;
    loop {
        match std::fs::read(path) {
            Err(e)
                if attempt < READ_ATTEMPTS && matches!(e.raw_os_error(), Some(32) | Some(33)) =>
            {
                log::warn!(
                    "[config] {} is held by another program ({e}), retry {attempt}/{READ_ATTEMPTS}",
                    path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(READ_RETRY_MS));
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn parse_config_bytes(bytes: &[u8]) -> Result<Value, String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err("it does not hold a settings object".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_config_copy(path: &Path) -> Option<Value> {
    parse_config_bytes(&std::fs::read(path).ok()?).ok()
}

fn newest_good_copy(path: &Path) -> Option<(&'static str, Value)> {
    ["tmp", "bak"]
        .into_iter()
        .filter_map(|suffix| {
            let copy = sibling(path, suffix);
            let value = parse_config_copy(&copy)?;
            let modified = std::fs::metadata(&copy).and_then(|m| m.modified()).ok();
            Some((modified, suffix, value))
        })
        .max_by_key(|(modified, _, _)| *modified)
        .map(|(_, suffix, value)| (suffix, value))
}

fn set_aside(path: &Path, read: &[u8]) -> std::io::Result<Option<PathBuf>> {
    let aside = fs_util::corrupt_sibling(path);
    let kept = match std::fs::rename(path, &aside) {
        Ok(()) => aside,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => match write_synced(&aside, read) {
            Ok(()) => aside,
            Err(_) => return Err(e),
        },
    };
    if let Some(dir) = path.parent() {
        prune_corrupt_backups(dir, Some(&kept));
    }
    Ok(Some(kept))
}

fn keep_last_good_copy(path: &Path, bytes: Vec<u8>) -> Option<std::thread::JoinHandle<()>> {
    let bak = sibling(path, "bak");
    let spawned = std::thread::Builder::new()
        .name("config-backup".into())
        .spawn(move || {
            if std::fs::read(&bak).is_ok_and(|held| held == bytes) {
                let touched = std::fs::File::options()
                    .write(true)
                    .open(&bak)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()));
                if let Err(e) = touched {
                    log::warn!("[config] Could not refresh the last good copy of the config: {e}");
                }
                return;
            }
            let tmp = sibling(&bak, "tmp");
            let result = write_synced(&tmp, &bytes).and_then(|()| rename_with_retry(&tmp, &bak));
            if let Err(e) = result {
                let _ = std::fs::remove_file(&tmp);
                log::warn!("[config] Could not keep a last good copy of the config: {e}");
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!("[config] Could not start keeping a last good copy of the config: {e}");
            None
        }
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn retry_while_held(mut op: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match op() {
            Err(e) if attempt < SAVE_ATTEMPTS && matches!(e.raw_os_error(), Some(5) | Some(32)) => {
                std::thread::sleep(std::time::Duration::from_millis(
                    SAVE_RETRY_MS * u64::from(attempt),
                ));
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_while_held(|| std::fs::rename(from, to))
}

fn volume_reachable(path: &str) -> bool {
    match super::fs_util::drive_root(path) {
        Some(root) => std::fs::metadata(&root).is_ok(),
        None => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadIssue {
    Missing,
    Damaged,
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Recovery {
    issue: LoadIssue,
    restored: bool,
}

fn kept_note(kept: &std::io::Result<Option<PathBuf>>) -> String {
    match kept {
        Ok(Some(aside)) => format!("backed up to {}", aside.display()),
        Ok(None) => "it is gone".to_string(),
        Err(e) => format!("it could not be moved aside ({e})"),
    }
}

fn restore_newest_copy(path: &Path, problem: &str) -> Option<Value> {
    match newest_good_copy(path) {
        Some((suffix, value)) => {
            log::warn!("[config] {problem}; restored from the .{suffix} copy.");
            Some(value)
        }
        None => {
            log::warn!("[config] {problem}; no usable copy, so defaults are used.");
            None
        }
    }
}

// ------------ Config Store ------------
// The live config the rest of the backend reads and writes by dotted key, like behavior.modsEnabled. Changes are saved in the background.
pub struct LauncherConfig {
    path: PathBuf,
    user_data: PathBuf,
    data: RwLock<Value>,
    dirty: AtomicBool,
    save_pending: AtomicBool,
    read_failed: AtomicBool,
    read_failed_warned: AtomicBool,
    read_failed_kept: AtomicBool,
    save_lock: Mutex<()>,
    recovery: Option<Recovery>,
    backup: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl LauncherConfig {
    pub fn load(user_data: &Path) -> Arc<Self> {
        let path = user_data.join(CONFIG_FILE);
        let _ = std::fs::create_dir_all(user_data);

        let mut existing = Value::Object(Map::new());
        let mut unreadable = false;
        let mut recovery = None;
        let mut has_bom = false;
        let mut backup = None;

        match read_with_retry(&path) {
            Ok(bytes) => match parse_config_bytes(&bytes) {
                Ok(v) => {
                    existing = v;
                    has_bom = bytes.starts_with(b"\xEF\xBB\xBF");
                    log::info!("[config] Existing configuration loaded");
                    backup = keep_last_good_copy(&path, bytes);
                }
                Err(e) => {
                    let kept = set_aside(&path, &bytes);
                    let restored = restore_newest_copy(
                        &path,
                        &format!("Config was corrupt ({e}); {}", kept_note(&kept)),
                    );
                    recovery = Some(Recovery {
                        issue: LoadIssue::Damaged,
                        restored: restored.is_some(),
                    });
                    if let Some(v) = restored {
                        existing = v;
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match newest_good_copy(&path) {
                Some((suffix, v)) => {
                    existing = v;
                    recovery = Some(Recovery {
                        issue: LoadIssue::Missing,
                        restored: true,
                    });
                    log::warn!(
                        "[config] No config found, but its .{suffix} copy was; restored from that copy."
                    );
                }
                None => log::info!(
                    "[config] No existing config found — starting from defaults (first run)."
                ),
            },
            Err(e) => {
                unreadable = true;
                let restored = restore_newest_copy(
                    &path,
                    &format!(
                        "Failed to read existing config ({e}); it is left alone and changes in this session are not saved"
                    ),
                );
                recovery = Some(Recovery {
                    issue: LoadIssue::Unreadable,
                    restored: restored.is_some(),
                });
                if let Some(v) = restored {
                    existing = v;
                }
            }
        }

        let mut merged = default_config();
        if existing.is_object() {
            deep_merge(&mut merged, &existing);
        }
        sanitize_behavior(&mut merged);
        heal_library(&mut merged);
        heal_games(&mut merged);
        heal_media(&mut merged, "wallpaper");
        heal_media(&mut merged, "gameIcons");
        heal_game_settings(&mut merged);

        let needs_save = recovery.is_some() || has_bom || merged != existing;

        let cfg = Arc::new(Self {
            path,
            user_data: user_data.to_path_buf(),
            data: RwLock::new(merged),
            dirty: AtomicBool::new(false),
            save_pending: AtomicBool::new(false),
            read_failed: AtomicBool::new(unreadable),
            read_failed_warned: AtomicBool::new(false),
            read_failed_kept: AtomicBool::new(false),
            save_lock: Mutex::new(()),
            recovery,
            backup: Mutex::new(backup),
        });

        if !unreadable && needs_save {
            cfg.dirty.store(true, Ordering::SeqCst);
            if let Err(e) = cfg.save_now() {
                log::warn!("[config] Could not save merged config immediately: {e}");
            }
        }

        cfg
    }

    pub fn active_game_id(&self) -> String {
        match self.get("behavior.activeGameId") {
            Value::String(s) if !s.is_empty() => s,
            _ => game_profiles::DEFAULT_GAME_ID.to_string(),
        }
    }

    pub fn startup_notice(&self) -> Option<Value> {
        let recovery = self.recovery?;
        let loaded = if recovery.restored {
            "the last good copy was loaded"
        } else {
            "Peebify started with default settings"
        };
        let save_blocked = self.read_failed.load(Ordering::SeqCst);
        let (kind, title, text) = if save_blocked {
            (
                "warning",
                "Settings can't be saved",
                format!(
                    "Peebify couldn't open its settings file, most likely because another program is using it, so {loaded}. Changes you make now won't be saved. Close that program and restart Peebify."
                ),
            )
        } else {
            let problem = match recovery.issue {
                LoadIssue::Missing => "was missing",
                LoadIssue::Damaged => "was damaged",
                LoadIssue::Unreadable => "couldn't be opened",
            };
            if recovery.restored {
                (
                    "info",
                    "Settings restored from a backup",
                    format!(
                        "Peebify's settings file {problem}, so {loaded}. Changes from your last session may be missing."
                    ),
                )
            } else {
                (
                    "warning",
                    "Settings were reset",
                    format!(
                        "Peebify's settings file {problem} and no backup of it could be used, so your settings were reset."
                    ),
                )
            }
        };
        Some(json!({ "type": kind, "title": title, "text": text, "saveBlocked": save_blocked }))
    }

    pub fn sanitize_installed_paths_deferred(self: &Arc<Self>) -> bool {
        let mut changed = false;
        for id in GAME_IDS {
            let game_path = match self.get(&format!("games.{id}.gamePath")) {
                Value::String(s) if !s.is_empty() => s,
                _ => continue,
            };
            if self.get(&format!("games.{id}.pendingMove")).is_object() {
                log::info!("[config] {id} has an unfinished move, keeping the path for its recovery: {game_path}");
                continue;
            }
            if !volume_reachable(&game_path) {
                log::warn!(
                    "[config] Install drive for {id} is not reachable, keeping the path: {game_path}"
                );
                continue;
            }
            let profile = game_profiles::profile(id);
            let validation = game_profiles::validate_game_path_for_profile(&game_path, profile);
            if !validation.is_valid {
                log::info!(
                    "[config] Clearing invalid install path for {id} ({}): {game_path}",
                    validation.error.as_deref().unwrap_or("not a valid install")
                );
                self.set(&format!("games.{id}.gamePath"), json!(""));
                changed = true;
            } else if let Some(resolved) = validation.resolved_path {
                if resolved != game_path {
                    self.set(&format!("games.{id}.gamePath"), json!(resolved));
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn get(&self, key: &str) -> Value {
        let data = self.data.read();
        if key.is_empty() {
            return data.clone();
        }
        let mut cur: &Value = &data;
        for k in key.split('.') {
            match cur.get(k) {
                Some(v) => cur = v,
                None => return Value::Null,
            }
        }
        cur.clone()
    }

    pub fn set(self: &Arc<Self>, key: &str, value: Value) -> bool {
        self.update(key, |slot| {
            *slot = value;
            true
        })
    }

    pub fn update(self: &Arc<Self>, key: &str, apply: impl FnOnce(&mut Value) -> bool) -> bool {
        if key.is_empty() {
            return false;
        }
        {
            let mut data = self.data.write();
            let mut value = lookup(&data, key).cloned().unwrap_or(Value::Null);
            if !apply(&mut value) {
                return false;
            }
            if lookup(&data, key) == Some(&value) {
                return true;
            }
            set_path(&mut data, key, value);
        }
        self.schedule_save();
        true
    }

    fn schedule_save(self: &Arc<Self>) {
        self.dirty.store(true, Ordering::SeqCst);
        if self.save_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(SAVE_DEBOUNCE_MS)).await;
            me.save_pending.store(false, Ordering::SeqCst);
            match tauri::async_runtime::spawn_blocking(move || me.save_now()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("[config] debounced save failed: {e}"),
                Err(e) => log::error!("[config] debounced save did not run: {e}"),
            }
        });
    }

    pub fn flush(&self) {
        if let Err(e) = self.save_now() {
            log::error!("[config] flush failed: {e}");
        }
    }

    fn save_now(&self) -> Result<(), String> {
        let _guard = self.save_lock.lock();
        self.save_locked()
    }

    fn save_locked(&self) -> Result<(), String> {
        if self.read_failed.load(Ordering::SeqCst) {
            self.dirty.store(true, Ordering::SeqCst);
            if !self.unread_config_replaceable() {
                return Ok(());
            }
            self.read_failed.store(false, Ordering::SeqCst);
        }

        if !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        let text = serde_json::to_string_pretty(&*self.data.read())
            .map_err(|e| format!("serialize failed: {e}"))?;

        let restore_dirty = |e: String| {
            self.dirty.store(true, Ordering::SeqCst);
            e
        };

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = sibling(&self.path, "tmp");
        retry_while_held(|| write_synced(&tmp, text.as_bytes()))
            .map_err(|e| restore_dirty(format!("write failed for {}: {e}", tmp.display())))?;
        rename_with_retry(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            restore_dirty(format!("rename failed for {}: {e}", self.path.display()))
        })?;
        Ok(())
    }

    fn unread_config_replaceable(&self) -> bool {
        if self.read_failed_kept.load(Ordering::SeqCst) {
            return false;
        }
        let reason = if !self.recovery.is_some_and(|r| r.restored) {
            "this session runs on defaults".to_string()
        } else {
            match std::fs::read(&self.path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    log::info!(
                        "[config] The config that could not be read at startup is gone, so settings are saved again."
                    );
                    return true;
                }
                Err(e) => format!("it still can't be read ({e})"),
                Ok(bytes) => match parse_config_bytes(&bytes) {
                    Ok(_) => {
                        self.read_failed_kept.store(true, Ordering::SeqCst);
                        "it can be read now, so it is kept as it is".to_string()
                    }
                    Err(problem) => match set_aside(&self.path, &bytes) {
                        Ok(kept) => {
                            log::warn!(
                                "[config] The config that could not be read at startup is damaged ({problem}); {}, so settings are saved again.",
                                kept_note(&Ok(kept))
                            );
                            return true;
                        }
                        Err(e) => {
                            format!("it is damaged ({problem}) but can't be moved aside ({e})")
                        }
                    },
                },
            }
        };
        if !self.read_failed_warned.swap(true, Ordering::SeqCst) {
            log::error!(
                "[config] The configuration file could not be read at startup and {reason}, so \
                 it is not overwritten and settings changed in this session are not saved. \
                 Restart the launcher once nothing else is holding the file."
            );
        }
        false
    }

    fn wait_for_backup(&self) {
        if let Some(handle) = self.backup.lock().take() {
            let _ = handle.join();
        }
    }

    pub fn wipe(&self) -> Result<(), String> {
        log::info!("[config] Wiping launcher configuration data...");
        self.wait_for_backup();
        let _guard = self.save_lock.lock();
        self.read_failed.store(false, Ordering::SeqCst);

        let fresh = {
            let old = self.data.read();
            let mut fresh = default_config();
            for id in GAME_IDS {
                if let Some(path) = old["games"][id]["gamePath"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                {
                    fresh["games"][id]["gamePath"] = json!(path);
                }
            }
            fresh
        };

        let _ = std::fs::remove_file(sibling(&self.path, "tmp"));
        let _ = std::fs::remove_file(sibling(&self.path, "bak"));
        let _ = std::fs::remove_file(sibling(&self.path, "bak.tmp"));
        prune_corrupt_backups(&self.user_data, None);

        for cache in [
            "api-config-cache.json",
            "api-config-cache.json.tmp",
            "mods/profiles.json",
            "mods/profiles.json.tmp",
        ] {
            let _ = std::fs::remove_file(self.user_data.join(cache));
        }
        for dir in ["wallpaper-cache", "manifest-cache"] {
            let path = self.user_data.join(dir);
            if path.exists() {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => log::info!("[config] Removed cache directory {dir}"),
                    Err(e) => log::warn!("[config] Could not remove {dir}: {e}"),
                }
            }
        }

        *self.data.write() = fresh;
        self.dirty.store(true, Ordering::SeqCst);
        self.save_locked()?;

        log::info!(
            "[config] Configuration data wiped successfully, install locations were kept"
        );
        Ok(())
    }
}

fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(root, |cur, k| cur.get(k))
}

fn set_path(root: &mut Value, key: &str, value: Value) {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let keys: Vec<&str> = key.split('.').collect();
    let mut cur = root;
    for k in &keys[..keys.len() - 1] {
        let Some(map) = cur.as_object_mut() else {
            return;
        };
        let entry = map
            .entry((*k).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        cur = entry;
    }
    if let Some(map) = cur.as_object_mut() {
        map.insert(keys[keys.len() - 1].to_string(), value);
    }
}

// ------------ Config Commands ------------
// The handlers behind the window's settings calls: reading and changing config, picking game folders, wallpapers and
// icons, opening logs and links, and wiping launcher data. The window is only allowed to change a fixed list of keys,
// and each value is checked before it is saved.
pub(crate) fn set_config_value(app: &AppHandle, key: &str, value: Value) {
    backend(app).config.set(key, value);
}

fn config_key_readable(key: &str) -> bool {
    key != "account" && !key.starts_with("account.")
}

pub(super) async fn get_config(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    if let Some(key) = arg_str(args, 0) {
        if !config_key_readable(key) {
            return Ok(err_response(format!("'{key}' cannot be read by the interface")));
        }
        return Ok(ok_with(json!({ "value": backend(app).config.get(key) })));
    }
    if let Some(keys) = args.first().and_then(Value::as_array) {
        let config = &backend(app).config;
        let mut values = Map::new();
        for key in keys {
            let Some(key) = key.as_str().filter(|k| !k.is_empty()) else {
                return Ok(err_response("Config keys must be non-empty strings"));
            };
            if !config_key_readable(key) {
                return Ok(err_response(format!("'{key}' cannot be read by the interface")));
            }
            values.insert(key.to_string(), config.get(key));
        }
        return Ok(ok_with(json!({ "values": values })));
    }
    Ok(err_response("Name the config keys to read."))
}

pub(crate) fn parse_content_tags(value: &Value) -> Option<Vec<String>> {
    let list = value.as_array()?;
    Some(
        list.iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

pub(crate) fn apply_game_packs(config: &LauncherConfig) {
    for id in GAME_IDS {
        if let Value::String(lang) = config.get(&format!("games.{id}.voicePackLanguage")) {
            game_profiles::set_audio_language(id, &lang);
        }
        let tags = config.get(&format!("games.{id}.contentTags"));
        game_profiles::set_content_tags(id, parse_content_tags(&tags));
    }
}

// ------------ What the Window May Change ------------
// The list of settings the interface can write, with the type each one must have, and the handler that applies a change.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Bool,
    Str,
    StrArray,
    StrArrayOrNull,
}

impl ValueKind {
    fn accepts(self, value: &Value) -> bool {
        match self {
            ValueKind::Bool => value.is_boolean(),
            ValueKind::Str => value.is_string(),
            ValueKind::StrArray => value
                .as_array()
                .is_some_and(|list| list.iter().all(Value::is_string)),
            ValueKind::StrArrayOrNull => value.is_null() || ValueKind::StrArray.accepts(value),
        }
    }
}

const WRITABLE_KEYS: &[(&str, ValueKind)] = &[
    ("behavior.modsEnabled", ValueKind::Bool),
    ("library.visible", ValueKind::StrArray),
    ("library.setupComplete", ValueKind::Bool),
];

const WRITABLE_GAME_KEYS: &[(&str, ValueKind)] = &[
    ("autoUpdate", ValueKind::Bool),
    ("autoUpdateOnStartup", ValueKind::Bool),
    ("autoUpdateSchedule", ValueKind::Str),
    ("contentTags", ValueKind::StrArrayOrNull),
    ("graphicsApi", ValueKind::Str),
    ("launchArgs", ValueKind::Str),
    ("launchViaSteam", ValueKind::Bool),
    ("playtimeColor", ValueKind::Str),
    ("resourceQuality", ValueKind::Str),
    ("voicePackLanguage", ValueKind::Str),
];

fn renderer_may_write(key: &str, value: &Value) -> Result<(), String> {
    if let Some((_, kind)) = WRITABLE_KEYS.iter().find(|(k, _)| *k == key) {
        return if kind.accepts(value) {
            Ok(())
        } else {
            Err(format!("the value for '{key}' has the wrong type"))
        };
    }

    if let Some(rest) = key.strip_prefix("games.") {
        if let Some((id, leaf)) = rest.split_once('.') {
            if !game_profiles::is_known_game_id(id) {
                return Err(format!("'{id}' is not a known game"));
            }
            if let Some((_, kind)) = WRITABLE_GAME_KEYS.iter().find(|(k, _)| *k == leaf) {
                return if kind.accepts(value) {
                    Ok(())
                } else {
                    Err(format!("the value for '{key}' has the wrong type"))
                };
            }
        }
    }

    Err(format!("'{key}' is not a setting the interface may change"))
}

const SETTING_LOG_MAX_CHARS: usize = 120;

fn setting_log_value(value: &str) -> String {
    if value.chars().count() <= SETTING_LOG_MAX_CHARS {
        return value.to_string();
    }
    let head: String = value.chars().take(SETTING_LOG_MAX_CHARS).collect();
    format!("{head}...")
}

fn library_change_needs_sync(key: &str, previous: &Value, value: &Value) -> bool {
    if key != "library.visible" {
        return previous != value;
    }
    let ids = |list: &Value| -> std::collections::BTreeSet<String> {
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    ids(previous) != ids(value)
}

pub(super) async fn set_config(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(key) = args
        .first()
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty())
    else {
        return Ok(err_response("Failed to set config value"));
    };
    let value = args.get(1).cloned().unwrap_or(Value::Null);

    if let Err(reason) = renderer_may_write(key, &value) {
        log::warn!("[config] Rejected a set-config write: {reason}");
        return Ok(err_response(format!(
            "Failed to set config value: {reason}"
        )));
    }
    if let Some(id) = key
        .strip_prefix("games.")
        .and_then(|rest| rest.strip_suffix(".voicePackLanguage"))
    {
        super::game_profiles::set_audio_language(id, value.as_str().unwrap_or_default());
        super::engine::invalidate_preview(id);
    }
    if let Some(id) = key
        .strip_prefix("games.")
        .and_then(|rest| rest.strip_suffix(".contentTags"))
    {
        super::game_profiles::set_content_tags(id, parse_content_tags(&value));
        super::engine::invalidate_preview(id);
    }
    let previous = key
        .starts_with("library.")
        .then(|| backend(app).config.get(key));
    set_config_value(app, key, value.clone());
    let shown = value.as_str().map_or_else(|| value.to_string(), str::to_string);
    log::info!("[settings] {key} = {}", setting_log_value(&shown));
    if previous.is_some_and(|previous| library_change_needs_sync(key, &previous, &value)) {
        backend(app).wallpaper.wake("after a library change");
    }
    use tauri::Emitter;
    let _ = app.emit("settings-changed", json!({ "key": key, "value": value }));
    Ok(ok_response())
}

pub(super) async fn get_app_version(app: &AppHandle) -> Result<Value, String> {
    Ok(ok_with(json!({
        "version": app.package_info().version.to_string()
    })))
}

// ------------ Game Selection and Paths ------------
// Switching the active game, browsing for or auto-detecting an install folder, and choosing a custom launcher.
pub(super) async fn set_active_game(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let requested = args.first().and_then(|v| v.as_str()).unwrap_or("");
    let profile = game_profiles::profile(requested);
    let id = game_profiles::profile_id(profile).to_string();
    set_config_value(app, "behavior.activeGameId", json!(id));
    backend(app).wallpaper.wake_if_uncached(&id);
    Ok(ok_with(json!({ "activeGameId": id })))
}

pub(super) fn browse_start_dir(app: &AppHandle, profile: &Value) -> Option<String> {
    let key = format!("games.{}.gamePath", game_profiles::profile_id(profile));
    let current = backend(app).config.get(&key);
    let current_parent = current
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Path::new(s).parent().map(Path::to_path_buf));
    current_parent
        .into_iter()
        .chain(
            game_profiles::default_install_dirs(profile)
                .into_iter()
                .filter_map(|(_, dir)| dir.parent().map(Path::to_path_buf)),
        )
        .find(|dir| dir.is_dir())
        .map(|dir| dir.to_string_lossy().into_owned())
}

fn refuse_locate_beside_job(app: &AppHandle, profile: &Value) -> Option<Value> {
    let id = game_profiles::profile_id(profile);
    if !backend(app).engine.queue.has_job_for(id) && !game_file_ops::is_uninstalling(id) {
        return None;
    }
    log::info!("Refusing to locate {id} because it has an operation running or queued.");
    Some(err_response(format!(
        "Finish or cancel {}'s current download, repair or move first.",
        game_profiles::display_name(profile)
    )))
}

fn adopt_located_path(app: &AppHandle, profile: &Value, resolved: &str, picked: Option<&str>) {
    let id = game_profiles::profile_id(profile);
    set_config_value(app, &format!("games.{id}.gamePath"), json!(resolved));
    super::file_channels::clear_pending_install(app, id);
    super::validator::clear_install_marker(Path::new(resolved));
    if let Some(picked) = picked.filter(|p| *p != resolved) {
        super::validator::clear_install_marker(Path::new(picked));
    }
}

pub(super) async fn browse_game_path(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = match game_file_ops::known_profile_or_error(app, arg_str(args, 0)) {
        Ok(profile) => profile,
        Err(refusal) => return Ok(refusal),
    };
    if let Some(refusal) = refuse_locate_beside_job(app, profile) {
        return Ok(refusal);
    }

    let selected = pick_folder(
        app,
        Pick {
            title: format!("Select {} Installation Folder", game_profiles::display_name(profile)),
            default_path: browse_start_dir(app, profile),
            ..Pick::default()
        },
    )
    .await?;
    let Some(selected) = selected else {
        return Ok(cancelled_pick());
    };

    let selected_for_walk = selected.clone();
    let validation = tauri::async_runtime::spawn_blocking(move || {
        game_profiles::validate_game_path_for_profile(&selected_for_walk, profile)
    })
    .await
    .map_err(|e| e.to_string())?;

    if validation.is_valid {
        if let Some(refusal) = refuse_locate_beside_job(app, profile) {
            return Ok(refusal);
        }
        let resolved = validation.resolved_path.unwrap_or_else(|| selected.clone());
        adopt_located_path(app, profile, &resolved, Some(&selected));
        game_profiles::resolve_audio_language(app, profile, Path::new(&resolved), None).await;
        Ok(ok_with(json!({ "path": resolved })))
    } else {
        Ok(err_response(
            validation.error.as_deref().unwrap_or("Invalid game path"),
        ))
    }
}

const LAUNCHER_EXTENSIONS: [&str; 3] = ["exe", "bat", "cmd"];

fn cancelled_pick() -> Value {
    json!({ "success": false, "cancelled": true })
}

pub(super) async fn set_custom_launcher(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(game_id) = args.first().and_then(Value::as_str) else {
        return Ok(err_response("No game was given."));
    };
    if !game_profiles::is_known_game_id(game_id) {
        return Ok(err_response(format!("'{game_id}' is not a known game.")));
    }
    let key = format!("games.{game_id}.customLauncher");

    if args.get(1).and_then(Value::as_str) != Some("pick") {
        set_config_value(app, &key, Value::String(String::new()));
        log::info!("[config] the custom launcher for {game_id} was cleared");
        return Ok(ok_with(json!({ "path": "" })));
    }

    let picked = pick_file(
        app,
        Pick {
            title: "Choose the program that starts the game".into(),
            filters: &[("Program", &LAUNCHER_EXTENSIONS)],
            ..Pick::default()
        },
    )
    .await?;
    let Some(path) = picked else {
        return Ok(cancelled_pick());
    };

    let candidate = Path::new(&path);
    if !candidate
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            LAUNCHER_EXTENSIONS
                .iter()
                .any(|ok| e.eq_ignore_ascii_case(ok))
        })
    {
        return Ok(err_response("Pick a program: an .exe, .bat or .cmd file."));
    }
    if !candidate.is_file() {
        return Ok(err_response(format!("There is no file at {path}.")));
    }

    set_config_value(app, &key, Value::String(path.clone()));
    log::info!("[config] the custom launcher for {game_id} is now {path}");
    Ok(ok_with(json!({ "path": path })))
}

pub(super) async fn detect_default_game_path(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let profile = match game_file_ops::known_profile_or_error(app, arg_str(args, 0)) {
        Ok(profile) => profile,
        Err(refusal) => return Ok(refusal),
    };
    if let Some(refusal) = refuse_locate_beside_job(app, profile) {
        return Ok(refusal);
    }

    let (found, unfinished) = tauri::async_runtime::spawn_blocking(move || {
        let found = game_profiles::install_in_default_dirs(profile);
        let unfinished = found.is_none()
            && game_profiles::default_install_dirs(profile)
                .iter()
                .any(|(_, dir)| {
                    dir.is_dir() && game_profiles::is_unfinished_fresh_install(dir, dir)
                });
        (found, unfinished)
    })
    .await
    .map_err(|e| e.to_string())?;

    if let Some(resolved) = found {
        if let Some(refusal) = refuse_locate_beside_job(app, profile) {
            return Ok(refusal);
        }
        adopt_located_path(app, profile, &resolved, None);
        game_profiles::resolve_audio_language(app, profile, Path::new(&resolved), None).await;
        return Ok(ok_with(json!({ "path": resolved })));
    }
    if unfinished {
        return Ok(err_response(
            "The install at Peebify's default location never finished. Install the game again to pick up where it stopped.",
        ));
    }
    Ok(err_response(
        "Nothing installed at Peebify's default install location.",
    ))
}

// ------------ Behavior Settings ------------
// Reads and saves the general launcher settings, merging in only what changed.
pub(super) async fn get_settings(app: &AppHandle) -> Result<Value, String> {
    let behavior = backend(app).config.get("behavior");
    let mut flat = Map::new();
    if let Some(b) = behavior.as_object() {
        for (k, v) in b {
            let s = match v {
                Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            flat.insert(k.clone(), json!(s));
        }
        let truthy = |v: Option<&Value>| match v {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
            Some(Value::String(s)) => !s.is_empty(),
            _ => false,
        };
        flat.insert(
            "animatedWallpaper".to_string(),
            json!(if truthy(b.get("disableAnimations")) {
                "false"
            } else {
                "true"
            }),
        );
    }
    Ok(ok_with(json!({
        "settings": Value::Object(flat),
        "configNotice": backend(app).config.startup_notice(),
    })))
}

const BACKEND_OWNED_BEHAVIOR: [&str; 3] = ["modsPath", "overlayCaptureFolder", "activeGameId"];

fn setting_may_write(key: &str) -> Result<(), String> {
    if key.is_empty() || key.contains('.') {
        return Err(format!("'{key}' is not a setting id"));
    }
    if BACKEND_OWNED_BEHAVIOR.contains(&key)
        || super::overlay::HOTKEYS.iter().any(|(id, _, _)| *id == key)
    {
        return Err(format!("'{key}' has its own control and cannot be set directly"));
    }
    Ok(())
}

fn setting_delta(key: &str, value: &str) -> Map<String, Value> {
    let mut delta = Map::new();
    match key {
        "animatedWallpaper" => {
            delta.insert("disableAnimations".to_string(), json!(value != "true"));
        }
        _ if value == "true" || value == "false" => {
            delta.insert(key.to_string(), json!(value == "true"));
        }
        _ => {
            delta.insert(key.to_string(), json!(value));
        }
    }
    delta
}

pub(super) async fn set_setting(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(key) = args
        .first()
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty())
    else {
        return Ok(err_response("Missing setting id"));
    };
    let value = args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Err(reason) = setting_may_write(key) {
        log::warn!("[config] Rejected a set-setting write: {reason}");
        return Ok(err_response(format!("Failed to save the setting: {reason}")));
    }

    let saved = save_behavior_settings(app, &[Value::Object(setting_delta(key, &value))]).await?;
    if saved.get("success") != Some(&Value::Bool(true)) {
        return Ok(saved);
    }
    log::info!("[settings] {key} = {}", setting_log_value(&value));
    if key == "animatedWallpaper" && value == "true" {
        backend(app).wallpaper.wake("after animated wallpapers were turned on");
    }
    if key == "peebifyContent" && value == "true" {
        backend(app).wallpaper.wake("after Peebify online content was turned on");
    }
    use tauri::Emitter;
    if key.starts_with("overlay") {
        backend(app).overlay.on_setting_changed(key);
    }
    if key == "uiScale" {
        backend(app).window.apply_ui_scale();
    }
    if key.starts_with("download") {
        let behavior = backend(app).config.get("behavior");
        super::http::shared_transfer().apply_settings(transfer_settings(&behavior));
    }
    let _ = app.emit("settings-changed", json!({ "key": key, "value": value }));
    Ok(saved)
}

fn merge_behavior(behavior: &mut Value, delta: &Map<String, Value>) -> bool {
    let current = behavior.take();
    let mut updated = current.as_object().cloned().unwrap_or_default();
    for (k, v) in delta {
        updated.insert(k.clone(), v.clone());
    }
    let mut wrapper = json!({ "behavior": Value::Object(updated) });
    sanitize_behavior(&mut wrapper);
    *behavior = wrapper["behavior"].take();
    delta.get("rememberWindowState") == Some(&Value::Bool(true))
        && current["rememberWindowState"] != Value::Bool(true)
}

pub(super) async fn save_behavior_settings(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let Some(new_behavior) = args.first().and_then(|v| v.as_object()) else {
        return Ok(err_response("Invalid behavior settings payload"));
    };

    let before = backend(app).config.get("behavior");

    let mut note = None;
    if let Some(new_boot) = new_behavior.get("startOnBoot") {
        if *new_boot != before["startOnBoot"] {
            let enable = new_boot.as_bool().unwrap_or(false);
            match platform::manage_windows_startup(enable).await {
                Ok(partial) => {
                    log::info!(
                        "[config] Start on boot {}",
                        if enable { "enabled" } else { "disabled" }
                    );
                    note = partial;
                }
                Err(e) => {
                    log::error!("[config] Failed to manage startup methods: {e}");
                    return Ok(err_response(format!(
                        "Could not update \"start on boot\": {e}"
                    )));
                }
            }
        }
    }

    let mut remember_turned_on = false;
    backend(app).config.update("behavior", |behavior| {
        remember_turned_on = merge_behavior(behavior, new_behavior);
        true
    });
    if remember_turned_on {
        backend(app).window.schedule_window_state_save();
    }

    match note {
        Some(text) => Ok(ok_with(json!({
            "warningTitle": "Start with Windows",
            "warning": text,
        }))),
        None => Ok(ok_response()),
    }
}

// ------------ Wallpapers and Game Icons ------------
// Picking custom wallpapers and game icons from disk, and only trusting files that were actually picked in the dialog.
const WALLPAPER_EXTENSIONS: [&str; 9] =
    ["mp4", "webm", "mov", "m4v", "png", "jpg", "jpeg", "webp", "gif"];
const ICON_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];
const MEDIA_SECTIONS: [&str; 2] = ["wallpaper", "gameIcons"];

fn media_extensions(section: &str) -> &'static [&'static str] {
    match section {
        "wallpaper" => &WALLPAPER_EXTENSIONS,
        _ => &ICON_EXTENSIONS,
    }
}

fn media_label(section: &str) -> &'static str {
    match section {
        "wallpaper" => "wallpaper",
        _ => "game icon",
    }
}

static PICKED_MEDIA: parking_lot::Mutex<Vec<PathBuf>> = parking_lot::Mutex::new(Vec::new());

fn record_picked_media(path: PathBuf) {
    let mut picked = PICKED_MEDIA.lock();
    if !picked.contains(&path) {
        picked.push(path);
    }
}

fn was_picked(path: &Path) -> bool {
    PICKED_MEDIA.lock().iter().any(|p| p == path)
}

fn remember_picked_media(app: &AppHandle, section: &str, picked: &str) {
    let path = PathBuf::from(picked);
    if custom_media_path_ok(section, &path) {
        fs_util::allow_asset_file(app, &path);
        record_picked_media(path);
    }
}

fn custom_media_path_ok(section: &str, path: &Path) -> bool {
    path.is_absolute()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                media_extensions(section)
                    .iter()
                    .any(|allowed| ext.eq_ignore_ascii_case(allowed))
            })
}

fn parse_media_slice(section: &str, slice: &Value) -> Result<(Value, Option<PathBuf>), String> {
    let label = media_label(section);
    match slice.get("type").and_then(Value::as_str) {
        Some("default") => Ok((json!({ "type": "default", "path": null }), None)),
        Some("custom") => {
            let raw = slice.get("path").and_then(Value::as_str).unwrap_or_default();
            let path = PathBuf::from(raw);
            if !custom_media_path_ok(section, &path) {
                return Err(format!("That file can't be used as a {label}."));
            }
            Ok((json!({ "type": "custom", "path": raw }), Some(path)))
        }
        _ => Err(format!("The {label} setting was not understood.")),
    }
}

pub(super) fn allow_custom_media(app: &AppHandle, config: &LauncherConfig) {
    let mut files: Vec<(&'static str, PathBuf)> = Vec::new();
    for section in MEDIA_SECTIONS {
        for id in game_profiles::GAME_IDS {
            let slice = config.get(&format!("{section}.{id}"));
            if let Ok((_, Some(file))) = parse_media_slice(section, &slice) {
                if !files.iter().any(|(_, seen)| *seen == file) {
                    files.push((section, file));
                }
            }
        }
    }
    for (section, file) in files {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if file.is_file() {
                fs_util::allow_asset_file(&app, &file);
            } else {
                log::info!(
                    "asset scope: the custom {section} {} is not reachable",
                    file.display()
                );
            }
        });
    }
}

fn media_dialog_filters(section: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match section {
        "wallpaper" => &[
            ("Images and videos", &WALLPAPER_EXTENSIONS),
            ("Images", &["png", "jpg", "jpeg", "webp", "gif"]),
            ("Videos", &["mp4", "webm", "mov", "m4v"]),
        ],
        _ => &[
            ("All icons", &ICON_EXTENSIONS),
            ("Images", &["png", "jpg", "jpeg"]),
            ("Animated", &["gif", "webp"]),
        ],
    }
}

pub(super) async fn select_media_file(app: &AppHandle, section: &str) -> Result<Value, String> {
    let title = if section == "wallpaper" { "Select Custom Wallpaper" } else { "Select Custom Game Icon" };
    let picked = pick_file(
        app,
        Pick {
            title: title.into(),
            filters: media_dialog_filters(section),
            ..Pick::default()
        },
    )
    .await?;
    let Some(path) = picked else {
        return Ok(cancelled_pick());
    };
    remember_picked_media(app, section, &path);
    Ok(ok_with(json!({ "path": path })))
}

pub(super) async fn save_media_slice(app: &AppHandle, args: &[Value], section: &str) -> Result<Value, String> {
    let label = media_label(section);
    let payload = args.first().cloned().unwrap_or(Value::Null);
    if !payload.is_object() {
        return Ok(err_response(format!("The {label} setting was not understood.")));
    }

    let game_id = match payload.get("gameId") {
        None | Some(Value::Null) => active_game_id(app),
        Some(Value::String(id)) if id.is_empty() => active_game_id(app),
        Some(Value::String(id)) if game_profiles::is_known_game_id(id) => id.clone(),
        Some(other) => {
            log::warn!(
                "[config] Rejected a {section} save for an unknown game: {}",
                setting_log_value(&other.to_string())
            );
            return Ok(err_response("That is not a known game."));
        }
    };

    let slice_config = match payload.get("config") {
        Some(c) if !c.is_null() => c,
        _ => &payload,
    };
    let (slice_config, file) = match parse_media_slice(section, slice_config) {
        Ok(parsed) => parsed,
        Err(reason) => {
            log::warn!("[config] Rejected a {section} save for {game_id}: {reason}");
            return Ok(err_response(reason));
        }
    };
    if let Some(file) = &file {
        let stored = backend(app).config.get(&format!("{section}.{game_id}.path"));
        if stored.as_str() != file.to_str() && !was_picked(file) {
            log::warn!(
                "[config] Rejected a {section} save for {game_id}: {} was not picked",
                setting_log_value(&file.display().to_string())
            );
            return Ok(err_response(format!("Choose the {label} with the file picker.")));
        }
        if file.is_file() {
            fs_util::allow_asset_file(app, file);
        }
    }

    let mut slices = match backend(app).config.get(section) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    slices.retain(|id, _| game_profiles::is_known_game_id(id));
    slices.insert(game_id, slice_config);
    set_config_value(app, section, Value::Object(slices));

    Ok(ok_response())
}

// ------------ Logs, Folders and Links ------------
// Opening the logs folder and external links, and wiping launcher data.
fn newest_launch_log(dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("launch-") && name.ends_with(".log"))
        .max()
        .map(|name| dir.join(name))
}

fn reveal_in_explorer(file: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", file.to_string_lossy()))
        .spawn()
        .is_ok()
}

pub(super) async fn open_logs_folder(app: &AppHandle) -> Result<Value, String> {
    super::logger::flush();
    let logs_dir = backend(app).logs_dir.clone();
    let _ = std::fs::create_dir_all(&logs_dir);
    let logs_str = logs_dir.to_string_lossy().to_string();

    if newest_launch_log(&logs_dir).is_some_and(|current| reveal_in_explorer(&current)) {
        return Ok(ok_with(json!({ "path": logs_str })));
    }

    match open_folder(&logs_dir) {
        Ok(()) => Ok(ok_with(json!({ "path": logs_str }))),
        Err(e) => {
            log::error!("[config] Failed to open logs folder: {e}");
            Ok(err_response(e))
        }
    }
}

const WIPE_BUSY_MSG: &str =
    "Finish or cancel the running download or uninstall before clearing launcher data.";

pub(super) async fn wipe_launcher_data(app: &AppHandle) -> Result<Value, String> {
    let uninstalling = game_profiles::GAME_IDS
        .iter()
        .any(|id| game_file_ops::is_uninstalling(id));
    if backend(app).engine.queue.is_busy()
        || uninstalling
        || super::gamebanana::any_in_flight()
    {
        return Ok(err_response(WIPE_BUSY_MSG));
    }
    if let Some((game_id, _)) = backend(app).game.running_game() {
        return Ok(err_response(format!(
            "Close {} before clearing launcher data.",
            game_profiles::display_name(game_profiles::profile(&game_id))
        )));
    }

    let partials: Vec<(&'static str, std::path::PathBuf)> = {
        let config = &backend(app).config;
        game_profiles::GAME_IDS
            .iter()
            .filter(|id| {
                config
                    .get(&format!("games.{id}.gamePath"))
                    .as_str()
                    .is_none_or(str::is_empty)
            })
            .filter_map(|id| {
                config
                    .get(&format!("games.{id}.pendingInstall.path"))
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .map(|p| (*id, std::path::PathBuf::from(p)))
            })
            .collect()
    };
    let (mut files, mut bytes) = (0, 0);
    for (id, path) in &partials {
        let (f, b) = super::file_channels::discard_partial_install(path, id).await;
        (files, bytes) = (files + f, bytes + b);
    }
    if files > 0 {
        log::info!(
            "[config] Discarded {files} unfinished download file(s), {:.2} GB, before the data wipe",
            bytes as f64 / 1e9
        );
    }

    match platform::manage_windows_startup(false).await {
        Ok(note) => {
            log::info!("[config] Cleaned up startup methods during data wipe");
            if let Some(note) = note {
                log::warn!("[config] {note}");
            }
        }
        Err(e) => log::warn!("[config] Could not clean up startup methods during wipe: {e}"),
    }

    if let Err(e) = backend(app).config.wipe() {
        log::error!("[config] Failed to wipe launcher data: {e}");
        return Ok(err_response(e));
    }

    super::logger::end_session("restart after the data wipe");
    app.restart()
}

pub(super) async fn open_external_url(args: &[Value]) -> Result<Value, String> {
    let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
    if url.trim().is_empty() {
        return Ok(err_response("Invalid URL provided."));
    }
    Ok(wrap_result(&format!("Failed to open external URL: {url}"), open_url(url)))
}

// ------------ Tests ------------
// Covers loading, recovery from damaged or locked files, saving, the default tables, the write allowlist and wallpaper or
// icon settings.
#[cfg(test)]
mod write_allowlist_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_keys_the_interface_actually_writes_are_allowed() {
        for (key, value) in [
            ("behavior.modsEnabled", json!(true)),
            ("library.setupComplete", json!(true)),
            ("library.visible", json!(["wuwa", "zzz"])),
            ("games.wuwa.launchArgs", json!("-dx11")),
            ("games.wuwa.launchViaSteam", json!(false)),
            ("games.zzz.graphicsApi", json!("dx12")),
            ("games.genshin.voicePackLanguage", json!("ja-jp")),
            ("games.hsr.contentTags", json!(["en-us"])),
            ("games.pgr.autoUpdate", json!(true)),
            ("games.pgr.autoUpdateOnStartup", json!(false)),
            ("games.pgr.autoUpdateSchedule", json!("daily")),
        ] {
            assert!(
                renderer_may_write(key, &value).is_ok(),
                "the interface writes {key} but the allowlist rejects it"
            );
        }
    }

    #[test]
    fn state_the_backend_owns_is_refused() {
        for (key, value) in [
            ("games.wuwa.gamePath", json!(r"C:\Windows\System32")),
            ("behavior.modsPath", json!(r"C:\Users\me")),
            ("account.token", json!("stolen")),
            ("account", json!({ "token": "stolen" })),
            ("deviceId", json!("spoofed")),
            ("games.wuwa.pendingInstall", json!({})),
        ] {
            assert!(
                renderer_may_write(key, &value).is_err(),
                "{key} must not be renderer-writable"
            );
        }
    }

    #[test]
    fn a_whole_subtree_cannot_be_written_in_one_go() {
        assert!(
            renderer_may_write("games", &json!({ "wuwa": { "gamePath": r"C:\Windows" } })).is_err()
        );
        assert!(renderer_may_write("behavior", &json!({ "modsEnabled": true })).is_err());
        assert!(renderer_may_write("library", &json!({ "visible": [] })).is_err());
    }

    #[test]
    fn content_tags_accept_null_to_mean_every_pack() {
        assert!(renderer_may_write("games.nte.contentTags", &Value::Null).is_ok());
        assert_eq!(parse_content_tags(&Value::Null), None);
        assert!(renderer_may_write("games.nte.launchArgs", &Value::Null).is_err());
        assert!(renderer_may_write("games.nte.contentTags", &json!([1])).is_err());
    }

    #[test]
    fn set_setting_accepts_every_key_the_interface_writes() {
        for key in [
            "startOnBoot",
            "startOnBootAction",
            "launchAction",
            "closeAction",
            "animatedWallpaper",
            "glassEffects",
            "timeFormat",
            "uiScale",
            "showNsfwMods",
            "overlayEnabled",
            "overlayAudioTracks",
            "overlayAudioDiscord",
            "modsEnabled",
            "modsPerPage",
            "downloadSpeedLimit",
            "downloadConnections",
            "downloadWhileGaming",
        ] {
            assert!(setting_may_write(key).is_ok(), "the interface writes {key}");
        }
    }

    #[test]
    fn set_setting_refuses_state_with_its_own_channel() {
        for key in [
            "modsPath",
            "overlayCaptureFolder",
            "activeGameId",
            "overlayHotkey",
            "overlayShotHotkey",
            "overlayRecHotkey",
            "",
            "account.token",
            "games.wuwa.gamePath",
        ] {
            assert!(setting_may_write(key).is_err(), "{key} must not be set-setting writable");
        }
    }

    #[test]
    fn a_behavior_merge_keeps_the_other_keys_and_reports_its_side_effects() {
        let delta = |v: Value| v.as_object().cloned().unwrap();
        let mut behavior = json!({
            "hideSocials": true,
            "rememberWindowState": false
        });
        let turned_on = merge_behavior(&mut behavior, &delta(json!({ "rememberWindowState": true })));
        assert!(turned_on);
        assert_eq!(behavior["hideSocials"], json!(true));
        assert_eq!(behavior["rememberWindowState"], json!(true));
        assert_eq!(behavior["closeAction"], json!("close"));

        let turned_on = merge_behavior(&mut behavior, &delta(json!({ "hideSocials": false })));
        assert!(!turned_on);
        assert_eq!(behavior["rememberWindowState"], json!(true));
        assert_eq!(behavior["hideSocials"], json!(false));
    }

    #[test]
    fn single_key_reads_never_reach_the_account() {
        assert!(!config_key_readable("account"));
        assert!(!config_key_readable("account.token"));
        assert!(config_key_readable("behavior.modsEnabled"));
        assert!(config_key_readable("games.wuwa"));
        assert!(config_key_readable("accountless"));
    }

    #[test]
    fn unknown_games_and_wrong_types_are_refused() {
        assert!(renderer_may_write("games.notagame.launchArgs", &json!("x")).is_err());
        assert!(renderer_may_write("games.wuwa.launchArgs", &json!(42)).is_err());
        assert!(renderer_may_write("games.wuwa.launchViaSteam", &json!("true")).is_err());
        assert!(renderer_may_write("library.visible", &json!([1, 2])).is_err());
        assert!(renderer_may_write("games.wuwa", &json!({})).is_err());
    }
}

#[cfg(test)]
mod media_slice_tests {
    use super::*;

    #[test]
    fn a_default_slice_is_stored_without_a_path_or_extra_fields() {
        let (slice, file) = parse_media_slice(
            "wallpaper",
            &json!({ "type": "default", "path": r"C:\x.png", "junk": [1, 2, 3] }),
        )
        .unwrap();
        assert_eq!(slice, json!({ "type": "default", "path": null }));
        assert_eq!(file, None);
    }

    #[test]
    fn a_custom_slice_keeps_only_its_type_and_path() {
        let (slice, file) = parse_media_slice(
            "wallpaper",
            &json!({ "type": "custom", "path": r"D:\Walls\Night.MP4", "extra": { "big": "x" } }),
        )
        .unwrap();
        assert_eq!(slice, json!({ "type": "custom", "path": r"D:\Walls\Night.MP4" }));
        assert_eq!(file, Some(PathBuf::from(r"D:\Walls\Night.MP4")));
        assert!(
            parse_media_slice("gameIcons", &json!({ "type": "custom", "path": r"C:\i\icon.WebP" }))
                .is_ok()
        );
    }

    #[test]
    fn custom_paths_must_be_absolute_and_a_type_their_picker_offers() {
        for (section, path) in [
            ("wallpaper", json!("")),
            ("wallpaper", json!("wall.png")),
            ("wallpaper", json!(r"C:\Users\me\.ssh\id_rsa")),
            ("wallpaper", json!(r"C:\Users\me\notes.txt")),
            ("gameIcons", json!(r"C:\clips\intro.mp4")),
            ("gameIcons", json!(7)),
            ("gameIcons", Value::Null),
        ] {
            let slice = json!({ "type": "custom", "path": path });
            assert!(parse_media_slice(section, &slice).is_err(), "{section} {slice}");
        }
        assert!(parse_media_slice("wallpaper", &json!({ "type": "custom" })).is_err());
    }

    #[test]
    fn unknown_slice_shapes_are_refused() {
        for slice in [
            json!({}),
            json!({ "type": "remote", "path": null }),
            json!({ "type": 1 }),
            json!("custom"),
            Value::Null,
        ] {
            assert!(parse_media_slice("gameIcons", &slice).is_err(), "{slice}");
        }
    }

    #[test]
    fn only_files_from_the_picker_are_remembered() {
        let picked = PathBuf::from(r"C:\peebify-media-test\picked.png");
        let other = PathBuf::from(r"C:\peebify-media-test\other.png");
        record_picked_media(picked.clone());
        record_picked_media(picked.clone());
        assert!(was_picked(&picked));
        assert!(!was_picked(&other));
        assert_eq!(PICKED_MEDIA.lock().iter().filter(|p| **p == picked).count(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_settings_are_clamped_and_reach_the_transfer_core() {
        let mut merged = json!({ "behavior": {
            "downloadSpeedLimit": "999999",
            "downloadConnections": "lots",
            "downloadWhileGaming": "sometimes",
        }});
        sanitize_behavior(&mut merged);
        let behavior = &merged["behavior"];
        assert_eq!(behavior["downloadSpeedLimit"], json!("10000"));
        assert_eq!(behavior["downloadConnections"], json!("0"));
        assert_eq!(behavior["downloadWhileGaming"], json!("off"));

        let settings = transfer_settings(&json!({
            "downloadSpeedLimit": "12",
            "downloadConnections": "8",
            "downloadWhileGaming": "pause",
        }));
        assert_eq!(settings.speed_limit, 12_000_000);
        assert_eq!(settings.connections, 8);
        assert_eq!(settings.while_gaming, peebify_transfer::WhileGaming::Pause);
        assert_eq!(transfer_settings(&default_behavior()), peebify_transfer::Settings::default());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "peebify-config-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_track_list_is_kept() {
        let mut merged = json!({ "behavior": {
            "overlayAudioTracks": "",
            "closeAction": ""
        } });
        sanitize_behavior(&mut merged);
        assert_eq!(merged["behavior"]["overlayAudioTracks"], json!(""));
        assert_eq!(merged["behavior"]["closeAction"], json!("close"));
    }

    #[test]
    fn a_missing_or_null_track_list_still_heals() {
        let mut merged = json!({ "behavior": { "overlayAudioTracks": "null" } });
        sanitize_behavior(&mut merged);
        assert_eq!(merged["behavior"]["overlayAudioTracks"], json!("desktop,mic"));
    }

    #[test]
    fn a_device_id_only_config_still_shows_the_picker() {
        let dir = scratch("device-only");
        std::fs::write(dir.join(CONFIG_FILE), r#"{"deviceId":"abcd1234"}"#).unwrap();
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.setupComplete"), json!(false));
        assert_eq!(cfg.get("library.visible"), json!([]));
        assert_eq!(cfg.get("deviceId"), json!("abcd1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_config_is_restored_from_the_last_good_copy() {
        let dir = scratch("restore");
        let good = r#"{"deviceId":"abcd1234","library":{"visible":["wuwa"],"setupComplete":true}}"#;
        std::fs::write(dir.join(CONFIG_FILE), good).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        assert!(dir.join(format!("{CONFIG_FILE}.bak")).exists());

        std::fs::write(dir.join(CONFIG_FILE), vec![0u8; 64]).unwrap();
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("deviceId"), json!("abcd1234"));
        assert_eq!(cfg.get("library.visible"), json!(["wuwa"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn corrupt_copies(dir: &Path) -> Vec<PathBuf> {
        let prefix = format!("{CONFIG_FILE}.corrupt-");
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .collect()
    }

    fn on_disk(dir: &Path, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
    }

    const GOOD: &str =
        r#"{"deviceId":"abcd1234","library":{"visible":["wuwa"],"setupComplete":true}}"#;

    #[test]
    fn a_config_that_is_not_utf8_is_set_aside_restored_and_saved_again() {
        let dir = scratch("not-utf8");
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();

        let damaged = b"{\"deviceId\":\"caf\xE9\"}";
        std::fs::write(dir.join(CONFIG_FILE), damaged).unwrap();
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.visible"), json!(["wuwa"]));
        assert!(!cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(on_disk(&dir, CONFIG_FILE)["deviceId"], json!("abcd1234"));
        let kept = corrupt_copies(&dir);
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read(&kept[0]).unwrap(), damaged);
        assert_eq!(cfg.startup_notice().unwrap()["type"], json!("info"));

        drop(cfg);
        assert!(LauncherConfig::load(&dir).startup_notice().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_saved_with_a_byte_order_mark_still_loads_and_is_saved_without_it() {
        let dir = scratch("bom");
        std::fs::write(dir.join(CONFIG_FILE), b"\xEF\xBB\xBF{\"deviceId\":\"abcd1234\"}").unwrap();
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("deviceId"), json!("abcd1234"));
        assert!(cfg.startup_notice().is_none());
        assert!(corrupt_copies(&dir).is_empty());
        assert!(!std::fs::read(dir.join(CONFIG_FILE)).unwrap().starts_with(b"\xEF\xBB\xBF"));
        assert_eq!(on_disk(&dir, CONFIG_FILE)["deviceId"], json!("abcd1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_that_is_not_an_object_counts_as_damaged() {
        let dir = scratch("not-object");
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        std::fs::write(dir.join(CONFIG_FILE), "null").unwrap();
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("deviceId"), json!("abcd1234"));
        assert_eq!(corrupt_copies(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_first_run_has_nothing_to_report() {
        let dir = scratch("first-run");
        let cfg = LauncherConfig::load(&dir);
        assert!(cfg.startup_notice().is_none());
        assert!(dir.join(CONFIG_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_is_restored_from_its_backup_instead_of_replacing_it() {
        let dir = scratch("missing");
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        std::fs::remove_file(dir.join(CONFIG_FILE)).unwrap();

        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.visible"), json!(["wuwa"]));
        assert_eq!(on_disk(&dir, CONFIG_FILE)["deviceId"], json!("abcd1234"));
        assert_eq!(
            on_disk(&dir, &format!("{CONFIG_FILE}.bak"))["deviceId"],
            json!("abcd1234")
        );
        assert_eq!(
            cfg.startup_notice().unwrap()["title"],
            json!("Settings restored from a backup")
        );

        drop(cfg);
        let cfg = LauncherConfig::load(&dir);
        assert!(cfg.startup_notice().is_none());
        assert_eq!(cfg.get("library.visible"), json!(["wuwa"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newer_of_the_tmp_and_bak_copies_is_restored() {
        let dir = scratch("newest-copy");
        let path = dir.join(CONFIG_FILE);
        std::fs::write(sibling(&path, "bak"), r#"{"deviceId":"from-bak"}"#).unwrap();
        std::fs::write(sibling(&path, "tmp"), r#"{"deviceId":"from-tmp"}"#).unwrap();
        let age = |suffix: &str, secs: u64| {
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
            std::fs::File::options()
                .write(true)
                .open(sibling(&path, suffix))
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        age("tmp", 3600);
        assert_eq!(newest_good_copy(&path).unwrap().0, "bak");
        age("bak", 7200);
        assert_eq!(newest_good_copy(&path).unwrap().0, "tmp");
        std::fs::write(sibling(&path, "tmp"), "{\"deviceId\":").unwrap();
        assert_eq!(newest_good_copy(&path).unwrap().0, "bak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    fn hold(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    const NEWER: &str =
        r#"{"deviceId":"abcd1234","library":{"visible":["zzz"],"setupComplete":true}}"#;

    #[cfg(windows)]
    fn load_while_held(name: &str) -> (PathBuf, Arc<LauncherConfig>) {
        let dir = scratch(name);
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        std::fs::write(dir.join(CONFIG_FILE), NEWER).unwrap();

        let holder = hold(&dir.join(CONFIG_FILE));
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.visible"), json!(["wuwa"]));
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(cfg.startup_notice().unwrap()["type"], json!("warning"));
        assert_eq!(cfg.startup_notice().unwrap()["saveBlocked"], json!(true));
        cfg.flush();
        drop(holder);
        assert_eq!(std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(), NEWER);
        assert!(corrupt_copies(&dir).is_empty());
        (dir, cfg)
    }

    #[cfg(windows)]
    #[test]
    fn a_config_held_by_another_program_is_kept_after_it_is_let_go() {
        let (dir, cfg) = load_while_held("held");
        set_path(&mut cfg.data.write(), "window.width", json!(1234));
        cfg.flush();
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert!(cfg.read_failed_kept.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(), NEWER);
        assert!(corrupt_copies(&dir).is_empty());
        assert_eq!(cfg.startup_notice().unwrap()["type"], json!("warning"));

        std::fs::write(dir.join(CONFIG_FILE), "{\"deviceId\":").unwrap();
        cfg.flush();
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert!(corrupt_copies(&dir).is_empty());
        std::fs::write(dir.join(CONFIG_FILE), NEWER).unwrap();

        drop(cfg);
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.visible"), json!(["zzz"]));
        assert!(cfg.startup_notice().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_config_with_no_backup_is_never_saved_over() {
        let dir = scratch("held-no-backup");
        std::fs::write(dir.join(CONFIG_FILE), NEWER).unwrap();
        let holder = hold(&dir.join(CONFIG_FILE));
        let cfg = LauncherConfig::load(&dir);
        assert_eq!(cfg.get("library.setupComplete"), json!(false));
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(cfg.startup_notice().unwrap()["type"], json!("warning"));
        assert_eq!(cfg.startup_notice().unwrap()["saveBlocked"], json!(true));

        drop(holder);
        set_path(&mut cfg.data.write(), "library.setupComplete", json!(true));
        cfg.flush();
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(), NEWER);

        std::fs::write(dir.join(CONFIG_FILE), "{\"deviceId\":").unwrap();
        cfg.flush();
        assert!(cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(),
            "{\"deviceId\":"
        );
        assert!(corrupt_copies(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_config_that_is_gone_once_let_go_is_saved_from_the_restored_copy() {
        let (dir, cfg) = load_while_held("held-gone");
        std::fs::remove_file(dir.join(CONFIG_FILE)).unwrap();
        cfg.flush();
        assert!(!cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(on_disk(&dir, CONFIG_FILE)["library"]["visible"], json!(["wuwa"]));
        assert!(corrupt_copies(&dir).is_empty());
        assert_eq!(cfg.startup_notice().unwrap()["type"], json!("info"));
        assert_eq!(cfg.startup_notice().unwrap()["saveBlocked"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_config_that_is_damaged_once_let_go_is_set_aside() {
        let (dir, cfg) = load_while_held("held-damaged");
        std::fs::write(dir.join(CONFIG_FILE), "{\"deviceId\":").unwrap();
        cfg.flush();
        assert!(!cfg.read_failed.load(Ordering::SeqCst));
        assert_eq!(on_disk(&dir, CONFIG_FILE)["library"]["visible"], json!(["wuwa"]));
        let kept = corrupt_copies(&dir);
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "{\"deviceId\":");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_never_collide_on_the_temp_file() {
        let dir = scratch("concurrent");
        let cfg = LauncherConfig::load(&dir);
        let workers: Vec<_> = (0..8)
            .map(|n| {
                let cfg = Arc::clone(&cfg);
                std::thread::spawn(move || {
                    for i in 0..25 {
                        set_path(&mut cfg.data.write(), "window.width", json!(n * 100 + i));
                        cfg.dirty.store(true, Ordering::SeqCst);
                        cfg.save_now().unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(on_disk["window"]["width"], cfg.get("window.width"));
        assert!(!dir.join(format!("{CONFIG_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_changes_nothing_is_not_saved() {
        let dir = scratch("no-op-write");
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        let cfg = LauncherConfig::load(&dir);
        cfg.flush();
        assert!(!cfg.dirty.load(Ordering::SeqCst));

        assert!(cfg.set("behavior.activeGameId", cfg.get("behavior.activeGameId")));
        assert!(cfg.set("library.visible", json!(["wuwa"])));
        assert!(cfg.update("window", |window| {
            window["width"] = window["width"].clone();
            true
        }));
        assert!(!cfg.dirty.load(Ordering::SeqCst));

        assert!(cfg.update("window", |window| {
            window["width"] = json!(1600);
            true
        }));
        assert!(cfg.dirty.load(Ordering::SeqCst));
        cfg.flush();
        assert_eq!(on_disk(&dir, CONFIG_FILE)["window"]["width"], json!(1600));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_startup_backup_is_replaced_whole_and_skipped_when_unchanged() {
        let dir = scratch("backup");
        let bak = dir.join(format!("{CONFIG_FILE}.bak"));
        std::fs::write(dir.join(CONFIG_FILE), GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), GOOD);
        assert!(!dir.join(format!("{CONFIG_FILE}.bak.tmp")).exists());

        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&bak).unwrap().set_modified(when).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), on_disk(&dir, CONFIG_FILE).to_string()).unwrap();
        let saved = std::fs::read(dir.join(CONFIG_FILE)).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        assert_eq!(std::fs::read(&bak).unwrap(), saved);

        let written = std::fs::metadata(&bak).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(&bak).unwrap().set_modified(when).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        assert_eq!(std::fs::read(&bak).unwrap(), saved);
        assert!(!dir.join(format!("{CONFIG_FILE}.bak.tmp")).exists());
        assert!(std::fs::metadata(&bak).unwrap().modified().unwrap() > when);
        assert!(written > when);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_backup_still_outranks_an_older_leftover_tmp() {
        let dir = scratch("backup-vs-tmp");
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, GOOD).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        std::fs::write(&path, on_disk(&dir, CONFIG_FILE).to_string()).unwrap();
        LauncherConfig::load(&dir).wait_for_backup();
        let bak = sibling(&path, "bak");
        let tmp = sibling(&path, "tmp");
        let held = std::fs::read(&bak).unwrap();
        assert_eq!(held, std::fs::read(&path).unwrap());
        std::fs::write(&tmp, r#"{"deviceId":"from-tmp"}"#).unwrap();
        let date = |file: &Path, secs: u64| {
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
            std::fs::File::options().write(true).open(file).unwrap().set_modified(when).unwrap();
        };
        date(&bak, 7200);
        date(&tmp, 3600);
        assert_eq!(newest_good_copy(&path).unwrap().0, "tmp");

        LauncherConfig::load(&dir).wait_for_backup();
        assert_eq!(std::fs::read(&bak).unwrap(), held);
        assert_eq!(newest_good_copy(&path).unwrap().0, "bak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wipe_keeps_install_locations() {
        let dir = scratch("wipe");
        std::fs::write(
            dir.join(CONFIG_FILE),
            r#"{"games":{"wuwa":{"gamePath":"D:/Games/Wuthering Waves","playtime":{"totalPlaytime":99}}},"library":{"visible":["wuwa"],"setupComplete":true}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::write(dir.join("mods").join("profiles.json"), "{}").unwrap();
        std::fs::write(dir.join("mods").join("library.json"), "{}").unwrap();
        let cfg = LauncherConfig::load(&dir);
        cfg.wipe().unwrap();

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(on_disk["games"]["wuwa"]["gamePath"], json!("D:/Games/Wuthering Waves"));
        assert_eq!(on_disk["games"]["wuwa"]["playtime"]["totalPlaytime"], json!(0));
        assert_eq!(on_disk["library"]["setupComplete"], json!(false));
        assert!(!dir.join(format!("{CONFIG_FILE}.bak")).exists());
        assert!(!dir.join("mods").join("profiles.json").exists());
        assert!(dir.join("mods").join("library.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_on_a_missing_drive_is_kept() {
        let Some(letter) = (b'F'..=b'Z')
            .rev()
            .map(char::from)
            .find(|l| std::fs::metadata(format!("{l}:\\")).is_err())
        else {
            return;
        };
        let dir = scratch("missing-drive");
        let cfg = LauncherConfig::load(&dir);
        let kept = format!("{letter}:\\Games\\Wuthering Waves");
        set_path(&mut cfg.data.write(), "games.wuwa.gamePath", json!(kept));
        assert!(!volume_reachable(&kept));
        assert!(!cfg.sanitize_installed_paths_deferred());
        assert_eq!(cfg.get("games.wuwa.gamePath"), json!(kept));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_game_with_an_unfinished_move_keeps_its_path_for_recovery() {
        let dir = scratch("pending-move");
        let cfg = LauncherConfig::load(&dir);
        let from = dir.join("old").to_string_lossy().into_owned();
        let to = dir.join("new").to_string_lossy().into_owned();
        set_path(&mut cfg.data.write(), "games.wuwa.gamePath", json!(from));
        set_path(
            &mut cfg.data.write(),
            "games.wuwa.pendingMove",
            json!({ "from": from, "to": to }),
        );
        assert!(!cfg.sanitize_installed_paths_deferred());
        assert_eq!(cfg.get("games.wuwa.gamePath"), json!(from));

        set_path(&mut cfg.data.write(), "games.wuwa.pendingMove", Value::Null);
        assert!(cfg.sanitize_installed_paths_deferred());
        assert_eq!(cfg.get("games.wuwa.gamePath"), json!(""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_behavior_defaults_are_filled() {
        let mut merged = json!({
            "behavior": {
                "closeAction": "tray",
                "showNsfwMods": "yes"
            }
        });
        sanitize_behavior(&mut merged);
        let behavior = merged["behavior"].as_object().unwrap();
        assert_eq!(behavior["closeAction"], json!("tray"));
        assert_eq!(behavior["showNsfwMods"], json!(false));
        assert_eq!(behavior["rememberWindowState"], json!(true));

        let mut fresh = json!({});
        sanitize_behavior(&mut fresh);
        assert_eq!(fresh["behavior"]["closeAction"], json!("close"));
    }

    #[test]
    fn graphics_api_choice_follows_the_profiles() {
        let mut merged = json!({
            "games": {
                "wuwa": {},
                "zzz": { "graphicsApi": "dx12" },
                "pgr": { "graphicsApi": "vulkan" },
                "hsr": { "graphicsApi": "dx12" }
            }
        });
        heal_game_settings(&mut merged);
        assert_eq!(merged["games"]["wuwa"]["graphicsApi"], json!("dx12"));
        assert_eq!(merged["games"]["zzz"]["graphicsApi"], json!("dx12"));
        assert_eq!(merged["games"]["pgr"]["graphicsApi"], json!("dx11"));
        assert!(merged["games"]["hsr"].get("graphicsApi").is_none());
    }

    #[test]
    fn resource_quality_choice_follows_the_profiles() {
        let mut merged = json!({
            "games": {
                "wuwa": { "resourceQuality": "8k" },
                "zzz": { "resourceQuality": "uhd" }
            }
        });
        heal_game_settings(&mut merged);
        assert_eq!(merged["games"]["wuwa"]["resourceQuality"], json!("hd"));
        assert!(merged["games"]["zzz"].get("resourceQuality").is_none());

        merged["games"]["wuwa"]["resourceQuality"] = json!("uhd");
        heal_game_settings(&mut merged);
        assert_eq!(merged["games"]["wuwa"]["resourceQuality"], json!("uhd"));
    }

    #[test]
    fn webui_game_table_agrees_with_the_profiles() {
        let source = include_str!("../../../webui/src/data/games.ts");
        let blocks: Vec<&str> = source.split("\n    id: \"").skip(1).collect();
        let mut ids = Vec::new();
        for block in blocks {
            let id = &block[..block.find('"').unwrap()];
            ids.push(id.to_string());
            let profile = game_profiles::profile(id);
            let flag = |text: &str| block.contains(text);
            assert_eq!(
                !flag("managed: false"),
                game_profiles::is_managed(profile),
                "{id} managed"
            );
            let api = graphics_api_default(profile);
            assert_eq!(flag("graphicsApiChoice: true"), api.is_some(), "{id} graphicsApiChoice");
            if let Some(api) = api {
                let ts_default = if flag("graphicsApiDefault: \"dx12\"") { "dx12" } else { "dx11" };
                assert_eq!(ts_default, api, "{id} graphicsApiDefault");
            }
            assert_eq!(
                flag("resourceQualityChoice: true"),
                game_profiles::resource_quality_default(profile).is_some(),
                "{id} resourceQualityChoice"
            );
            assert_eq!(
                flag("voicePackChoice: true"),
                game_profiles::install_mode(profile) == Some("sophon"),
                "{id} voicePackChoice"
            );
            assert_eq!(
                flag("fpsUnlock: true"),
                profile.get("fpsUnlock") == Some(&Value::Bool(true)),
                "{id} fpsUnlock"
            );
        }
        let mut expected: Vec<String> = GAME_IDS.iter().map(|id| id.to_string()).collect();
        expected.sort();
        ids.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn install_folder_names_match_the_install_modal() {
        let modal = include_str!("../../../webui/src/components/ui/InstallModalBody.tsx");
        let start = modal.find("const INVALID_PATH_CHARS = /[").unwrap() + "const INVALID_PATH_CHARS = /[".len();
        let end = start + modal[start..].find("]/g;").unwrap();
        let mut invalid = Vec::new();
        let mut chars = modal[start..end].chars();
        while let Some(c) = chars.next() {
            invalid.push(if c == '\u{5c}' { chars.next().unwrap() } else { c });
        }
        let ts_folder = |name: &str| -> String {
            name.chars()
                .filter(|c| !invalid.contains(c))
                .collect::<String>()
                .trim()
                .to_string()
        };
        let rust_folder = |profile: &Value| -> String {
            let dirs = super::super::game_profiles::default_install_dirs(profile);
            let (_, dir) = dirs.last().unwrap();
            dir.file_name().unwrap().to_string_lossy().to_string()
        };

        let games = include_str!("../../../webui/src/data/games.ts");
        for block in games.split("\n    id: \"").skip(1) {
            let id = &block[..block.find('"').unwrap()];
            let name_start = block.find("\n    name: \"").unwrap() + "\n    name: \"".len();
            let name = &block[name_start..name_start + block[name_start..].find('"').unwrap()];
            let profile = game_profiles::profile(id);
            assert_eq!(game_profiles::display_name(profile), name, "{id} name");
            assert_eq!(rust_folder(profile), ts_folder(name), "{id} folder");
        }

        for name in [" Odd<Game>: Part/2\u{5c}Final|Cut?* ", "A \"Quoted\" Title", "Plain Name"] {
            let profile = json!({ "displayName": name });
            assert_eq!(rust_folder(&profile), ts_folder(name), "{name}");
        }
    }
}
