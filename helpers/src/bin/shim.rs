#![windows_subsystem = "windows"]

// ------------ Launch Shim ------------
// peebify-launch-shim.exe, run as administrator. Starts a game as the child of a stand-in (the publisher's own launcher
// exe, created suspended so it never runs), or with --front-end drives the publisher's launcher out of sight.

#[cfg(windows)]
mod shim {
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, STARTUPINFOEXW, STARTUPINFOW,
    };

    use peebify_helpers::common::cmdline;
    use peebify_helpers::common::process::create_log_file;
    use peebify_helpers::common::wide;
    use peebify_helpers::front_end::{self, Plan};

    const EXIT_BAD_ARGUMENTS: i32 = 2;
    const EXIT_STAND_IN: i32 = 3;
    const EXIT_GAME: i32 = 4;

    enum Mode {
        StandIn { parent: PathBuf, game: PathBuf },
        FrontEnd(Plan),
    }

    struct Options {
        mode: Mode,
        log: Option<PathBuf>,
        game_arguments: Vec<String>,
    }

    struct Log(Option<File>);

    impl Log {
        fn line(&mut self, text: &str) {
            if let Some(file) = self.0.as_mut() {
                let _ = writeln!(file, "{text}");
                let _ = file.flush();
            }
        }
    }

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn parse_options(arguments: &[String]) -> Result<Options, String> {
        let (mut parent, mut game, mut log) = (None, None, None);
        let (mut launcher, mut client, mut ready_log, mut ready_line, mut button) = (None, None, None, None, None);
        let mut game_arguments = Vec::new();
        let mut rest = arguments.iter();
        while let Some(argument) = rest.next() {
            let mut value = || {
                rest.next()
                    .cloned()
                    .ok_or_else(|| format!("{argument} needs a value"))
            };
            match argument.as_str() {
                "--parent" => parent = Some(PathBuf::from(value()?)),
                "--game" => game = Some(PathBuf::from(value()?)),
                "--log" => log = Some(PathBuf::from(value()?)),
                "--front-end" => launcher = Some(PathBuf::from(value()?)),
                "--client" => client = Some(value()?),
                "--ready-log" => ready_log = Some(PathBuf::from(value()?)),
                "--ready-line" => ready_line = Some(value()?),
                "--button" => button = Some(value()?),
                "--" => {
                    game_arguments.extend(rest.cloned());
                    break;
                }
                other => return Err(format!("unexpected argument {other}")),
            }
        }
        let mode = match launcher {
            Some(launcher) => Mode::FrontEnd(Plan {
                launcher,
                arguments: game_arguments.clone(),
                client: client.ok_or("--client is required")?,
                ready_log: ready_log.ok_or("--ready-log is required")?,
                ready_line: ready_line.ok_or("--ready-line is required")?,
                button: button.ok_or("--button is required")?,
            }),
            None => Mode::StandIn {
                parent: parent.ok_or("--parent is required")?,
                game: game.ok_or("--game is required")?,
            },
        };
        Ok(Options {
            mode,
            log,
            game_arguments,
        })
    }

    fn start_error(what: &str) -> String {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        match cmdline::describe_start_error(code as u32) {
            Some(message) => format!("{what}: {message}"),
            None => format!("{what} (error {code})"),
        }
    }

    fn folder_of(path: &Path) -> Result<Vec<u16>, String> {
        let folder = path
            .parent()
            .ok_or_else(|| format!("{} has no folder", path.display()))?;
        Ok(wide(&folder.to_string_lossy()))
    }

    fn kill_on_close_job() -> Option<Owned> {
        let job = Owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
        if job.0.is_null() {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        (set != 0).then_some(job)
    }

    fn start_stand_in(path: &Path) -> Result<Owned, String> {
        let image = path.to_string_lossy().to_string();
        let application = wide(&image);
        let mut command_line = wide(&cmdline::build_command_line(&image, &[]));
        let directory = folder_of(path)?;
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
                CREATE_SUSPENDED,
                std::ptr::null(),
                directory.as_ptr(),
                &startup,
                &mut process,
            )
        };
        if started == 0 {
            return Err(start_error(&format!("Windows refused to start {}", path.display())));
        }
        unsafe { CloseHandle(process.hThread) };
        Ok(Owned(process.hProcess))
    }

    fn start_game(game: &Path, arguments: &[String], parent: HANDLE) -> Result<Owned, String> {
        let image = game.to_string_lossy().to_string();
        let application = wide(&image);
        let directory = folder_of(game)?;

        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut storage = vec![0u64; size.div_ceil(8)];
        let list = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
            return Err(start_error("Could not prepare the game's start"));
        }
        let parent_value = parent;
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
                &parent_value as *const HANDLE as *const core::ffi::c_void,
                std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if updated == 0 {
            let error = start_error("Could not give the game its stand-in parent");
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(error);
        }

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = list;
        let create = |flags| {
            let mut command_line =
                wide(&cmdline::build_command_line(&image, arguments));
            let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let started = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT | flags,
                    std::ptr::null(),
                    directory.as_ptr(),
                    &startup.StartupInfo,
                    &mut process,
                )
            };
            (started != 0).then_some(process)
        };
        let started = create(CREATE_BREAKAWAY_FROM_JOB).or_else(|| {
            let denied = std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_ACCESS_DENIED as i32);
            if denied {
                create(0)
            } else {
                None
            }
        });
        let outcome = match started {
            Some(process) => {
                unsafe { CloseHandle(process.hThread) };
                Ok(Owned(process.hProcess))
            }
            None => Err(start_error("Windows refused to start the game")),
        };
        unsafe { DeleteProcThreadAttributeList(list) };
        outcome
    }

    fn run_stand_in(parent: &Path, game_path: &Path, arguments: &[String], log: &mut Log) -> i32 {
        let job = kill_on_close_job();
        if job.is_none() {
            log.line("could not set up the job that ends the stand-in with the shim");
        }
        let stand_in = match start_stand_in(parent) {
            Ok(process) => process,
            Err(error) => {
                log.line(&error);
                return EXIT_STAND_IN;
            }
        };
        if let Some(job) = &job {
            if unsafe { AssignProcessToJobObject(job.0, stand_in.0) } == 0 {
                log.line("could not tie the stand-in to the shim's job");
            }
        }
        log.line(&format!("stand-in {} started suspended", parent.display()));

        let game = match start_game(game_path, arguments, stand_in.0) {
            Ok(process) => process,
            Err(error) => {
                log.line(&error);
                unsafe { TerminateProcess(stand_in.0, 1) };
                return EXIT_GAME;
            }
        };
        log.line(&format!("{} started", game_path.display()));

        unsafe { WaitForSingleObject(game.0, INFINITE) };
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(game.0, &mut code) };
        unsafe { TerminateProcess(stand_in.0, 0) };
        log.line(&format!("the game closed with exit code {code}"));
        code as i32
    }

    pub fn main() -> i32 {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let options = match parse_options(&arguments) {
            Ok(options) => options,
            Err(error) => {
                let log_path = arguments
                    .iter()
                    .position(|a| a == "--log")
                    .and_then(|i| arguments.get(i + 1));
                let mut log = Log(log_path.and_then(|p| create_log_file(Path::new(p)).ok()));
                log.line(&format!("bad arguments: {error}"));
                return EXIT_BAD_ARGUMENTS;
            }
        };
        let mut log = Log(options.log.as_deref().and_then(|p| create_log_file(p).ok()));
        match &options.mode {
            Mode::StandIn { parent, game } => run_stand_in(parent, game, &options.game_arguments, &mut log),
            Mode::FrontEnd(plan) => front_end::run(plan, &mut |text| log.line(text)),
        }
    }
}

#[cfg(windows)]
fn main() {
    std::process::exit(shim::main());
}

#[cfg(not(windows))]
fn main() {
    std::process::exit(1);
}
