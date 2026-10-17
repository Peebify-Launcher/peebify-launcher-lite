#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// ------------ Installer Entry Point ------------
// One exe does all the setup jobs. No arguments opens the install window, /S installs silently, /uninstall removes
// the launcher (from a temp copy of itself so it can delete its own folder), and pack and inspect are the build
// tools that scripts/build-installer.js uses to attach and check the payload.

mod consts;
mod ilog;
mod install;
mod msg;
mod paths;
mod payload;
mod prereqs;
mod swap;
mod ui;
mod win;

use std::path::{Path, PathBuf};

use ilog::ilog;
use install::{InstallOptions, UninstallOptions};
use msg::{Cancel, EngineError};
use payload::Payload;

mod exit {
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const PAYLOAD_CORRUPT: i32 = 2;
    pub const ALREADY_RUNNING: i32 = 3;
    pub const CANCELLED: i32 = 6;
    pub const BAD_USAGE: i32 = 64;
}

const KNOWN_FLAGS: &[&str] = &[
    "s",
    "silent",
    "uninstall",
    "second-stage",
    "choose",
    "remove-data",
    "remove-games",
    "no-desktop-shortcut",
    "launch",
    "vcredist",
    "help",
    "?",
];

const KNOWN_OPTIONS: &[&str] = &[
    "install-dir",
    "dir",
    "stub",
    "payload-dir",
    "out",
    "version",
    "main-binary",
    "file",
];

const USAGE_MODES: &[(&str, &str)] = &[
    ("(no arguments)", "Install with the setup window."),
    ("/S", "Install silently to the default location."),
    ("/uninstall", "Remove Peebify Launcher."),
];

const USAGE_OPTIONS: &[(&str, &str)] = &[
    ("--install-dir <path>", "Where to install (default: per-user Programs folder)."),
    ("--no-desktop-shortcut", "Do not create a desktop shortcut (silent install)."),
    ("--launch", "Start the launcher when a silent install finishes."),
    ("--vcredist", "Also install the Visual C++ runtime if it is missing."),
    ("--remove-data", "Also remove settings and launcher data (uninstall)."),
    ("--remove-games", "Also remove installed game files (uninstall)."),
];

const USAGE_EXIT_CODES: &[(&str, &str)] = &[
    ("0", "done"),
    ("1", "failed"),
    ("2", "damaged download"),
    ("3", "already running"),
    ("6", "cancelled"),
    ("64", "bad usage"),
];

fn usage(console: bool) -> String {
    let mut text = String::from("Peebify Launcher Setup\n");
    let mut section = |title: Option<&str>, rows: &[(&str, &str)]| {
        text.push('\n');
        if let Some(title) = title {
            text.push_str(title);
            text.push('\n');
        }
        for (name, what) in rows {
            if console {
                text.push_str(&format!("  {name:<24}{what}\n"));
            } else {
                text.push_str(&format!("{name}\n      {what}\n"));
            }
        }
    };
    section(None, USAGE_MODES);
    section(Some("Options"), USAGE_OPTIONS);
    let codes: Vec<String> = USAGE_EXIT_CODES
        .iter()
        .map(|(code, what)| format!("{code} {what}"))
        .collect();
    text.push_str("\nExit codes\n");
    if console {
        text.push_str("  ");
        text.push_str(&codes.join("   "));
    } else {
        text.push_str(&codes.join(", "));
    }
    text
}

fn main() {
    let dll_search_restricted = win::restrict_dll_search();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcommand = args.first().map(String::as_str);
    let is_tool = subcommand == Some("pack") || subcommand == Some("inspect");
    if !is_tool {
        ilog::init();
    }
    ilog!(
        "---- peebify-installer start: setup v{} args={args:?}",
        option_env!("PEEBIFY_VERSION").unwrap_or("0.0.0")
    );
    if !dll_search_restricted {
        ilog!("could not limit the DLL search path to System32");
    }

    let silent = flag(&args, "s") || flag(&args, "silent");
    let code = if let Err(bad) = validate_args(&args) {
        if is_tool {
            eprintln!("{bad}\n\n{}", usage(true));
        } else if silent {
            ilog!("bad usage: {bad}");
        } else {
            win::message_box_error("Peebify Launcher Setup", &format!("{bad}\n\n{}", usage(false)));
        }
        exit::BAD_USAGE
    } else if flag(&args, "help") || flag(&args, "?") {
        if is_tool {
            println!("{}", usage(true));
        } else if !silent {
            win::message_box_info("Peebify Launcher Setup", &usage(false));
        }
        exit::OK
    } else if subcommand == Some("pack") {
        pack_main(&args)
    } else if subcommand == Some("inspect") {
        inspect_main(&args)
    } else if flag(&args, "second-stage") {
        uninstall_main(&args, silent)
    } else {
        let Some(_instance) = win::claim_single_instance() else {
            ilog!("another copy of setup is already running");
            if !silent {
                win::message_box_error(
                    "Peebify Launcher Setup",
                    "Setup is already running. Finish or close the other window first.",
                );
            }
            exit_with(exit::ALREADY_RUNNING);
        };
        if flag(&args, "uninstall") {
            uninstall_main(&args, silent)
        } else if silent {
            silent_install_main(&args)
        } else {
            gui_main()
        }
    };
    exit_with(code);
}

fn exit_with(code: i32) -> ! {
    ilog!("---- peebify-installer exit: {code}");
    std::process::exit(code);
}

fn validate_args(args: &[String]) -> Result<(), String> {
    let mut i = 0;
    if matches!(
        args.first().map(String::as_str),
        Some("pack") | Some("inspect")
    ) {
        i = 1;
    }
    while i < args.len() {
        let token = norm(&args[i]);
        if KNOWN_OPTIONS.contains(&token.as_str()) {
            let missing = args.get(i + 1).is_none_or(|value| {
                value.trim().is_empty() || value.starts_with('-') || value.starts_with('/')
            });
            if missing {
                return Err(format!("{} needs a value", args[i]));
            }
            i += 2;
            continue;
        }
        if !KNOWN_FLAGS.contains(&token.as_str()) {
            return Err(format!("Unknown option: {}", args[i]));
        }
        i += 1;
    }
    Ok(())
}

fn engine_exit_code(result: Result<(), EngineError>, context: &str) -> i32 {
    match result {
        Ok(()) => exit::OK,
        Err(EngineError::Cancelled) => {
            ilog!("{context}: cancelled");
            exit::CANCELLED
        }
        Err(EngineError::Failed(e)) => {
            ilog!("{context} failed: {e}");
            exit::FAILED
        }
    }
}

fn norm(token: &str) -> String {
    token
        .trim_start_matches('/')
        .trim_start_matches("--")
        .trim_start_matches('-')
        .to_ascii_lowercase()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| norm(a) == name)
}

fn value_of(args: &[String], name: &str) -> Option<String> {
    let idx = args.iter().position(|a| norm(a) == name)?;
    args.get(idx + 1).cloned()
}

fn require_payload() -> Result<Payload, String> {
    match Payload::open_current_exe()? {
        Some(p) => {
            ilog!("payload: v{}", p.manifest.version);
            Ok(p)
        }
        None => Err("this installer has no payload attached, so the download is damaged".into()),
    }
}

fn gui_main() -> i32 {
    match Payload::open_current_exe() {
        Ok(Some(payload)) => {
            ilog!("payload: v{}", payload.manifest.version);
            match ui::wizard::run(payload.clone()) {
                Ok(code) => code,
                Err(e) => {
                    ilog!("setup: the window could not open ({e})");
                    install_without_window(&payload)
                }
            }
        }
        Ok(None) => {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            match dir {
                Some(dir) if dir.join(consts::INSTALL_MANIFEST_NAME).exists() => {
                    open_uninstaller(dir)
                }
                _ => {
                    win::message_box_error(
                        "Peebify Launcher Setup",
                        "This installer is damaged (no payload attached). Please download it again from peebify.net.",
                    );
                    exit::PAYLOAD_CORRUPT
                }
            }
        }
        Err(e) => {
            win::message_box_error("Peebify Launcher Setup", &e);
            exit::PAYLOAD_CORRUPT
        }
    }
}

fn silent_install_main(args: &[String]) -> i32 {
    let payload = match require_payload() {
        Ok(p) => p,
        Err(e) => {
            ilog!("silent install: {e}");
            return exit::PAYLOAD_CORRUPT;
        }
    };
    let opts = InstallOptions {
        install_dir: value_of(args, "install-dir")
            .map(PathBuf::from)
            .unwrap_or_else(consts::default_install_dir),
        desktop_shortcut: !flag(args, "no-desktop-shortcut"),
        install_vc_redist: flag(args, "vcredist"),
    };
    let sink = msg::log_sink();
    let code = engine_exit_code(
        install::perform_install(&payload, &opts, &Cancel::default(), &sink),
        "silent install",
    );
    if code != exit::OK {
        return code;
    }

    if flag(args, "launch") {
        if prereqs::webview2::is_installed() {
            let _ = install::launch_app(&opts.install_dir, &payload.manifest.main_binary);
        } else {
            ilog!("silent install: not launching, the WebView2 runtime is missing");
        }
    }
    exit::OK
}

const SETUP_TITLE: &str = "Peebify Launcher Setup";
const REMOVE_WITHOUT_WINDOW: &str = "Setup's window couldn't open on this PC. Remove Peebify Launcher anyway? Your settings and installed games are kept.";

fn install_without_window(payload: &Payload) -> i32 {
    if !win::message_box_yes_no(
        SETUP_TITLE,
        "Setup's window couldn't open on this PC. Install Peebify Launcher to the default \
         folder anyway?\n\nSetup tells you when it's done. To install without a window \
         later, run setup with /S.",
    ) {
        return exit::CANCELLED;
    }
    let opts = InstallOptions {
        install_dir: consts::default_install_dir(),
        desktop_shortcut: true,
        install_vc_redist: false,
    };
    let (sink, events) = std::sync::mpsc::channel();
    let result = install::perform_install(payload, &opts, &Cancel::default(), &sink);
    drop(sink);
    let warnings: Vec<String> = events
        .try_iter()
        .filter_map(|event| match event {
            msg::EngineEvent::Warning(warning) => Some(warning),
            _ => None,
        })
        .collect();
    match &result {
        Ok(()) => {
            let mut text = format!(
                "{} is installed in {}.",
                consts::PRODUCT_NAME,
                opts.install_dir.display()
            );
            for warning in &warnings {
                text.push_str("\n\n");
                text.push_str(warning);
            }
            win::message_box_info(SETUP_TITLE, &text);
            if prereqs::webview2::is_installed() {
                let _ = install::launch_app(&opts.install_dir, &payload.manifest.main_binary);
            }
        }
        Err(EngineError::Failed(e)) => {
            win::message_box_error(
                SETUP_TITLE,
                &format!("Setup could not install {}.\n\n{e}", consts::PRODUCT_NAME),
            );
        }
        Err(EngineError::Cancelled) => {}
    }
    engine_exit_code(result, "install without a window")
}

fn uninstall_without_window(install_dir: PathBuf) -> i32 {
    if !win::message_box_yes_no(consts::PRODUCT_NAME, REMOVE_WITHOUT_WINDOW) {
        return exit::CANCELLED;
    }
    let opts = UninstallOptions {
        install_dir,
        remove_data: false,
        remove_games: false,
    };
    match install::respawn_for_uninstall(&opts, install::SecondStage::Run) {
        Ok(()) => exit::OK,
        Err(e) => {
            ilog!("uninstall respawn failed: {e}");
            win::message_box_error(consts::PRODUCT_NAME, &e);
            exit::FAILED
        }
    }
}

fn uninstall_main(args: &[String], silent: bool) -> i32 {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let install_dir = match resolve_uninstall_dir(
        value_of(args, "dir").map(PathBuf::from),
        exe_dir,
        win::registered_install_location(),
        |dir| dir.join(consts::INSTALL_MANIFEST_NAME).exists(),
    ) {
        Ok(dir) => dir,
        Err(message) => {
            ilog!("uninstall refused: {message}");
            if !silent {
                win::message_box_error("Peebify Launcher", &message);
            }
            if flag(args, "second-stage") {
                install::schedule_self_delete();
            }
            return exit::FAILED;
        }
    };
    let opts = UninstallOptions {
        install_dir,
        remove_data: flag(args, "remove-data"),
        remove_games: flag(args, "remove-games"),
    };

    if flag(args, "second-stage") {
        let code = if silent {
            engine_exit_code(
                install::perform_uninstall(&opts, &msg::log_sink()),
                "uninstall",
            )
        } else {
            staged_uninstall(opts, flag(args, "choose"))
        };
        install::schedule_self_delete();
        code
    } else if silent {
        match install::respawn_for_uninstall(&opts, install::SecondStage::Silent) {
            Ok(()) => exit::OK,
            Err(e) => {
                ilog!("uninstall respawn failed: {e}");
                exit::FAILED
            }
        }
    } else {
        open_uninstaller(opts.install_dir)
    }
}

fn open_uninstaller(install_dir: PathBuf) -> i32 {
    let opts = UninstallOptions {
        install_dir: install_dir.clone(),
        remove_data: false,
        remove_games: false,
    };
    match install::respawn_for_uninstall(&opts, install::SecondStage::Choose) {
        Ok(()) => return exit::OK,
        Err(e) => ilog!("uninstall: could not stage the uninstaller ({e}), choosing in place"),
    }
    let launch = ui::uninstall::Launch {
        install_dir: install_dir.clone(),
        staged: false,
        start: ui::uninstall::Start::Choose,
    };
    match ui::uninstall::run(launch) {
        Ok(code) => code,
        Err(e) => {
            ilog!("uninstall: the window could not open ({e})");
            uninstall_without_window(install_dir)
        }
    }
}

fn staged_uninstall(opts: UninstallOptions, choose: bool) -> i32 {
    let launch = ui::uninstall::Launch {
        install_dir: opts.install_dir.clone(),
        staged: true,
        start: if choose {
            ui::uninstall::Start::Choose
        } else {
            ui::uninstall::Start::Remove(opts.clone())
        },
    };
    match ui::uninstall::run(launch) {
        Ok(code) => code,
        Err(e) => {
            ilog!("uninstall: the window could not open ({e}), running without it");
            let confirmed = !choose
                || win::message_box_yes_no(consts::PRODUCT_NAME, REMOVE_WITHOUT_WINDOW);
            if !confirmed {
                return exit::CANCELLED;
            }
            engine_exit_code(
                install::perform_uninstall(&opts, &msg::log_sink()),
                "uninstall",
            )
        }
    }
}

fn resolve_uninstall_dir(
    explicit: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    registered: Option<PathBuf>,
    has_manifest: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let is_registered = |dir: &Path| {
        registered
            .as_deref()
            .is_some_and(|r| paths::same_path(dir, r))
    };
    let dir = match explicit {
        Some(dir) => dir,
        None => match exe_dir {
            Some(dir) if has_manifest(&dir) || is_registered(&dir) => dir,
            other => registered
                .clone()
                .or(other)
                .ok_or_else(|| "Could not resolve the install directory.".to_string())?,
        },
    };
    let recognised = has_manifest(&dir) || is_registered(&dir) || paths::is_default_install_dir(&dir);
    if paths::validate_install_dir(&dir).is_err() || !recognised {
        return Err(format!(
            "{} is not a {} install folder, so nothing was removed.",
            dir.display(),
            consts::PRODUCT_NAME
        ));
    }
    Ok(dir)
}

fn tool_exit(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("{}", serde_json::json!({ "ok": false, "error": e }));
            exit::FAILED
        }
    }
}

fn pack_main(args: &[String]) -> i32 {
    let required = |name: &str| -> Result<String, String> {
        value_of(args, name).ok_or_else(|| format!("pack: missing --{name}"))
    };
    tool_exit((|| -> Result<(), String> {
        let stub = PathBuf::from(required("stub")?);
        let payload_dir = PathBuf::from(required("payload-dir")?);
        let out = PathBuf::from(required("out")?);
        let version = required("version")?;
        let main_binary =
            value_of(args, "main-binary").unwrap_or_else(|| consts::MAIN_BINARY.to_string());
        let manifest = payload::pack(&stub, &payload_dir, &out, &version, &main_binary)?;
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "out": out.display().to_string(),
                "version": manifest.version,
                "files": manifest.file_count,
                "estimatedSizeKb": manifest.estimated_size_kb,
            })
        );
        Ok(())
    })())
}

fn inspect_main(args: &[String]) -> i32 {
    tool_exit((|| -> Result<(), String> {
        let file = PathBuf::from(
            value_of(args, "file").ok_or_else(|| "inspect: missing --file".to_string())?,
        );
        let payload = Payload::open(&file)?
            .ok_or_else(|| "no payload footer found (plain stub?)".to_string())?;
        payload.verify()?;
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "verified": true,
                "manifest": serde_json::to_value(&payload.manifest).map_err(|e| e.to_string())?,
                "payloadBytes": payload.footer.payload_len,
            })
        );
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn options_need_a_value() {
        assert!(validate_args(&args(&["/S", "--install-dir", r"D:\Peebify"])).is_ok());
        assert!(validate_args(&args(&["pack", "--version", "1.2.3"])).is_ok());
        let missing = validate_args(&args(&["/S", "--install-dir"])).unwrap_err();
        assert!(missing.contains("--install-dir"));
        assert!(validate_args(&args(&["--install-dir", "--launch"])).is_err());
        assert!(validate_args(&args(&["--install-dir", "/S"])).is_err());
        assert!(validate_args(&args(&["--install-dir", ""])).is_err());
        assert!(validate_args(&args(&["--bogus"]))
            .unwrap_err()
            .starts_with("Unknown option: --bogus"));
        assert!(validate_args(&args(&["/passive"])).is_err());
    }

    #[test]
    fn usage_lists_every_option_in_both_layouts() {
        for console in [true, false] {
            let text = usage(console);
            for (name, what) in USAGE_MODES.iter().chain(USAGE_OPTIONS) {
                assert!(text.contains(name) && text.contains(what), "{name}");
            }
            assert!(text.contains("64 bad usage"));
        }
        assert!(usage(true).contains(&format!("  {:<24}Where to install", "--install-dir <path>")));
    }

    #[test]
    fn stray_setup_folder_falls_back_to_registered_install() {
        let real = p(r"D:\Games\Peebify Launcher");
        let dir = resolve_uninstall_dir(
            None,
            Some(p(r"D:\Stuff\Setups")),
            Some(real.clone()),
            |_| false,
        )
        .unwrap();
        assert_eq!(dir, real);
    }

    #[test]
    fn stray_setup_folder_without_registration_is_refused() {
        let result = resolve_uninstall_dir(None, Some(p(r"D:\Stuff\Setups")), None, |_| false);
        assert!(result.is_err());
    }

    #[test]
    fn exe_folder_with_manifest_is_used() {
        let own = p(r"D:\Apps\Peebify");
        let dir = resolve_uninstall_dir(
            None,
            Some(own.clone()),
            Some(p(r"D:\Games\Peebify Launcher")),
            |d| paths::same_path(d, &own),
        )
        .unwrap();
        assert_eq!(dir, own);
    }

    #[test]
    fn registered_folder_without_manifest_is_allowed() {
        let real = p(r"D:\Games\Peebify Launcher");
        let dir = resolve_uninstall_dir(
            None,
            Some(p(r"d:\games\peebify launcher\")),
            Some(real),
            |_| false,
        )
        .unwrap();
        assert_eq!(dir, p(r"d:\games\peebify launcher\"));
    }

    #[test]
    fn explicit_unrecognised_dir_is_refused() {
        let result = resolve_uninstall_dir(
            Some(p(r"D:\Stuff\Setups")),
            None,
            Some(p(r"D:\Games\Peebify Launcher")),
            |_| false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn explicit_drive_root_is_refused_even_with_manifest() {
        let result = resolve_uninstall_dir(Some(p(r"D:\")), None, None, |_| true);
        assert!(result.is_err());
    }

    #[test]
    fn default_install_dir_without_manifest_is_allowed() {
        let default = consts::default_install_dir();
        let dir = resolve_uninstall_dir(Some(default.clone()), None, None, |_| false).unwrap();
        assert_eq!(dir, default);
    }
}
