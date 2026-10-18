// ------------ Mod Loader Protocol ------------
// What the launcher and the mod loader exe agree on: command line options and their parsing, hook or inject mode,
// the ready and stop event names and the exit codes with a plain English reason for each.

use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const EXE_NAME: &str = "peebify-mod-loader.exe";

pub const EXIT_OK: i32 = 0;
pub const EXIT_BAD_ARGUMENTS: i32 = 2;
pub const EXIT_DLL_LOAD: i32 = 3;
pub const EXIT_DLL_EXPORTS: i32 = 4;
pub const EXIT_MODULE_CHECK: i32 = 5;
pub const EXIT_ALREADY_RUNNING: i32 = 10;
pub const EXIT_HOOK_FAILED: i32 = 11;
pub const EXIT_HOOK_MODULE_LOAD: i32 = 12;
pub const EXIT_HOOK_FOREIGN_MODULE: i32 = 13;
pub const EXIT_TARGET_TIMEOUT: i32 = 20;
pub const EXIT_STOPPED: i32 = 21;
pub const EXIT_NOT_MAPPED: i32 = 22;
pub const EXIT_TARGET_EXITED: i32 = 23;
pub const EXIT_INJECT_FAILED: i32 = 30;

pub const READY_EVENT_SUFFIX: &str = "-ready";
pub const STOP_EVENT_SUFFIX: &str = "-stop";
pub const STOP_ALL_EVENT: &str = "Local\\PeebifyModLoader-StopAll";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hook,
    Inject,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text.to_ascii_lowercase().as_str() {
            "hook" => Some(Mode::Hook),
            "inject" => Some(Mode::Inject),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Hook => "hook",
            Mode::Inject => "inject",
        }
    }
}

pub fn describe_exit(code: i32) -> String {
    match code {
        EXIT_OK => "the mod loader finished".to_string(),
        EXIT_BAD_ARGUMENTS => "the mod loader was started with bad arguments".to_string(),
        EXIT_DLL_LOAD => "3dmloader.dll could not be loaded. Security software may have blocked it. Delete it from the mod tools folder (shown under Mods > Setup) and launch again to get a fresh copy.".to_string(),
        EXIT_DLL_EXPORTS => "3dmloader.dll is too old for this Peebify. Delete it from the mod tools folder (shown under Mods > Setup) and launch again to get the current one.".to_string(),
        EXIT_MODULE_CHECK => "the mod library is missing or was changed after Peebify checked it. Security software may be interfering. Launch the game again and Peebify puts a fresh copy back.".to_string(),
        EXIT_ALREADY_RUNNING => "another mod loader is already running. Close the other game or loader first.".to_string(),
        EXIT_HOOK_FAILED => "Windows refused to install the mod hook. The loader log has the detail.".to_string(),
        EXIT_HOOK_MODULE_LOAD => "the mod library (d3d11.dll) could not be loaded to set up the hook. Security software may have blocked it. Delete it from the mod tools folder (shown under Mods > Setup) and launch again to get a fresh copy.".to_string(),
        EXIT_HOOK_FOREIGN_MODULE => "the d3d11.dll in the mod tools folder is not a 3DMigoto build, so it cannot be hooked. Delete it from the mod tools folder (shown under Mods > Setup) and launch again to get a fresh copy.".to_string(),
        EXIT_TARGET_TIMEOUT => "the game never started, so there was nothing to load mods into.".to_string(),
        EXIT_STOPPED => "the mod loader was stopped before the game started.".to_string(),
        EXIT_NOT_MAPPED => "the game started but never picked up the mod library.".to_string(),
        EXIT_TARGET_EXITED => "the game closed before the mod library was loaded.".to_string(),
        EXIT_INJECT_FAILED => "the mod library could not be written into the game. The loader log has the detail.".to_string(),
        other => format!("the mod loader exited with code {other}"),
    }
}

pub fn hook_exit_code(result: i32, hooked: bool) -> i32 {
    match result {
        0 if hooked => EXIT_OK,
        100 => EXIT_ALREADY_RUNNING,
        200 => EXIT_HOOK_MODULE_LOAD,
        300 => EXIT_HOOK_FOREIGN_MODULE,
        _ => EXIT_HOOK_FAILED,
    }
}

pub fn describe_hook_result(result: i32) -> &'static str {
    match result {
        0 => "the hook handle came back empty",
        100 => "another 3DMigoto loader holds Local\\3DMigotoLoader",
        200 => "the mod library could not be loaded",
        300 => "the mod library has no 3DMigoto entry point",
        400 => "the Windows hook could not be set up",
        _ => "an unknown hook error occurred",
    }
}

pub fn describe_inject_error(code: i32) -> &'static str {
    match code {
        100 => "the game process could not be opened",
        110 => "the mod library path does not exist",
        120 | 130 => "LoadLibraryW could not be resolved",
        200 => "memory could not be allocated in the game",
        300 => "the library path could not be written into the game",
        400 => "a thread could not be created in the game",
        500 => "the game did not finish loading the library in time",
        510 => "waiting for the game's load thread failed",
        600 => "the game rejected the mod library (LoadLibraryW returned null)",
        700 => "an unknown injection error occurred",
        _ => "an unexpected injection result",
    }
}

pub struct Options {
    pub dll: PathBuf,
    pub dll_sha256: Option<[u8; 32]>,
    pub module: PathBuf,
    pub module_sha256: Option<[u8; 32]>,
    pub target: String,
    pub mode: Mode,
    pub timeout: Duration,
    pub event: String,
    pub log: Option<PathBuf>,
}

pub fn log_argument(arguments: &[String]) -> Option<PathBuf> {
    arguments
        .iter()
        .position(|argument| argument == "--log")
        .and_then(|index| arguments.get(index + 1))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

pub fn sha256_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn checksum_argument(text: String) -> Result<[u8; 32], String> {
    parse_sha256(&text).ok_or_else(|| format!("bad checksum {text}"))
}

pub fn parse_options(arguments: &[String]) -> Result<Options, String> {
    let mut dll = None;
    let mut dll_sha256 = None;
    let mut module = None;
    let mut module_sha256 = None;
    let mut target = None;
    let mut mode = None;
    let mut timeout = None;
    let mut event = None;
    let mut log = None;

    let mut arguments = arguments.iter().cloned();
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} needs a value"))
        };
        match argument.as_str() {
            "--dll" => dll = Some(PathBuf::from(value()?)),
            "--dll-sha256" => dll_sha256 = Some(checksum_argument(value()?)?),
            "--module" => module = Some(PathBuf::from(value()?)),
            "--module-sha256" => module_sha256 = Some(checksum_argument(value()?)?),
            "--target" => target = Some(value()?),
            "--mode" => {
                let text = value()?;
                mode = Some(Mode::parse(&text).ok_or_else(|| format!("unknown mode {text}"))?);
            }
            "--timeout" => {
                let text = value()?;
                let seconds: u64 = text.parse().map_err(|_| format!("bad timeout {text}"))?;
                timeout = Some(Duration::from_secs(seconds));
            }
            "--event" => event = Some(value()?),
            "--log" => log = Some(PathBuf::from(value()?)),
            other => return Err(format!("unexpected argument {other}")),
        }
    }

    Ok(Options {
        dll: dll.ok_or("--dll is required")?,
        dll_sha256,
        module: module.ok_or("--module is required")?,
        module_sha256,
        target: target.ok_or("--target is required")?,
        mode: mode.ok_or("--mode is required")?,
        timeout: timeout.ok_or("--timeout is required")?,
        event: event.ok_or("--event is required")?,
        log,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Gone,
    Expired,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tick {
    pub appeared: Vec<u32>,
    pub exited: Vec<u32>,
    pub presence: Presence,
}

pub struct TargetWatch {
    grace: Duration,
    seen: Vec<(u32, Instant)>,
    gone_since: Option<Instant>,
}

impl TargetWatch {
    pub fn new(grace: Duration, first: u32, now: Instant) -> TargetWatch {
        TargetWatch {
            grace,
            seen: vec![(first, now)],
            gone_since: None,
        }
    }

    pub fn observe(&mut self, pids: &[u32], now: Instant) -> Tick {
        let exited: Vec<u32> = self
            .seen
            .iter()
            .map(|&(pid, _)| pid)
            .filter(|pid| !pids.contains(pid))
            .collect();
        self.seen.retain(|(pid, _)| pids.contains(pid));
        let mut appeared = Vec::new();
        for &pid in pids {
            if !self.seen.iter().any(|&(known, _)| known == pid) {
                self.seen.push((pid, now));
                appeared.push(pid);
            }
        }
        let presence = if !pids.is_empty() {
            self.gone_since = None;
            Presence::Present
        } else {
            let since = *self.gone_since.get_or_insert(now);
            if now.duration_since(since) >= self.grace {
                Presence::Expired
            } else {
                Presence::Gone
            }
        };
        Tick {
            appeared,
            exited,
            presence,
        }
    }

    pub fn seen_for(&self, pid: u32, now: Instant) -> Duration {
        self.seen
            .iter()
            .find(|&&(known, _)| known == pid)
            .map(|&(_, since)| now.duration_since(since))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn relaunch_is_followed_to_the_new_pid() {
        let start = Instant::now();
        let mut watch = TargetWatch::new(Duration::from_secs(20), 40716, start);
        let tick = watch.observe(&[], start + Duration::from_millis(400));
        assert_eq!(tick.exited, vec![40716]);
        assert_eq!(tick.presence, Presence::Gone);
        let tick = watch.observe(&[166972], start + Duration::from_millis(900));
        assert_eq!(tick.appeared, vec![166972]);
        assert!(tick.exited.is_empty());
        assert_eq!(tick.presence, Presence::Present);
        assert_eq!(
            watch.seen_for(166972, start + Duration::from_millis(1900)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn a_returning_target_resets_the_grace() {
        let start = Instant::now();
        let mut watch = TargetWatch::new(Duration::from_secs(20), 7, start);
        watch.observe(&[], start + Duration::from_secs(1));
        watch.observe(&[8], start + Duration::from_secs(15));
        watch.observe(&[], start + Duration::from_secs(16));
        assert_eq!(
            watch.observe(&[], start + Duration::from_secs(30)).presence,
            Presence::Gone
        );
        assert_eq!(
            watch.observe(&[], start + Duration::from_secs(36)).presence,
            Presence::Expired
        );
    }

    #[test]
    fn one_of_two_targets_exiting_keeps_the_watch_present() {
        let start = Instant::now();
        let mut watch = TargetWatch::new(Duration::from_secs(20), 1, start);
        watch.observe(&[1, 2], start);
        let tick = watch.observe(&[2], start + Duration::from_secs(1));
        assert_eq!(tick.exited, vec![1]);
        assert_eq!(tick.presence, Presence::Present);
        assert_eq!(watch.seen_for(1, start + Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn hook_results_map_to_their_own_exit_codes() {
        assert_eq!(hook_exit_code(0, true), EXIT_OK);
        assert_eq!(hook_exit_code(0, false), EXIT_HOOK_FAILED);
        assert_eq!(hook_exit_code(100, true), EXIT_ALREADY_RUNNING);
        assert_eq!(hook_exit_code(200, false), EXIT_HOOK_MODULE_LOAD);
        assert_eq!(hook_exit_code(300, false), EXIT_HOOK_FOREIGN_MODULE);
        assert_eq!(hook_exit_code(400, false), EXIT_HOOK_FAILED);
        assert_eq!(hook_exit_code(7, false), EXIT_HOOK_FAILED);
    }

    #[test]
    fn parse_errors_keep_their_message_and_log_path() {
        let arguments = strings(&["--log", "C:\\logs\\mod-loader.log", "--timeout"]);
        let error = parse_options(&arguments).err().unwrap();
        assert_eq!(error, "--timeout needs a value");
        assert_eq!(
            log_argument(&arguments),
            Some(PathBuf::from("C:\\logs\\mod-loader.log"))
        );
        assert_eq!(log_argument(&strings(&["--log"])), None);
        let error = parse_options(&strings(&["--bogus"])).err().unwrap();
        assert_eq!(error, "unexpected argument --bogus");
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn dll_checksum_parses_and_round_trips() {
        let mut arguments = strings(&[
            "--dll", "a.dll", "--module", "d3d11.dll", "--target", "Game.exe", "--mode",
            "hook", "--timeout", "30", "--event", "Local\\E", "--dll-sha256",
        ]);
        arguments.push(ABC.to_uppercase());
        let digest = parse_options(&arguments).unwrap().dll_sha256.unwrap();
        assert_eq!(digest[..3], [0xba, 0x78, 0x16]);
        assert_eq!(sha256_hex(&digest), ABC);

        arguments.pop();
        arguments.push("abc".to_string());
        assert_eq!(parse_options(&arguments).err().unwrap(), "bad checksum abc");
    }

    #[test]
    fn module_checksum_parses_apart_from_the_dll_checksum() {
        let mut arguments = strings(&[
            "--dll", "a.dll", "--module", "d3d11.dll", "--target", "Game.exe", "--mode",
            "inject", "--timeout", "30", "--event", "Local\\E", "--module-sha256", ABC,
        ]);
        let options = parse_options(&arguments).unwrap();
        assert!(options.dll_sha256.is_none());
        assert_eq!(sha256_hex(&options.module_sha256.unwrap()), ABC);

        arguments.pop();
        arguments.push(String::new());
        assert_eq!(parse_options(&arguments).err().unwrap(), "bad checksum ");
        arguments.pop();
        assert_eq!(
            parse_options(&arguments).err().unwrap(),
            "--module-sha256 needs a value"
        );
    }

    #[test]
    fn malformed_checksums_are_refused() {
        assert!(parse_sha256(&ABC[1..]).is_none());
        assert!(parse_sha256(&format!("{ABC}0")).is_none());
        assert!(parse_sha256(&format!("+{}", &ABC[1..])).is_none());
        assert!(parse_sha256(&format!("g{}", &ABC[1..])).is_none());
        assert!(parse_sha256(&"é".repeat(32)).is_none());
    }
}
