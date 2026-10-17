// ------------ File System Helpers ------------
// Error classification, atomic replaces, safe manifest paths, positional reads and writes, and MD5/CRC32/CRC64 hashing.
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use md5::{Digest, Md5};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Network,
    DiskFull,
    Validation,
    Cancelled,
    Locked,
    AccessDenied,
    DeviceGone,
    FileTooLarge,
    ShareLost,
    Other,
}

pub const SHARE_LOST_ATTEMPTS: u32 = 3;

pub fn classify(message: &str) -> FailureKind {
    let msg = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| msg.contains(n));

    if has(&[
        "cancelled",
        "canceled",
        "aborted by user",
        "repair aborted",
        "download aborted",
    ]) {
        return FailureKind::Cancelled;
    }
    let code = os_error_code(&msg);
    if matches!(code, Some(112) | Some(39)) || has(&["there is not enough space"]) {
        return FailureKind::DiskFull;
    }
    if matches!(code, Some(21) | Some(55) | Some(483) | Some(1167)) {
        return FailureKind::DeviceGone;
    }
    if code == Some(223) {
        return FailureKind::FileTooLarge;
    }
    if msg.contains("could not replace") && has(&["[os:32]", "[os:33]", "[os:5]"]) {
        return FailureKind::Locked;
    }
    if has(&["[os:5]", "(os error 5)", "access is denied"]) {
        return FailureKind::AccessDenied;
    }
    if has(&["validation failed", "failed verification"]) {
        return FailureKind::Validation;
    }
    if has(&[
        "request error",
        "stream error",
        "error sending request",
        "decoding response body",
        "stalled",
        "timed out",
        "timeout",
        "dns error",
        "getaddrinfo",
        "socket hang up",
        "econnreset",
        "connection refused",
        "connection reset",
        "connection closed",
        "connection aborted",
    ]) {
        return FailureKind::Network;
    }
    if matches!(code, Some(59) | Some(64)) {
        return FailureKind::ShareLost;
    }
    FailureKind::Other
}

pub fn os_error_code(message: &str) -> Option<i32> {
    ["(os error ", "[os:"].iter().find_map(|marker| {
        let start = message.rfind(marker)? + marker.len();
        let digits: String = message[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

pub fn drive_failure_message(
    kind: FailureKind,
    dir: &Path,
    file: &Path,
    file_size: u64,
) -> Option<String> {
    match kind {
        FailureKind::DeviceGone => Some(format!(
            "The drive holding {} is no longer available. Reconnect it and try again.",
            dir.display()
        )),
        FailureKind::FileTooLarge => Some(fat_limit_message(file, file_size).unwrap_or_else(|| {
            format!(
                "{} is too large for the drive it is on. Pick an NTFS or exFAT drive.",
                file.display()
            )
        })),
        FailureKind::ShareLost => Some(format!(
            "The network drive holding {} stopped responding. Check the connection and try again.",
            dir.display()
        )),
        _ => None,
    }
}

pub fn fmt_io(context: &str, e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("{context}: {e} [os:{code}]"),
        None => format!("{context}: {e}"),
    }
}

const REPLACE_ATTEMPTS: u32 = 5;
const REPLACE_RETRY_BASE_MS: u64 = 200;

pub fn finalize_replace(tmp_path: &Path, dest: &Path) -> Result<(), String> {
    replace_retrying(tmp_path, dest).map_err(|e| {
        fmt_io(
            &format!(
                "Could not replace {}. Is the game running, or is a file locked by antivirus?",
                dest.display()
            ),
            &e,
        )
    })
}

pub fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

pub fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fmt_io(&format!("Could not create {}", parent.display()), &e))?;
    }
    let tmp = tmp_sibling(dest);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(fmt_io(&format!("Could not write {}", tmp.display()), &e));
    }
    replace_retrying(&tmp, dest).map_err(|e| {
        if dest.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
        fmt_io(&format!("Could not save {}", dest.display()), &e)
    })
}

pub fn replace_retrying(tmp_path: &Path, dest: &Path) -> std::io::Result<()> {
    let mut attempt = 0u32;
    loop {
        match try_replace(tmp_path, dest) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                let retryable = matches!(e.raw_os_error(), Some(5) | Some(32));
                if attempt >= REPLACE_ATTEMPTS || !retryable {
                    return Err(e);
                }
                log::warn!(
                    "Replacing {} is blocked ({e}), retry {attempt}/{REPLACE_ATTEMPTS}",
                    dest.display()
                );
                std::thread::sleep(Duration::from_millis(
                    REPLACE_RETRY_BASE_MS * u64::from(attempt),
                ));
            }
        }
    }
}

pub fn retry_while_locked<T>(
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut attempt = 0u32;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) => {
                attempt += 1;
                let retryable = matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33));
                if attempt >= REPLACE_ATTEMPTS || !retryable {
                    return Err(e);
                }
                log::warn!("A file is locked ({e}), retry {attempt}/{REPLACE_ATTEMPTS}");
                std::thread::sleep(Duration::from_millis(
                    REPLACE_RETRY_BASE_MS * u64::from(attempt),
                ));
            }
        }
    }
}

pub fn is_access_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5)
}

pub fn not_writable_message(dir: &Path) -> String {
    format!(
        "Peebify cannot write to {}. Pick a folder you own, or copy the game into one and use Locate.",
        dir.display()
    )
}

pub const FAT_MAX_FILE_BYTES: u64 = u32::MAX as u64;

fn is_fat_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "FAT" | "FAT12" | "FAT16" | "FAT32"
    )
}

#[cfg(windows)]
pub fn volume_fs_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    let existing = path.ancestors().find(|p| p.exists())?;
    let wide: Vec<u16> = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut root = [0u16; 1024];
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let mut fs_name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
    Some(String::from_utf16_lossy(&fs_name[..len]))
}

#[cfg(not(windows))]
pub fn volume_fs_name(_path: &Path) -> Option<String> {
    None
}

pub fn fat_limit_message(path: &Path, file_size: u64) -> Option<String> {
    if file_size <= FAT_MAX_FILE_BYTES {
        return None;
    }
    let name = volume_fs_name(path)?;
    is_fat_name(&name).then(|| {
        format!(
            "This drive is formatted as {name}, which cannot store files larger than 4 GB. Pick an NTFS or exFAT drive."
        )
    })
}

pub fn explain_disk_full(path: &Path, file_size: u64, message: String) -> String {
    if classify(&message) != FailureKind::DiskFull {
        return message;
    }
    match fat_limit_message(path, file_size) {
        Some(hint) => format!("{hint} ({message})"),
        None => message,
    }
}

#[cfg(windows)]
pub fn write_all_at(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let written = file.seek_write(buf, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "wrote zero bytes",
            ));
        }
        buf = &buf[written..];
        offset += written as u64;
    }
    Ok(())
}

#[cfg(windows)]
pub fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let read = file.seek_read(buf, offset)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file ended early",
            ));
        }
        buf = &mut buf[read..];
        offset += read as u64;
    }
    Ok(())
}

#[cfg(unix)]
pub fn write_all_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(unix)]
pub fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

fn try_replace(tmp_path: &Path, dest: &Path) -> std::io::Result<()> {
    let first = match std::fs::rename(tmp_path, dest) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let Ok(meta) = std::fs::metadata(dest) else {
        return Err(first);
    };
    let mut perms = meta.permissions();
    if !perms.readonly() {
        return Err(first);
    }
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    if std::fs::set_permissions(dest, perms).is_err() {
        return Err(first);
    }
    std::fs::rename(tmp_path, dest)
}

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let normalised = rel.replace('\\', "/");
    let trimmed = normalised.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err("refusing empty path from manifest".to_string());
    }

    let mut out = root.to_path_buf();
    let mut pushed = 0usize;
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => {
                let name = part.to_string_lossy();
                if name.contains(':') {
                    return Err(format!(
                        "refusing manifest path '{rel}': ':' is not allowed in a name"
                    ));
                }
                if let Some(problem) = unsafe_name_problem(&name) {
                    return Err(format!("refusing manifest path '{rel}': {problem}"));
                }
                out.push(part);
                pushed += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("refusing manifest path '{rel}': contains '..'"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "refusing manifest path '{rel}': not a relative path"
                ));
            }
        }
    }

    if pushed == 0 {
        return Err(format!(
            "refusing manifest path '{rel}': it names no file under the install"
        ));
    }

    Ok(out)
}

const RESERVED_DEVICE_NAMES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

fn unsafe_name_problem(name: &str) -> Option<&'static str> {
    if name.trim_end_matches(['.', ' ']).is_empty() {
        return Some("a name made only of dots or spaces");
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Some("a name with characters Windows does not allow");
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    RESERVED_DEVICE_NAMES
        .contains(&stem.as_str())
        .then_some("a reserved device name")
}

pub const STREAM_CHUNK_SIZE: usize = 1 << 20;
thread_local! {
    static HASH_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
pub const CANCELLED_MSG: &str = "Operation cancelled by user.";

pub fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub const CRC64_PREFIX: &str = "crc64:";
static CRC64_XZ: crc::Crc<u64> = crc::Crc::<u64>::new(&crc::CRC_64_XZ);

pub const CRC32_PREFIX: &str = "crc32:";

pub enum ContentHasher {
    Md5(Md5),
    Crc64(crc::Digest<'static, u64>),
    Crc32(crc32fast::Hasher),
}

impl ContentHasher {
    pub fn for_expected(expected: &str) -> Self {
        if expected.starts_with(CRC64_PREFIX) {
            Self::Crc64(CRC64_XZ.digest())
        } else if expected.starts_with(CRC32_PREFIX) {
            Self::Crc32(crc32fast::Hasher::new())
        } else {
            Self::Md5(Md5::new())
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Md5(hasher) => hasher.update(bytes),
            Self::Crc64(digest) => digest.update(bytes),
            Self::Crc32(hasher) => hasher.update(bytes),
        }
    }

    pub fn finish(self) -> String {
        match self {
            Self::Md5(hasher) => hex::encode(hasher.finalize()),
            Self::Crc64(digest) => format!("{CRC64_PREFIX}{}", digest.finalize()),
            Self::Crc32(hasher) => format!("{CRC32_PREFIX}{}", hasher.finalize()),
        }
    }
}

pub fn checksum_matches(expected: &str, got: &str) -> bool {
    got.eq_ignore_ascii_case(expected)
}

pub fn md5_file(
    path: &Path,
    should_cancel: &mut dyn FnMut() -> bool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<String, String> {
    checksum_file(path, "", should_cancel, on_progress)
}

pub fn checksum_file(
    path: &Path,
    expected: &str,
    should_cancel: &mut dyn FnMut() -> bool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| fmt_io(&format!("Could not read {}", path.display()), &e))?;
    let mut hasher = ContentHasher::for_expected(expected);
    let mut hash_with = |buf: &mut [u8]| -> Result<(), String> {
        loop {
            if should_cancel() {
                return Err(CANCELLED_MSG.to_string());
            }
            let read = file
                .read(buf)
                .map_err(|e| fmt_io(&format!("Could not read {}", path.display()), &e))?;
            if read == 0 {
                return Ok(());
            }
            hasher.update(&buf[..read]);
            on_progress(read as u64);
        }
    };
    with_hash_buf(|buf| hash_with(buf))?;
    Ok(hasher.finish())
}

pub fn with_hash_buf<T>(f: impl FnOnce(&mut [u8]) -> T) -> T {
    HASH_BUF.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buf) => {
            if buf.len() < STREAM_CHUNK_SIZE {
                buf.resize(STREAM_CHUNK_SIZE, 0);
            }
            f(&mut buf)
        }
        Err(_) => f(&mut vec![0u8; STREAM_CHUNK_SIZE]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        crate::testing::scratch_dir(tag)
    }

    #[test]
    fn only_fat_family_names_hit_the_four_gigabyte_cap() {
        assert!(is_fat_name("FAT32"));
        assert!(is_fat_name("fat"));
        assert!(is_fat_name("FAT16"));
        assert!(!is_fat_name("exFAT"));
        assert!(!is_fat_name("NTFS"));
        assert!(!is_fat_name("ReFS"));
    }

    #[test]
    fn md5_file_reuses_its_buffer_across_files_and_nested_calls() {
        let dir = scratch("md5");
        let small = dir.join("small.bin");
        let large = dir.join("large.bin");
        let large_bytes: Vec<u8> = (0..STREAM_CHUNK_SIZE * 2 + 7).map(|i| i as u8).collect();
        std::fs::write(&small, b"abc").unwrap();
        std::fs::write(&large, &large_bytes).unwrap();

        let large_md5 = md5_file(&large, &mut || false, &mut |_| {}).unwrap();
        let small_md5 = md5_file(&small, &mut || false, &mut |_| {}).unwrap();
        let mut nested = None;
        let outer = md5_file(&large, &mut || false, &mut |_| {
            if nested.is_none() {
                nested = Some(md5_file(&small, &mut || false, &mut |_| {}).unwrap());
            }
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(large_md5, md5_hex(&large_bytes));
        assert_eq!(small_md5, md5_hex(b"abc"));
        assert_eq!(outer, large_md5);
        assert_eq!(nested.as_deref(), Some(small_md5.as_str()));
    }

    #[test]
    fn positional_reads_return_what_positional_writes_put_down() {
        let dir = scratch("positional");
        let path = dir.join("p.bin");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        write_all_at(&file, b"world", 6).unwrap();
        write_all_at(&file, b"hello ", 0).unwrap();
        let mut back = [0u8; 11];
        read_exact_at(&file, &mut back, 0).unwrap();
        let mut past = [0u8; 4];
        let short = read_exact_at(&file, &mut past, 9);
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(&back, b"hello world");
        assert!(short.is_err());
    }

    #[test]
    fn os_codes_are_read_from_both_spellings() {
        assert_eq!(os_error_code("write error: gone (os error 1167)"), Some(1167));
        assert_eq!(os_error_code("could not save x: denied [os:5]"), Some(5));
        assert_eq!(os_error_code("http 503 for https://a/b"), None);
        assert_eq!(os_error_code("(os error x)"), None);
    }

    #[test]
    fn drive_failures_are_classified_by_code() {
        let gone = fmt_io("Write error", &std::io::Error::from_raw_os_error(1167));
        assert_eq!(classify(&gone), FailureKind::DeviceGone);
        assert_eq!(classify("Write error: x (os error 21)"), FailureKind::DeviceGone);
        assert_eq!(classify("Write error: x (os error 223)"), FailureKind::FileTooLarge);
        assert_eq!(classify("Write error: x (os error 112)"), FailureKind::DiskFull);
        assert_eq!(classify("Write error: x (os error 64)"), FailureKind::ShareLost);
        assert_eq!(
            classify("Stream error: error decoding response body (os error 64)"),
            FailureKind::Network
        );
        let dir = Path::new("E:\\g");
        let file = dir.join("a.pak");
        assert!(drive_failure_message(FailureKind::DeviceGone, dir, &file, 1)
            .is_some_and(|m| m.contains("E:\\g")));
        assert!(drive_failure_message(FailureKind::Other, dir, &file, 1).is_none());
    }

    #[test]
    fn a_failed_replace_keeps_the_destination() {
        let dir = scratch("replace");
        let dest = dir.join("store.json");
        std::fs::write(&dest, b"old").unwrap();

        let missing_tmp = finalize_replace(&dir.join("absent.tmp"), &dest);
        let kept = std::fs::read(&dest).unwrap();

        let tmp = dir.join("store.json.tmp");
        std::fs::write(&tmp, b"new").unwrap();
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dest, perms).unwrap();
        let replaced = finalize_replace(&tmp, &dest);
        let after = std::fs::read(&dest).unwrap();
        let tmp_left = tmp.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(missing_tmp.is_err());
        assert_eq!(kept, b"old");
        assert!(replaced.is_ok());
        assert_eq!(after, b"new");
        assert!(!tmp_left);
    }

    #[test]
    fn atomic_writes_replace_and_leave_no_tmp() {
        let dir = scratch("atomic");
        let dest = dir.join("nested").join("state.json");

        let first = write_atomic(&dest, b"{\"a\":1}");
        let second = write_atomic(&dest, b"{\"a\":2}");
        let body = std::fs::read_to_string(&dest).unwrap();
        let tmp_left = tmp_sibling(&dest).exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(body, "{\"a\":2}");
        assert!(!tmp_left);
    }

    #[test]
    fn small_files_and_other_failures_skip_the_fat_hint() {
        let dir = std::env::temp_dir();
        assert_eq!(fat_limit_message(&dir, FAT_MAX_FILE_BYTES), None);
        let other = "Write error: stream error".to_string();
        assert_eq!(explain_disk_full(&dir, u64::MAX, other.clone()), other);
    }

    #[cfg(windows)]
    #[test]
    fn the_volume_of_an_existing_or_missing_path_is_named() {
        let missing = std::env::temp_dir().join("peebify-missing-volume-probe").join("x.pak");
        let name = volume_fs_name(&missing).unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn joined_manifest_paths_use_one_native_separator() {
        let root = Path::new(if cfg!(windows) { r"C:\game" } else { "/game" });
        let joined = safe_join(root, "Client/Content/Paks/pakchunk0.pak").unwrap();

        let rebuilt = joined.parent().unwrap().join(joined.file_name().unwrap());
        assert_eq!(joined, rebuilt);
        assert_eq!(joined.to_string_lossy(), rebuilt.to_string_lossy());

        if cfg!(windows) {
            assert!(
                !joined.to_string_lossy().contains('/'),
                "a joined path kept a foreign separator: {}",
                joined.display()
            );
        }
    }

    #[test]
    fn backslash_and_slash_manifests_agree() {
        let root = Path::new(if cfg!(windows) { r"C:\game" } else { "/game" });
        assert_eq!(
            safe_join(root, "a/b/c.pak").unwrap(),
            safe_join(root, r"a\b\c.pak").unwrap()
        );
    }

    #[test]
    fn traversal_and_empty_paths_are_refused() {
        let root = Path::new(if cfg!(windows) { r"C:\game" } else { "/game" });
        assert!(safe_join(root, "../escape.pak").is_err());
        assert!(safe_join(root, "a/../../escape.pak").is_err());
        assert!(safe_join(root, "").is_err());
        assert!(safe_join(root, "/").is_err());
        assert!(safe_join(root, ".").is_err());
        assert!(safe_join(root, "./").is_err());
    }

    #[test]
    fn names_windows_would_rewrite_or_treat_as_devices_are_refused() {
        let root = Path::new(r"C:\game");
        assert!(safe_join(root, "a/.. /escape.pak").is_err());
        assert!(safe_join(root, "a/.../b.pak").is_err());
        assert!(safe_join(root, "a/ ./b.pak").is_err());
        assert!(safe_join(root, "a/NUL").is_err());
        assert!(safe_join(root, "a/con.txt").is_err());
        assert!(safe_join(root, "a/b?.pak").is_err());
        assert!(safe_join(root, "a/b\u{1}.pak").is_err());
        assert!(safe_join(root, "Content/console.pak").is_ok());
        assert!(safe_join(root, "Content/.hidden/nul_table.bin").is_ok());
    }

    #[test]
    fn locked_and_denied_failures_are_told_apart() {
        let locked = fmt_io(
            "Could not replace C:\\g\\a.pak. Is the game running, or is a file locked by antivirus?",
            &std::io::Error::from_raw_os_error(32),
        );
        assert_eq!(classify(&format!("Finalize error for a.pak: {locked}")), FailureKind::Locked);
        let av = fmt_io("Could not replace C:\\g\\a.pak", &std::io::Error::from_raw_os_error(5));
        assert_eq!(classify(&av), FailureKind::Locked);
        assert_eq!(
            classify("Write error: Access is denied. (os error 5)"),
            FailureKind::AccessDenied
        );
        assert_eq!(classify("Write error: something (os error 50)"), FailureKind::Other);
        assert_eq!(classify("Download aborted by user."), FailureKind::Cancelled);
    }

    #[test]
    fn a_crc64_expectation_hashes_with_crc64_xz_in_decimal() {
        let mut hasher = ContentHasher::for_expected("crc64:0");
        hasher.update(b"1234");
        hasher.update(b"56789");
        assert_eq!(hasher.finish(), "crc64:11051210869376104954");
    }

    #[test]
    fn a_crc32_expectation_hashes_with_the_zip_crc_in_decimal() {
        let mut hasher = ContentHasher::for_expected("crc32:0");
        hasher.update(b"1234");
        hasher.update(b"56789");
        assert_eq!(hasher.finish(), "crc32:3421780262");
        assert_eq!(ContentHasher::for_expected("crc32:7").finish(), "crc32:0");
    }

    #[test]
    fn an_empty_crc64_input_hashes_to_zero() {
        assert_eq!(ContentHasher::for_expected("crc64:0").finish(), "crc64:0");
    }

    #[test]
    fn a_plain_expectation_still_hashes_with_md5() {
        let mut hasher = ContentHasher::for_expected("900150983cd24fb0d6963f7d28e17f72");
        hasher.update(b"abc");
        let got = hasher.finish();
        assert!(checksum_matches("900150983CD24FB0D6963F7D28E17F72", &got));
    }

    #[test]
    fn checksum_file_follows_the_expected_algorithm() {
        let dir = scratch("checksum");
        let path = dir.join("digits.bin");
        std::fs::write(&path, b"123456789").unwrap();
        let crc = checksum_file(&path, "crc64:1", &mut || false, &mut |_| {}).unwrap();
        let md5 = checksum_file(&path, "", &mut || false, &mut |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(crc, "crc64:11051210869376104954");
        assert_eq!(md5, "25f9e794323b453885f5181f1b624d0b");
    }
}
