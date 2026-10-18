#![windows_subsystem = "windows"]

// ------------ FPS Unlocker Helper ------------
// peebify-fps-helper.exe, run as administrator. Starts the game (--game) or waits for it (--attach), waits for its
// window, then hooks the stub DLL into it so the stub can lift the frame-rate cap. Progress and errors go back to the
// launcher through shared memory.

#[cfg(windows)]
mod helper {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, OpenProcess, Sleep, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
        STARTUPINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowThreadProcessId, PostThreadMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, WH_GETMESSAGE,
    };

    use peebify_helpers::common::process::{
        find_processes, is_running, visible_windows_of, PinnedLibrary,
    };
    use peebify_helpers::common::wide;
    use peebify_helpers::common::cmdline;
    use peebify_helpers::fps::section::SectionView;
    use peebify_helpers::fps::shared::{
        Shared, STATUS_ATTACHED, STATUS_FAILED, STATUS_INJECTING, STATUS_LAUNCHING, STATUS_READY,
        STOP_NONE, STUB_NAME,
    };

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(6 * 60);
    const WINDOW_TIMEOUT: Duration = Duration::from_secs(240);
    const ATTACH_TIMEOUT: Duration = Duration::from_secs(20);
    const READY_TIMEOUT: Duration = Duration::from_secs(60);

    const POLL: Duration = Duration::from_millis(250);
    const WM_NULL: u32 = 0;

    type HookProcedure = unsafe extern "system" fn(
        i32,
        windows_sys::Win32::Foundation::WPARAM,
        LPARAM,
    ) -> windows_sys::Win32::Foundation::LRESULT;

    struct Options {
        game: Option<PathBuf>,
        attach: Option<String>,
        game_arguments: Vec<String>,
    }

    struct Game {
        handle: HANDLE,
        pid: u32,
    }

    fn parse_options() -> Result<Options, String> {
        let mut game = None;
        let mut attach = None;
        let mut game_arguments = Vec::new();

        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| format!("{argument} needs a value"))
            };
            match argument.as_str() {
                "--game" => game = Some(PathBuf::from(value()?)),
                "--attach" => attach = Some(value()?),
                "--" => {
                    game_arguments.extend(arguments);
                    break;
                }
                other => return Err(format!("unexpected argument {other}")),
            }
        }

        if game.is_some() == attach.is_some() {
            return Err("pass either --game or --attach".to_string());
        }

        Ok(Options {
            game,
            attach,
            game_arguments,
        })
    }

    fn wait_for_process(shared: &Shared, name: &str) -> Result<Game, String> {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Some(&pid) = find_processes(name).first() {
                let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
                if !handle.is_null() {
                    return Ok(Game { handle, pid });
                }
            }
            if called_off(shared) {
                return Err("The unlocker was stopped before the game started.".to_string());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{name} never started, so the unlocker could not attach."
                ));
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    fn start_game(path: &Path, options: &Options) -> Result<Game, String> {
        let game = path.to_string_lossy().to_string();
        let directory = path.parent().ok_or("could not work out the game's folder")?;

        let application = wide(&game);
        let mut command_line = wide(&cmdline::build_command_line(
            &game,
            &options.game_arguments,
        ));
        let directory = wide(&directory.to_string_lossy());

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let started = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                directory.as_ptr(),
                &startup,
                &mut process,
            )
        };
        if started == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(
                match cmdline::describe_start_error(code as u32) {
                    Some(message) => message.to_string(),
                    None => format!("Windows refused to start the game (error {code})."),
                },
            );
        }
        unsafe { CloseHandle(process.hThread) };
        Ok(Game {
            handle: process.hProcess,
            pid: process.dwProcessId,
        })
    }

    fn game_window_thread(pid: u32) -> Option<u32> {
        let windows = visible_windows_of(pid);
        let is_unity = |window: &HWND| {
            let mut class = [0u16; 64];
            let length = unsafe { GetClassNameW(*window, class.as_mut_ptr(), class.len() as i32) };
            String::from_utf16_lossy(&class[..length.max(0) as usize]) == "UnityWndClass"
        };
        let window = *windows.iter().find(|w| is_unity(w)).or(windows.first())?;
        let mut owner = 0u32;
        let thread = unsafe { GetWindowThreadProcessId(window, &mut owner) };
        (thread != 0 && owner == pid).then_some(thread)
    }

    fn called_off(shared: &Shared) -> bool {
        shared.stop.load(Ordering::Acquire) != STOP_NONE
    }

    fn wait_for_window(shared: &Shared, process: HANDLE, pid: u32) -> Result<u32, String> {
        let deadline = Instant::now() + WINDOW_TIMEOUT;
        loop {
            if let Some(thread) = game_window_thread(pid) {
                return Ok(thread);
            }
            if !is_running(process) || called_off(shared) {
                return Err("The game closed before the unlocker could attach.".to_string());
            }
            if Instant::now() >= deadline {
                return Err(
                    "The game never opened a window, so the unlocker could not attach.".to_string(),
                );
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    fn stub_path() -> Result<PathBuf, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("Could not find the unlocker's own folder ({error})."))?;
        Ok(exe.with_file_name(STUB_NAME))
    }

    fn inject(shared: &Shared, thread: u32, process: HANDLE) -> Result<(), String> {
        if thread == 0 {
            return Err("The game's window closed before the unlocker could attach.".to_string());
        }
        let stub = stub_path()?;
        let failed =
            |error: std::io::Error| format!("Could not load {} ({error}).", stub.display());
        let pinned = PinnedLibrary::open(&stub).map_err(failed)?;
        let module = pinned.load().map_err(failed)?;
        let Some(procedure) = (unsafe { GetProcAddress(module, c"WndProc".as_ptr() as *const u8) })
        else {
            return Err("The unlocker's stub is missing its hook procedure.".to_string());
        };

        let procedure = unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, HookProcedure>(procedure)
        };
        let hook = unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(procedure), module, thread) };
        if hook.is_null() {
            return Err(format!(
                "Windows would not let the unlocker attach to the game ({}).",
                std::io::Error::last_os_error()
            ));
        }

        shared.status.store(STATUS_INJECTING, Ordering::Release);

        let deadline = Instant::now() + ATTACH_TIMEOUT;
        let mut attached = false;
        while !attached {
            unsafe { PostThreadMessageW(thread, WM_NULL, 0, 0) };
            unsafe { Sleep(POLL.as_millis() as u32) };
            attached = matches!(
                shared.status.load(Ordering::Acquire),
                STATUS_ATTACHED | STATUS_READY | STATUS_FAILED
            );
            if !attached
                && (Instant::now() >= deadline || !is_running(process) || called_off(shared))
            {
                break;
            }
        }

        unsafe { UnhookWindowsHookEx(hook) };
        drop(pinned);

        if !attached {
            return Err(
                "The unlocker attached to the game but its code never started running.".to_string(),
            );
        }
        Ok(())
    }

    fn wait_until_ready(shared: &Shared, process: HANDLE) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match shared.status.load(Ordering::Acquire) {
                STATUS_READY => return Ok(()),
                STATUS_FAILED => return Err(shared.error_message()),
                _ => {}
            }
            if !is_running(process) || called_off(shared) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "The unlocker could not find the game's frame-rate setting.".to_string()
                );
            }
            unsafe { Sleep(POLL.as_millis() as u32) };
        }
    }

    fn start_plain(options: &Options) -> i32 {
        let Some(path) = &options.game else {
            return 2;
        };
        match start_game(path, options) {
            Ok(game) => {
                unsafe { CloseHandle(game.handle) };
                0
            }
            Err(_) => 1,
        }
    }

    pub fn main() -> i32 {
        unsafe {
            windows_sys::Win32::System::LibraryLoader::SetDefaultDllDirectories(
                windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        let options = parse_options();
        let section = SectionView::open()
            .ok()
            .filter(|section| section.shared().is_configured());
        let Some(section) = section else {
            return match &options {
                Ok(options) => start_plain(options),
                Err(_) => 2,
            };
        };
        let shared = section.shared();
        let options = match options {
            Ok(options) => options,
            Err(error) => {
                shared.fail(&format!("bad arguments: {error}"));
                return 2;
            }
        };
        shared.clear_error();
        shared.status.store(STATUS_LAUNCHING, Ordering::Release);

        let started = match (&options.game, &options.attach) {
            (Some(path), _) => start_game(path, &options),
            (_, Some(name)) => wait_for_process(shared, name),
            _ => Err("nothing to launch or attach to".to_string()),
        };
        let game = match started {
            Ok(game) => game,
            Err(error) => {
                shared.fail(&error);
                return 1;
            }
        };
        shared.game_pid.store(game.pid, Ordering::Release);

        let outcome = wait_for_window(shared, game.handle, game.pid)
            .and_then(|thread| inject(shared, thread, game.handle))
            .and_then(|()| wait_until_ready(shared, game.handle));

        unsafe { CloseHandle(game.handle) };

        match outcome {
            Ok(()) => 0,
            Err(error) => {
                shared.fail(&error);
                1
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    std::process::exit(helper::main());
}

#[cfg(not(windows))]
fn main() {
    std::process::exit(1);
}
