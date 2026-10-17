// ------------ Process Utilities ------------
// Windows helpers for finding, launching and stopping processes: process snapshots, starting a game through the shell,
// resolving subst drives, and starting tools elevated.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use peebify_helpers::common::{process::list_processes, wide};
use peebify_helpers::fps::cmdline::quote_always;
use tokio::process::{Child, Command};

pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(super) fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn snapshot_with_parents() -> Vec<(String, u32, u32)> {
    let mut entries = list_processes();
    if entries.is_empty() {
        log::warn!("Process listing failed: the snapshot came back empty");
    }
    for (name, _, _) in &mut entries {
        *name = name.to_lowercase();
    }
    entries
}

pub(crate) fn snapshot_processes() -> Option<Vec<(String, u32)>> {
    let entries = snapshot_with_parents();
    (!entries.is_empty()).then(|| entries.into_iter().map(|(name, pid, _)| (name, pid)).collect())
}

fn rewrite_subst(path: &str, device: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    let rest = &path[2..];
    if !(rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/')) {
        return None;
    }
    let target = device.strip_prefix(r"\??\")?;
    let base = match target.strip_prefix(r"UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => target.to_string(),
    };
    let mut resolved = format!("{}{rest}", base.trim_end_matches('\\'));
    if resolved.ends_with(':') {
        resolved.push('\\');
    }
    Some(resolved)
}

fn subst_step(path: &Path) -> Option<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

    let text = path.to_str()?;
    let drive = wide(text.get(..2)?);
    let mut buffer = vec![0u16; 1024];
    let written =
        unsafe { QueryDosDeviceW(drive.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if written == 0 {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let device = String::from_utf16_lossy(&buffer[..end]);
    rewrite_subst(text, &device).map(PathBuf::from)
}

pub fn resolve_subst(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..4 {
        match subst_step(&current) {
            Some(next) if next != current => current = next,
            _ => break,
        }
    }
    if current != path {
        log::info!(
            "Using {} for {} because administrator programs cannot see subst drive letters",
            current.display(),
            path.display()
        );
    }
    current
}

pub async fn is_process_running(process_name: &str) -> bool {
    let name = process_name.to_lowercase();
    snapshot_processes()
        .map(|procs| procs.iter().any(|(n, _)| *n == name))
        .unwrap_or(false)
}

pub async fn pids_by_name(names: &[String]) -> Option<HashMap<String, u32>> {
    if names.is_empty() {
        return Some(HashMap::new());
    }
    Some(pick_wanted(&snapshot_processes()?, names))
}

fn pick_wanted(processes: &[(String, u32)], names: &[String]) -> HashMap<String, u32> {
    let wanted: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let mut found = HashMap::new();
    for (name, pid) in processes {
        if wanted.contains(name) && !found.contains_key(name) {
            found.insert(name.clone(), *pid);
        }
    }
    found
}

pub fn is_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

pub async fn launch_game_via_shell(
    executable_path: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<(), SpawnError> {
    shell_start(executable_path, args, cwd, false).await
}

pub async fn launch_game_minimized(
    executable_path: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<(), SpawnError> {
    shell_start(executable_path, args, cwd, true).await
}

const ERROR_CANCELLED: u32 = 1223;

async fn shell_start(
    executable_path: &Path,
    args: &[String],
    cwd: Option<&Path>,
    minimized: bool,
) -> Result<(), SpawnError> {
    let failed = |message: String| SpawnError {
        declined: false,
        message,
    };
    let working_dir = cwd
        .map(|p| p.to_path_buf())
        .or_else(|| executable_path.parent().map(|p| p.to_path_buf()))
        .ok_or_else(|| failed("could not resolve working directory".to_string()))?;

    let parameters = args
        .iter()
        .map(|a| quote_always(a))
        .collect::<Vec<_>>()
        .join(" ");
    let file = executable_path.to_path_buf();
    let started = tauri::async_runtime::spawn_blocking(move || {
        shell_execute(&file, &parameters, &working_dir, minimized)
    })
    .await
    .map_err(|e| failed(e.to_string()))?;

    started.map_err(|code| {
        let reason = std::io::Error::from_raw_os_error(code as i32);
        log::warn!("Windows did not start {}: {reason}", executable_path.display());
        SpawnError {
            declined: code == ERROR_CANCELLED,
            message: reason.to_string(),
        }
    })
}

fn shell_execute(
    executable_path: &Path,
    parameters: &str,
    working_dir: &Path,
    minimized: bool,
) -> Result<(), u32> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOZONECHECKS,
        SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOWMINNOACTIVE, SW_SHOWNORMAL};

    let file = wide_path(executable_path);
    let params = wide(parameters);
    let dir = wide_path(working_dir);

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOZONECHECKS;
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = dir.as_ptr();
    info.nShow = if minimized {
        SW_SHOWMINNOACTIVE
    } else {
        SW_SHOWNORMAL
    };

    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

pub async fn spawn_tool(
    executable_path: &Path,
    args: &[String],
    env: &[(&str, &str)],
    cwd: &Path,
) -> Result<Child, String> {
    let mut cmd = Command::new(executable_path);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);

    cmd.spawn()
        .map_err(|e| format!("could not start {}: {e}", executable_path.display()))
}

fn normalized_path(path: &str) -> String {
    let path = super::fs_util::normalize_path(path);
    path.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(path)
}

fn image_is_under(image: &str, roots: &[String]) -> bool {
    let image = normalized_path(image);
    roots.iter().any(|root| {
        let root = normalized_path(root);
        !root.is_empty()
            && image.len() > root.len()
            && image.starts_with(&root)
            && image[root.len()..].starts_with('\\')
    })
}

fn install_roots(root: &Path) -> [String; 2] {
    [
        root.to_string_lossy().to_string(),
        resolve_subst(root).to_string_lossy().to_string(),
    ]
}

pub(crate) unsafe fn image_path(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    (QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0)
        .then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

pub async fn find_by_name_under(process_name: &str, root: &Path) -> Option<u32> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let name = process_name.to_lowercase();
    let processes = snapshot_processes()?;
    let roots = install_roots(root);
    processes
        .iter()
        .filter(|(n, _)| *n == name)
        .find_map(|(proc_name, pid)| unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, *pid);
            if handle.is_null() {
                log::debug!("Could not open {proc_name} (pid {pid}) to read its image path");
                return None;
            }
            let image = image_path(handle);
            CloseHandle(handle);
            image
                .filter(|image| image_is_under(image, &roots))
                .map(|_| *pid)
        })
}

pub async fn terminate_by_name_under(process_name: &str, root: &Path) -> usize {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    let name = process_name.to_lowercase();
    let Some(processes) = snapshot_processes() else {
        return 0;
    };
    let roots = install_roots(root);
    let mut killed = 0usize;
    for (proc_name, pid) in processes.iter().filter(|(n, _)| *n == name) {
        unsafe {
            let handle = OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                *pid,
            );
            if handle.is_null() {
                log::warn!("Could not open {proc_name} (pid {pid}) to terminate it");
                continue;
            }
            match image_path(handle) {
                Some(image) if image_is_under(&image, &roots) => {
                    if TerminateProcess(handle, 1) != 0 {
                        killed += 1;
                    }
                }
                Some(image) => {
                    log::debug!("Leaving {proc_name} (pid {pid}) running because {image} belongs to another install");
                }
                None => {
                    log::debug!("Leaving {proc_name} (pid {pid}) running because its image path is unreadable");
                }
            }
            CloseHandle(handle);
        }
    }
    killed
}

pub struct SpawnError {
    pub declined: bool,
    pub message: String,
}

pub enum ReadyOutcome {
    Ready,
    Exited(u32),
    TimedOut,
}

pub struct ProcessHandle(isize);

unsafe impl Send for ProcessHandle {}
unsafe impl Sync for ProcessHandle {}

impl ProcessHandle {
    const STILL_ACTIVE: u32 = 259;

    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0 as windows_sys::Win32::Foundation::HANDLE
    }

    pub fn exit_code(&self) -> Option<u32> {
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(self.raw(), &mut code) } != 0;
        (ok && code != Self::STILL_ACTIVE).then_some(code)
    }

    pub fn wait(&self, timeout: std::time::Duration) -> Option<u32> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        if unsafe { WaitForSingleObject(self.raw(), millis) } == WAIT_OBJECT_0 {
            return self.exit_code();
        }
        None
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.raw()) };
    }
}

#[derive(Clone)]
pub struct NamedEvent(std::sync::Arc<EventHandle>);

struct EventHandle(isize);

unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl Drop for EventHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.0 as windows_sys::Win32::Foundation::HANDLE) };
    }
}

impl NamedEvent {
    pub fn create(name: &str) -> Option<NamedEvent> {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide(name).as_ptr()) };
        (!handle.is_null()).then(|| NamedEvent(std::sync::Arc::new(EventHandle(handle as isize))))
    }

    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0 .0 as windows_sys::Win32::Foundation::HANDLE
    }

    pub fn set(&self) {
        use windows_sys::Win32::System::Threading::SetEvent;
        unsafe { SetEvent(self.raw()) };
    }

    pub fn reset(&self) {
        use windows_sys::Win32::System::Threading::ResetEvent;
        unsafe { ResetEvent(self.raw()) };
    }
}

pub fn wait_ready_or_exit(
    ready: &NamedEvent,
    process: &ProcessHandle,
    timeout: std::time::Duration,
) -> ReadyOutcome {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
    let handles = [ready.raw(), process.raw()];
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, millis) };
    if result == WAIT_OBJECT_0 {
        ReadyOutcome::Ready
    } else if result == WAIT_OBJECT_0 + 1 {
        ReadyOutcome::Exited(process.exit_code().unwrap_or(0))
    } else if result == WAIT_TIMEOUT {
        ReadyOutcome::TimedOut
    } else {
        ReadyOutcome::Exited(process.exit_code().unwrap_or(0))
    }
}

pub fn spawn_tool_elevated_handle(
    executable_path: &Path,
    args: &[String],
    cwd: &Path,
) -> Result<ProcessHandle, SpawnError> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_CANCELLED};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };

    const SW_HIDE: i32 = 0;

    let line = args
        .iter()
        .map(|arg| peebify_helpers::fps::cmdline::quote_argument(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = wide("runas");
    let file = wide_path(&resolve_subst(executable_path));
    let params = wide(&line);
    let dir = wide_path(&resolve_subst(cwd));

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = dir.as_ptr();
    info.nShow = SW_HIDE;

    let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
    if !ok || info.hProcess.is_null() {
        let code = unsafe { GetLastError() };
        return Err(SpawnError {
            declined: code == ERROR_CANCELLED,
            message: peebify_helpers::fps::cmdline::describe_start_error(code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("ShellExecuteExW failed with error {code}")),
        });
    }
    Ok(ProcessHandle(info.hProcess as isize))
}

#[cfg(test)]
mod tests {
    use super::{image_is_under, rewrite_subst};
    use peebify_helpers::fps::cmdline::quote_always;

    #[cfg(windows)]
    #[test]
    fn launch_arguments_reach_the_program_unchanged() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

        let args = [
            "%PATH%",
            "!USERNAME!",
            "a&b|c^d<e>f",
            r#"say "hi""#,
            r"D:\My Games\",
            "",
            "-dx11",
        ];
        let line = std::iter::once("game.exe".to_string())
            .chain(args.iter().map(|a| quote_always(a)))
            .collect::<Vec<_>>()
            .join(" ");
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();

        let mut count = 0i32;
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
        assert!(!argv.is_null());
        let parsed: Vec<String> = (1..count as usize)
            .map(|i| unsafe {
                let arg = *argv.add(i);
                let len = (0..).take_while(|&n| *arg.add(n) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(arg, len))
            })
            .collect();
        unsafe { LocalFree(argv.cast()) };

        assert_eq!(parsed, args);
    }

    #[test]
    fn rewrite_subst_follows_dos_device_targets() {
        assert_eq!(
            rewrite_subst(r"S:\Games\Genshin", r"\??\C:\Subst").as_deref(),
            Some(r"C:\Subst\Games\Genshin")
        );
        assert_eq!(
            rewrite_subst(r"S:\Games", r"\??\C:\").as_deref(),
            Some(r"C:\Games")
        );
        assert_eq!(rewrite_subst("S:", r"\??\C:\").as_deref(), Some(r"C:\"));
        assert_eq!(
            rewrite_subst(r"S:\Mods", r"\??\UNC\server\share").as_deref(),
            Some(r"\\server\share\Mods")
        );
    }

    #[test]
    fn rewrite_subst_leaves_real_drives_alone() {
        assert_eq!(rewrite_subst(r"C:\Games", r"\Device\HarddiskVolume3"), None);
        assert_eq!(
            rewrite_subst(r"Z:\Mods", r"\Device\LanmanRedirector\;Z:0000\server\share"),
            None
        );
        assert_eq!(rewrite_subst(r"\\server\share\x", r"\??\C:\"), None);
        assert_eq!(rewrite_subst(r"S:relative", r"\??\C:\"), None);
    }

    #[test]
    fn companions_only_match_inside_the_install_root() {
        let roots = [r"D:\Games\Reverse1999\".to_string(), String::new()];
        assert!(image_is_under(r"d:\games\reverse1999\UnityCrashHandler64.exe", &roots));
        assert!(image_is_under(r"\\?\D:/Games/Reverse1999/sub/ZFGameBrowser.exe", &roots));
        assert!(!image_is_under(r"D:\Games\Reverse1999 Beta\UnityCrashHandler64.exe", &roots));
        assert!(!image_is_under(r"D:\Games\Genshin\UnityCrashHandler64.exe", &roots));
        assert!(!image_is_under(r"C:\UnityCrashHandler64.exe", &[String::new()]));
    }
}

pub mod front_end {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use peebify_helpers::common::process::visible_windows_of;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindow, IsWindowVisible, ShowWindow, SW_HIDE, SW_RESTORE,
    };

    const SWEEP_INTERVAL: Duration = Duration::from_millis(100);
    const SWEEP_LIMIT: Duration = Duration::from_secs(60);
    const CLIENT_TAIL: Duration = Duration::from_secs(5);

    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        Continue,
        Finish,
        Reveal,
    }

    fn next_step(elapsed: Duration, client_seen_for: Option<Duration>) -> Step {
        match client_seen_for {
            Some(seen) if seen >= CLIENT_TAIL => Step::Finish,
            Some(_) => Step::Continue,
            None if elapsed >= SWEEP_LIMIT => Step::Reveal,
            None => Step::Continue,
        }
    }

    fn front_end_pids(
        processes: &[(String, u32, u32)],
        front_end: &HashSet<String>,
    ) -> HashSet<u32> {
        processes
            .iter()
            .filter(|(name, _, _)| front_end.contains(name))
            .map(|(_, pid, _)| *pid)
            .collect()
    }

    fn hide_windows(pids: &HashSet<u32>, hidden: &mut Vec<HWND>) {
        for hwnd in pids.iter().flat_map(|&pid| visible_windows_of(pid)) {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            if !hidden.contains(&hwnd) {
                hidden.push(hwnd);
            }
        }
    }

    fn reveal_windows(hidden: &[HWND]) -> usize {
        let mut revealed = 0;
        for &hwnd in hidden {
            unsafe {
                if IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) == 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                    revealed += 1;
                }
            }
        }
        revealed
    }

    pub fn conceal_until_client(front_end: Vec<String>, client: String) {
        if front_end.is_empty() {
            return;
        }
        let front_end: HashSet<String> = front_end.iter().map(|n| n.to_lowercase()).collect();
        let client = client.to_lowercase();
        tauri::async_runtime::spawn_blocking(move || {
            let started = Instant::now();
            let mut client_seen: Option<Instant> = None;
            let mut hidden: Vec<HWND> = Vec::new();
            loop {
                let processes = super::snapshot_with_parents();
                if client_seen.is_none() && processes.iter().any(|(name, _, _)| *name == client) {
                    client_seen = Some(Instant::now());
                }
                hide_windows(&front_end_pids(&processes, &front_end), &mut hidden);
                match next_step(started.elapsed(), client_seen.map(|at| at.elapsed())) {
                    Step::Continue => std::thread::sleep(SWEEP_INTERVAL),
                    Step::Finish => {
                        log::info!(
                            "front end: kept {} official launcher window(s) hidden until {client} started",
                            hidden.len()
                        );
                        break;
                    }
                    Step::Reveal => {
                        let revealed = reveal_windows(&hidden);
                        log::warn!(
                            "front end: {client} did not start within {}s, so {revealed} hidden official launcher window(s) were shown again",
                            SWEEP_LIMIT.as_secs()
                        );
                        break;
                    }
                }
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sweep_continues_until_the_client_has_run_for_the_tail() {
            assert_eq!(next_step(Duration::ZERO, None), Step::Continue);
            assert_eq!(
                next_step(Duration::from_secs(20), Some(Duration::from_secs(1))),
                Step::Continue
            );
            assert_eq!(
                next_step(Duration::from_secs(20), Some(CLIENT_TAIL)),
                Step::Finish
            );
        }

        #[test]
        fn sweep_reveals_when_the_client_never_starts() {
            assert_eq!(
                next_step(SWEEP_LIMIT - Duration::from_millis(1), None),
                Step::Continue
            );
            assert_eq!(next_step(SWEEP_LIMIT, None), Step::Reveal);
        }

        #[test]
        fn late_client_still_gets_its_tail_past_the_limit() {
            assert_eq!(
                next_step(SWEEP_LIMIT + Duration::from_secs(1), Some(Duration::from_secs(2))),
                Step::Continue
            );
        }

        #[test]
        fn front_end_pids_match_every_instance_by_name() {
            let front_end: HashSet<String> = ["NTEGlobalGame.exe", "NTEGlobalBrowser.exe"]
                .iter()
                .map(|n| n.to_lowercase())
                .collect();
            let processes = vec![
                ("nteglobalgame.exe".to_string(), 10, 1),
                ("nteglobalupdate.exe".to_string(), 11, 10),
                ("nteglobalbrowser.exe".to_string(), 12, 10),
                ("nteglobalbrowser.exe".to_string(), 13, 10),
                ("htgame.exe".to_string(), 14, 10),
            ];
            let pids = front_end_pids(&processes, &front_end);
            assert_eq!(pids, HashSet::from([10, 12, 13]));
        }
    }
}
