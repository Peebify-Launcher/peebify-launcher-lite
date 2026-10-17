// ------------ Windows Helpers ------------
// Windows plumbing for install and uninstall: Start menu and desktop shortcuts, the Installed apps entry, the
// start with Windows Run key, closing running Peebify processes, message boxes, elevation and disk space.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, E_OUTOFMEMORY, HWND, LPARAM, LRESULT,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    VS_FIXEDFILEINFO,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateMutexW, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, INFINITE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Favorites,
    FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Programs, FOLDERID_SavedGames, FOLDERID_Videos,
    DefSubclassProc, IShellLinkW, RemoveWindowSubclass, SHGetKnownFolderPath, SetWindowSubclass,
    ShellExecuteExW, ShellExecuteW, ShellLink, KF_FLAG_DEFAULT, SEE_MASK_NOCLOSEPROCESS,
    SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, EnumWindows, GetWindowThreadProcessId, IsWindowVisible, MessageBoxW,
    PostMessageW, SystemParametersInfoW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING,
    MB_OK, MB_YESNO, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_CLOSE,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_NCDESTROY, WM_PAINT,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

use crate::consts;
use crate::ilog::ilog;

pub fn restrict_dll_search() -> bool {
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) }.is_ok()
}

pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        queried.is_ok() && elevation.TokenIsElevated != 0
    }
}

pub fn system32_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

pub fn create_run_dir(prefix: &str) -> Result<PathBuf, String> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn message_box(title: &str, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), style) }
}

pub fn message_box_error(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONERROR);
}

pub fn message_box_info(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONINFORMATION);
}

pub fn message_box_yes_no(title: &str, text: &str) -> bool {
    message_box(title, text, MB_YESNO | MB_ICONWARNING) == IDYES
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let pw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = pw.to_string().ok();
        CoTaskMemFree(Some(pw.as_ptr() as *const _));
        s.map(PathBuf::from)
    }
}

pub fn desktop_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

pub fn start_menu_programs_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs)
}

pub fn user_known_folders() -> Vec<PathBuf> {
    [
        FOLDERID_Desktop,
        FOLDERID_Documents,
        FOLDERID_Downloads,
        FOLDERID_Pictures,
        FOLDERID_Music,
        FOLDERID_Videos,
        FOLDERID_SavedGames,
        FOLDERID_Favorites,
    ]
    .iter()
    .filter_map(known_folder)
    .collect()
}

unsafe fn set_shortcut_app_id(link: &IShellLinkW) -> windows::core::Result<()> {
    let store: IPropertyStore = link.cast()?;

    let app_id = HSTRING::from(consts::APP_IDENTIFIER);
    let chars = app_id.len() + 1;
    let buffer = CoTaskMemAlloc(chars * std::mem::size_of::<u16>()) as *mut u16;
    if buffer.is_null() {
        return Err(windows::core::Error::from(E_OUTOFMEMORY));
    }
    std::ptr::copy_nonoverlapping(app_id.as_ptr(), buffer, chars);

    let mut value = PROPVARIANT::default();
    let inner = &mut *value.Anonymous.Anonymous;
    inner.vt = VT_LPWSTR;
    inner.Anonymous.pwszVal = PWSTR(buffer);

    store.SetValue(&PKEY_AppUserModel_ID, &value)?;
    store.Commit()
}

pub fn create_shortcut(
    lnk_path: &Path,
    target: &Path,
    working_dir: &Path,
    description: &str,
) -> Result<(), String> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(target.as_os_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(working_dir.as_os_str()))?;
            link.SetDescription(&HSTRING::from(description))?;
            link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
            if let Err(e) = set_shortcut_app_id(&link) {
                ilog!("shortcut: could not set AppUserModelID: {e}");
            }
            let persist: IPersistFile = link.cast()?;
            persist.Save(&HSTRING::from(lnk_path.as_os_str()), true)?;
            Ok(())
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result.map_err(|e| format!("create shortcut {}: {e}", lnk_path.display()))
    }
}

pub fn remove_shortcuts() {
    for dir in [start_menu_programs_dir(), desktop_dir()]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_file(dir.join(consts::SHORTCUT_NAME));
    }
}

pub fn write_uninstall_entry(
    install_dir: &Path,
    version: &str,
    estimated_size_kb: u64,
) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(consts::UNINSTALL_KEY)
        .map_err(|e| format!("create uninstall key: {e}"))?;
    let main_exe = install_dir.join(consts::MAIN_BINARY);
    let uninstaller = install_dir.join(consts::UNINSTALLER_NAME);
    let set = |name: &str, value: String| -> Result<(), String> {
        key.set_value(name, &value)
            .map_err(|e| format!("set {name}: {e}"))
    };
    set("DisplayName", consts::PRODUCT_NAME.into())?;
    set("DisplayVersion", version.into())?;
    set("DisplayIcon", format!("{},0", main_exe.display()))?;
    set("Publisher", consts::PUBLISHER.into())?;
    set("InstallLocation", install_dir.display().to_string())?;
    set(
        "UninstallString",
        format!("\"{}\" /uninstall", uninstaller.display()),
    )?;
    set(
        "QuietUninstallString",
        format!("\"{}\" /uninstall /S", uninstaller.display()),
    )?;
    set(
        "InstallDate",
        chrono::Local::now().format("%Y%m%d").to_string(),
    )?;
    key.set_value(
        "EstimatedSize",
        &(estimated_size_kb.min(u32::MAX as u64) as u32),
    )
    .map_err(|e| format!("set EstimatedSize: {e}"))?;
    key.set_value("NoModify", &1u32)
        .map_err(|e| format!("set NoModify: {e}"))?;
    key.set_value("NoRepair", &1u32)
        .map_err(|e| format!("set NoRepair: {e}"))?;
    Ok(())
}

pub fn delete_uninstall_entry() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(consts::UNINSTALL_KEY);
}

pub fn delete_app_user_model_id() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = format!(r"Software\Classes\AppUserModelId\{}", consts::APP_IDENTIFIER);
    match hkcu.delete_subkey_all(&key) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            ilog!("uninstall: could not remove the notification registration ({e})");
        }
        _ => {}
    }
}

pub fn registered_install_location() -> Option<PathBuf> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(consts::UNINSTALL_KEY, KEY_READ)
        .ok()?;
    let loc: String = key.get_value("InstallLocation").ok()?;
    let path = PathBuf::from(loc);
    path.join(consts::MAIN_BINARY).exists().then_some(path)
}

pub fn fix_run_key(new_exe: &Path) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(consts::RUN_KEY, KEY_READ | KEY_SET_VALUE) else {
        return;
    };
    let Ok(existing): Result<String, _> = key.get_value(consts::RUN_VALUE) else {
        return;
    };
    let new_cmd = format!("\"{}\" --from-boot", new_exe.display());
    if existing != new_cmd {
        let _ = key.set_value(consts::RUN_VALUE, &new_cmd);
    }
}

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session) }.ok()?;
    Some(session)
}

fn in_own_session(own: Option<u32>, other: Option<u32>) -> bool {
    match own {
        Some(own) => other == Some(own),
        None => true,
    }
}

pub fn pids_by_exe_name(exe_name: &str) -> Vec<u32> {
    let own_session = session_of(std::process::id());
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let me = std::process::id();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                if entry.th32ProcessID != me
                    && name.eq_ignore_ascii_case(exe_name)
                    && in_own_session(own_session, session_of(entry.th32ProcessID))
                {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let waited = WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128) as u32);
        let _ = CloseHandle(handle);
        waited == WAIT_OBJECT_0
    }
}

fn post_close_to_windows(pids: &[u32]) {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let pids = unsafe { &*(lparam.0 as *const Vec<u32>) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pids.contains(&pid) {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        true.into()
    }
    let owned: Vec<u32> = pids.to_vec();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&owned as *const Vec<u32> as isize));
    }
}

fn visible_windows_of(pid: u32) -> Vec<HWND> {
    struct Search {
        pid: u32,
        found: Vec<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == search.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.found.push(hwnd);
        }
        true.into()
    }

    let mut search = Search {
        pid,
        found: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut search as *mut Search as isize),
        );
    }
    search.found
}

pub fn animations_enabled() -> bool {
    let mut enabled = windows::core::BOOL(1);
    let queried = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut windows::core::BOOL as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    queried.is_err() || enabled.as_bool()
}

pub fn style_own_windows() -> bool {
    let found = visible_windows_of(std::process::id());
    unsafe {
        for hwnd in &found {
            let _ = SetWindowSubclass(*hwnd, Some(drag_subclass), DRAG_SUBCLASS_ID, 0);
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                *hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &round as *const DWM_WINDOW_CORNER_PREFERENCE as *const _,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
            let border: u32 = 0x0048_3c3c;
            let _ = DwmSetWindowAttribute(
                *hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
    !found.is_empty()
}

const DRAG_SUBCLASS_ID: usize = 0x5045_4542;

unsafe extern "system" fn drag_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    moving: usize,
) -> LRESULT {
    match msg {
        WM_ENTERSIZEMOVE => {
            let _ = SetWindowSubclass(hwnd, Some(drag_subclass), DRAG_SUBCLASS_ID, 1);
        }
        WM_EXITSIZEMOVE => {
            let _ = SetWindowSubclass(hwnd, Some(drag_subclass), DRAG_SUBCLASS_ID, 0);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        WM_PAINT if moving != 0 => return DefWindowProcW(hwnd, msg, wparam, lparam),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(drag_subclass), DRAG_SUBCLASS_ID);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let queried =
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        queried.ok()?;
        use std::os::windows::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
    }
}

const QUIT_FOR_UPDATE_FLAG: &str = "--quit-for-update";

fn request_cooperative_quit(pids: &[u32], budget: Duration) {
    if is_elevated() {
        ilog!("close: setup is elevated, so it will not run the launcher to ask it to quit");
        return;
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for pid in pids {
        if let Some(path) = process_image_path(*pid) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let deadline = Instant::now() + budget;
    for path in paths {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new(&path);
        cmd.arg(QUIT_FOR_UPDATE_FLAG).creation_flags(DETACHED_PROCESS);
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                ilog!("close: could not ask {} to quit: {e}", path.display());
                continue;
            }
        };
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    ilog!("close: quit request to {} did not finish in time", path.display());
                    break;
                }
            }
        }
    }
}

fn terminate_pid(pid: u32) {
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

pub fn close_all_by_name(exe_name: &str, graceful: Duration, force_wait: Duration) -> bool {
    let pids = pids_by_exe_name(exe_name);
    if pids.is_empty() {
        return true;
    }
    let started = Instant::now();
    request_cooperative_quit(&pids, graceful);
    post_close_to_windows(&pids);
    if wait_until_gone(exe_name, graceful.saturating_sub(started.elapsed())) {
        return true;
    }
    for pid in pids_by_exe_name(exe_name) {
        ilog!("close: pid {pid} did not exit in {graceful:?}, terminating");
        terminate_pid(pid);
    }
    wait_until_gone(exe_name, force_wait)
}

pub fn pids_by_exe_name_under(exe_name: &str, root: &Path) -> Vec<u32> {
    pids_by_exe_name(exe_name)
        .into_iter()
        .filter(|pid| {
            process_image_path(*pid)
                .is_some_and(|image| crate::paths::is_strictly_inside(&image, root))
        })
        .collect()
}

pub fn close_pids(pids: &[u32], graceful: Duration, force_wait: Duration) -> bool {
    if pids.is_empty() {
        return true;
    }
    post_close_to_windows(pids);
    let deadline = Instant::now() + graceful;
    for pid in pids {
        wait_for_pid_exit(*pid, deadline.saturating_duration_since(Instant::now()));
    }
    for pid in pids {
        if wait_for_pid_exit(*pid, Duration::ZERO) {
            continue;
        }
        ilog!("close: pid {pid} did not exit in {graceful:?}, terminating");
        terminate_pid(*pid);
    }
    let deadline = Instant::now() + force_wait;
    pids.iter()
        .all(|pid| wait_for_pid_exit(*pid, deadline.saturating_duration_since(Instant::now())))
}

pub fn file_in_use(path: &Path) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_USER_MAPPED_FILE: i32 = 1224;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(e) => matches!(
            e.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION) | Some(ERROR_USER_MAPPED_FILE)
        ),
    }
}

fn wait_until_gone(exe_name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut unwaitable: Option<u32> = None;
    loop {
        let pids = pids_by_exe_name(exe_name);
        if pids.is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if unwaitable == Some(pids[0]) {
            std::thread::sleep(remaining.min(Duration::from_millis(250)));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        unwaitable = wait_for_pid_exit(pids[0], remaining.min(Duration::from_millis(500)))
            .then_some(pids[0]);
    }
}

pub fn run_elevated_and_wait(exe: &Path, args: &str) -> Result<u32, String> {
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args);
    let owner = visible_windows_of(std::process::id())
        .into_iter()
        .next()
        .unwrap_or_default();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: owner,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: 1,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("elevation request failed: {e}"))?;
        if info.hProcess.is_invalid() {
            return Err("elevated process handle unavailable".into());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

pub fn spawn_detached(program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).creation_flags(DETACHED_PROCESS);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn {}: {e}", program.display()))
}

pub fn file_version(path: &Path) -> Option<(u32, u32, u32)> {
    let wide = HSTRING::from(path.as_os_str());
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            None,
            size,
            block.as_mut_ptr() as *mut core::ffi::c_void,
        )
        .ok()?;
        let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let root = HSTRING::from("\\");
        let found = VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            PCWSTR(root.as_ptr()),
            &mut info,
            &mut len,
        );
        if !found.as_bool()
            || info.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = std::ptr::read_unaligned(info as *const VS_FIXEDFILEINFO);
        Some((
            fixed.dwFileVersionMS >> 16,
            fixed.dwFileVersionMS & 0xffff,
            fixed.dwFileVersionLS >> 16,
        ))
    }
}

pub fn free_space_bytes(dir: &Path) -> Option<u64> {
    let mut probe = dir;
    loop {
        if probe.exists() {
            break;
        }
        probe = probe.parent()?;
    }
    let wide = HSTRING::from(probe.as_os_str());
    let mut available = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None).ok()?;
    }
    Some(available)
}

pub struct SingleInstance(windows::Win32::Foundation::HANDLE);

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn claim_single_instance() -> Option<SingleInstance> {
    unsafe {
        let name = HSTRING::from(r"Local\PeebifyLauncherSetup");
        let handle = CreateMutexW(None, true, PCWSTR(name.as_ptr())).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(SingleInstance(handle))
    }
}

pub fn open_folder(dir: &Path) {
    let verb = HSTRING::from("open");
    let file = HSTRING::from(dir.as_os_str());
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

pub fn retry<T>(
    attempts: u32,
    delay: Duration,
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut last;
    let mut n = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
        n += 1;
        if n >= attempts {
            return Err(last);
        }
        std::thread::sleep(delay);
    }
}

pub fn remove_quietly(mut op: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    retry(8, Duration::from_millis(250), || match op() {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_processes_in_our_session_count() {
        assert!(in_own_session(Some(1), Some(1)));
        assert!(!in_own_session(Some(1), Some(2)));
        assert!(!in_own_session(Some(1), None));
        assert!(in_own_session(None, Some(2)));
        assert!(in_own_session(None, None));
    }

    #[test]
    fn file_in_use_sees_a_held_file_only() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = create_run_dir("peebify-win-test").unwrap();
        let path = dir.join("hook.dll");
        assert!(!file_in_use(&path));
        std::fs::write(&path, b"MZ").unwrap();
        assert!(!file_in_use(&path));
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
            .open(&path)
            .unwrap();
        assert!(file_in_use(&path));
        drop(hold);
        assert!(!file_in_use(&path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mapped_image_counts_as_in_use() {
        use windows::Win32::Foundation::FreeLibrary;
        use windows::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        };
        let dir = create_run_dir("peebify-win-test").unwrap();
        let path = dir.join("hook.dll");
        std::fs::copy(system32_dir().join("version.dll"), &path).unwrap();
        let module = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_os_str()),
                None,
                LOAD_LIBRARY_AS_IMAGE_RESOURCE,
            )
        }
        .unwrap();
        assert!(file_in_use(&path));
        unsafe { FreeLibrary(module) }.unwrap();
        assert!(!file_in_use(&path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_version_reads_a_system_dll() {
        let (major, _, _) = file_version(&system32_dir().join("kernel32.dll")).expect("version");
        assert!(major >= 6);
        assert!(file_version(&system32_dir().join("no-such-file.dll")).is_none());
    }

    #[test]
    fn own_process_session_and_image_path_resolve() {
        let pid = std::process::id();
        assert!(session_of(pid).is_some());
        let image = process_image_path(pid).expect("image path");
        let expected = std::env::current_exe().unwrap();
        assert!(image
            .file_name()
            .unwrap()
            .eq_ignore_ascii_case(expected.file_name().unwrap()));
    }
}
