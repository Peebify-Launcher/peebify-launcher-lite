// ------------ Mod Library ------------
// Everything about installed mods for the games that support them: the mods folder, importing zip/7z/rar archives,
// turning mods on and off, replacing and deleting them, and installing the mod loader (the toolchain) itself.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use super::fs_util::{self, dialog::Pick};
use super::BackendState;
use super::{arg_str, err_response, game_profiles, ok_response, ok_with, resolve_profile, xxmi};

const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

type ProgressMarks = std::collections::BTreeMap<String, (std::time::Instant, String)>;

static LAST_PROGRESS: parking_lot::Mutex<ProgressMarks> =
    parking_lot::Mutex::new(std::collections::BTreeMap::new());

fn progress_due(
    last: &mut ProgressMarks,
    game_id: &str,
    message: &str,
    done: bool,
    now: std::time::Instant,
) -> bool {
    if done {
        last.remove(game_id);
        return true;
    }
    if last
        .get(game_id)
        .is_some_and(|(t, m)| m == message && now.duration_since(*t) < PROGRESS_INTERVAL)
    {
        return false;
    }
    last.insert(game_id.to_string(), (now, message.to_string()));
    true
}

pub(crate) fn publish_progress(app: &AppHandle, game_id: &str, message: &str, percent: f64) {
    let done = !(0.0..100.0).contains(&percent);

    if !progress_due(
        &mut LAST_PROGRESS.lock(),
        game_id,
        message,
        done,
        std::time::Instant::now(),
    ) {
        return;
    }

    let _ = app.emit(
        "mods-progress",
        json!({
            "gameId": game_id,
            "message": message,
            "percentage": percent,
            "done": done,
            "failed": percent < 0.0,
        }),
    );
}

pub(crate) fn finish_progress(app: &AppHandle, game_id: &str, failed: bool) {
    publish_progress(app, game_id, "", if failed { -1.0 } else { 100.0 });
}

pub(super) fn notify_mods_changed(app: &AppHandle, game_id: &str) {
    let _ = app.emit("mods-status-changed", json!({ "gameId": game_id }));
}

// ------------ Mod Folders and Library File ------------
// A disabled mod is just a folder renamed with a DISABLED prefix. Each mod also gets a stable id, and extra
// details (names, sizes, thumbnails, where it came from) are kept in library.json next to the mods.
const DISABLED_PREFIX: &str = "DISABLED ";

const DISABLED_MARK: &str = "DISABLED";

const LIBRARY_FILE: &str = "library.json";

const ROOTS_KEY: &str = "_roots";

pub(crate) const PARKED_KEY: &str = "_parked";

const REPLACED_PREFIX: &str = ".peebify-old-";

const SPENT_PREFIX: &str = ".peebify-spent-";

const MAX_UNPACKED_BYTES: u64 = 30 << 30;

const UNPACK_HEADROOM: u64 = 1 << 30;

fn mods_root(app: &AppHandle) -> PathBuf {
    let state = app.state::<BackendState>();
    state.user_data.join("mods")
}

pub(crate) fn toolchain_root(app: &AppHandle) -> PathBuf {
    let state = app.state::<BackendState>();
    xxmi::root(&state.config, &state.user_data)
}

pub fn master_enabled(app: &AppHandle) -> bool {
    app.state::<BackendState>()
        .config
        .get("behavior.modsEnabled")
        == Value::Bool(true)
}

pub fn active_for(app: &AppHandle, profile_id: &str, profile: &Value) -> bool {
    let Some(variant) = game_profiles::mod_variant(profile) else {
        return false;
    };
    if !master_enabled(app) {
        return false;
    }
    if app
        .state::<BackendState>()
        .config
        .get(&format!("games.{profile_id}.modsEnabled"))
        != Value::Bool(true)
    {
        return false;
    }
    xxmi::variant_installed(&toolchain_root(app), variant)
}

pub(crate) fn variant_for(profile: &Value) -> Result<String, String> {
    game_profiles::mod_variant(profile)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "Peebify has no mod loader for {}, so it can't use mods.",
                game_profiles::display_name(profile)
            )
        })
}

pub(crate) fn is_disabled_folder(name: &str) -> bool {
    name.get(..DISABLED_MARK.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(DISABLED_MARK))
}

pub(crate) fn display_name_of(folder: &str) -> &str {
    if is_disabled_folder(folder) {
        folder[DISABLED_MARK.len()..].trim_start_matches([' ', '_', '-'])
    } else {
        folder
    }
}

fn disabled_name(display: &str) -> String {
    format!("{DISABLED_PREFIX}{display}")
}

fn derived_mod_id(game_id: &str, display: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(game_id.to_lowercase().as_bytes());
    hasher.update([0u8]);
    hasher.update(display.to_lowercase().as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_sha1_bytes(bytes)
        .into_uuid()
        .to_string()
}

fn folder_mod_id(game_id: &str, folder: &str, attempt: u32) -> String {
    derived_mod_id(game_id, &format!("\u{0}folder\u{0}{folder}\u{0}{attempt}"))
}

fn stored_mod_id(meta: Option<&Value>) -> Option<&str> {
    meta.and_then(|m| m.get("modId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(crate) fn mod_id_of(game_id: &str, folder: &str, meta: Option<&Value>) -> String {
    stored_mod_id(meta)
        .map(str::to_string)
        .unwrap_or_else(|| derived_mod_id(game_id, display_name_of(folder)))
}

fn sanitize_folder_name(name: &str) -> String {
    let safe = super::fs_util::sanitize_folder_name(name);
    if safe.is_empty() {
        "Mod".to_string()
    } else {
        safe
    }
}

fn claim_folder(mods_dir: &Path, desired: &str) -> Result<String, String> {
    let shown: std::collections::HashSet<String> = std::fs::read_dir(mods_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| display_name_of(&e.file_name().to_string_lossy()).to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let taken = |name: &str| {
        mods_dir.join(name).exists()
            || mods_dir.join(disabled_name(name)).exists()
            || shown.contains(&display_name_of(name).to_lowercase())
    };
    let candidates = std::iter::once(desired.to_string())
        .chain((2..1000).map(|n| format!("{desired} ({n})")))
        .chain(std::iter::once(format!(
            "{desired} ({})",
            chrono::Utc::now().timestamp()
        )));
    for candidate in candidates {
        if taken(&candidate) {
            continue;
        }
        match std::fs::create_dir(mods_dir.join(&candidate)) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not create the folder for {candidate}: {e}")),
        }
    }
    Err(format!("There's no free folder name left for {desired}."))
}

fn library_path(app: &AppHandle) -> PathBuf {
    mods_root(app).join(LIBRARY_FILE)
}

fn library_root_key(app: &AppHandle) -> String {
    fs_util::normalize_path(&toolchain_root(app).to_string_lossy())
}

fn load_claimed_library(app: &AppHandle) -> Result<(Value, bool), String> {
    let mut library = super::fs_util::read_json_store(&library_path(app))
        .map(|stored| stored.filter(Value::is_object).unwrap_or_else(|| json!({})))?;
    let claimed = claim_root(&mut library, &library_root_key(app));
    Ok((library, claimed))
}

pub(crate) fn load_library(app: &AppHandle) -> Result<Value, String> {
    load_claimed_library(app).map(|(library, _)| library)
}

fn read_library(app: &AppHandle) -> Value {
    load_library(app).unwrap_or_else(|e| {
        log::warn!("mods: {e}, so the mod library reads as empty");
        json!({})
    })
}

fn object_at<'a>(
    map: &'a mut serde_json::Map<String, Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, Value> {
    let slot = map.entry(key).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    match slot {
        Value::Object(inner) => inner,
        _ => unreachable!("the slot was just made an object"),
    }
}

fn claim_root(library: &mut Value, root: &str) -> bool {
    let mut changed = false;
    for game_id in game_profiles::GAME_IDS {
        changed |= claim_game(library, game_id, root);
    }
    changed
}

fn claim_game(library: &mut Value, game_id: &str, root: &str) -> bool {
    let Some(top) = library.as_object_mut() else {
        return false;
    };
    let roots = object_at(top, ROOTS_KEY);
    let recorded = roots.get(game_id).and_then(Value::as_str).map(str::to_string);
    if recorded.as_deref() == Some(root) {
        return false;
    }
    roots.insert(game_id.to_string(), json!(root));
    let Some(recorded) = recorded else {
        return true;
    };

    let outgoing = top
        .remove(game_id)
        .filter(|entries| entries.as_object().is_some_and(|m| !m.is_empty()));
    let parked_games = object_at(top, PARKED_KEY);
    let parked = object_at(parked_games, game_id);
    let incoming = parked.remove(root).filter(Value::is_object);
    if let Some(outgoing) = outgoing {
        parked.insert(recorded.clone(), outgoing);
    }
    if parked.is_empty() {
        parked_games.remove(game_id);
    }
    if parked_games.is_empty() {
        top.remove(PARKED_KEY);
    }
    if let Some(incoming) = incoming {
        top.insert(game_id.to_string(), incoming);
    }
    log::info!("mods: the {game_id} library now follows {root} (was {recorded})");
    true
}

fn pin_library_root(app: &AppHandle) {
    let _guard = LIBRARY_LOCK.lock();
    match load_claimed_library(app) {
        Ok((library, true)) => {
            if let Err(e) = write_library(app, &library) {
                log::warn!("mods: could not record the library's tools folder: {e}");
            }
        }
        Ok((_, false)) => {}
        Err(e) => log::warn!("mods: could not record the library's tools folder: {e}"),
    }
}

pub(crate) static LIBRARY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub(crate) fn write_library(app: &AppHandle, library: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(library).map_err(|e| e.to_string())?;
    super::fs_util::write_atomic(&mods_root(app).join(LIBRARY_FILE), body.as_bytes())?;
    SIZING_FAILED.lock().clear();
    Ok(())
}

fn update_library(app: &AppHandle, what: &str, edit: impl FnOnce(&mut Value) -> bool) {
    let _guard = LIBRARY_LOCK.lock();
    update_library_locked(app, what, edit);
}

fn update_library_locked(app: &AppHandle, what: &str, edit: impl FnOnce(&mut Value) -> bool) {
    let mut library = match load_library(app) {
        Ok(library) => library,
        Err(e) => {
            log::warn!("mods: could not {what}: {e}");
            return;
        }
    };
    if edit(&mut library) {
        if let Err(e) = write_library(app, &library) {
            log::warn!("mods: could not {what}: {e}");
        }
    }
}

fn record_metadata(app: &AppHandle, game_id: &str, folder: &str, meta: Value) {
    update_library(app, &format!("record metadata for {folder}"), |library| {
        if !library[game_id].is_object() {
            library[game_id] = json!({});
        }
        library[game_id][folder] = meta;
        true
    });
}

fn rename_metadata_locked(app: &AppHandle, game_id: &str, moves: &[Toggled]) {
    if moves.is_empty() {
        return;
    }
    update_library_locked(app, &format!("re-key metadata for {game_id}"), |library| {
        let Some(map) = library[game_id].as_object_mut() else {
            return false;
        };
        let mut changed = false;
        for m in moves {
            if let Some(entry) = map.remove(&m.from) {
                map.insert(m.to.clone(), entry);
                changed = true;
            }
        }
        changed
    });
}

fn forget_metadata(app: &AppHandle, game_id: &str, folder: &str) {
    update_library(app, &format!("forget the metadata of {folder}"), |library| {
        library[game_id]
            .as_object_mut()
            .is_some_and(|map| map.remove(folder).is_some())
    });
}

const THUMBNAIL_CHECKED_KEY: &str = "thumbnailCheckedAt";

const THUMBNAIL_RECHECK_SECS: i64 = 7 * 24 * 60 * 60;

pub(super) fn mods_missing_thumbnails(app: &AppHandle, game_id: &str) -> Vec<(String, u64)> {
    missing_thumbnails_in(&read_library(app), game_id, chrono::Utc::now().timestamp())
}

fn missing_thumbnails_in(library: &Value, game_id: &str, now: i64) -> Vec<(String, u64)> {
    let Some(map) = library[game_id].as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(folder, meta)| {
            let has_thumbnail = meta
                .get("thumbnailUrl")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if has_thumbnail {
                return None;
            }
            let checked_recently = meta
                .get(THUMBNAIL_CHECKED_KEY)
                .and_then(Value::as_i64)
                .is_some_and(|at| (0..THUMBNAIL_RECHECK_SECS).contains(&now.saturating_sub(at)));
            if checked_recently {
                return None;
            }
            let source = meta.get("source")?;
            if source.get("kind").and_then(Value::as_str) != Some("gamebanana") {
                return None;
            }
            let mod_id = source.get("gbModId").and_then(Value::as_u64)?;
            Some((folder.clone(), mod_id))
        })
        .collect()
}

pub(super) fn set_mod_thumbnails(
    app: &AppHandle,
    game_id: &str,
    thumbnails: &[(String, Option<String>)],
) {
    if thumbnails.is_empty() {
        return;
    }
    update_library(app, &format!("save the thumbnails for {game_id}"), |library| {
        apply_thumbnails(library, game_id, thumbnails, chrono::Utc::now().timestamp())
    });
}

fn apply_thumbnails(
    library: &mut Value,
    game_id: &str,
    thumbnails: &[(String, Option<String>)],
    now: i64,
) -> bool {
    let Some(map) = library[game_id].as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for (folder, url) in thumbnails {
        let Some(entry) = map.get_mut(folder).and_then(Value::as_object_mut) else {
            continue;
        };
        match url {
            Some(url) => {
                entry.insert("thumbnailUrl".to_string(), json!(url));
                entry.remove(THUMBNAIL_CHECKED_KEY);
            }
            None => {
                entry.insert(THUMBNAIL_CHECKED_KEY.to_string(), json!(now));
            }
        }
        changed = true;
    }
    changed
}

fn size_of_meta(meta: Option<&Value>) -> Option<u64> {
    meta?.get("sizeBytes")?.as_u64()
}

fn mod_view_json(game_id: &str, folder: &str, meta: Option<&Value>) -> Value {
    let display = display_name_of(folder).to_string();
    json!({
        "modId": mod_id_of(game_id, folder, meta),
        "folderName": folder,
        "name": meta
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(&display),
        "enabled": !is_disabled_folder(folder),
        "sizeBytes": size_of_meta(meta),
        "source": meta
            .and_then(|m| m.get("source"))
            .cloned()
            .unwrap_or_else(|| json!({ "kind": "manual" })),
        "version": meta.and_then(|m| m.get("version")).cloned().unwrap_or(Value::Null),
        "installedAt": meta.and_then(|m| m.get("installedAt")).cloned().unwrap_or(Value::Null),
        "thumbnailUrl": meta.and_then(|m| m.get("thumbnailUrl")).cloned().unwrap_or(Value::Null),
    })
}

// ------------ Scanning the Mods Folder ------------
// Reads what is actually on disk, matches it with the library file, fixes ids and renamed folders, and
// measures folder sizes in the background so the list shows up quickly.
struct Scan {
    mods: Vec<Value>,
    changed: bool,
    unmeasured: Vec<String>,
    unreadable: Option<String>,
}

fn list_mod_folders(mods_dir: &Path) -> (Vec<String>, bool, Option<String>) {
    let entries = match std::fs::read_dir(mods_dir) {
        Ok(entries) => entries,
        Err(e) => {
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            let variant_present = mods_dir.parent().is_some_and(Path::is_dir);
            let removed = missing && variant_present;
            if !removed {
                log::warn!(
                    "mods: cannot read {} ({e}), so the library is left as it is",
                    mods_dir.display()
                );
            }
            return (Vec::new(), removed, (!missing).then(|| e.to_string()));
        }
    };
    let mut complete = true;
    let mut folders = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                if entry.path().is_dir() {
                    folders.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            Err(e) => {
                log::warn!(
                    "mods: an entry in {} could not be read ({e}), so nothing is pruned this time",
                    mods_dir.display()
                );
                complete = false;
            }
        }
    }
    (folders, complete, None)
}

fn is_placeholder_entry(entry: &Value) -> bool {
    entry.as_object().is_some_and(|map| {
        map.keys()
            .all(|k| matches!(k.as_str(), "modId" | "source" | "sizeBytes"))
    }) && entry["source"]["kind"].as_str().unwrap_or("manual") == "manual"
}

fn rekey_renamed(map: &mut serde_json::Map<String, Value>, folders: &[String]) -> bool {
    let present: std::collections::HashSet<&str> = folders.iter().map(String::as_str).collect();
    let orphans: Vec<String> = map
        .keys()
        .filter(|key| !present.contains(key.as_str()))
        .cloned()
        .collect();
    if orphans.is_empty() {
        return false;
    }
    let key = |folder: &str| display_name_of(folder).to_ascii_lowercase();
    let mut by_name: std::collections::HashMap<String, Vec<&String>> =
        std::collections::HashMap::new();
    for folder in folders {
        by_name.entry(key(folder)).or_default().push(folder);
    }
    let mut orphan_names: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for orphan in &orphans {
        *orphan_names.entry(key(orphan)).or_default() += 1;
    }
    let mut changed = false;
    for orphan in &orphans {
        let name = key(orphan);
        if orphan_names.get(&name) != Some(&1) {
            continue;
        }
        let Some(&[target]) = by_name.get(&name).map(Vec::as_slice) else {
            continue;
        };
        let orphan_id = stored_mod_id(map.get(orphan));
        if map.get(target).is_some_and(|entry| {
            !is_placeholder_entry(entry)
                || stored_mod_id(Some(entry)).is_some_and(|id| Some(id) != orphan_id)
        }) {
            continue;
        }
        if let Some(entry) = map.remove(orphan) {
            log::info!("mods: \"{orphan}\" was renamed to \"{target}\" outside Peebify, so its details moved with it");
            map.insert(target.clone(), entry);
            changed = true;
        }
    }
    changed
}

fn assign_unique_ids(
    game_id: &str,
    map: &mut serde_json::Map<String, Value>,
    folders: &[String],
    listed: bool,
) -> bool {
    let mut used: std::collections::HashSet<String> = if listed {
        std::collections::HashSet::new()
    } else {
        let present: std::collections::HashSet<&str> =
            folders.iter().map(String::as_str).collect();
        map.iter()
            .filter(|(folder, _)| !present.contains(folder.as_str()))
            .filter_map(|(_, meta)| stored_mod_id(Some(meta)).map(str::to_string))
            .collect()
    };
    let mut order: Vec<&String> = folders.iter().collect();
    order.sort_by_cached_key(|folder| {
        let meta = map.get(*folder);
        (
            stored_mod_id(meta).is_none(),
            meta.is_none_or(|m| m["source"]["kind"].as_str() != Some("gamebanana")),
            is_disabled_folder(folder),
            folder.to_lowercase(),
        )
    });

    let mut changed = false;
    for folder in order {
        let stored = stored_mod_id(map.get(folder)).map(str::to_string);
        let wanted = stored
            .clone()
            .unwrap_or_else(|| derived_mod_id(game_id, display_name_of(folder)));
        let id = if used.contains(&wanted) {
            (0u32..)
                .map(|attempt| folder_mod_id(game_id, folder, attempt))
                .find(|id| !used.contains(id))
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        } else {
            wanted
        };
        if stored.as_deref() != Some(id.as_str()) {
            let entry = map
                .entry(folder.clone())
                .or_insert_with(|| json!({ "source": { "kind": "manual" } }));
            if !entry.is_object() {
                *entry = json!({ "source": { "kind": "manual" } });
            }
            entry["modId"] = Value::String(id.clone());
            changed = true;
        }
        used.insert(id);
    }
    changed
}

fn scan_and_reconcile(game_id: &str, root: &str, mods_dir: &Path, library: &mut Value) -> Scan {
    let (folders, listed, unreadable) = list_mod_folders(mods_dir);

    let mut changed = claim_game(library, game_id, root);
    let mut unmeasured = Vec::new();
    let mut mods = Vec::with_capacity(folders.len());

    if !folders.is_empty() && !library[game_id].is_object() {
        library[game_id] = json!({});
        changed = true;
    }
    if let Some(map) = library[game_id].as_object_mut() {
        if listed && rekey_renamed(map, &folders) {
            changed = true;
        }
        changed |= assign_unique_ids(game_id, map, &folders, listed);
        for folder in &folders {
            let meta = map.get(folder);
            if size_of_meta(meta).is_none() {
                unmeasured.push(folder.clone());
            }
            mods.push(mod_view_json(game_id, folder, meta));
        }
        let before = map.len();
        if listed {
            let present: std::collections::HashSet<&str> =
                folders.iter().map(String::as_str).collect();
            map.retain(|folder, _| present.contains(folder.as_str()));
        }
        if map.len() != before {
            changed = true;
        }
    }

    mods.sort_by(|a, b| {
        let name = |v: &Value| v["name"].as_str().unwrap_or_default().to_lowercase();
        name(a).cmp(&name(b))
    });
    Scan {
        mods,
        changed,
        unmeasured,
        unreadable,
    }
}

fn scan_mods(app: &AppHandle, game_id: &str, mods_dir: &Path) -> Vec<Value> {
    let mut library = read_library(app);
    scan_and_reconcile(game_id, &library_root_key(app), mods_dir, &mut library).mods
}

static SIZING: parking_lot::Mutex<Option<std::collections::HashSet<String>>> =
    parking_lot::Mutex::new(None);

static SIZING_FAILED: parking_lot::Mutex<std::collections::BTreeSet<String>> =
    parking_lot::Mutex::new(std::collections::BTreeSet::new());

fn fill_sizes_in_background(app: &AppHandle, game_id: &str, mods_dir: PathBuf, folders: Vec<String>) {
    if SIZING_FAILED.lock().contains(game_id) {
        return;
    }
    {
        let mut sizing = SIZING.lock();
        let set = sizing.get_or_insert_with(std::collections::HashSet::new);
        if !set.insert(game_id.to_string()) {
            return;
        }
    }
    let app = app.clone();
    let game_id = game_id.to_string();
    let root = library_root_key(&app);
    tauri::async_runtime::spawn(async move {
        let walked = folders.clone();
        let sizes: Vec<(String, u64)> = tauri::async_runtime::spawn_blocking(move || {
            walked
                .iter()
                .map(|folder| (folder.clone(), super::fs_util::directory_size(&mods_dir.join(folder))))
                .collect()
        })
        .await
        .unwrap_or_default();

        let mut stale = false;
        let saved = 'save: {
            let _guard = LIBRARY_LOCK.lock();
            if library_root_key(&app) != root {
                log::info!("mods: the tools folder changed while sizing {game_id}'s mods, so the sizes are dropped");
                stale = true;
                break 'save false;
            }
            let mut library = match load_library(&app) {
                Ok(library) => library,
                Err(e) => {
                    log::warn!("mods: could not record mod sizes for {game_id}: {e}");
                    break 'save false;
                }
            };
            if !library[game_id.as_str()].is_object() {
                library[game_id.as_str()] = json!({});
            }
            if let Some(map) = library[game_id.as_str()].as_object_mut() {
                for (folder, size) in sizes {
                    match map.get_mut(&folder).and_then(Value::as_object_mut) {
                        Some(entry) => {
                            entry.insert("sizeBytes".to_string(), json!(size));
                        }
                        None => {
                            map.insert(
                                folder.clone(),
                                json!({
                                    "source": { "kind": "manual" },
                                    "sizeBytes": size,
                                }),
                            );
                        }
                    }
                }
            }
            match write_library(&app, &library) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("mods: could not record mod sizes for {game_id}: {e}");
                    false
                }
            }
        };
        if !saved && !stale {
            SIZING_FAILED.lock().insert(game_id.clone());
        }
        if let Some(set) = SIZING.lock().as_mut() {
            set.remove(&game_id);
        }
        if saved {
            let _ = app.emit("mods-status-changed", json!({ "gameId": game_id }));
        }
    });
}

// ------------ Importing Mod Archives ------------
// Finds the real mod folders inside an archive, checks there is room to unpack, extracts it and moves the result
// into the mods folder. Installs for the same game are queued so two imports never collide.
const ROGUE_INI_MARKERS: [&str; 4] = ["[loader", "[system", "[stereo", "include_recursive"];

fn rogue_config_in(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("ini"))
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.eq_ignore_ascii_case("d3dx.ini") || name.eq_ignore_ascii_case("d3dx_user.ini") {
            return Some(name);
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lower = text.to_lowercase();
        if ROGUE_INI_MARKERS.iter().any(|m| lower.contains(m)) {
            return Some(name);
        }
    }
    None
}

fn find_mod_roots(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<_> = entries
        .flatten()
        .filter(|e| !is_mac_metadata(&e.file_name().to_string_lossy()))
        .collect();

    let has_ini = entries.iter().any(|e| {
        e.path().is_file()
            && e.path()
                .extension()
                .map(|x| x.eq_ignore_ascii_case("ini"))
                .unwrap_or(false)
    });
    if has_ini {
        found.push(dir.to_path_buf());
        return;
    }
    for entry in entries.iter().filter(|e| e.path().is_dir()) {
        find_mod_roots(&entry.path(), depth + 1, found);
    }
}

fn is_mac_metadata(name: &str) -> bool {
    name.eq_ignore_ascii_case("__MACOSX") || name.starts_with("._")
}

fn remove_mac_metadata(dir: &Path, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if is_mac_metadata(&entry.file_name().to_string_lossy()) {
            let removed = if kind.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(e) = removed {
                log::warn!("mods: could not remove macOS metadata {} ({e})", path.display());
            }
        } else if kind.is_dir() && !kind.is_symlink() {
            remove_mac_metadata(&path, depth + 1);
        }
    }
}

fn place_mod(source: &Path, destination: &Path) -> Result<(), String> {
    let mut moved = Vec::new();
    if let Err(e) = move_contents(source, destination, &mut moved) {
        for name in moved.iter().rev() {
            let _ = std::fs::rename(destination.join(name), source.join(name));
        }
        let _ = std::fs::remove_dir_all(destination);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(source);
    Ok(())
}

fn move_contents(
    source: &Path,
    destination: &Path,
    moved: &mut Vec<std::ffi::OsString>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(source).map_err(|e| format!("Could not read {source:?}: {e}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let target = destination.join(&name);
        if std::fs::rename(entry.path(), &target).is_ok() {
            moved.push(name);
        } else if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("Could not copy {name:?}: {e}"))?;
        }
    }
    Ok(())
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir(destination)
        .map_err(|e| format!("Could not create {destination:?}: {e}"))?;
    let entries =
        std::fs::read_dir(source).map_err(|e| format!("Could not read {source:?}: {e}"))?;
    for entry in entries.flatten() {
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("Could not copy {:?}: {e}", entry.file_name()))?;
        }
    }
    Ok(())
}

pub(super) fn prune_stale_staging(dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if !stale {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => log::info!("Removed abandoned staging entry {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Could not remove staging entry {}: {e}", path.display()),
        }
    }
}

const REPLACED_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

const MAX_UNPACKED_ENTRIES: u64 = 200_000;

pub(super) fn sweep_replaced_copies(root: &Path) {
    let variants: std::collections::BTreeSet<&str> = game_profiles::GAME_IDS
        .iter()
        .filter_map(|id| game_profiles::mod_variant(game_profiles::profile(id)))
        .collect();
    for variant in variants {
        sweep_replaced_in(&xxmi::mods_dir(root, variant), std::time::Duration::ZERO);
    }
}

fn replaced_copy_name(folder: &str, stamp_ms: i64) -> String {
    format!("{REPLACED_PREFIX}{stamp_ms}-{folder}")
}

fn sweep_replaced_in(mods_dir: &Path, max_age: std::time::Duration) {
    let Some(holder) = mods_dir.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(holder) else {
        return;
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let max_age_ms = i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(SPENT_PREFIX) {
            let path = entry.path();
            match remove_dir_with_retry(&path) {
                Ok(()) => log::info!("mods: removed {}", path.display()),
                Err(e) => log::warn!("mods: could not remove {} ({e})", path.display()),
            }
            continue;
        }
        let Some((stamp, folder)) = name
            .strip_prefix(REPLACED_PREFIX)
            .and_then(|rest| rest.split_once('-'))
        else {
            continue;
        };
        let Ok(stamp) = stamp.parse::<i64>() else {
            continue;
        };
        if now_ms.saturating_sub(stamp) < max_age_ms {
            continue;
        }
        let path = entry.path();
        let original = mods_dir.join(folder);
        if is_valid_mod_folder(folder) && mods_dir.is_dir() && !original.exists() {
            match rename_with_retry(&path, &original) {
                Ok(()) => log::info!("mods: put \"{folder}\" back after an update that did not finish"),
                Err(e) => log::warn!("mods: could not put \"{folder}\" back from {} ({e})", path.display()),
            }
            continue;
        }
        match remove_dir_with_retry(&path) {
            Ok(()) => log::info!("mods: removed the replaced copy of \"{folder}\""),
            Err(e) => log::warn!("mods: could not remove {} ({e})", path.display()),
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Unpacked {
    bytes: u64,
    entries: u64,
    listing: bool,
}

impl Unpacked {
    fn read_line(&mut self, line: &str) {
        let line = line.trim_end();
        if line.starts_with("----------") {
            self.listing = true;
            return;
        }
        if !self.listing {
            return;
        }
        if line.starts_with("Path = ") {
            self.entries += 1;
        } else if let Some(size) = line.strip_prefix("Size = ") {
            self.bytes = self.bytes.saturating_add(size.trim().parse().unwrap_or(0));
        }
    }
}

async fn archive_unpacked_size(seven_zip: &Path, archive: &Path) -> Result<Unpacked, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut cmd = tokio::process::Command::new(seven_zip);
    cmd.arg("l")
        .arg("-slt")
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.creation_flags(0x0800_0000);

    let mut child = cmd.spawn().map_err(|e| format!("7z spawn failed: {e}"))?;
    let stdout = child.stdout.take().ok_or("failed to capture 7z stdout")?;
    let mut stderr_pipe = child.stderr.take().ok_or("failed to capture 7z stderr")?;
    let stderr_task = tauri::async_runtime::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    let mut tally = Unpacked::default();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("7z stdout read failed: {e}"))?
    {
        tally.read_line(&line);
        if tally.bytes > MAX_UNPACKED_BYTES || tally.entries > MAX_UNPACKED_ENTRIES {
            let _ = child.kill().await;
            return Ok(tally);
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("7z wait failed: {e}"))?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "7z could not list it (code {}): {}",
            status.code().unwrap_or(-1),
            if stderr_text.trim().is_empty() {
                "unknown error"
            } else {
                stderr_text.trim()
            }
        ));
    }
    Ok(tally)
}

fn check_unpack_budget(name: &str, unpacked: Unpacked, dirs: &[&Path]) -> Result<(), String> {
    if unpacked.bytes > MAX_UNPACKED_BYTES || unpacked.entries > MAX_UNPACKED_ENTRIES {
        log::warn!(
            "mods: refused {name}, which lists {} bytes in {} entries",
            unpacked.bytes,
            unpacked.entries
        );
        return Err(format!(
            "{name} would unpack to more than {} GB or {MAX_UNPACKED_ENTRIES} files, far more than any mod needs, so Peebify did not extract it.",
            MAX_UNPACKED_BYTES >> 30
        ));
    }
    for dir in dirs {
        super::engine::ensure_disk_space(dir, unpacked.bytes, 1.0, UNPACK_HEADROOM)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
pub struct ArchiveOptions<'a> {
    pub label: Option<&'a str>,
    pub progress: Option<(&'a str, f64, f64)>,
}

type InstallLocks =
    std::collections::BTreeMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>;

static INSTALL_LOCKS: parking_lot::Mutex<InstallLocks> =
    parking_lot::Mutex::new(std::collections::BTreeMap::new());

async fn lock_installs(game_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = INSTALL_LOCKS
        .lock()
        .entry(game_id.to_string())
        .or_default()
        .clone();
    lock.lock_owned().await
}

pub async fn install_archive(
    app: &AppHandle,
    game_id: &str,
    variant: &str,
    archive: &Path,
    source: Value,
    options: ArchiveOptions<'_>,
) -> Result<Vec<String>, String> {
    let root = toolchain_root(app);
    let mods_dir = xxmi::mods_dir(&root, variant);
    std::fs::create_dir_all(&mods_dir)
        .map_err(|e| format!("Could not create the Mods folder: {e}"))?;

    let staging_root = mods_root(app).join(".staging");
    prune_stale_staging(&staging_root, std::time::Duration::from_secs(60 * 60 * 24));
    sweep_replaced_in(&mods_dir, REPLACED_MAX_AGE);
    std::fs::create_dir_all(&staging_root)
        .map_err(|e| format!("Could not create staging dir: {e}"))?;
    let staging = staging_root.join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir(&staging).map_err(|e| format!("Could not create staging dir: {e}"))?;

    let result =
        extract_and_place(app, game_id, archive, &staging, &mods_dir, source, options).await;
    let _ = std::fs::remove_dir_all(&staging);
    result
}

async fn extract_and_place(
    app: &AppHandle,
    game_id: &str,
    archive: &Path,
    staging: &Path,
    mods_dir: &Path,
    source: Value,
    options: ArchiveOptions<'_>,
) -> Result<Vec<String>, String> {
    let started = std::time::Instant::now();
    let name = archive
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "the archive".to_string());
    let seven_zip = super::extract::resolve_bundled_7z_binary(app).await?;
    log::info!("Extracting {name} with {}", seven_zip.display());
    let unpacked = archive_unpacked_size(&seven_zip, archive)
        .await
        .map_err(|e| format!("Could not read {name}: {e}"))?;
    check_unpack_budget(&name, unpacked, &[staging, mods_dir])?;
    super::extract::run_7z_extract(&seven_zip, archive, staging, |p| {
        if let Some((message, from, to)) = options.progress {
            let percent = from + (to - from) * (p.clamp(0.0, 100.0) / 100.0);
            publish_progress(app, game_id, message, percent.clamp(1.0, 99.0));
        }
    })
    .await
    .map_err(|e| format!("Could not extract {name}: {e}"))?;
    let scan_dir = staging.to_path_buf();
    if let Ok(Some(link)) =
        tauri::async_runtime::spawn_blocking(move || super::fs_util::find_link(&scan_dir)).await
    {
        log::warn!("mods: refused {name}, which unpacked a link at {}", link.display());
        return Err(format!(
            "{name} contains a shortcut link to another folder, which mods never need, so Peebify did not install it."
        ));
    }

    let archive_stem = archive
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Mod".to_string());
    let staging_dir = staging.to_path_buf();
    let target_dir = mods_dir.to_path_buf();
    let label = options.label.map(str::trim).filter(|l| !l.is_empty());
    let blocking_label = label.map(str::to_string);
    let _placing = lock_installs(game_id).await;
    let placed = tauri::async_runtime::spawn_blocking(move || {
        place_extracted(&staging_dir, &target_dir, &archive_stem, blocking_label.as_deref())
    })
    .await
    .map_err(|e| format!("Could not place the mod: {e}"))??;

    let mut installed = Vec::new();
    let mut total_bytes = 0u64;
    for (folder, size, relative, display) in placed {
        let mut folder_source = source.clone();
        if let Some(map) = folder_source.as_object_mut() {
            map.insert("root".to_string(), Value::String(relative));
            if let Some(label) = label {
                map.insert("label".to_string(), json!(label));
            }
        }
        record_metadata(
            app,
            game_id,
            &folder,
            json!({
                "modId": uuid::Uuid::new_v4().to_string(),
                "name": display,
                "source": folder_source,
                "installedAt": chrono::Utc::now().to_rfc3339(),
                "version": source.get("version").cloned().unwrap_or(Value::Null),
                "thumbnailUrl": source.get("thumbnailUrl").cloned().unwrap_or(Value::Null),
                "sizeBytes": size,
            }),
        );
        total_bytes += size;
        installed.push(folder);
    }

    log::info!(
        "mods: installed {installed:?} for {game_id} from {name} ({total_bytes} bytes, {} ms)",
        started.elapsed().as_millis()
    );
    Ok(installed)
}

type Placed = Vec<(String, u64, String, String)>;

fn place_extracted(
    staging: &Path,
    mods_dir: &Path,
    archive_stem: &str,
    label: Option<&str>,
) -> Result<Placed, String> {
    let label = label.map(str::trim).filter(|l| !l.is_empty());
    remove_mac_metadata(staging, 0);
    let mut roots = Vec::new();
    find_mod_roots(staging, 0, &mut roots);
    if roots.is_empty() {
        return Err(
            "That file doesn't look like a mod. There was no .ini file inside it.".to_string(),
        );
    }
    for root_path in &roots {
        if let Some(name) = rogue_config_in(root_path) {
            return Err(format!(
                "This archive contains a 3DMigoto config ({name}), not a mod, so Peebify did not install it."
            ));
        }
    }

    let mut placed = Vec::new();
    for root_path in &roots {
        let raw_name = if roots.len() == 1 {
            label.unwrap_or(archive_stem).to_string()
        } else {
            root_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| archive_stem.to_string())
        };
        let desired = sanitize_folder_name(&raw_name);
        let folder = match claim_folder(mods_dir, &desired) {
            Ok(folder) => folder,
            Err(e) => {
                take_back_placed(mods_dir, &placed);
                return Err(e);
            }
        };
        let target = mods_dir.join(&folder);
        if let Err(e) = place_mod(root_path, &target) {
            take_back_placed(mods_dir, &placed);
            return Err(e);
        }
        let size = super::fs_util::directory_size(&target);
        let relative = root_path
            .strip_prefix(staging)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let display = match label {
            Some(label) if roots.len() == 1 => label.to_string(),
            Some(label) => format!("{label} ({raw_name})"),
            None => folder.clone(),
        };
        placed.push((folder, size, relative, display));
    }
    Ok(placed)
}

fn take_back_placed(mods_dir: &Path, placed: &Placed) {
    for (folder, ..) in placed {
        let path = mods_dir.join(folder);
        let Err(e) = remove_dir_with_retry(&path) else {
            continue;
        };
        let parked = mods_dir.join(disabled_name(display_name_of(folder)));
        match rename_with_retry(&path, &parked) {
            Ok(()) => log::warn!(
                "mods: \"{folder}\" from an install that did not finish could not be removed ({e}), so it was switched off"
            ),
            Err(off) => log::error!(
                "mods: \"{folder}\" from an install that did not finish could not be removed ({e}) or switched off ({off})"
            ),
        }
    }
}

// ------------ Mod Loader and Mod List Commands ------------
// The commands behind the Mods page: mod loader status, installing and removing the loader, the master and
// per-game switches, listing mods and importing an archive.
pub(super) async fn mods_status(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let root = toolchain_root(app);
    let versions = xxmi::installed_versions(&root);

    let config = &app.state::<BackendState>().config;
    let supported: Vec<Value> = game_profiles::GAME_IDS
        .iter()
        .filter_map(|id| {
            let p = game_profiles::profile(id);
            let variant = game_profiles::mod_variant(p)?;
            Some(json!({
                "id": id,
                "variant": variant,
                "gameBananaId": game_profiles::mod_gamebanana_id(p),
                "installed": xxmi::variant_installed(&root, variant),
                "enabled": config.get(&format!("games.{id}.modsEnabled")) == Value::Bool(true),
            }))
        })
        .collect();

    let variant = game_profiles::mod_variant(profile);
    let installed = variant
        .map(|v| xxmi::variant_installed(&root, v))
        .unwrap_or(false);

    let versions: serde_json::Map<String, Value> = versions
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();

    Ok(ok_with(json!({
        "masterEnabled": master_enabled(app),
        "gameEnabled": active_for(app, &profile_id, profile),
        "gameId": profile_id,
        "supportsMods": game_profiles::mod_config(profile).is_some(),
        "experimental": game_profiles::mod_is_experimental(profile),
        "forcesDx11": !game_profiles::mod_force_dx11_args(profile).is_empty(),
        "variant": variant,
        "toolchainInstalled": installed,
        "toolchainPath": root.to_string_lossy(),
        "isDefault": fs_util::same_path(
            &root.to_string_lossy(),
            &xxmi::default_root(&app.state::<BackendState>().user_data).to_string_lossy(),
        ),
        "versions": Value::Object(versions),
        "toolsInstalling": super::xxmi::installing(),
        "supportedGames": supported,
    })))
}

pub(crate) async fn set_game_mods_enabled(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    if game_profiles::mod_config(profile).is_none() {
        return Ok(err_response(format!(
            "{} can't use mods.",
            game_profiles::display_name(profile)
        )));
    }
    let enabled = args.get(1) == Some(&Value::Bool(true));

    app.state::<BackendState>()
        .config
        .set(&format!("games.{profile_id}.modsEnabled"), json!(enabled));

    log::info!(
        "mods: {} for {profile_id}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(ok_with(json!({ "enabled": enabled })))
}

pub(super) async fn list_mods(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let Ok(variant) = variant_for(profile) else {
        return Ok(ok_with(json!({ "mods": [] })));
    };

    let mods_dir = xxmi::mods_dir(&toolchain_root(app), &variant);
    let scanned = {
        let app = app.clone();
        let profile_id = profile_id.clone();
        let mods_dir = mods_dir.clone();
        tauri::async_runtime::spawn_blocking(move || -> Result<Scan, String> {
            let _guard = LIBRARY_LOCK.lock();
            let (mut library, claimed) = load_claimed_library(&app)?;
            let scan = scan_and_reconcile(&profile_id, &library_root_key(&app), &mods_dir, &mut library);
            if scan.changed || claimed {
                if let Err(e) = write_library(&app, &library) {
                    log::warn!("mods: could not reconcile the library for {profile_id}: {e}");
                }
            }
            Ok(scan)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|scanned| scanned)
    };
    let scan = match scanned {
        Ok(scan) => scan,
        Err(e) => {
            log::warn!("mods: could not list the mods for {profile_id}: {e}");
            return Ok(err_response(e));
        }
    };
    if let Some(e) = scan.unreadable {
        return Ok(err_response(format!(
            "Couldn't read the Mods folder at {}: {e}",
            mods_dir.display()
        )));
    }
    if !scan.unmeasured.is_empty() {
        fill_sizes_in_background(app, &profile_id, mods_dir.clone(), scan.unmeasured);
    }
    Ok(ok_with(json!({ "mods": scan.mods })))
}

pub(super) async fn import_mod_archive(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    if !master_enabled(app) {
        return Ok(err_response("Turn on mod support first."));
    }
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let variant = match variant_for(profile) {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };

    let mut paths = super::mod_profiles::string_list(args.get(1));
    let from_renderer = !paths.is_empty();

    if paths.is_empty() {
        paths = crate::backend::fs_util::dialog::pick_files(
            app,
            Pick {
                title: "Choose mod archives".into(),
                filters: &[("Mod archive", &["zip", "7z", "rar"])],
                ..Pick::default()
            },
        )
        .await?;
        if paths.is_empty() {
            return Ok(json!({ "success": false, "cancelled": true }));
        }
    }

    let mut installed = Vec::new();
    let mut failures = Vec::new();
    let count = paths.len();
    for (index, path) in paths.iter().enumerate() {
        let archive = PathBuf::from(path);
        let label = archive
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        let archive = if from_renderer {
            match importable_archive(path) {
                Ok(archive) => archive,
                Err(e) => {
                    log::warn!("mods: refused to import {path}: {e}");
                    failures.push(format!("{label}: {e}"));
                    continue;
                }
            }
        } else {
            archive
        };
        let message = format!("Importing {label} ({} of {count})…", index + 1);
        let from = index as f64 * 100.0 / count as f64;
        let to = (index + 1) as f64 * 100.0 / count as f64;
        publish_progress(app, &profile_id, &message, from.clamp(1.0, 99.0));
        match install_archive(
            app,
            &profile_id,
            &variant,
            &archive,
            json!({ "kind": "archive" }),
            ArchiveOptions {
                label: None,
                progress: Some((&message, from, to)),
            },
        )
        .await
        {
            Ok(names) => installed.extend(names),
            Err(e) => {
                log::warn!("mods: import of {label} failed: {e}");
                failures.push(format!("{label}: {e}"));
            }
        }
    }

    if installed.is_empty() {
        finish_progress(app, &profile_id, true);
        return Ok(err_response(failures.join("; ")));
    }
    finish_progress(app, &profile_id, false);
    let ids = mod_ids_for_folders(app, &profile_id, &installed);
    super::mod_profiles::add_to_active(app, &profile_id, &ids);
    notify_mods_changed(app, &profile_id);
    Ok(ok_with(json!({
        "installed": installed,
        "failed": failures,
    })))
}

fn is_valid_mod_folder(folder: &str) -> bool {
    !folder.is_empty()
        && !folder.contains(['/', '\\'])
        && !folder.contains("..")
        && !folder.contains(':')
        && folder != "."
        && !folder.ends_with(['.', ' '])
        && !folder
            .chars()
            .any(|c| (c as u32) < 0x20 || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
}

const IMPORT_EXTENSIONS: [&str; 3] = ["zip", "7z", "rar"];

fn importable_archive(path: &str) -> Result<PathBuf, String> {
    let archive = PathBuf::from(path);
    let local = matches!(
        archive.components().next(),
        Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_))
    );
    if !local || !archive.is_absolute() {
        return Err("Only archives on this PC's drives can be imported.".to_string());
    }
    let extension_ok = archive
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMPORT_EXTENSIONS.iter().any(|ok| e.eq_ignore_ascii_case(ok)));
    if !extension_ok {
        return Err("Only .zip, .7z and .rar archives can be imported.".to_string());
    }
    if !std::fs::symlink_metadata(&archive).is_ok_and(|m| m.is_file()) {
        return Err("That archive could not be found.".to_string());
    }
    Ok(archive)
}

pub(super) struct Toggled {
    pub from: String,
    pub to: String,
}

pub(crate) struct ModView {
    pub mod_id: String,
    pub folder: String,
    pub enabled: bool,
}

// ------------ Toggling, Replacing and Deleting Mods ------------
// Toggling a mod renames its folder, which Windows can refuse while a file is in use, so it retries a few times.
// This part also has the snapshot helpers other modules use, plus replace, bulk toggle and delete.
fn rename_with_retry(from: &Path, to: &Path) -> Result<(), String> {
    super::fs_util::retry_while_locked(|| std::fs::rename(from, to)).map_err(|e| e.to_string())
}

#[derive(Debug)]
enum ToggleError {
    Busy(String),
    Failed(String),
}

impl ToggleError {
    fn into_message(self) -> String {
        match self {
            ToggleError::Busy(e) | ToggleError::Failed(e) => e,
        }
    }
}

const TOGGLE_ATTEMPTS: u32 = 4;
const TOGGLE_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(80);

fn toggle_one(mods_dir: &Path, folder: &str, enabled: bool) -> Result<Option<Toggled>, ToggleError> {
    let failed = |e: &str| Err(ToggleError::Failed(e.to_string()));
    if !is_valid_mod_folder(folder) {
        return failed("That mod name isn't valid.");
    }
    let current = mods_dir.join(folder);
    if !current.is_dir() {
        return failed("That mod is no longer on disk.");
    }

    if is_disabled_folder(folder) != enabled {
        return Ok(None);
    }
    let display = display_name_of(folder).to_string();
    let target_name = if enabled {
        display
    } else {
        disabled_name(&display)
    };
    if target_name.is_empty() {
        return failed("Rename that folder in the Mods folder first, since its name is only DISABLED.");
    }
    if target_name == folder {
        return Ok(None);
    }

    let target = mods_dir.join(&target_name);
    if target.exists() {
        return failed(&format!(
            "There's already a mod folder called \"{target_name}\"."
        ));
    }
    if let Err(e) = std::fs::rename(&current, &target) {
        let message = format!("Could not update that mod: {e}");
        return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
            ToggleError::Busy(message)
        } else {
            ToggleError::Failed(message)
        });
    }
    Ok(Some(Toggled {
        from: folder.to_string(),
        to: target_name,
    }))
}

pub(super) fn mod_ids_for_folders(
    app: &AppHandle,
    game_id: &str,
    folders: &[String],
) -> Vec<String> {
    let library = read_library(app);
    folders
        .iter()
        .map(|folder| mod_id_of(game_id, folder, library[game_id].get(folder)))
        .collect()
}

pub(crate) struct GbEntry {
    pub folder: String,
    pub mod_id: String,
    pub name: String,
    pub gb_mod_id: u64,
    pub gb_file_id: u64,
    pub installed_at: Option<i64>,
    pub thumbnail_url: Option<String>,
    pub enabled: bool,
}

pub(crate) fn gamebanana_entries(app: &AppHandle, game_id: &str) -> Vec<GbEntry> {
    let Ok(mods_dir) = mods_dir_for(app, game_id) else {
        return Vec::new();
    };
    let library = read_library(app);
    let Some(folders) = library[game_id].as_object() else {
        return Vec::new();
    };
    folders
        .iter()
        .filter_map(|(folder, meta)| {
            let source = meta.get("source")?;
            if source["kind"].as_str() != Some("gamebanana") {
                return None;
            }
            if !mods_dir.join(folder).is_dir() {
                return None;
            }
            Some(GbEntry {
                folder: folder.clone(),
                mod_id: mod_id_of(game_id, folder, Some(meta)),
                name: meta["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(display_name_of(folder))
                    .to_string(),
                gb_mod_id: source["gbModId"].as_u64()?,
                gb_file_id: source["gbFileId"].as_u64()?,
                installed_at: meta["installedAt"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp()),
                thumbnail_url: meta["thumbnailUrl"].as_str().map(str::to_string),
                enabled: !is_disabled_folder(folder),
            })
        })
        .collect()
}

pub(super) async fn replace_mod(
    app: &AppHandle,
    game_id: &str,
    variant: &str,
    folder: &str,
    archive: &Path,
    source: Value,
    progress: Option<(&str, f64, f64)>,
) -> Result<String, String> {
    if !is_valid_mod_folder(folder) {
        return Err("That mod name isn't valid.".to_string());
    }
    let mods_dir = xxmi::mods_dir(&toolchain_root(app), variant);
    let old_path = mods_dir.join(folder);
    if !old_path.is_dir() {
        return Err("That mod is no longer on disk.".to_string());
    }

    let new_file_id = source["gbFileId"].clone();
    let installed = install_archive(
        app,
        game_id,
        variant,
        archive,
        source,
        ArchiveOptions {
            label: None,
            progress,
        },
    )
    .await?;
    let _placing = lock_installs(game_id).await;
    let discard = |folders: &[String]| {
        for extra in folders {
            let _ = std::fs::remove_dir_all(mods_dir.join(extra));
            forget_metadata(app, game_id, extra);
        }
    };
    let chosen = {
        let library = read_library(app);
        let root_of = |f: &str| {
            library[game_id][f]["source"]["root"]
                .as_str()
                .map(str::to_string)
        };
        let candidates: Vec<(String, Option<String>)> = installed
            .iter()
            .map(|f| (f.clone(), root_of(f)))
            .collect();
        pick_replacement(folder, root_of(folder).as_deref(), &candidates)
    };
    let Some(new_folder) = chosen else {
        discard(&installed);
        return Err(if installed.is_empty() {
            "The update contained nothing to install.".to_string()
        } else {
            format!(
                "The update has no part that matches \"{}\", so it was left as it is.",
                display_name_of(folder)
            )
        });
    };
    let extras: Vec<String> = installed
        .iter()
        .filter(|f| **f != new_folder)
        .cloned()
        .collect();
    discard(&extras);

    let Some(holder) = mods_dir.parent().map(Path::to_path_buf) else {
        discard(std::slice::from_ref(&new_folder));
        return Err("The Mods folder has no parent folder to swap the update through.".to_string());
    };
    let new_path = mods_dir.join(&new_folder);
    let swap_old = old_path.clone();
    let swap_folder = folder.to_string();
    let swapped = tauri::async_runtime::spawn_blocking(move || {
        swap_in(&holder, &swap_folder, &swap_old, &new_path)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    if let Err(e) = swapped {
        discard(std::slice::from_ref(&new_folder));
        return Err(e);
    }

    let old_file_id = {
        let _guard = LIBRARY_LOCK.lock();
        let mut library = load_library(app).map_err(|e| {
            log::warn!("mods: {folder} was updated on disk but its details could not be merged: {e}");
            e
        })?;
        let old_meta = library[game_id].get(folder).cloned().unwrap_or_else(|| json!({}));
        let old_file_id = old_meta["source"]["gbFileId"].clone();
        let mut merged = library[game_id]
            .get(&new_folder)
            .filter(|entry| entry.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        merged["modId"] = json!(mod_id_of(game_id, folder, Some(&old_meta)));
        merged["name"] = old_meta
            .get("name")
            .cloned()
            .unwrap_or_else(|| json!(display_name_of(folder)));
        if merged["thumbnailUrl"].is_null() {
            merged["thumbnailUrl"] = old_meta["thumbnailUrl"].clone();
        }
        if let (Some(source), Some(label)) = (
            merged.get_mut("source").and_then(Value::as_object_mut),
            old_meta["source"]["label"].as_str(),
        ) {
            source
                .entry("label")
                .or_insert_with(|| Value::String(label.to_string()));
        }
        if let Some(map) = library[game_id].as_object_mut() {
            map.remove(&new_folder);
            map.insert(folder.to_string(), merged);
        }
        write_library(app, &library)?;
        old_file_id
    };

    log::info!(
        "mods: replaced {folder} for {game_id} with a newer upload (file {old_file_id} to {new_file_id})"
    );
    Ok(folder.to_string())
}

fn pick_replacement(
    old_folder: &str,
    old_root: Option<&str>,
    installed: &[(String, Option<String>)],
) -> Option<String> {
    if let [(only, _)] = installed {
        return Some(only.clone());
    }
    if let Some(root) = old_root.filter(|r| !r.is_empty()) {
        let by_root = installed
            .iter()
            .find(|(_, r)| r.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(root)));
        if let Some((found, _)) = by_root {
            return Some(found.clone());
        }
    }
    let base = |name: &str| super::mod_profiles::strip_copy_suffix(display_name_of(name)).to_string();
    let wanted = base(old_folder);
    installed
        .iter()
        .find(|(f, _)| base(f).eq_ignore_ascii_case(&wanted))
        .map(|(f, _)| f.clone())
}

fn swap_in(holder: &Path, folder: &str, old: &Path, new: &Path) -> Result<(), String> {
    let mut stamp = chrono::Utc::now().timestamp_millis();
    let mut backup = holder.join(replaced_copy_name(folder, stamp));
    while backup.exists() {
        stamp += 1;
        backup = holder.join(replaced_copy_name(folder, stamp));
    }
    rename_with_retry(old, &backup).map_err(|e| {
        format!("The old files could not be replaced ({e}). Close the game and try again.")
    })?;
    if let Err(e) = rename_with_retry(new, old) {
        if let Err(back) = rename_with_retry(&backup, old) {
            log::error!(
                "mods: the old copy of {folder} stays at {} because it could not be put back ({back})",
                backup.display()
            );
        }
        return Err(format!("The updated files could not be moved into place: {e}"));
    }
    if let Err(e) = remove_dir_with_retry(&backup) {
        let spent = holder.join(format!("{SPENT_PREFIX}{stamp}-{folder}"));
        let parked = rename_with_retry(&backup, &spent).is_ok();
        log::warn!(
            "mods: the replaced copy of {folder} stays at {} for now ({e})",
            if parked { spent.display() } else { backup.display() }
        );
    }
    Ok(())
}

pub(super) fn storage_dir(app: &AppHandle, game_id: &str) -> Option<(PathBuf, usize, usize)> {
    let dir = mods_dir_for(app, game_id).ok()?;
    let (mut installed, mut enabled) = (0, 0);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            installed += 1;
            if !is_disabled_folder(&name) {
                enabled += 1;
            }
        }
    }
    Some((dir, installed, enabled))
}

pub(crate) fn mods_dir_for(app: &AppHandle, game_id: &str) -> Result<PathBuf, String> {
    let variant = variant_for(game_profiles::profile(game_id))?;
    Ok(xxmi::mods_dir(&toolchain_root(app), &variant))
}

pub(super) fn log_launch_state(app: &AppHandle, game_id: &str, profile: &Value) {
    let mods = snapshot(app, game_id);
    let enabled = mods.iter().filter(|m| m.enabled).count();
    let variant = game_profiles::mod_variant(profile).unwrap_or("none");
    let version = xxmi::installed_versions(&toolchain_root(app))
        .remove(variant)
        .unwrap_or_else(|| "unknown".to_string());
    let active = super::mod_profiles::active_profile_name(app, game_id)
        .unwrap_or_else(|| "none".to_string());
    log::info!(
        "mods: {game_id} starting with {enabled} enabled, {} disabled ({variant} {version}, profile \"{active}\")",
        mods.len() - enabled
    );
}

pub(crate) fn snapshot(app: &AppHandle, game_id: &str) -> Vec<ModView> {
    let Ok(mods_dir) = mods_dir_for(app, game_id) else {
        return Vec::new();
    };
    scan_mods(app, game_id, &mods_dir)
        .into_iter()
        .filter_map(|m| {
            Some(ModView {
                mod_id: m["modId"].as_str()?.to_string(),
                folder: m["folderName"].as_str()?.to_string(),
                enabled: m["enabled"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

pub(super) async fn apply_enabled(
    app: &AppHandle,
    game_id: &str,
    mods_dir: &Path,
    wanted: &[(String, bool)],
) -> (Vec<Value>, Vec<Value>) {
    let app = app.clone();
    let game = game_id.to_string();
    let dir = mods_dir.to_path_buf();
    let plan = wanted.to_vec();
    let count = plan.len();
    match tauri::async_runtime::spawn_blocking(move || apply_enabled_now(&app, &game, &dir, &plan))
        .await
    {
        Ok(result) => result,
        Err(e) => {
            log::warn!("mods: toggling {count} mod(s) for {game_id} did not finish: {e}");
            (Vec::new(), Vec::new())
        }
    }
}

fn apply_enabled_now(
    app: &AppHandle,
    game_id: &str,
    mods_dir: &Path,
    wanted: &[(String, bool)],
) -> (Vec<Value>, Vec<Value>) {
    let mut changed: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    let mut pending: Vec<(String, bool)> = wanted.to_vec();
    for attempt in 1..=TOGGLE_ATTEMPTS {
        let mut moves: Vec<Toggled> = Vec::new();
        let mut busy: Vec<(String, bool)> = Vec::new();
        let guard = LIBRARY_LOCK.lock();
        for (folder, enabled) in pending {
            match toggle_one(mods_dir, &folder, enabled) {
                Ok(Some(moved)) => {
                    changed.push(json!({
                        "folderName": moved.to,
                        "previousFolderName": moved.from,
                        "enabled": enabled,
                    }));
                    moves.push(moved);
                }
                Ok(None) => {}
                Err(ToggleError::Busy(_)) if attempt < TOGGLE_ATTEMPTS => {
                    busy.push((folder, enabled));
                }
                Err(error) => failed.push(json!({
                    "folderName": folder,
                    "error": error.into_message(),
                })),
            }
        }
        rename_metadata_locked(app, game_id, &moves);
        drop(guard);
        if busy.is_empty() {
            break;
        }
        pending = busy;
        std::thread::sleep(TOGGLE_RETRY_PAUSE);
    }

    if !changed.is_empty() {
        log::info!(
            "mods: toggled {} mod(s) for {game_id} ({} failed)",
            changed.len(),
            failed.len()
        );
    }
    (changed, failed)
}

pub(super) async fn set_mods_enabled_bulk(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let mods_dir = match mods_dir_for(app, &profile_id) {
        Ok(dir) => dir,
        Err(e) => return Ok(err_response(e)),
    };

    let enabled = args.get(2) == Some(&Value::Bool(true));
    let wanted: Vec<(String, bool)> = super::mod_profiles::string_list(args.get(1))
        .into_iter()
        .filter(|f| !f.is_empty())
        .map(|f| (f, enabled))
        .collect();

    if wanted.is_empty() {
        return Ok(err_response("No mods were specified."));
    }

    let (changed, failed) = apply_enabled(app, &profile_id, &mods_dir, &wanted).await;
    if !changed.is_empty() {
        notify_mods_changed(app, &profile_id);
    }
    Ok(ok_with(json!({
        "changed": changed,
        "failed": failed,
    })))
}

pub(super) async fn set_mod_enabled(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let variant = match variant_for(profile) {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };
    let Some(folder) = arg_str(args, 1) else {
        return Ok(err_response("No mod was specified."));
    };
    if !is_valid_mod_folder(folder) {
        return Ok(err_response("That mod name isn't valid."));
    }
    let enabled = args.get(2) == Some(&Value::Bool(true));

    let mods_dir = xxmi::mods_dir(&toolchain_root(app), &variant);
    let display = display_name_of(folder).to_string();
    let wanted = [(folder.to_string(), enabled)];
    let (changed, failed) = apply_enabled(app, &profile_id, &mods_dir, &wanted).await;
    if let Some(failure) = failed.first() {
        return Ok(err_response(
            failure["error"].as_str().unwrap_or("Could not update that mod."),
        ));
    }
    let Some(moved) = changed.first().and_then(|c| c["folderName"].as_str()) else {
        return Ok(ok_with(json!({ "folderName": folder })));
    };
    log::info!(
        "mods: {} \"{display}\" for {profile_id}",
        if enabled { "enabled" } else { "disabled" }
    );
    notify_mods_changed(app, &profile_id);
    Ok(ok_with(json!({ "folderName": moved })))
}

pub(super) async fn delete_mod(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let variant = match variant_for(profile) {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };
    let Some(asked) = arg_str(args, 1) else {
        return Ok(err_response("No mod was specified."));
    };

    if !is_valid_mod_folder(asked) {
        return Ok(err_response("That mod name isn't valid."));
    }

    let mods_dir = xxmi::mods_dir(&toolchain_root(app), &variant);
    let listed = snapshot(app, &profile_id);
    let current = if mods_dir.join(asked).is_dir() {
        listed.iter().find(|m| m.folder == asked)
    } else {
        match arg_str(args, 2).filter(|id| !id.is_empty()) {
            Some(id) => listed.iter().find(|m| m.mod_id == id),
            None if asked_still_recorded(app, &profile_id, asked) => None,
            None => {
                let same = |m: &&ModView| {
                    display_name_of(&m.folder).eq_ignore_ascii_case(display_name_of(asked))
                };
                match listed.iter().filter(same).collect::<Vec<_>>().as_slice() {
                    [only] => Some(*only),
                    _ => None,
                }
            }
        }
    };
    let (folder, mod_id) = match current {
        Some(m) => (m.folder.clone(), Some(m.mod_id.clone())),
        None if mods_dir.join(asked).is_dir() => (asked.to_string(), None),
        None => {
            log::warn!("mods: \"{asked}\" for {profile_id} was not found to delete");
            return Ok(err_response(
                "That mod was renamed or moved. Refresh and try again.",
            ));
        }
    };
    if folder != asked {
        log::info!("mods: \"{asked}\" is now \"{folder}\" for {profile_id}, so that folder is deleted");
    }
    let folder = folder.as_str();

    let path = mods_dir.join(folder);
    if let Err(e) = remove_dir_blocking(path).await {
        return Ok(err_response(format!("Could not delete that mod: {e}")));
    }
    forget_metadata(app, &profile_id, folder);
    if let Some(id) = mod_id.as_deref() {
        super::mod_profiles::forget_mod(app, &profile_id, id);
    }
    log::info!("mods: deleted \"{folder}\" for {profile_id}");
    notify_mods_changed(app, &profile_id);
    Ok(ok_response())
}

fn asked_still_recorded(app: &AppHandle, game_id: &str, folder: &str) -> bool {
    match load_library(app) {
        Ok(library) => library[game_id].get(folder).is_some(),
        Err(e) => {
            log::warn!("mods: could not check the library before deleting \"{folder}\": {e}");
            true
        }
    }
}

async fn remove_dir_blocking(path: PathBuf) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || remove_dir_with_retry(&path))
        .await
        .map_err(|e| e.to_string())?
}

fn remove_dir_with_retry(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() != std::io::ErrorKind::PermissionDenied => {
            return Err(e.to_string());
        }
        Err(_) => {}
    }
    clear_readonly(path);
    std::fs::remove_dir_all(path).map_err(|e| e.to_string())
}

fn clear_readonly(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if is_link(&meta) {
            continue;
        }
        let child = entry.path();
        if meta.is_dir() {
            clear_readonly(&child);
        } else {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&child, perms);
        }
    }
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

pub(super) async fn open_mods_folder(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let variant = match variant_for(profile) {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };
    let path = xxmi::mods_dir(&toolchain_root(app), &variant);
    if !path.is_dir() {
        return Ok(err_response(
            "The mod tools aren't installed yet, so there's no folder to open.",
        ));
    }
    crate::backend::fs_util::dialog::open_folder(&path)?;
    Ok(ok_response())
}

pub(super) async fn set_mods_path(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let state = app.state::<BackendState>();

    if args.first() == Some(&Value::Null) || arg_str(args, 0) == Some("") {
        pin_library_root(app);
        state
            .config
            .set("behavior.modsPath", Value::String(String::new()));
        return Ok(ok_with(
            json!({ "path": xxmi::root(&state.config, &state.user_data).to_string_lossy() }),
        ));
    }

    let picked = crate::backend::fs_util::dialog::pick_folder(
        app,
        Pick {
            title: "Choose a folder for Peebify's mod tools".into(),
            ..Pick::default()
        },
    )
    .await?;
    let Some(path) = picked.as_deref() else {
        return Ok(json!({ "success": false, "cancelled": true }));
    };

    let games: Vec<(String, PathBuf)> = game_profiles::GAME_IDS
        .iter()
        .filter_map(|id| match state.config.get(&format!("games.{id}.gamePath")) {
            Value::String(p) if !p.trim().is_empty() => Some((
                game_profiles::display_name(game_profiles::profile(id)).to_string(),
                PathBuf::from(p.trim()),
            )),
            _ => None,
        })
        .collect();
    let launcher_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let folder = match choose_toolchain_folder(Path::new(path), &games, launcher_dir.as_deref()) {
        Ok(folder) => folder,
        Err(e) => return Ok(err_response(e)),
    };
    if super::game_profiles::is_protected_root(&folder) {
        return Ok(err_response(
            "Windows protects that folder. Pick one outside Program Files and the Windows folder.",
        ));
    }
    if let Err(e) = std::fs::create_dir_all(&folder) {
        return Ok(err_response(format!("Could not create {}: {e}", folder.display())));
    }

    let folder = folder.to_string_lossy().to_string();
    pin_library_root(app);
    state
        .config
        .set("behavior.modsPath", Value::String(folder.clone()));
    log::info!("mods: toolchain path set to {folder} (picked {path})");
    Ok(ok_with(json!({ "path": folder })))
}

fn choose_toolchain_folder(
    pick: &Path,
    games: &[(String, PathBuf)],
    launcher_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    if pick.parent().is_none() {
        return Err(
            "Peebify can't keep its mod tools at the top of a drive. Pick a folder instead."
                .to_string(),
        );
    }
    let occupied = std::fs::read_dir(pick)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    let folder = if occupied && !xxmi::is_toolchain_folder(pick) {
        pick.join(xxmi::OWN_SUBFOLDER)
    } else {
        pick.to_path_buf()
    };
    let target = folder.to_string_lossy();
    let overlaps = |other: &Path| {
        let other = other.to_string_lossy();
        fs_util::is_same_or_inside(&target, &other) || fs_util::is_same_or_inside(&other, &target)
    };
    if let Some((name, _)) = games.iter().find(|(_, game)| overlaps(game)) {
        return Err(format!(
            "That folder overlaps the {name} install. Pick a folder outside your games for the mod tools."
        ));
    }
    if launcher_dir.is_some_and(overlaps) {
        return Err(
            "That folder is part of Peebify's own install. Pick a different folder for the mod tools."
                .to_string(),
        );
    }
    Ok(folder)
}

pub(super) async fn remove_game_mods(app: &AppHandle, profile: &Value) -> usize {
    let profile_id = game_profiles::profile_id(profile).to_string();
    let Ok(variant) = variant_for(profile) else {
        return 0;
    };

    let mods_dir = xxmi::mods_dir(&toolchain_root(app), &variant);
    let folders: Vec<(PathBuf, Option<String>)> = scan_mods(app, &profile_id, &mods_dir)
        .iter()
        .filter_map(|entry| {
            let folder = entry["folderName"].as_str()?;
            Some((
                mods_dir.join(folder),
                entry["modId"].as_str().map(str::to_string),
            ))
        })
        .collect();
    let (removed, removed_ids) = tauri::async_runtime::spawn_blocking(move || {
        let mut count = 0usize;
        let mut ids = Vec::new();
        for (path, mod_id) in &folders {
            if remove_dir_with_retry(path).is_ok() {
                count += 1;
                ids.extend(mod_id.clone());
            }
        }
        (count, ids)
    })
    .await
    .unwrap_or_default();
    super::mod_profiles::forget_mods(app, &profile_id, &removed_ids);

    update_library(app, &format!("drop the library entries of {profile_id}"), |library| {
        if let Some(map) = library.as_object_mut() {
            map.remove(&profile_id);
        }
        true
    });

    log::info!("mods: removed {removed} mod(s) for {profile_id}");
    removed
}

// ------------ Mod Library Tests ------------
// Unit tests for folder naming, archive roots and placement, progress throttling and the disabled-folder rules.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_folder_names_windows_would_rewrite_are_refused() {
        assert!(is_valid_mod_folder("Keqing"));
        assert!(is_valid_mod_folder("DISABLED_Keqing v1.2"));
        for bad in ["", ".", "..", " .", ". ", " ", "a.", "a ", "a/b", r"a\b", "C:x", "a?", "a\u{7}"] {
            assert!(!is_valid_mod_folder(bad), "{bad:?} was accepted");
        }
    }

    #[test]
    fn only_local_archive_files_can_be_imported_by_path() {
        let dir = std::env::temp_dir().join(format!("peebify-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("mod.zip");
        std::fs::write(&zip, "x").unwrap();
        std::fs::write(dir.join("mod.exe"), "x").unwrap();
        assert_eq!(importable_archive(&zip.to_string_lossy()).unwrap(), zip);
        assert!(importable_archive(&dir.join("mod.exe").to_string_lossy()).is_err());
        assert!(importable_archive(&dir.join("gone.7z").to_string_lossy()).is_err());
        assert!(importable_archive(r"\\attacker\share\mod.zip").is_err());
        assert!(importable_archive(r"\\?\C:\mod.zip").is_err());
        assert!(importable_archive("mod.zip").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_throttle_is_per_game_and_never_holds_back_a_new_message() {
        let mut last = std::collections::BTreeMap::new();
        let start = std::time::Instant::now();
        assert!(progress_due(&mut last, "genshin", "a", false, start));
        assert!(!progress_due(&mut last, "genshin", "a", false, start));
        assert!(progress_due(&mut last, "zzz", "a", false, start));
        assert!(progress_due(&mut last, "genshin", "a", false, start + PROGRESS_INTERVAL));
        assert!(progress_due(&mut last, "zzz", "a", true, start));
        assert!(!last.contains_key("zzz"));
        assert!(progress_due(&mut last, "zzz", "a", false, start));
        assert!(progress_due(&mut last, "zzz", "b", false, start));
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_copy_leaves_no_partial_mod() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("partial");
        let source = dir.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("a.ini"), b"[x]").unwrap();
        std::fs::write(source.join("b.dds"), b"data").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(source.join("b.dds"))
            .unwrap();
        std::fs::create_dir_all(dir.join("Mods")).unwrap();
        let folder = claim_folder(&dir.join("Mods"), "Keqing").unwrap();
        let destination = dir.join("Mods").join(folder);
        assert!(place_mod(&source, &destination).is_err());
        assert!(!destination.exists());
        drop(lock);
        assert!(source.join("a.ini").is_file());
        assert!(source.join("b.dds").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mod_folder_names_avoid_reserved_devices() {
        assert_eq!(sanitize_folder_name("nul"), "nul_");
        assert_eq!(sanitize_folder_name("AUX"), "AUX_");
        assert_eq!(sanitize_folder_name("  ...  "), "Mod");
        assert_eq!(sanitize_folder_name("Keqing: Outfit"), "Keqing_ Outfit");
    }

    #[test]
    fn a_single_root_takes_the_display_label() {
        let dir = scratch("label");
        let staging = dir.join("staging");
        std::fs::create_dir_all(staging.join("Release")).unwrap();
        std::fs::write(staging.join("Release").join("mod.ini"), b"[x]").unwrap();
        let mods_dir = dir.join("Mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        let placed =
            place_extracted(&staging, &mods_dir, "Release", Some("Keqing: Summer")).unwrap();
        assert_eq!(placed[0].0, "Keqing_ Summer");
        assert_eq!(placed[0].3, "Keqing: Summer");
        assert!(mods_dir.join("Keqing_ Summer").join("mod.ini").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_roots_keep_their_folders_and_name_the_label() {
        let dir = scratch("roots");
        let staging = dir.join("staging");
        for root in ["Body", "Hair"] {
            std::fs::create_dir_all(staging.join(root)).unwrap();
            std::fs::write(staging.join(root).join("mod.ini"), b"[x]").unwrap();
        }
        let mods_dir = dir.join("Mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        let mut placed =
            place_extracted(&staging, &mods_dir, "pack", Some("Ellen")).unwrap();
        placed.sort();
        assert_eq!(placed[0].0, "Body");
        assert_eq!(placed[0].3, "Ellen (Body)");
        assert_eq!(placed[1].3, "Ellen (Hair)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn comparable_path(path: &Path) -> String {
        fs_util::normalize_path(&path.to_string_lossy())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peebify-mods-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_occupied_folder_gets_its_own_subfolder() {
        let dir = scratch("occupied");
        assert_eq!(choose_toolchain_folder(&dir, &[], None).unwrap(), dir);
        std::fs::write(dir.join("XXMI Launcher.exe"), b"MZ").unwrap();
        assert_eq!(
            choose_toolchain_folder(&dir, &[], None).unwrap(),
            dir.join(xxmi::OWN_SUBFOLDER)
        );
        std::fs::write(dir.join(xxmi::VERSION_FILE), "xxmi=v1\n").unwrap();
        assert_eq!(choose_toolchain_folder(&dir, &[], None).unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn game_and_launcher_folders_are_refused() {
        let dir = scratch("games");
        let game = dir.join("ZZZ");
        std::fs::create_dir_all(&game).unwrap();
        let games = vec![("Zenless Zone Zero".to_string(), game.clone())];
        assert!(choose_toolchain_folder(&game, &games, None).is_err());
        assert!(choose_toolchain_folder(&game.join("Tools"), &games, None).is_err());
        assert_eq!(
            choose_toolchain_folder(&dir, &games, None).unwrap(),
            dir.join(xxmi::OWN_SUBFOLDER)
        );
        assert!(choose_toolchain_folder(&dir, &[], Some(&dir)).is_err());
        assert!(choose_toolchain_folder(Path::new("D:\\"), &[], None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_disabled_spelling_is_recognised() {
        assert!(is_disabled_folder("DISABLED Keqing"));
        assert!(is_disabled_folder("DISABLED_Keqing Outfit"));
        assert!(is_disabled_folder("disabledKeqing"));
        assert!(!is_disabled_folder("Keqing"));
        assert!(!is_disabled_folder("DISABLE"));
        assert!(!is_disabled_folder("日本語テキスト"));
        assert_eq!(display_name_of("DISABLED Keqing"), "Keqing");
        assert_eq!(display_name_of("DISABLED_Keqing Outfit"), "Keqing Outfit");
        assert_eq!(display_name_of("DISABLED - Keqing"), "Keqing");
        assert_eq!(display_name_of("DISABLEDKeqing"), "Keqing");
        assert_eq!(display_name_of("Keqing"), "Keqing");
    }

    #[test]
    fn a_foreign_disabled_folder_switches_on_and_stays_off() {
        let dir = scratch("toggle");
        std::fs::create_dir_all(dir.join("DISABLED_Keqing")).unwrap();
        assert!(toggle_one(&dir, "DISABLED_Keqing", false).unwrap().is_none());
        let moved = toggle_one(&dir, "DISABLED_Keqing", true).unwrap().unwrap();
        assert_eq!(moved.to, "Keqing");
        assert!(dir.join("Keqing").is_dir());
        assert!(toggle_one(&dir, "Keqing", true).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn gb_entry(id: &str) -> Value {
        json!({ "modId": id, "source": { "kind": "gamebanana", "gbModId": 1, "gbFileId": 2 }, "installedAt": "x" })
    }

    #[test]
    fn a_folder_renamed_outside_peebify_keeps_its_details() {
        let folders = vec!["DISABLED Ellen".to_string()];
        let mut map = serde_json::Map::new();
        map.insert("Ellen".into(), gb_entry("u1"));
        assert!(rekey_renamed(&mut map, &folders));
        assert_eq!(map["DISABLED Ellen"]["modId"], "u1");
        assert!(!map.contains_key("Ellen"));

        let mut map = serde_json::Map::new();
        map.insert("Ellen".into(), gb_entry("u1"));
        map.insert(
            "ellen".into(),
            json!({ "source": { "kind": "manual" }, "sizeBytes": 4 }),
        );
        assert!(rekey_renamed(&mut map, &["ellen".to_string()]));
        assert_eq!(map["ellen"]["modId"], "u1");

        let mut map = serde_json::Map::new();
        map.insert("Ellen".into(), gb_entry("u1"));
        map.insert(
            "DISABLED Ellen".into(),
            json!({ "modId": "d", "source": { "kind": "manual" }, "sizeBytes": 4 }),
        );
        assert!(!rekey_renamed(&mut map, &["DISABLED Ellen".to_string()]));
        assert_eq!(map["DISABLED Ellen"]["modId"], "d");

        let mut map = serde_json::Map::new();
        map.insert("Ellen".into(), gb_entry("u1"));
        map.insert("DISABLED Ellen".into(), gb_entry("u2"));
        assert!(!rekey_renamed(&mut map, &["DISABLED Ellen".to_string()]));

        let mut map = serde_json::Map::new();
        map.insert("Ellen".into(), gb_entry("u1"));
        let two = vec!["DISABLED Ellen".to_string(), "DISABLED_Ellen".to_string()];
        assert!(!rekey_renamed(&mut map, &two));
    }

    #[test]
    fn an_unreachable_mods_folder_leaves_the_library_alone() {
        let dir = scratch("unreachable");
        let root = comparable_path(&dir);
        let mut library = json!({
            "_roots": { "genshin": root },
            "genshin": { "Ellen": gb_entry("u1") },
        });

        let offline = dir.join("unplugged").join("gimi").join("Mods");
        let scan = scan_and_reconcile("genshin", &root, &offline, &mut library);
        assert!(!scan.changed);
        assert!(scan.unreadable.is_none());
        assert!(library["genshin"]["Ellen"].is_object());

        let not_installed = dir.join("gimi").join("Mods");
        let scan = scan_and_reconcile("genshin", &root, &not_installed, &mut library);
        assert!(!scan.changed);
        assert!(scan.unreadable.is_none());
        assert!(library["genshin"]["Ellen"].is_object());

        std::fs::create_dir_all(dir.join("gimi")).unwrap();
        std::fs::write(&not_installed, b"not a folder").unwrap();
        let scan = scan_and_reconcile("genshin", &root, &not_installed, &mut library);
        assert!(!scan.changed);
        assert!(scan.unreadable.is_some());
        assert!(library["genshin"]["Ellen"].is_object());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_mods_folder_prunes_the_library() {
        let dir = scratch("deleted-mods");
        let root = comparable_path(&dir);
        std::fs::create_dir_all(dir.join("gimi")).unwrap();
        let mut library = json!({
            "_roots": { "genshin": root },
            "genshin": { "Ellen": gb_entry("u1") },
        });

        let deleted = dir.join("gimi").join("Mods");
        let scan = scan_and_reconcile("genshin", &root, &deleted, &mut library);
        assert!(scan.changed);
        assert!(library["genshin"]["Ellen"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_tools_folder_leaves_the_old_ones_library_alone() {
        let dir = scratch("root-change");
        let old_root = dir.join("old");
        let new_root = dir.join("new");
        let (old_key, new_key) = (comparable_path(&old_root), comparable_path(&new_root));
        std::fs::create_dir_all(old_root.join("gimi").join("Mods").join("Ellen")).unwrap();
        std::fs::create_dir_all(new_root.join("gimi").join("Mods")).unwrap();

        let mut library = json!({ "genshin": { "Ellen": gb_entry("u1") } });
        let old_mods = old_root.join("gimi").join("Mods");
        let scan = scan_and_reconcile("genshin", &old_key, &old_mods, &mut library);
        assert!(scan.changed);
        assert_eq!(library["_roots"]["genshin"], old_key.as_str());
        assert_eq!(scan.mods[0]["modId"], "u1");

        let new_mods = new_root.join("gimi").join("Mods");
        let scan = scan_and_reconcile("genshin", &new_key, &new_mods, &mut library);
        assert!(scan.mods.is_empty());
        assert!(library["genshin"]["Ellen"].is_null());
        assert_eq!(library["_parked"]["genshin"][old_key.as_str()]["Ellen"]["modId"], "u1");

        let scan = scan_and_reconcile("genshin", &old_key, &old_mods, &mut library);
        assert_eq!(scan.mods[0]["modId"], "u1");
        assert_eq!(scan.mods[0]["source"]["kind"], "gamebanana");
        assert_eq!(library["genshin"]["Ellen"]["modId"], "u1");
        assert!(library.get("_parked").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_update_replaces_the_matching_part_of_a_pack() {
        let installed = vec![
            ("Nicole Outfit (2)".to_string(), Some("Pack/Nicole Outfit".to_string())),
            ("Nicole Weapon (2)".to_string(), Some("Pack/Nicole Weapon".to_string())),
        ];
        assert_eq!(
            pick_replacement("Nicole Weapon", None, &installed).as_deref(),
            Some("Nicole Weapon (2)")
        );
        assert_eq!(
            pick_replacement("DISABLED Nicole Weapon", None, &installed).as_deref(),
            Some("Nicole Weapon (2)")
        );
        assert_eq!(
            pick_replacement("Renamed", Some("pack/nicole weapon"), &installed).as_deref(),
            Some("Nicole Weapon (2)")
        );
        assert_eq!(pick_replacement("Nicole Hat", None, &installed), None);
        let single = vec![("Anything".to_string(), None)];
        assert_eq!(
            pick_replacement("Nicole Hat", None, &single).as_deref(),
            Some("Anything")
        );
        assert_eq!(pick_replacement("Nicole Hat", None, &[]), None);
    }

    #[test]
    fn an_update_swap_never_loses_the_old_copy() {
        let dir = scratch("swap");
        let mods = dir.join("Mods");
        std::fs::create_dir_all(mods.join("Ellen")).unwrap();
        std::fs::write(mods.join("Ellen").join("old.ini"), b"old").unwrap();

        let missing = swap_in(&dir, "Ellen", &mods.join("Ellen"), &mods.join("Nope"));
        assert!(missing.is_err());
        assert!(mods.join("Ellen").join("old.ini").is_file());

        std::fs::create_dir_all(mods.join("Ellen (2)")).unwrap();
        std::fs::write(mods.join("Ellen (2)").join("new.ini"), b"new").unwrap();
        swap_in(&dir, "Ellen", &mods.join("Ellen"), &mods.join("Ellen (2)")).unwrap();
        assert!(mods.join("Ellen").join("new.ini").is_file());
        assert!(!mods.join("Ellen (2)").exists());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(REPLACED_PREFIX))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_copies_are_restored_or_removed() {
        let dir = scratch("sweep");
        let mods = dir.join("Mods");
        std::fs::create_dir_all(mods.join("Kept")).unwrap();
        let orphaned = dir.join(replaced_copy_name("Lost", 1));
        let stale = dir.join(replaced_copy_name("Kept", 1));
        let fresh_stamp = chrono::Utc::now().timestamp_millis();
        let fresh = dir.join(replaced_copy_name("Kept", fresh_stamp));
        for path in [&orphaned, &stale, &fresh] {
            std::fs::create_dir_all(path).unwrap();
        }

        sweep_replaced_in(&mods, REPLACED_MAX_AGE);
        assert!(mods.join("Lost").is_dir());
        assert!(!orphaned.exists());
        assert!(!stale.exists());
        assert!(fresh.exists());

        sweep_replaced_in(&mods, std::time::Duration::ZERO);
        assert!(!fresh.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listing_is_totalled_after_its_header() {
        let listing = "Listing archive: pack.zip\n\n--\nPath = pack.zip\nType = zip\nPhysical Size = 900\n\n----------\nPath = a.ini\nFolder = -\nSize = 100\nPacked Size = 40\n\nPath = dir\nFolder = +\nSize = 0\n\nPath = dir/b.buf\nSize = 2500\r\n";
        let mut tally = Unpacked::default();
        for line in listing.lines() {
            tally.read_line(line);
        }
        assert_eq!(tally.bytes, 2600);
        assert_eq!(tally.entries, 3);
    }

    #[test]
    fn a_decompression_bomb_is_refused() {
        let bomb = Unpacked {
            bytes: MAX_UNPACKED_BYTES + 1,
            entries: 1,
            listing: true,
        };
        assert!(check_unpack_budget("bomb.zip", bomb, &[]).is_err());
        let swarm = Unpacked {
            bytes: 1,
            entries: MAX_UNPACKED_ENTRIES + 1,
            listing: true,
        };
        assert!(check_unpack_budget("swarm.zip", swarm, &[]).is_err());
        let fine = Unpacked {
            bytes: 10,
            entries: 2,
            listing: true,
        };
        assert!(check_unpack_budget("fine.zip", fine, &[]).is_ok());
    }

    fn manual_entry(id: &str) -> Value {
        json!({ "modId": id, "source": { "kind": "manual" }, "sizeBytes": 1 })
    }

    fn ids_by_folder(scan: &Scan) -> std::collections::BTreeMap<String, String> {
        scan.mods
            .iter()
            .map(|m| {
                (
                    m["folderName"].as_str().unwrap().to_string(),
                    m["modId"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn mods_dir_with(name: &str, folders: &[&str]) -> (PathBuf, String, PathBuf) {
        let dir = scratch(name);
        let mods = dir.join("gimi").join("Mods");
        for folder in folders {
            std::fs::create_dir_all(mods.join(folder)).unwrap();
        }
        let root = comparable_path(&dir);
        (dir, root, mods)
    }

    #[test]
    fn folders_that_differ_only_by_disabled_get_their_own_ids() {
        let (dir, root, mods) = mods_dir_with("unique-ids", &["Keqing", "DISABLED_Keqing"]);
        let mut library = json!({ "_roots": { "genshin": root } });

        let scan = scan_and_reconcile("genshin", &root, &mods, &mut library);
        let ids = ids_by_folder(&scan);
        assert_ne!(ids["Keqing"], ids["DISABLED_Keqing"]);
        assert_eq!(ids["Keqing"], derived_mod_id("genshin", "Keqing"));
        assert!(scan.changed);
        assert_eq!(
            library["genshin"]["DISABLED_Keqing"]["modId"],
            ids["DISABLED_Keqing"].as_str()
        );

        let mut unsaved = json!({ "_roots": { "genshin": root } });
        let again = scan_and_reconcile("genshin", &root, &mods, &mut unsaved);
        assert_eq!(ids_by_folder(&again), ids);
        let stored = scan_and_reconcile("genshin", &root, &mods, &mut library);
        assert_eq!(ids_by_folder(&stored), ids);
        assert!(!stored.changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shared_stored_id_is_split_once() {
        let (dir, root, mods) = mods_dir_with(
            "split-ids",
            &["Keqing", "DISABLED Keqing", "Raiden", "DISABLED Raiden", "Ellen"],
        );
        let shared = derived_mod_id("genshin", "Keqing");
        let original = json!({
            "_roots": { "genshin": root },
            "genshin": {
                "Keqing": manual_entry(&shared),
                "DISABLED Keqing": manual_entry(&shared),
                "Raiden": manual_entry(&shared),
                "DISABLED Raiden": manual_entry(&shared),
                "Ellen": gb_entry("u1"),
            },
        });

        let mut library = original.clone();
        let scan = scan_and_reconcile("genshin", &root, &mods, &mut library);
        let ids = ids_by_folder(&scan);
        assert_eq!(ids["Keqing"], shared);
        assert_eq!(ids["Ellen"], "u1");
        assert_ne!(ids["DISABLED Keqing"], shared);
        assert_ne!(ids["Raiden"], shared);
        assert_ne!(ids["Raiden"], ids["DISABLED Keqing"]);
        assert_ne!(ids["DISABLED Raiden"], shared);
        assert_ne!(ids["DISABLED Raiden"], ids["Raiden"]);
        assert!(scan.changed);

        let after = scan_and_reconcile("genshin", &root, &mods, &mut library);
        assert!(!after.changed);
        assert_eq!(ids_by_folder(&after), ids);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gamebanana_id_is_never_the_one_replaced() {
        let (dir, root, mods) =
            mods_dir_with("gb-keeps-id", &["Keqing", "DISABLED Keqing", "Ellen"]);
        let mut library = json!({
            "_roots": { "genshin": root },
            "genshin": {
                "Keqing": manual_entry("u1"),
                "DISABLED Keqing": gb_entry("u1"),
                "Ellen": manual_entry("u1"),
            },
        });
        let scan = scan_and_reconcile("genshin", &root, &mods, &mut library);
        let ids = ids_by_folder(&scan);
        assert_eq!(ids["DISABLED Keqing"], "u1");
        assert_ne!(ids["Keqing"], "u1");
        assert_ne!(ids["Ellen"], "u1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_folder_never_takes_a_name_shown_by_a_disabled_one() {
        let dir = scratch("unique-name");
        std::fs::create_dir_all(dir.join("DISABLED_Keqing")).unwrap();
        assert_eq!(claim_folder(&dir, "Keqing").unwrap(), "Keqing (2)");
        assert_eq!(claim_folder(&dir, "keqing").unwrap(), "keqing (3)");
        assert_eq!(claim_folder(&dir, "Ellen").unwrap(), "Ellen");
        assert!(dir.join("Ellen").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn macos_metadata_is_not_installed() {
        let dir = scratch("macosx");
        let staging = dir.join("staging");
        std::fs::create_dir_all(staging.join("Keqing")).unwrap();
        std::fs::write(staging.join("Keqing").join("mod.ini"), b"[x]").unwrap();
        std::fs::write(staging.join("Keqing").join("._mod.ini"), b"\x00\x05\x16\x07").unwrap();
        std::fs::create_dir_all(staging.join("__MACOSX").join("Keqing")).unwrap();
        std::fs::write(
            staging.join("__MACOSX").join("Keqing").join("._mod.ini"),
            b"\x00\x05\x16\x07",
        )
        .unwrap();
        let mods_dir = dir.join("Mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        let placed = place_extracted(&staging, &mods_dir, "Keqing", None).unwrap();
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].0, "Keqing");
        assert!(mods_dir.join("Keqing").join("mod.ini").is_file());
        assert!(!mods_dir.join("Keqing").join("._mod.ini").exists());

        let flat = dir.join("flat");
        std::fs::create_dir_all(flat.join("__MACOSX")).unwrap();
        std::fs::write(flat.join("mod.ini"), b"[x]").unwrap();
        std::fs::write(flat.join("__MACOSX").join("._mod.ini"), b"\x00").unwrap();
        let placed = place_extracted(&flat, &mods_dir, "Ellen", None).unwrap();
        assert_eq!(placed[0].0, "Ellen");
        assert!(!mods_dir.join("Ellen").join("__MACOSX").exists());

        let mut roots = Vec::new();
        let only_meta = dir.join("only-meta");
        std::fs::create_dir_all(only_meta.join("__MACOSX").join("X")).unwrap();
        std::fs::write(only_meta.join("__MACOSX").join("X").join("._a.ini"), b"\x00").unwrap();
        find_mod_roots(&only_meta, 0, &mut roots);
        assert!(roots.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_pack_install_takes_back_the_parts_it_placed() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("pack-rollback");
        let staging = dir.join("staging");
        for root in ["Body", "Hair"] {
            std::fs::create_dir_all(staging.join(root)).unwrap();
            std::fs::write(staging.join(root).join("mod.ini"), b"[x]").unwrap();
        }
        std::fs::write(staging.join("Hair").join("b.dds"), b"data").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(staging.join("Hair").join("b.dds"))
            .unwrap();
        let mods_dir = dir.join("Mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        assert!(place_extracted(&staging, &mods_dir, "pack", Some("Ellen")).is_err());
        drop(lock);
        let left: Vec<String> = std::fs::read_dir(&mods_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn gb_part(id: &str, gb_mod: i64, gb_file: i64, name: &str, root: &str) -> Value {
        json!({
            "modId": id,
            "name": name,
            "source": { "kind": "gamebanana", "gbModId": gb_mod, "gbFileId": gb_file, "root": root },
        })
    }

    #[test]
    fn a_mod_without_a_preview_is_not_asked_about_every_session() {
        let now = 1_000_000_000;
        let mut library = json!({
            "genshin": {
                "Bare": gb_part("a", 1, 10, "Bare", ""),
                "Shown": gb_part("b", 2, 20, "Shown", ""),
                "Manual": { "modId": "c", "source": { "kind": "manual" } },
            },
        });
        let missing = |library: &Value, now| {
            let mut ids: Vec<u64> = missing_thumbnails_in(library, "genshin", now)
                .into_iter()
                .map(|(_, id)| id)
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(missing(&library, now), vec![1, 2]);

        let answers = [
            ("Bare".to_string(), None),
            ("Shown".to_string(), Some("https://img/b.jpg".to_string())),
            ("Removed".to_string(), Some("https://img/x.jpg".to_string())),
        ];
        assert!(apply_thumbnails(&mut library, "genshin", &answers, now));
        assert_eq!(library["genshin"]["Shown"]["thumbnailUrl"], "https://img/b.jpg");
        assert!(library["genshin"].get("Removed").is_none());
        assert!(missing(&library, now + 60).is_empty());
        assert_eq!(missing(&library, now + THUMBNAIL_RECHECK_SECS), vec![1]);
        assert_eq!(missing(&library, now - 60), vec![1]);
        assert!(!apply_thumbnails(&mut library, "hkrpg", &answers, now));
    }

    #[cfg(windows)]
    #[test]
    fn clearing_read_only_never_leaves_the_mod_through_a_junction() {
        let set_readonly = |path: &Path, on: bool| {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(on);
            std::fs::set_permissions(path, perms).unwrap();
        };
        let dir = scratch("junction");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let kept = outside.join("shared.dds");
        std::fs::write(&kept, b"data").unwrap();
        set_readonly(&kept, true);

        let mod_dir = dir.join("Mods").join("Keqing");
        std::fs::create_dir_all(&mod_dir).unwrap();
        let inside = mod_dir.join("mod.ini");
        std::fs::write(&inside, b"[x]").unwrap();
        set_readonly(&inside, true);
        let linked = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(mod_dir.join("Textures"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());

        clear_readonly(&mod_dir);
        assert!(!std::fs::metadata(&inside).unwrap().permissions().readonly());
        if linked {
            assert!(std::fs::metadata(&kept).unwrap().permissions().readonly());
            remove_dir_with_retry(&mod_dir).unwrap();
            assert!(kept.is_file());
        }
        set_readonly(&kept, false);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
