// ------------ XXMI Mod Loader ------------
// The modding toolchain behind the Mods tab: one shared core (the 3DMigoto loader) plus a
// package per game (GIMI, SRMI, ZZMI, WWMI, HIMI, EFMI). It installs and updates them,
// keeps their settings file in order, lets players change a few loader options, and starts the loader just before a
// game launches. A background updater keeps the packages current.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use peebify_helpers::mods::{describe_exit, Mode, EXE_NAME, EXIT_OK, EXIT_STOPPED, STOP_ALL_EVENT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use super::extract::{resolve_7z_binary, run_7z_extract};
use super::{
    arg_str, err_response, fs_util, game_profiles, http, ok_with, process_utils, resolve_profile,
    BackendState,
};

pub const LOADER_DLL: &str = "3dmloader.dll";
pub const CORE_KEY: &str = "xxmi";
pub const VERSION_FILE: &str = "VERSION.txt";
pub const OWN_SUBFOLDER: &str = "Peebify XXMI";

const CORE_LIBS: [&str; 2] = ["d3d11.dll", "d3dcompiler_47.dll"];
const CORE_FILES: [&str; 3] = [LOADER_DLL, "d3d11.dll", "d3dcompiler_47.dll"];

const PRESERVE_ON_UPDATE: [&str; 3] = ["Mods", "ShaderCache", "d3dx_user.ini"];
const XCMD_FILE: [&str; 2] = ["Core", "auto_update.xcmd"];
const XCMD_ROOTS: [&str; 2] = ["Core", "ShaderFixes"];
const ORPHAN_GRACE: Duration = Duration::from_secs(3);

const SHADERFIXES_EXTRAS: [&str; 4] = ["3dvision2sbs.ini", "help.ini", "mouse.ini", "upscale.ini"];
const ROGUE_MOD_FILES: [&str; 2] = ["d3dx.ini", "d3dx_user.ini"];
const ROGUE_SCAN_DEPTH: usize = 6;

const READY_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_GRACE: Duration = Duration::from_millis(1500);
const WATCH_SLACK: Duration = Duration::from_secs(240);

// ------------ Folders And Records ------------
// Where the toolchain lives on disk, which GitHub repo each package comes from, and the
// small record of installed versions.
const PACKAGES: [(&str, &str); 7] = [
    (CORE_KEY, "SpectrumQT/XXMI-Libs-Package"),
    ("gimi", "SilentNightSound/GIMI-Package"),
    ("srmi", "SpectrumQT/SRMI-Package"),
    ("zzmi", "leotorrez/ZZMI-Package"),
    ("wwmi", "SpectrumQT/WWMI-Package"),
    ("himi", "leotorrez/HIMI-Package"),
    ("efmi", "SpectrumQT/EFMI-Package"),
];

pub fn repo_for(package: &str) -> Option<&'static str> {
    PACKAGES
        .iter()
        .find(|(key, _)| *key == package)
        .map(|(_, repo)| *repo)
}

fn is_known_variant(variant: &str) -> bool {
    variant != CORE_KEY && repo_for(variant).is_some()
}

pub fn root(config: &super::config::LauncherConfig, user_data: &Path) -> PathBuf {
    if let Value::String(custom) = config.get("behavior.modsPath") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_root(user_data)
}

pub fn default_root(user_data: &Path) -> PathBuf {
    user_data.join("mods").join(CORE_KEY)
}

fn variant_dir(root: &Path, variant: &str) -> PathBuf {
    root.join(variant)
}

pub fn package_dir(root: &Path, package: &str) -> PathBuf {
    if package == CORE_KEY {
        root.to_path_buf()
    } else {
        variant_dir(root, package)
    }
}

pub fn mods_dir(root: &Path, variant: &str) -> PathBuf {
    variant_dir(root, variant).join("Mods")
}

pub fn installed_versions(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(root.join(VERSION_FILE)) else {
        return out;
    };
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            if !key.is_empty() && !value.is_empty() {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

fn write_versions(root: &Path, versions: &BTreeMap<String, String>) -> Result<(), String> {
    let body: String = versions.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    fs_util::write_atomic(&root.join(VERSION_FILE), body.as_bytes())
}

pub fn is_toolchain_folder(dir: &Path) -> bool {
    dir.join(VERSION_FILE).is_file()
}

pub fn record_version(root: &Path, package: &str, version: &str) -> Result<(), String> {
    let mut versions = installed_versions(root);
    versions.insert(package.to_string(), version.to_string());
    write_versions(root, &versions)
}

pub fn core_installed(root: &Path) -> bool {
    CORE_FILES.iter().all(|name| root.join(name).is_file())
}

pub fn variant_installed(root: &Path, variant: &str) -> bool {
    core_installed(root) && variant_dir(root, variant).join("d3dx.ini").exists()
}

pub fn boot(app: &AppHandle) {
    let state = app.state::<super::BackendState>();
    let root = root(&state.config, &state.user_data);
    tauri::async_runtime::spawn_blocking(move || super::mods::sweep_replaced_copies(&root));
    if stop_all_event().is_none() {
        log::warn!("xxmi: could not create the mod loader's shared stop event");
    }
    start_background(app);
}

// ------------ Install And Update ------------
// Makes sure the core and a game's package are present and current, downloading and
// unpacking them under a per-package lock so two installs never collide.
static INSTALLING: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static INSTALL_DONE: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub struct InstallGuard(String);

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if let Some(set) = INSTALLING.lock().as_mut() {
            set.remove(&self.0);
        }
        INSTALL_DONE.notify_waiters();
    }
}

pub fn try_lock_variant(variant: &str) -> Option<InstallGuard> {
    let mut guard = INSTALLING.lock();
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.insert(variant.to_string()) {
        return None;
    }
    Some(InstallGuard(variant.to_string()))
}

pub async fn lock_variant_when_free(variant: &str, patience: Duration) -> Option<InstallGuard> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let notified = INSTALL_DONE.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(guard) = try_lock_variant(variant) {
            return Some(guard);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let _ = tokio::time::timeout(remaining, notified).await;
    }
}

pub fn supported_variants() -> Vec<&'static str> {
    let mut variants: Vec<&'static str> = game_profiles::GAME_IDS
        .iter()
        .filter_map(|id| game_profiles::mod_variant(game_profiles::profile(id)))
        .collect();
    variants.sort_unstable();
    variants.dedup();
    variants
}

pub async fn finish_variant(root: &Path, variant: &str, force: bool) -> Result<(), String> {
    let (link_root, link_variant) = (root.to_path_buf(), variant.to_string());
    tauri::async_runtime::spawn_blocking(move || link_core_libs(&link_root, &link_variant, force))
        .await
        .map_err(|e| format!("Could not place the mod tools: {e}"))??;
    std::fs::create_dir_all(mods_dir(root, variant))
        .map_err(|e| format!("Could not create the Mods folder: {e}"))
}

pub async fn ensure_variant(
    app: &AppHandle,
    root: &Path,
    variant: &str,
    mut on_percent: impl FnMut(&str, f64),
) -> Result<(), String> {
    if !is_known_variant(variant) {
        return Err(format!("Peebify has no mod loader called \"{variant}\"."));
    }

    let installed = installed_versions(root);
    let mut changed = false;
    for package in [CORE_KEY, variant] {
        if installed.contains_key(package) && package_looks_installed(root, package) {
            continue;
        }
        let release = check_package(root, package).await?;
        install_package(app, root, package, &release, |p| on_percent(package, p)).await?;
        changed = true;
    }

    finish_variant(root, variant, changed).await
}

pub fn package_looks_installed(root: &Path, package: &str) -> bool {
    if package == CORE_KEY {
        core_installed(root)
    } else {
        variant_dir(root, package).join("d3dx.ini").exists()
    }
}

fn same_contents(source: &Path, target: &Path) -> bool {
    let (Ok(ms), Ok(mt)) = (std::fs::metadata(source), std::fs::metadata(target)) else {
        return false;
    };
    ms.len() == mt.len()
        && matches!((file_sha256(source), file_sha256(target)), (Ok(a), Ok(b)) if a == b)
}

fn link_core_libs(root: &Path, variant: &str, force: bool) -> Result<(), String> {
    let target_dir = variant_dir(root, variant);
    for lib in CORE_LIBS {
        let source = root.join(lib);
        if !source.exists() {
            return Err(format!(
                "The XXMI mod tools are missing {lib}. Install them again to fix this."
            ));
        }
        let target = target_dir.join(lib);
        if !force && same_contents(&source, &target) {
            continue;
        }
        std::fs::copy(&source, &target)
            .map_err(|e| format!("Could not place {lib} in {variant}: {e}"))?;
    }
    Ok(())
}

pub fn disable_shaderfixes_extras(variant_dir: &Path) {
    let dir = variant_dir.join("ShaderFixes");
    for name in SHADERFIXES_EXTRAS {
        let path = dir.join(name);
        if path.is_file() {
            let parked = dir.join(format!("DISABLED_{name}"));
            let _ = std::fs::remove_file(&parked);
            if std::fs::rename(&path, &parked).is_ok() {
                log::info!("xxmi: parked ShaderFixes/{name}");
            }
        }
    }
}

fn park_rogue_config_files(dir: &Path, depth: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut parked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if depth > 0 && !name.to_ascii_uppercase().starts_with("DISABLED") {
                parked += park_rogue_config_files(&path, depth - 1);
            }
            continue;
        }
        if ROGUE_MOD_FILES.iter().any(|r| r.eq_ignore_ascii_case(&name)) {
            let target = dir.join(format!("DISABLED {name}"));
            let _ = std::fs::remove_file(&target);
            if std::fs::rename(&path, &target).is_ok() {
                log::warn!("xxmi: parked a stray {name} inside Mods at {}", dir.display());
                parked += 1;
            }
        }
    }
    parked
}

pub fn unwrap_single_dir(dir: &Path) -> PathBuf {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return dir.to_path_buf();
    };
    let entries: Vec<_> = entries.flatten().collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
        return entries[0].path();
    }
    dir.to_path_buf()
}

pub fn merge_dir(source: &Path, destination: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(source).map_err(|e| format!("Could not read {source:?}: {e}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let target = destination.join(&name);

        if PRESERVE_ON_UPDATE
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&name_str))
            && target.exists()
        {
            log::debug!("xxmi: keeping existing {name_str}");
            continue;
        }

        if entry.path().is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("Could not create {target:?}: {e}"))?;
            merge_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("Could not copy {name_str}: {e}"))?;
        }
    }
    Ok(())
}

fn xcmd_delete_target(package_dir: &Path, value: &str) -> Result<PathBuf, String> {
    let parts: Vec<&str> = value
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    let Some(first) = parts.first() else {
        return Err("an empty path".to_string());
    };
    if parts.iter().any(|p| p.contains(':')) {
        return Err(format!("{value} is not a relative path"));
    }
    if !XCMD_ROOTS.iter().any(|r| r.eq_ignore_ascii_case(first)) {
        return Err(format!("{value} is outside Core and ShaderFixes"));
    }
    if parts.len() == 1 {
        return Err(format!("{value} would remove a whole folder"));
    }
    Ok(parts.iter().fold(package_dir.to_path_buf(), |path, part| path.join(part)))
}

fn xcmd_commands(text: &str, section: &str) -> Vec<(String, String)> {
    fs_util::ini_entries(text)
        .filter(|(s, _, _)| s.eq_ignore_ascii_case(section))
        .map(|(_, name, value)| (name.to_string(), value.to_string()))
        .collect()
}

pub fn xcmd_path(package_dir: &Path) -> PathBuf {
    XCMD_FILE.iter().fold(package_dir.to_path_buf(), |path, part| path.join(part))
}

pub fn run_update_commands(xcmd: &Path, section: &str, package_dir: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(xcmd) else {
        return 0;
    };
    let mut removed = 0;
    for (name, value) in xcmd_commands(&text, section) {
        if !name.eq_ignore_ascii_case("delete") {
            log::warn!("xxmi: skipped the unknown {section} command {name} = {value}");
            continue;
        }
        let target = match xcmd_delete_target(package_dir, &value) {
            Ok(target) => target,
            Err(e) => {
                log::warn!("xxmi: skipped {section} delete = {value}: {e}");
                continue;
            }
        };
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match result {
            Ok(()) => {
                log::info!("xxmi: {section} removed {}", target.display());
                removed += 1;
            }
            Err(e) => log::warn!("xxmi: {section} could not remove {}: {e}", target.display()),
        }
    }
    removed
}

pub async fn download_to(
    url: &str,
    destination: &Path,
    mut on_percent: impl FnMut(f64),
) -> Result<(), String> {
    let job = http::ToolJob::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let refusal = |received: u64, announced: u64| {
        if announced > MAX_PACKAGE_BYTES {
            Some(format!("{url} is {announced} bytes, more than any mod tools package should be"))
        } else if received > MAX_PACKAGE_BYTES {
            Some(format!(
                "{url} sent more than {MAX_PACKAGE_BYTES} bytes, more than any mod tools package should be"
            ))
        } else {
            None
        }
    };
    peebify_transfer::download_stream(
        &http::shared_transfer(),
        &job,
        url,
        destination,
        &refusal,
        &mut |written, announced| {
            if announced > 0 {
                on_percent((written as f64 / announced as f64) * 100.0);
            }
        },
    )
    .await
    .map(|_| ())
}

const MAX_PACKAGE_BYTES: u64 = 512 << 20;

#[derive(Default)]
pub struct IniOptions {
    pub target: String,
    pub hunting_mode: u64,
    pub reload_key: String,
    pub defaults: Vec<(String, String, String)>,
    pub screen: Option<(i32, i32)>,
}

pub(super) enum IniEntry {
    Set(String),
    Unset,
    EnsureLine(String),
}

pub(super) fn is_reserved(section: &str, key: &str) -> bool {
    const RESERVED: [(&str, &str); 10] = [
        ("Loader", "target"),
        ("Loader", "loader"),
        ("Loader", "launch"),
        ("Include", "include_recursive"),
        ("Include", "exclude_recursive"),
        ("Hunting", "hunting"),
        ("Hunting", "reload_fixes"),
        ("Hunting", "reload_config"),
        ("System", "check_foreground_window"),
        ("System", "additional_foreground_window"),
    ];
    RESERVED
        .iter()
        .any(|(s, k)| s.eq_ignore_ascii_case(section) && k.eq_ignore_ascii_case(key))
}

pub const HUNTING_KEYBINDS_ONLY: u64 = 2;

pub const DEFAULT_RELOAD_KEY: &str = "no_modifiers VK_F10";

fn write_ini_entries(path: &Path, entries: &[(String, String, IniEntry)]) -> Result<bool, String> {
    let original =
        std::fs::read_to_string(path).map_err(|e| format!("Could not read d3dx.ini: {e}"))?;
    let updated = rewrite_ini(&original, entries);
    if updated == original {
        return Ok(false);
    }
    fs_util::write_atomic(path, updated.as_bytes())?;
    Ok(true)
}

fn override_entries(overrides: &[(String, String, String)]) -> Vec<(String, String, IniEntry)> {
    overrides
        .iter()
        .filter(|(section, key, _)| !is_reserved(section, key))
        .map(|(section, key, value)| (section.clone(), key.clone(), IniEntry::Set(value.clone())))
        .collect()
}

fn apply_managed_ini(
    variant_dir: &Path,
    options: &IniOptions,
    overrides: &[(String, String, String)],
) -> Result<(), String> {
    let mut entries = override_entries(&options.defaults);
    if let Some((width, height)) = options.screen {
        entries.push(("System".into(), "screen_width".into(), IniEntry::Set(width.to_string())));
        entries.push(("System".into(), "screen_height".into(), IniEntry::Set(height.to_string())));
    }
    entries.append(&mut override_entries(overrides));

    let mut managed: Vec<(String, String, IniEntry)> = vec![
        (
            "Loader".into(),
            "loader".into(),
            IniEntry::Set(EXE_NAME.to_string()),
        ),
        ("Loader".into(), "launch".into(), IniEntry::Unset),
        (
            "Include".into(),
            "include_recursive".into(),
            IniEntry::Set("Mods".into()),
        ),
        (
            "Include".into(),
            "exclude_recursive".into(),
            IniEntry::EnsureLine("DISABLED*".into()),
        ),
        (
            "Hunting".into(),
            "hunting".into(),
            IniEntry::Set(options.hunting_mode.to_string()),
        ),
        (
            "Hunting".into(),
            "reload_fixes".into(),
            IniEntry::Set(options.reload_key.clone()),
        ),
        (
            "Hunting".into(),
            "reload_config".into(),
            IniEntry::Set(options.reload_key.clone()),
        ),
        (
            "System".into(),
            "check_foreground_window".into(),
            IniEntry::Set("1".into()),
        ),
        (
            "System".into(),
            "additional_foreground_window".into(),
            IniEntry::Set("Peebify Overlay".into()),
        ),
    ];
    if !options.target.is_empty() {
        managed.insert(
            0,
            (
                "Loader".into(),
                "target".into(),
                IniEntry::Set(options.target.clone()),
            ),
        );
    }

    entries.append(&mut managed);
    write_ini_entries(&variant_dir.join("d3dx.ini"), &entries).map(|_| ())
}

pub(super) fn stored_overrides(
    config: &super::config::LauncherConfig,
    game_id: &str,
) -> Vec<(String, String, String)> {
    let Value::Object(map) = config.get(&format!("games.{game_id}.modIni")) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(compound, value)| {
            let sanitized = sanitize_stored(compound, value);
            if sanitized.is_none() {
                log::warn!("Ignoring the saved mod loader setting \"{compound}\" for {game_id}: it is not a setting Peebify manages or its value is not valid.");
            }
            let (section, key, text) = sanitized?;
            Some((section.to_string(), key.to_string(), text))
        })
        .collect()
}

pub(super) fn apply_ini_overrides(
    variant_dir: &Path,
    overrides: &[(String, String, String)],
) -> Result<bool, String> {
    let entries = override_entries(overrides);
    if entries.is_empty() {
        return Ok(false);
    }
    write_ini_entries(&variant_dir.join("d3dx.ini"), &entries)
}

fn rewrite_ini(original: &str, managed: &[(String, String, IniEntry)]) -> String {
    let uses_crlf = original.contains("\r\n");
    let newline = if uses_crlf { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();

    for (section, key, entry) in managed {
        let value_safe = match entry {
            IniEntry::Set(value) | IniEntry::EnsureLine(value) => {
                ini_text_is_safe(value)
            }
            IniEntry::Unset => true,
        };
        if !value_safe
            || !ini_text_is_safe(section)
            || !ini_text_is_safe(key)
        {
            log::warn!("Skipped a d3dx.ini entry in [{}] because it contains a line break or a bracket.", section.escape_debug());
            continue;
        }
        let mut section_start: Option<usize> = None;
        let mut section_end = lines.len();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
                continue;
            }
            let name = trimmed[1..trimmed.len() - 1].trim();
            if section_start.is_some() {
                section_end = i;
                break;
            }
            if name.eq_ignore_ascii_case(section) {
                section_start = Some(i);
            }
        }

        let Some(start) = section_start else {
            let value = match entry {
                IniEntry::Set(value) | IniEntry::EnsureLine(value) => value,
                IniEntry::Unset => continue,
            };
            if !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.push(format!("{key} = {value}"));
            continue;
        };

        let existing = (start + 1..section_end).find(|&i| {
            fs_util::ini_key_value(&lines[i])
                .map(|(k, _)| k.eq_ignore_ascii_case(key))
                .unwrap_or(false)
        });

        let insert_line = |lines: &mut Vec<String>, text: String| {
            let mut insert_at = section_end;
            while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            lines.insert(insert_at, text);
        };

        match (existing, entry) {
            (Some(i), IniEntry::Set(value)) => lines[i] = format!("{key} = {value}"),
            (Some(i), IniEntry::Unset) => lines[i] = format!(";{}", lines[i]),
            (None, IniEntry::Set(value)) => insert_line(&mut lines, format!("{key} = {value}")),
            (None, IniEntry::Unset) => {}
            (_, IniEntry::EnsureLine(value)) => {
                let present = (start + 1..section_end).any(|i| {
                    fs_util::ini_key_value(&lines[i])
                        .map(|(k, v)| k.eq_ignore_ascii_case(key) && v.eq_ignore_ascii_case(value))
                        .unwrap_or(false)
                });
                if !present {
                    insert_line(&mut lines, format!("{key} = {value}"));
                }
            }
        }
    }

    let mut out = lines.join(newline);
    if original.ends_with('\n') && !out.ends_with('\n') {
        out.push_str(newline);
    }
    out
}

fn screen_size() -> Option<(i32, i32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    (width > 0 && height > 0).then_some((width, height))
}

// ------------ Running The Loader ------------
// Starts the loader armed for a game, watches how it exits, and cleans up a stray one
// left over from a crash. Only one loader runs at a time.
struct ActiveLoader {
    game_id: String,
    stop: process_utils::NamedEvent,
    process: std::sync::Arc<process_utils::ProcessHandle>,
}

static ACTIVE_LOADER: Mutex<Option<ActiveLoader>> = Mutex::new(None);
static LOADER_SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static STOP_ALL: std::sync::OnceLock<Option<process_utils::NamedEvent>> = std::sync::OnceLock::new();

fn stop_all_event() -> Option<&'static process_utils::NamedEvent> {
    STOP_ALL
        .get_or_init(|| process_utils::NamedEvent::create(STOP_ALL_EVENT))
        .as_ref()
}

fn loader_log_path(app: &AppHandle) -> PathBuf {
    app.state::<super::BackendState>()
        .logs_dir
        .join("mod-loader.log")
}

fn loader_log_tail(log_path: &Path, limit: usize) -> String {
    let Ok(text) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().rev().take(limit).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

const HOOK_ASSUMED: &str = "assumed to have landed";

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn loader_failure_message(display: &str, code: i32, game_running: bool) -> String {
    let reason = capitalized(&describe_exit(code));
    if code == peebify_helpers::mods::EXIT_TARGET_EXITED && game_running {
        format!("{display} restarted itself before mods loaded. Close it and launch it again from Peebify.")
    } else if game_running {
        format!("{display} is running without mods. {reason}")
    } else {
        format!("Mods were not loaded for {display}. {reason}")
    }
}

fn log_loader_finish(game_id: &str, outcome: &str, log_path: &Path) {
    let detail = loader_log_tail(log_path, usize::MAX);
    if detail.contains(HOOK_ASSUMED) {
        log::warn!("xxmi: the mod loader {outcome} for {game_id} without confirming the hook: {detail}");
    } else {
        log::info!("xxmi: the mod loader {outcome} for {game_id}: {detail}");
    }
}

fn announce_tools_busy(app: &AppHandle, game_id: &str, announced: &mut bool) {
    if std::mem::replace(announced, true) {
        return;
    }
    log::info!("xxmi: {game_id} is waiting for the mod tools to finish updating");
    let _ = app.emit("mods-tools-updating", json!({ "gameId": game_id }));
}

pub fn loader_active() -> bool {
    ACTIVE_LOADER
        .lock()
        .as_ref()
        .is_some_and(|a| a.process.exit_code().is_none())
}

fn checksum_arg(integrity: &BTreeMap<String, String>, option: &str, name: &str) -> Vec<String> {
    match integrity
        .get(name)
        .filter(|digest| peebify_helpers::mods::parse_sha256(digest).is_some())
    {
        Some(digest) => vec![option.to_string(), digest.clone()],
        None => {
            log::warn!("xxmi: no checksum is on hand for {name}, so the mod loader will not check it");
            Vec::new()
        }
    }
}

pub async fn prepare_and_spawn(
    app: &AppHandle,
    root: &Path,
    profile: &Value,
    confirm_timeout: Duration,
) -> Result<(), String> {
    let variant = game_profiles::mod_variant(profile)
        .ok_or("This game does not support mods.")?
        .to_string();
    let profile_id = game_profiles::profile_id(profile).to_string();
    let target = game_profiles::client_process_name(profile).to_string();

    let integrity = {
        let mut announced = false;
        let guard = match try_lock_variant(&variant) {
            Some(guard) => Some(guard),
            None => {
                announce_tools_busy(app, &profile_id, &mut announced);
                lock_variant_when_free(&variant, Duration::from_secs(300)).await
            }
        };
        let Some(_guard) = guard else {
            return Err(format!(
                "The mod tools for {variant} are still installing. Try again in a moment."
            ));
        };
        let _update = match UPDATE_LOCK.try_lock() {
            Ok(update) => update,
            Err(_) => {
                announce_tools_busy(app, &profile_id, &mut announced);
                UPDATE_LOCK.lock().await
            }
        };
        ensure_variant(app, root, &variant, |package, percent| {
            log::debug!("xxmi: {package} {percent:.0}%");
        })
        .await?;
        let verify_root = root.to_path_buf();
        tauri::async_runtime::spawn_blocking(move || verify_integrity(&verify_root))
            .await
            .map_err(|e| format!("Could not check the mod tools: {e}"))??
    };

    let state = app.try_state::<super::BackendState>();
    let overrides = state
        .as_ref()
        .map(|state| stored_overrides(&state.config, &profile_id))
        .unwrap_or_default();
    let reload_key = overrides
        .iter()
        .find(|(s, k, _)| s.eq_ignore_ascii_case("Hunting") && k.eq_ignore_ascii_case("reload_fixes"))
        .map(|(_, _, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_RELOAD_KEY.to_string());

    let dir = variant_dir(root, &variant);
    apply_managed_ini(
        &dir,
        &IniOptions {
            target: target.clone(),
            hunting_mode: HUNTING_KEYBINDS_ONLY,
            reload_key,
            defaults: game_profiles::mod_ini_defaults(profile),
            screen: screen_size(),
        },
        &overrides,
    )?;

    park_rogue_config_files(&mods_dir(root, &variant), ROGUE_SCAN_DEPTH);

    let host = fs_util::resource(app, EXE_NAME).ok_or_else(|| {
        "The mod loader is missing from this Peebify install. Reinstall Peebify to restore it."
            .to_string()
    })?;
    let mode = Mode::parse(game_profiles::mod_loader_mode(profile)).unwrap_or(Mode::Hook);

    displace_active_loader(app, &profile_id).await;

    let serial = LOADER_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let event = format!("Local\\PeebifyModLoader-{}-{serial}", std::process::id());
    let ready = process_utils::NamedEvent::create(&format!(
        "{event}{}",
        peebify_helpers::mods::READY_EVENT_SUFFIX
    ))
    .ok_or("Could not create the mod loader's handshake event.")?;
    let stop = process_utils::NamedEvent::create(&format!(
        "{event}{}",
        peebify_helpers::mods::STOP_EVENT_SUFFIX
    ))
    .ok_or("Could not create the mod loader's stop event.")?;

    let log_path = loader_log_path(app);
    let elevated = |path: &Path| {
        process_utils::resolve_subst(path)
            .to_string_lossy()
            .to_string()
    };
    let mut args = vec!["--dll".to_string(), elevated(&root.join(LOADER_DLL))];
    args.extend(checksum_arg(&integrity, "--dll-sha256", LOADER_DLL));
    args.extend(["--module".to_string(), elevated(&dir.join("d3d11.dll"))]);
    args.extend(checksum_arg(&integrity, "--module-sha256", "d3d11.dll"));
    args.extend([
        "--target".to_string(),
        target.clone(),
        "--mode".to_string(),
        mode.as_str().to_string(),
        "--timeout".to_string(),
        confirm_timeout.as_secs().to_string(),
        "--event".to_string(),
        event.clone(),
        "--log".to_string(),
        elevated(&log_path),
    ]);
    let versions = installed_versions(root);
    let version_of = |key: &str| versions.get(key).map(String::as_str).unwrap_or("unknown");
    log::info!(
        "Starting mod loader for {variant} (mode={}, target={target}, timeout={}s, {CORE_KEY}={}, {variant}={}, root={})",
        mode.as_str(),
        confirm_timeout.as_secs(),
        version_of(CORE_KEY),
        version_of(&variant),
        root.display()
    );

    if let Some(stop_all) = stop_all_event() {
        stop_all.reset();
    }
    let process = {
        let (host, args, root) = (host.clone(), args.clone(), root.to_path_buf());
        tauri::async_runtime::spawn_blocking(move || {
            process_utils::spawn_tool_elevated_handle(&host, &args, &root)
        })
        .await
        .map_err(|e| format!("The mod loader could not be started: {e}"))?
    }
    .map_err(|e| {
        if e.declined {
            "You declined the Windows prompt, so the game is starting without mods.".to_string()
        } else {
            format!("The mod loader could not be started: {}", e.message)
        }
    })?;
    let process = std::sync::Arc::new(process);

    let outcome = {
        let ready = ready.clone();
        let process = process.clone();
        tauri::async_runtime::spawn_blocking(move || {
            process_utils::wait_ready_or_exit(&ready, &process, READY_TIMEOUT)
        })
        .await
        .map_err(|e| format!("The mod loader wait failed: {e}"))?
    };

    match outcome {
        process_utils::ReadyOutcome::Ready => {}
        process_utils::ReadyOutcome::Exited(code) => {
            let reason = describe_exit(code as i32);
            log::error!(
                "xxmi: the mod loader exited early for {profile_id}: {reason} {}",
                loader_log_tail(&log_path, 4)
            );
            return Err(format!("The mod loader stopped before the game started: {reason}"));
        }
        process_utils::ReadyOutcome::TimedOut => {
            stop.set();
            return Err(
                "The mod loader started but never reported ready. Security software may be \
                 scanning it. Try launching again."
                    .to_string(),
            );
        }
    }

    *ACTIVE_LOADER.lock() = Some(ActiveLoader {
        game_id: profile_id.clone(),
        stop,
        process: process.clone(),
    });
    watch_loader(app.clone(), profile_id, log_path, process, confirm_timeout);
    log::info!("{EXE_NAME} is armed for {target} — starting the game");
    Ok(())
}

fn watch_loader(
    app: AppHandle,
    game_id: String,
    log_path: PathBuf,
    process: std::sync::Arc<process_utils::ProcessHandle>,
    confirm_timeout: Duration,
) {
    tauri::async_runtime::spawn(async move {
        let waited = process.clone();
        let code = tauri::async_runtime::spawn_blocking(move || {
            waited.wait(confirm_timeout + WATCH_SLACK)
        })
        .await
        .ok()
        .flatten();

        {
            let mut active = ACTIVE_LOADER.lock();
            if active
                .as_ref()
                .is_some_and(|a| std::sync::Arc::ptr_eq(&a.process, &process))
            {
                *active = None;
            }
        }

        match code.map(|c| c as i32) {
            Some(EXIT_OK) => log_loader_finish(&game_id, "finished", &log_path),
            Some(EXIT_STOPPED) => log_loader_finish(&game_id, "was stopped", &log_path),
            Some(code) => {
                let error = capitalized(&describe_exit(code));
                let detail = loader_log_tail(&log_path, 4);
                log::error!("xxmi: mod loader failed for {game_id}: {error} {detail}");
                let display = game_profiles::display_name(game_profiles::profile(&game_id));
                let game_running = app
                    .try_state::<super::BackendState>()
                    .is_some_and(|state| state.game.is_game_running_id(&game_id));
                let message = loader_failure_message(display, code, game_running);
                super::platform::notify_if_backgrounded(&app, "Mods didn't load", &message);
                let _ = app.emit(
                    "mods-load-failed",
                    json!({ "gameId": game_id, "error": error, "message": message }),
                );
            }
            None => log::warn!("xxmi: the mod loader for {game_id} is still running after its window"),
        }
    });
}

async fn stop_active_loader() {
    let active = ACTIVE_LOADER.lock().take();
    let Some(active) = active else {
        return;
    };
    if active.process.exit_code().is_some() {
        return;
    }
    active.stop.set();
    let process = active.process.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || process.wait(STOP_GRACE)).await;
}

async fn displace_active_loader(app: &AppHandle, game_id: &str) {
    let displaced = ACTIVE_LOADER
        .lock()
        .as_ref()
        .filter(|a| a.game_id != game_id && a.process.exit_code().is_none())
        .map(|a| a.game_id.clone());
    stop_active_loader().await;
    let Some(other) = displaced else {
        return;
    };
    let display = game_profiles::display_name(game_profiles::profile(&other));
    let error = "Another game with mods was launched before it finished starting.".to_string();
    log::warn!("xxmi: stopped the mod loader for {other} because {game_id} launched with mods");
    let message = format!("{display} will run without mods. {error}");
    super::platform::notify_if_backgrounded(app, "Mods didn't load", &message);
    let _ = app.emit(
        "mods-load-failed",
        json!({ "gameId": other, "error": error, "message": message }),
    );
}

pub async fn kill_orphan_loader() {
    stop_active_loader().await;
    if !process_utils::is_process_running(EXE_NAME).await {
        return;
    }
    let Some(stop_all) = stop_all_event() else {
        log::warn!("An orphaned {EXE_NAME} is still running. It exits on its own once its wait runs out.");
        return;
    };
    stop_all.set();
    let deadline = tokio::time::Instant::now() + ORPHAN_GRACE;
    let mut running = true;
    while running && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        running = process_utils::is_process_running(EXE_NAME).await;
    }
    stop_all.reset();
    if running {
        log::warn!("An orphaned {EXE_NAME} did not stop when asked. It exits on its own once its wait runs out.");
    } else {
        log::info!("Stopped an orphaned {EXE_NAME}");
    }
}

// ------------ Mod Loader Settings ------------
// The few mod loader (d3dx.ini) options players can change from the UI, like the reload shortcut and the start
// delay. Values are checked here, saved in the config and written into the loader's ini file.

pub enum Kind {
    Bool,
    Millis { min: u32, max: u32 },
    Key,
}

pub struct IniSetting {
    pub id: &'static str,
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub kind: Kind,
    pub shipped: &'static str,
    pub mirrors: &'static [(&'static str, &'static str)],
}

pub const SETTINGS: &[IniSetting] = &[
    IniSetting {
        id: "reloadMods",
        section: "Hunting",
        key: "reload_fixes",
        label: "Reload mods in game",
        description:
            "The mod loader's own shortcut. It re-reads every enabled mod without restarting the game.",
        kind: Kind::Key,
        shipped: "no_modifiers VK_F10",
        mirrors: &[("Hunting", "reload_config")],
    },
    IniSetting {
        id: "showWarnings",
        section: "Logging",
        key: "show_warnings",
        label: "Show mod warnings in game",
        description:
            "Red text the mod loader prints on screen when a mod's ini has a problem. Off by default, because nearly all of it comes from the mods themselves.",
        kind: Kind::Bool,
        shipped: "0",
        mirrors: &[],
    },
    IniSetting {
        id: "injectionDelay",
        section: "System",
        key: "dll_initialization_delay",
        label: "Mod library start delay",
        description:
            "How long the mod library waits inside the game before it starts, in milliseconds. Raise it if the game crashes on launch with mods on.",
        kind: Kind::Millis { min: 0, max: 10000 },
        shipped: "0",
        mirrors: &[],
    },
];

fn setting(id: &str) -> Option<&'static IniSetting> {
    SETTINGS.iter().find(|s| s.id == id)
}

pub fn validate(spec: &IniSetting, raw: &str) -> Result<String, &'static str> {
    match spec.kind {
        Kind::Bool => match raw.trim() {
            "true" | "1" => Ok("1".to_string()),
            "false" | "0" => Ok("0".to_string()),
            _ => Err("That setting is on or off."),
        },
        Kind::Millis { min, max } => raw
            .trim()
            .parse::<u32>()
            .map(|n| n.clamp(min, max).to_string())
            .map_err(|_| "That setting is a whole number of milliseconds."),
        Kind::Key => accelerator_to_ini(raw).ok_or(
            "The mod loader only takes Ctrl, Alt and Shift with a function key or a single letter.",
        ),
    }
}

pub fn ini_text_is_safe(text: &str) -> bool {
    !text.contains(['\r', '\n', '[', ']'])
}

pub fn sanitize_stored(
    compound: &str,
    value: &Value,
) -> Option<(&'static str, &'static str, String)> {
    let (section, key) = compound.split_once('/')?;
    let (section, key) = (section.trim(), key.trim());
    let (spec, target) = SETTINGS.iter().find_map(|spec| {
        std::iter::once((spec.section, spec.key))
            .chain(spec.mirrors.iter().copied())
            .find(|(s, k)| s.eq_ignore_ascii_case(section) && k.eq_ignore_ascii_case(key))
            .map(|target| (spec, target))
    })?;
    let raw = match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => u8::from(*b).to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if !ini_text_is_safe(&raw) {
        return None;
    }
    let normalized = match spec.kind {
        Kind::Key => accelerator_to_ini(&ini_to_accelerator(&raw))?,
        _ => validate(spec, &raw).ok()?,
    };
    Some((target.0, target.1, normalized))
}

const KEY_MODIFIERS: [(&str, &str); 3] = [("ctrl", "Ctrl"), ("alt", "Alt"), ("shift", "Shift")];

pub fn accelerator_to_ini(text: &str) -> Option<String> {
    let parts: Vec<&str> = text
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (key, mods) = parts.split_last()?;
    let mut out: Vec<String> = Vec::new();
    for m in mods {
        let lower = m.to_lowercase();
        if !KEY_MODIFIERS.iter().any(|(ini, _)| *ini == lower) {
            return None;
        }
        out.push(lower);
    }
    if out.is_empty() {
        out.push("no_modifiers".to_string());
    }
    let named = key.to_uppercase();
    let key = if named.starts_with('F')
        && named[1..]
            .parse::<u8>()
            .is_ok_and(|n| (1..=24).contains(&n))
    {
        format!("VK_{named}")
    } else if named.len() == 1 && named.chars().all(|c| c.is_ascii_alphanumeric()) {
        named
    } else {
        return None;
    };
    out.push(key);
    Some(out.join(" "))
}

pub fn ini_to_accelerator(text: &str) -> String {
    let mut mods: Vec<&str> = Vec::new();
    let mut key = String::new();
    for token in text.split_whitespace() {
        let lower = token.to_lowercase();
        match KEY_MODIFIERS.iter().find(|(ini, _)| *ini == lower) {
            Some((_, label)) => mods.push(label),
            None if lower == "no_modifiers" => {}
            None => {
                key = token
                    .trim_start_matches("VK_")
                    .trim_start_matches("vk_")
                    .to_string()
            }
        }
    }
    mods.push(&key);
    mods.join("+")
}

fn variant_for(app: &AppHandle, game_id: &str) -> Option<(String, std::path::PathBuf)> {
    let state = app.state::<BackendState>();
    let profile = game_profiles::profile(game_id);
    let variant = game_profiles::mod_variant(profile)?.to_string();
    let root = root(&state.config, &state.user_data);
    Some((variant.clone(), root.join(&variant)))
}

pub(super) async fn mod_ini_settings(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();
    let state = app.state::<BackendState>();

    let Some((variant, dir)) = variant_for(app, &game_id) else {
        return Ok(ok_with(json!({
            "gameId": game_id,
            "supported": false,
            "toolchainInstalled": false,
            "settings": Vec::<Value>::new(),
        })));
    };

    let installed = variant_installed(&root(&state.config, &state.user_data), &variant);
    let text = std::fs::read_to_string(dir.join("d3dx.ini")).unwrap_or_default();
    let defaults = game_profiles::mod_ini_defaults(profile);

    let rows: Vec<Value> = SETTINGS
        .iter()
        .map(|s| {
            let shipped = defaults
                .iter()
                .find(|(section, key, _)| {
                    section.eq_ignore_ascii_case(s.section) && key.eq_ignore_ascii_case(s.key)
                })
                .map(|(_, _, value)| value.as_str())
                .unwrap_or(s.shipped);
            let compound = format!("{}/{}", s.section, s.key);
            let stored = state
                .config
                .get(&format!("games.{game_id}.modIni.{compound}"));
            let stored = sanitize_stored(&compound, &stored).map(|(_, _, v)| v);
            let value = stored
                .or_else(|| fs_util::ini_value(&text, s.section, s.key))
                .unwrap_or_else(|| shipped.to_string());
            json!({
                "id": s.id,
                "label": s.label,
                "description": s.description,
                "kind": match s.kind {
                    Kind::Bool => "bool",
                    Kind::Millis { .. } => "millis",
                    Kind::Key => "key",
                },
                "accelerator": matches!(s.kind, Kind::Key).then(|| ini_to_accelerator(&value)),
                "defaultAccelerator": matches!(s.kind, Kind::Key)
                    .then(|| ini_to_accelerator(shipped)),
                "min": match s.kind { Kind::Millis { min, .. } => Some(min), _ => None },
                "max": match s.kind { Kind::Millis { max, .. } => Some(max), _ => None },
                "value": value,
            })
        })
        .collect();

    Ok(ok_with(json!({
        "gameId": game_id,
        "supported": true,
        "toolchainInstalled": installed,
        "settings": rows,
    })))
}

pub(super) async fn set_mod_ini_setting(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();

    let Some(id) = arg_str(args, 1) else {
        return Ok(err_response("No setting was given."));
    };
    let Some(spec) = setting(id) else {
        return Ok(err_response("That is not a mod loader setting."));
    };
    let Some(raw) = arg_str(args, 2) else {
        return Ok(err_response("No value was given."));
    };

    let value = match validate(spec, raw) {
        Ok(value) => value,
        Err(message) => return Ok(err_response(message)),
    };

    let config = &app.state::<BackendState>().config;
    for (section, key) in
        std::iter::once((spec.section, spec.key)).chain(spec.mirrors.iter().copied())
    {
        config.set(
            &format!("games.{game_id}.modIni.{section}/{key}"),
            Value::String(value.clone()),
        );
    }

    let applied = match variant_for(app, &game_id) {
        Some((variant, dir)) => {
            let state = app.state::<BackendState>();
            let root = root(&state.config, &state.user_data);
            if variant_installed(&root, &variant) {
                let overrides = stored_overrides(&state.config, &game_id);
                apply_ini_overrides(&dir, &overrides).unwrap_or(false)
            } else {
                false
            }
        }
        None => false,
    };

    let _ = app.emit(
        "mod-ini-changed",
        json!({ "gameId": game_id, "settingId": spec.id, "value": value, "applied": applied }),
    );

    Ok(ok_with(json!({
        "settingId": spec.id,
        "value": value,
        "applied": applied,
    })))
}

// ------------ Mod Tools Updater ------------
// Installs the XXMI mod loader packages for every supported game once mod support is on, and keeps them up to
// date. It starts as soon as the launcher opens or mod support is switched on, then checks their GitHub
// releases every five minutes, verifies each download's signature, and holds an install back while a modded
// game is running, applying it once the game closes.

pub const STATE_FILE: &str = "update-state.json";
pub const STAGING_DIR: &str = ".staging";
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const WATCH_TICK: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
const OFFLINE: &str = "offline";
const RATE_LIMIT_FALLBACK_SECS: i64 = 30 * 60;

const SPECTRUM_KEY: &str = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEYac352uRGKZh6LOwK0fVDW/TpyECEfnRtUp+bP2PJPP63SWOkJ3a/d9pAnPfYezRVJ1hWjZtpRTT8HEAN/b4mWpJvqO43SAEV/1Q6vz9Rk/VvRV3jZ6B/tmqVnIeHKEb";
const GIMI_KEY: &str = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAET5SWORxEdlJ3RXWIFiuwMX6oyZedz+DgaxtsbpWyxNQJDgIDj4uKLLJlvhRNpnkFEuQntgJKzJs0SpASBEguPOTE7VSnmp+x5uyDmsQsWzsRSAZip++a02jqR/K2j18H";
const ZZMI_KEY: &str = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEb11GjbKQS6SmRe8TcIc5VMu5Ob3moo5v2YeD+s53xEe4bVPGcToUNLu3Jgqo0OwWZ4RsNy1nR0HId6pR09HedyEMifxebsyPT3T5PH82QozEXHQlTDySklWUfGItoOdf";
const HIMI_KEY: &str = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEeigvK7REsX3f/vb+RRuFkZt/6VRbykI2oQcEU3IiI3N9s6jWqKkxAE2cTC9wKXDlkeSzlHjPxgzrTrKdqwkFzROMjw5T2LixFB5BYaT633aU/cCiHDbArIJ46+GrqemG";

const PUBLIC_KEYS: [(&str, &str); 7] = [
    ("xxmi", SPECTRUM_KEY),
    ("gimi", GIMI_KEY),
    ("srmi", SPECTRUM_KEY),
    ("zzmi", ZZMI_KEY),
    ("wwmi", SPECTRUM_KEY),
    ("himi", HIMI_KEY),
    ("efmi", SPECTRUM_KEY),
];

pub static UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING: AtomicBool = AtomicBool::new(false);
static WAITING_LOGGED: AtomicBool = AtomicBool::new(false);
static INSTALLING_NEW: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct PackageState {
    pub etag: Option<String>,
    pub latest_tag: Option<String>,
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub signature: Option<String>,
    pub retry_after: Option<i64>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateState {
    pub packages: BTreeMap<String, PackageState>,
    pub integrity: BTreeMap<String, String>,
}

static STATE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn load_state(root: &Path) -> Result<UpdateState, String> {
    let Some(stored) = super::fs_util::read_json_store(&root.join(STATE_FILE))? else {
        return Ok(UpdateState::default());
    };
    Ok(serde_json::from_value(stored).unwrap_or_else(|e| {
        log::warn!("xxmi: {STATE_FILE} could not be parsed ({e}), so it is read as empty");
        UpdateState::default()
    }))
}

fn current_state(root: &Path) -> Result<UpdateState, String> {
    let _guard = STATE_LOCK.lock();
    load_state(root)
}

fn write_state(root: &Path, state: &UpdateState) -> Result<(), String> {
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    fs_util::write_atomic(&root.join(STATE_FILE), text.as_bytes())
}

fn update_state(root: &Path, change: impl FnOnce(&mut UpdateState)) -> Result<(), String> {
    let _guard = STATE_LOCK.lock();
    let mut state = load_state(root)?;
    change(&mut state);
    write_state(root, &state)
}

fn update_package(root: &Path, package: &str, change: impl FnOnce(&mut PackageState)) {
    if let Err(e) = update_state(root, |state| {
        change(state.packages.entry(package.to_string()).or_default())
    }) {
        log::warn!("xxmi: could not record the {package} release check: {e}");
    }
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub signature: Option<String>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn cached_release(state: &PackageState) -> Option<Release> {
    Some(Release {
        tag: state.latest_tag.clone()?,
        asset_name: state.asset_name.clone()?,
        download_url: state.asset_url.clone()?,
        signature: state.signature.clone(),
    })
}

pub fn parse_signature(body: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?m)^## Signature[\r\n]+- ((?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{4}|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{2}==))\s*$",
        )
        .expect("signature pattern")
    });
    pattern
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

pub fn parse_release(package: &str, text: &str) -> Result<Release, String> {
    let json: Value = serde_json::from_str(text)
        .map_err(|e| format!("Invalid release JSON for {package}: {e}"))?;

    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("No tag_name in the latest {package} release"))?
        .to_string();

    let assets = json
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("No assets in the latest {package} release"))?;

    let asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(Value::as_str)
                .map(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower.ends_with(".zip") && lower.contains("package")
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("The latest {package} release has no package zip"))?;

    let asset_name = asset
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let download_url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("The {package} package asset has no download URL"))?
        .to_string();
    let signature = json
        .get("body")
        .and_then(Value::as_str)
        .and_then(parse_signature);

    Ok(Release {
        tag,
        asset_name,
        download_url,
        signature,
    })
}

fn rate_limit_reset(headers: &reqwest::header::HeaderMap) -> i64 {
    if let Some(secs) = http::retry_after_secs(headers) {
        return now() + secs as i64;
    }
    headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|reset| *reset > now())
        .unwrap_or_else(|| now() + RATE_LIMIT_FALLBACK_SECS)
}

pub async fn check_package(root: &Path, package: &str) -> Result<Release, String> {
    let repo = repo_for(package).ok_or_else(|| format!("Unknown mod package \"{package}\""))?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");

    let entry = current_state(root)?
        .packages
        .remove(package)
        .unwrap_or_default();

    if let Some(until) = entry.retry_after.filter(|until| *until > now()) {
        if let Some(cached) = cached_release(&entry) {
            log::info!(
                "xxmi: GitHub is rate limited for {} more s, using the cached {package} release",
                until - now()
            );
            return Ok(cached);
        }
        return Err(format!(
            "GitHub is rate limiting release checks for another {} minutes.",
            ((until - now()) / 60).max(1)
        ));
    }

    let mut request = http::client()
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(etag) = entry.etag.as_deref().filter(|e| !e.is_empty()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("Request error: {e}"))?;
    let status = response.status().as_u16();

    match status {
        304 => {
            update_package(root, package, |stored| stored.retry_after = None);
            cached_release(&entry).ok_or_else(|| format!("GitHub said the {package} release was unchanged, but nothing was cached."))
        }
        200 => {
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let text = http::read_text_capped(response, &url, http::MAX_FEED_RESPONSE).await?;
            let release = parse_release(package, &text)?;
            update_package(root, package, |stored| {
                stored.etag = etag;
                stored.latest_tag = Some(release.tag.clone());
                stored.asset_name = Some(release.asset_name.clone());
                stored.asset_url = Some(release.download_url.clone());
                stored.signature = release.signature.clone();
                stored.retry_after = None;
            });
            log::info!(
                "xxmi: {package} {} resolves to asset \"{}\"",
                release.tag,
                release.asset_name
            );
            Ok(release)
        }
        403 | 429 => {
            let until = rate_limit_reset(response.headers());
            update_package(root, package, |stored| stored.retry_after = Some(until));
            let cached = cached_release(&entry);
            log::warn!(
                "xxmi: GitHub rate limited the {package} check (HTTP {status}), retrying after {} s",
                until - now()
            );
            cached.ok_or_else(|| {
                "GitHub is rate limiting release checks right now. Try again in a while.".to_string()
            })
        }
        other => Err(format!("HTTP {other} for {url}")),
    }
}

pub fn public_key_for(package: &str) -> Option<&'static str> {
    PUBLIC_KEYS
        .iter()
        .find(|(key, _)| *key == package)
        .map(|(_, key)| *key)
}

pub fn verify_signature(package: &str, bytes: &[u8], signature: &str) -> Result<(), String> {
    let key_text = public_key_for(package)
        .ok_or_else(|| format!("Peebify has no publisher key for the {package} package."))?;
    verify_with_key(package, key_text, bytes, signature)
}

fn verify_with_key(package: &str, key_text: &str, bytes: &[u8], signature: &str) -> Result<(), String> {
    use base64::Engine;
    use p384::ecdsa::signature::hazmat::PrehashVerifier;
    use p384::ecdsa::{Signature, VerifyingKey};
    use p384::pkcs8::DecodePublicKey;
    use sha2::{Digest, Sha256};

    let key_der = base64::engine::general_purpose::STANDARD
        .decode(key_text)
        .map_err(|e| format!("The {package} publisher key is malformed: {e}"))?;
    let key = VerifyingKey::from_public_key_der(&key_der)
        .map_err(|e| format!("The {package} publisher key could not be parsed: {e}"))?;
    let signature_der = base64::engine::general_purpose::STANDARD
        .decode(signature.trim())
        .map_err(|e| format!("The {package} release signature is malformed: {e}"))?;
    let signature = Signature::from_der(&signature_der)
        .map_err(|e| format!("The {package} release signature could not be parsed: {e}"))?;
    let digest = Sha256::digest(bytes);
    key.verify_prehash(&digest, &signature).map_err(|_| {
        format!(
            "The {package} package does not match its publisher's signature. \
             Peebify will not install it."
        )
    })
}

pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut file, &mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn integrity_digests(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut pins = BTreeMap::new();
    for name in CORE_FILES {
        let digest = file_sha256(&root.join(name))
            .map_err(|e| format!("Could not read {name} to record its checksum: {e}"))?;
        pins.insert(name.to_string(), digest);
    }
    Ok(pins)
}

pub fn record_integrity(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let pins = integrity_digests(root)?;
    update_state(root, |state| state.integrity = pins.clone())?;
    Ok(pins)
}

fn record_integrity_locked(
    root: &Path,
    state: &mut UpdateState,
) -> Result<BTreeMap<String, String>, String> {
    state.integrity = integrity_digests(root)?;
    write_state(root, state)?;
    Ok(state.integrity.clone())
}

pub fn verify_integrity(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let _guard = STATE_LOCK.lock();
    let mut state = load_state(root)?;
    if state.integrity.is_empty() {
        let recorded = record_integrity_locked(root, &mut state)?;
        log::warn!("xxmi: no checksums were on record, so the mod tools on disk were recorded as trusted");
        return Ok(recorded);
    }
    let mut verified = BTreeMap::new();
    let mut added = false;
    for name in CORE_FILES {
        let digest = file_sha256(&root.join(name))
            .map_err(|e| format!("Could not read {name} before starting the mod loader: {e}"))?;
        verified.insert(name.to_string(), digest.clone());
        match state.integrity.get(name) {
            Some(recorded) if *recorded == digest => {}
            Some(recorded) => {
                log::error!("xxmi: {name} checksum mismatch, recorded {recorded}, found {digest}");
                return Err(format!(
                    "{name} on disk does not match the copy Peebify installed. Something else \
                     changed it, so mods will not be loaded. Delete it from the mod tools folder \
                     (shown under Mods > Setup) and launch again to get a fresh copy."
                ));
            }
            None => {
                log::warn!("xxmi: {name} had no recorded checksum, so the copy on disk was recorded");
                state.integrity.insert(name.to_string(), digest);
                added = true;
            }
        }
    }
    if added {
        write_state(root, &state)?;
    }
    Ok(verified)
}

pub async fn install_package(
    app: &AppHandle,
    root: &Path,
    package: &str,
    release: &Release,
    mut on_percent: impl FnMut(f64),
) -> Result<String, String> {
    let signature = release.signature.as_deref().ok_or_else(|| {
        format!(
            "The latest {package} release carries no publisher signature, so Peebify will not \
             install it."
        )
    })?;

    if !release.download_url.starts_with("https://") {
        return Err(format!(
            "The {package} release links to a download that is not HTTPS, so Peebify will not \
             install it."
        ));
    }

    std::fs::create_dir_all(root).map_err(|e| format!("Could not create {root:?}: {e}"))?;
    let staging = root.join(STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("Could not create staging dir: {e}"))?;

    let archive_name = super::fs_util::sanitize_folder_name(&release.asset_name);
    let archive_name = if archive_name.to_ascii_lowercase().ends_with(".zip") {
        archive_name
    } else {
        format!("{package}-package.zip")
    };
    let archive = staging.join(archive_name);
    download_to(&release.download_url, &archive, |p| on_percent(p * 0.6)).await?;

    let checked = {
        let (archive, owned_package, signature) =
            (archive.clone(), package.to_string(), signature.to_string());
        tauri::async_runtime::spawn_blocking(move || {
            let bytes = std::fs::read(&archive).map_err(|e| {
                format!("Could not read the downloaded {owned_package} package: {e}")
            })?;
            verify_signature(&owned_package, &bytes, &signature)
        })
        .await
        .map_err(|e| format!("Could not check the downloaded {package} package: {e}"))?
    };
    checked.inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    on_percent(62.0);

    let extract_dir = staging.join("out");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("Could not create extract dir: {e}"))?;
    let seven_zip = resolve_7z_binary(app).await?;
    run_7z_extract(&seven_zip, &archive, &extract_dir, |p| {
        on_percent(62.0 + p * 0.33)
    })
    .await?;
    if let Some(link) = super::fs_util::find_link(&extract_dir) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "The {package} package contains a link ({}), so Peebify will not install it.",
            link.display()
        ));
    }

    let source = unwrap_single_dir(&extract_dir);
    let destination = package_dir(root, package);
    std::fs::create_dir_all(&destination)
        .map_err(|e| format!("Could not create {}: {e}", destination.display()))?;
    let is_core = package == CORE_KEY;
    if !is_core {
        run_update_commands(&xcmd_path(&source), "PreInstall", &destination);
    }
    {
        let (source, destination) = (source.clone(), destination.clone());
        tauri::async_runtime::spawn_blocking(move || merge_dir(&source, &destination))
            .await
            .map_err(|e| format!("Could not install the {package} package: {e}"))??;
    }
    if !is_core {
        run_update_commands(&xcmd_path(&destination), "PostInstall", &destination);
    }
    on_percent(98.0);

    let _ = std::fs::remove_dir_all(&staging);

    if is_core {
        record_integrity(root)?;
    } else {
        disable_shaderfixes_extras(&destination);
    }

    record_version(root, package, &release.tag)?;
    on_percent(100.0);

    log::info!(
        "xxmi: installed {package} {} into {} (signature verified)",
        release.tag,
        destination.display()
    );
    Ok(release.tag.clone())
}

pub fn installing() -> bool {
    INSTALLING_NEW.load(Ordering::SeqCst)
}

fn current_root(app: &AppHandle) -> PathBuf {
    let state = app.state::<BackendState>();
    root(&state.config, &state.user_data)
}

struct InstallingGuard(AppHandle);

impl InstallingGuard {
    fn start(app: &AppHandle) -> Self {
        INSTALLING_NEW.store(true, Ordering::SeqCst);
        let _ = app.emit("mods-status-changed", json!({}));
        Self(app.clone())
    }
}

impl Drop for InstallingGuard {
    fn drop(&mut self) {
        INSTALLING_NEW.store(false, Ordering::SeqCst);
        let _ = self.0.emit("mods-status-changed", json!({}));
    }
}

pub fn start_background(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut next: Option<(Instant, PathBuf)> = None;
        let mut was_on = false;
        loop {
            let on = super::mods::master_enabled(&app);
            let root = current_root(&app);
            let due = !was_on
                || next
                    .as_ref()
                    .is_none_or(|(at, planned_for)| Instant::now() >= *at || *planned_for != root);
            if on && due {
                let wait = match run_once(&app).await {
                    Ok(true) => Some(CHECK_INTERVAL),
                    Ok(false) => Some(RETRY_INTERVAL),
                    Err(e) if e == OFFLINE => None,
                    Err(e) => {
                        log::warn!("xxmi: background update check skipped: {e}");
                        Some(CHECK_INTERVAL)
                    }
                };
                if let Some(wait) = wait {
                    next = Some((Instant::now() + wait, root));
                }
            }
            was_on = on;
            tokio::time::sleep(WATCH_TICK).await;
        }
    });
}

fn modded_game_active(app: &AppHandle) -> bool {
    let state = app.state::<BackendState>();
    game_profiles::GAME_IDS.iter().any(|&id| {
        state.game.is_game_active_id(id) && super::mods::active_for(app, id, game_profiles::profile(id))
    })
}

fn toolchain_in_use(app: &AppHandle) -> bool {
    modded_game_active(app) || loader_active()
}

pub fn on_game_stopped(app: &AppHandle) {
    if !PENDING.swap(false, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_once(&app).await {
            log::warn!("xxmi: deferred update check skipped: {e}");
        }
    });
}

fn is_current(root: &Path, package: &str, release: &Release) -> bool {
    package_looks_installed(root, package)
        && installed_versions(root).get(package) == Some(&release.tag)
}

async fn run_once(app: &AppHandle) -> Result<bool, String> {
    if !super::mods::master_enabled(app) {
        return Ok(true);
    }
    if !http::is_online_cached() {
        return Err(OFFLINE.to_string());
    }
    let root = current_root(app);
    let mut packages = vec![CORE_KEY];
    packages.extend(supported_variants());

    let missing = packages.iter().any(|p| !package_looks_installed(&root, p));
    let _installing = missing.then(|| InstallingGuard::start(app));

    let mut complete = true;
    let mut core_changed = false;
    for package in packages {
        let release = match check_package(&root, package).await {
            Ok(release) => release,
            Err(e) => {
                log::warn!("xxmi: could not check {package} for updates: {e}");
                complete = false;
                continue;
            }
        };
        if is_current(&root, package, &release) {
            continue;
        }
        let _update = UPDATE_LOCK.lock().await;
        if is_current(&root, package, &release) {
            continue;
        }
        if toolchain_in_use(app) {
            PENDING.store(true, Ordering::SeqCst);
            if !WAITING_LOGGED.swap(true, Ordering::SeqCst) {
                log::info!("xxmi: {package} {} is ready, installing once no game is running", release.tag);
            }
            break;
        }
        let Some(_guard) = try_lock_variant(package) else {
            continue;
        };
        let tag = match install_package(app, &root, package, &release, |_| {}).await {
            Ok(tag) => tag,
            Err(e) => {
                log::warn!("xxmi: background install of {package} failed: {e}");
                continue;
            }
        };
        log::info!("xxmi: background update installed {package} {tag}");
        WAITING_LOGGED.store(false, Ordering::SeqCst);
        if package == CORE_KEY {
            core_changed = true;
        } else if let Err(e) = finish_variant(&root, package, core_changed).await {
            log::warn!("xxmi: could not finish setting up {package}: {e}");
        }
        let _ = app.emit("mods-status-changed", json!({}));
    }
    Ok(complete)
}

// ------------ Tests ------------
// Runs the install, update, settings and ini rewriting logic against scratch folders.
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peebify-xxmi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zzmi")).unwrap();
        std::fs::write(dir.join("zzmi").join("d3dx.ini"), "[Loader]\n").unwrap();
        dir
    }

    fn install_core(root: &Path) {
        for name in CORE_FILES {
            std::fs::write(root.join(name), b"MZ").unwrap();
        }
    }

    #[test]
    fn the_launch_key_is_commented_out_rather_than_left_empty() {
        let original = "[Loader]\nloader = 3dmloader.exe\nlaunch = Game.exe\nrequire_admin = 1\n";
        let managed = vec![
            (
                "Loader".to_string(),
                "loader".to_string(),
                IniEntry::Set(EXE_NAME.to_string()),
            ),
            ("Loader".to_string(), "launch".to_string(), IniEntry::Unset),
        ];
        let out = rewrite_ini(original, &managed);
        assert!(out.contains(";launch = Game.exe"), "{out}");
        assert!(!out.lines().any(|l| l.trim() == "launch ="), "{out}");
        assert!(out.contains(&format!("loader = {EXE_NAME}")), "{out}");
    }

    #[test]
    fn exclude_recursive_keeps_the_other_patterns() {
        let original = "[Include]\ninclude_recursive = Mods\nexclude_recursive = desktop.ini\n\n[Hunting]\nhunting = 0\n";
        let managed = vec![(
            "Include".to_string(),
            "exclude_recursive".to_string(),
            IniEntry::EnsureLine("DISABLED*".to_string()),
        )];
        let out = rewrite_ini(original, &managed);
        assert!(out.contains("exclude_recursive = desktop.ini"), "{out}");
        assert!(out.contains("exclude_recursive = DISABLED*"), "{out}");
        let again = rewrite_ini(&out, &managed);
        assert_eq!(again, out);
        assert_eq!(out.matches("DISABLED*").count(), 1);
    }

    #[test]
    fn entries_with_line_breaks_or_brackets_are_never_written() {
        let original = "[Hunting]\nhunting = 0\n";
        let managed = vec![
            (
                "Hunting".to_string(),
                "reload_fixes".to_string(),
                IniEntry::Set("no_modifiers VK_F10\r\n[System]\nproxy_d3d11 = x.dll".to_string()),
            ),
            (
                "System]\n[Loader".to_string(),
                "launch".to_string(),
                IniEntry::Set("x.exe".to_string()),
            ),
            (
                "Hunting".to_string(),
                "hunting".to_string(),
                IniEntry::Set("2".to_string()),
            ),
        ];
        let out = rewrite_ini(original, &managed);
        assert_eq!(out, "[Hunting]\nhunting = 2\n");
    }

    #[test]
    fn defaults_lose_to_user_overrides_and_managed_keys() {
        let dir = scratch("ini");
        let variant = dir.join("zzmi");
        std::fs::write(
            variant.join("d3dx.ini"),
            "[Loader]\ntarget = Game.exe\nloader = XXMI Launcher.exe\nrequire_admin = true\n\n[Rendering]\ntexture_hash = 0\n\n[Logging]\nshow_warnings = 1\n",
        )
        .unwrap();
        apply_managed_ini(
            &variant,
            &IniOptions {
                target: "Real.exe".into(),
                hunting_mode: HUNTING_KEYBINDS_ONLY,
                reload_key: DEFAULT_RELOAD_KEY.into(),
                defaults: vec![
                    ("Rendering".into(), "texture_hash".into(), "1".into()),
                    ("Logging".into(), "show_warnings".into(), "0".into()),
                    ("Loader".into(), "target".into(), "Ignored.exe".into()),
                ],
                screen: Some((2560, 1440)),
            },
            &[("Logging".into(), "show_warnings".into(), "1".into())],
        )
        .unwrap();
        let out = std::fs::read_to_string(variant.join("d3dx.ini")).unwrap();
        assert!(out.contains("target = Real.exe"), "{out}");
        assert!(out.contains("texture_hash = 1"), "{out}");
        assert!(out.contains("show_warnings = 1"), "{out}");
        assert!(out.contains("screen_width = 2560"), "{out}");
        assert!(out.contains(&format!("loader = {EXE_NAME}")), "{out}");
        assert!(out.contains("require_admin = true"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn updates_merge_into_shaderfixes_without_dropping_extra_files() {
        let root = scratch("merge");
        let source = root.join("pkg");
        std::fs::create_dir_all(source.join("ShaderFixes")).unwrap();
        std::fs::create_dir_all(source.join("Mods")).unwrap();
        std::fs::write(source.join("ShaderFixes").join("fix.ini"), "new").unwrap();
        std::fs::write(source.join("d3dx.ini"), "[Loader]").unwrap();
        let destination = root.join("zzmi");
        std::fs::create_dir_all(destination.join("ShaderFixes")).unwrap();
        std::fs::create_dir_all(destination.join("Mods")).unwrap();
        std::fs::write(destination.join("ShaderFixes").join("fix.ini"), "old").unwrap();
        std::fs::write(destination.join("ShaderFixes").join("dump.txt"), "mine").unwrap();
        merge_dir(&source, &destination).unwrap();
        assert!(destination.join("d3dx.ini").is_file());
        let fix = std::fs::read_to_string(destination.join("ShaderFixes").join("fix.ini")).unwrap();
        assert_eq!(fix, "new");
        assert!(destination.join("ShaderFixes").join("dump.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn update_commands_follow_xxmi_path_rules() {
        let dir = Path::new("X");
        assert_eq!(
            xcmd_delete_target(dir, "Core\\GIMI\\old.ini").unwrap(),
            dir.join("Core").join("GIMI").join("old.ini")
        );
        assert_eq!(
            xcmd_delete_target(dir, "../ShaderFixes/./x.txt").unwrap(),
            dir.join("ShaderFixes").join("x.txt")
        );
        assert!(xcmd_delete_target(dir, "Core").is_err());
        assert!(xcmd_delete_target(dir, "Mods/a").is_err());
        assert!(xcmd_delete_target(dir, "C:\\Windows\\x").is_err());
        assert!(xcmd_delete_target(dir, "").is_err());
        let text = "[PreInstall]\ndelete = Core/a.ini\n; delete = Core/b.ini\n[PostInstall]\ndelete = ShaderFixes/c.ini\nrun = x\n";
        assert_eq!(
            xcmd_commands(text, "PreInstall"),
            vec![("delete".to_string(), "Core/a.ini".to_string())]
        );
        assert_eq!(xcmd_commands(text, "postinstall").len(), 2);
    }

    #[test]
    fn update_commands_delete_inside_the_package_only() {
        let root = scratch("xcmd");
        let variant = root.join("zzmi");
        std::fs::create_dir_all(variant.join("Core").join("Old")).unwrap();
        std::fs::write(variant.join("Core").join("Old").join("a.ini"), "x").unwrap();
        std::fs::write(variant.join("Core").join("keep.ini"), "x").unwrap();
        std::fs::write(
            xcmd_path(&variant),
            "[PostInstall]\ndelete = Core\\Old\ndelete = Core\ndelete = ..\\..\\zzmi\\d3dx.ini\n",
        )
        .unwrap();
        assert_eq!(run_update_commands(&xcmd_path(&variant), "PostInstall", &variant), 1);
        assert!(!variant.join("Core").join("Old").exists());
        assert!(variant.join("Core").join("keep.ini").exists());
        assert!(variant.join("d3dx.ini").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stray_configs_inside_mods_are_parked() {
        let dir = scratch("rogue");
        let mods = dir.join("zzmi").join("Mods");
        std::fs::create_dir_all(mods.join("Cool Mod").join("sub")).unwrap();
        std::fs::write(mods.join("Cool Mod").join("mod.ini"), "[TextureOverride]").unwrap();
        std::fs::write(mods.join("Cool Mod").join("sub").join("d3dx.ini"), "[Loader]").unwrap();
        std::fs::create_dir_all(mods.join("DISABLED Old")).unwrap();
        std::fs::write(mods.join("DISABLED Old").join("d3dx.ini"), "[Loader]").unwrap();
        assert_eq!(park_rogue_config_files(&mods, ROGUE_SCAN_DEPTH), 1);
        assert!(mods.join("Cool Mod").join("sub").join("DISABLED d3dx.ini").exists());
        assert!(mods.join("Cool Mod").join("mod.ini").exists());
        assert!(mods.join("DISABLED Old").join("d3dx.ini").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_variant_copy_with_the_same_size_and_time_is_still_replaced() {
        let dir = scratch("relink");
        install_core(&dir);
        link_core_libs(&dir, "zzmi", false).unwrap();
        let copy = dir.join("zzmi").join("d3d11.dll");
        let modified = std::fs::metadata(&copy).unwrap().modified().unwrap();
        std::fs::write(&copy, b"XX").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        link_core_libs(&dir, "zzmi", false).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"MZ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_loader_is_handed_the_checksums_that_were_verified() {
        use peebify_helpers::mods::{parse_options, sha256_hex};
        let dir = scratch("loader-checksums");
        for name in CORE_FILES {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        std::fs::write(dir.join("zzmi").join("d3d11.dll"), b"stale").unwrap();
        let integrity = verify_integrity(&dir).unwrap();
        link_core_libs(&dir, "zzmi", false).unwrap();
        let module = dir.join("zzmi").join("d3d11.dll");
        let mut args = vec!["--dll".to_string(), dir.join(LOADER_DLL).display().to_string()];
        args.extend(checksum_arg(&integrity, "--dll-sha256", LOADER_DLL));
        args.extend(["--module".to_string(), module.display().to_string()]);
        args.extend(checksum_arg(&integrity, "--module-sha256", "d3d11.dll"));
        args.extend(
            ["--target", "ZenlessZoneZero.exe", "--mode", "hook", "--timeout", "30", "--event", "Local\\E"]
                .map(String::from),
        );
        let options = parse_options(&args).unwrap();
        let loader = file_sha256(&dir.join(LOADER_DLL)).unwrap();
        let linked = file_sha256(&module).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(sha256_hex(&options.dll_sha256.unwrap()), loader);
        assert_eq!(sha256_hex(&options.module_sha256.unwrap()), linked);
        assert_eq!(args[2], "--dll-sha256");
        assert_eq!(args[6], "--module-sha256");

        let mut partial = integrity.clone();
        partial.remove("d3d11.dll");
        partial.insert(LOADER_DLL.to_string(), "not a digest".to_string());
        assert!(checksum_arg(&partial, "--dll-sha256", LOADER_DLL).is_empty());
        assert!(checksum_arg(&partial, "--module-sha256", "d3d11.dll").is_empty());
    }

    #[test]
    fn stored_values_outside_the_declared_settings_are_dropped() {
        assert_eq!(
            sanitize_stored("System/proxy_d3d11", &json!(r"\\host\share\x.dll")),
            None
        );
        assert_eq!(sanitize_stored("Loader/target", &json!("Evil.exe")), None);
        assert_eq!(sanitize_stored("no compound", &json!("1")), None);
    }

    #[test]
    fn declared_settings_are_normalized_by_kind() {
        assert_eq!(
            sanitize_stored("Hunting/reload_fixes", &json!("no_modifiers VK_F10")),
            Some(("Hunting", "reload_fixes", "no_modifiers VK_F10".to_string()))
        );
        assert_eq!(
            sanitize_stored("hunting/RELOAD_CONFIG", &json!("ctrl alt VK_F5")),
            Some(("Hunting", "reload_config", "ctrl alt VK_F5".to_string()))
        );
        assert_eq!(
            sanitize_stored("Logging/show_warnings", &json!(true)),
            Some(("Logging", "show_warnings", "1".to_string()))
        );
        assert_eq!(
            sanitize_stored("System/dll_initialization_delay", &json!("99999")),
            Some(("System", "dll_initialization_delay", "10000".to_string()))
        );
        assert_eq!(sanitize_stored("Logging/show_warnings", &json!("yes")), None);
    }

    #[test]
    fn stored_values_cannot_smuggle_lines_or_sections() {
        assert_eq!(
            sanitize_stored(
                "Hunting/reload_fixes",
                &json!("no_modifiers VK_F10\r\n[System]\nproxy_d3d11 = x.dll")
            ),
            None
        );
        assert_eq!(
            sanitize_stored("Hunting/reload_fixes", &json!(r"no_modifiers \\host\x.dll")),
            None
        );
        assert_eq!(
            sanitize_stored("System/dll_initialization_delay", &json!("5\n[x]")),
            None
        );
    }


    #[test]
    fn a_signature_is_read_out_of_the_release_notes() {
        let body = "Some notes\r\n\r\n## Signature\r\n- MEUCIQDabc+/==\r\n\r\nmore";
        assert_eq!(parse_signature(body), None);
        let body = "## Signature\n- MEUCIQDa\n";
        assert_eq!(parse_signature(body).as_deref(), Some("MEUCIQDa"));
        let body = "## Signature\r\n- MEUCIQDabcd=\r\n";
        assert_eq!(parse_signature(body).as_deref(), Some("MEUCIQDabcd="));
    }

    #[test]
    fn a_release_needs_a_package_zip_and_keeps_its_signature() {
        let text = r###"{"tag_name":"v1.2.3","body":"## Signature\n- AAAA\n","assets":[{"name":"XXMI-PACKAGE-v1.2.3.zip","browser_download_url":"https://x/y.zip"}]}"###;
        let release = parse_release("xxmi", text).unwrap();
        assert_eq!(release.tag, "v1.2.3");
        assert_eq!(release.asset_name, "XXMI-PACKAGE-v1.2.3.zip");
        assert_eq!(release.signature.as_deref(), Some("AAAA"));
        let text = r#"{"tag_name":"v1","body":"","assets":[{"name":"notes.txt","browser_download_url":"u"}]}"#;
        assert!(parse_release("xxmi", text).is_err());
    }

    #[test]
    fn every_package_has_a_publisher_key_that_parses() {
        use base64::Engine;
        use p384::ecdsa::VerifyingKey;
        use p384::pkcs8::DecodePublicKey;
        for (package, key) in PUBLIC_KEYS {
            let der = base64::engine::general_purpose::STANDARD.decode(key).unwrap();
            assert!(VerifyingKey::from_public_key_der(&der).is_ok(), "{package}");
        }
    }

    #[test]
    fn a_bad_signature_is_refused() {
        let bogus = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [0x30u8, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01],
        );
        assert!(verify_signature("xxmi", b"hello", &bogus).is_err());
        assert!(verify_signature("xxmi", b"hello", "not base64!").is_err());
    }

    fn empty_scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("peebify-xxmi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_pin_is_recorded_and_a_changed_file_is_refused() {
        let dir = empty_scratch("pins");
        for name in CORE_FILES {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        let mut state = UpdateState::default();
        for name in [LOADER_DLL, "d3d11.dll"] {
            state
                .integrity
                .insert(name.to_string(), file_sha256(&dir.join(name)).unwrap());
        }
        write_state(&dir, &state).unwrap();
        let verified = verify_integrity(&dir).unwrap();
        assert!(current_state(&dir).unwrap().integrity.contains_key("d3dcompiler_47.dll"));
        for name in CORE_FILES {
            assert_eq!(verified[name], file_sha256(&dir.join(name)).unwrap());
        }
        std::fs::write(dir.join("d3dcompiler_47.dll"), b"swapped").unwrap();
        assert!(verify_integrity(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_state_file_reads_as_empty() {
        let dir = empty_scratch("torn");
        std::fs::write(dir.join(STATE_FILE), "{\"packages\":{\"xx").unwrap();
        assert!(current_state(&dir).unwrap().packages.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_state_file_that_cannot_be_read_is_left_alone() {
        let dir = empty_scratch("blocked");
        for name in CORE_FILES {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        std::fs::create_dir_all(dir.join(STATE_FILE)).unwrap();
        let recorded = record_integrity(&dir);
        let still_there = dir.join(STATE_FILE).is_dir();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(recorded.is_err());
        assert!(still_there);
    }

    #[test]
    fn a_release_check_keeps_pins_recorded_while_it_ran() {
        let dir = empty_scratch("pin-race");
        for name in CORE_FILES {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        let mut stale = UpdateState::default();
        stale.integrity.insert("d3d11.dll".into(), "00".into());
        stale.packages.insert(
            "xxmi".into(),
            PackageState {
                etag: Some("\"abc\"".into()),
                ..Default::default()
            },
        );
        write_state(&dir, &stale).unwrap();

        let entry = current_state(&dir).unwrap().packages.remove("xxmi").unwrap();
        let pins = record_integrity(&dir).unwrap();
        update_package(&dir, "xxmi", |stored| stored.latest_tag = Some("v2".into()));
        let after = current_state(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(entry.etag.as_deref(), Some("\"abc\""));
        assert_eq!(after.integrity, pins);
        assert_eq!(after.packages["xxmi"].latest_tag.as_deref(), Some("v2"));
        assert_eq!(after.packages["xxmi"].etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn concurrent_state_writers_lose_nothing() {
        let dir = empty_scratch("writers");
        for name in CORE_FILES {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        let packages: Vec<String> = (0..8).map(|i| format!("pkg{i}")).collect();
        std::thread::scope(|scope| {
            for package in &packages {
                let dir = &dir;
                scope.spawn(move || {
                    for round in 0..5 {
                        update_package(dir, package, |stored| stored.retry_after = Some(round));
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..5 {
                    verify_integrity(&dir).unwrap();
                    record_integrity(&dir).unwrap();
                }
            });
        });
        let state = current_state(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(state.integrity.len(), CORE_FILES.len());
        for package in &packages {
            assert_eq!(state.packages[package].retry_after, Some(4), "{package}");
        }
    }

    #[test]
    fn a_pinned_publisher_signature_still_verifies() {
        let key = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEcszeM3U3YiReAV2pLkj6AoSVUi3EI1bH499R3PVqXhnedCrNOhn3mvNy3JcF9WDYV7kFEaBAasE3vmG2lZnOTIbBxTEK7cxP8LBKvJOuXGPRXkoBV89q57pfrIXn3mZi";
        let sig = "MGYCMQDDJeBNaUthhNxgQeHTZNF4sG7Su4LXw0kifkTE8koTy6FVoxfhqaoM/6hBb3w8WKkCMQCnUcUcckgCdy2g8pz2+l4e/7TxTgleQHEVReI660bVqm80AnqFfR5WXQwikLYG03Y=";
        assert!(verify_with_key("xxmi", key, b"peebify-xxmi-kat", sig).is_ok());
        assert!(verify_with_key("xxmi", key, b"peebify-xxmi-kaT", sig).is_err());
    }
}
