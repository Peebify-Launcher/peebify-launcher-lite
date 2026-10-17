// ------------ Install And Uninstall Engine ------------
// The work behind both setup flows, with no window code. Install verifies the payload, closes the running launcher,
// swaps the files in, then registers with Windows, adds shortcuts and checks WebView2. Uninstall removes it all again.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::consts;
use crate::ilog::ilog;
use crate::msg::{committed, status, warn, Cancel, EngineError, EventSink};
use crate::paths;
use crate::payload::Payload;
use crate::swap;
use crate::win;

pub const ROLLBACK_FAILED: &str = "rollback failed";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallManifest {
    pub version: String,
    pub main_binary: String,
    pub installed_at: String,
    pub desktop_shortcut: bool,
    #[serde(default)]
    pub created_dir: bool,
    pub files: Vec<String>,
}

impl InstallManifest {
    pub fn load(install_dir: &Path) -> Result<Self, String> {
        let path = install_dir.join(consts::INSTALL_MANIFEST_NAME);
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("parse install manifest: {e}"))
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(consts::INSTALL_MANIFEST_NAME), bytes)
            .map_err(|e| format!("write install manifest: {e}"))
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub install_dir: PathBuf,
    pub desktop_shortcut: bool,
    pub install_vc_redist: bool,
}

pub fn required_bytes(estimated_size_kb: u64) -> u64 {
    let size = estimated_size_kb.saturating_mul(1024);
    size.saturating_add(size / 7)
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} bytes")
}

fn check_free_space(dir: &Path, payload: &Payload) -> Result<(), String> {
    let needed = required_bytes(payload.manifest.estimated_size_kb);
    let Some(free) = win::free_space_bytes(dir) else {
        return Ok(());
    };
    if free >= needed {
        return Ok(());
    }
    Err(format!(
        "Not enough space to install here. Setup needs about {} and only {} is free. \
         Free some space or choose a folder on another drive.",
        human_bytes(needed),
        human_bytes(free)
    ))
}

fn recover_interrupted_update(
    dir: &Path,
    backup: &Path,
    sink: &EventSink,
) -> Option<InstallManifest> {
    let marker = dir.join(consts::UPDATE_STATE_NAME);
    if !marker.exists() {
        return None;
    }
    let mut fallback = None;
    if backup.exists() {
        status(sink, "Recovering from an interrupted update…", 4.0);
        ilog!("install: found an interrupted update, rolling it back first");
        if let Err(e) = swap::recover_interrupted(dir) {
            ilog!("install: could not roll back the interrupted update ({e}), installing over it");
            fallback = InstallManifest::load(backup).ok();
        }
    }
    let _ = std::fs::remove_file(&marker);
    fallback
}

pub fn perform_install(
    payload: &Payload,
    opts: &InstallOptions,
    cancel: &Cancel,
    sink: &EventSink,
) -> Result<(), EngineError> {
    let normalized = paths::normalize_install_dir(&opts.install_dir);
    let dir = &normalized;
    ilog!(
        "install: v{} -> {} (desktop shortcut: {})",
        payload.manifest.version,
        dir.display(),
        opts.desktop_shortcut
    );

    if let Err(refusal) = paths::validate_install_dir(dir) {
        ilog!("install: refusing target {}: {refusal:?}", dir.display());
        return Err(refusal.message().to_string().into());
    }

    payload.verify_with_progress(|frac| {
        status(sink, "Verifying installer…", frac * 4.0);
    })?;

    let main_exe = dir.join(&payload.manifest.main_binary);
    if !win::pids_by_exe_name(&payload.manifest.main_binary).is_empty() {
        status(sink, "Closing Peebify Launcher…", 4.0);
        if !win::close_all_by_name(
            &payload.manifest.main_binary,
            Duration::from_secs(4),
            Duration::from_secs(5),
        ) {
            return Err(format!(
                "{} is still running and could not be closed. Close it manually and run the installer again.",
                consts::PRODUCT_NAME
            )
            .into());
        }
    }
    if dir.is_dir() {
        release_install_dir(dir)?;
    }

    let staged = dir.join(consts::STAGED_DIR_NAME);
    let backup = dir.join(consts::BACKUP_DIR_NAME);
    let interrupted_manifest = recover_interrupted_update(dir, &backup, sink);
    let old_manifest = InstallManifest::load(dir).ok().or(interrupted_manifest);
    let created_dir = directory_ownership(dir, old_manifest.as_ref());
    check_free_space(dir, payload)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&backup);

    let mut installed: Vec<PathBuf> = Vec::new();
    let result = stage_and_swap(
        payload,
        opts,
        cancel,
        sink,
        dir,
        &staged,
        &backup,
        created_dir,
        old_manifest.as_ref(),
        &mut installed,
    );
    if let Err(e) = result {
        ilog!("install: failed ({e}), cleaning up");
        let rollback = swap::roll_back_swap(dir, &backup, &installed);
        let _ = std::fs::remove_dir_all(&staged);
        if created_dir {
            let _ = std::fs::remove_dir(dir);
        }
        if let Err(re) = rollback {
            ilog!("install: rollback failed ({re})");
            return Err(EngineError::Failed(format!("{e}; {ROLLBACK_FAILED}: {re}")));
        }
        return Err(e);
    }

    status(sink, "Registering with Windows…", 78.0);
    if let Err(e) = win::write_uninstall_entry(
        dir,
        &payload.manifest.version,
        payload.manifest.estimated_size_kb,
    ) {
        ilog!("install: uninstall entry failed: {e}");
        warn(
            sink,
            format!(
                "{} is installed, but Windows would not list it under Installed apps ({e}). \
                 Run {} in the install folder to remove it.",
                consts::PRODUCT_NAME,
                consts::UNINSTALLER_NAME
            ),
        );
    }
    win::fix_run_key(&main_exe);

    status(sink, "Creating shortcuts…", 82.0);
    let shortcut = |dir_of: Option<PathBuf>, what: &str| {
        let Some(parent) = dir_of else {
            ilog!("install: no {what} folder, skipping that shortcut");
            return;
        };
        let lnk = parent.join(consts::SHORTCUT_NAME);
        match win::create_shortcut(&lnk, &main_exe, dir, consts::PRODUCT_NAME) {
            Ok(()) => ilog!("install: wrote {}", lnk.display()),
            Err(e) => {
                ilog!("install: {what} shortcut failed: {e}");
                warn(
                    sink,
                    format!("The {what} shortcut could not be created ({e})."),
                );
            }
        }
    };
    shortcut(win::start_menu_programs_dir(), "Start menu");
    if opts.desktop_shortcut {
        shortcut(win::desktop_dir(), "desktop");
    }

    status(sink, "Checking WebView2 runtime…", 86.0);
    if let Err(e) = crate::prereqs::webview2::ensure(|phase, frac| {
        status(sink, phase, band(frac, 86.0, 90.0));
    }) {
        warn(
            sink,
            format!(
                "Setup couldn't install the Microsoft WebView2 runtime ({e}). {} needs it to \
                 open. Install it from {}, then start the launcher.",
                consts::PRODUCT_NAME,
                crate::prereqs::webview2::BOOTSTRAPPER_URL
            ),
        );
    }

    if opts.install_vc_redist {
        status(sink, "Checking Visual C++ runtime…", 92.0);
        if let Err(e) = crate::prereqs::vcredist::ensure(|phase, frac| {
            status(sink, phase, band(frac, 92.0, 97.0));
        }) {
            warn(sink, e);
        }
    }

    status(sink, "Finishing up…", 98.0);
    ilog!("install: complete");
    Ok(())
}

fn band(fraction: f32, from: f32, to: f32) -> f32 {
    if fraction < 0.0 {
        crate::msg::INDETERMINATE
    } else {
        from + fraction.clamp(0.0, 1.0) * (to - from)
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_and_swap(
    payload: &Payload,
    opts: &InstallOptions,
    cancel: &Cancel,
    sink: &EventSink,
    dir: &Path,
    staged: &Path,
    backup: &Path,
    created_dir: bool,
    old_manifest: Option<&InstallManifest>,
    installed: &mut Vec<PathBuf>,
) -> Result<(), EngineError> {
    status(sink, "Copying files…", 5.0);
    let files = payload.extract_to(staged, cancel, |frac| {
        status(sink, "Copying files…", 5.0 + frac * 60.0);
    })?;

    if old_manifest.is_none() {
        let collisions = swap::foreign_collisions(dir, &files);
        if !collisions.is_empty() {
            ilog!(
                "install: {} already holds {collisions:?}, refusing to replace them",
                dir.display()
            );
            return Err(swap::collision_message(&collisions).into());
        }
    }

    status(sink, "Writing uninstaller…", 68.0);
    payload.write_stub_copy(&staged.join(consts::UNINSTALLER_NAME))?;

    let replaceable = swap::replaceable_names(old_manifest, &files);
    InstallManifest {
        version: payload.manifest.version.clone(),
        main_binary: payload.manifest.main_binary.clone(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        desktop_shortcut: opts.desktop_shortcut,
        created_dir,
        files,
    }
    .save(staged)?;

    cancel.check()?;
    committed(sink);

    status(sink, "Installing files…", 72.0);
    swap::swap(dir, staged, backup, &replaceable, installed)?;
    let _ = std::fs::remove_dir_all(backup);
    let _ = std::fs::remove_dir_all(staged);
    Ok(())
}

fn release_install_dir(dir: &Path) -> Result<(), String> {
    let running = |name: &str| !win::pids_by_exe_name_under(name, dir).is_empty();
    for name in consts::HELPER_BINARIES {
        let pids = win::pids_by_exe_name_under(name, dir);
        if pids.is_empty() {
            continue;
        }
        ilog!("install: closing {name} ({} running)", pids.len());
        win::close_pids(&pids, Duration::from_secs(2), Duration::from_secs(3));
    }
    let hook_dll = dir.join(consts::RESOURCES_DIR_NAME).join(consts::HOOK_DLL_NAME);
    if win::file_in_use(&hook_dll) {
        ilog!("install: {} is still loaded by a running process", hook_dll.display());
        return Err(
            "Close your game first. Peebify's FPS unlocker is still attached to it.".into(),
        );
    }
    if let Some(name) = consts::HELPER_BINARIES.into_iter().find(|name| running(name)) {
        ilog!("install: {name} is still running and could not be closed");
        return Err(format!(
            "Close your game first. {name} from {} is still running.",
            consts::PRODUCT_NAME
        ));
    }
    Ok(())
}

pub fn launch_app(install_dir: &Path, main_binary: &str) -> Result<(), String> {
    if win::is_elevated() {
        ilog!("launch: setup is running as administrator, not starting the launcher elevated");
        return Err(format!(
            "setup ran as administrator, so it did not start {} with those rights. \
             Open it from the Start menu",
            consts::PRODUCT_NAME
        ));
    }
    win::spawn_detached(&install_dir.join(main_binary), &[], Some(install_dir))
}

// ------------ Uninstall Helpers ------------
// Uninstall starts here. These work out what to remove: the launcher files, installed games, settings and cache,
// always sparing anything that is not ours, anything in Steam libraries and the captures folder.
#[derive(Debug, Clone)]
pub struct UninstallOptions {
    pub install_dir: PathBuf,
    pub remove_data: bool,
    pub remove_games: bool,
}

fn read_launcher_config() -> Option<serde_json::Value> {
    let path = consts::user_data_dir()?.join("launcher-config.json");
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn configured_game_paths(config: Option<&serde_json::Value>) -> Vec<PathBuf> {
    config
        .and_then(|json| json.get("games"))
        .and_then(|games| games.as_object())
        .map(|games| {
            games
                .values()
                .filter_map(|game| game.get("gamePath").and_then(|p| p.as_str()))
                .map(|path| PathBuf::from(path.trim()))
                .collect()
        })
        .unwrap_or_default()
}

fn configured_behavior_folder(config: Option<&serde_json::Value>, key: &str) -> Option<PathBuf> {
    config?
        .get("behavior")?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn configured_capture_folder(config: Option<&serde_json::Value>) -> Option<PathBuf> {
    configured_behavior_folder(config, "overlayCaptureFolder")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameDirVerdict {
    Removable,
    Steam,
    Guarded,
}

pub fn game_dir_verdict(dir: &Path) -> GameDirVerdict {
    if !paths::safe_to_remove_game_dir(dir) {
        GameDirVerdict::Guarded
    } else if paths::in_steam_library(dir) {
        GameDirVerdict::Steam
    } else {
        GameDirVerdict::Removable
    }
}

fn game_removal_phase(game_dir: &Path) -> String {
    let name = game_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| game_dir.display().to_string());
    format!("Removing {name}…")
}

pub fn installed_game_dirs(install_dir: &Path) -> Vec<PathBuf> {
    game_dirs_from(install_dir, read_launcher_config().as_ref())
}

fn game_dirs_from(install_dir: &Path, config: Option<&serde_json::Value>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dir.as_os_str().is_empty()
            && dir.is_dir()
            && !dirs.iter().any(|existing| existing == &dir)
        {
            dirs.push(dir);
        }
    };

    for path in configured_game_paths(config) {
        push(path);
    }

    if let Ok(entries) = std::fs::read_dir(install_dir.join(consts::GAMES_DIR_NAME)) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                push(entry.path());
            }
        }
    }
    dirs
}

pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => stack.push(entry.path()),
                Ok(meta) => total = total.saturating_add(meta.len()),
                Err(_) => {}
            }
        }
    }
    total
}

fn sweep_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn is_same_or_inside(key: &str, ancestor: &str) -> bool {
    key.strip_prefix(ancestor)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

fn remove_dir_contents_keeping(dir: &Path, keep: &[PathBuf]) -> Vec<PathBuf> {
    let keep: Vec<String> = keep.iter().map(|path| sweep_key(path)).collect();
    let dir_key = sweep_key(dir);
    let mut leftovers = Vec::new();
    if keep.iter().any(|kept| is_same_or_inside(&dir_key, kept)) {
        return leftovers;
    }
    sweep_keeping(dir, &keep, &mut leftovers);
    leftovers
}

fn sweep_keeping(dir: &Path, keep: &[String], leftovers: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let key = sweep_key(&path);
        if keep.contains(&key) {
            continue;
        }
        if keep.iter().any(|kept| is_same_or_inside(kept, &key)) {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                sweep_keeping(&path, keep, leftovers);
            }
            continue;
        }
        let result = win::remove_quietly(|| {
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
        });
        if let Err(e) = result {
            ilog!("sweep: could not remove {} ({e})", path.display());
            leftovers.push(path);
        }
    }
}

fn report_leftovers(sink: &EventSink, dir: &Path, leftovers: &[PathBuf]) {
    let leftovers: Vec<&PathBuf> = leftovers.iter().filter(|path| path.exists()).collect();
    if leftovers.is_empty() {
        return;
    }
    ilog!(
        "uninstall: {} item(s) in {} could not be removed",
        leftovers.len(),
        dir.display()
    );
    warn(
        sink,
        format!(
            "Setup could not remove {} item(s) from {}. Close any program using them and delete them by hand.",
            leftovers.len(),
            dir.display()
        ),
    );
}

fn rebase_inside(root: &Path, path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let path = paths::normalize_install_dir(path);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.as_path();
    loop {
        if paths::same_path(cur, root) {
            return Some(
                tail.iter()
                    .rev()
                    .fold(root.to_path_buf(), |acc, part| acc.join(part)),
            );
        }
        tail.push(cur.file_name()?.to_os_string());
        cur = cur.parent()?;
    }
}

fn keep_inside(root: &Path, candidates: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut keep: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        let Some(inside) = rebase_inside(root, &candidate) else {
            continue;
        };
        if !keep.iter().any(|kept| sweep_key(kept) == sweep_key(&inside)) {
            keep.push(inside);
        }
    }
    keep
}

fn owned_dir_keep_list(
    dir: &Path,
    opts: &UninstallOptions,
    config: Option<&serde_json::Value>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if !opts.remove_games {
        candidates.push(dir.join(consts::GAMES_DIR_NAME));
        candidates.extend(configured_game_paths(config));
    }
    if !opts.remove_data {
        candidates.extend(configured_behavior_folder(config, "modsPath"));
    }
    candidates.extend(configured_capture_folder(config));
    keep_inside(dir, candidates)
}

fn data_dir_keep_list(
    data: &Path,
    install_dir: &Path,
    opts: &UninstallOptions,
    config: Option<&serde_json::Value>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if !opts.remove_games {
        candidates.extend(game_dirs_from(install_dir, config));
    }
    candidates.extend(configured_capture_folder(config));
    candidates.push(data.join("logs").join("installer.log"));
    keep_inside(data, candidates)
}

fn app_cache_removals(cache: &Path, remove_data: bool) -> Vec<PathBuf> {
    if remove_data {
        return vec![cache.to_path_buf()];
    }
    let profile = cache.join("EBWebView").join("Default");
    vec![
        profile.join("Cache"),
        profile.join("Code Cache"),
        profile.join("GPUCache"),
    ]
}

fn is_plain_relative(rel: &str) -> bool {
    use std::path::Component;
    let mut named = false;
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return false,
        }
    }
    named
}

fn manifest_entry_path(dir: &Path, rel: &str) -> Option<PathBuf> {
    if !is_plain_relative(rel) {
        return None;
    }
    let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    paths::is_strictly_inside(&path, dir).then_some(path)
}

fn manifest_parent_dirs(files: &[String]) -> Vec<String> {
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rel in files {
        if !is_plain_relative(rel) {
            continue;
        }
        let parts: Vec<&str> = rel
            .split(['/', '\\'])
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        for depth in 1..parts.len() {
            dirs.insert(parts[..depth].join("/"));
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));
    dirs
}

fn prune_manifest_dirs(dir: &Path, manifest: Option<&InstallManifest>) {
    for rel in manifest
        .map(|m| manifest_parent_dirs(&m.files))
        .unwrap_or_default()
    {
        if let Some(path) = manifest_entry_path(dir, &rel) {
            let _ = std::fs::remove_dir(path);
        }
    }
    let _ = std::fs::remove_dir(dir);
}

fn remove_known_layout(dir: &Path, keep: &[PathBuf]) -> Vec<PathBuf> {
    let mut leftovers = Vec::new();
    if !dir.join(consts::MAIN_BINARY).is_file() {
        return leftovers;
    }
    for name in swap::expected_top_names() {
        let path = dir.join(&name);
        if path.is_dir() {
            leftovers.extend(remove_dir_contents_keeping(&path, keep));
            let _ = std::fs::remove_dir(&path);
            continue;
        }
        let result = win::remove_quietly(|| std::fs::remove_file(&path));
        if let Err(e) = result {
            ilog!("uninstall: could not remove {} ({e})", path.display());
            leftovers.push(path);
        }
    }
    leftovers
}

fn directory_ownership(dir: &Path, old: Option<&InstallManifest>) -> bool {
    match old {
        Some(old) => old.created_dir,
        None => !dir.exists() || paths::may_claim_existing_dir(dir),
    }
}

fn owns_directory(dir: &Path, manifest: Option<&InstallManifest>) -> bool {
    manifest.map(|m| m.created_dir).unwrap_or(false) || paths::is_default_install_dir(dir)
}

// ------------ Uninstall Run ------------
// respawn_for_uninstall copies setup to a temp folder and runs it from there (the second stage), because the
// installed uninstall.exe cannot delete its own folder. The second stage either opens the uninstall window to let you
// choose what to remove, or goes straight to removing. perform_uninstall then does the removal step by step.
const UNINSTALL_RUN_PREFIX: &str = "peebify-uninstall";

fn is_run_dir(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(&format!("{UNINSTALL_RUN_PREFIX}-")))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SecondStage {
    Choose,
    Run,
    Silent,
}

pub fn respawn_for_uninstall(opts: &UninstallOptions, stage: SecondStage) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let run_dir =
        win::create_run_dir(UNINSTALL_RUN_PREFIX).map_err(|e| format!("stage uninstaller: {e}"))?;
    let temp_copy = run_dir.join(format!("{UNINSTALL_RUN_PREFIX}.exe"));
    if let Err(e) = std::fs::copy(&current, &temp_copy) {
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(format!("stage uninstaller: {e}"));
    }
    let dir_arg = opts.install_dir.display().to_string();
    let mut args: Vec<&str> = vec!["/uninstall", "--second-stage", "--dir", &dir_arg];
    if stage == SecondStage::Choose {
        args.push("--choose");
    }
    if opts.remove_data {
        args.push("--remove-data");
    }
    if opts.remove_games {
        args.push("--remove-games");
    }
    if stage == SecondStage::Silent {
        args.push("/S");
    }
    if let Err(e) = win::spawn_detached(&temp_copy, &args, Some(&run_dir)) {
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(e);
    }
    ilog!(
        "uninstall: second stage spawned from {}",
        temp_copy.display()
    );
    Ok(())
}

pub fn perform_uninstall(opts: &UninstallOptions, sink: &EventSink) -> Result<(), EngineError> {
    let dir = &opts.install_dir;
    ilog!(
        "uninstall: {} (remove data: {}, remove games: {})",
        dir.display(),
        opts.remove_data,
        opts.remove_games
    );

    status(sink, "Closing Peebify Launcher…", 5.0);
    let main_binary = InstallManifest::load(dir)
        .map(|m| m.main_binary)
        .unwrap_or_else(|_| consts::MAIN_BINARY.to_string());
    if !win::close_all_by_name(&main_binary, Duration::from_secs(4), Duration::from_secs(5)) {
        return Err(format!(
            "{} is still running and could not be closed. Close it manually and try again.",
            consts::PRODUCT_NAME
        )
        .into());
    }
    release_install_dir(dir)?;

    status(sink, "Removing files…", 20.0);
    let manifest = InstallManifest::load(dir).ok();
    let owned = owns_directory(dir, manifest.as_ref());
    let config = read_launcher_config();
    let mut leftovers: Vec<PathBuf> = Vec::new();
    match &manifest {
        Some(manifest) => {
            let total = manifest.files.len().max(1);
            for (i, rel) in manifest.files.iter().enumerate() {
                let Some(path) = manifest_entry_path(dir, rel) else {
                    ilog!(
                        "uninstall: skipping manifest entry {rel:?}, it is not inside {}",
                        dir.display()
                    );
                    continue;
                };
                let result = win::remove_quietly(|| std::fs::remove_file(&path));
                if let Err(e) = result {
                    ilog!("uninstall: could not remove {} ({e})", path.display());
                    leftovers.push(path);
                }
                status(
                    sink,
                    "Removing files…",
                    20.0 + (i as f32 / total as f32) * 50.0,
                );
            }
        }
        None => {
            warn(
                sink,
                "The install record was missing, so setup removed only the launcher's own \
                 files and folders."
                    .to_string(),
            );
            if !owned {
                let keep = owned_dir_keep_list(dir, opts, config.as_ref());
                leftovers.extend(remove_known_layout(dir, &keep));
            }
        }
    }
    for name in [
        consts::INSTALL_MANIFEST_NAME,
        consts::UNINSTALLER_NAME,
        consts::UPDATE_STATE_NAME,
    ] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    for name in [consts::STAGED_DIR_NAME, consts::BACKUP_DIR_NAME] {
        let _ = std::fs::remove_dir_all(dir.join(name));
    }

    let games_dir = dir.join(consts::GAMES_DIR_NAME);
    if !owned {
        ilog!(
            "uninstall: {} was not created by setup, removing only our files",
            dir.display()
        );
        prune_manifest_dirs(dir, manifest.as_ref());
    } else {
        let keep = owned_dir_keep_list(dir, opts, config.as_ref());
        if keep.is_empty() {
            let result = win::remove_quietly(|| std::fs::remove_dir_all(dir));
            if let Err(e) = result {
                warn(sink, format!("Could not remove {}: {e}", dir.display()));
            }
        } else {
            for kept in &keep {
                ilog!("uninstall: keeping {}", kept.display());
            }
            leftovers.extend(remove_dir_contents_keeping(dir, &keep));
        }
    }
    report_leftovers(sink, dir, &leftovers);

    status(sink, "Removing shortcuts and registry entries…", 80.0);
    let registered = win::registered_install_location();
    let other_install = registered
        .as_deref()
        .filter(|registered| !paths::same_path(registered, dir));
    match other_install {
        Some(registered) => {
            ilog!(
                "uninstall: keeping the shortcuts and install entry of {}",
                registered.display()
            );
        }
        None => {
            win::remove_shortcuts();
            win::delete_uninstall_entry();
            win::delete_app_user_model_id();
        }
    }
    remove_startup_registration(dir);

    if let Some(cache) = consts::app_cache_dir() {
        for target in app_cache_removals(&cache, opts.remove_data) {
            let result = win::remove_quietly(|| std::fs::remove_dir_all(&target));
            if let Err(e) = result {
                ilog!("uninstall: could not remove the cache folder {} ({e})", target.display());
            }
        }
    }
    if opts.remove_games {
        status(sink, "Removing installed games…", 85.0);
        let game_dirs = game_dirs_from(dir, config.as_ref());
        let count = game_dirs.len();
        for (i, game_dir) in game_dirs.into_iter().enumerate() {
            status(
                sink,
                &game_removal_phase(&game_dir),
                band(i as f32 / count as f32, 85.0, 90.0),
            );
            match game_dir_verdict(&game_dir) {
                GameDirVerdict::Removable => {}
                GameDirVerdict::Steam => {
                    warn(
                        sink,
                        format!("Kept {} because Steam manages that folder.", game_dir.display()),
                    );
                    continue;
                }
                GameDirVerdict::Guarded => {
                    warn(
                        sink,
                        format!(
                            "Kept {} because setup cannot safely delete that folder.",
                            game_dir.display()
                        ),
                    );
                    continue;
                }
            }
            ilog!("uninstall: removing game folder {}", game_dir.display());
            let result = win::remove_quietly(|| std::fs::remove_dir_all(&game_dir));
            if let Err(e) = result {
                warn(
                    sink,
                    format!("Could not remove {}: {e}", game_dir.display()),
                );
            }
        }
    }
    if !owned {
        let _ = std::fs::remove_dir(&games_dir);
        let _ = std::fs::remove_dir(dir);
    }
    if opts.remove_data {
        status(sink, "Removing settings and data…", 90.0);
        if let Some(data) = consts::user_data_dir() {
            let device_id = device_id_in(config.as_ref());
            let keep = data_dir_keep_list(&data, dir, opts, config.as_ref());
            if keep.is_empty() {
                let result = win::remove_quietly(|| std::fs::remove_dir_all(&data));
                if let Err(e) = result {
                    ilog!("uninstall: could not remove the data folder {} ({e})", data.display());
                }
            } else {
                for kept in &keep {
                    ilog!("uninstall: keeping {} inside the data folder", kept.display());
                }
                let leftovers = remove_dir_contents_keeping(&data, &keep);
                report_leftovers(sink, &data, &leftovers);
            }
            if let Some(id) = device_id {
                restore_device_id(&data, &id);
            }
        }
    }

    ilog!("uninstall: complete");
    Ok(())
}

// ------------ Startup Entry And Self Delete ------------
// Last bits of uninstall: keeping the device id when settings are removed, dropping the start with Windows entry
// and scheduling the temp copy of setup to delete itself.
fn device_id_in(config: Option<&serde_json::Value>) -> Option<String> {
    let id = config?.get("deviceId")?.as_str()?.trim().to_string();
    let valid =
        (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then_some(id)
}

fn restore_device_id(data_dir: &Path, device_id: &str) {
    if std::fs::create_dir_all(data_dir).is_err() {
        return;
    }
    let body = serde_json::json!({ "deviceId": device_id }).to_string();
    if std::fs::write(data_dir.join("launcher-config.json"), body).is_ok() {
        ilog!("uninstall: kept deviceId so a reinstall rejoins the same device");
    }
}

fn run_command_exe(cmd: &str) -> Option<&str> {
    let cmd = cmd.trim_start();
    let exe = match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next(),
        None => cmd.split(' ').next(),
    };
    exe.filter(|exe| !exe.is_empty())
}

fn run_command_targets(cmd: &str, dir: &Path) -> bool {
    run_command_exe(cmd)
        .and_then(|exe| Path::new(exe).parent())
        .is_some_and(|parent| paths::same_path(parent, dir))
}

fn remove_startup_registration(dir: &Path) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey_with_flags(consts::RUN_KEY, KEY_READ | KEY_SET_VALUE) {
        if let Ok(cmd) = key.get_value::<String, _>(consts::RUN_VALUE) {
            if run_command_targets(&cmd, dir) {
                let _ = key.delete_value(consts::RUN_VALUE);
            }
        }
    }
}

fn self_delete_command(me: &Path, system32: &Path) -> String {
    let mut script = format!(
        "\"{}\" 127.0.0.1 -n 4 >nul & del /f \"{}\"",
        system32.join("PING.EXE").display(),
        me.display()
    );
    if let Some(run_dir) = me.parent().filter(|dir| is_run_dir(dir)) {
        script.push_str(&format!(" & rmdir \"{}\"", run_dir.display()));
    }
    format!("/S /C \"{script}\"")
}

pub fn schedule_self_delete() {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    use crate::win::CREATE_NO_WINDOW;
    use std::os::windows::process::CommandExt;
    let system32 = win::system32_dir();
    let _ = std::process::Command::new(system32.join("cmd.exe"))
        .raw_arg(self_delete_command(&me, &system32))
        .current_dir(&system32)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

// ------------ Tests ------------
// Mostly uninstall safety: sweeping, keep lists, folder ownership and manifest paths that try to escape the folder.
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "peebify-install-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest(created_dir: bool, files: &[&str]) -> InstallManifest {
        InstallManifest {
            version: "1.0.0".to_string(),
            main_binary: consts::MAIN_BINARY.to_string(),
            installed_at: String::new(),
            desktop_shortcut: false,
            created_dir,
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn sweep_spares_games_folder_in_any_case() {
        let dir = scratch("games-case");
        std::fs::create_dir_all(dir.join("Games").join("Some Game")).unwrap();
        std::fs::write(dir.join("Games").join("Some Game").join("game.bin"), b"x").unwrap();
        std::fs::write(dir.join(consts::MAIN_BINARY), b"x").unwrap();
        std::fs::create_dir_all(dir.join("resources")).unwrap();

        remove_dir_contents_keeping(&dir, &[dir.join(consts::GAMES_DIR_NAME)]);

        assert!(dir.join("Games").join("Some Game").join("game.bin").exists());
        assert!(!dir.join(consts::MAIN_BINARY).exists());
        assert!(!dir.join("resources").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_descends_into_ancestors_of_kept_paths() {
        let dir = scratch("nested");
        std::fs::create_dir_all(dir.join("A").join("Keep")).unwrap();
        std::fs::write(dir.join("A").join("Keep").join("clip.mp4"), b"x").unwrap();
        std::fs::write(dir.join("A").join("other.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();

        let leftovers = remove_dir_contents_keeping(&dir, &[dir.join("a").join("keep")]);

        assert!(leftovers.is_empty());
        assert!(dir.join("A").join("Keep").join("clip.mp4").exists());
        assert!(!dir.join("A").join("other.txt").exists());
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_reports_files_it_could_not_remove() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("locked");
        std::fs::write(dir.join("locked.bin"), b"x").unwrap();
        std::fs::write(dir.join("free.bin"), b"x").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join("locked.bin"))
            .unwrap();

        let leftovers = remove_dir_contents_keeping(&dir, &[]);

        assert_eq!(leftovers, vec![dir.join("locked.bin")]);
        assert!(!dir.join("free.bin").exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_keeps_everything_when_the_root_itself_is_kept() {
        let dir = scratch("root-kept");
        std::fs::write(dir.join("capture.png"), b"x").unwrap();

        remove_dir_contents_keeping(&dir, std::slice::from_ref(&dir));

        assert!(dir.join("capture.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parent_dirs_are_deepest_first_and_skip_escapes() {
        let files: Vec<String> = [
            "a/b/c.dll",
            "a/d.txt",
            "root.exe",
            "../evil/x.bin",
            "e\\f.bin",
            "C:/Users/x/Documents/a.txt",
            "/rooted/g.bin",
        ]
        .iter()
        .map(|f| f.to_string())
        .collect();
        assert_eq!(manifest_parent_dirs(&files), vec!["a/b", "a", "e"]);
    }

    #[test]
    fn manifest_entries_outside_the_folder_are_skipped() {
        let dir = scratch("manifest-escape");
        let outside = scratch("manifest-escape-outside");
        std::fs::write(outside.join("victim.txt"), b"x").unwrap();
        let absolute = outside.join("victim.txt").display().to_string();
        let slashed = absolute.replace('\\', "/");
        for rel in [
            absolute.as_str(),
            slashed.as_str(),
            "../victim.txt",
            "resources/../../victim.txt",
            "resources/..",
            "/victim.txt",
            "\\victim.txt",
            "C:victim.txt",
            r"\\?\C:\victim.txt",
            "",
            ".",
        ] {
            assert_eq!(manifest_entry_path(&dir, rel), None, "{rel}");
        }
        assert_eq!(
            manifest_entry_path(&dir, "resources/app.dll"),
            Some(dir.join("resources").join("app.dll"))
        );
        assert_eq!(
            manifest_entry_path(&dir, "resources\\./app.dll"),
            Some(dir.join("resources").join("app.dll"))
        );
        assert_eq!(
            manifest_entry_path(&dir, consts::MAIN_BINARY),
            Some(dir.join(consts::MAIN_BINARY))
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn manifest_entries_through_a_junction_are_skipped() {
        let dir = scratch("manifest-junction");
        let outside = scratch("manifest-junction-outside");
        std::fs::write(outside.join("victim.txt"), b"x").unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        let link = dir.join("link");
        let made = std::process::Command::new(win::system32_dir().join("cmd.exe"))
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|out| out.status.success());
        assert!(made, "mklink /J failed");

        assert!(link.join("victim.txt").exists());
        assert_eq!(manifest_entry_path(&dir, "link/victim.txt"), None);
        prune_manifest_dirs(&dir, Some(&manifest(false, &["link/sub/x.dll"])));
        assert!(outside.join("sub").is_dir());

        let _ = std::fs::remove_dir(&link);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn self_delete_uses_absolute_tools_and_clears_only_its_run_dir() {
        let system32 = Path::new(r"C:\Windows\System32");
        let run_dir = std::env::temp_dir().join(format!("{UNINSTALL_RUN_PREFIX}-1-2-3"));
        let staged =
            self_delete_command(&run_dir.join(format!("{UNINSTALL_RUN_PREFIX}.exe")), system32);
        assert!(staged.starts_with("/S /C \"\"C:\\Windows\\System32\\PING.EXE\" 127.0.0.1"));
        assert!(staged.ends_with(&format!(" & rmdir \"{}\"\"", run_dir.display())));

        assert!(!is_run_dir(Path::new(r"C:\Temp\peebify-uninstaller")));

        let installed = Path::new(r"D:\Games\Peebify Launcher").join(consts::UNINSTALLER_NAME);
        let command = self_delete_command(&installed, system32);
        assert!(command.contains(&format!("del /f \"{}\"", installed.display())));
        assert!(!command.contains("rmdir"));
    }

    #[test]
    fn borrowed_folder_prune_leaves_foreign_empty_dirs() {
        let dir = scratch("borrowed");
        std::fs::create_dir_all(dir.join("resources").join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("Saved")).unwrap();
        std::fs::create_dir_all(dir.join("Other Game").join("Logs")).unwrap();

        prune_manifest_dirs(&dir, Some(&manifest(false, &["resources/sub/x.dll"])));

        assert!(!dir.join("resources").exists());
        assert!(dir.join("Saved").is_dir());
        assert!(dir.join("Other Game").join("Logs").is_dir());
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn borrowed_folder_prune_removes_the_folder_once_empty() {
        let dir = scratch("borrowed-empty");
        std::fs::create_dir_all(dir.join("resources")).unwrap();

        prune_manifest_dirs(&dir, Some(&manifest(false, &["resources/x.dll"])));

        assert!(!dir.exists());
    }

    #[test]
    fn ownership_is_claimed_only_for_folders_setup_makes() {
        let root = scratch("ownership");
        let missing = root.join("Missing");
        let empty = root.join("Games");
        let busy = root.join("Busy");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("notes.txt"), b"x").unwrap();

        assert!(directory_ownership(&missing, None));
        assert!(!directory_ownership(&empty, None));
        assert!(!directory_ownership(&busy, None));
        assert!(directory_ownership(&busy, Some(&manifest(true, &[]))));
        assert!(!directory_ownership(&missing, Some(&manifest(false, &[]))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_install_dir_is_owned_without_a_record() {
        let root = scratch("owns");
        assert!(owns_directory(&consts::default_install_dir(), None));
        assert!(owns_directory(&consts::default_install_dir(), Some(&manifest(false, &[]))));
        assert!(!owns_directory(&root, None));
        assert!(!owns_directory(&root, Some(&manifest(false, &[]))));
        assert!(owns_directory(&root, Some(&manifest(true, &[]))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_inside_rebases_other_spellings_and_drops_outside_paths() {
        let root = scratch("rebase");
        let outside = scratch("rebase-outside");
        std::fs::create_dir_all(root.join("Mods")).unwrap();
        let slashed = PathBuf::from(format!("{}/MODS", root.display()).replace('\\', "/"));

        let keep = keep_inside(&root, [slashed, outside.clone(), root.join("Absent")]);

        assert_eq!(keep.len(), 1);
        assert_eq!(sweep_key(&keep[0]), sweep_key(&root.join("Mods")));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn owned_keep_list_follows_the_options() {
        let dir = scratch("keep-list");
        std::fs::create_dir_all(dir.join(consts::GAMES_DIR_NAME)).unwrap();
        std::fs::create_dir_all(dir.join("Captures")).unwrap();
        std::fs::create_dir_all(dir.join("Mods")).unwrap();
        std::fs::create_dir_all(dir.join("Wuthering Waves")).unwrap();
        let config = serde_json::json!({
            "games": { "wuwa": { "gamePath": dir.join("Wuthering Waves").display().to_string() } },
            "behavior": {
                "overlayCaptureFolder": dir.join("Captures").display().to_string(),
                "modsPath": dir.join("Mods").display().to_string()
            }
        });
        let opts = |remove_data: bool, remove_games: bool| UninstallOptions {
            install_dir: dir.clone(),
            remove_data,
            remove_games,
        };
        let captures = sweep_key(&dir.join("Captures"));
        let keeps_captures = |keep: &[PathBuf]| keep.iter().any(|kept| sweep_key(kept) == captures);

        for (remove_data, remove_games, expected) in
            [(false, false, 4), (true, false, 3), (false, true, 2), (true, true, 1)]
        {
            let keep = owned_dir_keep_list(&dir, &opts(remove_data, remove_games), Some(&config));
            assert_eq!(keep.len(), expected);
            assert!(keeps_captures(&keep));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_keep_list_always_spares_captures() {
        let data = scratch("data-keep-list");
        let install = scratch("data-keep-list-install");
        std::fs::create_dir_all(data.join("Captures")).unwrap();
        std::fs::create_dir_all(data.join("Mods")).unwrap();
        std::fs::create_dir_all(data.join("Wuthering Waves")).unwrap();
        let config = serde_json::json!({
            "games": { "wuwa": { "gamePath": data.join("Wuthering Waves").display().to_string() } },
            "behavior": {
                "overlayCaptureFolder": data.join("Captures").display().to_string(),
                "modsPath": data.join("Mods").display().to_string()
            }
        });
        let opts = |remove_games: bool| UninstallOptions {
            install_dir: install.clone(),
            remove_data: true,
            remove_games,
        };

        assert_eq!(data_dir_keep_list(&data, &install, &opts(false), Some(&config)).len(), 2);
        let keep = data_dir_keep_list(&data, &install, &opts(true), Some(&config));
        assert_eq!(keep.len(), 1);
        assert_eq!(sweep_key(&keep[0]), sweep_key(&data.join("Captures")));
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&install);
    }

    #[test]
    fn app_cache_keeps_the_webview_profile_unless_data_goes() {
        let cache = scratch("app-cache");

        assert_eq!(app_cache_removals(&cache, true), vec![cache.clone()]);
        let removals = app_cache_removals(&cache, false);
        assert!(removals.contains(&cache.join("EBWebView").join("Default").join("Cache")));
        assert!(!removals.contains(&cache.join("EBWebView")));
        assert!(!removals.contains(&cache.join("EBWebView").join("Default")));
        assert!(removals.iter().all(|path| path.starts_with(&cache) && path != &cache));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn install_recovers_an_interrupted_update_first() {
        let dir = scratch("interrupted");
        let backup = dir.join(consts::BACKUP_DIR_NAME);
        std::fs::create_dir_all(&backup).unwrap();
        manifest(true, &[consts::MAIN_BINARY]).save(&backup).unwrap();
        std::fs::write(backup.join(consts::MAIN_BINARY), b"old").unwrap();
        std::fs::write(dir.join(consts::UPDATE_STATE_NAME), b"{}").unwrap();

        let fallback = recover_interrupted_update(&dir, &backup, &crate::msg::log_sink());

        assert!(fallback.is_none());
        assert!(!dir.join(consts::UPDATE_STATE_NAME).exists());
        assert!(!backup.exists());
        assert!(InstallManifest::load(&dir).unwrap().created_dir);
        assert_eq!(std::fs::read(dir.join(consts::MAIN_BINARY)).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_marker_without_backup_is_cleared() {
        let dir = scratch("stale-marker");
        std::fs::write(dir.join(consts::UPDATE_STATE_NAME), b"{}").unwrap();

        let backup = dir.join(consts::BACKUP_DIR_NAME);
        let fallback = recover_interrupted_update(&dir, &backup, &crate::msg::log_sink());

        assert!(fallback.is_none());
        assert!(!dir.join(consts::UPDATE_STATE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_record_removes_only_the_payload_layout() {
        let dir = scratch("no-record");
        std::fs::write(dir.join(consts::MAIN_BINARY), b"x").unwrap();
        std::fs::create_dir_all(dir.join("resources").join("sub")).unwrap();
        std::fs::write(dir.join("resources").join("sub").join("a.dll"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("icons")).unwrap();
        std::fs::create_dir_all(dir.join("resources").join("Captures")).unwrap();
        std::fs::write(dir.join("resources").join("Captures").join("shot.png"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(dir.join(consts::GAMES_DIR_NAME)).unwrap();

        let keep = [dir.join("resources").join("Captures")];
        let leftovers = remove_known_layout(&dir, &keep);

        assert!(leftovers.is_empty());
        assert!(!dir.join(consts::MAIN_BINARY).exists());
        assert!(!dir.join("icons").exists());
        assert!(!dir.join("resources").join("sub").exists());
        assert!(dir.join("resources").join("Captures").join("shot.png").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(dir.join(consts::GAMES_DIR_NAME).is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_record_without_the_launcher_leaves_the_folder_alone() {
        let dir = scratch("no-record-foreign");
        std::fs::create_dir_all(dir.join("resources")).unwrap();
        std::fs::create_dir_all(dir.join("icons")).unwrap();

        assert!(remove_known_layout(&dir, &[]).is_empty());

        assert!(dir.join("resources").is_dir());
        assert!(dir.join("icons").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_value_must_name_this_folder_exactly() {
        let dir = Path::new(r"D:\Apps\Peebify");
        let sibling = r#""D:\Apps\Peebify Launcher\Peebify Launcher.exe" --from-boot"#;
        let ours = r#""d:\apps\peebify\Peebify Launcher.exe" --from-boot"#;
        let unquoted = r"D:\Apps\Peebify\launcher.exe --from-boot";
        assert!(!run_command_targets(sibling, dir));
        assert!(run_command_targets(ours, dir));
        assert!(run_command_targets(unquoted, dir));
        assert!(!run_command_targets("", dir));
        assert!(!run_command_targets("\"\"", dir));
        assert_eq!(
            run_command_exe(sibling),
            Some(r"D:\Apps\Peebify Launcher\Peebify Launcher.exe")
        );
    }

    #[test]
    fn data_keep_list_spares_the_running_log() {
        let data = scratch("data-keep-log");
        let install = scratch("data-keep-log-install");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::write(data.join("logs").join("installer.log"), b"x").unwrap();
        std::fs::write(data.join("logs").join("launcher.log"), b"x").unwrap();
        std::fs::write(data.join("launcher-config.json"), b"{}").unwrap();
        let opts = UninstallOptions {
            install_dir: install.clone(),
            remove_data: true,
            remove_games: true,
        };

        let keep = data_dir_keep_list(&data, &install, &opts, None);
        let leftovers = remove_dir_contents_keeping(&data, &keep);

        assert!(leftovers.is_empty());
        assert!(data.join("logs").join("installer.log").exists());
        assert!(!data.join("logs").join("launcher.log").exists());
        assert!(!data.join("launcher-config.json").exists());
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&install);
    }

    #[test]
    fn game_dir_verdict_keeps_steam_and_guarded_folders() {
        assert_eq!(
            game_dir_verdict(Path::new("D:/Games/Wuthering Waves")),
            GameDirVerdict::Removable
        );
        assert_eq!(
            game_dir_verdict(Path::new("D:/SteamLibrary/steamapps/common/Wuthering Waves")),
            GameDirVerdict::Steam
        );
        assert_eq!(game_dir_verdict(Path::new("C:/")), GameDirVerdict::Guarded);
        assert_eq!(game_dir_verdict(Path::new("Games/Relative")), GameDirVerdict::Guarded);
    }
}
