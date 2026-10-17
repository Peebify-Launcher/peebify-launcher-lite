// ------------ File System Helpers ------------
// Shared odds and ends for touching the disk: finding bundled resources, safe atomic writes, working out why a write failed, path safety checks, MD5 hashing and reading the JSON stores.
// Also the small text formats several games share: dotted version strings and INI files.
use std::path::{Path, PathBuf};

pub use peebify_transfer::fsx::{
    checksum_file, checksum_matches, classify, explain_disk_full, fat_limit_message,
    finalize_replace, fmt_io, is_access_denied, md5_file, md5_hex, not_writable_message,
    retry_while_locked, safe_join, tmp_sibling, write_all_at, write_atomic, FailureKind,
    CANCELLED_MSG, CRC32_PREFIX, CRC64_PREFIX,
};

pub fn is_disk_full(message: &str) -> bool {
    message.starts_with("Not enough disk space") || classify(message) == FailureKind::DiskFull
}

pub fn walk_files(
    dir: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    mut visit: impl FnMut(&Path, u64),
) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => {
                    visit(&entry.path(), entry.metadata().map(|m| m.len()).unwrap_or(0));
                }
                _ => {}
            }
        }
    }
    true
}

pub fn folder_bytes(dir: &Path, cancel: &std::sync::atomic::AtomicBool) -> Option<u64> {
    let mut total = 0u64;
    walk_files(dir, cancel, |_, len| total = total.saturating_add(len)).then_some(total)
}

pub fn directory_size(dir: &Path) -> u64 {
    folder_bytes(dir, &std::sync::atomic::AtomicBool::new(false)).unwrap_or(0)
}

pub fn resource(app: &tauri::AppHandle, name: &str) -> Option<PathBuf> {
    use tauri::Manager;

    if let Ok(resource_dir) = app.path().resource_dir() {
        for candidate in [
            resource_dir.join("resources").join(name),
            resource_dir.join(name),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if cfg!(debug_assertions) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for candidate in [
            root.join("resources").join(name),
            root.join("..").join("target").join("release").join(name),
            root.join("..").join("target").join("debug").join(name),
        ] {
            if candidate.exists() {
                log::info!(
                    "resolved {name} to {} (modified {:?})",
                    candidate.display(),
                    std::fs::metadata(&candidate)
                        .and_then(|m| m.modified())
                        .ok()
                );
                return Some(candidate);
            }
        }
    }
    None
}

pub fn allow_asset_dir(app: &tauri::AppHandle, dir: &Path) {
    if !asset_scope_accepts(dir) {
        log::warn!("asset scope: ignored the relative folder {}", dir.display());
        return;
    }
    if dir.parent().is_none() {
        log::warn!("asset scope: ignored the drive root {}", dir.display());
        return;
    }
    allow_asset_dir_shallow(app, dir);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
            allow_asset_dir_shallow(app, &entry.path());
        }
    }
}

pub fn allow_asset_dir_shallow(app: &tauri::AppHandle, dir: &Path) {
    use tauri::Manager;

    if already_allowed(dir) {
        return;
    }
    match app.asset_protocol_scope().allow_directory(dir, false) {
        Ok(()) => remember_allowed(dir),
        Err(e) => log::warn!("asset scope: could not allow {}: {e}", dir.display()),
    }
}

pub fn allow_asset_file(app: &tauri::AppHandle, file: &Path) {
    use tauri::Manager;

    if !asset_scope_accepts(file) {
        log::warn!("asset scope: ignored the relative file {}", file.display());
        return;
    }
    if already_allowed(file) {
        return;
    }
    match app.asset_protocol_scope().allow_file(file) {
        Ok(()) => remember_allowed(file),
        Err(e) => log::warn!("asset scope: could not allow {}: {e}", file.display()),
    }
}

static ALLOWED_ASSET_PATHS: parking_lot::Mutex<Vec<PathBuf>> =
    parking_lot::Mutex::new(Vec::new());

fn already_allowed(path: &Path) -> bool {
    ALLOWED_ASSET_PATHS.lock().iter().any(|p| p == path)
}

fn remember_allowed(path: &Path) {
    let mut allowed = ALLOWED_ASSET_PATHS.lock();
    if !allowed.iter().any(|p| p == path) {
        allowed.push(path.to_path_buf());
    }
}

fn asset_scope_accepts(path: &Path) -> bool {
    path.is_absolute()
}

pub fn manifest_key(rel: &str) -> String {
    rel.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase()
}

const WRITE_PROBE_NAME: &str = ".peebify-write-test";

pub fn probe_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(WRITE_PROBE_NAME);
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}

pub fn dir_denies_writes(dir: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;

    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!("{WRITE_PROBE_NAME}-{}", uuid::Uuid::new_v4().simple()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).custom_flags(0x0400_0000);
    match options.open(&probe) {
        Ok(file) => {
            drop(file);
            false
        }
        Err(e) => is_access_denied(&e),
    }
}

pub fn list_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    fn walk(dir: &Path, files: &mut Vec<(PathBuf, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            log::warn!("Cannot read {}", dir.display());
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir() && !t.is_symlink())
                .unwrap_or(false);
            if is_dir {
                walk(&entry.path(), files);
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push((entry.path(), size));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files
}

pub fn find_link(dir: &Path) -> Option<PathBuf> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                return Some(path);
            }
            if meta.is_dir() {
                pending.push(path);
            }
        }
    }
    None
}

pub fn sanitize_folder_name(name: &str) -> String {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let capped: String = trimmed.chars().take(120).collect();
    let stem = capped.split('.').next().unwrap_or(&capped).to_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("{capped}_");
    }
    capped
}

pub fn plain_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    }
}

pub fn corrupt_sibling(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.corrupt-{}", path.display(), chrono::Utc::now().timestamp_millis()))
}

pub fn read_json_store(path: &Path) -> Result<Option<serde_json::Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(unfinished_save(path)),
        Err(e) => return Err(fmt_io(&format!("Could not read {}", path.display()), &e)),
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            let aside = corrupt_sibling(path);
            match std::fs::rename(path, &aside) {
                Ok(()) => log::warn!(
                    "{} could not be parsed ({e}), so it was set aside as {} and starts empty",
                    path.display(),
                    aside.display()
                ),
                Err(re) => log::warn!(
                    "{} could not be parsed ({e}) and could not be set aside ({re}), so it starts empty",
                    path.display()
                ),
            }
            Ok(None)
        }
    }
}

fn unfinished_save(path: &Path) -> Option<serde_json::Value> {
    let tmp = tmp_sibling(path);
    let bytes = std::fs::read(&tmp).ok()?;
    let value = serde_json::from_slice(&bytes).ok()?;
    log::warn!(
        "{} is missing, so the copy an unfinished save left in {} was used",
        path.display(),
        tmp.display()
    );
    Some(value)
}

// ------------ Version Strings ------------
// Game versions are dotted strings that are not always semver ("2.4.0", "1.0.12.3", "1.2.0b").

fn version_parts(version: &str) -> Vec<Result<i64, String>> {
    version
        .trim()
        .split('.')
        .map(|part| part.parse::<i64>().map_err(|_| part.to_lowercase()))
        .collect()
}

pub fn is_version_newer(remote: &str, local: &str) -> bool {
    if remote.is_empty() || local.is_empty() {
        return false;
    }
    let (remote, local) = (version_parts(remote), version_parts(local));
    let number = |parts: &[Result<i64, String>], i: usize| match parts.get(i) {
        Some(Ok(n)) => *n,
        _ => 0,
    };
    (0..remote.len().max(local.len()))
        .map(|i| number(&remote, i).cmp(&number(&local, i)))
        .find(|order| order.is_ne())
        .is_some_and(|order| order.is_gt())
}

pub fn json_version(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn same_version(a: &str, b: &str) -> bool {
    let (a, b) = (version_parts(a), version_parts(b));
    let zero = Ok(0i64);
    (0..a.len().max(b.len())).all(|i| a.get(i).unwrap_or(&zero) == b.get(i).unwrap_or(&zero))
}

// ------------ INI Files ------------

pub fn ini_key_value(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.starts_with(';') || trimmed.starts_with('#') {
        return None;
    }
    let (k, v) = trimmed.split_once('=')?;
    Some((k.trim(), v.trim()))
}

pub fn ini_entries(text: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    let mut section = "";
    text.lines().filter_map(move |line| {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
            return None;
        }
        let (key, value) = ini_key_value(trimmed)?;
        Some((section, key, value))
    })
}

pub fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    ini_entries(text)
        .find(|(s, k, _)| s.eq_ignore_ascii_case(section) && k.eq_ignore_ascii_case(key))
        .map(|(_, _, v)| v.to_string())
}

// ------------ Disk Space and Folder Access ------------
// Free space on the drive under a path, a write probe that leaves nothing behind, and folders only this account can change.
pub fn drive_root(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut components = Path::new(path).components();
    match components.next() {
        Some(std::path::Component::Prefix(prefix)) => {
            Some(format!("{}\\", prefix.as_os_str().to_string_lossy()))
        }
        _ => None,
    }
}

pub fn nearest_existing_dir(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.is_dir())
}

pub fn existing_probe_dir(path: &Path) -> Option<String> {
    let text = nearest_existing_dir(path)?.to_string_lossy();
    Some(if text.ends_with('\\') || text.ends_with('/') {
        text.into_owned()
    } else {
        format!("{text}\\")
    })
}

pub fn disk_space_for(path: &Path) -> Option<(u64, u64)> {
    let probe = existing_probe_dir(path)?;
    disk_free_total(&probe).ok()
}

pub fn remove_empty_parents(path: &Path, root: &Path) {
    let mut parent = path.parent();
    while let Some(folder) = parent {
        if folder == root || std::fs::remove_dir(folder).is_err() {
            break;
        }
        parent = folder.parent();
    }
}

pub fn free_bytes_for(path: &Path) -> Option<u64> {
    disk_space_for(path).map(|(free, _)| free)
}

pub fn disk_free_total(root: &str) -> Result<(u64, u64), String> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide = peebify_helpers::common::wide(root);
    let mut free_available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return Err(format!("GetDiskFreeSpaceExW failed for {root}"));
    }
    Ok((free_available, total))
}

pub fn preview_writable(dir: &Path) -> Result<(), String> {
    let Some(existing) = nearest_existing_dir(dir) else {
        return Err(format!("{} is not reachable.", dir.display()));
    };
    if existing == dir {
        return if super::fs_util::dir_denies_writes(dir) {
            Err(format!("Access is denied to {}.", dir.display()))
        } else {
            Ok(())
        };
    }
    let probe = existing.join(format!(
        ".peebify-folder-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&probe).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::remove_dir(&probe) {
        log::warn!("Could not remove the write probe {}: {e}", probe.display());
    }
    Ok(())
}

pub fn create_restricted_dir(dir: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, CreateWellKnownSid, GetLengthSid, GetTokenInformation,
        InitializeAcl, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TokenUser, WinAuthenticatedUserSid,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL, ACL_REVISION,
        CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE, PSID, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
        WELL_KNOWN_SID_TYPE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    let failed = |what: &str| format!("{what} failed: {}", std::io::Error::last_os_error());

    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(failed("OpenProcessToken"));
    }
    let mut user_buf = vec![0u64; 64];
    let mut needed = 0u32;
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            user_buf.as_mut_ptr().cast(),
            (user_buf.len() * std::mem::size_of::<u64>()) as u32,
            &mut needed,
        )
    };
    let read_error = (read == 0).then(|| failed("GetTokenInformation"));
    unsafe { CloseHandle(token) };
    if let Some(e) = read_error {
        return Err(e);
    }
    let user_sid: PSID = unsafe { (*(user_buf.as_ptr() as *const TOKEN_USER)).User.Sid };

    let well_known = |kind: WELL_KNOWN_SID_TYPE| -> Result<Vec<u8>, String> {
        let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = SECURITY_MAX_SID_SIZE;
        let ok = unsafe {
            CreateWellKnownSid(kind, std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut size)
        };
        if ok == 0 {
            return Err(failed("CreateWellKnownSid"));
        }
        Ok(sid)
    };
    let mut system = well_known(WinLocalSystemSid)?;
    let mut admins = well_known(WinBuiltinAdministratorsSid)?;
    let mut everyone_signed_in = well_known(WinAuthenticatedUserSid)?;
    let entries: [(PSID, u32); 4] = [
        (user_sid, FILE_ALL_ACCESS),
        (system.as_mut_ptr().cast(), FILE_ALL_ACCESS),
        (admins.as_mut_ptr().cast(), FILE_ALL_ACCESS),
        (
            everyone_signed_in.as_mut_ptr().cast(),
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        ),
    ];

    let acl_len = std::mem::size_of::<ACL>() as u32
        + entries
            .iter()
            .map(|(sid, _)| 8 + unsafe { GetLengthSid(*sid) })
            .sum::<u32>();
    let mut acl_buf = vec![0u32; (acl_len as usize).div_ceil(4)];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, acl_len, ACL_REVISION) } == 0 {
        return Err(failed("InitializeAcl"));
    }
    for (sid, mask) in entries {
        let ok = unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                mask,
                sid,
            )
        };
        if ok == 0 {
            return Err(failed("AddAccessAllowedAceEx"));
        }
    }

    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let descriptor_ptr = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
        return Err(failed("InitializeSecurityDescriptor"));
    }
    if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl, 0) } == 0 {
        return Err(failed("SetSecurityDescriptorDacl"));
    }
    if unsafe {
        SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
    } == 0
    {
        return Err(failed("SetSecurityDescriptorControl"));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
            return Ok(false);
        }
        return Err(format!("CreateDirectoryW failed: {error}"));
    }
    Ok(true)
}

pub fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

pub fn same_path(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

pub fn is_same_or_inside(child: &str, parent: &str) -> bool {
    let (child, parent) = (normalize_path(child), normalize_path(parent));
    !parent.is_empty() && (child == parent || child.starts_with(&format!("{parent}\\")))
}

pub fn create_shared_root_for(install_path: &Path, shared_root: &Path) -> bool {
    let root = shared_root.to_string_lossy();
    if !install_path
        .ancestors()
        .any(|dir| same_path(&dir.to_string_lossy(), &root))
    {
        return false;
    }
    match create_restricted_dir(shared_root) {
        Ok(true) => {
            log::info!(
                "Created {} writable by this account only.",
                shared_root.display()
            );
            true
        }
        Ok(false) => false,
        Err(e) => {
            log::warn!(
                "Could not create {} with restricted access: {e}",
                shared_root.display()
            );
            false
        }
    }
}

#[cfg(test)]
pub(crate) struct TempDir(pub(crate) PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "peebify-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = safe_join(&self.0, rel).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ------------ File System Tests ------------
// Covers the helpers above.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_bytes_adds_up_nested_files() {
        let dir = TempDir::new("bytes");
        dir.write("a.bin", &[0u8; 7]);
        dir.write("sub/b.bin", &[0u8; 3]);
        let never = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(folder_bytes(dir.path(), &never), Some(10));
        assert_eq!(folder_bytes(&dir.path().join("missing"), &never), Some(0));
        assert_eq!(folder_bytes(dir.path(), &std::sync::atomic::AtomicBool::new(true)), None);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peebify-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_absolute_paths_can_widen_the_asset_scope() {
        assert!(asset_scope_accepts(Path::new(r"C:\Users\me\Videos\Peebify")));
        assert!(asset_scope_accepts(Path::new(r"\\?\D:\wall.png")));
        assert!(asset_scope_accepts(Path::new(r"\\server\share\caps")));
        assert!(!asset_scope_accepts(Path::new("")));
        assert!(!asset_scope_accepts(Path::new("wall.png")));
        assert!(!asset_scope_accepts(Path::new(r"\Users\me")));
        assert!(!asset_scope_accepts(Path::new("C:caps")));
    }

    #[test]
    fn the_static_asset_scope_is_only_the_launchers_own_folders() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let allow: Vec<&str> = config["app"]["security"]["assetProtocol"]["scope"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(allow.contains(&"$DATA/Peebify Launcher/wallpaper-cache/**"));
        for pattern in allow {
            assert!(
                pattern.starts_with("$DATA/Peebify Launcher/") || pattern == "$VIDEO/Peebify/**",
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_store_whose_swap_never_finished_loads_from_its_tmp() {
        let dir = scratch("recover");
        let path = dir.join("profiles.json");
        std::fs::write(tmp_sibling(&path), b"{\"a\":1}").unwrap();

        let recovered = read_json_store(&path);
        std::fs::write(tmp_sibling(&path), b"{\"a\":").unwrap();
        let partial = read_json_store(&path);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(recovered, Ok(Some(serde_json::json!({ "a": 1 }))));
        assert_eq!(partial, Ok(None));
    }

    #[test]
    fn json_stores_separate_missing_corrupt_and_unreadable() {
        let dir = std::env::temp_dir().join(format!("peebify-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");

        let missing = read_json_store(&path);
        std::fs::write(&path, b"{\"a\":1}").unwrap();
        let good = read_json_store(&path);
        std::fs::write(&path, b"{\"a\":").unwrap();
        let corrupt = read_json_store(&path);
        let set_aside = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("store.json.corrupt-"));
        let original_left = path.exists();
        std::fs::create_dir_all(&path).unwrap();
        let unreadable = read_json_store(&path);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(missing, Ok(None));
        assert_eq!(good, Ok(Some(serde_json::json!({ "a": 1 }))));
        assert_eq!(corrupt, Ok(None));
        assert!(set_aside);
        assert!(!original_left);
        assert!(unreadable.is_err());
    }

    #[test]
    fn a_store_with_invalid_utf8_is_set_aside_and_starts_empty() {
        let dir = scratch("store-utf8");
        let path = dir.join("library.json");
        std::fs::write(&path, b"{\"a\":\"\xff\xfe\"}").unwrap();

        let torn = read_json_store(&path);
        let set_aside = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("library.json.corrupt-"));
        let original_left = path.exists();
        std::fs::write(tmp_sibling(&path), b"{\"a\":\"\xff\"}").unwrap();
        let torn_tmp = read_json_store(&path);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(torn, Ok(None));
        assert!(set_aside);
        assert!(!original_left);
        assert_eq!(torn_tmp, Ok(None));
    }

    #[test]
    fn folder_names_can_never_be_a_path() {
        for name in ["../escape", r"..\escape", "a/b", r"a\b", "C:/x"] {
            let safe = sanitize_folder_name(name);
            assert!(
                !safe.contains('/') && !safe.contains('\\') && !safe.contains(':'),
                "{name} sanitized to {safe}, which still has a separator"
            );
        }
    }

    #[test]
    fn links_inside_an_extracted_tree_are_found() {
        let dir = std::env::temp_dir().join(format!("peebify-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        std::fs::write(dir.join("a").join("b").join("f.txt"), "x").unwrap();
        assert_eq!(find_link(&dir), None);
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.join("a").join("j"))
            .arg(std::env::temp_dir())
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert_eq!(find_link(&dir), Some(dir.join("a").join("j")));
            std::fs::remove_dir(dir.join("a").join("j")).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_keys_agree_for_every_spelling_of_one_file() {
        let key = manifest_key("Client/Content/Paks/pakchunk0.pak");
        assert_eq!(manifest_key(r"/client\Content/./Paks/PAKCHUNK0.pak"), key);
        assert_eq!(manifest_key("CLIENT//content/paks/pakchunk0.pak"), key);
        assert_ne!(manifest_key("Client/Content/Paks/pakchunk1.pak"), key);
    }

    #[test]
    fn a_writable_folder_passes_both_probes_and_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("peebify-probe-{}", uuid::Uuid::new_v4()));
        let nested = dir.join("a").join("b");

        let probed = probe_writable(&nested);
        let denied = dir_denies_writes(&nested);
        let leftovers = std::fs::read_dir(&nested).map(|rd| rd.count()).unwrap_or(99);
        let missing_denied = dir_denies_writes(&dir.join("missing"));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(probed.is_ok());
        assert!(!denied);
        assert_eq!(leftovers, 0);
        assert!(!missing_denied);
    }

    #[test]
    fn versions_compare_by_number_and_match_with_missing_parts_as_zero() {
        assert!(is_version_newer("2.4.1", "2.4.0"));
        assert!(is_version_newer("2.10", "2.9.9"));
        assert!(!is_version_newer("2.4", "2.4.0"));
        assert!(is_version_newer("1.0.5", "1.0.b"), "a part that is not a number counts as zero");
        assert!(!is_version_newer("1.0.b", "1.0.a"));
        assert!(!is_version_newer("", "1.0") && !is_version_newer("1.0", ""));

        assert!(same_version(" 1.2 ", "1.2.0"));
        assert!(same_version("1.2.0B", "1.2.0b"));
        assert!(!same_version("1.2.a", "1.2.0"), "text is not zero when matching");
        assert!(!same_version("1.2", "1.3"));
    }

    #[test]
    fn ini_values_are_found_by_section_and_key_without_case() {
        let text = "top = 0\n[General]\n; game_version = 9\ngame_version = 5.1.0\n[ Other ]\nGame_Version=\n";
        assert_eq!(ini_value(text, "general", "GAME_VERSION").as_deref(), Some("5.1.0"));
        assert_eq!(ini_value(text, "other", "game_version").as_deref(), Some(""));
        assert_eq!(ini_value(text, "", "top").as_deref(), Some("0"));
        assert_eq!(ini_value(text, "missing", "top"), None);
        assert_eq!(ini_entries(text).count(), 3);
    }

    #[test]
    fn free_space_is_measured_at_the_nearest_folder_that_exists() {
        let dir = scratch("free");
        let missing = dir.join("not").join("yet").join("created");

        let probe = existing_probe_dir(&missing).expect("an existing ancestor");
        assert_eq!(
            Path::new(probe.trim_end_matches('\\')),
            dir.as_path(),
            "probed {probe}"
        );
        assert!(probe.ends_with('\\'));
        assert!(free_bytes_for(&missing).is_some());
        assert!(existing_probe_dir(Path::new("")).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    fn dacl_is_protected(dir: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::{
            GetFileSecurityW, GetSecurityDescriptorControl, DACL_SECURITY_INFORMATION,
            SE_DACL_PROTECTED,
        };

        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut needed = 0u32;
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let read = unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        assert_ne!(read, 0, "could not read the DACL of {}", dir.display());
        let (mut control, mut revision) = (0u16, 0u32);
        let ok = unsafe {
            GetSecurityDescriptorControl(buf.as_mut_ptr().cast(), &mut control, &mut revision)
        };
        assert_ne!(ok, 0);
        control & SE_DACL_PROTECTED != 0
    }

    #[test]
    fn a_restricted_root_stays_fully_usable_by_this_account() {
        let dir = scratch("acl");
        let root = dir.join("Peebify Games");

        assert_eq!(create_restricted_dir(&root), Ok(true));
        #[cfg(windows)]
        assert!(dacl_is_protected(&root), "the drive root's access was inherited");
        let game = root.join("Some Game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("data.bin"), b"x").unwrap();
        assert_eq!(std::fs::read(game.join("data.bin")).unwrap(), b"x");
        assert_eq!(create_restricted_dir(&root), Ok(false));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_preview_probe_never_leaves_a_folder_behind() {
        let dir = scratch("preview");
        let target = dir.join("Peebify Games").join("Game");
        assert_eq!(preview_writable(&target), Ok(()));
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(preview_writable(&dir), Ok(()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_fresh_install_creates_only_a_missing_shared_root() {
        let dir = scratch("shared-root");
        let root = dir.join("Peebify Games");

        assert!(!create_shared_root_for(&dir.join("Elsewhere").join("Game"), &root));
        assert!(!root.exists(), "a folder outside the shared root created it");

        let install = dir.join("peebify games").join("Game");
        assert!(create_shared_root_for(&install, &root));
        assert!(root.is_dir());
        assert!(!install.exists(), "the game folder is left to the install");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_root_the_user_already_has_keeps_its_access() {
        let dir = scratch("shared-root-kept");
        let root = dir.join("Peebify Games");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();

        assert!(!create_shared_root_for(&root.join("Game"), &root));
        #[cfg(windows)]
        assert!(!dacl_is_protected(&root), "a folder Peebify did not create was tightened");
        assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"mine");

        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn paths_compare_without_case_slashes_or_a_trailing_separator() {
        assert!(same_path(r"D:\Games\WW\", "d:/games/ww"));
        assert!(!same_path(r"D:\Games\WW", r"E:\Games\WW"));
    }
}

// ------------ Native Dialogs ------------
// File and folder pickers, plus opening a folder or a web link with Windows.
pub mod dialog {
    use std::path::Path;

    use tauri::{AppHandle, Manager};
    use tauri_plugin_dialog::DialogExt;

    #[derive(Default)]
    pub struct Pick {
        pub title: String,
        pub default_path: Option<String>,
        pub filters: &'static [(&'static str, &'static [&'static str])],
        pub owner: &'static str,
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Folder,
        File,
        Files,
    }

    async fn pick(app: &AppHandle, p: Pick, kind: Kind) -> Result<Vec<String>, String> {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let mut builder = app.dialog().file();
            let owner = if p.owner.is_empty() { "main" } else { p.owner };
            if let Some(win) = app.get_webview_window(owner).filter(|w| {
                w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(true)
            }) {
                builder = builder.set_parent(&win);
            }
            if !p.title.is_empty() {
                builder = builder.set_title(&p.title);
            }
            if let Some(dp) = p.default_path.as_deref().filter(|s| !s.is_empty()) {
                builder = builder.set_directory(dp);
            }
            for (name, extensions) in p.filters {
                builder = builder.add_filter(*name, extensions);
            }
            let picked = match kind {
                Kind::Folder => builder.blocking_pick_folder().map(|f| vec![f]),
                Kind::File => builder.blocking_pick_file().map(|f| vec![f]),
                Kind::Files => builder.blocking_pick_files(),
            };
            picked.unwrap_or_default().into_iter().map(|f| f.to_string()).collect()
        })
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn pick_folder(app: &AppHandle, p: Pick) -> Result<Option<String>, String> {
        Ok(pick(app, p, Kind::Folder).await?.pop())
    }

    pub async fn pick_file(app: &AppHandle, p: Pick) -> Result<Option<String>, String> {
        Ok(pick(app, p, Kind::File).await?.pop())
    }

    pub async fn pick_files(app: &AppHandle, p: Pick) -> Result<Vec<String>, String> {
        pick(app, p, Kind::Files).await
    }

    pub fn open_folder(path: &Path) -> Result<(), String> {
        let resolved = std::fs::canonicalize(path)
            .map_err(|_| format!("Folder not found: {}", path.display()))?;
        if !resolved.is_dir() {
            return Err("Only folders can be opened.".to_string());
        }
        open::that_detached(super::plain_path(&resolved)).map_err(|e| e.to_string())
    }

    pub fn open_url(raw: &str) -> Result<(), String> {
        let url = url::Url::parse(raw.trim()).map_err(|_| "Invalid URL.".to_string())?;
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(format!("Refusing to open a {other}: link.")),
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err("Refusing to open a URL with no host.".to_string());
        }
        open::that_detached(url.to_string()).map_err(|e| e.to_string())
    }
}
