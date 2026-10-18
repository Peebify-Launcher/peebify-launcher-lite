// ------------ Official Launcher Driver ------------
// Runs a publisher's own launcher out of sight for the launch shim: hides its windows, presses its Play button once its
// log says it is ready, keeps its main window hidden while the game runs, then closes it after the game.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Children, UIA_ButtonControlTypeId};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_INFORMATION, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetAncestor, IsWindow, MsgWaitForMultipleObjects, PeekMessageW, PostMessageW, ShowWindow,
    TranslateMessage, CHILDID_SELF, EVENT_OBJECT_SHOW, GA_ROOT, MSG, OBJID_WINDOW, PM_REMOVE, QS_ALLINPUT, SW_HIDE,
    SW_SHOW, WINEVENT_OUTOFCONTEXT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

use crate::common::cmdline;
use crate::common::process::{find_processes, list_processes, visible_windows_of};
use crate::common::wide;

pub const EXIT_LAUNCHER: i32 = 5;
pub const EXIT_NOT_STARTED: i32 = 6;

const TICK: Duration = Duration::from_millis(50);
const LOG_EVERY: u32 = 4;
const READY_LIMIT: Duration = Duration::from_secs(120);
const PRESS_RETRY: Duration = Duration::from_secs(10);
const PRESSES: u32 = 3;
const CLICK_GAP: Duration = Duration::from_millis(30);
const WATCH_TICK: Duration = Duration::from_millis(250);
const RESTART_GRACE: Duration = Duration::from_secs(10);
const MK_LBUTTON: usize = 1;

pub struct Plan {
    pub launcher: PathBuf,
    pub arguments: Vec<String>,
    pub client: String,
    pub ready_log: PathBuf,
    pub ready_line: String,
    pub button: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Rule {
    Every,
    Only(HWND),
    Nothing,
}

struct Hiding {
    rule: Rule,
    hidden: Vec<HWND>,
}

thread_local! {
    static HIDING: RefCell<Hiding> = const { RefCell::new(Hiding { rule: Rule::Nothing, hidden: Vec::new() }) };
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

impl Owned {
    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) != WAIT_OBJECT_0 }
    }
}

// ------------ Ready Line ------------

pub fn says_ready(log: &str, pid: u32, line: &str) -> bool {
    let marker = format!("pid={pid}");
    let mut from = 0;
    while let Some(found) = log[from..].find(&marker) {
        let end = from + found + marker.len();
        if !log[end..].starts_with(|c: char| c.is_ascii_digit()) {
            return log[end..].contains(line);
        }
        from = end;
    }
    false
}

fn read_log(path: &Path) -> String {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// ------------ Windows ------------

fn set_rule(rule: Rule) {
    HIDING.with(|hiding| hiding.borrow_mut().rule = rule);
}

fn hide(window: HWND) {
    let wanted = HIDING.with(|hiding| match hiding.borrow().rule {
        Rule::Every => true,
        Rule::Only(main) => main == window,
        Rule::Nothing => false,
    });
    if !wanted {
        return;
    }
    unsafe { ShowWindow(window, SW_HIDE) };
    HIDING.with(|hiding| {
        let mut hiding = hiding.borrow_mut();
        if !hiding.hidden.contains(&window) {
            hiding.hidden.push(window);
        }
    });
}

fn hidden_windows() -> Vec<HWND> {
    HIDING.with(|hiding| hiding.borrow().hidden.clone())
}

unsafe extern "system" fn on_show(
    _hook: HWINEVENTHOOK,
    _event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object == OBJID_WINDOW && child == CHILDID_SELF as i32 && GetAncestor(window, GA_ROOT) == window {
        hide(window);
    }
}

fn sweep(pid: u32) {
    for window in visible_windows_of(pid) {
        hide(window);
    }
}

fn reveal() -> usize {
    set_rule(Rule::Nothing);
    hidden_windows()
        .into_iter()
        .filter(|&window| unsafe { IsWindow(window) != 0 && ShowWindow(window, SW_SHOW) == 0 })
        .count()
}

fn pump(wait: Duration) {
    let until = Instant::now() + wait;
    loop {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        unsafe { MsgWaitForMultipleObjects(0, std::ptr::null(), 0, left.as_millis() as u32, QS_ALLINPUT) };
    }
}

// ------------ Play Button ------------

struct Automation(IUIAutomation);

impl Automation {
    fn new() -> Option<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok().map(Self)
        }
    }

    fn button(&self, window: HWND, prefix: &str) -> Option<RECT> {
        unsafe {
            let root = self.0.ElementFromHandle(windows::Win32::Foundation::HWND(window)).ok()?;
            let children = root.FindAll(TreeScope_Children, &self.0.CreateTrueCondition().ok()?).ok()?;
            for index in 0..children.Length().ok()? {
                let Ok(child) = children.GetElement(index) else { continue };
                if child.CurrentControlType().ok() != Some(UIA_ButtonControlTypeId) {
                    continue;
                }
                let class = child.CurrentClassName().map(|c| c.to_string()).unwrap_or_default();
                let Ok(r) = child.CurrentBoundingRectangle() else { continue };
                if class.starts_with(prefix) && r.right > r.left && r.bottom > r.top {
                    return Some(RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom });
                }
            }
            None
        }
    }

    fn find(&self, prefix: &str) -> Option<(HWND, RECT)> {
        hidden_windows()
            .into_iter()
            .filter(|&window| unsafe { IsWindow(window) != 0 })
            .find_map(|window| self.button(window, prefix).map(|rect| (window, rect)))
    }
}

fn press(window: HWND, button: RECT) {
    let mut point = POINT {
        x: (button.left + button.right) / 2,
        y: (button.top + button.bottom) / 2,
    };
    unsafe { ScreenToClient(window, &mut point) };
    let position = (((point.y as u32 & 0xFFFF) << 16) | (point.x as u32 & 0xFFFF)) as isize;
    unsafe { PostMessageW(window, WM_MOUSEMOVE, 0, position) };
    pump(CLICK_GAP);
    unsafe { PostMessageW(window, WM_LBUTTONDOWN, MK_LBUTTON, position) };
    pump(CLICK_GAP);
    unsafe { PostMessageW(window, WM_LBUTTONUP, 0, position) };
}

// ------------ Processes ------------

fn start(plan: &Plan) -> Result<(Owned, u32), String> {
    let image = plan.launcher.to_string_lossy().to_string();
    let application = wide(&image);
    let mut command_line = wide(&cmdline::build_command_line(&image, &plan.arguments));
    let folder = plan
        .launcher
        .parent()
        .ok_or_else(|| format!("{} has no folder", plan.launcher.display()))?;
    let directory = wide(&folder.to_string_lossy());
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
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        let reason = cmdline::describe_start_error(code).map(str::to_string).unwrap_or(format!("error {code}"));
        return Err(format!("Windows refused to start {}: {reason}", plan.launcher.display()));
    }
    unsafe { CloseHandle(process.hThread) };
    Ok((Owned(process.hProcess), process.dwProcessId))
}

fn client_running(client: &str) -> bool {
    !find_processes(client).is_empty()
}

fn image_of(pid: u32) -> Option<PathBuf> {
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if process.0.is_null() {
        return None;
    }
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let read = unsafe { QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length) };
    (read != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

fn close_launcher(launcher: &Owned, pid: u32, folder: &Path) -> usize {
    let processes = list_processes();
    let mut tree = vec![pid];
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next];
        for (_, child, of) in &processes {
            if *of == parent && !tree.contains(child) {
                tree.push(*child);
            }
        }
        next += 1;
    }
    let mut closed = 0;
    for &member in tree.iter().skip(1).rev() {
        if !image_of(member).is_some_and(|image| image.starts_with(folder)) {
            continue;
        }
        let process = Owned(unsafe { OpenProcess(PROCESS_TERMINATE, 0, member) });
        if !process.0.is_null() && unsafe { TerminateProcess(process.0, 0) } != 0 {
            closed += 1;
        }
    }
    if launcher.alive() && unsafe { TerminateProcess(launcher.0, 0) } != 0 {
        closed += 1;
    }
    closed
}

// ------------ Run ------------

pub fn run(plan: &Plan, log: &mut dyn FnMut(&str)) -> i32 {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    set_rule(Rule::Every);
    let (launcher, pid) = match start(plan) {
        Ok(started) => started,
        Err(error) => {
            log(&error);
            return EXIT_LAUNCHER;
        }
    };
    log(&format!("{} started out of sight, pid {pid}", plan.launcher.display()));
    let hook = unsafe {
        SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, std::ptr::null_mut(), Some(on_show), pid, 0, WINEVENT_OUTOFCONTEXT)
    };
    if hook.is_null() {
        log("could not watch the launcher's windows as they open, falling back to checking every 50 ms");
    }
    let code = drive(plan, &launcher, pid, log);
    if !hook.is_null() {
        unsafe { UnhookWinEvent(hook) };
    }
    code
}

fn drive(plan: &Plan, launcher: &Owned, pid: u32, log: &mut dyn FnMut(&str)) -> i32 {
    let automation = Automation::new();
    if automation.is_none() {
        log("UI Automation is unavailable, so the Play button can't be pressed");
    }
    let started = Instant::now();
    let mut ready = false;
    let mut main: Option<HWND> = None;
    let mut presses = 0;
    let mut last_press: Option<Instant> = None;
    let mut gave_up = false;
    let mut tick = 0u32;

    while !client_running(&plan.client) {
        if !launcher.alive() {
            log("the launcher closed before the game started");
            return EXIT_LAUNCHER;
        }
        sweep(pid);
        if !ready && tick % LOG_EVERY == 0 && says_ready(&read_log(&plan.ready_log), pid, &plan.ready_line) {
            ready = true;
            log(&format!("the launcher is ready after {} ms", started.elapsed().as_millis()));
        }
        let due = last_press.is_none_or(|at| at.elapsed() >= PRESS_RETRY);
        if ready && !gave_up && presses < PRESSES && due {
            if let Some((window, button)) = automation.as_ref().and_then(|a| a.find(&plan.button)) {
                press(window, button);
                main = Some(window);
                presses += 1;
                last_press = Some(Instant::now());
                log(&format!("pressed Play (try {presses}) after {} ms", started.elapsed().as_millis()));
            }
        }
        let stuck = started.elapsed() >= READY_LIMIT || (presses == PRESSES && due);
        if stuck && !gave_up {
            gave_up = true;
            let shown = reveal();
            log(&format!(
                "the game didn't start (ready: {ready}, presses: {presses}), so {shown} launcher window(s) were shown again"
            ));
        }
        pump(TICK);
        tick = tick.wrapping_add(1);
    }
    log(&format!("{} started after {} ms", plan.client, started.elapsed().as_millis()));
    set_rule(match (gave_up, main) {
        (false, Some(window)) => Rule::Only(window),
        _ => Rule::Nothing,
    });

    loop {
        while client_running(&plan.client) {
            sweep(pid);
            pump(WATCH_TICK);
        }
        let closed_at = Instant::now();
        while closed_at.elapsed() < RESTART_GRACE && !client_running(&plan.client) {
            sweep(pid);
            pump(WATCH_TICK);
        }
        if !client_running(&plan.client) {
            break;
        }
        log(&format!("{} started again", plan.client));
    }

    if gave_up || !launcher.alive() {
        log("the game closed");
        return if gave_up { EXIT_NOT_STARTED } else { 0 };
    }
    let folder = plan.launcher.parent().unwrap_or(Path::new(""));
    let closed = close_launcher(launcher, pid, folder);
    log(&format!("the game closed, so the launcher was closed ({closed} process(es))"));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_line_must_follow_this_launchers_pid() {
        let log = "Log Init (pid=7068) =====\nall ready, wait for start game\n";
        assert!(says_ready(log, 7068, "all ready, wait for start game"));
        assert!(!says_ready(log, 706, "all ready, wait for start game"));
        assert!(!says_ready(log, 9448, "all ready, wait for start game"));
        assert!(!says_ready("Log Init (pid=7068)\nchecking\n", 7068, "all ready"));
        assert!(!says_ready("all ready\nLog Init (pid=7068)\n", 7068, "all ready"));
        assert!(says_ready("pid=70681 x\npid=7068)\nall ready\n", 7068, "all ready"));
    }
}
