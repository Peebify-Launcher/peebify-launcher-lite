#![windows_subsystem = "windows"]

// ------------ Mod Loader ------------
// peebify-mod-loader.exe, run as administrator, gets the 3DMigoto loader library into a game. Hook mode installs a
// Windows hook and waits for the game to pick the library up, inject mode writes it into the running game.
// The library is checked against its expected SHA-256 first.

#[cfg(windows)]
mod host {
    use std::fs::File;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_BAD_LENGTH, ERROR_PARTIAL_COPY, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
        TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenEventW, SetEvent, Sleep, WaitForSingleObject,
        SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::HHOOK;

    use peebify_helpers::common::process::{
        create_log_file, find_processes, process_alive, visible_windows_of, PinnedLibrary,
    };
    use peebify_helpers::common::wide;
    use peebify_helpers::mods::*;

    const POLL: Duration = Duration::from_millis(100);
    const MAPPED_TIMEOUT: Duration = Duration::from_secs(180);
    const UNREADABLE_GRACE: Duration = Duration::from_secs(15);
    const RELAUNCH_GRACE: Duration = Duration::from_secs(20);
    const WINDOW_TIMEOUT: Duration = Duration::from_secs(120);
    const HOOK_LINGER: Duration = Duration::from_secs(3);
    const HOOK_FOLLOWS: usize = 3;
    const INJECT_LOAD_TIMEOUT_SECS: i32 = 30;

    type HookLibraryFn = unsafe extern "C" fn(*const u16, *mut HHOOK, *mut HANDLE) -> i32;
    type UnhookLibraryFn = unsafe extern "C" fn(*mut HHOOK, *mut HANDLE) -> i32;
    type InjectFn = unsafe extern "C" fn(u32, *const u16, i32) -> i32;
    type RawExport = unsafe extern "system" fn() -> isize;

    struct Log(Option<File>);

    impl Log {
        fn open(path: Option<&Path>) -> Log {
            Log(path.and_then(|p| create_log_file(p).ok()))
        }

        fn line(&mut self, text: &str) {
            if let Some(file) = self.0.as_mut() {
                let _ = writeln!(file, "{text}");
                let _ = file.flush();
            }
        }
    }

    fn open_event(name: &str) -> HANDLE {
        let name = wide(name);
        unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) }
    }

    fn open_existing_event(name: &str) -> HANDLE {
        let name = wide(name);
        unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) }
    }

    fn signalled(event: HANDLE) -> bool {
        !event.is_null() && unsafe { WaitForSingleObject(event, 0) } == WAIT_OBJECT_0
    }

    struct Stop {
        own: HANDLE,
        all: HANDLE,
    }

    impl Stop {
        fn requested(&self) -> bool {
            signalled(self.own) || signalled(self.all)
        }
    }

    impl Drop for Stop {
        fn drop(&mut self) {
            for handle in [self.own, self.all] {
                if !handle.is_null() {
                    unsafe { CloseHandle(handle) };
                }
            }
        }
    }

    fn live_targets(name: &str) -> Vec<u32> {
        find_processes(name)
            .into_iter()
            .filter(|&pid| process_alive(pid))
            .collect()
    }

    enum Mapped {
        Yes,
        No,
        Starting,
        Unreadable(u32),
    }

    fn module_mapped(pid: u32, module: &Path) -> Mapped {
        let snapshot =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if snapshot == INVALID_HANDLE_VALUE {
            let error = unsafe { GetLastError() };
            return if error == ERROR_BAD_LENGTH || error == ERROR_PARTIAL_COPY {
                Mapped::Starting
            } else {
                Mapped::Unreadable(error)
            };
        }
        let wanted = module.to_string_lossy().to_lowercase();
        let wanted_name = module
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let wanted_dir = module
            .parent()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut mapped = false;
        if unsafe { Module32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                let end = entry
                    .szExePath
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExePath.len());
                let path = String::from_utf16_lossy(&entry.szExePath[..end]).to_lowercase();
                if path == wanted
                    || (path.ends_with(&wanted_name) && path.starts_with(&wanted_dir))
                {
                    mapped = true;
                    break;
                }
                if unsafe { Module32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snapshot) };
        if mapped {
            Mapped::Yes
        } else {
            Mapped::No
        }
    }

    fn wait_for_target(options: &Options, stop: &Stop, log: &mut Log) -> Result<u32, i32> {
        let deadline = Instant::now() + options.timeout;
        loop {
            if let Some(&pid) = live_targets(&options.target).first() {
                log.line(&format!("target {} is running as pid {pid}", options.target));
                return Ok(pid);
            }
            if stop.requested() {
                log.line("stopped by the launcher before the target appeared");
                return Err(EXIT_STOPPED);
            }
            if Instant::now() >= deadline {
                log.line(&format!(
                    "target {} did not appear within {} s",
                    options.target,
                    options.timeout.as_secs()
                ));
                return Err(EXIT_TARGET_TIMEOUT);
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    fn wait_until_mapped(
        options: &Options,
        module: &Path,
        first: u32,
        stop: &Stop,
        log: &mut Log,
    ) -> Result<u32, i32> {
        let started = Instant::now();
        let deadline = started + MAPPED_TIMEOUT;
        let mut watch = TargetWatch::new(RELAUNCH_GRACE, first, started);
        loop {
            let now = Instant::now();
            let pids = live_targets(&options.target);
            let tick = watch.observe(&pids, now);
            for pid in &tick.exited {
                if tick.presence == Presence::Present {
                    log.line(&format!("pid {pid} exited"));
                } else {
                    log.line(&format!(
                        "pid {pid} exited, waiting up to {} s for the game to come back",
                        RELAUNCH_GRACE.as_secs()
                    ));
                }
            }
            for pid in &tick.appeared {
                log.line(&format!("target {} is running as pid {pid}", options.target));
            }
            if tick.presence == Presence::Expired {
                log.line(&format!(
                    "the target exited before the module was mapped and did not come back within {} s",
                    RELAUNCH_GRACE.as_secs()
                ));
                return Err(EXIT_TARGET_EXITED);
            }
            for &pid in &pids {
                match module_mapped(pid, module) {
                    Mapped::Yes => {
                        log.line(&format!("{} is mapped into pid {pid}", module.display()));
                        return Ok(pid);
                    }
                    Mapped::Unreadable(error)
                        if watch.seen_for(pid, now) >= UNREADABLE_GRACE
                            && !visible_windows_of(pid).is_empty() =>
                    {
                        log.line(&format!(
                            "the module list of pid {pid} cannot be read (error {error}) but its window is up, so the hook is assumed to have landed"
                        ));
                        return Ok(pid);
                    }
                    Mapped::No | Mapped::Starting | Mapped::Unreadable(_) => {}
                }
            }
            if stop.requested() {
                log.line("stopped by the launcher while waiting for the module to map");
                return Err(EXIT_STOPPED);
            }
            if Instant::now() >= deadline {
                log.line(&format!(
                    "the module was not mapped within {} s",
                    MAPPED_TIMEOUT.as_secs()
                ));
                return Err(EXIT_NOT_MAPPED);
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    enum WindowWait {
        Shown,
        Exited,
        Stopped,
        TimedOut,
    }

    fn wait_for_window(pid: u32, stop: &Stop) -> WindowWait {
        let deadline = Instant::now() + WINDOW_TIMEOUT;
        loop {
            if !process_alive(pid) {
                return WindowWait::Exited;
            }
            if !visible_windows_of(pid).is_empty() {
                return WindowWait::Shown;
            }
            if stop.requested() {
                return WindowWait::Stopped;
            }
            if Instant::now() >= deadline {
                return WindowWait::TimedOut;
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    fn hold_hook(options: &Options, module: &Path, first: u32, stop: &Stop, log: &mut Log) -> i32 {
        let mut next = first;
        for _ in 0..HOOK_FOLLOWS {
            let pid = match wait_until_mapped(options, module, next, stop, log) {
                Ok(pid) => pid,
                Err(code) => return code,
            };
            match wait_for_window(pid, stop) {
                WindowWait::Shown => {
                    log.line(&format!(
                        "the window of pid {pid} is up, keeping the hook {} s longer",
                        HOOK_LINGER.as_secs()
                    ));
                    unsafe { Sleep(HOOK_LINGER.as_millis() as u32) };
                    return EXIT_OK;
                }
                WindowWait::Stopped | WindowWait::TimedOut => return EXIT_OK,
                WindowWait::Exited => {
                    log.line(&format!(
                        "pid {pid} exited after the module was mapped, waiting up to {} s for the game to restart",
                        RELAUNCH_GRACE.as_secs()
                    ));
                    let deadline = Instant::now() + RELAUNCH_GRACE;
                    let restarted = loop {
                        if let Some(&pid) = live_targets(&options.target).first() {
                            break Some(pid);
                        }
                        if stop.requested() || Instant::now() >= deadline {
                            break None;
                        }
                        unsafe { Sleep(POLL.as_millis() as u32) };
                    };
                    match restarted {
                        Some(pid) => {
                            log.line(&format!("target {} restarted as pid {pid}", options.target));
                            next = pid;
                        }
                        None => return EXIT_OK,
                    }
                }
            }
        }
        EXIT_OK
    }

    struct Library {
        hook: HookLibraryFn,
        unhook: UnhookLibraryFn,
        inject: InjectFn,
    }

    fn pin_checked(
        path: &Path,
        expected: Option<[u8; 32]>,
        log: &mut Log,
    ) -> Option<PinnedLibrary> {
        let pinned = match PinnedLibrary::open(path) {
            Ok(pinned) => pinned,
            Err(error) => {
                log.line(&format!("could not open {} ({error})", path.display()));
                return None;
            }
        };
        let Some(expected) = expected else {
            log.line(&format!(
                "no checksum was passed for {}, so it is loaded unchecked",
                pinned.path().display()
            ));
            return Some(pinned);
        };
        match pinned.sha256() {
            Ok(found) if found == expected => Some(pinned),
            Ok(found) => {
                log.line(&format!(
                    "{} does not match the copy the launcher checked (expected {}, found {})",
                    pinned.path().display(),
                    sha256_hex(&expected),
                    sha256_hex(&found)
                ));
                None
            }
            Err(error) => {
                log.line(&format!(
                    "could not read {} to check it ({error})",
                    pinned.path().display()
                ));
                None
            }
        }
    }

    fn load_library(options: &Options, log: &mut Log) -> Result<Library, i32> {
        let pinned = pin_checked(&options.dll, options.dll_sha256, log).ok_or(EXIT_DLL_LOAD)?;
        let module = pinned.load().map_err(|error| {
            log.line(&format!(
                "could not load {} ({error})",
                pinned.path().display()
            ));
            EXIT_DLL_LOAD
        })?;
        drop(pinned);
        let export = |name: &std::ffi::CStr| unsafe {
            GetProcAddress(module, name.as_ptr() as *const u8)
        };
        let (Some(hook), Some(unhook), Some(inject)) = (
            export(c"HookLibrary"),
            export(c"UnhookLibrary"),
            export(c"Inject"),
        ) else {
            log.line("3dmloader.dll is missing HookLibrary, UnhookLibrary or Inject");
            return Err(EXIT_DLL_EXPORTS);
        };
        Ok(unsafe {
            Library {
                hook: std::mem::transmute::<RawExport, HookLibraryFn>(hook),
                unhook: std::mem::transmute::<RawExport, UnhookLibraryFn>(unhook),
                inject: std::mem::transmute::<RawExport, InjectFn>(inject),
            }
        })
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn run_hook(
        options: &Options,
        library: &Library,
        module: &Path,
        ready: HANDLE,
        stop: &Stop,
        log: &mut Log,
    ) -> i32 {
        let path = wide_path(module);
        let mut hook: HHOOK = std::ptr::null_mut();
        let mut mutex: HANDLE = std::ptr::null_mut();
        let result = unsafe { (library.hook)(path.as_ptr(), &mut hook, &mut mutex) };
        let code = hook_exit_code(result, !hook.is_null());
        if code != EXIT_OK {
            log.line(&format!(
                "HookLibrary failed with {result}: {} ({})",
                describe_hook_result(result),
                std::io::Error::last_os_error()
            ));
            if result == 0 {
                unsafe { (library.unhook)(&mut hook, &mut mutex) };
            }
            return code;
        }
        log.line("hook installed");
        unsafe { SetEvent(ready) };

        let outcome = match wait_for_target(options, stop, log) {
            Ok(pid) => hold_hook(options, module, pid, stop, log),
            Err(code) => code,
        };

        let unhooked = unsafe { (library.unhook)(&mut hook, &mut mutex) };
        log.line(&format!("hook removed (result {unhooked})"));
        outcome
    }

    fn run_inject(
        options: &Options,
        library: &Library,
        module: &Path,
        ready: HANDLE,
        stop: &Stop,
        log: &mut Log,
    ) -> i32 {
        unsafe { SetEvent(ready) };
        let mut pid = match wait_for_target(options, stop, log) {
            Ok(pid) => pid,
            Err(code) => return code,
        };
        if !process_alive(pid) {
            let Some(&next) = live_targets(&options.target).first() else {
                log.line("the target exited before injection");
                return EXIT_TARGET_EXITED;
            };
            log.line(&format!(
                "pid {pid} exited before injection, so the module goes into pid {next}"
            ));
            pid = next;
        }
        let path = wide_path(module);
        let code = unsafe { (library.inject)(pid, path.as_ptr(), INJECT_LOAD_TIMEOUT_SECS) };
        if code != 0 {
            log.line(&format!(
                "Inject failed with {code}: {} ({})",
                describe_inject_error(code),
                std::io::Error::last_os_error()
            ));
            return EXIT_INJECT_FAILED;
        }
        log.line(&format!("{} injected into pid {pid}", module.display()));
        EXIT_OK
    }

    pub fn main() -> i32 {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let options = match parse_options(&arguments) {
            Ok(options) => options,
            Err(error) => {
                let mut log = Log::open(log_argument(&arguments).as_deref());
                log.line(&format!("bad arguments: {error}"));
                log.line(&format!("arguments: {}", arguments.join(" ")));
                log.line(&format!(
                    "exit {EXIT_BAD_ARGUMENTS}: {}",
                    describe_exit(EXIT_BAD_ARGUMENTS)
                ));
                return EXIT_BAD_ARGUMENTS;
            }
        };
        let mut log = Log::open(options.log.as_deref());
        log.line(&format!(
            "mode={} target={} module={} timeout={}s",
            options.mode.as_str(),
            options.target,
            options.module.display(),
            options.timeout.as_secs()
        ));

        let ready = open_event(&format!("{}{READY_EVENT_SUFFIX}", options.event));
        let stop = Stop {
            own: open_event(&format!("{}{STOP_EVENT_SUFFIX}", options.event)),
            all: open_existing_event(STOP_ALL_EVENT),
        };
        if stop.all.is_null() {
            log.line("the launcher's shared stop event is not available");
        }

        let library = match load_library(&options, &mut log) {
            Ok(library) => library,
            Err(code) => return code,
        };

        let Some(module) = pin_checked(&options.module, options.module_sha256, &mut log) else {
            return EXIT_MODULE_CHECK;
        };
        let code = match options.mode {
            Mode::Hook => run_hook(&options, &library, module.path(), ready, &stop, &mut log),
            Mode::Inject => run_inject(&options, &library, module.path(), ready, &stop, &mut log),
        };
        drop(module);
        log.line(&format!("exit {code}: {}", describe_exit(code)));
        unsafe {
            CloseHandle(ready);
        }
        code
    }
}

#[cfg(windows)]
fn main() {
    std::process::exit(host::main());
}

#[cfg(not(windows))]
fn main() {
    std::process::exit(1);
}
