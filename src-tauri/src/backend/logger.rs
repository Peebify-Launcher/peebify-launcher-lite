// ------------ Launcher Logger ------------
// Writes the launch log and the crash log in the logs folder. Old logs are pruned, the Windows user folder is
// hidden in paths, a note is added when the last session did not exit cleanly, and the UI can send log lines too.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde_json::Value;

const KEEP_PREVIOUS_LAUNCH_LOGS: usize = 4;
const MAX_LAUNCH_LOGS_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_CRASH_LOG_BYTES: u64 = 1024 * 1024;
const MAX_LAUNCH_LOG_BYTES: u64 = 8 * 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_PENDING_LINES: usize = 200;
const SESSION_MARKER: &str = ".session-open";
const PROFILE_TOKEN: &str = "%USERPROFILE%";
const MIN_PROFILE_PREFIX_CHARS: usize = 4;

struct LaunchWriter {
    writer: Option<BufWriter<File>>,
    written: u64,
    last_flush: Instant,
}

struct Sink {
    launch_file: PathBuf,
    crash_file: PathBuf,
    session_marker: PathBuf,
    launch: Mutex<LaunchWriter>,
}

static SINK: OnceLock<Sink> = OnceLock::new();
static BOOT: OnceLock<Instant> = OnceLock::new();
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());
static SESSION_ENDED: AtomicBool = AtomicBool::new(false);
static PENDING_IN_CRASH_FILE: AtomicUsize = AtomicUsize::new(0);

fn rank(level: &str) -> u8 {
    match level {
        "debug" => 10,
        "info" => 20,
        "warn" => 30,
        "error" => 40,
        "crash" => 50,
        _ => 20,
    }
}

fn level_name(rank: u8) -> &'static str {
    match rank {
        0..=10 => "debug",
        11..=20 => "info",
        21..=30 => "warn",
        31..=40 => "error",
        _ => "crash",
    }
}

fn env_rank() -> Option<u8> {
    static ENV_RANK: OnceLock<Option<u8>> = OnceLock::new();
    *ENV_RANK.get_or_init(|| {
        let env = std::env::var("PEEBIFY_LOG_LEVEL").ok()?.to_lowercase();
        matches!(env.as_str(), "debug" | "info" | "warn" | "error" | "crash").then(|| rank(&env))
    })
}

fn effective_rank(env: Option<u8>) -> u8 {
    env.unwrap_or_else(|| rank("debug"))
}

fn min_rank() -> u8 {
    effective_rank(env_rank())
}

fn should_log(level: &str) -> bool {
    rank(level) >= min_rank()
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn profile_prefix() -> Option<&'static str> {
    static PREFIX: OnceLock<Option<String>> = OnceLock::new();
    PREFIX
        .get_or_init(|| {
            std::env::var("USERPROFILE")
                .ok()
                .map(|p| p.trim().trim_end_matches(['\\', '/']).to_string())
                .filter(|p| p.chars().count() >= MIN_PROFILE_PREFIX_CHARS)
        })
        .as_deref()
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower = haystack.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        let at_boundary = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-')));
        out.push_str(&haystack[from..start]);
        out.push_str(if at_boundary { with } else { &haystack[start..end] });
        from = end;
    }
    out.push_str(&haystack[from..]);
    out
}

fn redact_profile(line: &str, prefix: &str) -> String {
    let escaped = prefix.replace('\\', "\\\\");
    let forward = prefix.replace('\\', "/");
    let mut out = replace_ascii_case_insensitive(line, &escaped, PROFILE_TOKEN);
    out = replace_ascii_case_insensitive(&out, prefix, PROFILE_TOKEN);
    if forward != prefix {
        out = replace_ascii_case_insensitive(&out, &forward, PROFILE_TOKEN);
    }
    out
}

fn format_line(level: &str, message: &str) -> String {
    let line = format!("[{}] [{}] {}\n", iso_now(), level.to_uppercase(), message);
    match profile_prefix() {
        Some(prefix) => redact_profile(&line, prefix),
        None => line,
    }
}

pub fn mark_boot() {
    let _ = BOOT.set(Instant::now());
}

pub fn since_boot_ms() -> u128 {
    BOOT.get().map(|t| t.elapsed().as_millis()).unwrap_or(0)
}

pub fn init(logs_dir: &Path, boot_launch: Option<bool>) {
    if SINK.get().is_some() {
        return;
    }
    let _ = std::fs::create_dir_all(logs_dir);

    let stamp = iso_now().replace(':', "-");
    let launch_name = format!("launch-{stamp}.log");
    {
        let dir = logs_dir.to_path_buf();
        let current = launch_name.clone();
        let _ = std::thread::Builder::new()
            .name("log-prune".to_string())
            .spawn(move || prune_old_launch_logs(&dir, &current));
    }
    let session_marker = logs_dir.join(SESSION_MARKER);
    let unclean_previous = std::fs::read_to_string(&session_marker).ok();
    let sink = Sink {
        launch_file: logs_dir.join(&launch_name),
        crash_file: logs_dir.join("crashes.log"),
        session_marker,
        launch: Mutex::new(LaunchWriter {
            writer: None,
            written: 0,
            last_flush: Instant::now(),
        }),
    };
    if SINK.set(sink).is_err() {
        return;
    }
    let Some(sink) = SINK.get() else {
        return;
    };
    let _ = std::fs::write(&sink.session_marker, &launch_name);

    let mut opening = vec![format_line("info", &session_header(boot_launch))];
    if let Some(previous) = unclean_previous {
        opening.push(format_line("warn", &unclean_exit_note(previous.trim())));
    }
    let pending = std::mem::take(&mut *PENDING.lock());
    {
        let mut state = sink.launch.lock();
        for line in opening.iter().chain(pending.iter()) {
            append_launch(sink, &mut state, line);
        }
        flush_launch(&mut state);
    }
    start_flush_ticker();
}

fn unclean_exit_note(previous_log: &str) -> String {
    let base = "previous session ended without a clean exit (crash, kill, or Windows shutdown)";
    if previous_log.is_empty() {
        base.to_string()
    } else {
        format!("{base}, its log is {previous_log}")
    }
}

fn session_header(boot_launch: Option<bool>) -> String {
    let webview = tauri::webview_version().unwrap_or_else(|_| "unknown".to_string());
    let boot = match boot_launch {
        Some(boot) => format!(", boot launch {boot}"),
        None => String::new(),
    };
    format!(
        "session start: Peebify Launcher {} ({}), pid {}, Windows {}, WebView2 {webview}{boot}, log level {}, UTC offset {}",
        env!("CARGO_PKG_VERSION"),
        super::BUILD_TYPE,
        std::process::id(),
        windows_build(),
        level_name(min_rank()),
        chrono::Local::now().offset()
    )
}

fn windows_build() -> String {
    let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return "unknown".to_string();
    };
    let build = key
        .get_value::<String, _>("CurrentBuildNumber")
        .or_else(|_| key.get_value::<String, _>("CurrentBuild"))
        .unwrap_or_else(|_| "unknown".to_string());
    match key.get_value::<u32, _>("UBR") {
        Ok(ubr) => format!("{build}.{ubr}"),
        Err(_) => build,
    }
}

fn start_flush_ticker() {
    let _ = std::thread::Builder::new()
        .name("log-flush".to_string())
        .spawn(|| loop {
            std::thread::sleep(FLUSH_INTERVAL);
            let Some(sink) = SINK.get() else {
                return;
            };
            if let Some(mut state) = sink.launch.try_lock() {
                if state.last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush_launch(&mut state);
                }
            }
        });
}

pub fn end_session(reason: &str) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if !SESSION_ENDED.swap(true, Ordering::SeqCst) {
        write_line(false, "info", &format!("session end ({reason})"));
        let _ = std::fs::remove_file(&sink.session_marker);
    }
    flush();
}

struct LaunchLogFile {
    name: String,
    modified: SystemTime,
    len: u64,
}

fn launch_log_group(name: &str) -> &str {
    name.strip_suffix(".1.log")
        .or_else(|| name.strip_suffix(".log"))
        .unwrap_or(name)
}

fn stale_launch_logs(files: Vec<LaunchLogFile>) -> Vec<String> {
    let mut groups: BTreeMap<String, Vec<LaunchLogFile>> = BTreeMap::new();
    for file in files {
        groups
            .entry(launch_log_group(&file.name).to_string())
            .or_default()
            .push(file);
    }
    let mut stale = Vec::new();
    let mut kept_bytes: u64 = 0;
    let mut groups: Vec<Vec<LaunchLogFile>> = groups.into_values().collect();
    groups.sort_by_key(|group| group.iter().map(|f| f.modified).max());
    for (index, group) in groups.into_iter().rev().enumerate() {
        let bytes: u64 = group.iter().map(|f| f.len).sum();
        let keep = index < KEEP_PREVIOUS_LAUNCH_LOGS
            && kept_bytes.saturating_add(bytes) <= MAX_LAUNCH_LOGS_TOTAL_BYTES;
        if keep {
            kept_bytes += bytes;
        } else {
            stale.extend(group.into_iter().map(|f| f.name));
        }
    }
    stale
}

fn prune_old_launch_logs(dir: &Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let current_group = launch_log_group(current).to_string();
    let files: Vec<LaunchLogFile> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("launch-") && name.ends_with(".log")) {
                return None;
            }
            if launch_log_group(&name) == current_group {
                return None;
            }
            let meta = e.metadata().ok()?;
            Some(LaunchLogFile {
                name,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: meta.len(),
            })
        })
        .collect();
    for stale in stale_launch_logs(files) {
        let _ = std::fs::remove_file(dir.join(stale));
    }
}

pub fn append(level: &str, message: &str) {
    let level = level.to_lowercase();
    if level == "crash" {
        crash(message);
        return;
    }
    if level != "error" && !should_log(&level) {
        return;
    }
    write_line(false, &level, message);
}

fn fallback_crash_file() -> Option<PathBuf> {
    super::user_data_dir_before_setup().map(|dir| dir.join("logs").join("crashes.log"))
}

fn crash(message: &str) {
    let crash_file = match SINK.get() {
        Some(sink) => Some(sink.crash_file.clone()),
        None => fallback_crash_file(),
    };
    if let Some(crash_file) = crash_file {
        if let Ok(meta) = std::fs::metadata(&crash_file) {
            if meta.len() > MAX_CRASH_LOG_BYTES {
                let rotated = PathBuf::from(format!("{}.1", crash_file.display()));
                let _ = std::fs::rename(&crash_file, rotated);
            }
        }
    }
    write_line(true, "crash", message);
}

fn append_crash_file(path: &Path, text: &str) {
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.flush()));
    if let Err(e) = result {
        eprintln!("[logger] failed to write crash log: {e}");
    }
}

fn unbound_crash_text(pending: &[String], already_written: usize, line: &str) -> String {
    let mut text = pending[already_written.min(pending.len())..].concat();
    text.push_str(line);
    text
}

fn write_unbound_crash(line: String) {
    let text = match PENDING.try_lock_for(Duration::from_millis(250)) {
        Some(mut pending) => {
            let already = PENDING_IN_CRASH_FILE.load(Ordering::SeqCst);
            let text = unbound_crash_text(&pending, already, &line);
            if pending.len() < MAX_PENDING_LINES {
                pending.push(line);
            }
            PENDING_IN_CRASH_FILE.store(pending.len(), Ordering::SeqCst);
            text
        }
        None => line,
    };
    let Some(path) = fallback_crash_file() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    append_crash_file(&path, &text);
}

fn hold_pending(line: String) {
    let mut pending = PENDING.lock();
    if pending.len() < MAX_PENDING_LINES {
        pending.push(line);
    }
}

fn write_line(to_crash_file: bool, level: &str, message: &str) {
    let line = format_line(level, message);
    let Some(sink) = SINK.get() else {
        eprint!("[logger-unbound] {line}");
        if to_crash_file {
            write_unbound_crash(line);
        } else {
            hold_pending(line);
        }
        return;
    };

    if to_crash_file {
        if let Some(mut state) = sink.launch.try_lock_for(Duration::from_millis(250)) {
            let summary = line.lines().next().unwrap_or_default();
            append_launch(sink, &mut state, &format!("{summary} (details in crashes.log)\n"));
            flush_launch(&mut state);
        }
        append_crash_file(&sink.crash_file, &line);
        return;
    }

    let mut state = sink.launch.lock();
    append_launch(sink, &mut state, &line);
    let due = state.last_flush.elapsed() >= FLUSH_INTERVAL;
    if due || matches!(level, "warn" | "error") {
        flush_launch(&mut state);
    }
}

fn append_launch(sink: &Sink, state: &mut LaunchWriter, line: &str) {
    let bytes = line.as_bytes();
    if state.written + bytes.len() as u64 > MAX_LAUNCH_LOG_BYTES {
        rotate_launch(sink, state);
    }
    if state.writer.is_none() {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sink.launch_file)
        {
            Ok(file) => {
                state.written = file.metadata().map(|m| m.len()).unwrap_or(0);
                state.writer = Some(BufWriter::new(file));
            }
            Err(e) => {
                eprintln!("[logger] failed to open log: {e}");
                return;
            }
        }
    }

    let Some(writer) = state.writer.as_mut() else {
        return;
    };
    if let Err(e) = writer.write_all(bytes) {
        eprintln!("[logger] failed to write log: {e}");
        return;
    }
    state.written += bytes.len() as u64;
}

fn flush_launch(state: &mut LaunchWriter) {
    if let Some(writer) = state.writer.as_mut() {
        let _ = writer.flush();
    }
    state.last_flush = Instant::now();
}

fn rotate_launch(sink: &Sink, state: &mut LaunchWriter) {
    state.writer = None;
    let rotated = sink.launch_file.with_extension("1.log");
    let _ = std::fs::rename(&sink.launch_file, rotated);
    state.written = 0;
}

pub fn flush() {
    if let Some(sink) = SINK.get() {
        flush_launch(&mut sink.launch.lock());
    }
}

pub fn handle_log_message_channel(args: &[Value]) -> Value {
    let payload = args.first().cloned().unwrap_or(Value::Null);
    let entries: Vec<Value> = match payload {
        Value::Array(list) => list,
        other => vec![other],
    };
    for entry in entries {
        if entry.is_null() {
            continue;
        }
        let raw_level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("");
        let level = if matches!(raw_level, "debug" | "info" | "warn" | "error") {
            raw_level
        } else {
            "info"
        };
        append(level, &renderer_line(&entry, chrono::Utc::now().timestamp_millis()));
    }
    super::ok_response()
}

const MAX_RENDERER_MESSAGE_BYTES: usize = 4096;
const MAX_WINDOW_TAG_CHARS: usize = 32;
const LATE_RENDERER_LINE_MS: i64 = 1000;

fn renderer_line(entry: &Value, now_ms: i64) -> String {
    let message = match entry.get("message") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let mut message = truncate_on_char_boundary(message, MAX_RENDERER_MESSAGE_BYTES);
    let tag: String = entry
        .get("w")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(MAX_WINDOW_TAG_CHARS)
        .collect();
    if let Some(t) = entry.get("t").and_then(|v| v.as_i64()) {
        let late = now_ms.saturating_sub(t);
        if late >= LATE_RENDERER_LINE_MS {
            message.push_str(&format!(" (+{late}ms)"));
        }
    }
    if tag.is_empty() {
        format!("[renderer] {message}")
    } else {
        format!("[renderer:{tag}] {message}")
    }
}

fn truncate_on_char_boundary(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(" [truncated]");
    text
}

const MAX_RPC_ERROR_BYTES: usize = 512;
const SLOW_RPC: Duration = Duration::from_secs(2);
const LOG_MESSAGE_CHANNEL: &str = "log-message";

static RPC_REPLY_ERRORS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn reply_error(reply: &Value) -> Option<&str> {
    if reply.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    if reply.get("cancelled").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(
        reply
            .get("error")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .unwrap_or("(no detail)"),
    )
}

fn note_reply_error(seen: &mut HashMap<String, String>, channel: &str, error: Option<&str>) -> bool {
    match error {
        Some(error) => seen.insert(channel.to_owned(), error.to_owned()).as_deref() != Some(error),
        None => {
            seen.remove(channel);
            false
        }
    }
}

pub fn record_rpc(channel: &str, elapsed: Duration, result: &Result<Value, String>) {
    if channel == LOG_MESSAGE_CHANNEL {
        return;
    }
    let ms = elapsed.as_millis();
    match result {
        Err(e) => log::warn!(
            "rpc {channel} failed after {ms} ms: {}",
            truncate_on_char_boundary(e.clone(), MAX_RPC_ERROR_BYTES)
        ),
        Ok(reply) => {
            let error = reply_error(reply);
            let fresh = note_reply_error(
                &mut RPC_REPLY_ERRORS.get_or_init(Mutex::default).lock(),
                channel,
                error,
            );
            if let (true, Some(error)) = (fresh, error) {
                log::info!(
                    "rpc {channel} returned an error: {}",
                    truncate_on_char_boundary(error.to_owned(), MAX_RPC_ERROR_BYTES)
                );
            }
        }
    }
    if elapsed >= SLOW_RPC {
        log::debug!("rpc {channel} took {ms} ms");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reply_error_reads_only_real_failures() {
        assert_eq!(reply_error(&json!({ "success": true })), None);
        assert_eq!(reply_error(&json!({ "value": 1 })), None);
        assert_eq!(reply_error(&json!(null)), None);
        assert_eq!(
            reply_error(&json!({ "success": false, "cancelled": true, "error": "x" })),
            None
        );
        assert_eq!(
            reply_error(&json!({ "success": false, "error": "Wrong password." })),
            Some("Wrong password.")
        );
        assert_eq!(reply_error(&json!({ "success": false, "error": "" })), Some("(no detail)"));
        assert_eq!(reply_error(&json!({ "success": false })), Some("(no detail)"));
    }

    #[test]
    fn repeated_reply_errors_are_logged_once() {
        let mut seen = HashMap::new();
        assert!(note_reply_error(&mut seen, "get-news-data", Some("You are not signed in.")));
        assert!(!note_reply_error(&mut seen, "get-news-data", Some("You are not signed in.")));
        assert!(note_reply_error(&mut seen, "get-news-data", Some("HTTP 500")));
        assert!(note_reply_error(&mut seen, "other", Some("HTTP 500")));
        assert!(!note_reply_error(&mut seen, "get-news-data", None));
        assert!(note_reply_error(&mut seen, "get-news-data", Some("HTTP 500")));
    }

    #[test]
    fn renderer_line_tags_the_window() {
        let entry = json!({ "level": "info", "message": "hello", "t": 1000, "w": "overlay" });
        assert_eq!(renderer_line(&entry, 1000), "[renderer:overlay] hello");
    }

    #[test]
    fn renderer_line_without_tag_keeps_plain_prefix() {
        let entry = json!({ "level": "info", "message": "hello" });
        assert_eq!(renderer_line(&entry, 0), "[renderer] hello");
    }

    #[test]
    fn renderer_line_sanitizes_the_tag() {
        let entry = json!({ "message": "x", "w": "over]lay hud\n" });
        assert_eq!(renderer_line(&entry, 0), "[renderer:overlayhud] x");
    }

    #[test]
    fn renderer_line_marks_late_entries() {
        let entry = json!({ "message": "queued", "t": 1000, "w": "main" });
        assert_eq!(renderer_line(&entry, 1999), "[renderer:main] queued");
        assert_eq!(renderer_line(&entry, 4500), "[renderer:main] queued (+3500ms)");
    }

    #[test]
    fn renderer_line_truncates_long_messages_on_a_char_boundary() {
        let long = format!("a{}", "é".repeat(4000));
        let entry = json!({ "message": long });
        let line = renderer_line(&entry, 0);
        let body = line.strip_prefix("[renderer] ").unwrap();
        let kept = body.strip_suffix(" [truncated]").unwrap();
        assert!(kept.len() <= MAX_RENDERER_MESSAGE_BYTES);
        assert!(kept.len() >= MAX_RENDERER_MESSAGE_BYTES - 1);
        assert!(kept.starts_with('a'));
    }

    #[test]
    fn short_messages_are_not_truncated() {
        assert_eq!(truncate_on_char_boundary("abc".to_owned(), 4096), "abc");
    }

    #[test]
    fn profile_paths_are_redacted_in_every_form() {
        let prefix = r"C:\Users\Alice";
        assert_eq!(
            redact_profile(r"Launching game from: C:\Users\Alice\AppData\Local\game.exe", prefix),
            r"Launching game from: %USERPROFILE%\AppData\Local\game.exe"
        );
        assert_eq!(
            redact_profile(r#"into "C:\\Users\\alice\\AppData\\x""#, prefix),
            r#"into "%USERPROFILE%\\AppData\\x""#
        );
        assert_eq!(
            redact_profile("url file:///c:/users/ALICE/Videos", prefix),
            "url file:///%USERPROFILE%/Videos"
        );
        assert_eq!(redact_profile(r"end C:\Users\Alice", prefix), r"end %USERPROFILE%");
    }

    #[test]
    fn profile_redaction_respects_name_boundaries() {
        let prefix = r"C:\Users\Alice";
        assert_eq!(
            redact_profile(r"C:\Users\Alice2\x and C:\Users\Alice.old\y", prefix),
            r"C:\Users\Alice2\x and C:\Users\Alice.old\y"
        );
        assert_eq!(
            redact_profile(r"C:\Users\Alice2\x C:\Users\Alice\y", prefix),
            r"C:\Users\Alice2\x %USERPROFILE%\y"
        );
    }

    #[test]
    fn profile_redaction_keeps_non_ascii_text_intact() {
        let prefix = r"C:\Users\Jörg";
        assert_eq!(
            redact_profile(r"é C:\Users\Jörg\Spiele é", prefix),
            r"é %USERPROFILE%\Spiele é"
        );
        assert_eq!(replace_ascii_case_insensitive("abc", "", "x"), "abc");
    }

    #[test]
    fn env_level_overrides_the_detailed_default() {
        assert_eq!(effective_rank(Some(30)), 30);
        assert_eq!(level_name(effective_rank(None)), "debug");
    }

    fn log_file(name: &str, age_days: u64, len: u64, now: SystemTime) -> LaunchLogFile {
        LaunchLogFile {
            name: name.to_string(),
            modified: now - Duration::from_secs(age_days * 24 * 60 * 60),
            len,
        }
    }

    #[test]
    fn retention_keeps_only_the_newest_four_however_recent() {
        let now = SystemTime::now();
        let files: Vec<LaunchLogFile> = (0..20)
            .map(|i| log_file(&format!("launch-2026-09-{:02}.log", i + 1), 20 - i as u64, 1024, now))
            .collect();
        let mut stale = stale_launch_logs(files);
        stale.sort();
        let expected: Vec<String> = (0..16)
            .map(|i| format!("launch-2026-09-{:02}.log", i + 1))
            .collect();
        assert_eq!(stale, expected);
    }

    #[test]
    fn retention_follows_modified_time_not_the_name() {
        let now = SystemTime::now();
        let files = vec![
            log_file("launch-a.log", 1, 1024, now),
            log_file("launch-b.log", 5, 1024, now),
            log_file("launch-c.log", 2, 1024, now),
            log_file("launch-d.log", 3, 1024, now),
            log_file("launch-e.log", 4, 1024, now),
        ];
        assert_eq!(stale_launch_logs(files), vec!["launch-b.log"]);
    }

    #[test]
    fn retention_pairs_rotated_files_with_their_base() {
        let now = SystemTime::now();
        let mut files: Vec<LaunchLogFile> = (0..5)
            .map(|i| log_file(&format!("launch-2026-01-{:02}.log", i + 1), 10 - i as u64, 1024, now))
            .collect();
        files.push(log_file("launch-2026-01-05.1.log", 6, 1024, now));
        files.push(log_file("launch-2026-01-01.1.log", 10, 1024, now));
        let mut stale = stale_launch_logs(files);
        stale.sort();
        assert_eq!(stale, vec!["launch-2026-01-01.1.log", "launch-2026-01-01.log"]);
    }

    #[test]
    fn retention_caps_the_total_size() {
        let now = SystemTime::now();
        let mb = 1024 * 1024;
        let files: Vec<LaunchLogFile> = (0..4)
            .map(|i| log_file(&format!("launch-2026-09-{:02}.log", i + 1), 4 - i as u64, 20 * mb, now))
            .collect();
        let mut stale = stale_launch_logs(files);
        stale.sort();
        assert_eq!(stale, vec!["launch-2026-09-01.log", "launch-2026-09-02.log"]);
    }

    #[test]
    fn unbound_crash_text_skips_lines_already_in_the_crash_file() {
        let pending: Vec<String> = ["a\n", "b\n", "c\n"].iter().map(|s| s.to_string()).collect();
        assert_eq!(unbound_crash_text(&pending, 0, "x\n"), "a\nb\nc\nx\n");
        assert_eq!(unbound_crash_text(&pending, 2, "x\n"), "c\nx\n");
        assert_eq!(unbound_crash_text(&pending, 9, "x\n"), "x\n");
    }

    #[test]
    fn unclean_exit_note_names_the_previous_log() {
        assert_eq!(
            unclean_exit_note("launch-x.log"),
            "previous session ended without a clean exit (crash, kill, or Windows shutdown), its log is launch-x.log"
        );
        assert!(!unclean_exit_note("").contains("its log"));
    }
}
