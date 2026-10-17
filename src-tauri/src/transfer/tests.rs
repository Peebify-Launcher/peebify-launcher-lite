// ------------ End-to-End Tests ------------
// Real transfers against the local test server: resume, restarts, stalls, splits, limits, the gaming hold and crash recovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use parking_lot::Mutex;

use crate::files::{download_files, FileSpec, JobHooks};
use crate::fsx::{md5_hex, ContentHasher};
use crate::ledger::Ledger;
use crate::progress::ProgressTracker;
use crate::runtime::{RuntimeConfig, Settings, TransferRuntime};
use crate::testing::{install_crypto, pattern, scratch_dir, Behavior, TestServer};
use crate::RunGate;

struct Hooks {
    gate: RunGate,
    tracker: ProgressTracker,
    dir: PathBuf,
    verified: Mutex<HashSet<String>>,
    mirrors: Option<(String, String)>,
    moved: AtomicU64,
    refused: AtomicUsize,
}

impl Hooks {
    fn new(dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            gate: RunGate::sharing(Arc::default(), false),
            tracker: ProgressTracker::new(),
            dir: dir.to_path_buf(),
            verified: Mutex::new(HashSet::new()),
            mirrors: None,
            moved: AtomicU64::new(0),
            refused: AtomicUsize::new(0),
        })
    }
}

impl JobHooks for Hooks {
    fn gate(&self) -> &RunGate {
        &self.gate
    }
    fn tracker(&self) -> &ProgressTracker {
        &self.tracker
    }
    fn install_dir(&self) -> &Path {
        &self.dir
    }
    fn route(&self, url: &str) -> String {
        match &self.mirrors {
            Some((from, to)) if self.moved.load(Ordering::SeqCst) > 0 => url.replace(from, to),
            _ => url.to_string(),
        }
    }
    fn route_epoch(&self) -> u64 {
        self.moved.load(Ordering::SeqCst)
    }
    fn on_refused(&self, _url: &str) -> bool {
        self.refused.fetch_add(1, Ordering::SeqCst);
        if self.mirrors.is_some() && self.moved.load(Ordering::SeqCst) == 0 {
            self.moved.store(1, Ordering::SeqCst);
            return true;
        }
        false
    }
    fn reaches_home(&self) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
    fn verified(&self, id: &str) {
        self.verified.lock().insert(id.to_string());
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        stall_timeout: Duration::from_millis(400),
        checkpoint_interval: Duration::from_millis(100),
        split_min_left: 2 << 20,
        preallocate_min: 4 << 20,
        max_retries: 6,
        retry_base_ms: 10,
        network_recheck: Duration::from_millis(20),
        network_probe_rounds: 2,
        ..RuntimeConfig::default()
    }
}

fn runtime_with(config: RuntimeConfig, connections: usize) -> Arc<TransferRuntime> {
    install_crypto();
    let bulk = crate::net::bulk_client(reqwest::Client::builder(), "peebify-test")
        .build()
        .unwrap();
    let multiplexed = crate::net::multiplexed_client(reqwest::Client::builder(), "peebify-test")
        .build()
        .unwrap();
    let rt = TransferRuntime::new(bulk, multiplexed, config);
    rt.apply_settings(Settings {
        connections,
        ..Settings::default()
    });
    rt
}

fn runtime(connections: usize) -> Arc<TransferRuntime> {
    runtime_with(config(), connections)
}

fn spec(dir: &Path, id: &str, url: &str, body: &[u8], md5: Option<&str>) -> FileSpec {
    FileSpec {
        id: id.to_string(),
        path: dir.join(id),
        size: body.len() as u64,
        md5: md5.map_or_else(|| md5_hex(body), str::to_string),
        url: url.to_string(),
        ..FileSpec::default()
    }
}

async fn run(
    rt: &Arc<TransferRuntime>,
    hooks: &Arc<Hooks>,
    files: Vec<FileSpec>,
) -> Result<crate::TransferReport, String> {
    let ledger = Ledger::new(None);
    let total: u64 = files.iter().map(|f| f.size).sum();
    hooks.tracker.begin(
        "downloading",
        total as f64,
        files.len(),
        Some(files.iter().map(|f| (f.id.clone(), f.size as f64)).collect()),
    );
    let hooks_dyn: Arc<dyn JobHooks> = Arc::clone(hooks) as Arc<dyn JobHooks>;
    tokio::time::timeout(
        Duration::from_secs(60),
        download_files(rt, hooks_dyn, &ledger, files),
    )
    .await
    .map_err(|_| "the transfer hung".to_string())?
}

fn leftovers(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walk(dir) {
        let name = entry.to_string_lossy().to_string();
        if name.ends_with(".part") || name.ends_with(".json") || name.ends_with(".tmp") {
            out.push(name);
        }
    }
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_files_of_every_checksum_kind_land_verified_and_renamed() {
    let server = TestServer::start();
    let dir = scratch_dir("fresh");
    let a = pattern(300_000, 1);
    let b = pattern(5 << 20, 2);
    let c = pattern(70_000, 3);
    let empty: Vec<u8> = Vec::new();
    let mut crc = ContentHasher::for_expected("crc64:0");
    crc.update(&c);
    let crc = crc.finish();
    let files = vec![
        spec(&dir, "a.bin", &server.add("/a.bin", a.clone(), Behavior::default()), &a, None),
        spec(&dir, "sub/b.bin", &server.add("/b.bin", b.clone(), Behavior::default()), &b, None),
        spec(&dir, "c.bin", &server.add("/c.bin", c.clone(), Behavior::default()), &c, Some(&crc)),
        spec(&dir, "d.bin", &server.add("/d.bin", c.clone(), Behavior::default()), &c, Some("")),
        spec(&dir, "e.bin", &server.add("/e.bin", empty.clone(), Behavior::default()), &empty, None),
    ];
    let hooks = Hooks::new(&dir);
    let report = run(&runtime(4), &hooks, files).await.unwrap();

    assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), a);
    assert_eq!(std::fs::read(dir.join("sub/b.bin")).unwrap(), b);
    assert_eq!(std::fs::read(dir.join("c.bin")).unwrap(), c);
    assert_eq!(std::fs::read(dir.join("d.bin")).unwrap(), c);
    assert_eq!(std::fs::read(dir.join("e.bin")).unwrap(), empty);
    assert_eq!(leftovers(&dir), Vec::<String>::new());
    let verified = hooks.verified.lock().clone();
    for id in ["a.bin", "sub/b.bin", "c.bin", "e.bin"] {
        assert!(verified.contains(id), "{id} was hashed while it streamed");
    }
    assert!(!verified.contains("d.bin"), "a file without a checksum is only size-checked");
    assert!(report.read_back <= b.len() as u64, "only the split file reads back: {report:?}");
    let metrics = hooks.tracker.calculate_metrics();
    assert_eq!(metrics["percentage"].as_f64(), Some(100.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_connection_resumes_with_a_range() {
    let server = TestServer::start();
    let dir = scratch_dir("cut");
    let body = pattern(3 << 20, 4);
    let url = server.add(
        "/cut.bin",
        body.clone(),
        Behavior {
            cut_after: Some((1, 1 << 20)),
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    run(&runtime(1), &hooks, vec![spec(&dir, "cut.bin", &url, &body, None)])
        .await
        .unwrap();

    assert_eq!(std::fs::read(dir.join("cut.bin")).unwrap(), body);
    assert_eq!(server.requests("/cut.bin"), 2);
    assert_eq!(server.ranged_requests("/cut.bin"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_ignores_ranges_restarts_the_file_and_still_gets_it_right() {
    let server = TestServer::start();
    let dir = scratch_dir("ignore");
    let body = pattern(3 << 20, 5);
    let url = server.add(
        "/ignore.bin",
        body.clone(),
        Behavior {
            cut_after: Some((1, 1 << 20)),
            ignore_range: true,
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    run(&runtime(2), &hooks, vec![spec(&dir, "ignore.bin", &url, &body, None)])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("ignore.bin")).unwrap(), body);
    assert!(hooks.verified.lock().contains("ignore.bin"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_content_range_starts_the_file_over() {
    let server = TestServer::start();
    let dir = scratch_dir("wrong-range");
    let body = pattern(2 << 20, 6);
    let url = server.add(
        "/w.bin",
        body.clone(),
        Behavior {
            cut_after: Some((1, 300_000)),
            wrong_range_once: true,
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    run(&runtime(1), &hooks, vec![spec(&dir, "w.bin", &url, &body, None)])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("w.bin")).unwrap(), body);
    assert!(server.requests("/w.bin") >= 3, "cut, refused range, fresh start");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_stream_is_torn_down_and_retried() {
    let server = TestServer::start();
    let dir = scratch_dir("stall");
    let body = pattern(1 << 20, 7);
    let url = server.add(
        "/s.bin",
        body.clone(),
        Behavior {
            stall: Some((256 << 10, Duration::from_secs(3))),
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    let started = Instant::now();
    run(&runtime(1), &hooks, vec![spec(&dir, "s.bin", &url, &body, None)])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("s.bin")).unwrap(), body);
    assert!(started.elapsed() < Duration::from_secs(3), "the stall was cut short");
    assert_eq!(server.requests("/s.bin"), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_connections_split_a_big_file_and_it_is_hashed_without_a_full_reread() {
    let server = TestServer::start();
    let dir = scratch_dir("split");
    let body = pattern(24 << 20, 8);
    let url = server.add(
        "/big.bin",
        body.clone(),
        Behavior {
            drip: Some(24 << 20),
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    let report = run(&runtime(6), &hooks, vec![spec(&dir, "big.bin", &url, &body, None)])
        .await
        .unwrap();

    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), body);
    assert!(report.splits >= 2, "{report:?}");
    assert!(server.peak_requests("/big.bin") >= 3);
    assert!(
        report.read_back < body.len() as u64,
        "the first range is hashed as it streams: {report:?}"
    );
    assert!(hooks.verified.lock().contains("big.bin"));
    assert_eq!(leftovers(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_without_a_checksum_or_etag_is_never_split() {
    let server = TestServer::start();
    let dir = scratch_dir("nosplit");
    let body = pattern(12 << 20, 9);
    let url = server.add(
        "/n.bin",
        body.clone(),
        Behavior {
            no_etag: true,
            drip: Some(24 << 20),
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    let report = run(&runtime(6), &hooks, vec![spec(&dir, "n.bin", &url, &body, Some(""))])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("n.bin")).unwrap(), body);
    assert_eq!(report.splits, 0);
    assert_eq!(server.requests("/n.bin"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_never_exceed_the_budget() {
    let server = TestServer::start();
    let dir = scratch_dir("budget");
    let mut files = Vec::new();
    for i in 0..30u8 {
        let body = pattern(200_000, i);
        let path = format!("/f{i}.bin");
        let url = server.add(
            &path,
            body.clone(),
            Behavior {
                drip: Some(4 << 20),
                ..Behavior::default()
            },
        );
        files.push(spec(&dir, &format!("f{i}.bin"), &url, &body, None));
    }
    let hooks = Hooks::new(&dir);
    run(&runtime(3), &hooks, files).await.unwrap();
    assert!(server.peak_connections() <= 3 + 1, "peak {}", server.peak_connections());
    for i in 0..30u8 {
        assert_eq!(std::fs::read(dir.join(format!("f{i}.bin"))).unwrap(), pattern(200_000, i));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_pushes_back_still_delivers_every_file() {
    let server = TestServer::start();
    let dir = scratch_dir("aimd");
    let mut files = Vec::new();
    for i in 0..12u8 {
        let body = pattern(150_000, i);
        let path = format!("/p{i}.bin");
        let url = server.add(
            &path,
            body.clone(),
            Behavior {
                status: Some((1, 503)),
                drip: Some(8 << 20),
                ..Behavior::default()
            },
        );
        files.push(spec(&dir, &format!("p{i}.bin"), &url, &body, None));
    }
    let hooks = Hooks::new(&dir);
    run(&runtime(8), &hooks, files).await.unwrap();
    for i in 0..12u8 {
        assert_eq!(std::fs::read(dir.join(format!("p{i}.bin"))).unwrap(), pattern(150_000, i));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_speed_limit_holds_the_rate_down() {
    let server = TestServer::start();
    let dir = scratch_dir("limit");
    let body = pattern(3 << 20, 10);
    let url = server.add("/l.bin", body.clone(), Behavior::default());
    let rt = runtime(4);
    rt.apply_settings(Settings {
        speed_limit: 2 << 20,
        connections: 4,
        ..Settings::default()
    });
    let hooks = Hooks::new(&dir);
    let started = Instant::now();
    run(&rt, &hooks, vec![spec(&dir, "l.bin", &url, &body, None)])
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!(std::fs::read(dir.join("l.bin")).unwrap(), body);
    assert!(took >= Duration::from_millis(1200), "3 MiB at 2 MiB/s took {took:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gaming_hold_stops_a_transfer_until_the_user_overrides_it() {
    let _hold = crate::testing::hold_lock();
    let server = TestServer::start();
    let dir = scratch_dir("hold");
    let body = pattern(2 << 20, 11);
    let url = server.add("/h.bin", body.clone(), Behavior::default());
    let rt = runtime(2);
    crate::GAMING_HOLD.set(true);
    let hooks = Arc::new(Hooks {
        gate: RunGate::new(),
        ..Arc::try_unwrap(Hooks::new(&dir)).ok().unwrap()
    });
    let files = vec![spec(&dir, "h.bin", &url, &body, None)];
    let job = {
        let rt = Arc::clone(&rt);
        let hooks = Arc::clone(&hooks);
        tokio::spawn(async move { run(&rt, &hooks, files).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requests_while_held = server.requests("/h.bin");
    hooks.gate.set_hold_override(true);
    let outcome = job.await.unwrap();
    crate::GAMING_HOLD.set(false);

    assert_eq!(requests_while_held, 0);
    outcome.unwrap();
    assert_eq!(std::fs::read(dir.join("h.bin")).unwrap(), body);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_loses_only_what_no_checkpoint_recorded() {
    for with_checksum in [true, false] {
        let server = TestServer::start();
        let dir = scratch_dir("crash");
        let body = pattern(20 << 20, 12);
        let url = server.add(
            "/c.bin",
            body.clone(),
            Behavior {
                drip: Some(16 << 20),
                ..Behavior::default()
            },
        );
        let md5 = if with_checksum { None } else { Some("") };
        let first = runtime_with(
            RuntimeConfig {
                skip_final_checkpoint: true,
                ..config()
            },
            2,
        );
        let hooks = Hooks::new(&dir);
        let job = {
            let hooks = Arc::clone(&hooks);
            let files = vec![spec(&dir, "c.bin", &url, &body, md5)];
            tokio::spawn(async move { run(&first, &hooks, files).await })
        };
        tokio::time::sleep(Duration::from_millis(600)).await;
        hooks.gate.cancel();
        assert!(job.await.unwrap().is_err());

        let file_path = dir.join("c.bin");
        let part = crate::part::part_path_for(&file_path);
        let durable = crate::part::load_sidecar(&file_path, body.len() as u64)
            .map(|s| s.durable)
            .unwrap_or_default();
        if part.exists() {
            let mut bytes = std::fs::read(&part).unwrap();
            for (i, b) in bytes.iter_mut().enumerate() {
                let i = i as u64;
                if !durable.iter().any(|s| s.start <= i && i < s.end) {
                    *b = 0;
                }
            }
            std::fs::write(&part, bytes).unwrap();
        }
        assert!(!file_path.exists());

        let again = Hooks::new(&dir);
        run(&runtime(2), &again, vec![spec(&dir, "c.bin", &url, &body, md5)])
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file_path).unwrap(), body, "checksum: {with_checksum}");
        assert_eq!(leftovers(&dir), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_that_never_matches_its_checksum_fails_after_the_refetches() {
    let server = TestServer::start();
    let dir = scratch_dir("bad");
    let body = pattern(100_000, 13);
    let url = server.add("/bad.bin", body.clone(), Behavior::default());
    let hooks = Hooks::new(&dir);
    let error = run(
        &runtime(2),
        &hooks,
        vec![spec(&dir, "bad.bin", &url, &body, Some("00000000000000000000000000000000"))],
    )
    .await
    .unwrap_err();
    assert!(error.contains("does not match its checksum"), "{error}");
    assert_eq!(server.requests("/bad.bin"), 3);
    assert!(!dir.join("bad.bin").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verified_part_already_on_disk_needs_no_request() {
    let server = TestServer::start();
    let dir = scratch_dir("ready");
    let body = pattern(500_000, 14);
    let url = server.add("/r.bin", body.clone(), Behavior::default());
    std::fs::write(crate::part::part_path_for(&dir.join("r.bin")), &body).unwrap();
    let hooks = Hooks::new(&dir);
    run(&runtime(2), &hooks, vec![spec(&dir, "r.bin", &url, &body, None)])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("r.bin")).unwrap(), body);
    assert_eq!(server.requests("/r.bin"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_file_moves_to_the_other_mirror() {
    let first = TestServer::start();
    let second = TestServer::start();
    let dir = scratch_dir("mirror");
    let body = pattern(400_000, 15);
    let url = first.add(
        "/m.bin",
        body.clone(),
        Behavior {
            status: Some((usize::MAX, 404)),
            ..Behavior::default()
        },
    );
    second.add("/m.bin", body.clone(), Behavior::default());
    let hooks = Arc::new(Hooks {
        mirrors: Some((first.base.clone(), second.base.clone())),
        ..Arc::try_unwrap(Hooks::new(&dir)).ok().unwrap()
    });
    run(&runtime(2), &hooks, vec![spec(&dir, "m.bin", &url, &body, None)])
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("m.bin")).unwrap(), body);
    assert_eq!(hooks.refused.load(Ordering::SeqCst), 1);
    assert_eq!(second.requests("/m.bin"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pieces_of_one_object_fill_shared_files_in_place_and_the_marker_goes_last() {
    let server = TestServer::start();
    let dir = scratch_dir("in-place");
    let object = pattern(12 << 20, 30);
    let url = server.add(
        "/object",
        object.clone(),
        Behavior {
            drip: Some(24 << 20),
            ..Behavior::default()
        },
    );
    let a_size = (8 << 20) + (1 << 20);
    let b_size = 3 << 20;
    let marker = dir.join("a.bin.pending");
    let a = crate::files::SharedFile::new("a.bin".into(), dir.join("a.bin"), a_size, 2, Some(marker.clone()), None);
    let b = crate::files::SharedFile::new("b.bin".into(), dir.join("b.bin"), b_size, 1, None, None);
    let piece = |file: &Arc<crate::files::SharedFile>, id: &str, src: usize, len: usize, dst: u64| FileSpec {
        id: id.to_string(),
        path: file.path.clone(),
        size: len as u64,
        md5: md5_hex(&object[src..src + len]),
        url: url.clone(),
        source_offset: src as u64,
        target: crate::files::Target::InPlace(Arc::clone(file), dst),
        ..FileSpec::default()
    };
    let specs = vec![
        piece(&a, "a.bin", 4 << 20, 8 << 20, 0),
        piece(&a, "a.bin", 0, 1 << 20, 8 << 20),
        piece(&b, "b.bin", 1 << 20, 3 << 20, 0),
    ];
    let hooks = Hooks::new(&dir);
    hooks.tracker.begin(
        "downloading",
        (a_size + b_size) as f64,
        2,
        Some([("a.bin".to_string(), a_size as f64), ("b.bin".to_string(), b_size as f64)].into()),
    );
    let ledger = Ledger::new(None);
    let hooks_dyn: Arc<dyn JobHooks> = Arc::clone(&hooks) as Arc<dyn JobHooks>;
    let report = download_files(&runtime(4), hooks_dyn, &ledger, specs).await.unwrap();

    let mut want_a = object[4 << 20..12 << 20].to_vec();
    want_a.extend_from_slice(&object[..1 << 20]);
    assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), want_a);
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), object[1 << 20..4 << 20]);
    assert!(!marker.exists(), "the marker goes once every range is flushed");
    assert!(report.splits >= 1, "an in-place range splits like a part: {report:?}");
    assert_eq!(server.requests("/object"), server.ranged_requests("/object"), "every request asks for a range");
    assert_eq!(hooks.tracker.calculate_metrics()["percentage"].as_f64(), Some(100.0));
    assert_eq!(leftovers(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_reader_reopens_where_a_cut_left_off_and_refuses_a_server_without_ranges() {
    let server = TestServer::start();
    let dir = scratch_dir("ranges");
    let object = pattern(3 << 20, 40);
    let url = server.add(
        "/r",
        object.clone(),
        Behavior {
            cut_after: Some((1, 1 << 20)),
            ..Behavior::default()
        },
    );
    let rt = runtime(4);
    let hooks = Hooks::new(&dir);
    let reader = crate::RangeReader::new(Arc::clone(&rt));

    let mut body = reader.open(hooks.as_ref(), &url, 100, 3 << 20).await.unwrap();
    let mut got = Vec::new();
    let error = loop {
        match body.next().await {
            Ok(Some(piece)) => got.extend_from_slice(&piece),
            Ok(None) => panic!("the first stream is cut"),
            Err(e) => break e,
        }
    };
    assert!(error.starts_with("Stream error"), "{error}");
    let resumed_at = 100 + got.len() as u64;
    assert_eq!(body.left(), (3 << 20) - resumed_at);
    let mut rest = reader.open(hooks.as_ref(), &url, resumed_at, 3 << 20).await.unwrap();
    while let Some(piece) = rest.next().await.unwrap() {
        got.extend_from_slice(&piece);
    }
    assert_eq!(got, object[100..]);

    let whole = reader.fetch(hooks.as_ref(), &url, 0, 1000).await.unwrap();
    assert_eq!(whole, object[..1000]);

    let plain = server.add("/p", object.clone(), Behavior { ignore_range: true, ..Behavior::default() });
    let refused = reader.open(hooks.as_ref(), &plain, 10, 20).await.err().unwrap();
    assert_eq!(refused, crate::ranges::RANGE_IGNORED);
    assert_eq!(server.requests("/p"), 1, "a server without ranges is not asked again");
    let _ = std::fs::remove_dir_all(&dir);
}

fn mod_hooks(dir: &Path) -> Hooks {
    Arc::try_unwrap(Hooks::new(dir)).ok().unwrap()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_package_of_unknown_size_resumes_within_the_call_and_ignores_the_gaming_hold() {
    let _hold = crate::testing::hold_lock();
    let server = TestServer::start();
    let dir = scratch_dir("stream");
    let body = pattern(3 << 20, 50);
    let url = server.add(
        "/tools.zip",
        body.clone(),
        Behavior {
            cut_after: Some((1, 1 << 20)),
            ..Behavior::default()
        },
    );
    let hooks = mod_hooks(&dir);
    let dest = dir.join("tools.zip");
    let mut seen = 0u64;
    crate::GAMING_HOLD.set(true);
    let written = crate::download_stream(
        &runtime(2),
        &hooks,
        &url,
        &dest,
        &|_, _| None,
        &mut |done, _| seen = done,
    )
    .await;
    crate::GAMING_HOLD.set(false);

    assert_eq!(written.unwrap(), body.len() as u64);
    assert_eq!(seen, body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(server.requests("/tools.zip"), 2);
    assert_eq!(server.ranged_requests("/tools.zip"), 1, "the second request picks up where the cut left off");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_or_missing_package_stops_at_once() {
    let server = TestServer::start();
    let dir = scratch_dir("stream-refused");
    let body = pattern(2 << 20, 51);
    let big = server.add("/big.zip", body.clone(), Behavior::default());
    let gone = server.add("/gone.zip", body, Behavior { status: Some((usize::MAX, 404)), ..Behavior::default() });
    let hooks = mod_hooks(&dir);
    let rt = runtime(2);
    let limit = |received: u64, announced: u64| {
        (announced > 1 << 20 || received > 1 << 20).then(|| "too big for a mod".to_string())
    };
    let refused = crate::download_stream(&rt, &hooks, &big, &dir.join("big.zip"), &limit, &mut |_, _| {})
        .await
        .unwrap_err();
    assert_eq!(refused, "too big for a mod");
    assert_eq!(server.requests("/big.zip"), 1);
    let missing = crate::download_stream(&rt, &hooks, &gone, &dir.join("gone.zip"), &|_, _| None, &mut |_, _| {})
        .await
        .unwrap_err();
    assert!(missing.starts_with("HTTP Error: 404"), "{missing}");
    assert_eq!(server.requests("/gone.zip"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

struct RecordingSink {
    bodies: Mutex<Vec<(usize, Vec<u8>)>>,
    damaged_once: Mutex<HashSet<usize>>,
    always_damaged: Option<usize>,
    release: std::sync::atomic::AtomicBool,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(Vec::new()),
            damaged_once: Mutex::new(HashSet::new()),
            always_damaged: None,
            release: std::sync::atomic::AtomicBool::new(true),
        })
    }
}

impl crate::ChunkSink for RecordingSink {
    fn accept(&self, index: usize, body: bytes::Bytes) -> Result<(), crate::ChunkError> {
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.always_damaged == Some(index) {
            return Err(crate::ChunkError::Corrupt("checksum mismatch".to_string()));
        }
        if self.damaged_once.lock().remove(&index) {
            return Err(crate::ChunkError::Corrupt("not valid zstd".to_string()));
        }
        self.bodies.lock().push((index, body.to_vec()));
        Ok(())
    }
}

fn chunk_specs(server: &TestServer, count: u8, behavior: Behavior) -> (Vec<crate::ChunkSpec>, Vec<Vec<u8>>) {
    let mut specs = Vec::new();
    let mut bodies = Vec::new();
    for i in 0..count {
        let body = pattern(64 << 10, i.wrapping_add(100));
        let url = server.add(&format!("/chunk/{i}"), body.clone(), behavior.clone());
        specs.push(crate::ChunkSpec {
            id: format!("chunk-{i}"),
            url,
            size_hint: body.len() as u64,
        });
        bodies.push(body);
    }
    (specs, bodies)
}

async fn run_chunks(
    rt: &Arc<TransferRuntime>,
    hooks: &Arc<Hooks>,
    specs: Vec<crate::ChunkSpec>,
    sink: Arc<RecordingSink>,
) -> Result<crate::ChunkReport, String> {
    let ledger = Ledger::new(None);
    let hooks_dyn: Arc<dyn JobHooks> = Arc::clone(hooks) as Arc<dyn JobHooks>;
    tokio::time::timeout(
        Duration::from_secs(60),
        crate::fetch_chunks(rt, hooks_dyn, &ledger, specs, sink, 32 << 20),
    )
    .await
    .map_err(|_| "the chunk transfer hung".to_string())?
    .map(|(report, _sealed)| report)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_chunk_reaches_the_sink_once_and_a_damaged_one_is_fetched_again() {
    let server = TestServer::start();
    let dir = scratch_dir("chunks");
    let (specs, bodies) = chunk_specs(&server, 20, Behavior::default());
    let sink = Arc::new(RecordingSink {
        damaged_once: Mutex::new(HashSet::from([3usize, 7])),
        ..Arc::try_unwrap(RecordingSink::new()).ok().unwrap()
    });
    let hooks = Hooks::new(&dir);
    let report = run_chunks(&runtime(4), &hooks, specs, Arc::clone(&sink)).await.unwrap();

    let mut got = sink.bodies.lock().clone();
    got.sort_by_key(|(i, _)| *i);
    assert_eq!(got.len(), 20);
    for (i, body) in got {
        assert_eq!(body, bodies[i], "chunk {i}");
    }
    assert_eq!(report.refetched, 2);
    assert_eq!(server.requests("/chunk/3"), 2);
    assert_eq!(server.requests("/chunk/0"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunk_that_never_checks_out_fails_the_job() {
    let server = TestServer::start();
    let dir = scratch_dir("chunks-bad");
    let (specs, _) = chunk_specs(&server, 4, Behavior::default());
    let sink = Arc::new(RecordingSink {
        always_damaged: Some(2),
        ..Arc::try_unwrap(RecordingSink::new()).ok().unwrap()
    });
    let hooks = Hooks::new(&dir);
    let error = run_chunks(&runtime(2), &hooks, specs, sink).await.unwrap_err();
    assert!(error.contains("checksum mismatch"), "{error}");
    assert_eq!(server.requests("/chunk/2"), 4, "the first fetch and three more");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_keep_fetching_while_the_sink_is_busy() {
    let server = TestServer::start();
    let dir = scratch_dir("chunks-busy");
    let (specs, _) = chunk_specs(&server, 12, Behavior::default());
    let sink = RecordingSink::new();
    sink.release.store(false, Ordering::SeqCst);
    let hooks = Hooks::new(&dir);
    let job = {
        let rt = runtime(2);
        let hooks = Arc::clone(&hooks);
        let sink = Arc::clone(&sink);
        tokio::spawn(async move { run_chunks(&rt, &hooks, specs, sink).await })
    };
    let started = Instant::now();
    let fetched = |server: &TestServer| (0..12).map(|i| server.requests(&format!("/chunk/{i}"))).sum::<usize>();
    while fetched(&server) < 12 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let fetched_while_busy = fetched(&server);
    let written_while_busy = sink.bodies.lock().len();
    sink.release.store(true, Ordering::SeqCst);
    job.await.unwrap().unwrap();

    assert_eq!(fetched_while_busy, 12, "two connections fetched every chunk while none was written");
    assert_eq!(written_while_busy, 0);
    assert_eq!(sink.bodies.lock().len(), 12);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunk_a_mirror_refuses_comes_from_the_publisher() {
    let publisher = TestServer::start();
    let mirror = TestServer::start();
    let dir = scratch_dir("chunks-mirror");
    let (specs, bodies) = chunk_specs(&publisher, 3, Behavior::default());
    for i in 0..3 {
        mirror.add(
            &format!("/chunk/{i}"),
            bodies[i as usize].clone(),
            Behavior {
                status: Some((usize::MAX, 404)),
                ..Behavior::default()
            },
        );
    }
    let hooks = Arc::new(Hooks {
        mirrors: Some((publisher.base.clone(), mirror.base.clone())),
        ..Arc::try_unwrap(Hooks::new(&dir)).ok().unwrap()
    });
    hooks.moved.store(1, Ordering::SeqCst);
    let sink = RecordingSink::new();
    run_chunks(&runtime(2), &hooks, specs, Arc::clone(&sink)).await.unwrap();
    assert_eq!(sink.bodies.lock().len(), 3);
    for i in 0..3 {
        assert_eq!(mirror.requests(&format!("/chunk/{i}")), 1);
        assert_eq!(publisher.requests(&format!("/chunk/{i}")), 1);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_file_with_no_other_mirror_fails_at_once() {
    let server = TestServer::start();
    let dir = scratch_dir("refused");
    let body = pattern(1000, 16);
    let url = server.add(
        "/x.bin",
        body.clone(),
        Behavior {
            status: Some((usize::MAX, 403)),
            ..Behavior::default()
        },
    );
    let hooks = Hooks::new(&dir);
    let error = run(&runtime(2), &hooks, vec![spec(&dir, "x.bin", &url, &body, None)])
        .await
        .unwrap_err();
    assert!(error.starts_with("HTTP Error: 403"), "{error}");
    assert_eq!(server.requests("/x.bin"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
