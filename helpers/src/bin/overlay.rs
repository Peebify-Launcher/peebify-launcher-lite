#![cfg_attr(not(windows), allow(unused))]
#![cfg_attr(windows, windows_subsystem = "windows")]

// ------------ Overlay Helper ------------
// peebify-overlay-helper.exe is a small background process that registers the overlay hotkeys while the game is in
// front and tells the launcher when one is pressed. It stops on request or when the launcher exits. Windows only.

#[cfg(windows)]
fn main() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use peebify_helpers::common::section::wait_for_message;
    use peebify_helpers::overlay::hotkeys::{self, Registrations};
    use peebify_helpers::overlay::section::{watchdog, SectionView, Signal};
    use peebify_helpers::overlay::shared::{
        HELPER_PROTOCOL, SIGNAL_TO_HELPER, SIGNAL_TO_LAUNCHER, STATUS_RUNNING, STATUS_STARTING,
    };
    use peebify_helpers::overlay::telemetry::{log_line, rotate_log};

    const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

    let launcher_pid: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);

    rotate_log();
    log_line(&format!(
        "session: helper {} protocol {HELPER_PROTOCOL:#x} pid {} launcher pid {launcher_pid}",
        option_env!("PEEBIFY_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        std::process::id()
    ));

    let section = match SectionView::open() {
        Ok(section) => section,
        Err(code) => {
            log_line(&format!(
                "the launcher's shared section is gone (error {code}), so the session already ended"
            ));
            std::process::exit(2);
        }
    };
    let shared = section.shared();
    if !shared.is_valid() || shared.stop.load(Ordering::Acquire) != 0 {
        log_line("the launcher already ended this session");
        std::process::exit(2);
    }
    shared
        .helper_protocol
        .store(HELPER_PROTOCOL, Ordering::Release);
    shared.status.store(STATUS_STARTING, Ordering::Release);

    let to_launcher = match Signal::open_or_create(SIGNAL_TO_LAUNCHER) {
        Ok(signal) => signal,
        Err(code) => {
            shared.fail(&format!("could not create the launcher signal ({code})"));
            std::process::exit(2);
        }
    };
    let from_launcher = Signal::open_or_create(SIGNAL_TO_HELPER).ok();

    let mut registrations = Registrations::default();

    hotkeys::allow_foreground(launcher_pid);
    registrations.sync(shared);
    shared.status.store(STATUS_RUNNING, Ordering::Release);
    log_line(&format!(
        "running (launcher pid {launcher_pid}, hotkeys {})",
        if registrations.any_registered() {
            "registered"
        } else {
            "none bound yet"
        }
    ));

    let launcher = watchdog::open(launcher_pid);

    loop {
        if shared.stop.load(Ordering::Acquire) != 0 {
            log_line("stop requested by the launcher");
            break;
        }
        if watchdog::gone(&launcher) {
            log_line("the launcher exited, shutting down");
            break;
        }

        let timeout = HEARTBEAT_INTERVAL.as_millis() as u32;
        if let Some(signal) = from_launcher.as_ref() {
            signal.wait_or_message(timeout);
        } else {
            wait_for_message(timeout);
        }

        registrations.sync(shared);
        if hotkeys::pump(shared) {
            hotkeys::allow_foreground(launcher_pid);
            to_launcher.raise();
        }

        shared.sample_seq.fetch_add(1, Ordering::Release);
    }

    registrations.unregister_all();
    log_line("exiting");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The Peebify overlay helper only runs on Windows.");
    std::process::exit(1);
}
