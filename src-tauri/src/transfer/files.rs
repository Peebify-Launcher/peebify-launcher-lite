// ------------ File Transfers ------------
// Downloads a list of files into parts, splits the biggest one for idle connections, verifies every byte as it lands and hands finished files to the ledger.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use parking_lot::Mutex;

use crate::control::RunGate;
use crate::fsx::{self, ContentHasher, FailureKind};
use crate::ledger::{InProgress, Ledger, ProgressToken};
use crate::net;
use crate::part::{self, PartIdentity, Sidecar, Span};
use crate::progress::{FileCounter, ProgressTracker};
use crate::retry::{is_protocol_error, pushes_back, NetWait};
use crate::runtime::{Slot, TransferRuntime};

pub const MIRROR_MOVED: &str = "The download moved to a faster mirror.";
pub const CANCELLED: &str = "Download aborted by user.";
pub const HOST_UNREACHABLE: &str = "Could not reach the download server";
const PAUSED: &str = "The download was paused.";
const RESTARTING: &str = "The file starts over.";
const MIRROR_MOVE_MIN_LEFT: u64 = 32 << 20;
const MAX_VALIDATION_REFETCHES: u32 = 2;
const PROGRESS_RESETS_RETRIES_BYTES: u64 = 1 << 20;
const WRITE_BATCH: usize = 1 << 20;
const SPLIT_ALIGN: u64 = 1 << 20;
const IDLE_RECHECK: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct FileSpec {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub md5: String,
    pub url: String,
    pub live: bool,
    pub source_offset: u64,
    pub target: Target,
}

#[derive(Default)]
pub enum Target {
    #[default]
    Part,
    InPlace(Arc<SharedFile>, u64),
}

pub struct SharedFile {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    marker: Option<PathBuf>,
    before_open: Option<Arc<dyn Fn() + Send + Sync>>,
    handle: Mutex<Option<Arc<std::fs::File>>>,
    ranges_left: AtomicUsize,
}

impl SharedFile {
    pub fn new(
        id: String,
        path: PathBuf,
        size: u64,
        ranges: usize,
        marker: Option<PathBuf>,
        before_open: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            path,
            size,
            marker,
            before_open,
            handle: Mutex::new(None),
            ranges_left: AtomicUsize::new(ranges),
        })
    }

    fn open(&self) -> Result<Arc<std::fs::File>, String> {
        let mut handle = self.handle.lock();
        if let Some(file) = handle.as_ref() {
            return Ok(Arc::clone(file));
        }
        if let Some(note) = &self.before_open {
            note();
        }
        if let Some(marker) = &self.marker {
            std::fs::File::create(marker)
                .map_err(|e| fsx::fmt_io(&format!("Could not create {}", marker.display()), &e))?;
        }
        let file = fsx::retry_while_locked(|| {
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&self.path)
        })
        .map_err(|e| fsx::fmt_io(&format!("Could not open {}", self.path.display()), &e))?;
        file.set_len(self.size).map_err(|e| {
            fsx::explain_disk_full(
                &self.path,
                self.size,
                fsx::fmt_io(&format!("Could not size {}", self.path.display()), &e),
            )
        })?;
        let file = Arc::new(file);
        *handle = Some(Arc::clone(&file));
        Ok(file)
    }

    fn range_done(&self, ledger: &Ledger, tracker: &ProgressTracker) {
        if self.ranges_left.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        if let Some(file) = self.handle.lock().take() {
            ledger.complete_in_place(file, self.path.clone());
        }
        if let Some(marker) = &self.marker {
            ledger.clear_marker(marker.clone());
        }
        tracker.file_done(&self.id);
    }
}

pub trait JobHooks: Send + Sync {
    fn gate(&self) -> &RunGate;
    fn tracker(&self) -> &ProgressTracker;
    fn install_dir(&self) -> &Path;
    fn route(&self, url: &str) -> String {
        url.to_string()
    }
    fn route_epoch(&self) -> u64 {
        0
    }
    fn on_refused(&self, _url: &str) -> bool {
        false
    }
    fn on_outage(&self, _url: &str) -> bool {
        false
    }
    fn reaches_home(&self) -> BoxFuture<'static, bool>;
    fn can_reach(&self, url: &str) -> BoxFuture<'static, bool> {
        let target = net::host_and_port(url);
        let home = self.reaches_home();
        Box::pin(async move {
            match target {
                Some((host, port)) => net::connects(&host, port).await,
                None => home.await,
            }
        })
    }
    fn network_waiting(&self, _waiting: bool) {}
    fn verified(&self, _id: &str) {}
    fn locked_message(&self, error: &str) -> String {
        error.to_string()
    }
}

#[derive(Debug, Default, Clone)]
pub struct TransferReport {
    pub files: usize,
    pub bytes: u64,
    pub splits: u64,
    pub read_back: u64,
}

struct Seg {
    start: u64,
    state: Mutex<SegState>,
}

struct SegState {
    pos: u64,
    written: u64,
    end: u64,
}

impl Seg {
    fn new(start: u64, end: u64) -> Arc<Self> {
        Arc::new(Self {
            start,
            state: Mutex::new(SegState {
                pos: start,
                written: start,
                end,
            }),
        })
    }
}

#[derive(Default)]
struct VerifierState {
    hasher: Option<ContentHasher>,
    busy: bool,
    watermark: u64,
    ahead: BTreeMap<u64, u64>,
}

struct Verifier {
    expected: String,
    base: u64,
    state: Mutex<VerifierState>,
    catch_ups: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    read_back: Arc<AtomicU64>,
}

impl Verifier {
    fn new(expected: &str, base: u64, read_back: Arc<AtomicU64>) -> Arc<Self> {
        let v = Arc::new(Self {
            expected: expected.to_string(),
            base,
            state: Mutex::new(VerifierState::default()),
            catch_ups: Mutex::new(Vec::new()),
            read_back,
        });
        v.reset();
        v
    }

    fn enabled(&self) -> bool {
        !self.expected.is_empty()
    }

    fn reset(&self) {
        let mut s = self.state.lock();
        *s = VerifierState::default();
        if self.enabled() {
            s.hasher = Some(ContentHasher::for_expected(&self.expected));
        }
    }

    fn on_disk(self: &Arc<Self>, file: &Arc<std::fs::File>, spans: &[Span]) {
        if !self.enabled() {
            return;
        }
        let mut s = self.state.lock();
        for span in spans {
            insert_ahead(&mut s.ahead, span.start, span.end);
        }
        self.maybe_catch_up(s, file);
    }

    fn wrote(self: &Arc<Self>, file: &Arc<std::fs::File>, offset: u64, data: &[u8]) {
        if !self.enabled() || data.is_empty() {
            return;
        }
        let end = offset + data.len() as u64;
        let mut s = self.state.lock();
        if !s.busy && offset == s.watermark {
            if let Some(h) = s.hasher.as_mut() {
                h.update(data);
            }
            s.watermark = end;
        } else if end > s.watermark {
            let start = offset.max(s.watermark);
            insert_ahead(&mut s.ahead, start, end);
        }
        self.maybe_catch_up(s, file);
    }

    fn maybe_catch_up(
        self: &Arc<Self>,
        mut s: parking_lot::MutexGuard<'_, VerifierState>,
        file: &Arc<std::fs::File>,
    ) {
        if s.busy || s.ahead.first_key_value().map(|(k, _)| *k) != Some(s.watermark) {
            return;
        }
        s.busy = true;
        drop(s);
        let me = Arc::clone(self);
        let file = Arc::clone(file);
        let handle = tokio::task::spawn_blocking(move || me.catch_up(&file));
        self.catch_ups.lock().push(handle);
    }

    fn catch_up(&self, file: &std::fs::File) {
        let mut hasher = self.state.lock().hasher.take();
        loop {
            let next = {
                let mut s = self.state.lock();
                let watermark = s.watermark;
                match s.ahead.first_key_value().map(|(k, v)| (*k, *v)) {
                    Some((start, end)) if start == watermark => {
                        s.ahead.remove(&start);
                        Some((start, end))
                    }
                    _ => {
                        s.hasher = hasher.take();
                        s.busy = false;
                        None
                    }
                }
            };
            let Some((start, end)) = next else {
                return;
            };
            let read = fsx::with_hash_buf(|buf| -> std::io::Result<()> {
                let mut at = start;
                while at < end {
                    let n = ((end - at) as usize).min(buf.len());
                    fsx::read_exact_at(file, &mut buf[..n], self.base + at)?;
                    if let Some(h) = hasher.as_mut() {
                        h.update(&buf[..n]);
                    }
                    at += n as u64;
                }
                Ok(())
            });
            self.read_back.fetch_add(end - start, Ordering::Relaxed);
            let mut s = self.state.lock();
            match read {
                Ok(()) => s.watermark = end,
                Err(e) => {
                    log::warn!("Could not read back bytes to verify them: {e}");
                    s.hasher = None;
                    s.busy = false;
                    return;
                }
            }
        }
    }

    async fn finish(self: &Arc<Self>, file: Option<Arc<std::fs::File>>, size: u64) -> Option<String> {
        loop {
            let pending: Vec<_> = std::mem::take(&mut *self.catch_ups.lock());
            if pending.is_empty() {
                break;
            }
            for handle in pending {
                let _ = handle.await;
            }
        }
        if let Some(file) = file {
            let me = Arc::clone(self);
            let _ = tokio::task::spawn_blocking(move || {
                let busy = {
                    let mut s = me.state.lock();
                    let ready = !s.busy
                        && s.ahead.first_key_value().map(|(k, _)| *k) == Some(s.watermark);
                    if ready {
                        s.busy = true;
                    }
                    ready
                };
                if busy {
                    me.catch_up(&file);
                }
            })
            .await;
        }
        let mut s = self.state.lock();
        if s.watermark != size || !s.ahead.is_empty() {
            return None;
        }
        s.hasher.take().map(ContentHasher::finish)
    }
}

fn insert_ahead(ahead: &mut BTreeMap<u64, u64>, mut start: u64, mut end: u64) {
    if let Some((&prev_start, &prev_end)) = ahead.range(..=start).next_back() {
        if prev_end >= start {
            start = prev_start;
            end = end.max(prev_end);
            ahead.remove(&prev_start);
        }
    }
    while let Some((&next_start, &next_end)) = ahead.range(start..).next() {
        if next_start > end {
            break;
        }
        end = end.max(next_end);
        ahead.remove(&next_start);
    }
    ahead.insert(start, end);
}

struct FileInner {
    segs: Vec<Arc<Seg>>,
    prior: Vec<Span>,
    identity: Option<PartIdentity>,
    handle: Option<Arc<std::fs::File>>,
    token: Option<ProgressToken>,
    sparse: bool,
    preallocated: bool,
    resumed: bool,
}

struct FileRun {
    spec: FileSpec,
    part: PathBuf,
    counter: FileCounter,
    inner: Mutex<FileInner>,
    verifier: Arc<Verifier>,
    segs_left: AtomicUsize,
    restart: AtomicBool,
    ranges_ok: AtomicBool,
    validation_failures: AtomicU32,
    tracked: bool,
    counted: AtomicU64,
}

impl FileRun {
    fn handle(&self) -> Option<Arc<std::fs::File>> {
        self.inner.lock().handle.clone()
    }

    fn shared(&self) -> Option<&Arc<SharedFile>> {
        match &self.spec.target {
            Target::InPlace(file, _) => Some(file),
            Target::Part => None,
        }
    }

    fn base(&self) -> u64 {
        match &self.spec.target {
            Target::InPlace(_, offset) => *offset,
            Target::Part => 0,
        }
    }

    fn count(&self, tracker: &ProgressTracker, bytes: u64) {
        let before = self.counted.swap(bytes, Ordering::SeqCst);
        tracker.add_to(&self.counter, bytes as i64 - before as i64);
    }

    fn count_more(&self, tracker: &ProgressTracker, bytes: u64) {
        self.counted.fetch_add(bytes, Ordering::SeqCst);
        tracker.add_to(&self.counter, bytes as i64);
    }

    fn identity(&self) -> Option<PartIdentity> {
        self.inner.lock().identity.clone()
    }

    fn splittable(&self) -> bool {
        if !self.ranges_ok.load(Ordering::SeqCst) || self.restart.load(Ordering::SeqCst) {
            return false;
        }
        let inner = self.inner.lock();
        inner.preallocated
            && (!self.spec.md5.is_empty()
                || inner.identity.as_ref().is_some_and(|i| i.etag.is_some()))
    }
}

impl InProgress for FileRun {
    fn path(&self) -> &Path {
        &self.part
    }

    fn handle(&self) -> Option<Arc<std::fs::File>> {
        FileRun::handle(self)
    }

    fn snapshot(&self) -> Option<Box<dyn FnOnce() + Send>> {
        if !self.tracked || self.restart.load(Ordering::SeqCst) {
            return None;
        }
        let inner = self.inner.lock();
        let mut spans = inner.prior.clone();
        for seg in &inner.segs {
            let written = seg.state.lock().written;
            spans.push(Span {
                start: seg.start,
                end: written,
            });
        }
        let sidecar = Sidecar {
            size: self.spec.size,
            durable: part::normalize(spans),
            identity: inner.identity.clone(),
        };
        let path = self.spec.path.clone();
        Some(Box::new(move || part::persist_sidecar(&path, &sidecar)))
    }
}

enum Unit {
    Open(Arc<FileRun>),
    Range(Arc<FileRun>, Arc<Seg>),
    Split {
        file: Arc<FileRun>,
        parent: Arc<Seg>,
        at: u64,
        end: u64,
    },
}

struct Transfer {
    rt: Arc<TransferRuntime>,
    hooks: Arc<dyn JobHooks>,
    ledger: Arc<Ledger>,
    queue: Mutex<VecDeque<Unit>>,
    in_flight: Mutex<Vec<Arc<FileRun>>>,
    active: AtomicUsize,
    work: tokio::sync::Notify,
    abort: AtomicBool,
    failure: Mutex<Option<String>>,
    net_wait: NetWait,
    splits: AtomicU64,
    read_back: Arc<AtomicU64>,
    bytes: AtomicU64,
}

pub async fn download_files(
    rt: &Arc<TransferRuntime>,
    hooks: Arc<dyn JobHooks>,
    ledger: &Arc<Ledger>,
    mut files: Vec<FileSpec>,
) -> Result<TransferReport, String> {
    let count = files.len();
    create_parent_dirs(&files).await?;
    files.sort_by_key(|f| std::cmp::Reverse(f.size));
    let read_back = Arc::new(AtomicU64::new(0));
    let runs: VecDeque<Unit> = files
        .into_iter()
        .map(|spec| Unit::Open(new_run(spec, hooks.tracker(), &read_back)))
        .collect();
    let me = Arc::new(Transfer {
        rt: Arc::clone(rt),
        hooks,
        ledger: Arc::clone(ledger),
        queue: Mutex::new(runs),
        in_flight: Mutex::new(Vec::new()),
        active: AtomicUsize::new(0),
        work: tokio::sync::Notify::new(),
        abort: AtomicBool::new(false),
        failure: Mutex::new(None),
        net_wait: NetWait::default(),
        splits: AtomicU64::new(0),
        read_back,
        bytes: AtomicU64::new(0),
    });

    let checkpoints = tokio::spawn(Arc::clone(ledger).run(rt.config.checkpoint_interval));
    let workers: Vec<_> = (0..rt.connections().max(1))
        .map(|_| {
            let me = Arc::clone(&me);
            tokio::spawn(async move { me.worker().await })
        })
        .collect();
    let mut first_error: Option<String> = None;
    for worker in workers {
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                first_error.get_or_insert(e);
            }
            Err(e) => {
                first_error.get_or_insert(format!("download worker panicked: {e}"));
            }
        }
    }
    checkpoints.abort();
    let first_error = me.failure.lock().clone().or(first_error);

    if rt.config.skip_final_checkpoint {
        return Err(first_error.unwrap_or_else(|| "stopped before the final checkpoint".to_string()));
    }

    let sealed = ledger.checkpoint().await.and_then(|()| match ledger.failure() {
        Some(e) => Err(e),
        None => Ok(()),
    });
    if let Some(e) = first_error {
        return Err(e);
    }
    if let Err(e) = sealed {
        return Err(if fsx::classify(&e) == FailureKind::Locked {
            me.hooks.locked_message(&e)
        } else {
            e
        });
    }
    Ok(TransferReport {
        files: count,
        bytes: me.bytes.load(Ordering::Relaxed),
        splits: me.splits.load(Ordering::Relaxed),
        read_back: me.read_back.load(Ordering::Relaxed),
    })
}

fn new_run(spec: FileSpec, tracker: &ProgressTracker, read_back: &Arc<AtomicU64>) -> Arc<FileRun> {
    let (tracked, part, base) = match &spec.target {
        Target::Part => (
            part::records_identity(spec.size, &spec.md5),
            part::part_path_for(&spec.path),
            0,
        ),
        Target::InPlace(file, offset) => (false, file.path.clone(), *offset),
    };
    Arc::new(FileRun {
        part,
        counter: tracker.counter(&spec.id),
        verifier: Verifier::new(&spec.md5, base, Arc::clone(read_back)),
        inner: Mutex::new(FileInner {
            segs: Vec::new(),
            prior: Vec::new(),
            identity: None,
            handle: None,
            token: None,
            sparse: false,
            preallocated: false,
            resumed: false,
        }),
        segs_left: AtomicUsize::new(0),
        restart: AtomicBool::new(false),
        ranges_ok: AtomicBool::new(false),
        validation_failures: AtomicU32::new(0),
        tracked,
        counted: AtomicU64::new(0),
        spec,
    })
}

async fn create_parent_dirs(files: &[FileSpec]) -> Result<(), String> {
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .filter_map(|f| f.path.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    tokio::task::spawn_blocking(move || {
        for dir in dirs {
            std::fs::create_dir_all(&dir).map_err(|e| {
                fsx::fmt_io(
                    &format!("Write error: could not create {}", dir.display()),
                    &e,
                )
            })?;
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("folder task panicked: {e}"))?
}

enum Opened {
    Done,
    Ranges(Vec<Arc<Seg>>),
}

impl Transfer {
    fn gate(&self) -> &RunGate {
        self.hooks.gate()
    }

    fn is_cancelled(&self) -> bool {
        self.gate().is_cancelled()
    }

    fn fail(&self, error: String) {
        self.failure.lock().get_or_insert(error);
        self.abort.store(true, Ordering::SeqCst);
        self.work.notify_waiters();
    }

    fn push_front(&self, unit: Unit) {
        self.queue.lock().push_front(unit);
        self.work.notify_waiters();
    }

    fn push_back(&self, unit: Unit) {
        self.queue.lock().push_back(unit);
        self.work.notify_waiters();
    }

    async fn worker(self: Arc<Self>) -> Result<(), String> {
        loop {
            if self.abort.load(Ordering::SeqCst) {
                return Ok(());
            }
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if self.gate().is_stopped() {
                self.gate().wait_while_stopped().await;
                continue;
            }
            let notified = self.work.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let unit = self.queue.lock().pop_front().or_else(|| self.split_candidate());
            let Some(unit) = unit else {
                if self.active.load(Ordering::SeqCst) == 0 && self.queue.lock().is_empty() {
                    self.work.notify_waiters();
                    return Ok(());
                }
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(IDLE_RECHECK) => {}
                }
                continue;
            };
            self.active.fetch_add(1, Ordering::SeqCst);
            let outcome = self.run_unit(unit).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.work.notify_waiters();
            if let Err(e) = outcome {
                if e != CANCELLED {
                    self.fail(e.clone());
                }
                return Err(e);
            }
        }
    }

    async fn run_unit(self: &Arc<Self>, unit: Unit) -> Result<(), String> {
        match unit {
            Unit::Open(file) => {
                match self.open(&file).await? {
                    Opened::Done => Ok(()),
                    Opened::Ranges(mut segs) => {
                        self.in_flight.lock().push(Arc::clone(&file));
                        let first = segs.remove(0);
                        for seg in segs {
                            self.push_back(Unit::Range(Arc::clone(&file), seg));
                        }
                        self.run_range(&file, &first, None).await
                    }
                }
            }
            Unit::Range(file, seg) => self.run_range(&file, &seg, None).await,
            Unit::Split {
                file,
                parent,
                at,
                end,
            } => self.run_split(&file, &parent, at, end).await,
        }
    }

    async fn open(self: &Arc<Self>, file: &Arc<FileRun>) -> Result<Opened, String> {
        let url = self.hooks.route(&file.spec.url);
        let wanted = PartIdentity::expected(&url, file.spec.size, &file.spec.md5);
        let preallocate_min = self.rt.config.preallocate_min;
        let me = Arc::clone(file);
        let plan = tokio::task::spawn_blocking(move || plan_file(&me, wanted, preallocate_min))
            .await
            .map_err(|e| format!("open task panicked: {e}"))??;
        match plan {
            Plan::Verified => {
                log::info!("{}: a complete verified download is already on disk, finishing it.", file.spec.id);
                file.count(self.hooks.tracker(), file.spec.size);
                self.finish(file, true).await;
                Ok(Opened::Done)
            }
            Plan::Ranges { segs, prior, handle } => {
                let done: u64 = prior.iter().map(|s| s.end - s.start).sum();
                if done > 0 {
                    log::info!(
                        "{}: resuming with {done} of {} bytes already on disk.",
                        file.spec.id,
                        file.spec.size
                    );
                    self.hooks.tracker().reset_speed_baseline();
                }
                file.count(self.hooks.tracker(), done);
                file.verifier.on_disk(&handle, &prior);
                file.segs_left.store(segs.len(), Ordering::SeqCst);
                {
                    let mut inner = file.inner.lock();
                    inner.segs = segs.clone();
                    inner.prior = prior;
                }
                if file.tracked {
                    let token = self.ledger.track(Arc::clone(file) as Arc<dyn InProgress>);
                    file.inner.lock().token = Some(token);
                }
                if segs.is_empty() {
                    file.segs_left.store(1, Ordering::SeqCst);
                    self.range_ended(file).await?;
                    return Ok(Opened::Done);
                }
                Ok(Opened::Ranges(segs))
            }
        }
    }

    async fn run_range(
        self: &Arc<Self>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
        mut opened: Option<(reqwest::Response, String, Slot, Slot)>,
    ) -> Result<(), String> {
        let mut attempt = 0u32;
        let mut share_failures = 0u32;
        loop {
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if self.abort.load(Ordering::SeqCst) || file.restart.load(Ordering::SeqCst) {
                break;
            }
            if opened.is_none() && self.gate().is_stopped() {
                self.gate().wait_while_stopped().await;
                continue;
            }
            let (from, end) = {
                let mut st = seg.state.lock();
                st.pos = st.written;
                (st.written, st.end)
            };
            if from >= end {
                break;
            }
            let epoch = self.gate().transfer_epoch();
            let url = match &opened {
                Some((_, url, _, _)) => url.clone(),
                None => self.hooks.route(&file.spec.url),
            };
            let outcome = self.fetch(file, seg, &url, opened.take()).await;
            let advanced = seg.state.lock().written.saturating_sub(from);
            let e = match outcome {
                Ok(()) => {
                    self.net_wait.clear(&url);
                    self.rt.host_ok(&net::url_host(&url));
                    continue;
                }
                Err(e) => e,
            };
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if e == RESTARTING {
                break;
            }
            if e == MIRROR_MOVED {
                log::info!("{} continues on {}.", file.spec.id, net::url_host(&self.hooks.route(&file.spec.url)));
                continue;
            }
            if advanced >= PROGRESS_RESETS_RETRIES_BYTES {
                self.net_wait.clear(&url);
            }
            if net::permanent_client_status(&e).is_some() {
                if self.hooks.on_refused(&url) {
                    log::warn!("{} refused {} ({e}), trying another mirror.", net::url_host(&url), file.spec.id);
                    continue;
                }
                log::warn!("Download of {} refused by the server, not retrying: {e}", file.spec.id);
                return Err(e);
            }
            if is_host_outage(&e) {
                if self.hooks.on_outage(&url) {
                    self.hooks.tracker().reset_speed_baseline();
                    continue;
                }
                return Err(e);
            }
            let kind = fsx::classify(&e);
            if kind == FailureKind::DiskFull {
                if let Some(hint) = fsx::fat_limit_message(&file.spec.path, file.spec.size) {
                    return Err(format!("{hint} ({e})"));
                }
                return Err(format!(
                    "Not enough disk space while downloading {}. Free up space and try again. ({e})",
                    file.spec.id
                ));
            }
            if kind == FailureKind::AccessDenied {
                return Err(format!("{} ({e})", fsx::not_writable_message(self.hooks.install_dir())));
            }
            if kind == FailureKind::ShareLost {
                share_failures += 1;
                if share_failures < fsx::SHARE_LOST_ATTEMPTS {
                    log::warn!(
                        "The drive holding {} stopped responding ({share_failures}/{}), trying again: {e}",
                        file.spec.id,
                        fsx::SHARE_LOST_ATTEMPTS
                    );
                    tokio::time::sleep(net::retry_delay(share_failures, self.rt.config.retry_base_ms)).await;
                    continue;
                }
            }
            if let Some(message) =
                fsx::drive_failure_message(kind, self.hooks.install_dir(), &file.spec.path, file.spec.size)
            {
                return Err(format!("{message} ({e})"));
            }
            if self.gate().is_stopped() {
                self.gate().wait_while_stopped().await;
                continue;
            }
            if self.gate().transfer_epoch() != epoch || e == PAUSED {
                log::info!("Download of {} was interrupted by a pause, retrying without counting it: {e}", file.spec.id);
                continue;
            }
            if pushes_back(&e) {
                self.rt.host_overloaded(&net::url_host(&url));
            }
            if is_protocol_error(&e) {
                self.rt.prefer_multiplexed(&net::url_host(&url));
            }
            if kind == FailureKind::Network && !self.hooks.can_reach(&url).await {
                if let Err(waited) = self.net_wait.wait(&self.rt, self.hooks.as_ref(), &url).await {
                    if is_host_outage(&waited) && self.hooks.on_outage(&url) {
                        self.hooks.tracker().reset_speed_baseline();
                        continue;
                    }
                    return Err(waited);
                }
                self.hooks.tracker().reset_speed_baseline();
                continue;
            }
            if advanced >= PROGRESS_RESETS_RETRIES_BYTES && attempt > 0 {
                log::info!(
                    "{} advanced {advanced} bytes before failing, so its retry count starts over.",
                    file.spec.id
                );
                attempt = 0;
            }
            attempt += 1;
            if attempt >= self.rt.config.max_retries {
                return Err(e);
            }
            log::warn!("Download attempt {attempt} for {} failed: {e}. Retrying...", file.spec.id);
            tokio::time::sleep(net::retry_delay(attempt, self.rt.config.retry_base_ms)).await;
        }
        self.range_ended(file).await
    }

    async fn run_split(
        self: &Arc<Self>,
        file: &Arc<FileRun>,
        parent: &Arc<Seg>,
        at: u64,
        end: u64,
    ) -> Result<(), String> {
        let url = self.hooks.route(&file.spec.url);
        let host = net::url_host(&url);
        let Some((host_slot, slot)) = self.slots(&host).await else {
            return Ok(());
        };
        let request = self.request(file, &url, at, end, true);
        let Ok(response) = request.send().await else {
            return Ok(());
        };
        if response.status().as_u16() != 206
            || !content_range_starts(&response, file.spec.source_offset + at)
        {
            if response.status().as_u16() == 200 {
                file.ranges_ok.store(false, Ordering::SeqCst);
            }
            return Ok(());
        }
        {
            let mut st = parent.state.lock();
            if st.end != end || st.pos > at || file.restart.load(Ordering::SeqCst) {
                return Ok(());
            }
            st.end = at;
            file.segs_left.fetch_add(1, Ordering::SeqCst);
        }
        let child = Seg::new(at, end);
        file.inner.lock().segs.push(Arc::clone(&child));
        self.splits.fetch_add(1, Ordering::Relaxed);
        log::debug!("{}: a free connection took bytes {at}..{end}.", file.spec.id);
        self.run_range(file, &child, Some((response, url, host_slot, slot))).await
    }

    #[allow(clippy::type_complexity)]
    fn split_candidate(&self) -> Option<Unit> {
        let min_left = self.rt.config.split_min_left;
        let files = self.in_flight.lock().clone();
        let mut best: Option<(u64, Arc<FileRun>, Arc<Seg>, u64, u64)> = None;
        for file in files {
            if !file.splittable() {
                continue;
            }
            let segs = file.inner.lock().segs.clone();
            for seg in segs {
                let (pos, end) = {
                    let st = seg.state.lock();
                    (st.pos, st.end)
                };
                let left = end.saturating_sub(pos);
                if left > min_left && best.as_ref().is_none_or(|b| left > b.0) {
                    let mid = (pos + left / 2).div_ceil(SPLIT_ALIGN) * SPLIT_ALIGN;
                    if mid > pos && mid < end {
                        best = Some((left, Arc::clone(&file), seg, mid, end));
                    }
                }
            }
        }
        best.map(|(_, file, parent, at, end)| Unit::Split { file, parent, at, end })
    }

    async fn slots(&self, host: &str) -> Option<(Slot, Slot)> {
        let wait = self.rt.connection(host);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                slots = &mut wait => return Some(slots),
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if self.is_cancelled() || self.abort.load(Ordering::SeqCst) {
                        return None;
                    }
                }
            }
        }
    }

    fn request(&self, file: &FileRun, url: &str, from: u64, end: u64, ranged: bool) -> reqwest::RequestBuilder {
        let host = net::url_host(url);
        let mut request = self.rt.client_for(&host).get(url);
        if ranged {
            let offset = file.spec.source_offset;
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", offset + from, offset + end - 1),
            );
            if file.spec.md5.is_empty() {
                let wanted = PartIdentity::expected(url, file.spec.size, "");
                if let Some(validator) = file
                    .identity()
                    .filter(|r| r.same_host(&wanted))
                    .as_ref()
                    .and_then(PartIdentity::if_range)
                {
                    request = request.header(reqwest::header::IF_RANGE, validator);
                }
            }
        }
        request
    }

    async fn fetch(
        self: &Arc<Self>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
        url: &str,
        opened: Option<(reqwest::Response, String, Slot, Slot)>,
    ) -> Result<(), String> {
        let (from, end) = {
            let st = seg.state.lock();
            (st.pos, st.end)
        };
        let (response, slots) = match opened {
            Some((response, _, host_slot, slot)) => (response, (host_slot, slot)),
            None => {
                let host = net::url_host(url);
                let Some(slots) = self.slots(&host).await else {
                    return Err(CANCELLED.to_string());
                };
                let whole = from == 0
                    && end == file.spec.size
                    && file.spec.source_offset == 0
                    && file.shared().is_none()
                    && !file.inner.lock().resumed
                    && file.segs_left.load(Ordering::SeqCst) == 1;
                let offset = file.spec.source_offset;
                let response = self
                    .request(file, url, from, end, !whole)
                    .send()
                    .await
                    .map_err(|e| format!("Request error: {}", net::describe(&e)))?;
                let status = response.status().as_u16();
                net::note_protocol(url, response.version());
                match status {
                    200 if whole => {}
                    206 if !whole && content_range_starts(&response, offset + from) => {}
                    206 if !whole => {
                        self.restart_file(file, "the server sent an unexpected Content-Range");
                        return Err(format!("Server sent an unexpected Content-Range for {}", file.spec.id));
                    }
                    200 if file.shared().is_some() || offset > 0 => {
                        if !(offset == 0 && from == 0 && response.content_length() == Some(end)) {
                            return Err(format!(
                                "The download server sent the whole file instead of the requested part of {} ({url}).",
                                file.spec.id
                            ));
                        }
                    }
                    200 => {
                        file.ranges_ok.store(false, Ordering::SeqCst);
                        log::info!("Server ignored Range for {}, restarting the file.", file.spec.id);
                        self.restart_file(file, "the server ignored the range");
                        return Err(RESTARTING.to_string());
                    }
                    416 => {
                        self.restart_file(file, "the server could not continue it");
                        return Err(format!(
                            "The server could not continue {} from where it stopped, so it starts over.",
                            file.spec.id
                        ));
                    }
                    _ => return Err(format!("HTTP Error: {status} for URL {url}")),
                }
                if status == 206 || accepts_ranges(&response) {
                    file.ranges_ok.store(true, Ordering::SeqCst);
                    self.work.notify_waiters();
                }
                if from == 0 || file.identity().is_none() {
                    let identity = PartIdentity::expected(url, file.spec.size, &file.spec.md5)
                        .with_validators(response.headers());
                    let mut inner = file.inner.lock();
                    if inner.identity.is_none() || from == 0 {
                        inner.identity = Some(identity);
                    }
                }
                (response, slots)
            }
        };
        self.pump(file, seg, url, response, slots, end).await
    }

    async fn pump(
        self: &Arc<Self>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
        url: &str,
        response: reqwest::Response,
        slots: (Slot, Slot),
        requested_end: u64,
    ) -> Result<(), String> {
        let handle = file.handle().ok_or("Write error: the part file is not open")?;
        let mut writer = WriteBehind::new(Arc::clone(&handle), seg.state.lock().pos);
        let mut stream = response.bytes_stream();
        let mut epoch = self.hooks.route_epoch();
        let stall = self.rt.config.stall_timeout;
        let mut failure: Option<String> = None;
        loop {
            if self.is_cancelled() {
                failure = Some(CANCELLED.to_string());
                break;
            }
            if file.restart.load(Ordering::SeqCst) {
                failure = Some(RESTARTING.to_string());
                break;
            }
            if self.gate().is_stopped() {
                failure = Some(PAUSED.to_string());
                break;
            }
            let chunk = match tokio::time::timeout(stall, stream.next()).await {
                Err(_) => {
                    if self.gate().is_stopped() {
                        continue;
                    }
                    log::warn!(
                        "Stream for {} stalled for {}ms, tearing down to retry.",
                        file.spec.id,
                        stall.as_millis()
                    );
                    failure = Some("Download stalled. The connection went idle.".to_string());
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    failure = Some(format!("Stream error: {}", net::describe(&e)));
                    break;
                }
                Ok(Some(Ok(chunk))) => chunk,
            };
            self.rt.limiter.consume(chunk.len() as u64).await;
            self.hooks.tracker().add_network(chunk.len() as u64);
            let (take, cut) = {
                let mut st = seg.state.lock();
                let room = st.end.saturating_sub(st.pos);
                let len = chunk.len() as u64;
                if len > room && st.end >= requested_end {
                    failure = Some(format!(
                        "Server sent more bytes than requested for {}",
                        file.spec.id
                    ));
                    (0, true)
                } else {
                    let take = len.min(room);
                    st.pos += take;
                    (take as usize, take < len || st.pos >= st.end)
                }
            };
            if take > 0 {
                if let Err(e) = writer.push(&chunk[..take], self, file, seg).await {
                    failure = Some(e);
                    break;
                }
            }
            if cut {
                break;
            }
            let route_epoch = self.hooks.route_epoch();
            if route_epoch != epoch {
                epoch = route_epoch;
                let left = {
                    let st = seg.state.lock();
                    st.end.saturating_sub(st.pos)
                };
                if left > MIRROR_MOVE_MIN_LEFT && self.hooks.route(&file.spec.url) != url {
                    failure = Some(MIRROR_MOVED.to_string());
                    break;
                }
            }
        }
        drop(stream);
        drop(slots);
        let flushed = writer.finish(self, file, seg).await;
        if let Some(e) = failure {
            return Err(e);
        }
        flushed?;
        let st = seg.state.lock();
        if st.written < st.end {
            return Err(format!(
                "Stream error: the connection closed early ({} of {} bytes of {})",
                st.written - seg.start,
                st.end - seg.start,
                file.spec.id
            ));
        }
        Ok(())
    }

    async fn range_ended(self: &Arc<Self>, file: &Arc<FileRun>) -> Result<(), String> {
        if file.segs_left.fetch_sub(1, Ordering::SeqCst) != 1 {
            return Ok(());
        }
        if self.abort.load(Ordering::SeqCst) || self.is_cancelled() {
            return Ok(());
        }
        if file.restart.load(Ordering::SeqCst) {
            self.reset_file(file).await;
            return Ok(());
        }
        self.finish(file, false).await;
        Ok(())
    }

    async fn finish(self: &Arc<Self>, file: &Arc<FileRun>, already_verified: bool) {
        self.in_flight.lock().retain(|f| !Arc::ptr_eq(f, file));
        let size = file.spec.size;
        let handle = file.handle();
        let hashed = if already_verified {
            true
        } else if file.verifier.enabled() {
            match file.verifier.finish(handle.clone(), size).await {
                Some(got) if fsx::checksum_matches(&file.spec.md5, &got) => true,
                got => {
                    let got = got.unwrap_or_else(|| "an incomplete hash".to_string());
                    log::warn!(
                        "{} failed its checksum: expected {}, got {got}, {size} bytes from {}.",
                        file.spec.id,
                        file.spec.md5,
                        net::url_host(&self.hooks.route(&file.spec.url))
                    );
                    self.validation_failed(file).await;
                    return;
                }
            }
        } else if file.shared().is_some() {
            false
        } else {
            let len = handle
                .as_ref()
                .and_then(|h| h.metadata().ok())
                .map_or(0, |m| m.len());
            if len != size {
                log::warn!("{} is {len} bytes after download, expected {size}.", file.spec.id);
                self.validation_failed(file).await;
                return;
            }
            false
        };
        if let Some(shared) = file.shared() {
            file.inner.lock().handle = None;
            shared.range_done(&self.ledger, self.hooks.tracker());
            self.bytes.fetch_add(size, Ordering::Relaxed);
            return;
        }
        let (handle, token) = {
            let mut inner = file.inner.lock();
            if inner.sparse {
                if let Some(h) = inner.handle.as_ref() {
                    part::set_sparse(h, false);
                }
            }
            (inner.handle.take(), inner.token.take())
        };
        drop(token);
        let path = file.spec.path.clone();
        self.ledger.complete_part(
            handle,
            file.part.clone(),
            file.spec.path.clone(),
            file.spec.live,
            move || {
                part::remove_sidecar(&path);
                part::remove_identity(&path);
            },
        );
        if hashed {
            self.hooks.verified(&file.spec.id);
        }
        self.hooks.tracker().file_done(&file.spec.id);
        self.bytes.fetch_add(size, Ordering::Relaxed);
    }

    async fn validation_failed(self: &Arc<Self>, file: &Arc<FileRun>) {
        let failures = file.validation_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if failures > MAX_VALIDATION_REFETCHES {
            self.fail(format!(
                "The server sent a copy of {} that does not match its checksum. Try again later.",
                file.spec.id
            ));
            return;
        }
        log::warn!(
            "{} failed validation ({failures}/{MAX_VALIDATION_REFETCHES}), downloading it again.",
            file.spec.id
        );
        file.restart.store(true, Ordering::SeqCst);
        tokio::time::sleep(net::retry_delay(1, self.rt.config.retry_base_ms)).await;
        self.reset_file(file).await;
    }

    fn restart_file(&self, file: &FileRun, why: &str) {
        if !file.restart.swap(true, Ordering::SeqCst) {
            log::info!("{}: starting the file over because {why}.", file.spec.id);
        }
    }

    async fn reset_file(self: &Arc<Self>, file: &Arc<FileRun>) {
        self.in_flight.lock().retain(|f| !Arc::ptr_eq(f, file));
        let (handle, token) = {
            let mut inner = file.inner.lock();
            inner.segs.clear();
            inner.prior.clear();
            inner.identity = None;
            inner.resumed = false;
            inner.sparse = false;
            inner.preallocated = false;
            (inner.handle.take(), inner.token.take())
        };
        drop(token);
        drop(handle);
        let _ = file.verifier.finish(None, 0).await;
        file.verifier.reset();
        if file.shared().is_none() {
            let path = file.spec.path.clone();
            let _ = tokio::task::spawn_blocking(move || part::discard_part(&path)).await;
        }
        file.count(self.hooks.tracker(), 0);
        file.restart.store(false, Ordering::SeqCst);
        self.push_front(Unit::Open(Arc::clone(file)));
    }

}

enum Plan {
    Verified,
    Ranges {
        segs: Vec<Arc<Seg>>,
        prior: Vec<Span>,
        handle: Arc<std::fs::File>,
    },
}

fn plan_file(file: &FileRun, wanted: PartIdentity, preallocate_min: u64) -> Result<Plan, String> {
    if let Some(shared) = file.shared() {
        let handle = shared.open()?;
        let mut inner = file.inner.lock();
        inner.handle = Some(Arc::clone(&handle));
        inner.preallocated = true;
        let segs = if file.spec.size > 0 {
            vec![Seg::new(0, file.spec.size)]
        } else {
            Vec::new()
        };
        return Ok(Plan::Ranges {
            segs,
            prior: Vec::new(),
            handle,
        });
    }
    let size = file.spec.size;
    let md5 = file.spec.md5.as_str();
    let path = &file.spec.path;
    let part_len = std::fs::metadata(&file.part).ok().map(|m| m.len());
    let sidecar = part_len.and_then(|_| part::load_sidecar(path, size));
    let mut prior: Vec<Span> = Vec::new();
    let mut identity: Option<PartIdentity> = None;

    match (part_len, sidecar) {
        (Some(len), Some(s))
            if s.durable.iter().all(|span| span.end <= len)
                && s.identity.as_ref().is_none_or(|r| r.matches(&wanted)) =>
        {
            prior = s.durable;
            identity = s.identity;
        }
        (Some(_), Some(_)) => {
            log::info!("{}: the part on disk belongs to another version, starting over.", file.spec.id);
            part::discard_part(path);
        }
        (Some(len), None) if size > 0 && len < size && !md5.is_empty() => {
            let recorded = part::load_identity(path);
            if recorded.as_ref().is_none_or(|r| r.matches(&wanted)) {
                prior.push(Span { start: 0, end: len });
                identity = recorded;
            } else {
                log::info!("{}: the part on disk belongs to another version, starting over.", file.spec.id);
                part::discard_part(path);
            }
        }
        (Some(len), None) if size > 0 && len == size && !md5.is_empty() => {
            let got = fsx::checksum_file(&file.part, md5, &mut || false, &mut |_| {})?;
            if fsx::checksum_matches(md5, &got) {
                return Ok(Plan::Verified);
            }
            part::discard_part(path);
        }
        (Some(_), None) => part::discard_part(path),
        (None, _) => part::remove_sidecar(path),
    }
    prior.retain(|s| s.end > s.start);
    let resumed = !prior.is_empty();

    let handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(!resumed)
        .open(&file.part)
        .map_err(|e| fsx::fmt_io("Write error", &e))?;
    let mut sparse = false;
    let preallocated = size >= preallocate_min;
    if preallocated {
        let len = handle.metadata().map(|m| m.len()).unwrap_or(0);
        if len < size {
            sparse = part::set_sparse(&handle, true);
            handle
                .set_len(size)
                .map_err(|e| fsx::fmt_io("Write error", &e))?;
        }
    }
    let probe = Sidecar {
        size,
        durable: prior.clone(),
        identity: None,
    };
    let segs: Vec<Arc<Seg>> = probe
        .missing()
        .into_iter()
        .map(|span| Seg::new(span.start, span.end))
        .collect();
    let mut inner = file.inner.lock();
    inner.identity = identity;
    inner.handle = Some(Arc::new(handle));
    inner.sparse = sparse || (preallocated && resumed);
    inner.preallocated = preallocated;
    inner.resumed = resumed;
    Ok(Plan::Ranges {
        segs,
        prior,
        handle: Arc::clone(inner.handle.as_ref().expect("just opened")),
    })
}

struct WriteBehind {
    handle: Arc<std::fs::File>,
    buf: Vec<u8>,
    spare: Option<Vec<u8>>,
    offset: u64,
    pending: Option<tokio::task::JoinHandle<Result<Vec<u8>, String>>>,
}

impl WriteBehind {
    fn new(handle: Arc<std::fs::File>, offset: u64) -> Self {
        Self {
            handle,
            buf: Vec::with_capacity(WRITE_BATCH),
            spare: None,
            offset,
            pending: None,
        }
    }

    async fn push(
        &mut self,
        bytes: &[u8],
        transfer: &Arc<Transfer>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
    ) -> Result<(), String> {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= WRITE_BATCH {
            self.submit(transfer, file, seg).await?;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<(), String> {
        if let Some(pending) = self.pending.take() {
            let spare = pending
                .await
                .map_err(|e| format!("Write task panicked: {e}"))??;
            self.spare = Some(spare);
        }
        Ok(())
    }

    async fn submit(
        &mut self,
        transfer: &Arc<Transfer>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
    ) -> Result<(), String> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.wait().await?;
        let next = self
            .spare
            .take()
            .map(|mut v| {
                v.clear();
                v
            })
            .unwrap_or_else(|| Vec::with_capacity(WRITE_BATCH));
        let data = std::mem::replace(&mut self.buf, next);
        let offset = self.offset;
        self.offset += data.len() as u64;
        let handle = Arc::clone(&self.handle);
        let file = Arc::clone(file);
        let seg = Arc::clone(seg);
        let transfer = Arc::clone(transfer);
        self.pending = Some(tokio::task::spawn_blocking(move || {
            fsx::write_all_at(&handle, &data, file.base() + offset)
                .map_err(|e| fsx::fmt_io("Write error", &e))?;
            file.verifier.wrote(&handle, offset, &data);
            let len = data.len() as u64;
            seg.state.lock().written = offset + len;
            file.count_more(transfer.hooks.tracker(), len);
            transfer.hooks.tracker().add_disk(len);
            transfer.ledger.note_written(len);
            Ok(data)
        }));
        Ok(())
    }

    async fn finish(
        mut self,
        transfer: &Arc<Transfer>,
        file: &Arc<FileRun>,
        seg: &Arc<Seg>,
    ) -> Result<(), String> {
        self.submit(transfer, file, seg).await?;
        self.wait().await
    }
}

fn content_range_starts(response: &reqwest::Response, from: u64) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().starts_with(&format!("bytes {from}-")))
}

fn accepts_ranges(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("bytes"))
}

pub fn host_unreachable_message(host: &str) -> String {
    format!(
        "{HOST_UNREACHABLE} {host}. Your internet connection works, so the server may be down or blocked. Try again later."
    )
}

pub fn is_host_outage(message: &str) -> bool {
    message.starts_with(HOST_UNREACHABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahead_ranges_merge_with_their_neighbours() {
        let mut ahead = BTreeMap::new();
        insert_ahead(&mut ahead, 10, 20);
        insert_ahead(&mut ahead, 30, 40);
        insert_ahead(&mut ahead, 20, 30);
        assert_eq!(ahead.into_iter().collect::<Vec<_>>(), vec![(10, 40)]);
        let mut ahead = BTreeMap::new();
        insert_ahead(&mut ahead, 50, 60);
        insert_ahead(&mut ahead, 10, 20);
        insert_ahead(&mut ahead, 15, 55);
        assert_eq!(ahead.into_iter().collect::<Vec<_>>(), vec![(10, 60)]);
    }

    #[test]
    fn a_host_outage_names_the_host_and_is_not_read_as_a_network_blip() {
        let message = host_unreachable_message("cdn.example.com");
        assert!(is_host_outage(&message));
        assert!(message.contains("cdn.example.com"), "{message}");
        assert_eq!(fsx::classify(&message), FailureKind::Other);
    }
}
