// ------------ Install, Repair and Verify Engine ------------
// What installs, repairs and verifies share and how each one runs: the job core (profile, pause and cancel, keeping the PC
// awake, progress), the mirror set a job fetches from, the adapters the transfer core reports through and the disk space
// rules. Then the three operations, which reconcile a game's folder to its publisher's manifest, each with the steps only
// that game has supplied by its publisher.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use peebify_transfer::control::{self, RunGate};
use peebify_transfer::net::url_host;
use peebify_transfer::progress::{self, ProgressTracker};
use peebify_transfer::Sealed;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::extract::{self, remove_downloaded_packages, ExtractionProgress};
use super::fs_util::manifest_key;
use super::game_profiles::InstallMode;
use super::manifest::{self, Cx, Entry, Hash, Manifest, Publisher, Shape, Source};
use super::platform::TransferGuard;
use super::queue::{Phase, Update};
use super::validator::{self, clear_install_marker, prepare_install_dir, remove_part_files, undersized_files};
use super::{err_response, game_profiles, http, kuro, members, ok_with, BackendState};

pub mod status {
    pub const FETCHING_CONFIG: &str = "Fetching remote configuration...";
    pub const FETCHING_REPAIR_CONFIG: &str = "Fetching configuration...";
    pub const FETCHING_INDEX: &str = "Fetching file index...";
    pub const DOWNLOADING: &str = "Downloading...";
    pub const VALIDATING: &str = "Validating";
    pub const REPAIRING: &str = "Repairing";
    pub const UNPACKING: &str = "Unpacking the client package";
    pub const FINALIZING: &str = "Finalizing...";
    pub const PAUSED: &str = "Paused";
    pub const CANCELLED: &str = "Cancelled";
    pub const COMPLETED: &str = "Completed";
    pub const ERROR: &str = "Error";
    pub const WAITING_NETWORK: &str = "Waiting for connection...";
    pub const HELD_FOR_GAME: &str = "Paused while you play";
    pub const OFFLINE: &str = "Internet connection lost. It will resume automatically.";
}

pub(super) const SCAN_PHASE: Phase = Phase::Verifying;

// ------------ Error Wording ------------
// Turns raw network and server errors into plain advice; anything else is shown as it is.
pub(super) fn user_facing_error(error: &str) -> Option<String> {
    if let Some(rest) = error.strip_prefix("HTTP Error: ") {
        let code = rest.split_whitespace().next().unwrap_or_default();
        return Some(format!(
            "The download server answered with HTTP {code}. Press Retry, or try again later if it keeps happening."
        ));
    }
    (super::fs_util::classify(error) == super::fs_util::FailureKind::Network).then(|| {
        "Lost the connection to the download server. Check your internet connection and press Retry."
            .to_string()
    })
}

// ------------ Disk Space ------------
// Checks there is enough free space before a job writes, with headroom on top.
pub(super) const HEADROOM_INSTALL: u64 = 2 << 30;
pub(super) const HEADROOM_REPAIR: u64 = 256 << 20;
pub(super) const SPLIT_ARCHIVE_MULTIPLIER: f64 = 2.1;

pub(super) fn replacement_bytes(files: &[(u64, u64)], in_flight: usize, waiting_cap: u64) -> u64 {
    let growth: u64 = files
        .iter()
        .map(|(size, on_disk)| size.saturating_sub(*on_disk))
        .sum();
    let mut overlaps: Vec<u64> = files
        .iter()
        .map(|(size, on_disk)| (*size).min(*on_disk))
        .filter(|overlap| *overlap > 0)
        .collect();
    overlaps.sort_unstable_by(|a, b| b.cmp(a));
    let flying: u64 = overlaps.iter().take(in_flight).sum();
    let waiting: u64 = overlaps.iter().skip(in_flight).sum::<u64>().min(waiting_cap);
    growth.saturating_add(flying).saturating_add(waiting)
}

pub(super) fn sizes_on_disk(dir: &Path, files: &[(String, u64)]) -> Vec<(u64, u64)> {
    files
        .iter()
        .map(|(dest, size)| {
            let on_disk = super::fs_util::safe_join(dir, dest)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .filter(|m| m.is_file())
                .map_or(0, |m| m.len());
            (*size, on_disk)
        })
        .collect()
}

pub(super) fn archive_space(download: u64, unpacked: u64) -> (u64, f64) {
    match unpacked {
        0 => (download, SPLIT_ARCHIVE_MULTIPLIER),
        unpacked => (download.saturating_add(unpacked), 1.0),
    }
}

pub(super) fn required_free_bytes(write_bytes: u64, multiplier: f64, headroom_floor: u64) -> u64 {
    if write_bytes == 0 {
        return 0;
    }
    let scaled = (write_bytes as f64 * multiplier) as u64;
    let headroom = (write_bytes / 20).max(headroom_floor);
    scaled.saturating_add(headroom)
}

pub(super) fn ensure_disk_space(
    install_path: &Path,
    write_bytes: u64,
    multiplier: f64,
    headroom_floor: u64,
) -> Result<(), String> {
    if write_bytes == 0 {
        return Ok(());
    }
    let required = required_free_bytes(write_bytes, multiplier, headroom_floor);
    let Some(free) = super::fs_util::free_bytes_for(install_path) else {
        log::warn!(
            "Could not read the free space for {}, so the disk space check was skipped.",
            install_path.display()
        );
        return Ok(());
    };
    if free < required {
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        return Err(format!(
            "Not enough disk space in {}. This needs about {:.1} GB free but only {:.1} GB is available. Free up space and try again.",
            install_path.display(),
            gib(required),
            gib(free)
        ));
    }
    Ok(())
}

// ------------ Job Core ------------
// One game's install or repair: its profile, pause and cancel, the power guard that keeps the PC awake while it runs,
// and how it publishes progress. A verify borrows one too, for its progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JobKind {
    Install,
    Repair,
    Verify,
}

pub struct JobCore {
    pub(crate) app: AppHandle,
    pub(super) kind: JobKind,
    pub tracker: Arc<ProgressTracker>,
    pub(crate) gate: RunGate,
    profile: Mutex<&'static Value>,
    running: AtomicBool,
    starting: AtomicBool,
    power: Mutex<Weak<TransferGuard>>,
    patch: Mutex<Option<String>>,
}

impl JobCore {
    pub(super) fn new(app: AppHandle, kind: JobKind, profile: &'static Value) -> Arc<Self> {
        Self::with_gate(app, kind, profile, RunGate::new())
    }

    pub(super) fn verify(app: &AppHandle, profile: &'static Value, cancel: &Arc<AtomicBool>) -> Arc<Self> {
        let gate = RunGate::sharing(Arc::clone(cancel), false);
        Self::with_gate(app.clone(), JobKind::Verify, profile, gate)
    }

    fn with_gate(app: AppHandle, kind: JobKind, profile: &'static Value, gate: RunGate) -> Arc<Self> {
        Arc::new(Self {
            app,
            kind,
            tracker: Arc::new(ProgressTracker::new()),
            gate,
            profile: Mutex::new(profile),
            running: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            power: Mutex::new(Weak::new()),
            patch: Mutex::new(None),
        })
    }

    pub fn set_profile(&self, profile: &'static Value) {
        *self.profile.lock() = profile;
    }

    pub(crate) fn profile(&self) -> &'static Value {
        *self.profile.lock()
    }

    pub(crate) fn profile_id(&self) -> String {
        game_profiles::profile_id(self.profile()).to_string()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.gate.is_cancelled()
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.is_running() && self.gate.is_paused()
    }

    fn noun(&self) -> &'static str {
        match self.kind {
            JobKind::Install => "Download",
            JobKind::Repair => "Repair",
            JobKind::Verify => "Verify",
        }
    }

    pub(super) fn set_paused_power(&self, paused: bool) {
        if let Some(power) = self.power.lock().upgrade() {
            power.set_paused(paused);
        }
    }

    pub(super) fn set_offline_power(&self, offline: bool) {
        if let Some(power) = self.power.lock().upgrade() {
            power.set_offline(offline);
        }
    }

    pub fn prepare_start(&self) {
        if self.is_running() {
            return;
        }
        self.gate.reset();
        self.starting.store(true, Ordering::SeqCst);
    }

    pub fn abandon_start(&self) {
        self.starting.store(false, Ordering::SeqCst);
    }

    pub fn start_cancelled(&self) -> bool {
        self.starting.load(Ordering::SeqCst) && self.gate.is_cancelled()
    }

    pub(super) fn begin(&self) -> Option<Arc<TransferGuard>> {
        if self.running.swap(true, Ordering::SeqCst) {
            return None;
        }
        if !self.starting.swap(false, Ordering::SeqCst) {
            self.gate.reset();
        }
        self.tracker.reset();
        *self.patch.lock() = None;
        let power = Arc::new(TransferGuard::acquire());
        *self.power.lock() = Arc::downgrade(&power);
        if self.gate.is_paused() {
            power.set_paused(true);
        }
        Some(power)
    }

    pub(super) fn end(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.tracker.reset();
        *self.patch.lock() = None;
    }

    pub(super) fn cancel(&self) {
        if self.is_running() || self.starting.load(Ordering::SeqCst) {
            log::info!("Cancelling {} ({})...", self.noun().to_lowercase(), self.profile_id());
            self.gate.cancel();
        }
    }

    pub(super) fn keep_while_playing(&self) -> bool {
        if !self.is_running() {
            return false;
        }
        let verb = if self.kind == JobKind::Repair { "repairing" } else { "downloading" };
        log::info!("{} keeps {verb} while the game runs.", self.profile_id());
        self.gate.set_hold_override(true);
        true
    }

    pub(super) async fn cancellable<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        self.gate.cancellable(operation).await
    }

    pub(super) fn block_while_paused(&self) {
        while self.gate.is_paused() && !self.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    pub(super) fn lift_pause_for_unpacking(&self) {
        if self.gate.resume() {
            self.set_paused_power(false);
            log::info!("Unpacking cannot pause, so the pending pause was lifted.");
        }
    }

    pub(super) fn cancel_aware(&self, error: String) -> String {
        if self.is_cancelled() {
            "Download aborted by user.".to_string()
        } else {
            error
        }
    }

    pub(super) fn unpacking(self: &Arc<Self>, archive: &Path, total: u64, files: usize, label: String) -> Arc<dyn Fn(f64) + Send + Sync> {
        self.lift_pause_for_unpacking();
        self.tracker.begin("extracting", total as f64, files, None);
        self.tracker.force_next_update();
        let (job, archive) = (Arc::clone(self), archive.to_path_buf());
        let extraction = Mutex::new(ExtractionProgress::new(1, std::collections::HashMap::new()));
        let report = move |percent: f64| job.emit_extraction_progress(&extraction, &archive, percent, total as f64, &label);
        report(0.0);
        Arc::new(report)
    }

    pub(super) fn emit_extraction_progress(
        &self,
        extraction: &Mutex<ExtractionProgress>,
        archive: &Path,
        archive_percent: f64,
        total_bytes: f64,
        phase_text: &str,
    ) {
        let tick = extraction.lock().tick(archive, archive_percent, total_bytes);
        if archive_percent < 100.0 && !self.tracker.should_update_ui() {
            return;
        }
        let status = if self.kind == JobKind::Repair { status::UNPACKING } else { "Extracting packages..." };
        self.send_progress(
            Phase::Extracting,
            status,
            json!({
                "percentage": tick.percent,
                "speed": tick.speed,
                "networkSpeed": 0,
                "diskSpeed": tick.speed,
                "processedBytes": tick.unpacked,
                "totalBytes": total_bytes,
                "eta": tick.eta,
                "message": phase_text,
            }),
        );
    }

    pub(super) fn clear_unfinished_update(&self) {
        super::file_channels::clear_update_incomplete(&self.app, &self.profile_id());
    }

    pub(super) fn clear_update_cache(&self) {
        self.app
            .state::<BackendState>()
            .game
            .clear_update_cache(&self.profile_id());
    }

    // ------------ Progress ------------
    // Every job update goes to the queue with the game's id, and reads "Paused" while the job is paused.

    fn event(&self) -> &'static str {
        match self.kind {
            JobKind::Repair => "repair-progress",
            JobKind::Install | JobKind::Verify => "download-progress",
        }
    }

    pub(crate) fn publish(&self, update: impl Into<Update>, payload: Value) {
        let update = update.into();
        let paused = self.gate.is_paused() && !update.phase().is_terminal();
        let mut payload = payload;
        if let Some(map) = payload.as_object_mut() {
            map.insert("gameId".to_string(), json!(self.profile_id()));
            if paused {
                map.insert("status".to_string(), json!(status::PAUSED));
            }
        }
        super::queue::publish(&self.app, self.event(), update.paused(paused), payload);
    }

    pub(crate) fn send_progress(&self, update: impl Into<Update>, status_text: &str, extra: Value) {
        let mut payload = self.tracker.calculate_metrics();
        if let Some(map) = payload.as_object_mut() {
            map.insert("status".to_string(), json!(status_text));
            if let Value::Object(extra) = extra {
                map.extend(extra);
            }
        }
        self.publish(update, payload);
    }

    pub(super) fn set_patch(&self, version: &str) {
        *self.patch.lock() = Some(version.to_string());
    }

    pub(super) fn transfer_phase(&self) -> Phase {
        match self.kind {
            JobKind::Repair => Phase::Repairing,
            JobKind::Install | JobKind::Verify => Phase::Downloading,
        }
    }

    pub(crate) fn transfer_status(&self) -> String {
        match (self.kind, self.patch.lock().as_ref()) {
            (JobKind::Repair, _) => status::REPAIRING.to_string(),
            (_, Some(v)) => format!("Downloading Patch {v}"),
            (_, None) => status::DOWNLOADING.to_string(),
        }
    }

    pub(crate) fn report_transfer(&self) {
        self.send_progress(self.transfer_phase(), &self.transfer_status(), json!({}));
    }

    pub(super) fn report_finalizing(&self, percent: f64) {
        self.tracker.set_phase("finalizing");
        self.publish(
            Phase::Finalizing,
            json!({
                "status": status::FINALIZING,
                "percentage": percent,
                "speed": 0,
                "networkSpeed": 0,
                "diskSpeed": 0,
                "eta": 0,
            }),
        );
    }

    pub(super) fn report_scan(&self, status_text: &str) {
        match self.kind {
            JobKind::Install => {
                self.send_progress(Phase::Scanning, status_text, json!({ "speed": 0, "eta": 0 }))
            }
            JobKind::Repair => self.send_progress(SCAN_PHASE, status_text, json!({})),
            JobKind::Verify => self.send_progress(
                Update::new(Phase::Verifying).kind("verify"),
                status_text,
                json!({}),
            ),
        }
    }

    pub(super) fn report_offline(&self, message: &str) {
        if self.kind == JobKind::Verify {
            return;
        }
        self.set_offline_power(!message.is_empty());
        if message.is_empty() {
            self.tracker.reset_speed_baseline();
            self.report_transfer();
            return;
        }
        let update = Update::new(self.transfer_phase()).waiting_network(true);
        match self.kind {
            JobKind::Repair => self.publish(
                update,
                json!({ "status": status::REPAIRING, "speed": 0, "subStatus": message }),
            ),
            _ => self.send_progress(
                update,
                status::WAITING_NETWORK,
                json!({ "speed": 0, "subStatus": message }),
            ),
        }
    }

    pub(super) fn publish_while_transferring(self: &Arc<Self>) -> peebify_transfer::progress::Publisher {
        let me = Arc::clone(self);
        peebify_transfer::progress::Publisher::every(
            std::time::Duration::from_millis(peebify_transfer::progress::MIN_UI_UPDATE_INTERVAL_MS),
            move || {
                if me.gate.is_held() {
                    let update = Update::new(me.transfer_phase()).held_for_game(true);
                    match me.kind {
                        JobKind::Repair => me.publish(
                            update,
                            json!({ "status": status::HELD_FOR_GAME, "speed": 0 }),
                        ),
                        _ => me.send_progress(update, status::HELD_FOR_GAME, json!({ "speed": 0 })),
                    }
                } else if !me.gate.is_paused() {
                    me.report_transfer();
                }
            },
        )
    }

    pub(super) fn fail(&self, error: &str) -> Value {
        let noun = self.noun();
        if self.is_cancelled() {
            log::info!("{noun} cancelled ({}).", self.profile_id());
            self.send_progress(Phase::Cancelled, status::CANCELLED, json!({ "error": error }));
            return json!({ "success": false, "cancelled": true, "error": error });
        }
        log::error!("{noun} failed ({}): {error}", self.profile_id());
        let shown = user_facing_error(error).unwrap_or_else(|| error.to_string());
        self.send_progress(
            Phase::Error,
            status::ERROR,
            json!({ "error": shown, "detail": error }),
        );
        json!({ "success": false, "cancelled": false, "error": shown, "detail": error })
    }
}

// ------------ Mirrors ------------
// The servers one job may fetch from. URLs on any of them move onto the active one, and a server that is down or
// refuses a file hands over to the next one not tried yet.

pub(crate) fn url_origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

fn strip_base<'a>(url: &'a str, base: &str) -> Option<&'a str> {
    let base = base.trim_end_matches('/');
    let rest = url.strip_prefix(base)?;
    (rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')).then_some(rest)
}

pub(crate) fn mirror_rebase(url: &str, bases: &[String], to: &str) -> Option<String> {
    let (_, rest) = bases
        .iter()
        .filter_map(|base| strip_base(url, base).map(|rest| (base.len(), rest)))
        .max_by_key(|(len, _)| *len)?;
    Some(format!("{}{rest}", to.trim_end_matches('/')))
}

pub(crate) fn mirror_covers(url: &str, bases: &[String]) -> bool {
    bases.iter().any(|base| strip_base(url, base).is_some())
}

#[derive(Default)]
pub(crate) struct MirrorSet {
    state: Mutex<MirrorState>,
    epoch: AtomicU64,
}

#[derive(Default)]
struct MirrorState {
    bases: Vec<String>,
    active: Option<usize>,
    tried: HashSet<String>,
}

pub(crate) enum Switch {
    Stuck,
    Moved,
    To { failed: String, to: String },
}

impl MirrorSet {
    pub(crate) fn new(bases: Vec<String>, active: Option<usize>) -> Self {
        let set = Self::default();
        set.reset(bases, active);
        set
    }

    pub(crate) fn reset(&self, bases: Vec<String>, active: Option<usize>) {
        let active = active.map(|i| if i < bases.len() { i } else { 0 });
        *self.state.lock() = MirrorState {
            bases,
            active,
            tried: HashSet::new(),
        };
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn route(&self, url: &str) -> String {
        let state = self.state.lock();
        match state.active.and_then(|i| state.bases.get(i)) {
            Some(to) => mirror_rebase(url, &state.bases, to).unwrap_or_else(|| url.to_string()),
            None => url.to_string(),
        }
    }

    pub(crate) fn switch_from(&self, failed_url: &str) -> Switch {
        let failed = url_host(failed_url);
        let mut state = self.state.lock();
        if !mirror_covers(failed_url, &state.bases) {
            return Switch::Stuck;
        }
        let active_host = state.active.and_then(|i| state.bases.get(i)).map(|b| url_host(b));
        if active_host.is_some_and(|host| host != failed) {
            return Switch::Moved;
        }
        state.tried.insert(failed.clone());
        let Some(next) = state.bases.iter().position(|base| !state.tried.contains(&url_host(base))) else {
            return Switch::Stuck;
        };
        state.active = Some(next);
        let to = state.bases[next].clone();
        drop(state);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        Switch::To { failed, to }
    }
}

// ------------ Transfer Adapters ------------
// How the transfer core and the publishers' scanners reach a job: its pause and cancel, its progress, its network
// status and the mirrors it fetches from.

pub(crate) type BeforeWrite = Option<Arc<dyn Fn() + Send + Sync>>;

pub(crate) fn note_write(before_write: &BeforeWrite) {
    if let Some(note) = before_write {
        note();
    }
}

pub(super) struct TransferJob {
    core: Arc<JobCore>,
    install_dir: PathBuf,
    mirrors: Option<Arc<MirrorSet>>,
    verified: Option<Arc<Mutex<HashSet<String>>>>,
}

impl TransferJob {
    pub(super) fn new(core: &Arc<JobCore>, install_dir: &Path, mirrors: Option<Arc<MirrorSet>>) -> Self {
        Self {
            core: Arc::clone(core),
            install_dir: install_dir.to_path_buf(),
            mirrors,
            verified: None,
        }
    }

    pub(super) fn noting_verified(mut self, verified: Arc<Mutex<HashSet<String>>>) -> Self {
        self.verified = Some(verified);
        self
    }

    pub(super) fn shared(self) -> Arc<dyn peebify_transfer::JobHooks> {
        Arc::new(self)
    }

    fn switch_from(&self, url: &str) -> bool {
        let Some(mirrors) = &self.mirrors else {
            return false;
        };
        match mirrors.switch_from(url) {
            Switch::Stuck => false,
            Switch::Moved => true,
            Switch::To { failed, to } => {
                if self.core.kind == JobKind::Repair {
                    log::warn!("{failed} failed, so the repair continues from {}.", url_host(&to));
                } else {
                    log::warn!("{failed} is unreachable, switching the download to {}.", url_host(&to));
                    self.core.report_transfer();
                }
                true
            }
        }
    }
}

impl peebify_transfer::JobHooks for TransferJob {
    fn gate(&self) -> &RunGate {
        &self.core.gate
    }

    fn tracker(&self) -> &ProgressTracker {
        &self.core.tracker
    }

    fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    fn route(&self, url: &str) -> String {
        match &self.mirrors {
            Some(mirrors) => mirrors.route(url),
            None => url.to_string(),
        }
    }

    fn route_epoch(&self) -> u64 {
        self.mirrors.as_ref().map_or(0, |m| m.epoch())
    }

    fn on_refused(&self, url: &str) -> bool {
        self.switch_from(url)
    }

    fn on_outage(&self, url: &str) -> bool {
        self.switch_from(url)
    }

    fn reaches_home(&self) -> futures::future::BoxFuture<'static, bool> {
        Box::pin(http::reaches_home())
    }

    fn can_reach(&self, url: &str) -> futures::future::BoxFuture<'static, bool> {
        let url = url.to_string();
        Box::pin(async move { http::can_reach(&url).await })
    }

    fn network_waiting(&self, waiting: bool) {
        if waiting {
            http::note_unreachable();
        } else {
            http::note_reachable();
        }
        self.core.report_offline(if waiting { status::OFFLINE } else { "" });
    }

    fn verified(&self, id: &str) {
        if let Some(verified) = &self.verified {
            verified.lock().insert(id.to_string());
        }
    }

    fn locked_message(&self, error: &str) -> String {
        match self.core.kind {
            JobKind::Install => format!(
                "A game file is in use and could not be replaced. Close {} and press Update again. ({error})",
                game_profiles::display_name(self.core.profile())
            ),
            _ => error.to_string(),
        }
    }
}

pub(super) struct FileProgress {
    core: Arc<JobCore>,
    mirrors: Option<Arc<MirrorSet>>,
}

impl FileProgress {
    pub(super) fn transfer(core: &Arc<JobCore>) -> Self {
        Self {
            core: Arc::clone(core),
            mirrors: None,
        }
    }

    pub(super) fn on(mut self, mirrors: &Arc<MirrorSet>) -> Self {
        self.mirrors = Some(Arc::clone(mirrors));
        self
    }
}

impl control::Control for FileProgress {
    fn is_cancelled(&self) -> bool {
        self.core.is_cancelled()
    }

    fn gate(&self) -> Option<&RunGate> {
        Some(&self.core.gate)
    }
}

impl control::FileHooks for FileProgress {
    fn event(&self, event: control::FileEvent) {
        let tracker = &self.core.tracker;
        match event {
            control::FileEvent::Bytes { path, delta } => {
                tracker.update_file_progress(path, delta as f64, false);
                if tracker.should_update_ui() {
                    self.core.report_transfer();
                }
            }
            control::FileEvent::Written { delta } => tracker.add_disk(delta),
            control::FileEvent::FileDone { path } => {
                tracker.update_file_progress(path, 0.0, true);
                if self.core.kind == JobKind::Repair {
                    tracker.increment_repaired_files();
                }
            }
        }
    }

    fn rebase_url(&self, url: &str) -> Option<String> {
        let moved = self.mirrors.as_ref()?.route(url);
        (moved != url).then_some(moved)
    }

    fn status(&self, message: &str) {
        self.core.report_offline(message);
    }
}

// ------------ Install Manager ------------
// The object that owns one game's install: its pause, resume and cancel, and the entry point that starts an install
// or update, which reconciles the game's folder to its publisher's manifest: it checks what is there, fetches what is
// missing or wrong, checks that again, unpacks the archives and records the install.
const MAX_PACKAGE_REFETCH_ROUNDS: u32 = 3;

pub(super) fn unsupported_channel(version_type: &str) -> Option<String> {
    (version_type != kuro::LIVE_CHANNEL).then(|| {
        format!(
            "The '{version_type}' version cannot be installed yet. Only the live version is supported, so nothing was written to the game folder."
        )
    })
}

pub fn combine_url(base: &str, part: &str) -> String {
    let base = base.strip_suffix('/').unwrap_or(base);
    let part = part.strip_prefix('/').unwrap_or(part);
    format!("{base}/{part}")
}

pub type CompletionHook = Box<dyn FnOnce(std::path::PathBuf) + Send + 'static>;

pub struct GameDownloadManager {
    core: Arc<JobCore>,
    extracting: AtomicBool,
    pub(crate) mirrors: Arc<MirrorSet>,
    on_complete: Mutex<Option<CompletionHook>>,
    verified: Arc<Mutex<HashSet<String>>>,
    install_writes: InstallWrites,
}

impl std::ops::Deref for GameDownloadManager {
    type Target = JobCore;

    fn deref(&self) -> &JobCore {
        &self.core
    }
}

#[derive(Default)]
struct InstallWrites {
    in_place: AtomicBool,
    touched: AtomicBool,
    recording: Mutex<()>,
}

impl InstallWrites {
    fn begin(&self, in_place: bool) {
        self.in_place.store(in_place, Ordering::SeqCst);
        self.touched.store(false, Ordering::SeqCst);
    }

    fn end(&self) {
        self.in_place.store(false, Ordering::SeqCst);
    }

    fn before_write(&self, record: impl FnOnce()) {
        if !self.in_place.load(Ordering::SeqCst) || self.touched.load(Ordering::SeqCst) {
            return;
        }
        let _recording = self.recording.lock();
        if !self.touched.load(Ordering::SeqCst) {
            record();
            self.touched.store(true, Ordering::SeqCst);
        }
    }
}

pub(super) fn write_bytes(dir: &Path, files: &[Entry], waiting: u64) -> u64 {
    let (placed, rest): (Vec<&Entry>, Vec<&Entry>) = files.iter().partition(|f| f.in_place());
    let (members, replaced): (Vec<&Entry>, Vec<&Entry>) = rest.into_iter().partition(|f| matches!(f.source, Source::Member { .. }));
    let on_disk = |list: &[&Entry]| {
        let listed: Vec<(String, u64)> = list.iter().map(|f| (f.path.to_string(), f.size)).collect();
        sizes_on_disk(dir, &listed)
    };
    replacement_bytes(&on_disk(&placed), 0, 0)
        + replacement_bytes(&on_disk(&replaced), http::shared_transfer().connections(), waiting)
        + replacement_bytes(&on_disk(&members), members::MAX_EXTRACT_WORKERS, waiting)
}

pub(super) struct ExtractingGuard<'a>(&'a AtomicBool);

impl Drop for ExtractingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl GameDownloadManager {
    pub fn new(app: AppHandle, profile: &'static Value) -> Arc<Self> {
        Arc::new(Self {
            core: JobCore::new(app, JobKind::Install, profile),
            extracting: AtomicBool::new(false),
            mirrors: Arc::new(MirrorSet::default()),
            on_complete: Mutex::new(None),
            verified: Arc::new(Mutex::new(HashSet::new())),
            install_writes: InstallWrites::default(),
        })
    }

    pub fn is_extracting(&self) -> bool {
        self.extracting.load(Ordering::SeqCst)
    }

    pub fn pause_download(&self) {
        if self.is_extracting() {
            log::info!("Pause ignored because unpacking cannot be paused.");
            return;
        }
        if self.tracker.phase() == "finalizing" {
            log::info!("Pause ignored because the install is finalizing.");
            return;
        }
        if self.is_running() && self.gate.pause() {
            self.set_paused_power(true);
            self.send_progress(Phase::Downloading, status::PAUSED, json!({}));
            log::info!("Download paused ({}).", self.profile_id());
        }
    }

    pub fn resume_download(&self) {
        if self.is_running() && self.gate.resume() {
            self.set_paused_power(false);
            self.tracker.reset_speed_baseline();
            self.send_progress(
                Update::new(Phase::Downloading).resumed(),
                &self.transfer_status(),
                json!({}),
            );
            log::info!("Download resumed ({}).", self.profile_id());
        }
    }

    pub fn cancel_download(&self) {
        self.cancel();
    }

    pub fn keep_downloading_while_playing(&self) {
        if self.keep_while_playing() {
            self.report_transfer();
        }
    }

    pub async fn download_game_with(
        self: &Arc<Self>,
        install_path: &Path,
        in_place: bool,
        on_complete: Option<CompletionHook>,
    ) -> Value {
        let Some(_power) = self.begin() else {
            return err_response("A download is already in progress.");
        };
        self.install_writes.begin(in_place);
        *self.on_complete.lock() = on_complete;
        self.mirrors.reset(Vec::new(), None);
        log::info!(
            "Download for {} starting: mode {}, installed version {}, install path {}",
            self.profile_id(),
            game_profiles::install_mode(self.profile()).unwrap_or("default"),
            super::game_manager::local_game_version_for(
                self.profile(),
                &install_path.to_string_lossy()
            )
            .as_deref()
            .unwrap_or("none"),
            install_path.display()
        );
        let result = if self.is_cancelled() {
            Err("Download aborted by user.".to_string())
        } else {
            self.reconcile(install_path).await
        };

        self.end();
        self.install_writes.end();
        self.gate.clear_paused();
        self.on_complete.lock().take();

        match result {
            Ok(response) => response,
            Err(e) => self.fail(&e),
        }
    }

    // ------------ Manifest Install ------------
    // Reconciles the folder to the publisher's manifest: checks what is there, fetches what is missing or wrong, checks
    // that again and re-fetches what still fails, unpacks the archives, and records the install.
    async fn reconcile(self: &Arc<Self>, dir: &Path) -> Result<Value, String> {
        let profile = self.profile();
        if !game_profiles::is_managed(profile) {
            return Err("This game is not supported for managed install in Peebify Launcher yet.".to_string());
        }
        let publisher = manifest::publisher(profile);
        self.send_progress(Phase::Scanning, status::FETCHING_CONFIG, json!({}));
        let cx = Cx { app: &self.app, profile, dir, purpose: JobKind::Install, job: self.transfer_job(dir) };
        let manifest = self.cancellable(publisher.manifest(&cx)).await?;
        self.set_patch(&manifest.version);
        kuro::note_quality_switch(&self.app, profile, dir);
        self.install_mirrors(manifest.mirrors.clone());
        prepare_install_dir(dir, &self.profile_id())?;
        manifest.containers.iter().for_each(|c| extract::remove_other_packages(dir, c));

        self.verified.lock().clear();
        let files: Vec<Entry> = manifest.files().cloned().collect();
        let fetch = self.plan(&publisher, &files, &manifest.version, dir).await?;
        let mut sealed = Sealed::nothing_written();
        if fetch.is_empty() {
            log::info!("All files are valid for {}, no download needed.", self.profile_id());
        }
        if !fetch.is_empty() || !manifest.containers.is_empty() {
            self.ensure_room(&manifest, &fetch, dir).await?;
        }
        if !fetch.is_empty() {
            if !manifest.entries.is_empty() {
                validator::hold_record(dir, &manifest.version).await;
            }
            sealed = self.execute_download(fetch.clone(), dir).await?;
            sealed = self.verify_fetched(&files, fetch, sealed, dir).await?;
        }
        self.seal(publisher.as_ref(), manifest, &files, dir, sealed).await
    }

    async fn plan(
        &self,
        publisher: &Arc<dyn Publisher>,
        files: &[Entry],
        version: &str,
        dir: &Path,
    ) -> Result<Vec<Entry>, String> {
        let (path, listed, publisher) = (dir.to_path_buf(), files.to_vec(), Arc::clone(publisher));
        let triage = tauri::async_runtime::spawn_blocking(move || publisher.triage(&path, &listed))
            .await
            .map_err(|e| format!("install record task panicked: {e}"))?;
        let key = |f: &Entry| super::fs_util::manifest_key(&f.path);
        let (stale, check): (Vec<Entry>, Vec<Entry>) = files
            .iter()
            .filter(|f| !triage.trusted.contains(&key(f)))
            .cloned()
            .partition(|f| triage.stale.contains(&key(f)));
        if check.len() < files.len() {
            log::info!(
                "{}: {} of {} files are unchanged since the last verified install and skip hashing ({} of them changed in this version); checking {}.",
                self.profile_id(),
                files.len() - check.len(),
                files.len(),
                stale.len(),
                check.len()
            );
        }

        let status = format!("Checking files for Patch {version}...");
        let mut fetch: Vec<Entry> = validator::check_files(&self.core, dir, check, false, &status, false)
            .await?
            .into_iter()
            .map(|(file, _)| file)
            .collect();
        fetch.extend(stale);

        let total_size: u64 = fetch.iter().map(Entry::fetch_size).sum();
        self.tracker.reset();
        self.tracker.set_totals(total_size as f64, fetch.len());
        self.tracker
            .set_file_sizes(fetch.iter().map(|f| (f.path.to_string(), f.fetch_size() as f64)).collect());
        log::info!(
            "Files to download for {}: {}, Total size: {:.2}GB",
            self.profile_id(),
            fetch.len(),
            progress::gib(total_size as f64)
        );
        Ok(fetch)
    }

    async fn ensure_room(&self, manifest: &Manifest, fetch: &[Entry], dir: &Path) -> Result<(), String> {
        let download: u64 = fetch.iter().map(|f| f.size).sum();
        let (write_bytes, multiplier) = if manifest.containers.is_empty() {
            (self.update_write_bytes(dir, fetch).await?, 1.0)
        } else {
            let unpacked = manifest.listed.iter().map(|f| f.size).sum::<u64>() + manifest.containers.iter().map(|c| c.room).sum::<u64>();
            archive_space(download, unpacked)
        };
        ensure_disk_space(dir, write_bytes, multiplier, HEADROOM_INSTALL)?;
        let largest = fetch.iter().map(|f| f.size).max().unwrap_or(0);
        match super::fs_util::fat_limit_message(dir, largest) {
            Some(hint) => Err(hint),
            None => Ok(()),
        }
    }

    async fn update_write_bytes(&self, dir: &Path, files: &[Entry]) -> Result<u64, String> {
        let (dir, files) = (dir.to_path_buf(), files.to_vec());
        tauri::async_runtime::spawn_blocking(move || write_bytes(&dir, &files, peebify_transfer::ledger::CHECKPOINT_BYTES))
            .await
            .map_err(|e| format!("disk space task panicked: {e}"))
    }

    async fn verify_fetched(
        self: &Arc<Self>,
        files: &[Entry],
        mut pending: Vec<Entry>,
        mut sealed: Sealed,
        dir: &Path,
    ) -> Result<Sealed, String> {
        let mut round = 0u32;
        loop {
            if self.is_cancelled() {
                return Err("Download aborted by user.".to_string());
            }
            let verified = self.verified.lock().clone();
            let needs_hash: Vec<Entry> = pending
                .into_iter()
                .filter(|f| {
                    f.hash != Hash::None
                        && !f.in_place()
                        && !(matches!(f.source, Source::Url(_) | Source::Member { .. }) && verified.contains(&*f.path))
                })
                .collect();
            let mut invalid: Vec<Entry> = if needs_hash.is_empty() {
                log::info!("Every downloaded file was checksummed while streaming, so no full re-read is needed.");
                self.report_finalizing(0.0);
                Vec::new()
            } else {
                log::info!("Verifying {} file(s) that could not be hashed while streaming...", needs_hash.len());
                validator::check_files(&self.core, dir, needs_hash, false, "Verifying integrity...", true)
                    .await?
                    .into_iter()
                    .map(|(file, _)| file)
                    .collect()
            };

            if round == 0 {
                let flagged: HashSet<String> = invalid.iter().map(|f| f.path.to_string()).collect();
                let listed: Vec<(String, u64)> = files
                    .iter()
                    .filter(|f| !f.in_place() && !flagged.contains(&*f.path))
                    .map(|f| (f.path.to_string(), f.size))
                    .collect();
                let path = dir.to_path_buf();
                let short = tauri::async_runtime::spawn_blocking(move || undersized_files(&path, listed))
                    .await
                    .map_err(|e| format!("size check task panicked: {e}"))?;
                invalid.extend(files.iter().filter(|f| short.contains(&*f.path)).cloned());
            }

            if invalid.is_empty() {
                return Ok(sealed);
            }
            round += 1;
            if round > MAX_PACKAGE_REFETCH_ROUNDS {
                return Err(format!(
                    "Validation failed: {} file(s) are still corrupt after {MAX_PACKAGE_REFETCH_ROUNDS} re-fetch attempts.",
                    invalid.len()
                ));
            }

            let count = invalid.len();
            log::warn!(
                "Validation found {count} incomplete file(s); re-fetching them (round {round}/{MAX_PACKAGE_REFETCH_ROUNDS})."
            );
            self.tracker.reset();
            self.tracker.set_totals(invalid.iter().map(|f| f.fetch_size() as f64).sum(), count);
            self.tracker
                .set_file_sizes(invalid.iter().map(|f| (f.path.to_string(), f.fetch_size() as f64)).collect());
            self.send_progress(
                Phase::Downloading,
                &format!(
                    "Re-fetching {count} incomplete package{}...",
                    if count == 1 { "" } else { "s" }
                ),
                json!({}),
            );
            {
                let mut verified = self.verified.lock();
                for file in &invalid {
                    verified.remove(&*file.path);
                }
            }
            sealed = self.execute_download(invalid.clone(), dir).await?;
            pending = invalid;
        }
    }

    pub(super) async fn execute_download(
        self: &Arc<Self>,
        files: Vec<Entry>,
        install_path: &Path,
    ) -> Result<Sealed, String> {
        log::info!(
            "Starting download of {} files for {}...",
            files.len(),
            self.profile_id()
        );
        self.tracker.set_phase("downloading");
        self.report_transfer();

        let sealed = self.transfer_files(files, install_path).await?;
        if self.is_cancelled() {
            return Err("Download was cancelled by the user.".to_string());
        }
        Ok(sealed)
    }

    async fn transfer_files(self: &Arc<Self>, files: Vec<Entry>, install_path: &Path) -> Result<Sealed, String> {
        let _publisher = self.core.publish_while_transferring();
        let hooks = Arc::new(FileProgress::transfer(&self.core).on(&self.mirrors));
        manifest::fetch_entries(install_path, &files, self.transfer_job(install_path), hooks, self.before_write()).await
    }

    async fn seal(
        self: &Arc<Self>,
        publisher: &dyn Publisher,
        mut manifest: Manifest,
        files: &[Entry],
        dir: &Path,
        sealed: Sealed,
    ) -> Result<Value, String> {
        log::info!(
            "Game download completed successfully ({}).",
            self.profile_id()
        );
        let parts: Vec<Entry> = manifest.containers.iter().flat_map(|c| c.parts.iter().cloned()).collect();
        let (packages, sealed) = match manifest.containers.iter().find(|c| c.unpack.is_some()) {
            Some(container) => {
                let _extracting = self.begin_extracting();
                self.note_install_write();
                let (unpacked, sealed) = extract::unpack_package(&self.core, dir, container, None).await?;
                manifest.listed.extend(unpacked);
                (extract::downloaded_package_paths(dir, &parts), sealed)
            }
            None if parts.is_empty() => (Vec::new(), sealed),
            None => extract::split_archives(self, dir, &parts, sealed).await?,
        };
        let unpacked = !packages.is_empty();
        if self.is_cancelled() {
            if !unpacked {
                return Err("Download aborted by user.".to_string());
            }
            log::info!(
                "Cancel arrived after the packages were unpacked, so the install is finished instead."
            );
        }

        self.report_finalizing(30.0);
        let path = dir.to_path_buf();
        let dests: Vec<String> = files.iter().filter(|f| !f.in_place()).map(|f| f.path.to_string()).collect();
        if let Err(e) =
            tauri::async_runtime::spawn_blocking(move || remove_part_files(&path, &dests)).await
        {
            log::warn!("Leftover .part cleanup did not finish: {e}");
        }
        if !unpacked && self.is_cancelled() {
            return Err("Download aborted by user.".to_string());
        }

        self.report_finalizing(50.0);
        let installed = manifest.installed();
        if !installed.is_empty() {
            validator::prune_dropped(dir, installed).await;
        } else if !manifest.containers.is_empty() {
            validator::forget_record(dir);
        }
        manifest::make_dirs(dir, &manifest.dirs);
        self.report_finalizing(65.0);
        let packages_done = publisher.after_seal(&self.core, dir, &manifest, &sealed).await?;
        self.report_finalizing(85.0);
        log::info!(
            "Game config for {} updated to version {}",
            self.profile_id(),
            manifest.version
        );
        if packages_done {
            remove_downloaded_packages(&packages);
        } else if unpacked {
            log::warn!(
                "Kept the {} downloaded package(s) so the next update can unpack them again.",
                packages.len()
            );
        }
        if !manifest.installed().is_empty() {
            validator::record_install(dir, &manifest).await;
        }
        if matches!(
            InstallMode::of(self.profile()),
            InstallMode::Default | InstallMode::Unknown | InstallMode::Yostar | InstallMode::Gf2 | InstallMode::Bluepoch
        ) {
            validator::write_local_index(dir, files);
        }
        self.report_finalizing(95.0);
        self.finish_completion(dir).await;
        Ok(ok_with(json!({
            "installPath": dir.to_string_lossy(),
            "version": manifest.version,
        })))
    }

    async fn finish_completion(&self, install_path: &Path) {
        clear_install_marker(install_path);
        self.clear_unfinished_update();
        kuro::forget_quality_switch(&self.app, &self.profile_id());
        self.clear_update_cache();
        let hook = self.on_complete.lock().take();
        if let Some(hook) = hook {
            let path = install_path.to_path_buf();
            if let Err(e) = tauri::async_runtime::spawn_blocking(move || hook(path)).await {
                log::warn!("The install completion step for {} failed: {e}", self.profile_id());
            }
        }
        self.send_progress(Phase::Done, status::COMPLETED, json!({ "percentage": 100 }));
    }

    // ------------ Install Helpers ------------
    // The mirrors of this download, the transfer core's view of it, and the record that an update is writing into the game.

    pub(super) fn install_mirrors(&self, publisher: Vec<String>) {
        self.mirrors.reset(publisher, None);
    }

    pub(super) fn transfer_job(&self, install_dir: &Path) -> Arc<dyn peebify_transfer::JobHooks> {
        TransferJob::new(&self.core, install_dir, Some(Arc::clone(&self.mirrors)))
            .noting_verified(Arc::clone(&self.verified))
            .shared()
    }

    pub(super) fn note_install_write(&self) {
        self.install_writes.before_write(|| {
            log::info!(
                "Update of {} is writing into the installed game, so it can't be played until the update or a repair finishes.",
                self.profile_id()
            );
            super::file_channels::mark_update_incomplete(&self.app, &self.profile_id());
        });
    }

    pub(super) fn before_write(self: &Arc<Self>) -> BeforeWrite {
        let me = Arc::clone(self);
        Some(Arc::new(move || me.note_install_write()))
    }

    pub(super) fn begin_extracting(&self) -> ExtractingGuard<'_> {
        self.extracting.store(true, Ordering::SeqCst);
        self.lift_pause_for_unpacking();
        ExtractingGuard(&self.extracting)
    }
}

// ------------ Install Preview ------------
// Answers the "what will this cost me?" question before a download starts: download size, install size, the parts
// that make it up and the free disk space needed. Each publisher sizes its own game; results are cached a few minutes.
const PREVIEW_CACHE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const OFFLINE: &str = "No internet connection.";

static PREVIEW_CACHE: http::TtlCache<Value> = http::TtlCache::new(PREVIEW_CACHE_TIMEOUT, 16);

pub(super) struct Preview {
    pub(super) version: String,
    pub(super) download_bytes: u64,
    pub(super) install_bytes: u64,
    pub(super) file_count: u64,
    pub(super) approximate: bool,
    pub(super) parts: Vec<Value>,
}

pub(super) fn package_preview(version: String, download_bytes: u64, install_bytes: u64, files: u64) -> Preview {
    Preview {
        version,
        download_bytes,
        install_bytes,
        file_count: files,
        approximate: true,
        parts: merge_parts(&[("game".to_string(), "game client".to_string(), download_bytes, files)]),
    }
}

pub(super) fn resource_list_preview(manifest: Manifest) -> Preview {
    let bytes = manifest.entries.iter().map(|e| e.size).sum();
    Preview {
        version: manifest.version,
        download_bytes: bytes,
        install_bytes: bytes,
        file_count: manifest.entries.len() as u64,
        approximate: false,
        parts: Vec::new(),
    }
}

pub(super) fn merge_parts(entries: &[(String, String, u64, u64)]) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut totals: std::collections::HashMap<String, (String, u64, u64)> = std::collections::HashMap::new();
    for (key, label, bytes, files) in entries {
        let slot = totals.entry(label.clone()).or_insert_with(|| {
            order.push(label.clone());
            (key.clone(), 0, 0)
        });
        slot.1 += bytes;
        slot.2 += files;
    }
    order
        .iter()
        .filter_map(|label| {
            let (key, bytes, files) = totals.get(label)?;
            Some(json!({ "key": key, "label": label, "bytes": bytes, "files": files }))
        })
        .collect()
}

pub(super) async fn offline_or(error: String) -> String {
    if !http::is_online_cached() || !http::is_online().await {
        OFFLINE.to_string()
    } else {
        error
    }
}

fn opt_bytes(total: u64) -> Value {
    if total > 0 {
        json!(total)
    } else {
        Value::Null
    }
}

pub(super) fn invalidate_preview(profile_id: &str) {
    PREVIEW_CACHE.remove(profile_id);
    let profile = game_profiles::profile(profile_id);
    if let Some(qualities) = profile["resourceQualityArgs"].as_object() {
        for quality in qualities.keys() {
            PREVIEW_CACHE.remove(&format!("{profile_id}:{quality}"));
        }
    }
}

pub(super) async fn get_install_preview(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let requested = args
        .first()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let game_id = match requested {
        Some(id) => id.to_string(),
        None => app.state::<BackendState>().config.active_game_id(),
    };
    let profile = game_profiles::profile(&game_id);
    let profile_id = game_profiles::profile_id(profile).to_string();

    if !game_profiles::is_managed(profile) {
        return Ok(ok_with(json!({
            "gameId": profile_id,
            "version": null,
            "downloadBytes": null,
            "installBytes": null,
            "fileCount": null,
            "approximate": false,
            "parts": [],
            "notSupported": true,
        })));
    }

    let quality = kuro::selected_quality(app, profile);
    let cache_key = match &quality {
        Some(quality) => format!("{profile_id}:{quality}"),
        None => profile_id.clone(),
    };
    if let Some(hit) = PREVIEW_CACHE.get(&cache_key) {
        return Ok(ok_with(hit));
    }

    let publisher = manifest::publisher(profile);
    let preview = match publisher.preview(profile, quality.as_deref()).await {
        Ok(preview) => preview,
        Err(e) => {
            log::warn!("Install preview failed ({profile_id}): {e}");
            return Ok(err_response(offline_or(e).await));
        }
    };

    let (write_bytes, multiplier) = publisher.write_space(preview.download_bytes, preview.install_bytes);
    let required = required_free_bytes(write_bytes, multiplier, HEADROOM_INSTALL);
    let result = json!({
        "gameId": profile_id,
        "version": (!preview.version.is_empty()).then_some(preview.version),
        "downloadBytes": opt_bytes(preview.download_bytes),
        "installBytes": opt_bytes(preview.install_bytes),
        "requiredBytes": opt_bytes(required),
        "fileCount": opt_bytes(preview.file_count),
        "approximate": preview.approximate,
        "parts": preview.parts,
        "notSupported": false,
    });
    PREVIEW_CACHE.set(&cache_key, result.clone());
    Ok(ok_with(result))
}

// ------------ Repair Manager ------------
// The object that owns one game's repair: its pause, resume and cancel, and the entry point that starts a repair.
// A repair checks an installed game against its publisher's manifest and fetches again whatever is missing or corrupt,
// in a quick mode (the install record, sizes only) or a full mode (a fresh manifest, checksums). A game that repairs to
// its latest build always takes the fresh manifest, its quick repair skipping what the record has at that build. A game
// that comes as one package fixes any damage by fetching the package again and unpacking it over the game. A publisher
// whose game repairs itself some other way (Reverse: 1999 clears its update staging) supplies that repair instead.
pub struct GameRepairManager {
    core: Arc<JobCore>,
}

impl std::ops::Deref for GameRepairManager {
    type Target = JobCore;

    fn deref(&self) -> &JobCore {
        &self.core
    }
}

impl GameRepairManager {
    pub fn new(app: AppHandle, profile: &'static Value) -> Arc<Self> {
        Arc::new(Self {
            core: JobCore::new(app, JobKind::Repair, profile),
        })
    }

    pub fn keep_repairing_while_playing(&self) {
        self.keep_while_playing();
    }

    fn stage_phase(&self) -> Phase {
        match self.tracker.phase().as_str() {
            "repairing" => Phase::Repairing,
            "extracting" => Phase::Extracting,
            "validating" => SCAN_PHASE,
            _ => Phase::Downloading,
        }
    }

    pub fn pause_repair(&self) {
        if self.tracker.phase() == "extracting" {
            log::info!("Pause ignored because unpacking cannot be paused.");
            return;
        }
        if self.is_running() && self.gate.pause() {
            if self.tracker.phase() == "extracting" {
                self.gate.resume();
                log::info!("Pause ignored because unpacking cannot be paused.");
                return;
            }
            self.set_paused_power(true);
            self.publish(self.stage_phase(), json!({ "status": status::PAUSED }));
            log::info!("Repair paused ({}).", self.profile_id());
        }
    }

    pub fn resume_repair(&self) {
        if self.is_running() && self.gate.resume() {
            self.set_paused_power(false);
            self.tracker.reset_speed_baseline();
            let phase = self.stage_phase();
            let status = match phase {
                Phase::Repairing => status::REPAIRING,
                Phase::Extracting => status::UNPACKING,
                _ => status::VALIDATING,
            };
            self.publish(Update::new(phase).resumed(), json!({ "status": status }));
            log::info!("Repair resumed ({}).", self.profile_id());
        }
    }

    pub fn cancel_repair(&self) {
        self.cancel();
    }

    pub async fn repair_game(self: &Arc<Self>, game_path: &Path, mode: &str) {
        let Some(_power) = self.begin() else {
            self.publish(
                Phase::Error,
                json!({ "status": status::ERROR, "error": "Repair process is already running." }),
            );
            return;
        };
        let start = chrono::Utc::now().timestamp_millis();
        let result = if self.is_cancelled() {
            Err("Repair aborted".to_string())
        } else {
            self.run_repair(game_path, mode == "quick", start).await
        };
        if let Err(e) = result {
            self.fail(&e);
        }
        self.end();
    }

    async fn run_repair(self: &Arc<Self>, game_path: &Path, quick: bool, start_ms: i64) -> Result<(), String> {
        if let Some(message) = super::game_file_ops::running_refusal(&self.app, self.profile()).await {
            return Err(message);
        }
        let publisher = manifest::publisher(self.profile());
        if let Some(repair) = publisher.repair(&self.core, game_path, start_ms) {
            return repair.await;
        }
        self.repair_file_list(publisher, game_path, quick, start_ms).await
    }

    // ------------ Manifest Repair ------------
    // Checks every file the install record lists by size (a quick repair) or every file of the publisher's manifest by
    // checksum (a full repair), and fetches the bad ones again. A game that repairs to its latest build checks the
    // manifest either way, its quick repair skipping what the record has at that build, and reuses a verify's scan from
    // moments ago.
    async fn repair_file_list(self: &Arc<Self>, publisher: Arc<dyn Publisher>, game_path: &Path, quick: bool, start_ms: i64) -> Result<(), String> {
        let policy = publisher.policy(game_path);
        let latest = policy.latest;
        let by_size = quick && !policy.checksums;
        let mut manifest: Option<Manifest> = None;
        let local = if quick && !latest { Some(self.local_entries(game_path).await) } else { None };
        let files = match local {
            Some(Ok(local)) => {
                log::info!("Quick repair using local index: {} files", local.len());
                local
            }
            local => {
                if let Some(Err(local_error)) = local {
                    log::warn!("Local index failed ({local_error}), falling back to remote index");
                }
                let fetched = self.fetch_manifest(publisher.as_ref(), game_path, false).await?;
                if !latest {
                    self.refuse_stale_repair(game_path, &fetched.version)?;
                }
                log::info!(
                    "{} repair using remote index: {} files",
                    if quick { "Quick" } else { "Full" },
                    fetched.entries.len()
                );
                manifest.insert(fetched).entries.clone()
            }
        };
        if files.is_empty() && !manifest.as_ref().is_some_and(|m| m.containers.iter().any(|c| c.unpack.is_some())) {
            return Err("Invalid game index structure.".to_string());
        }
        let from_remote = manifest.is_some();
        let vouched = from_remote && !by_size;
        if let Some(fetched) = &manifest {
            validator::prune_dropped(game_path, fetched.installed()).await;
            if matches!(InstallMode::of(self.profile()), InstallMode::Default | InstallMode::Yostar) {
                validator::write_local_index(game_path, &fetched.entries);
            }
        }

        let corrupt = match manifest.as_ref().and_then(|fetched| self.take_verify_cache(fetched)) {
            Some(cached) => {
                log::info!("Repair is reusing the integrity scan from the verify run moments ago.");
                cached
            }
            None => {
                let trusted = match (&manifest, quick && !by_size) {
                    (Some(fetched), true) => self.recorded_at(game_path, fetched).await,
                    _ => HashSet::new(),
                };
                let check = files.iter().filter(|f| !trusted.contains(&manifest_key(&f.path))).cloned().collect();
                self.validate_files(check, game_path, by_size).await?
            }
        };
        if self.is_cancelled() {
            return Err("cancelled".to_string());
        }

        let mut sealed = Sealed::nothing_written();
        let (mut packaged, mut wanted_files) = (false, 0);
        if !corrupt.is_empty() || files.is_empty() {
            let mut fetched = match manifest.take() {
                Some(fetched) => fetched,
                None => {
                    let fetched = self.fetch_manifest(publisher.as_ref(), game_path, true).await?;
                    self.refuse_stale_repair(game_path, &fetched.version)?;
                    fetched
                }
            };
            if fetched.containers.iter().any(|c| c.unpack.is_some()) {
                let wanted = (!files.is_empty()).then(|| corrupt.iter().map(|f| f.path.to_string()).collect::<HashSet<_>>());
                wanted_files = wanted.as_ref().map_or(0, HashSet::len);
                sealed = self.repair_package(publisher.as_ref(), &mut fetched, wanted, game_path).await?;
                packaged = true;
            } else {
                let fetch = if from_remote { corrupt } else { listed_in(&fetched, &corrupt)? };
                sealed = self.repair_corrupt_files(fetch, fetched.mirrors.clone(), game_path).await?;
            }
            if self.is_cancelled() {
                return Err("cancelled".to_string());
            }
            manifest = Some(fetched);
        }
        if let Some(fetched) = &manifest {
            if vouched {
                manifest::make_dirs(game_path, &fetched.dirs);
                validator::record_install(game_path, fetched).await;
            }
            if packaged || vouched && policy.records {
                if let Err(e) = publisher.after_seal(&self.core, game_path, fetched, &sealed).await {
                    log::warn!("Could not write the files of version {} after the repair: {e}", fetched.version);
                }
                self.clear_update_cache();
            }
        }

        let repaired = self.tracker.repaired_files();
        if !by_size && repaired >= wanted_files {
            self.clear_unfinished_update();
        }
        self.complete(start_ms, repaired, files.len().max(repaired));
        Ok(())
    }

    // ------------ Package Repair ------------
    // A game that comes as one package has no file to fetch for a damaged file: its package is fetched again (a whole
    // one staged is reused) and unpacked over the game.

    async fn repair_package(
        self: &Arc<Self>,
        publisher: &dyn Publisher,
        fetched: &mut Manifest,
        wanted: Option<HashSet<String>>,
        game_path: &Path,
    ) -> Result<Sealed, String> {
        let container = fetched.containers.iter().find(|c| c.unpack.is_some()).ok_or("There is no package to repair from.")?;
        extract::remove_other_packages(game_path, container);
        let trusted = publisher.triage(game_path, &container.parts).trusted;
        let to_check = container.parts.iter().filter(|p| !trusted.contains(&manifest_key(&p.path))).cloned().collect();
        let fetch: Vec<Entry> = validator::check_files(&self.core, game_path, to_check, false, status::VALIDATING, false)
            .await?
            .into_iter()
            .map(|(part, _)| part)
            .collect();
        if !fetch.is_empty() {
            self.tracker.reset();
            self.tracker.set_totals(fetch.iter().map(|p| p.size).sum::<u64>() as f64, 1);
            self.repair_corrupt_files(fetch, fetched.mirrors.clone(), game_path).await?;
        }
        self.gate.wait_while_paused().await;
        if self.is_cancelled() {
            return Err("Repair aborted".to_string());
        }
        let (files, sealed) = extract::unpack_package(&self.core, game_path, container, wanted.as_ref()).await?;
        remove_downloaded_packages(&extract::downloaded_package_paths(game_path, &container.parts));

        let repaired = match &wanted {
            Some(wanted) if !files.is_empty() => files.iter().filter(|f| wanted.contains(&*f.path)).count(),
            Some(wanted) => wanted.len(),
            None => files.len(),
        };
        if let Some(wanted) = wanted.as_ref().filter(|w| repaired < w.len()) {
            log::warn!("{} damaged file(s) are not in the {} package.", wanted.len() - repaired, fetched.version);
        }
        match &wanted {
            _ if !files.is_empty() => fetched.entries = files,
            Some(wanted) => {
                for entry in fetched.entries.iter_mut().filter(|e| wanted.contains(&*e.path)) {
                    let on_disk = super::fs_util::safe_join(game_path, &entry.path).ok().and_then(|p| std::fs::metadata(p).ok());
                    entry.size = on_disk.map_or(entry.size, |meta| meta.len());
                }
            }
            None => {}
        }
        (0..repaired).for_each(|_| self.tracker.increment_repaired_files());
        Ok(sealed)
    }

    fn refuse_stale_repair(&self, game_path: &Path, remote_version: &str) -> Result<(), String> {
        let local = super::game_manager::local_game_version_for(self.profile(), &game_path.to_string_lossy());
        match local {
            Some(local) if super::fs_util::is_version_newer(remote_version, &local) => {
                log::info!(
                    "Repair stopped: version {remote_version} is available while the install is on {local}."
                );
                Err("An update is available. Update the game first, then repair.".to_string())
            }
            _ => Ok(()),
        }
    }

    async fn recorded_at(&self, game_path: &Path, fetched: &Manifest) -> HashSet<String> {
        let (dir, version, files) = (game_path.to_path_buf(), fetched.version.trim().to_string(), fetched.entries.clone());
        tauri::async_runtime::spawn_blocking(move || {
            let record = validator::Record::load(&dir);
            if record.version == version {
                return record.vouches(&dir, &files);
            }
            log::info!("Quick repair: no recorded install for {version}, so every file is checked in full.");
            HashSet::new()
        })
        .await
        .unwrap_or_default()
    }

    fn take_verify_cache(&self, fetched: &Manifest) -> Option<Vec<Entry>> {
        let pools = self.app.state::<BackendState>().engine.clone();
        let mut slot = pools.verify_cache.lock();
        match slot.take() {
            Some(cache)
                if cache.game_id == self.profile_id()
                    && cache.version == fetched.version
                    && cache.groups == fetched.groups
                    && cache.at.elapsed() < Duration::from_secs(300) =>
            {
                Some(cache.broken)
            }
            Some(cache) if cache.game_id != self.profile_id() => {
                *slot = Some(cache);
                None
            }
            _ => None,
        }
    }

    async fn local_entries(&self, game_path: &Path) -> Result<Vec<Entry>, String> {
        self.tracker.set_phase("fetching");
        self.publish(
            Phase::Downloading,
            json!({ "status": status::FETCHING_INDEX, "message": "Reading local file index..." }),
        );
        let dir = game_path.to_path_buf();
        tauri::async_runtime::spawn_blocking(move || {
            let recorded = validator::Record::load(&dir).entries();
            let origin = || validator::read_index_file(&dir.join("OriginResource.json"));
            for (name, files) in [("the install record", recorded), ("OriginResource.json", origin())] {
                if files.len() >= 100 {
                    log::info!("Found {} files in {name}", files.len());
                    return Ok(files);
                }
                if !files.is_empty() {
                    log::warn!("Only {} files found in {name}, checking next option...", files.len());
                }
            }
            Err("Quick Check failed: No valid local index found with sufficient files. Please run a Full Check."
                .to_string())
        })
        .await
        .map_err(|e| format!("local index task panicked: {e}"))?
    }

    async fn fetch_manifest(&self, publisher: &dyn Publisher, game_path: &Path, after_scan: bool) -> Result<Manifest, String> {
        let phase = if after_scan {
            SCAN_PHASE
        } else {
            self.tracker.set_phase("fetching");
            Phase::Downloading
        };
        self.publish(phase, json!({ "status": status::FETCHING_REPAIR_CONFIG }));
        let job = self.transfer_job(game_path, None);
        let cx = Cx { app: &self.app, profile: self.profile(), dir: game_path, purpose: JobKind::Repair, job };
        self.cancellable(publisher.manifest(&cx)).await
    }

    async fn validate_files(self: &Arc<Self>, files: Vec<Entry>, game_path: &Path, quick: bool) -> Result<Vec<Entry>, String> {
        log::info!(
            "Repair {} ({}, {}): validating {} files",
            self.profile_id(),
            if quick { "quick" } else { "full" },
            game_profiles::install_mode(self.profile()).unwrap_or("default"),
            files.len()
        );
        self.publish(
            SCAN_PHASE,
            json!({ "status": status::VALIDATING, "message": format!("Validating {} files...", files.len()) }),
        );
        let flagged = validator::check_files(&self.core, game_path, files, quick, status::VALIDATING, false).await?;
        Ok(flagged.into_iter().map(|(file, _)| file).collect())
    }

    async fn repair_corrupt_files(
        self: &Arc<Self>,
        corrupt: Vec<Entry>,
        publisher: Vec<String>,
        game_path: &Path,
    ) -> Result<Sealed, String> {
        let total_files = corrupt.len();
        let total_bytes: u64 = corrupt.iter().map(Entry::fetch_size).sum();
        let chunked = corrupt.iter().any(Entry::chunked);
        let space_needed = {
            let (dir, files) = (game_path.to_path_buf(), corrupt.clone());
            tauri::async_runtime::spawn_blocking(move || write_bytes(&dir, &files, 0))
                .await
                .unwrap_or(total_bytes)
        };
        ensure_disk_space(game_path, space_needed, 1.0, HEADROOM_REPAIR)?;
        if let Err(e) = super::fs_util::probe_writable(game_path) {
            if super::fs_util::is_access_denied(&e) {
                log::warn!("{} is not writable: {e}", game_path.display());
                return Err(super::fs_util::not_writable_message(game_path));
            }
            log::warn!("Could not probe {} for write access: {e}", game_path.display());
        }

        self.tracker.set_totals(total_bytes as f64, self.tracker.total_files());
        self.tracker
            .set_file_sizes(corrupt.iter().map(|f| (f.path.to_string(), f.fetch_size() as f64)).collect());
        self.tracker.set_phase("repairing");

        log::info!(
            "Starting repair of {total_files} files ({:.2}GB)",
            peebify_transfer::progress::gib(total_bytes as f64)
        );
        self.send_progress(Phase::Repairing, "Repairing missing/corrupted files", json!({}));

        let bases = publisher.iter().map(|base| base.trim_end_matches('/').to_string()).filter(|base| !base.is_empty());
        let route = Some(Arc::new(MirrorSet::new(bases.collect(), Some(0))));
        let started = Instant::now();
        let _publisher = self.core.publish_while_transferring();
        let job = self.transfer_job(game_path, route);
        let fetched = manifest::fetch_entries(game_path, &corrupt, job, Arc::new(FileProgress::transfer(&self.core)), None).await;
        let sealed = fetched.map_err(|e| repair_failure(&e))?;
        if !chunked {
            for _ in 0..total_files {
                self.tracker.increment_repaired_files();
            }
        }

        log::info!(
            "Repair complete for {}: {} of {total_files} files repaired, {:.2}GB fetched in {:.1}s",
            self.profile_id(),
            self.tracker.repaired_files(),
            peebify_transfer::progress::gib(total_bytes as f64),
            started.elapsed().as_secs_f64()
        );
        Ok(sealed)
    }

    pub(super) fn transfer_job(
        &self,
        install_dir: &Path,
        route: Option<Arc<MirrorSet>>,
    ) -> Arc<dyn peebify_transfer::JobHooks> {
        TransferJob::new(&self.core, install_dir, route).shared()
    }

    pub(super) fn complete(&self, start_ms: i64, repaired: usize, total_validated: usize) {
        let message = completion_message(start_ms, repaired, total_validated);
        log::info!("Repair completed for {}: {message}", self.profile_id());

        if repaired > 0 {
            log::info!(
                "Clearing GameManager update cache for {} after repair completion",
                self.profile_id()
            );
            self.clear_update_cache();
        }

        self.publish(
            Phase::Done,
            json!({ "status": status::COMPLETED, "message": message }),
        );
    }
}

pub(super) fn completion_message(start_ms: i64, repaired: usize, validated: usize) -> String {
    let duration = (chrono::Utc::now().timestamp_millis() - start_ms) as f64 / 1000.0;
    if repaired > 0 {
        format!(
            "Successfully repaired {repaired} files out of {validated} validated in {duration:.1}s"
        )
    } else {
        format!("All {validated} files are valid!")
    }
}

fn repair_failure(error: &str) -> String {
    let file_of = |text: &str| -> String {
        text.split('?')
            .next()
            .unwrap_or(text)
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(text)
            .to_string()
    };
    if let Some(code) = http::permanent_client_status(error) {
        let file = error.split_whitespace().last().map(file_of).unwrap_or_default();
        return format!(
            "The download server has no usable copy of {file} (HTTP {code}). The file index is likely outdated (was the game updated?). Update the game first, or run a Full Repair."
        );
    }
    if let Some(name) = error
        .strip_prefix("The server sent a copy of ")
        .and_then(|rest| rest.strip_suffix(" that does not match its checksum. Try again later."))
    {
        return format!(
            "{} doesn't match the file index even after re-downloading. The index is likely outdated (was the game updated?). Run a Full Repair, or update the game first.",
            file_of(name)
        );
    }
    error.to_string()
}

fn listed_in(manifest: &Manifest, files: &[Entry]) -> Result<Vec<Entry>, String> {
    let listed: HashMap<String, &Entry> =
        manifest.entries.iter().map(|e| (manifest_key(&e.path), e)).collect();
    let found: Vec<Entry> = files
        .iter()
        .filter_map(|f| listed.get(&manifest_key(&f.path)).map(|e| (*e).clone()))
        .collect();
    match files.len() - found.len() {
        0 => Ok(found),
        gone => Err(format!(
            "{gone} file(s) the local index lists are no longer part of the game's file list. Run a Full Repair."
        )),
    }
}

// ------------ Verify ------------
// Checks an install against what the game's server says it should be, without changing anything: its files against the
// manifest, or for a game that publishes no file checksums, the shape of the install.
pub(super) enum VerifyError {
    Cancelled,
    Other(String),
}

pub(super) fn verify_failure(cancel: &AtomicBool, error: String) -> VerifyError {
    if cancel.load(Ordering::SeqCst) {
        VerifyError::Cancelled
    } else {
        VerifyError::Other(error)
    }
}

pub(super) fn verify_update(phase: Phase) -> Update {
    Update::new(phase).kind("verify")
}

pub(super) fn verify_result_update(broken: usize) -> Update {
    verify_update(if broken == 0 { Phase::Done } else { Phase::Error })
}

pub(super) fn broken_files_error(count: usize) -> String {
    if count == 1 {
        "1 file needs repair. Run a repair to replace it.".to_string()
    } else {
        format!("{count} files need repair. Run a repair to replace them.")
    }
}

pub(super) fn verify_result_payload(broken: usize) -> Value {
    let mut payload = json!({
        "status": if broken == 0 { "Verification Complete" } else { "Verification Failed" },
        "percentage": 100,
    });
    if broken > 0 {
        payload["error"] = json!(broken_files_error(broken));
    }
    payload
}

pub(super) fn publish_verify_result(job: &JobCore, broken: usize) {
    job.tracker.force_completion();
    let mut payload = job.tracker.calculate_metrics();
    if let (Some(map), Value::Object(result)) = (payload.as_object_mut(), verify_result_payload(broken)) {
        map.extend(result);
    }
    job.publish(verify_result_update(broken), payload);
}

pub(super) fn pending_update_note(broken: usize, installed: Option<&str>, latest: &str) -> Option<String> {
    let latest = latest.trim();
    let installed = installed.map(str::trim).filter(|v| !v.is_empty())?;
    if broken == 0 || latest.is_empty() || installed == latest {
        return None;
    }
    Some(if broken == 1 {
        format!("1 file differs from version {latest}. An update is available, and it replaces that file.")
    } else {
        format!("{broken} files differ from version {latest}. An update is available, and it replaces these files.")
    })
}

fn older_install_note(broken: usize, installed: Option<&str>, latest: &str) -> Option<String> {
    installed
        .filter(|v| super::fs_util::is_version_newer(latest, v))
        .and_then(|_| pending_update_note(broken, installed, latest))
}

pub(super) fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

pub(super) async fn verify(job: &Arc<JobCore>, game_path: &str) -> Result<Value, VerifyError> {
    let profile = job.profile();
    if !game_profiles::is_managed(profile) {
        return Err(VerifyError::Other(format!(
            "{} integrity verification is not supported yet.",
            game_profiles::display_name(profile)
        )));
    }
    let cancel = job.gate.flag();
    let publisher = manifest::publisher(profile);
    let dir = Path::new(game_path);
    let cx = Cx { app: &job.app, profile, dir, purpose: JobKind::Verify, job: TransferJob::new(job, dir, None).shared() };
    if let Some(shape) = publisher.shape_check(&cx) {
        return verify_shape(job, game_path, shape).await;
    }
    let manifest = publisher.manifest(&cx).await.map_err(|e| verify_failure(cancel, e))?;

    let invalid = validator::check_files(job, dir, manifest.entries.clone(), false, "Verifying integrity...", false)
        .await
        .map_err(|e| verify_failure(cancel, e))?;
    if !invalid.is_empty() {
        validator::hold_record(dir, &manifest.version).await;
    } else if manifest.adopts {
        validator::record_install(dir, &manifest).await;
        match publisher.after_seal(job, dir, &manifest, &Sealed::nothing_written()).await {
            Ok(_) => log::info!("{}: the install matches version {} and is now tracked.", job.profile_id(), manifest.version),
            Err(e) => log::warn!("{}: could not record the install's version: {e}", job.profile_id()),
        }
        job.clear_update_cache();
    }
    if publisher.policy(dir).latest {
        return Ok(verify_latest(job, game_path, manifest, invalid.into_iter().map(|(file, _)| file).collect()).await);
    }
    publish_verify_result(job, invalid.len());

    let local_version = super::game_manager::local_game_version_for(profile, game_path);
    let differing = invalid.len().max(usize::from(manifest.entries.is_empty()));
    let update_pending = older_install_note(differing, local_version.as_deref(), &manifest.version).is_some();
    let invalid_json: Vec<Value> = invalid.iter().map(|(file, _)| file.to_json()).collect();
    Ok(super::ok_with(json!({
        "invalidFiles": invalid_json,
        "updatePending": update_pending,
    })))
}

// ------------ Verify Shape ------------
// A game that publishes no file checksums is checked by the shape of its install: the launch executable, the data
// folder, whatever only that game can say (its `Shape`), and the folder's size against the install size it publishes.
const MIN_SIZE_RATIO: f64 = 0.90;

async fn verify_shape(job: &Arc<JobCore>, game_path: &str, shape: BoxFuture<'_, Shape>) -> Result<Value, VerifyError> {
    let profile = job.profile();
    let name = game_profiles::display_name(profile);
    let root = PathBuf::from(game_path);
    let tick = |percentage: u32, status: &str| {
        job.publish(verify_update(Phase::Verifying), json!({ "status": status, "percentage": percentage }));
    };
    tick(5, "Verifying installed files...");

    if !root.is_dir() {
        return Err(VerifyError::Other(format!(
            "{name} is no longer in {game_path}. Point Peebify at it again with Locate existing install, or reinstall it."
        )));
    }
    let launch_exe = game_profiles::launch_executable_path(&root, profile);
    if !launch_exe.exists() {
        let base = launch_exe.file_name().map_or_else(|| game_profiles::executable_name(profile).to_string(), |n| n.to_string_lossy().to_string());
        return Err(VerifyError::Other(format!("{base} is missing from {game_path}, so this install cannot start. Reinstall {name}.")));
    }
    if !game_profiles::data_dir(&root, game_profiles::executable_name(profile)).is_dir() {
        return Err(VerifyError::Other(format!(
            "{name}'s game data folder is missing from {game_path}, so the install is not usable. Reinstall it."
        )));
    }
    let mut problems: Vec<String> = game_profiles::path_budget_error(&root, profile).into_iter().collect();

    tick(35, "Verifying installed files: measuring the install...");
    let (walk_root, walk_cancel) = (root.clone(), Arc::clone(job.gate.flag()));
    let on_disk = tauri::async_runtime::spawn_blocking(move || super::fs_util::folder_bytes(&walk_root, &walk_cancel).ok_or(VerifyError::Cancelled))
        .await
        .map_err(|e| VerifyError::Other(format!("verify task panicked: {e}")))??;
    if job.is_cancelled() {
        return Err(VerifyError::Cancelled);
    }

    tick(80, "Verifying installed files: comparing with the published size...");
    let shape = shape.await;
    problems.extend(shape.problems);
    match shape.published {
        Some((version, bytes)) if bytes > 0 => {
            let ratio = on_disk as f64 / bytes as f64;
            log::info!(
                "{name} verify: {} on disk against a published install size of {} ({:.1}% of it).",
                fmt_gb(on_disk),
                fmt_gb(bytes),
                ratio * 100.0,
            );
            if ratio < MIN_SIZE_RATIO {
                problems.push(format!(
                    "The folder holds {}, but {name} {version} installs to about {}, so files are missing. Reinstall to replace them.",
                    fmt_gb(on_disk),
                    fmt_gb(bytes),
                ));
            }
        }
        Some(_) => log::info!("{name} verify: no install size is published for this package; skipped the size check."),
        None => {}
    }
    if !problems.is_empty() {
        log::warn!("{name} verify found {} problem(s): {}", problems.len(), problems.join(" | "));
        return Err(VerifyError::Other(problems.join(" ")));
    }

    let message = format!("{name} looks intact with {} installed{}", fmt_gb(on_disk), shape.note);
    log::info!("{name} verify: {message}");
    job.publish(verify_update(Phase::Done), json!({ "status": "Verification Complete", "percentage": 100, "message": message }));
    Ok(super::ok_with(json!({ "invalidFiles": [], "message": message })))
}

pub struct VerifyCache {
    game_id: String,
    version: String,
    groups: Vec<String>,
    at: Instant,
    broken: Vec<Entry>,
}

async fn verify_latest(job: &Arc<JobCore>, game_path: &str, manifest: Manifest, flagged: Vec<Entry>) -> Value {
    let (dir, listed) = (PathBuf::from(game_path), flagged.clone());
    let (installed, unchanged) = tauri::async_runtime::spawn_blocking(move || {
        let record = validator::Record::load(&dir);
        let unchanged = record.unchanged(&listed);
        (record.version, unchanged)
    })
    .await
    .unwrap_or_default();
    let installed = Some(installed).filter(|v| !v.is_empty());
    let behind = installed.as_deref().is_some_and(|v| v != manifest.version.trim());
    let (pending, broken): (Vec<&Entry>, Vec<&Entry>) =
        flagged.iter().partition(|f| behind && !unchanged.contains(&manifest_key(&f.path)));
    let (count, pending, tag) = (broken.len(), pending.len(), &manifest.version);
    let local_version =
        installed.clone().or_else(|| super::game_manager::local_game_version_for(job.profile(), game_path));
    let mut payload = verify_result_payload(count);
    let invalid: Vec<Value> = broken.iter().map(|f| f.to_json()).collect();
    let mut body = json!({ "invalidFiles": invalid, "updatePending": false });
    if installed.is_some() {
        body["updatePending"] = json!(count == 0 && pending > 0);
        if let Some(note) = pending_update_note(pending, local_version.as_deref(), tag) {
            let text = if count == 0 { note } else { format!("{} {note}", broken_files_error(count)) };
            payload[if count == 0 { "message" } else { "error" }] = json!(text);
            body["message"] = json!(text);
        }
    } else if let Some(note) = pending_update_note(count, local_version.as_deref(), tag) {
        payload["error"] = json!(note);
        body["message"] = json!(note);
        body["updatePending"] = json!(true);
    }
    job.publish(verify_result_update(count), payload);
    log::info!(
        "Verify for {}: {count} files need repair, {pending} wait for the update (installed {}, latest {tag}).",
        job.profile_id(),
        local_version.as_deref().unwrap_or("unknown")
    );
    *job.app.state::<BackendState>().engine.verify_cache.lock() = Some(VerifyCache {
        game_id: job.profile_id(),
        version: manifest.version,
        groups: manifest.groups,
        at: Instant::now(),
        broken: flagged,
    });
    super::ok_with(body)
}

// ------------ Tests ------------
// Error wording, disk space, mirror failover, the install and repair rules, and the verify result.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{bd2, bluepoch, hypergryph, sophon};
    use std::time::Duration;

    #[test]
    fn raw_transfer_errors_reach_the_user_as_plain_advice() {
        let http =
            user_facing_error("HTTP Error: 404 Not Found for URL https://cdn.example.com/a.pak")
                .unwrap();
        assert!(http.contains("HTTP 404"), "{http}");
        assert!(!http.contains("cdn.example.com"), "{http}");
        let network = user_facing_error("Stream error: error decoding response body").unwrap();
        assert!(network.contains("press Retry"), "{network}");
        assert_eq!(
            user_facing_error(
                "a.pak is in use and could not be replaced. Close PGR and press Update again. (Finalize error for a.pak: Could not replace a.pak. Is the game running? [os:32])"
            ),
            None
        );
        assert_eq!(user_facing_error("Validation failed: 2 file(s) are still corrupt."), None);
        for text in [http, network] {
            assert!(!text.contains(" \u{2014} ") && !text.contains(" \u{2013} ") && !text.contains(" - "));
        }
    }

    #[test]
    fn a_host_outage_is_passed_on_as_it_is_and_never_read_as_a_network_blip() {
        let message = peebify_transfer::files::host_unreachable_message("cdn.example.com");
        assert_ne!(super::super::fs_util::classify(&message), super::super::fs_util::FailureKind::Network);
        assert_eq!(user_facing_error(&message), None);
    }

    #[test]
    fn a_full_disk_is_recognized_from_the_os_and_from_the_space_check() {
        use super::super::fs_util::is_disk_full;
        assert!(is_disk_full(
            "Not enough disk space in D:\\Games. This needs about 3.0 GB free but only 1.0 GB is available. Free up space and try again."
        ));
        assert!(is_disk_full("Write error: x (os error 112)"));
        assert!(!is_disk_full("range request failed: connection reset"));
    }

    #[test]
    fn replaced_files_count_their_growth_and_the_copies_waiting_beside_their_parts() {
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let cap = peebify_transfer::ledger::CHECKPOINT_BYTES;
        let paks: Vec<(u64, u64)> = (0..10).map(|_| (2 * gib, 2 * gib - 100 * mib)).collect();
        let needed = replacement_bytes(&paks, 4, cap);
        let growth = 10 * 100 * mib;
        let in_flight = 4 * (2 * gib - 100 * mib);
        assert_eq!(needed, growth + in_flight + cap);
        assert!(needed < 20 * gib);

        assert_eq!(replacement_bytes(&[(gib, 0), (10 * mib, 0)], 24, cap), gib + 10 * mib);
        assert_eq!(replacement_bytes(&[(10 * mib, 50 * mib)], 24, cap), 10 * mib);

        let small: Vec<(u64, u64)> = (0..40).map(|_| (mib, mib)).collect();
        assert_eq!(replacement_bytes(&small, 24, cap), 40 * mib);
        assert_eq!(replacement_bytes(&[], 24, cap), 0);

        let hashed = [(10u64 << 30, 10u64 << 30), (10 << 30, 10 << 30)];
        assert_eq!(replacement_bytes(&hashed, 1, 0), 10 << 30);
        assert_eq!(replacement_bytes(&hashed, 24, 0), 20 << 30);
        assert_eq!(replacement_bytes(&[(500, 0)], 24, 0), 500);
        assert_eq!(replacement_bytes(&[(500, 200)], 24, 0), 300 + 200);
        assert_eq!(replacement_bytes(&[(100, 400)], 24, 0), 100);

        assert_eq!(replacement_bytes(&[(12, 0), (12, 4), (12, 12)], 0, 0), 20);
    }

    #[test]
    fn nothing_to_write_needs_nothing() {
        assert_eq!(required_free_bytes(0, 2.1, HEADROOM_INSTALL), 0);
    }

    #[test]
    fn small_writes_use_the_headroom_floor() {
        let gib = 1u64 << 30;
        assert_eq!(required_free_bytes(gib, 1.0, HEADROOM_INSTALL), gib + HEADROOM_INSTALL);
    }

    #[test]
    fn large_writes_scale_the_headroom() {
        let write = 100u64 << 30;
        assert_eq!(
            required_free_bytes(write, SPLIT_ARCHIVE_MULTIPLIER, HEADROOM_INSTALL),
            (write as f64 * SPLIT_ARCHIVE_MULTIPLIER) as u64 + write / 20
        );
    }

    #[test]
    fn rebase_moves_a_url_onto_another_mirror() {
        let bases: Vec<String> = vec!["https://a.example.com/game".into(), "https://b.example.com/game".into()];
        assert_eq!(
            mirror_rebase("https://a.example.com/game/data/x.pak", &bases, "https://b.example.com/game").as_deref(),
            Some("https://b.example.com/game/data/x.pak")
        );
        assert_eq!(mirror_rebase("https://a.example.com/gamex/x.pak", &bases, "https://b.example.com/game"), None);
        assert_eq!(mirror_rebase("https://other.example.com/game/x", &bases, "https://b.example.com/game"), None);
        assert!(mirror_covers("https://b.example.com/game/x", &bases));
        assert!(!mirror_covers("https://b.example.com/gamex", &bases));
    }

    #[test]
    fn rebase_prefers_the_longest_base() {
        let bases: Vec<String> = vec!["https://a.example.com".into(), "https://a.example.com/deep".into()];
        assert_eq!(
            mirror_rebase("https://a.example.com/deep/f", &bases, "https://b.example.com/x").as_deref(),
            Some("https://b.example.com/x/f")
        );
    }

    #[test]
    fn a_mirror_set_routes_to_the_active_server_and_moves_on_when_one_fails() {
        let moved = |switch: Switch| !matches!(switch, Switch::Stuck);
        let mirrors = MirrorSet::new(
            vec!["https://a.example.com/game".into(), "https://b.example.com/game".into()],
            Some(1),
        );
        assert_eq!(mirrors.route("https://a.example.com/game/x.pak"), "https://b.example.com/game/x.pak");
        assert_eq!(mirrors.route("https://other.example.com/x.pak"), "https://other.example.com/x.pak");

        assert!(moved(mirrors.switch_from("https://b.example.com/game/x.pak")));
        assert_eq!(mirrors.epoch(), 1);
        assert_eq!(mirrors.route("https://a.example.com/game/x.pak"), "https://a.example.com/game/x.pak");
        assert!(moved(mirrors.switch_from("https://b.example.com/game/y.pak")), "already moved off b");
        assert!(!moved(mirrors.switch_from("https://a.example.com/game/x.pak")), "every server failed");
        assert!(!moved(mirrors.switch_from("https://other.example.com/x.pak")));

        let publisher_only = MirrorSet::new(vec!["https://a.example.com/pkg".into()], Some(0));
        assert_eq!(publisher_only.route("https://a.example.com/pkg/full.zip"), "https://a.example.com/pkg/full.zip");
        assert!(!moved(publisher_only.switch_from("https://a.example.com/pkg/full.zip")));

        let untouched = MirrorSet::new(vec!["https://a.example.com/game".into()], None);
        assert_eq!(untouched.route("https://a.example.com/game/x.pak"), "https://a.example.com/game/x.pak");
    }

    #[test]
    fn archive_members_need_growth_and_room_beside_the_old_copy_of_one_file_per_worker() {
        let dir = super::super::fs_util::TempDir::new("member-room");
        let files: Vec<Entry> = (0..10u64)
            .map(|i| {
                std::fs::write(dir.0.join(format!("m{i}.bin")), vec![0u8; i as usize + 1]).unwrap();
                Entry::new(format!("m{i}.bin"), 100, Hash::Crc32(0), "").member_of(&[].into())
            })
            .collect();
        assert_eq!(write_bytes(&dir.0, &files, 0), 945 + 52);
        assert_eq!(write_bytes(&dir.0, &files, 1 << 30), 945 + 52 + 3, "the rest wait for the next checkpoint");
    }

    #[test]
    fn only_the_live_channel_may_install() {
        assert!(unsupported_channel("default").is_none());
        assert!(unsupported_channel("predownload").is_some());
        assert!(unsupported_channel("").is_some());
    }

    #[test]
    fn only_the_first_write_of_an_in_place_update_is_recorded() {
        let writes = InstallWrites::default();
        let records = std::sync::atomic::AtomicUsize::new(0);
        let write = || {
            writes.before_write(|| {
                records.fetch_add(1, Ordering::SeqCst);
            })
        };
        writes.begin(false);
        write();
        assert_eq!(records.load(Ordering::SeqCst), 0);
        writes.begin(true);
        write();
        write();
        assert_eq!(records.load(Ordering::SeqCst), 1);
        writes.end();
        write();
        assert_eq!(records.load(Ordering::SeqCst), 1);
        writes.begin(true);
        write();
        assert_eq!(records.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn writers_wait_until_the_first_write_is_recorded() {
        let writes = Arc::new(InstallWrites::default());
        writes.begin(true);
        let saved = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let first = {
            let (writes, saved) = (Arc::clone(&writes), Arc::clone(&saved));
            std::thread::spawn(move || {
                writes.before_write(|| {
                    entered_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    saved.store(true, Ordering::SeqCst);
                });
            })
        };
        entered_rx.recv().unwrap();
        writes.before_write(|| panic!("the record is saved once"));
        assert!(saved.load(Ordering::SeqCst));
        first.join().unwrap();
    }

    const GIB: u64 = 1 << 30;

    fn required(game: impl Publisher, download: u64, install: u64) -> u64 {
        let (write_bytes, multiplier) = game.write_space(download, install);
        required_free_bytes(write_bytes, multiplier, HEADROOM_INSTALL)
    }

    #[test]
    fn each_kind_of_install_needs_room_for_what_it_writes() {
        let archives = (50 * GIB) as f64 * SPLIT_ARCHIVE_MULTIPLIER;
        let archives = archives as u64 + (50 * GIB / 20).max(HEADROOM_INSTALL);
        assert_eq!(required(hypergryph::Hypergryph, 50 * GIB, 0), archives);
        assert_eq!(required(bluepoch::Bluepoch, 50 * GIB, 60 * GIB), archives);
        assert_eq!(required(hypergryph::Hypergryph, 50 * GIB, 60 * GIB), 110 * GIB + (110 * GIB / 20).max(HEADROOM_INSTALL));
        assert_eq!(required(bd2::Bd2::default(), 4 * GIB, 6 * GIB), 10 * GIB + HEADROOM_INSTALL);
        assert_eq!(required(sophon::Sophon, 20 * GIB, 30 * GIB), 30 * GIB + HEADROOM_INSTALL);
        assert_eq!(required(kuro::Kuro::default(), 30 * GIB, 0), 30 * GIB + HEADROOM_INSTALL);
        assert_eq!(required(kuro::Kuro::default(), 0, 0), 0, "unknown sizes need nothing");
    }

    #[test]
    fn only_client_errors_are_permanent_for_a_transfer() {
        let status = http::permanent_client_status;
        for code in [403, 404, 410] {
            assert_eq!(status(&format!("HTTP {code} for https://cdn/x.pak")), Some(code));
        }
        for transient in ["HTTP 408 for https://cdn/x.pak", "HTTP 429 for https://cdn/x.pak", "HTTP 503 for https://cdn/x.pak", "HTTP 206 for https://cdn/x.pak"] {
            assert_eq!(status(transient), None, "{transient}");
        }
        let refused = "The server could not continue a.pak from where it stopped, so it starts over.";
        for other in ["Request error: timed out", "failed verification after re-download (got a, expected b)", "Write error: HTTP 404", refused] {
            assert_eq!(status(other), None, "{other}");
        }
        assert_eq!(super::super::fs_util::classify(refused), super::super::fs_util::FailureKind::Other);
        assert!(status("HTTP Error: 404 for URL https://cdn/a.pak").is_some());
    }

    #[test]
    fn index_mismatches_read_as_repair_advice() {
        let missing = repair_failure("HTTP Error: 404 for URL https://cdn.example.com/game/Client/pakchunk1.pak?sign=x");
        assert!(missing.starts_with("The download server has no usable copy of pakchunk1.pak (HTTP 404)."), "{missing}");
        let stale = repair_failure(
            "The server sent a copy of Client/Content/a.pak that does not match its checksum. Try again later.",
        );
        assert!(stale.starts_with("a.pak doesn't match the file index"), "{stale}");
        assert_eq!(repair_failure("Repair aborted"), "Repair aborted");
    }

    #[test]
    fn a_quick_repair_fetches_recorded_files_from_the_manifest() {
        use crate::backend::manifest::Hash;
        let manifest = Manifest {
            version: "1.0".to_string(),
            entries: vec![Entry::new("Client/A.pak", 4, Hash::None, "https://cdn.example/Client/A.pak")],
            ..Manifest::default()
        };
        let recorded = |path: &str| Entry::new(path, 4, Hash::None, "");
        let found = listed_in(&manifest, &[recorded("client\\a.pak")]).unwrap();
        assert_eq!(found[0].url(), "https://cdn.example/Client/A.pak");
        let gone = listed_in(&manifest, &[recorded("client/a.pak"), recorded("old.pak")]).unwrap_err();
        assert!(gone.starts_with("1 file(s)"), "{gone}");
    }

    #[test]
    fn checksum_failures_classify_as_validation() {
        assert_eq!(
            super::super::fs_util::classify(
                "failed verification after re-download (got a, expected b)"
            ),
            super::super::fs_util::FailureKind::Validation
        );
    }

    #[test]
    fn a_clean_verify_is_complete_without_an_error() {
        let payload = verify_result_payload(0);
        assert_eq!(payload["status"], "Verification Complete");
        assert!(payload.get("error").is_none());
    }

    #[test]
    fn a_verify_with_broken_files_explains_itself() {
        let payload = verify_result_payload(3);
        assert_eq!(payload["status"], "Verification Failed");
        assert_eq!(payload["error"], "3 files need repair. Run a repair to replace them.");
        assert_eq!(broken_files_error(1), "1 file needs repair. Run a repair to replace it.");
    }

    #[test]
    fn only_the_cancel_flag_turns_a_verify_failure_into_a_cancel() {
        let flag = AtomicBool::new(false);
        let aborted = "Write error: The I/O operation has been aborted because of either a thread exit or an application request. (os error 995)".to_string();
        assert!(matches!(verify_failure(&flag, aborted), VerifyError::Other(_)));
        assert!(matches!(
            verify_failure(&flag, "Operation cancelled by user.".to_string()),
            VerifyError::Other(_)
        ));
        flag.store(true, Ordering::SeqCst);
        assert!(matches!(
            verify_failure(&flag, "anything".to_string()),
            VerifyError::Cancelled
        ));
    }

    #[test]
    fn a_pending_update_is_named_only_when_the_recorded_version_is_older() {
        assert_eq!(pending_update_note(3, Some("1.2.0"), "1.2.0"), None);
        assert_eq!(pending_update_note(0, Some("1.1.0"), "1.2.0"), None);
        assert_eq!(pending_update_note(3, None, "1.2.0"), None);
        assert_eq!(pending_update_note(3, Some("1.1.0"), ""), None);
        assert_eq!(
            pending_update_note(3, Some("1.1.0"), "1.2.0").as_deref(),
            Some("3 files differ from version 1.2.0. An update is available, and it replaces these files.")
        );
        assert_eq!(
            pending_update_note(1, Some(" 1.1.0 "), "1.2.0").as_deref(),
            Some("1 file differs from version 1.2.0. An update is available, and it replaces that file.")
        );
    }

    #[test]
    fn only_an_older_install_reads_its_differences_as_the_pending_update() {
        assert!(older_install_note(3, Some("1.1.0"), "1.2.0").is_some());
        assert_eq!(older_install_note(3, Some("1.2.0"), "1.2.0"), None);
        assert_eq!(older_install_note(3, Some("1.2"), "1.2.0"), None);
        assert_eq!(older_install_note(3, Some("1.3.0"), "1.2.0"), None);
        assert_eq!(older_install_note(3, None, "1.2.0"), None);
        assert_eq!(older_install_note(0, Some("1.1.0"), "1.2.0"), None);
    }
}
