// ------------ Process And File Helpers ------------
// Finding processes by name, checking they are alive, and "pinned" libraries held open so they cannot be swapped
// between the checksum and the load. Also a log file opener that refuses links and redirected folders.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HMODULE, HWND, INVALID_HANDLE_VALUE, LPARAM, NTSTATUS,
};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_ALG_HANDLE, BCRYPT_SHA256_ALG_HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, VOLUME_NAME_DOS, VOLUME_NAME_GUID, VOLUME_NAME_NT,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LoadLibraryW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};

use super::wide;

const STILL_ACTIVE: u32 = 259;

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    (unsafe { ProcessIdToSessionId(pid, &mut session) } != 0).then_some(session)
}

fn in_own_session(own: Option<u32>, other: Option<u32>) -> bool {
    match own {
        Some(own) => other == Some(own),
        None => true,
    }
}

pub fn list_processes() -> Vec<(String, u32, u32)> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            found.push((name, entry.th32ProcessID, entry.th32ParentProcessID));
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
    found
}

pub fn find_processes(name: &str) -> Vec<u32> {
    let own_session = session_of(std::process::id());
    list_processes()
        .into_iter()
        .filter(|(exe, pid, _)| {
            exe.eq_ignore_ascii_case(name) && in_own_session(own_session, session_of(*pid))
        })
        .map(|(_, pid, _)| pid)
        .collect()
}

pub fn visible_windows_of(pid: u32) -> Vec<HWND> {
    struct Search {
        pid: u32,
        found: Vec<HWND>,
    }

    unsafe extern "system" fn collect(window: HWND, param: LPARAM) -> BOOL {
        let search = &mut *(param as *mut Search);
        let mut owner = 0u32;
        GetWindowThreadProcessId(window, &mut owner);
        if owner == search.pid && IsWindowVisible(window) != 0 {
            search.found.push(window);
        }
        1
    }

    let mut search = Search {
        pid,
        found: Vec::new(),
    };
    unsafe { EnumWindows(Some(collect), &mut search as *mut Search as LPARAM) };
    search.found
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn is_running(process: HANDLE) -> bool {
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE }
}

pub fn process_alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let alive = is_running(handle);
    unsafe { CloseHandle(handle) };
    alive
}

pub struct PinnedLibrary {
    file: File,
    path: PathBuf,
}

impl PinnedLibrary {
    pub fn open(path: &Path) -> std::io::Result<PinnedLibrary> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("it is not a file"));
        }
        let path = final_path(&file)?;
        Ok(PinnedLibrary { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sha256(&self) -> std::io::Result<[u8; 32]> {
        let mut file = &self.file;
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let length = u32::try_from(bytes.len())
            .map_err(|_| std::io::Error::other("it is too large to check"))?;
        let hash = bcrypt_hash()?;
        let mut digest = [0u8; 32];
        let status = unsafe {
            hash(
                BCRYPT_SHA256_ALG_HANDLE,
                std::ptr::null(),
                0,
                bytes.as_ptr(),
                length,
                digest.as_mut_ptr(),
                digest.len() as u32,
            )
        };
        if status < 0 {
            return Err(std::io::Error::other(format!(
                "Windows could not hash it (status {status:#010x})"
            )));
        }
        Ok(digest)
    }

    pub fn load(&self) -> std::io::Result<HMODULE> {
        let path: Vec<u16> = self
            .path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module = unsafe { LoadLibraryW(path.as_ptr()) };
        if module.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(module)
    }
}

fn handle_info(file: &File) -> std::io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info)
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    };
    text.trim_end_matches('\\').to_lowercase()
}

pub fn create_log_file(path: &Path) -> std::io::Result<File> {
    let redirected = || std::io::Error::other("its folder is a link or does not match its path");
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("it has no file name"))?;
    let path = std::path::absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("it has no folder"))?;

    let folder = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent)?;
    let folder_info = handle_info(&folder)?;
    let folder_path = final_path(&folder)?;
    if folder_info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || path_key(&folder_path) != path_key(parent)
    {
        return Err(redirected());
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder_path.join(name))?;
    let info = handle_info(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.nNumberOfLinks != 1
        || path_key(&final_path(&file)?) != path_key(&folder_path.join(name))
    {
        return Err(std::io::Error::other("it is a link to another file"));
    }
    file.set_len(0)?;
    drop(folder);
    Ok(file)
}

type BCryptHashFn = unsafe extern "system" fn(
    BCRYPT_ALG_HANDLE,
    *const u8,
    u32,
    *const u8,
    u32,
    *mut u8,
    u32,
) -> NTSTATUS;
type RawExport = unsafe extern "system" fn() -> isize;

fn bcrypt_hash() -> std::io::Result<BCryptHashFn> {
    let name = wide("bcrypt.dll");
    let module = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let export = unsafe { GetProcAddress(module, c"BCryptHash".as_ptr() as *const u8) }
        .ok_or_else(|| std::io::Error::other("bcrypt.dll has no BCryptHash"))?;
    Ok(unsafe { std::mem::transmute::<RawExport, BCryptHashFn>(export) })
}

fn final_path(file: &File) -> std::io::Result<PathBuf> {
    let path = match final_name(file, VOLUME_NAME_DOS) {
        Ok(path) => without_verbatim_prefix(&path),
        Err(error) => match final_name(file, VOLUME_NAME_GUID) {
            Ok(path) => path,
            Err(_) => {
                let device = final_name(file, VOLUME_NAME_NT).map_err(|_| error)?;
                r"\\?\GLOBALROOT".encode_utf16().chain(device).collect()
            }
        },
    };
    Ok(PathBuf::from(OsString::from_wide(&path)))
}

fn final_name(file: &File, volume: u32) -> std::io::Result<Vec<u16>> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut buffer = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | volume,
            )
        } as usize;
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(buffer);
        }
        buffer.resize(length, 0);
    }
}

fn without_verbatim_prefix(path: &[u16]) -> Vec<u16> {
    const MAX_PATH: usize = 260;
    let utf16 = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    let (verbatim, unc) = (utf16(r"\\?\"), utf16(r"\\?\UNC\"));
    let short = if path.starts_with(&unc) {
        [&utf16(r"\\")[..], &path[unc.len()..]].concat()
    } else if path.starts_with(&verbatim) && path.get(verbatim.len() + 1) == Some(&(b':' as u16)) {
        path[verbatim.len()..].to_vec()
    } else {
        return path.to_vec();
    };
    if short.len() < MAX_PATH {
        short
    } else {
        path.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("peebify-helpers-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn pinned_library_cannot_be_changed_or_moved() {
        let folder = scratch("pinned");
        let path = folder.join("library.dll");
        std::fs::write(&path, b"abc").unwrap();

        let pinned = PinnedLibrary::open(&path).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, folder.join("moved.dll")).is_err());
        let mut renamed = folder.clone().into_os_string();
        renamed.push("-renamed");
        assert!(std::fs::rename(&folder, &renamed).is_err());

        assert!(pinned.path().is_absolute());
        assert!(pinned.path().ends_with("library.dll"));
        assert_eq!(
            pinned.sha256().unwrap(),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert_eq!(pinned.sha256().unwrap(), pinned.sha256().unwrap());

        drop(pinned);
        std::fs::rename(&path, folder.join("moved.dll")).unwrap();
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn pinned_library_loads_while_held() {
        use windows_sys::Win32::Foundation::FreeLibrary;

        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let folder = scratch("load");
        let path = folder.join("pinned-version.dll");
        std::fs::copy(system.join("System32").join("version.dll"), &path).unwrap();

        let pinned = PinnedLibrary::open(&path).unwrap();
        let module = pinned.load().unwrap();
        let export =
            unsafe { GetProcAddress(module, c"GetFileVersionInfoSizeW".as_ptr() as *const u8) };
        assert!(export.is_some());
        unsafe { FreeLibrary(module) };
        drop(pinned);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn fallback_path_forms_load() {
        use windows_sys::Win32::Foundation::FreeLibrary;

        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let folder = scratch("volume");
        for (index, volume) in [VOLUME_NAME_GUID, VOLUME_NAME_NT].into_iter().enumerate() {
            let path = folder.join(format!("pinned-version-{index}.dll"));
            std::fs::copy(system.join("System32").join("version.dll"), &path).unwrap();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&path)
                .unwrap();
            let name = final_name(&file, volume).unwrap();
            let name = if volume == VOLUME_NAME_NT {
                [utf16(r"\\?\GLOBALROOT"), name].concat()
            } else {
                assert!(name.starts_with(&utf16(r"\\?\Volume{")));
                name
            };
            let pinned = PinnedLibrary {
                file,
                path: PathBuf::from(OsString::from_wide(&name)),
            };
            let module = pinned.load().unwrap();
            unsafe { FreeLibrary(module) };
            drop(pinned);
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn links_resolve_to_their_target_and_folders_are_refused() {
        let folder = scratch("link");
        let target = folder.join("real.dll");
        std::fs::write(&target, b"abc").unwrap();
        let link = folder.join("link.dll");
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            let pinned = PinnedLibrary::open(&link).unwrap();
            assert!(pinned.path().ends_with("real.dll"));
            assert!(std::fs::rename(&target, folder.join("moved.dll")).is_err());
        }
        assert!(PinnedLibrary::open(&folder).is_err());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn log_file_is_created_and_truncated_in_a_plain_folder() {
        use std::io::Write;
        let folder = scratch("log-plain");
        let path = folder.join("mod-loader.log");
        std::fs::write(&path, b"old contents").unwrap();
        let mut file = create_log_file(&path).unwrap();
        writeln!(file, "new").unwrap();
        drop(file);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let slashed = PathBuf::from(path.to_string_lossy().replace('\\', "/"));
        assert!(create_log_file(&slashed).is_ok());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn log_file_refuses_a_hard_link_and_leaves_its_target_alone() {
        let folder = scratch("log-link");
        let target = folder.join("protected.dll");
        std::fs::write(&target, b"keep me").unwrap();
        let path = folder.join("mod-loader.log");
        std::fs::hard_link(&target, &path).unwrap();
        assert!(create_log_file(&path).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn log_file_refuses_a_junctioned_folder() {
        let folder = scratch("log-junction");
        let real = folder.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("mod-loader.log"), b"keep me").unwrap();
        let junction = folder.join("logs");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&real)
            .output()
            .is_ok_and(|out| out.status.success());
        if made {
            assert!(create_log_file(&junction.join("mod-loader.log")).is_err());
            assert_eq!(std::fs::read(real.join("mod-loader.log")).unwrap(), b"keep me");
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn session_filter_keeps_only_matching_sessions() {
        assert!(in_own_session(Some(1), Some(1)));
        assert!(!in_own_session(Some(1), Some(0)));
        assert!(!in_own_session(Some(1), None));
        assert!(in_own_session(None, Some(0)));
        assert!(session_of(std::process::id()).is_some());
    }

    #[test]
    fn verbatim_prefixes_are_dropped_when_the_path_fits() {
        assert_eq!(
            without_verbatim_prefix(&utf16(r"\\?\C:\Peebify\a.dll")),
            utf16(r"C:\Peebify\a.dll")
        );
        assert_eq!(
            without_verbatim_prefix(&utf16(r"\\?\UNC\server\share\a.dll")),
            utf16(r"\\server\share\a.dll")
        );
        assert_eq!(
            without_verbatim_prefix(&utf16(r"\\?\Volume{1234}\a.dll")),
            utf16(r"\\?\Volume{1234}\a.dll")
        );
        assert_eq!(
            without_verbatim_prefix(&utf16(r"C:\a.dll")),
            utf16(r"C:\a.dll")
        );
        let long = format!(r"\\?\C:\{}\a.dll", "x".repeat(300));
        assert_eq!(without_verbatim_prefix(&utf16(&long)), utf16(&long));
    }
}
