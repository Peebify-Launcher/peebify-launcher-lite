// ------------ Progress Tracking ------------
// Counts bytes and files for a running job and works out the percentage, smoothed speed and time left.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

pub const MIN_UI_UPDATE_INTERVAL_MS: u64 = 200;
const SPEED_SMOOTHING_FACTOR: f64 = 0.7;

struct TrackerState {
    total_bytes: f64,

    total_files: usize,
    file_sizes: HashMap<String, f64>,

    average_speed: f64,
    last_update: Instant,
    last_bytes: f64,

    network_speed: f64,
    last_network: u64,
    disk_speed: f64,
    last_disk: u64,

    phase: String,

    repaired_files: usize,
}

impl TrackerState {
    fn new() -> Self {
        Self {
            total_bytes: 0.0,
            total_files: 0,
            file_sizes: HashMap::new(),
            average_speed: 0.0,
            last_update: Instant::now(),
            last_bytes: 0.0,
            network_speed: 0.0,
            last_network: 0,
            disk_speed: 0.0,
            last_disk: 0,
            phase: "idle".to_string(),
            repaired_files: 0,
        }
    }

    fn is_transfer_phase(&self) -> bool {
        self.phase == "downloading" || self.phase == "repairing"
    }
}

pub struct ProgressTracker {
    state: Mutex<TrackerState>,
    downloaded: AtomicI64,
    processed: AtomicI64,
    network: AtomicU64,
    disk: AtomicU64,
    file_counters: RwLock<HashMap<String, Arc<AtomicI64>>>,
    ui_epoch: Instant,
    last_ui_ms: AtomicU64,
    force_ui: AtomicBool,
}

#[derive(Clone)]
pub struct FileCounter(Arc<AtomicI64>);

impl FileCounter {
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TrackerState::new()),
            downloaded: AtomicI64::new(0),
            processed: AtomicI64::new(0),
            network: AtomicU64::new(0),
            disk: AtomicU64::new(0),
            file_counters: RwLock::new(HashMap::new()),
            ui_epoch: Instant::now(),
            last_ui_ms: AtomicU64::new(0),
            force_ui: AtomicBool::new(true),
        }
    }

    pub fn reset(&self) {
        *self.state.lock() = TrackerState::new();
        self.downloaded.store(0, Ordering::SeqCst);
        self.processed.store(0, Ordering::SeqCst);
        self.network.store(0, Ordering::SeqCst);
        self.disk.store(0, Ordering::SeqCst);
        self.file_counters.write().clear();
    }

    pub fn add_network(&self, bytes: u64) {
        self.network.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_disk(&self, bytes: u64) {
        self.disk.fetch_add(bytes, Ordering::Relaxed);
    }

    fn rebase_rates(&self, s: &mut TrackerState) {
        s.last_network = self.network.load(Ordering::Relaxed);
        s.last_disk = self.disk.load(Ordering::Relaxed);
    }

    fn current_bytes(&self, state: &TrackerState) -> f64 {
        let raw = if state.is_transfer_phase() {
            self.downloaded.load(Ordering::Relaxed)
        } else {
            self.processed.load(Ordering::Relaxed)
        };
        (raw.max(0) as f64).min(state.total_bytes.max(0.0)).max(0.0)
    }

    fn counter_for(&self, file_id: &str) -> Arc<AtomicI64> {
        let existing = self.file_counters.read().get(file_id).map(Arc::clone);
        if let Some(counter) = existing {
            return counter;
        }
        let mut map = self.file_counters.write();
        Arc::clone(
            map.entry(file_id.to_string())
                .or_insert_with(|| Arc::new(AtomicI64::new(0))),
        )
    }

    pub fn counter(&self, file_id: &str) -> FileCounter {
        FileCounter(self.counter_for(file_id))
    }

    pub fn begin(
        &self,
        phase: &str,
        total_bytes: f64,
        total_files: usize,
        file_sizes: Option<HashMap<String, f64>>,
    ) {
        self.reset();
        self.set_totals(total_bytes, total_files);
        if let Some(sizes) = file_sizes {
            self.set_file_sizes(sizes);
        }
        self.set_phase(phase);
    }

    pub fn set_totals(&self, total_bytes: f64, total_files: usize) {
        let mut s = self.state.lock();
        s.total_bytes = total_bytes;
        s.total_files = total_files;
    }

    pub fn set_file_sizes(&self, sizes: HashMap<String, f64>) {
        {
            let mut counters = self.file_counters.write();
            counters.clear();
            for id in sizes.keys() {
                counters.insert(id.clone(), Arc::new(AtomicI64::new(0)));
            }
        }
        self.state.lock().file_sizes = sizes;
    }

    pub fn set_phase(&self, phase: &str) {
        let mut s = self.state.lock();
        s.phase = phase.to_string();
        s.last_update = Instant::now();
        s.last_bytes = self.current_bytes(&s);
        self.rebase_rates(&mut s);
    }

    pub fn phase(&self) -> String {
        self.state.lock().phase.clone()
    }

    pub fn increment_repaired_files(&self) {
        self.state.lock().repaired_files += 1;
    }

    pub fn repaired_files(&self) -> usize {
        self.state.lock().repaired_files
    }

    pub fn total_files(&self) -> usize {
        self.state.lock().total_files
    }

    pub fn update_validation_progress(&self, bytes: f64) {
        self.processed.fetch_add(bytes as i64, Ordering::Relaxed);
    }

    pub fn update_file_progress(&self, file_id: &str, bytes: f64, complete: bool) {
        if complete {
            self.file_done(file_id);
        } else {
            self.add_bytes(file_id, bytes as i64);
        }
    }

    pub fn add_bytes(&self, file_id: &str, delta: i64) {
        let counter = self.counter_for(file_id);
        self.add_to(&FileCounter(counter), delta);
    }

    pub fn add_to(&self, counter: &FileCounter, delta: i64) {
        self.downloaded.fetch_add(delta, Ordering::Relaxed);
        self.processed.fetch_add(delta, Ordering::Relaxed);
        counter.0.fetch_add(delta, Ordering::Relaxed);
    }

    pub fn file_done(&self, file_id: &str) {
        let expected = self
            .state
            .lock()
            .file_sizes
            .get(file_id)
            .copied()
            .unwrap_or(0.0) as i64;
        self.settle(&self.counter_for(file_id), expected);
    }

    fn settle(&self, counter: &AtomicI64, bytes: i64) {
        let counted = counter.swap(bytes, Ordering::Relaxed);
        let settle = bytes - counted;
        if settle != 0 {
            self.downloaded.fetch_add(settle, Ordering::Relaxed);
            self.processed.fetch_add(settle, Ordering::Relaxed);
        }
    }

    pub fn transfer_bytes(&self) -> (f64, f64) {
        let s = self.state.lock();
        (self.current_bytes(&s), s.total_bytes)
    }

    pub fn reset_speed_baseline(&self) {
        let mut s = self.state.lock();
        s.last_update = Instant::now();
        s.last_bytes = self.current_bytes(&s);
        self.rebase_rates(&mut s);
    }

    pub fn force_completion(&self) {
        let mut s = self.state.lock();
        let total = s.total_bytes.max(0.0) as i64;
        self.downloaded.store(total, Ordering::SeqCst);
        self.processed.store(total, Ordering::SeqCst);
        s.last_bytes = s.total_bytes;
        log::info!("Progress forced to completion state.");
    }

    fn update_speed_metrics(&self, s: &mut TrackerState) {
        let now = Instant::now();
        let time_diff = now.duration_since(s.last_update).as_secs_f64();
        if time_diff > 0.1 {
            let current = self.current_bytes(s);
            let instant = ((current - s.last_bytes) / time_diff).max(0.0);
            s.average_speed = smoothed_speed(s.average_speed, instant);
            let network = self.network.load(Ordering::Relaxed);
            let disk = self.disk.load(Ordering::Relaxed);
            let per_second = |now: u64, last: u64| now.saturating_sub(last) as f64 / time_diff;
            s.network_speed = smoothed_speed(s.network_speed, per_second(network, s.last_network));
            s.disk_speed = smoothed_speed(s.disk_speed, per_second(disk, s.last_disk));
            s.last_network = network;
            s.last_disk = disk;
            s.last_update = now;
            s.last_bytes = current;
        }
    }

    pub fn calculate_metrics(&self) -> Value {
        let mut s = self.state.lock();
        self.update_speed_metrics(&mut s);

        let current_bytes = self.current_bytes(&s);
        let percentage = if s.total_bytes > 0.0 {
            ((current_bytes / s.total_bytes) * 100.0).min(100.0)
        } else {
            0.0
        };
        let remaining = s.total_bytes - current_bytes;
        let eta = if s.average_speed > 0.0 {
            remaining / s.average_speed
        } else {
            0.0
        };
        json!({
            "percentage": percentage,
            "speed": s.average_speed,
            "networkSpeed": s.network_speed,
            "diskSpeed": s.disk_speed,
            "eta": eta,
            "processedBytes": current_bytes,
            "totalBytes": s.total_bytes,
            "phase": s.phase,
        })
    }

    pub fn should_update_ui(&self) -> bool {
        let now = self.ui_epoch.elapsed().as_millis() as u64;
        if self.force_ui.swap(false, Ordering::Relaxed) {
            self.last_ui_ms.store(now, Ordering::Relaxed);
            return true;
        }
        let last = self.last_ui_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= MIN_UI_UPDATE_INTERVAL_MS {
            self.last_ui_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        } else {
            false
        }
    }

    pub fn force_next_update(&self) {
        self.force_ui.store(true, Ordering::Relaxed);
    }
}

fn smoothed_speed(previous: f64, instant: f64) -> f64 {
    let next = if previous <= 0.0 {
        instant
    } else {
        previous * SPEED_SMOOTHING_FACTOR + instant * (1.0 - SPEED_SMOOTHING_FACTOR)
    };
    next.max(0.0)
}

pub fn gib(bytes: f64) -> f64 {
    bytes / 1024.0 / 1024.0 / 1024.0
}

pub struct Publisher {
    task: tokio::task::JoinHandle<()>,
}

impl Publisher {
    pub fn every(interval: Duration, mut publish: impl FnMut() + Send + 'static) -> Self {
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                publish();
            }
        });
        Self { task }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downloaded(tracker: &ProgressTracker, bytes: i64) {
        tracker.downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    fn processed(tracker: &ProgressTracker) -> f64 {
        tracker.calculate_metrics()["processedBytes"].as_f64().unwrap()
    }

    #[test]
    fn begin_downloading_reflects_download_progress() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 1000.0, 2, None);
        downloaded(&tracker, 400);
        let metrics = tracker.calculate_metrics();
        assert_eq!(metrics["processedBytes"].as_f64(), Some(400.0));
        assert_eq!(metrics["phase"].as_str(), Some("downloading"));
        assert_eq!(tracker.total_files(), 2);
        assert_eq!(metrics["percentage"].as_f64(), Some(40.0));
    }

    #[test]
    fn a_bare_reset_ignores_download_progress() {
        let tracker = ProgressTracker::new();
        tracker.reset();
        tracker.set_totals(1000.0, 1);
        downloaded(&tracker, 400);
        assert_eq!(processed(&tracker), 0.0);
    }

    #[test]
    fn begin_clears_the_previous_stage() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 1000.0, 1, None);
        downloaded(&tracker, 900);
        tracker.begin("validating", 500.0, 1, None);
        assert_eq!(processed(&tracker), 0.0);
        tracker.update_validation_progress(200.0);
        assert_eq!(processed(&tracker), 200.0);
    }

    #[test]
    fn file_done_settles_to_the_recorded_size() {
        let tracker = ProgressTracker::new();
        let sizes = HashMap::from([("a".to_string(), 300.0), ("b".to_string(), 700.0)]);
        tracker.begin("downloading", 1000.0, 2, Some(sizes));
        tracker.add_bytes("a", 120);
        tracker.file_done("a");
        assert_eq!(processed(&tracker), 300.0);
        tracker.add_bytes("b", 800);
        tracker.file_done("b");
        assert_eq!(processed(&tracker), 1000.0);
    }

    #[test]
    fn a_counter_handle_moves_the_same_totals_as_the_named_api() {
        let tracker = ProgressTracker::new();
        let sizes = HashMap::from([("a".to_string(), 300.0)]);
        tracker.begin("downloading", 300.0, 1, Some(sizes));
        let a = tracker.counter("a");
        tracker.add_to(&a, 100);
        tracker.add_bytes("a", 50);
        assert_eq!(a.get(), 150);
        assert_eq!(processed(&tracker), 150.0);
        tracker.file_done("a");
        assert_eq!(processed(&tracker), 300.0);
    }

    #[test]
    fn force_completion_reads_full() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 1000.0, 1, None);
        downloaded(&tracker, 10);
        tracker.force_completion();
        assert_eq!(
            tracker.calculate_metrics()["percentage"].as_f64(),
            Some(100.0)
        );
    }

    #[test]
    fn a_steady_rate_reads_true_from_the_first_sample() {
        assert_eq!(smoothed_speed(0.0, 100.0), 100.0);
        let mut speed = 0.0;
        for _ in 0..5 {
            speed = smoothed_speed(speed, 100.0);
        }
        assert!((speed - 100.0).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn a_speed_drop_is_followed_within_a_few_seconds() {
        let mut speed = 100.0;
        for _ in 0..10 {
            speed = smoothed_speed(speed, 10.0);
        }
        assert!(speed > 10.0 && speed < 13.0, "{speed}");
    }

    #[test]
    fn the_tracker_reports_the_first_measured_rate_in_full() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 10_000.0, 1, None);
        tracker.state.lock().last_update = Instant::now() - Duration::from_secs(1);
        downloaded(&tracker, 1000);
        let speed = tracker.calculate_metrics()["speed"].as_f64().unwrap();
        assert!(speed > 800.0 && speed <= 1000.0, "{speed}");
    }

    #[test]
    fn speed_is_measured_on_the_monotonic_clock() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 1000.0, 1, None);
        tracker.state.lock().last_update = Instant::now() - Duration::from_secs(1);
        downloaded(&tracker, 500);
        let speed = tracker.calculate_metrics()["speed"].as_f64().unwrap();
        assert!(speed > 0.0);
    }

    #[test]
    fn network_and_disk_rates_ignore_progress_that_moved_no_bytes() {
        let tracker = ProgressTracker::new();
        tracker.begin("downloading", 10_000.0, 1, None);
        tracker.state.lock().last_update = Instant::now() - Duration::from_secs(1);
        downloaded(&tracker, 5000);
        tracker.add_network(200);
        tracker.add_disk(400);
        let metrics = tracker.calculate_metrics();
        let network = metrics["networkSpeed"].as_f64().unwrap();
        let disk = metrics["diskSpeed"].as_f64().unwrap();
        assert!(network > 150.0 && network <= 200.0, "{network}");
        assert!(disk > 300.0 && disk <= 400.0, "{disk}");
        assert!(metrics["speed"].as_f64().unwrap() > 4000.0);
    }

    #[tokio::test]
    async fn the_publisher_beats_until_it_is_dropped() {
        let beats = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&beats);
        let publisher = Publisher::every(Duration::from_millis(10), move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(publisher);
        let seen = beats.load(Ordering::SeqCst);
        assert!(seen >= 2, "{seen}");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(beats.load(Ordering::SeqCst), seen);
    }
}
