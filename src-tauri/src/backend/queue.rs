// ------------ Operation Queue ------------
// Lines up the heavy jobs (downloads, repairs, moves) so only one runs at a time, and lets a paused one park
// and resume later. Also logs transfer speed now and then so slow downloads are easy to spot in the logs.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

type JobFuture = Pin<Box<dyn Future<Output = Value> + Send + 'static>>;

const THROUGHPUT_LOG_INTERVAL: Duration = Duration::from_secs(60);
const THROUGHPUT_DROP_WINDOW: Duration = Duration::from_secs(10);
const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

struct Transfer {
    game_id: String,
    kind: String,
    phase: Phase,
    since: Instant,
    bytes_then: f64,
    last_rate: f64,
    last_seen: Instant,
    last_bytes: f64,
    paused: bool,
    backgrounded: bool,
    moved: f64,
    active: Duration,
}

impl Transfer {
    fn new(job: &Job, paused: bool, backgrounded: bool, now: Instant) -> Self {
        Self {
            game_id: job.game_id.clone(),
            kind: job.kind.clone(),
            phase: job.phase,
            since: now,
            bytes_then: job.downloaded,
            last_rate: 0.0,
            last_seen: now,
            last_bytes: job.downloaded,
            paused,
            backgrounded,
            moved: 0.0,
            active: Duration::ZERO,
        }
    }

    fn settle(&mut self, now: Instant) {
        if self.phase == Phase::Downloading && !self.paused {
            self.active += now.saturating_duration_since(self.last_seen);
        }
        self.last_seen = now;
    }

    fn observe(
        &mut self,
        job: &Job,
        paused: bool,
        backgrounded: bool,
        now: Instant,
    ) -> Option<(f64, Duration)> {
        self.settle(now);
        let same_stage = job.phase == self.phase && job.kind == self.kind;
        if same_stage && self.phase == Phase::Downloading && job.downloaded > self.last_bytes {
            self.moved += job.downloaded - self.last_bytes;
        }
        self.last_bytes = job.downloaded;
        let flipped = paused != self.paused || backgrounded != self.backgrounded;
        self.paused = paused;
        self.backgrounded = backgrounded;
        if !same_stage || job.downloaded < self.bytes_then {
            self.phase = job.phase;
            self.kind = job.kind.clone();
            self.since = now;
            self.bytes_then = job.downloaded;
            self.last_rate = 0.0;
            return None;
        }
        let elapsed = now.saturating_duration_since(self.since);
        let secs = elapsed.as_secs_f64();
        let rate = if secs > 0.0 {
            (job.downloaded - self.bytes_then) / secs / MIB
        } else {
            0.0
        };
        let dropped = self.last_rate > 0.0
            && elapsed >= THROUGHPUT_DROP_WINDOW
            && rate < self.last_rate * 0.1;
        if !(flipped || dropped || elapsed >= THROUGHPUT_LOG_INTERVAL) {
            return None;
        }
        self.since = now;
        self.bytes_then = job.downloaded;
        self.last_rate = rate;
        Some((rate, elapsed))
    }
}

struct TransferSummary {
    game_id: String,
    moved: f64,
    active: Duration,
}

static THROUGHPUT_LOG: Mutex<Vec<Transfer>> = Mutex::new(Vec::new());
static TRANSFER_SUMMARIES: Mutex<Vec<TransferSummary>> = Mutex::new(Vec::new());

fn log_throughput(job: &Job, paused: bool, terminal: bool) {
    let now = Instant::now();
    let mut log_state = THROUGHPUT_LOG.lock();
    let idx = log_state.iter().position(|t| t.game_id == job.game_id);
    if terminal {
        if let Some(i) = idx {
            let mut transfer = log_state.remove(i);
            transfer.settle(now);
            if transfer.moved > 0.0 {
                let mut summaries = TRANSFER_SUMMARIES.lock();
                summaries.retain(|s| s.game_id != job.game_id);
                summaries.push(TransferSummary {
                    game_id: job.game_id.clone(),
                    moved: transfer.moved,
                    active: transfer.active,
                });
            }
        }
        return;
    }
    let backgrounded = super::window_manager::is_backgrounded();
    let Some(i) = idx else {
        log_state.push(Transfer::new(job, paused, backgrounded, now));
        return;
    };
    let Some((rate_mb, elapsed)) = log_state[i].observe(job, paused, backgrounded, now) else {
        return;
    };
    log::info!(
        "transfer: {} {} {} {:.2}/{:.2} GB at {rate_mb:.1} MB/s over {}s ({:.1}% done, paused={paused}, window backgrounded={backgrounded})",
        job.game_id,
        job.kind,
        job.phase.as_str(),
        job.downloaded / GIB,
        job.total / GIB,
        elapsed.as_secs(),
        job.percent
    );
}

fn forget_transfer(game_id: &str) {
    THROUGHPUT_LOG.lock().retain(|t| t.game_id != game_id);
    TRANSFER_SUMMARIES.lock().retain(|s| s.game_id != game_id);
}

fn take_transfer_summary(game_id: &str) -> Option<TransferSummary> {
    let mut summaries = TRANSFER_SUMMARIES.lock();
    let i = summaries.iter().position(|s| s.game_id == game_id)?;
    Some(summaries.remove(i))
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m{:02}s", secs / 3600, secs % 3600 / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn outcome_of(result: &Value) -> String {
    if result.get("success") == Some(&Value::Bool(true)) {
        "completed".to_string()
    } else if is_deferred(result) {
        "deferred".to_string()
    } else if result.get("cancelled") == Some(&Value::Bool(true)) {
        "cancelled".to_string()
    } else if let Some(error) = result
        .get("error")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        format!("failed ({})", error.trim_end_matches('.'))
    } else if result.is_null() {
        "finished".to_string()
    } else {
        "failed".to_string()
    }
}

fn is_deferred(result: &Value) -> bool {
    result.get("deferredBusy").is_some_and(Value::is_string)
}

fn transfer_note(summary: Option<TransferSummary>) -> String {
    match summary {
        Some(s) if s.moved > 0.0 => {
            let secs = s.active.as_secs_f64();
            let avg = if secs > 0.0 { s.moved / secs / MIB } else { 0.0 };
            format!(
                ", downloaded {:.2} GB in {} (avg {avg:.1} MB/s)",
                s.moved / GIB,
                format_elapsed(s.active)
            )
        }
        _ => String::new(),
    }
}

type ResumeHook = Box<dyn Fn(&JobMeta) + Send + Sync>;

// ------------ Job Queue ------------
// The queue itself: what is running, what is waiting, and what is parked.
#[derive(Clone)]
pub struct JobMeta {
    pub id: u64,
    pub game_id: String,
    pub op_type: String,
    pub kind: String,
    pub was_paused: bool,
}

struct PendingJob {
    meta: JobMeta,
    run: JobFuture,
    tx: oneshot::Sender<Value>,
}

struct QueueState {
    seq: u64,
    current: Option<JobMeta>,
    pending: Vec<PendingJob>,
    parked: Vec<JobMeta>,
}

enum Pick {
    Fresh(usize),
    Resume(usize),
    Idle,
}

impl QueueState {
    fn launch_blocker(&self, game_id: &str) -> Option<&str> {
        let blocks = |m: &&JobMeta| {
            m.game_id == game_id && matches!(m.op_type.as_str(), "download" | "repair" | "move")
        };
        self.current
            .iter()
            .chain(self.pending.iter().map(|j| &j.meta))
            .chain(self.parked.iter())
            .find(blocks)
            .map(|m| m.op_type.as_str())
    }

    fn download_in_flight(&self, game_id: &str) -> bool {
        let is_download = |m: &JobMeta| m.game_id == game_id && m.op_type == "download";
        self.current.as_ref().is_some_and(is_download) || self.parked.iter().any(is_download)
    }

    fn has_download_for(&self, game_id: &str) -> bool {
        self.download_in_flight(game_id)
            || self
                .pending
                .iter()
                .any(|j| j.meta.game_id == game_id && j.meta.op_type == "download")
    }

    fn park(&mut self, job_id: u64, was_paused: bool) -> bool {
        if self.current.as_ref().map(|m| m.id) != Some(job_id) {
            return false;
        }
        let Some(mut meta) = self.current.take() else {
            return false;
        };
        log::info!(
            "Parking {} for {} (yielding to a prioritized op{}).",
            meta.op_type,
            meta.game_id,
            if was_paused { ", it was already paused" } else { "" }
        );
        meta.was_paused = was_paused;
        self.parked.push(meta);
        true
    }

    fn pick_next(&self, prefer_pending: bool) -> Pick {
        if let (true, Some(first)) = (prefer_pending, self.pending.first()) {
            return match self
                .parked
                .iter()
                .position(|m| m.game_id == first.meta.game_id)
            {
                Some(i) => Pick::Resume(i),
                None => Pick::Fresh(0),
            };
        }
        if let Some(last) = self.parked.len().checked_sub(1) {
            Pick::Resume(last)
        } else if !self.pending.is_empty() {
            Pick::Fresh(0)
        } else {
            Pick::Idle
        }
    }
}

pub struct OperationQueue {
    app: AppHandle,
    state: Mutex<QueueState>,
    on_resume_parked: Mutex<Option<ResumeHook>>,
}

impl OperationQueue {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            state: Mutex::new(QueueState {
                seq: 0,
                current: None,
                pending: Vec::new(),
                parked: Vec::new(),
            }),
            on_resume_parked: Mutex::new(None),
        })
    }

    pub fn set_resume_hook(&self, hook: impl Fn(&JobMeta) + Send + Sync + 'static) {
        *self.on_resume_parked.lock() = Some(Box::new(hook));
    }

    pub fn is_busy(&self) -> bool {
        self.state.lock().current.is_some()
    }

    pub fn current_meta(&self) -> Option<JobMeta> {
        self.state.lock().current.clone()
    }

    pub fn has_pending(&self, game_id: &str, op_type: Option<&str>) -> bool {
        self.state.lock().pending.iter().any(|j| {
            j.meta.game_id == game_id && op_type.map(|t| j.meta.op_type == t).unwrap_or(true)
        })
    }

    pub fn has_job_for(&self, game_id: &str) -> bool {
        let state = self.state.lock();
        state.current.as_ref().is_some_and(|m| m.game_id == game_id)
            || state.pending.iter().any(|j| j.meta.game_id == game_id)
            || state.parked.iter().any(|m| m.game_id == game_id)
    }

    pub fn launch_blocker(&self, game_id: &str) -> Option<String> {
        self.state.lock().launch_blocker(game_id).map(str::to_string)
    }

    pub fn has_parked(&self, game_id: &str) -> bool {
        self.state.lock().parked.iter().any(|m| m.game_id == game_id)
    }

    pub fn download_in_flight(&self, game_id: &str) -> bool {
        self.state.lock().download_in_flight(game_id)
    }

    pub fn has_download_for(&self, game_id: &str) -> bool {
        self.state.lock().has_download_for(game_id)
    }

    fn live(&self, game_id: &str) -> Live {
        let state = self.state.lock();
        let running = state
            .current
            .iter()
            .chain(state.parked.iter())
            .find(|m| m.game_id == game_id);
        let queued = || state.pending.iter().map(|j| &j.meta).find(|m| m.game_id == game_id);
        Live {
            running: running.map(|m| m.id),
            kind: running.or_else(queued).map(|m| m.kind.clone()),
        }
    }

    pub fn enqueue(
        self: &Arc<Self>,
        game_id: &str,
        op_type: &str,
        kind: &str,
        run: JobFuture,
    ) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock();
            if op_type == "download" && state.has_download_for(game_id) {
                drop(state);
                log::info!(
                    "A download for {game_id} is already running or queued, so the new request joins it."
                );
                let _ = tx.send(already_queued(game_id));
                return rx;
            }
            state.seq += 1;
            let meta = JobMeta {
                id: state.seq,
                game_id: game_id.to_string(),
                op_type: op_type.to_string(),
                kind: kind.to_string(),
                was_paused: false,
            };
            let will_wait = state.current.is_some() || !state.pending.is_empty();
            state.pending.push(PendingJob { meta, run, tx });
            if will_wait {
                log::info!(
                    "Queued {op_type} for {game_id} (position {}); waiting for current operation.",
                    state.pending.len()
                );
            }
        }
        self.emit_status();
        self.drain();
        rx
    }

    pub fn cancel_pending(&self, game_id: &str, op_type: Option<&str>) -> bool {
        let removed = {
            let mut state = self.state.lock();
            let idx = state.pending.iter().position(|j| {
                j.meta.game_id == game_id && op_type.map(|t| j.meta.op_type == t).unwrap_or(true)
            });
            idx.map(|i| state.pending.remove(i))
        };
        let Some(job) = removed else {
            return false;
        };
        log::info!(
            "Removed queued {} for {game_id} (cancelled while waiting).",
            job.meta.op_type
        );
        let _ = job.tx.send(json!({
            "success": false,
            "cancelled": true,
            "error": "Cancelled while queued.",
        }));
        self.emit_status();
        true
    }

    pub fn prioritize(&self, game_id: &str) -> bool {
        let found = {
            let mut state = self.state.lock();
            match state.pending.iter().position(|j| j.meta.game_id == game_id) {
                None => false,
                Some(0) => true,
                Some(idx) => {
                    let job = state.pending.remove(idx);
                    state.pending.insert(0, job);
                    true
                }
            }
        };
        if found {
            self.emit_status();
        }
        found
    }

    pub fn park_current(self: &Arc<Self>, job_id: u64, was_paused: bool) -> bool {
        if !self.state.lock().park(job_id, was_paused) {
            return false;
        }
        self.emit_status();
        self.drain_next(true);
        true
    }

    fn drain(self: &Arc<Self>) {
        self.drain_next(false);
    }

    fn drain_next(self: &Arc<Self>, prefer_pending: bool) {
        enum Next {
            Fresh(JobMeta, JobFuture, oneshot::Sender<Value>),
            Resumed(JobMeta),
            Idle,
        }

        let next = {
            let mut state = self.state.lock();
            if state.current.is_some() {
                return;
            }
            match state.pick_next(prefer_pending) {
                Pick::Fresh(i) => {
                    let job = state.pending.remove(i);
                    state.current = Some(job.meta.clone());
                    Next::Fresh(job.meta, job.run, job.tx)
                }
                Pick::Resume(i) => {
                    let meta = state.parked.remove(i);
                    state.current = Some(meta.clone());
                    Next::Resumed(meta)
                }
                Pick::Idle => Next::Idle,
            }
        };

        match next {
            Next::Idle => self.emit_status(),
            Next::Resumed(meta) => {
                self.emit_status();
                if meta.was_paused {
                    log::info!(
                        "Parked {} for {} is current again and stays paused until it is resumed.",
                        meta.op_type,
                        meta.game_id
                    );
                } else {
                    log::info!("Resuming parked {} for {}.", meta.op_type, meta.game_id);
                    if let Some(hook) = self.on_resume_parked.lock().as_ref() {
                        hook(&meta);
                    }
                }
            }
            Next::Fresh(meta, run, tx) => {
                log::info!("Starting {} for {}.", meta.op_type, meta.game_id);
                forget_transfer(&meta.game_id);
                self.emit_status();
                let queue = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    let started = Instant::now();
                    let result = run.await;
                    log::info!(
                        "Finished {} for {} (job #{}) in {}: {}{}.",
                        meta.op_type,
                        meta.game_id,
                        meta.id,
                        format_elapsed(started.elapsed()),
                        outcome_of(&result),
                        transfer_note(take_transfer_summary(&meta.game_id))
                    );
                    if !queue.has_pending(&meta.game_id, None) {
                        close_silent_job(&queue.app, &meta, &result);
                    }
                    let _ = tx.send(result);

                    let owned_slot = {
                        let mut state = queue.state.lock();
                        state.parked.retain(|m| m.id != meta.id);
                        if state.current.as_ref().map(|m| m.id) == Some(meta.id) {
                            state.current = None;
                            true
                        } else {
                            false
                        }
                    };
                    queue.emit_status();
                    if owned_slot {
                        queue.drain();
                    }
                });
            }
        }
    }

    fn emit_status(&self) {
        let (pending, parked): (Vec<(String, String)>, Vec<String>) = {
            let state = self.state.lock();
            (
                state
                    .pending
                    .iter()
                    .map(|j| (j.meta.game_id.clone(), j.meta.kind.clone()))
                    .collect(),
                state.parked.iter().map(|m| m.game_id.clone()).collect(),
            )
        };
        sync_pending(&self.app, &pending, &parked);
        if let Some(state) = self.app.try_state::<super::BackendState>() {
            state.window.update_tray_menu();
        }
    }
}

pub fn already_queued(game_id: &str) -> Value {
    json!({
        "success": true,
        "alreadyQueued": true,
        "gameId": game_id,
    })
}

// ------------ Status Board ------------
// What the UI sees. Workers publish updates here and they are merged into one list of jobs sent to the frontend,
// so a late or stray event cannot bring a finished job back.
const EVENT: &str = "download-queue-state";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Queued,
    Downloading,
    Scanning,
    Verifying,
    Repairing,
    Extracting,
    Moving,
    Done,
    Cancelled,
    Deferred,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Downloading => "downloading",
            Phase::Scanning => "scanning",
            Phase::Verifying => "verifying",
            Phase::Repairing => "repairing",
            Phase::Extracting => "extracting",
            Phase::Moving => "moving",
            Phase::Done => "done",
            Phase::Cancelled => "cancelled",
            Phase::Deferred => "deferred",
            Phase::Error => "error",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Done | Phase::Cancelled | Phase::Deferred | Phase::Error
        )
    }
}

#[derive(Clone, Debug)]
pub struct Update {
    phase: Phase,
    kind: Option<String>,
    paused: bool,
    resumed: bool,
    warning: bool,
    waiting_network: bool,
    held_for_game: bool,
}

impl From<Phase> for Update {
    fn from(phase: Phase) -> Self {
        Self {
            phase,
            kind: None,
            paused: false,
            resumed: false,
            warning: false,
            waiting_network: false,
            held_for_game: false,
        }
    }
}

impl Update {
    pub fn new(phase: Phase) -> Self {
        phase.into()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }

    pub fn resumed(mut self) -> Self {
        self.resumed = true;
        self
    }

    pub fn warning(mut self, warning: bool) -> Self {
        self.warning = warning;
        self
    }

    pub fn waiting_network(mut self, waiting: bool) -> Self {
        self.waiting_network = waiting;
        self
    }

    pub fn held_for_game(mut self, held: bool) -> Self {
        self.held_for_game = held;
        self
    }
}

#[derive(Clone)]
struct Job {
    game_id: String,
    kind: String,
    base: String,
    phase: Phase,
    total: f64,
    downloaded: f64,
    percent: f64,
    speed: f64,
    network_speed: f64,
    disk_speed: f64,
    eta_secs: f64,
    paused: bool,
    error: Option<String>,
    message: Option<String>,
    warning: bool,
    parked: bool,
    waiting_network: bool,
    held_for_game: bool,
}

impl Job {
    fn to_json(&self) -> Value {
        json!({
            "id": self.game_id,
            "gameId": self.game_id,
            "kind": self.kind,
            "baseKind": self.base,
            "phase": self.phase.as_str(),
            "total": self.total,
            "downloaded": self.downloaded,
            "percent": self.percent,
            "speed": self.speed,
            "networkSpeed": self.network_speed,
            "diskSpeed": self.disk_speed,
            "etaSecs": self.eta_secs,
            "paused": self.paused,
            "error": self.error,
            "message": self.message,
            "warning": self.warning,
            "parked": self.parked,
            "waitingNetwork": self.waiting_network,
            "heldForGame": self.held_for_game,
        })
    }
}

#[derive(Default)]
struct Live {
    running: Option<u64>,
    kind: Option<String>,
}

struct Board {
    jobs: Vec<Job>,
    emitted: Vec<Job>,
    ended: Vec<(String, u64)>,
}

static BOARD: Mutex<Board> = Mutex::new(Board {
    jobs: Vec::new(),
    emitted: Vec::new(),
    ended: Vec::new(),
});

fn num(payload: &Value, key: &str, prev: f64) -> f64 {
    payload
        .get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .unwrap_or(prev)
}

fn done_message(payload: &Value, phase: Phase) -> Option<String> {
    if phase != Phase::Done {
        return None;
    }
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn source_kind(source: &str) -> Option<&'static str> {
    match source {
        "repair-progress" => Some("repair"),
        "move-progress" => Some("move"),
        "uninstall-progress" => Some("uninstall"),
        _ => None,
    }
}

fn resolve(
    prev: Option<&Job>,
    game_id: &str,
    source: &str,
    update: &Update,
    payload: &Value,
    live_kind: Option<&str>,
) -> Job {
    let terminal = update.phase.is_terminal();
    let held = prev.filter(|p| {
        (update.paused || update.resumed)
            && !terminal
            && !p.phase.is_terminal()
            && p.phase != Phase::Queued
    });
    let phase = held.map_or(update.phase, |p| p.phase);
    let fixed = source_kind(source);
    let base = [
        fixed,
        live_kind,
        prev.map(|p| p.base.as_str()),
        update.kind.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|k| !k.is_empty())
    .unwrap_or("install")
    .to_string();
    let kind = match (fixed, held) {
        (Some(fixed), _) => fixed.to_string(),
        (None, Some(p)) => p.kind.clone(),
        (None, None) => update
            .kind
            .clone()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| base.clone()),
    };
    let paused = update.paused && !terminal;
    Job {
        game_id: game_id.to_string(),
        kind,
        base,
        error: (phase == Phase::Error).then(|| {
            payload
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("Operation failed")
                .to_string()
        }),
        message: done_message(payload, phase),
        warning: phase == Phase::Done && update.warning,
        phase,
        total: num(payload, "totalBytes", prev.map_or(0.0, |p| p.total)),
        downloaded: num(
            payload,
            "processedBytes",
            prev.map_or(0.0, |p| p.downloaded),
        ),
        percent: num(payload, "percentage", prev.map_or(0.0, |p| p.percent)),
        speed: num(payload, "speed", prev.map_or(0.0, |p| p.speed)),
        network_speed: num(payload, "networkSpeed", prev.map_or(0.0, |p| p.network_speed)),
        disk_speed: num(payload, "diskSpeed", prev.map_or(0.0, |p| p.disk_speed)),
        eta_secs: num(payload, "eta", prev.map_or(0.0, |p| p.eta_secs)),
        paused,
        parked: !terminal && prev.is_some_and(|p| p.parked),
        waiting_network: !terminal && !paused && update.waiting_network,
        held_for_game: !terminal && !paused && update.held_for_game,
    }
}

fn snapshot(jobs: &[Job]) -> Value {
    json!({ "jobs": jobs.iter().map(Job::to_json).collect::<Vec<_>>() })
}

fn looks_unchanged(a: &Job, b: &Job) -> bool {
    a.phase == b.phase
        && a.kind == b.kind
        && a.base == b.base
        && a.paused == b.paused
        && a.error == b.error
        && a.message == b.message
        && a.warning == b.warning
        && a.parked == b.parked
        && a.waiting_network == b.waiting_network
        && a.held_for_game == b.held_for_game
        && a.total == b.total
        && a.downloaded == b.downloaded
        && a.percent == b.percent
        && (a.speed - b.speed).abs() < 1.0
        && (a.network_speed - b.network_speed).abs() < 1.0
        && (a.disk_speed - b.disk_speed).abs() < 1.0
        && (a.eta_secs - b.eta_secs).abs() < 0.5
}

impl Board {
    fn admits(&mut self, game_id: &str, source: &str, terminal: bool, live: &Live) -> bool {
        let ended = self.ended.iter().position(|(g, _)| g == game_id);
        if terminal {
            if let Some(id) = live.running {
                match ended {
                    Some(i) => self.ended[i].1 = id,
                    None => self.ended.push((game_id.to_string(), id)),
                }
            }
            return true;
        }
        let Some(i) = ended else {
            return true;
        };
        if source == "uninstall-progress" {
            return true;
        }
        match live.running {
            Some(id) if id != self.ended[i].1 => {
                self.ended.remove(i);
                true
            }
            _ => false,
        }
    }

    fn apply(
        &mut self,
        game_id: &str,
        source: &str,
        update: &Update,
        payload: &Value,
        live: &Live,
    ) -> Option<(Job, Vec<Value>)> {
        let terminal = update.phase.is_terminal();
        if !self.admits(game_id, source, terminal, live) {
            log::debug!(
                "Dropped a late {source} event for {game_id} ({}) after its job ended.",
                update.phase.as_str()
            );
            return None;
        }
        let idx = self.jobs.iter().position(|j| j.game_id == game_id);
        let job = resolve(
            idx.map(|i| &self.jobs[i]),
            game_id,
            source,
            update,
            payload,
            live.kind.as_deref(),
        );
        let unchanged = !terminal
            && self
                .emitted
                .iter()
                .find(|j| j.game_id == game_id)
                .is_some_and(|last| looks_unchanged(&job, last));
        match idx {
            Some(i) => self.jobs[i] = job.clone(),
            None => self.jobs.push(job.clone()),
        }
        let snaps = if unchanged {
            Vec::new()
        } else if terminal {
            self.emitted.retain(|j| j.game_id != game_id);
            let with = snapshot(&self.jobs);
            self.jobs.retain(|j| j.game_id != game_id);
            vec![with, snapshot(&self.jobs)]
        } else {
            match self.emitted.iter().position(|j| j.game_id == game_id) {
                Some(i) => self.emitted[i] = job.clone(),
                None => self.emitted.push(job.clone()),
            }
            vec![snapshot(&self.jobs)]
        };
        Some((job, snaps))
    }

    fn needs_closing(&self, game_id: &str, job_id: u64) -> bool {
        let ended_it = self
            .ended
            .iter()
            .any(|(g, id)| g == game_id && *id == job_id);
        !ended_it
            && self
                .jobs
                .iter()
                .any(|j| j.game_id == game_id && !j.phase.is_terminal())
    }
}

fn emit(app: &AppHandle, snap: Value) {
    let _ = app.emit(EVENT, snap);
}

fn live_job(app: &AppHandle, game_id: &str) -> Live {
    app.try_state::<super::BackendState>()
        .map(|state| state.engine.queue.live(game_id))
        .unwrap_or_default()
}

pub fn publish(app: &AppHandle, source: &str, update: impl Into<Update>, payload: Value) {
    let Some(game_id) = payload
        .get("gameId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let update = update.into();
    let live = live_job(app, &game_id);
    let Some((job, snaps)) = BOARD
        .lock()
        .apply(&game_id, source, &update, &payload, &live)
    else {
        return;
    };
    log_throughput(&job, job.paused, job.phase.is_terminal());
    for snap in snaps {
        emit(app, snap);
    }
}

fn source_of(op_type: &str) -> &'static str {
    match op_type {
        "repair" => "repair-progress",
        "move" => "move-progress",
        _ => "download-progress",
    }
}

fn closing_update(meta: &JobMeta, result: &Value) -> (Update, Value) {
    let phase = if result.get("success") == Some(&Value::Bool(true)) {
        Phase::Done
    } else if is_deferred(result) {
        Phase::Deferred
    } else if result.get("cancelled") == Some(&Value::Bool(true)) {
        Phase::Cancelled
    } else {
        Phase::Error
    };
    let mut payload = json!({ "gameId": meta.game_id, "status": phase.as_str() });
    if phase == Phase::Error {
        payload["error"] = result.get("error").cloned().unwrap_or(Value::Null);
    }
    (Update::new(phase).kind(meta.kind.clone()), payload)
}

fn close_silent_job(app: &AppHandle, meta: &JobMeta, result: &Value) {
    if !BOARD.lock().needs_closing(&meta.game_id, meta.id) {
        return;
    }
    let (update, payload) = closing_update(meta, result);
    log::warn!(
        "{} for {} (job #{}) ended without a final progress event, so it is shown as {}.",
        meta.op_type,
        meta.game_id,
        meta.id,
        update.phase().as_str()
    );
    publish(app, source_of(&meta.op_type), update, payload);
}

fn merge_pending(jobs: &mut Vec<Job>, pending: &[(String, String)], parked: &[String]) {
    for (game_id, kind) in pending {
        if game_id.is_empty() || jobs.iter().any(|j| &j.game_id == game_id) {
            continue;
        }
        let kind = if kind.is_empty() { "install" } else { kind.as_str() };
        jobs.push(Job {
            game_id: game_id.clone(),
            kind: kind.to_string(),
            base: kind.to_string(),
            phase: Phase::Queued,
            total: 0.0,
            downloaded: 0.0,
            percent: 0.0,
            speed: 0.0,
            network_speed: 0.0,
            disk_speed: 0.0,
            eta_secs: 0.0,
            paused: false,
            error: None,
            message: None,
            warning: false,
            parked: false,
            waiting_network: false,
            held_for_game: false,
        });
    }
    for job in jobs.iter_mut() {
        job.parked = parked.contains(&job.game_id);
    }
}

pub fn sync_pending(app: &AppHandle, pending: &[(String, String)], parked: &[String]) {
    let snap = {
        let mut board = BOARD.lock();
        merge_pending(&mut board.jobs, pending, parked);
        snapshot(&board.jobs)
    };
    emit(app, snap);
}

pub async fn get_state() -> Result<Value, String> {
    Ok(super::ok_with(snapshot(&BOARD.lock().jobs)))
}

// ------------ Queue Tests ------------
// Covers how status updates are routed and how the queue picks the next job.
#[cfg(test)]
mod status_routing_tests {
    use super::*;

    fn live(running: Option<u64>, kind: Option<&str>) -> Live {
        Live {
            running,
            kind: kind.map(str::to_string),
        }
    }

    fn board() -> Board {
        Board {
            jobs: Vec::new(),
            emitted: Vec::new(),
            ended: Vec::new(),
        }
    }

    fn send(
        board: &mut Board,
        source: &str,
        update: impl Into<Update>,
        payload: Value,
        live: &Live,
    ) -> Option<Job> {
        board
            .apply("wuwa", source, &update.into(), &payload, live)
            .map(|(job, _)| job)
    }

    fn fresh(source: &str, update: impl Into<Update>, live_kind: Option<&str>) -> Job {
        resolve(None, "wuwa", source, &update.into(), &json!({}), live_kind)
    }

    #[test]
    fn phases_use_the_names_the_ui_reads() {
        let names: Vec<&str> = [
            Phase::Queued,
            Phase::Downloading,
            Phase::Scanning,
            Phase::Verifying,
            Phase::Repairing,
            Phase::Extracting,
            Phase::Moving,
            Phase::Done,
            Phase::Cancelled,
            Phase::Deferred,
            Phase::Error,
        ]
        .into_iter()
        .map(Phase::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "queued",
                "downloading",
                "scanning",
                "verifying",
                "repairing",
                "extracting",
                "moving",
                "done",
                "cancelled",
                "deferred",
                "error"
            ]
        );
        assert!(Phase::Done.is_terminal() && Phase::Cancelled.is_terminal());
        assert!(Phase::Error.is_terminal() && !Phase::Queued.is_terminal());
        assert!(Phase::Deferred.is_terminal() && !Phase::Moving.is_terminal());
    }

    #[test]
    fn a_failure_word_in_the_status_never_ends_the_job() {
        let mut b = board();
        let running = live(Some(3), Some("update"));
        send(
            &mut b,
            "download-progress",
            Phase::Downloading,
            json!({ "status": "Downloading Patch 1.2" }),
            &running,
        );
        let job = send(
            &mut b,
            "download-progress",
            Phase::Scanning,
            json!({ "status": "Patch download failed, switching to a full download..." }),
            &running,
        )
        .expect("event accepted");
        assert_eq!(job.phase, Phase::Scanning);
        assert_eq!((job.kind.as_str(), job.base.as_str()), ("update", "update"));
        assert!(job.error.is_none());
        assert_eq!(b.jobs.len(), 1, "the job stays on the board");

        let job = send(
            &mut b,
            "download-progress",
            Phase::Downloading,
            json!({ "status": "Error correction failed, cancelled and completed" }),
            &running,
        )
        .expect("event accepted");
        assert_eq!(job.phase, Phase::Downloading);
        assert_eq!(job.base, "update");
    }

    #[test]
    fn a_cancelled_verify_ends_as_cancelled() {
        let mut b = board();
        let running = live(Some(4), Some("verify"));
        send(
            &mut b,
            "download-progress",
            Update::new(Phase::Verifying).kind("verify"),
            json!({ "status": "Verifying files..." }),
            &running,
        );
        let (job, snaps) = b
            .apply(
                "wuwa",
                "download-progress",
                &Update::new(Phase::Cancelled).kind("verify"),
                &json!({ "status": "Verification Cancelled", "percentage": 0 }),
                &running,
            )
            .expect("event accepted");
        assert_eq!(job.phase, Phase::Cancelled);
        assert!(job.error.is_none());
        assert_eq!(snaps[0]["jobs"][0]["phase"], json!("cancelled"));
        assert_eq!(snaps[0]["jobs"][0]["kind"], json!("verify"));
        assert_eq!(snaps[1]["jobs"], json!([]));

        let reply = super::super::file_channels::verify_cancelled_response();
        assert_eq!(reply["cancelled"], json!(true));
        assert_eq!(reply["success"], json!(false));
        assert_eq!(outcome_of(&reply), "cancelled");
    }

    #[test]
    fn a_repair_scan_shows_as_verifying() {
        let mut b = board();
        let job = send(
            &mut b,
            "repair-progress",
            super::super::engine::SCAN_PHASE,
            json!({ "status": super::super::engine::status::VALIDATING }),
            &live(Some(5), Some("repair")),
        )
        .expect("event accepted");
        assert_eq!(job.phase.as_str(), "verifying");
        assert_eq!((job.kind.as_str(), job.base.as_str()), ("repair", "repair"));
    }

    #[test]
    fn a_paused_job_keeps_its_stage_and_a_resume_goes_on_in_it() {
        let mut b = board();
        let running = live(Some(6), Some("update"));
        send(
            &mut b,
            "download-progress",
            Phase::Scanning,
            json!({ "status": "Checking existing files..." }),
            &running,
        );
        let paused = send(
            &mut b,
            "download-progress",
            Update::new(Phase::Downloading).paused(true),
            json!({ "status": "Paused" }),
            &running,
        )
        .expect("event accepted");
        assert_eq!((paused.phase, paused.paused), (Phase::Scanning, true));
        let resumed = send(
            &mut b,
            "download-progress",
            Update::new(Phase::Downloading).resumed(),
            json!({ "status": "Downloading..." }),
            &running,
        )
        .expect("event accepted");
        assert_eq!((resumed.phase, resumed.paused), (Phase::Scanning, false));
        let next = send(
            &mut b,
            "download-progress",
            Phase::Downloading,
            json!({}),
            &running,
        )
        .expect("event accepted");
        assert_eq!(next.phase, Phase::Downloading);

        let mut r = board();
        let repair = live(Some(7), Some("repair"));
        send(&mut r, "repair-progress", Phase::Verifying, json!({}), &repair);
        let resumed = send(
            &mut r,
            "repair-progress",
            Update::new(Phase::Repairing).resumed(),
            json!({}),
            &repair,
        )
        .expect("event accepted");
        assert_eq!(resumed.phase, Phase::Verifying);
    }

    #[test]
    fn a_pause_keeps_the_activity_kind_and_a_new_job_takes_the_phase_sent() {
        let verifying = fresh(
            "download-progress",
            Update::new(Phase::Verifying).kind("verify"),
            Some("install"),
        );
        let paused = resolve(
            Some(&verifying),
            "wuwa",
            "download-progress",
            &Update::new(Phase::Downloading).paused(true),
            &json!({}),
            Some("install"),
        );
        assert_eq!(
            (paused.phase, paused.kind.as_str(), paused.base.as_str()),
            (Phase::Verifying, "verify", "install")
        );
        let queued = fresh("download-progress", Phase::Queued, Some("install"));
        let first = resolve(
            Some(&queued),
            "wuwa",
            "download-progress",
            &Update::new(Phase::Downloading).paused(true),
            &json!({}),
            Some("install"),
        );
        assert_eq!((first.phase, first.paused), (Phase::Downloading, true));
        let ended = resolve(
            Some(&paused),
            "wuwa",
            "download-progress",
            &Update::new(Phase::Cancelled).paused(true),
            &json!({}),
            Some("install"),
        );
        assert_eq!((ended.phase, ended.paused), (Phase::Cancelled, false));
    }

    #[test]
    fn a_lost_connection_flags_the_job_without_changing_its_phase() {
        let job = fresh(
            "download-progress",
            Update::new(Phase::Downloading).waiting_network(true),
            None,
        );
        assert!(job.waiting_network);
        assert_eq!(job.phase, Phase::Downloading);
        let job = fresh(
            "repair-progress",
            Update::new(Phase::Repairing).waiting_network(true),
            None,
        );
        assert!(job.waiting_network);
        assert!(
            !fresh(
                "download-progress",
                Update::new(Phase::Downloading)
                    .waiting_network(true)
                    .paused(true),
                None
            )
            .waiting_network
        );
        assert!(
            !fresh(
                "download-progress",
                Update::new(Phase::Error).waiting_network(true),
                None
            )
            .waiting_network
        );
        assert!(!fresh("download-progress", Phase::Downloading, None).waiting_network);
    }

    #[test]
    fn only_a_finished_job_carries_its_message() {
        let payload = json!({ "message": "  Cleared 2 leftover update folders.  " });
        assert_eq!(
            done_message(&payload, Phase::Done).as_deref(),
            Some("Cleared 2 leftover update folders.")
        );
        assert_eq!(done_message(&payload, Phase::Repairing), None);
        assert_eq!(done_message(&payload, Phase::Error), None);
        assert_eq!(done_message(&json!({ "message": " " }), Phase::Done), None);
        assert_eq!(done_message(&json!({}), Phase::Done), None);
    }

    #[test]
    fn only_a_finished_job_carries_a_warning() {
        let done = fresh("move-progress", Update::new(Phase::Done).warning(true), None);
        assert!(done.warning);
        assert_eq!(done.kind, "move");
        assert!(!fresh("move-progress", Phase::Done, None).warning);
        assert!(
            !fresh(
                "move-progress",
                Update::new(Phase::Downloading).warning(true),
                None
            )
            .warning
        );
    }

    #[test]
    fn a_failed_job_carries_its_error() {
        let job = resolve(
            None,
            "wuwa",
            "download-progress",
            &Phase::Error.into(),
            &json!({ "error": "Disk full." }),
            None,
        );
        assert_eq!(job.error.as_deref(), Some("Disk full."));
        assert_eq!(
            fresh("download-progress", Phase::Error, None).error.as_deref(),
            Some("Operation failed")
        );
        assert!(fresh("download-progress", Phase::Cancelled, None)
            .error
            .is_none());
    }

    #[test]
    fn the_base_kind_comes_from_the_queue_and_outlasts_a_verify_stage() {
        let checking = fresh("download-progress", Phase::Scanning, Some("update"));
        assert_eq!(
            (checking.kind.as_str(), checking.base.as_str()),
            ("update", "update")
        );
        let verifying = resolve(
            Some(&checking),
            "wuwa",
            "download-progress",
            &Update::new(Phase::Verifying).kind("verify"),
            &json!({}),
            Some("update"),
        );
        assert_eq!(
            (verifying.kind.as_str(), verifying.base.as_str()),
            ("verify", "update")
        );
        let snapshot = verifying.to_json();
        assert_eq!(snapshot["kind"], json!("verify"));
        assert_eq!(snapshot["baseKind"], json!("update"));
        let done = resolve(
            Some(&verifying),
            "wuwa",
            "download-progress",
            &Phase::Done.into(),
            &json!({}),
            None,
        );
        assert_eq!((done.kind.as_str(), done.base.as_str()), ("update", "update"));

        let verify = fresh(
            "download-progress",
            Update::new(Phase::Verifying).kind("verify"),
            Some("verify"),
        );
        assert_eq!((verify.kind.as_str(), verify.base.as_str()), ("verify", "verify"));
        let queued = fresh(
            "download-progress",
            Update::new(Phase::Queued).kind("update"),
            None,
        );
        assert_eq!(queued.base, "update");
        let unknown = fresh("download-progress", Phase::Downloading, None);
        assert_eq!((unknown.kind.as_str(), unknown.base.as_str()), ("install", "install"));
        let repair = fresh(
            "repair-progress",
            Update::new(Phase::Repairing).kind("verify"),
            Some("update"),
        );
        assert_eq!((repair.kind.as_str(), repair.base.as_str()), ("repair", "repair"));
    }

    #[test]
    fn late_worker_events_cannot_bring_back_an_ended_job() {
        let mut b = board();
        let running = live(Some(7), Some("verify"));
        assert!(send(&mut b, "download-progress", Phase::Verifying, json!({}), &running).is_some());
        assert!(send(&mut b, "download-progress", Phase::Cancelled, json!({}), &running).is_some());
        assert!(b.jobs.is_empty());

        assert!(send(&mut b, "download-progress", Phase::Verifying, json!({}), &running).is_none());
        assert!(send(
            &mut b,
            "download-progress",
            Phase::Verifying,
            json!({}),
            &live(None, None)
        )
        .is_none());
        assert!(b.jobs.is_empty(), "no ghost job is left behind");

        assert!(send(
            &mut b,
            "uninstall-progress",
            Phase::Downloading,
            json!({}),
            &live(None, None)
        )
        .is_some());
        let outside = live(None, None);
        assert!(send(&mut b, "uninstall-progress", Phase::Done, json!({}), &outside).is_some());

        assert!(send(
            &mut b,
            "download-progress",
            Phase::Downloading,
            json!({}),
            &live(Some(8), Some("update"))
        )
        .is_some());
        assert!(b.ended.is_empty());
        assert_eq!(b.jobs[0].base, "update");
    }

    #[test]
    fn a_job_that_ends_without_a_final_event_is_closed() {
        let mut b = board();
        merge_pending(&mut b.jobs, &[("wuwa".to_string(), "verify".to_string())], &[]);
        let meta = JobMeta {
            id: 9,
            game_id: "wuwa".to_string(),
            op_type: "verify".to_string(),
            kind: "verify".to_string(),
            was_paused: false,
        };
        assert!(b.needs_closing("wuwa", 9));
        let (update, payload) = closing_update(
            &meta,
            &json!({ "success": false, "error": "No install manifest found." }),
        );
        assert_eq!(update.phase(), Phase::Error);
        let (job, _) = b
            .apply(
                "wuwa",
                source_of(&meta.op_type),
                &update,
                &payload,
                &live(Some(9), Some("verify")),
            )
            .expect("event accepted");
        assert_eq!(job.error.as_deref(), Some("No install manifest found."));
        assert_eq!(job.kind, "verify");
        assert!(b.jobs.is_empty());
        assert!(!b.needs_closing("wuwa", 9));

        merge_pending(&mut b.jobs, &[("wuwa".to_string(), "update".to_string())], &[]);
        assert!(!b.needs_closing("wuwa", 9));

        let ok = closing_update(&meta, &json!({ "success": true })).0;
        assert_eq!(ok.phase(), Phase::Done);
        let stopped = closing_update(&meta, &json!({ "success": false, "cancelled": true })).0;
        assert_eq!(stopped.phase(), Phase::Cancelled);
        let deferred = json!({ "success": false, "deferredBusy": "A game is running." });
        assert_eq!(closing_update(&meta, &deferred).0.phase(), Phase::Deferred);
        assert_eq!(source_of("repair"), "repair-progress");
        assert_eq!(source_of("move"), "move-progress");
        assert_eq!(source_of("download"), "download-progress");
    }

    fn meta(id: u64, game_id: &str, op_type: &str) -> JobMeta {
        JobMeta {
            id,
            game_id: game_id.to_string(),
            op_type: op_type.to_string(),
            kind: op_type.to_string(),
            was_paused: false,
        }
    }

    fn pending(id: u64, game_id: &str, op_type: &str) -> PendingJob {
        PendingJob {
            meta: meta(id, game_id, op_type),
            run: Box::pin(async { Value::Null }),
            tx: oneshot::channel().0,
        }
    }

    fn state(pending: Vec<PendingJob>, parked: Vec<JobMeta>) -> QueueState {
        QueueState {
            seq: 0,
            current: None,
            pending,
            parked,
        }
    }

    #[test]
    fn a_parked_job_resumes_before_fresh_pending_jobs() {
        let s = state(vec![pending(2, "zzz", "download")], vec![meta(1, "wuwa", "download")]);
        assert!(matches!(s.pick_next(false), Pick::Resume(0)));
        assert!(matches!(s.pick_next(true), Pick::Fresh(0)));
    }

    #[test]
    fn a_pending_job_never_runs_beside_its_own_parked_download() {
        let s = state(
            vec![pending(2, "wuwa", "move"), pending(3, "zzz", "download")],
            vec![meta(1, "genshin", "download"), meta(4, "wuwa", "download")],
        );
        assert!(matches!(s.pick_next(true), Pick::Resume(1)));
        assert!(matches!(s.pick_next(false), Pick::Resume(1)));
        let free = state(
            vec![pending(3, "zzz", "download"), pending(2, "wuwa", "move")],
            vec![meta(4, "wuwa", "download")],
        );
        assert!(matches!(free.pick_next(true), Pick::Fresh(0)));
        assert!(matches!(free.pick_next(false), Pick::Resume(0)));
    }

    #[test]
    fn the_job_parked_last_resumes_first() {
        let s = state(
            Vec::new(),
            vec![meta(1, "a", "download"), meta(2, "b", "download")],
        );
        assert!(matches!(s.pick_next(false), Pick::Resume(1)));
    }

    #[test]
    fn only_the_job_that_was_paused_is_parked() {
        let mut s = state(Vec::new(), Vec::new());
        s.current = Some(meta(5, "b", "download"));
        assert!(!s.park(4, false));
        assert_eq!(s.current.as_ref().map(|m| m.id), Some(5));
        assert!(s.parked.is_empty());
        assert!(s.park(5, true));
        assert!(s.current.is_none());
        assert!(s.parked[0].was_paused);
        assert!(!s.park(5, false), "nothing is current any more");
    }

    #[test]
    fn a_deferred_update_ends_as_deferred_not_cancelled() {
        let mut b = board();
        merge_pending(&mut b.jobs, &[("wuwa".to_string(), "update".to_string())], &[]);
        let (job, snaps) = b
            .apply(
                "wuwa",
                "download-progress",
                &Phase::Deferred.into(),
                &json!({ "status": "Deferred", "percentage": 0 }),
                &live(Some(10), Some("update")),
            )
            .expect("event accepted");
        assert_eq!(job.phase, Phase::Deferred);
        assert!(job.error.is_none());
        assert_eq!(snaps[0]["jobs"][0]["phase"], json!("deferred"));
        assert_eq!(snaps[1]["jobs"], json!([]));
    }

    #[test]
    fn an_empty_queue_is_idle_and_pending_runs_in_order() {
        assert!(matches!(state(Vec::new(), Vec::new()).pick_next(false), Pick::Idle));
        let s = state(vec![pending(1, "a", "download"), pending(2, "b", "verify")], Vec::new());
        assert!(matches!(s.pick_next(false), Pick::Fresh(0)));
    }

    #[test]
    fn launch_blockers_are_the_games_own_file_changing_ops() {
        let mut s = state(
            vec![pending(2, "zzz", "verify"), pending(3, "wuwa", "move")],
            vec![meta(1, "genshin", "download")],
        );
        s.current = Some(meta(4, "hsr", "repair"));
        assert_eq!(s.launch_blocker("genshin"), Some("download"));
        assert_eq!(s.launch_blocker("wuwa"), Some("move"));
        assert_eq!(s.launch_blocker("hsr"), Some("repair"));
        assert_eq!(s.launch_blocker("zzz"), None);
        assert_eq!(s.launch_blocker("bd2"), None);
    }

    #[test]
    fn download_checks_count_current_pending_and_parked() {
        let mut s = state(vec![pending(2, "zzz", "download")], vec![meta(1, "wuwa", "download")]);
        s.current = Some(meta(3, "genshin", "verify"));
        assert!(s.download_in_flight("wuwa"));
        assert!(!s.download_in_flight("zzz"));
        assert!(s.has_download_for("zzz"));
        assert!(!s.has_download_for("genshin"));
        s.current = Some(meta(5, "genshin", "download"));
        assert!(s.download_in_flight("genshin"));
    }

    fn job(phase: Phase, downloaded: f64) -> Job {
        Job {
            game_id: "wuwa".to_string(),
            kind: "update".to_string(),
            base: "update".to_string(),
            phase,
            total: 10.0 * GIB,
            downloaded,
            percent: 0.0,
            speed: 0.0,
            network_speed: 0.0,
            disk_speed: 0.0,
            eta_secs: 0.0,
            paused: false,
            error: None,
            message: None,
            warning: false,
            parked: false,
            waiting_network: false,
            held_for_game: false,
        }
    }

    #[test]
    fn merged_snapshots_flag_parked_jobs_and_clear_the_flag_once_they_leave() {
        let mut jobs = vec![job(Phase::Downloading, 0.0)];
        merge_pending(
            &mut jobs,
            &[("zzz".to_string(), "install".to_string())],
            &["wuwa".to_string()],
        );
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].parked);
        assert_eq!(jobs[1].phase, Phase::Queued);
        assert!(!jobs[1].parked);
        assert_eq!(jobs[0].to_json()["parked"], json!(true));
        merge_pending(&mut jobs, &[], &[]);
        assert!(!jobs[0].parked);
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn throughput_logs_on_the_heartbeat_and_on_flips_only() {
        let t0 = Instant::now();
        let mut t = Transfer::new(&job(Phase::Downloading, 0.0), false, false, t0);
        assert!(t
            .observe(&job(Phase::Downloading, 100.0 * MIB), false, false, t0 + Duration::from_secs(30))
            .is_none());
        let (rate, elapsed) = t
            .observe(&job(Phase::Downloading, 600.0 * MIB), false, false, t0 + Duration::from_secs(60))
            .unwrap();
        assert_eq!(elapsed, Duration::from_secs(60));
        assert!((rate - 10.0).abs() < 1e-9);
        assert!(t
            .observe(&job(Phase::Downloading, 650.0 * MIB), false, true, t0 + Duration::from_secs(65))
            .is_some());
        assert!(t
            .observe(&job(Phase::Downloading, 700.0 * MIB), true, true, t0 + Duration::from_secs(70))
            .is_some());
    }

    #[test]
    fn throughput_restarts_its_window_on_a_phase_change_and_counts_download_bytes_only() {
        let t0 = Instant::now();
        let mut t = Transfer::new(&job(Phase::Downloading, 0.0), false, false, t0);
        t.observe(&job(Phase::Downloading, 300.0 * MIB), false, false, t0 + Duration::from_secs(30));
        assert!(t
            .observe(&job(Phase::Verifying, 0.0), false, false, t0 + Duration::from_secs(40))
            .is_none());
        assert_eq!(t.since, t0 + Duration::from_secs(40));
        t.observe(&job(Phase::Verifying, 900.0 * MIB), false, false, t0 + Duration::from_secs(50));
        assert!((t.moved - 300.0 * MIB).abs() < 1e-6);
        assert_eq!(t.active, Duration::from_secs(40));
    }

    #[test]
    fn throughput_logs_a_sharp_drop() {
        let t0 = Instant::now();
        let mut t = Transfer::new(&job(Phase::Downloading, 0.0), false, false, t0);
        t.observe(&job(Phase::Downloading, 6000.0 * MIB), false, false, t0 + Duration::from_secs(60))
            .expect("heartbeat line");
        let (rate, _) = t
            .observe(&job(Phase::Downloading, 6005.0 * MIB), false, false, t0 + Duration::from_secs(72))
            .expect("drop line");
        assert!(rate < 10.0);
    }

    #[test]
    fn job_outcomes_read_the_result_fields() {
        assert_eq!(outcome_of(&json!({ "success": true })), "completed");
        assert_eq!(outcome_of(&json!({ "success": false, "cancelled": true })), "cancelled");
        assert_eq!(
            outcome_of(&json!({ "success": false, "error": "Disk full." })),
            "failed (Disk full)"
        );
        assert_eq!(outcome_of(&Value::Null), "finished");
        assert_eq!(
            outcome_of(&json!({ "success": false, "deferredBusy": "A game is running." })),
            "deferred"
        );
        assert_eq!(format_elapsed(Duration::from_secs(1384)), "23m04s");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h02m05s");
    }
}
