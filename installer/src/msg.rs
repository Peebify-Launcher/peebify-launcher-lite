// ------------ Engine Messages ------------
// How the install and uninstall work, which runs on its own thread, talks to the window: progress, warnings,
// finished, failed or cancelled, plus the cancel flag. Progress is logged too so silent runs leave a trail.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const INDETERMINATE: f32 = -1.0;

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Status { phase: String, percent: f32 },
    Warning(String),
    Committed,
    Finished,
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum EngineError {
    Failed(String),
    Cancelled,
}

impl From<String> for EngineError {
    fn from(message: String) -> Self {
        EngineError::Failed(message)
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Failed(m) => f.write_str(m),
            EngineError::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub type EventSink = std::sync::mpsc::Sender<EngineEvent>;

static LAST_LOGGED: Mutex<Option<(String, i32)>> = Mutex::new(None);

pub(crate) fn phase_key(phase: &str) -> &str {
    phase.split(": ").next().unwrap_or(phase)
}

fn progress_bucket(percent: f32) -> i32 {
    if percent < 0.0 {
        -1
    } else if percent >= 100.0 {
        10
    } else {
        (percent / 10.0).floor() as i32
    }
}

fn should_log(last: &mut Option<(String, i32)>, phase: &str, percent: f32) -> bool {
    let entry = (phase_key(phase).to_string(), progress_bucket(percent));
    if last.as_ref() == Some(&entry) {
        return false;
    }
    *last = Some(entry);
    true
}

pub fn status(sink: &EventSink, phase: &str, percent: f32) {
    let log_it = LAST_LOGGED
        .lock()
        .map(|mut last| should_log(&mut last, phase, percent))
        .unwrap_or(true);
    if log_it {
        if percent < 0.0 {
            crate::ilog::ilog!("[  ..  ] {phase}");
        } else {
            crate::ilog::ilog!("[{percent:>5.1}%] {phase}");
        }
    }
    let _ = sink.send(EngineEvent::Status {
        phase: phase.to_string(),
        percent,
    });
}

pub fn committed(sink: &EventSink) {
    crate::ilog::ilog!("install: committed");
    let _ = sink.send(EngineEvent::Committed);
}

pub fn warn(sink: &EventSink, text: String) {
    crate::ilog::ilog!("WARN: {text}");
    let _ = sink.send(EngineEvent::Warning(text));
}

pub fn log_sink() -> EventSink {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || while rx.recv().is_ok() {});
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_ticks_log_once_per_phase_and_ten_percent() {
        let mut last = None;
        assert!(should_log(&mut last, "Verifying installer\u{2026}", 0.5));
        assert!(!should_log(&mut last, "Verifying installer\u{2026}", 3.0));
        assert!(!should_log(&mut last, "Verifying installer\u{2026}", 4.0));
        assert!(should_log(&mut last, "Copying files\u{2026}", 10.0));
        assert!(!should_log(&mut last, "Copying files\u{2026}", 19.9));
        assert!(should_log(&mut last, "Copying files\u{2026}", 20.0));
        assert!(should_log(&mut last, "Copying files\u{2026}", 60.0));
        assert!(!should_log(&mut last, "Copying files\u{2026}", 60.0));
        assert!(should_log(&mut last, "Done", 100.0));
        assert!(!should_log(&mut last, "Done", 100.0));
    }
}
