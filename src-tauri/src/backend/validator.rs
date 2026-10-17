// ------------ File Validator and Install Records ------------
// Checks installed files against the expected size and hash using a few worker threads, reports progress while it goes,
// and hands back the files that are missing or wrong so they can be repaired. It also keeps the records an install
// leaves in its folder: the ownership marker, the install record (and the readers that carry an older Peebify's records
// over to it) and the version file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use peebify_transfer::part::discard_part;
use peebify_transfer::Sealed;
use serde_json::{json, Value};

use super::engine::{JobCore, JobKind};
use super::fs_util::manifest_key;
use super::game_profiles;
use super::kuro;
use super::manifest::{Entry, Hash, Manifest, Pack, Source};
use super::queue::{Phase, Update};

const LOGGED_INVALID_FILES: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCheck {
    Valid,
    Missing,
    SizeMismatch,
    HashMismatch,
    Unreadable,
}

impl FileCheck {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            FileCheck::Valid => "is valid",
            FileCheck::Missing => "is missing",
            FileCheck::SizeMismatch => "has the wrong size",
            FileCheck::HashMismatch => "fails the checksum",
            FileCheck::Unreadable => "could not be read",
        }
    }
}

pub struct FileValidator;

impl FileValidator {
    pub fn check(
        path: &Path,
        expected_size: u64,
        expected_md5: &str,
        cancelled: Option<&AtomicBool>,
        mut on_progress: impl FnMut(u64),
    ) -> Result<FileCheck, String> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileCheck::Missing),
            Err(e) => {
                log::debug!("validate: could not stat {}: {e}", path.display());
                return Ok(FileCheck::Unreadable);
            }
        };
        if meta.len() != expected_size {
            return Ok(FileCheck::SizeMismatch);
        }
        if expected_md5.is_empty() {
            return Ok(FileCheck::Valid);
        }

        match super::fs_util::checksum_file(
            path,
            expected_md5,
            &mut || cancelled.is_some_and(|flag| flag.load(Ordering::SeqCst)),
            &mut |n| on_progress(n),
        ) {
            Ok(actual) if super::fs_util::checksum_matches(expected_md5, &actual) => {
                Ok(FileCheck::Valid)
            }
            Ok(_) => Ok(FileCheck::HashMismatch),
            Err(e) if e == super::fs_util::CANCELLED_MSG => {
                Err("Validation cancelled".to_string())
            }
            Err(e) => {
                log::warn!("validate: {e}");
                Ok(FileCheck::Unreadable)
            }
        }
    }
}

pub(super) async fn check_files(
    job: &Arc<JobCore>,
    dir: &Path,
    files: Vec<Entry>,
    quick: bool,
    status: &str,
    after_fetch: bool,
) -> Result<Vec<(Entry, FileCheck)>, String> {
    let total = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    job.tracker.begin("validating", total_bytes as f64, total, None);
    log::info!(
        "{}: checking {total} files ({:.2} GB){}.",
        job.profile_id(),
        peebify_transfer::progress::gib(total_bytes as f64),
        if quick { " by size" } else { "" }
    );
    let started = Instant::now();
    let report: Arc<dyn Fn() + Send + Sync> = {
        let (job, status) = (Arc::clone(job), status.to_string());
        Arc::new(move || {
            if !job.tracker.should_update_ui() {
                return;
            }
            if after_fetch {
                job.send_progress(Update::new(Phase::Verifying).kind("verify"), &status, json!({}));
            } else {
                job.report_scan(&status);
            }
        })
    };
    let workers = match job.kind {
        JobKind::Repair => peebify_transfer::cores().min(8),
        _ => super::platform::validation_workers(),
    };
    let (dir, worker_job, worker_report) = (dir.to_path_buf(), Arc::clone(job), Arc::clone(&report));
    let flagged = tauri::async_runtime::spawn_blocking(move || {
        let (job, report) = (worker_job, worker_report);
        let cancelled = job.gate.flag();
        par_scan(&files, workers, "validation worker", Vec::new, |scratch, file| {
            job.block_while_paused();
            if cancelled.load(Ordering::SeqCst) {
                return Err("Validation cancelled".to_string());
            }
            let path = match super::fs_util::safe_join(&dir, &file.path) {
                Ok(path) => path,
                Err(e) => {
                    log::warn!("validate: skipping a file: {e}");
                    return Ok(None);
                }
            };
            let mut hashed = 0u64;
            let mut on_progress = |chunk: u64| {
                hashed += chunk;
                job.tracker.update_validation_progress(chunk as f64);
                report();
                job.block_while_paused();
            };
            let flagged = match &file.source {
                Source::Chunks { .. } => super::manifest::scan_chunks(&path, file, scratch, cancelled, &mut on_progress)?,
                Source::Segments { .. } => {
                    super::manifest::scan_segments(&path, file, quick, scratch, cancelled, &mut on_progress)?
                }
                _ => {
                    let expected = if quick { String::new() } else { file.hash.expected() };
                    let check = FileValidator::check(&path, file.size, &expected, Some(cancelled), &mut on_progress)?;
                    (check != FileCheck::Valid).then(|| (file.clone(), check))
                }
            };
            let unreported = file.size.saturating_sub(hashed);
            if unreported > 0 {
                job.tracker.update_validation_progress(unreported as f64);
            }
            report();
            Ok(flagged)
        })
    })
    .await
    .map_err(|e| format!("validation worker panicked: {e}"))??;

    job.tracker.force_next_update();
    report();
    let count = |kind: FileCheck| flagged.iter().filter(|(_, check)| *check == kind).count();
    log::info!(
        "{}: checked {total} files in {:.1}s, {} invalid ({} missing, {} wrong size, {} bad checksum, {} unreadable).",
        job.profile_id(),
        started.elapsed().as_secs_f64(),
        flagged.len(),
        count(FileCheck::Missing),
        count(FileCheck::SizeMismatch),
        count(FileCheck::HashMismatch),
        count(FileCheck::Unreadable),
    );
    for (file, check) in flagged.iter().take(LOGGED_INVALID_FILES) {
        log::info!("Invalid file: {} {}", file.path, check.describe());
    }
    if flagged.len() > LOGGED_INVALID_FILES {
        log::info!("{} more invalid files are not listed.", flagged.len() - LOGGED_INVALID_FILES);
    }
    Ok(flagged)
}

pub(super) fn par_scan<T, S, R>(
    items: &[T],
    workers: usize,
    label: &str,
    init: impl Fn() -> S + Sync,
    scan: impl Fn(&mut S, &T) -> Result<Option<R>, String> + Sync,
) -> Result<Vec<R>, String>
where
    T: Sync,
    R: Send,
{
    let workers = workers.min(items.len()).max(1);
    let cursor = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let fail = |e: String| {
        stop.store(true, Ordering::SeqCst);
        let mut slot = first_error.lock();
        if slot.is_none() {
            *slot = Some(e);
        }
    };
    let mut collected: Vec<(usize, R)> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut state = init();
                    let mut local: Vec<(usize, R)> = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        let index = cursor.fetch_add(1, Ordering::SeqCst);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        match scan(&mut state, item) {
                            Ok(Some(found)) => local.push((index, found)),
                            Ok(None) => {}
                            Err(e) => {
                                fail(e);
                                break;
                            }
                        }
                    }
                    local
                })
            })
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(local) => collected.extend(local),
                Err(_) => fail(format!("{label} panicked")),
            }
        }
    });
    if let Some(e) = first_error.into_inner() {
        return Err(e);
    }
    collected.sort_by_key(|(index, _)| *index);
    Ok(collected.into_iter().map(|(_, found)| found).collect())
}

// ------------ Install Folder Bookkeeping ------------
// Marks the install folder as ours, sweeps leftover part files, and writes the game's own config files.
pub(super) const GAME_CONFIG_FILE: &str = "launcherDownloadConfig.json";
pub(super) const INSTALL_MARKER_FILE: &str = ".peebify-install";

fn dir_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

pub(super) fn prepare_install_dir(install_path: &Path, game_id: &str) -> Result<(), String> {
    let owned_dir = !install_path.exists() || dir_is_empty(install_path);

    let profile = game_profiles::profile(game_id);
    if let Some(too_deep) = game_profiles::path_budget_error(install_path, profile) {
        if owned_dir {
            return Err(too_deep);
        }
        log::warn!(
            "{} is installed at {} ({} characters), which is deeper than the {} this game can work from. {too_deep}",
            game_profiles::display_name(profile),
            install_path.display(),
            game_profiles::root_len(install_path),
            game_profiles::max_install_root_len(profile),
        );
    }

    let not_writable = |e: &std::io::Error| {
        if super::fs_util::is_access_denied(e) {
            log::warn!("{} is not writable: {e}", install_path.display());
            super::fs_util::not_writable_message(install_path)
        } else {
            super::fs_util::fmt_io(
                &format!("Could not prepare {}", install_path.display()),
                e,
            )
        }
    };
    super::fs_util::probe_writable(install_path).map_err(|e| not_writable(&e))?;

    let marker = install_path.join(INSTALL_MARKER_FILE);
    if marker.exists() {
        let owner = read_install_marker(install_path)
            .and_then(|m| m.get("gameId").and_then(Value::as_str).map(str::to_string));
        if owner.as_deref() == Some(game_id) {
            return Ok(());
        }
        log::warn!(
            "Replacing the install marker in {} (it was for {}).",
            install_path.display(),
            owner.as_deref().unwrap_or("an unknown game")
        );
    }
    let payload = json!({
        "gameId": game_id,
        "ownedDir": owned_dir,
        "startedAt": chrono::Utc::now().to_rfc3339(),
    });
    let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    match std::fs::write(&marker, text) {
        Ok(()) => {}
        Err(e) if super::fs_util::is_access_denied(&e) => return Err(not_writable(&e)),
        Err(e) => log::warn!("Could not write install marker {}: {e}", marker.display()),
    }
    Ok(())
}

pub(super) fn clear_install_marker(install_path: &Path) {
    let marker = install_path.join(INSTALL_MARKER_FILE);
    if let Err(e) = std::fs::remove_file(&marker) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("Could not remove install marker {}: {e}", marker.display());
        }
    }
}

fn read_install_marker(install_path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(install_path.join(INSTALL_MARKER_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub(super) fn install_marker_owns_dir(install_path: &Path) -> Option<bool> {
    let marker = read_install_marker(install_path)?;
    Some(
        marker
            .get("ownedDir")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    )
}

fn list_paths(dir: &Path) -> Vec<PathBuf> {
    super::fs_util::list_files(dir).into_iter().map(|(path, _)| path).collect()
}

fn marker_owns_dir_for(marker: &Value, game_id: &str) -> bool {
    marker.get("ownedDir").and_then(Value::as_bool).unwrap_or(false)
        && marker.get("gameId").and_then(Value::as_str) == Some(game_id)
}

pub(super) const PART_ARTIFACT_SUFFIXES: &[&str] = &[
    ".part",
    ".part.segments.json",
    ".part.segments.json.tmp",
    ".part.id.json",
    ".part.id.json.tmp",
    ".peebify_tmp",
    ".peebify-tmp",
    ".peebify-part",
];

fn is_part_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    PART_ARTIFACT_SUFFIXES
        .iter()
        .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
}

pub(super) fn sweep_orphaned_parts(
    install_path: &Path,
    min_age: std::time::Duration,
) -> (u64, u64) {
    if !install_path.is_dir() {
        return (0, 0);
    }

    let files = list_paths(install_path);

    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    for file in files.iter().filter(|f| is_part_artifact(f)) {
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= min_age);
        if !stale {
            continue;
        }
        let size = meta.len();
        match std::fs::remove_file(file) {
            Ok(()) => {
                removed_files += 1;
                removed_bytes += size;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Part sweep: could not remove {}: {e}", file.display()),
        }
    }

    if removed_files > 0 {
        log::info!(
            "Part sweep: removed {removed_files} abandoned part file(s), {:.2} GB from {}.",
            removed_bytes as f64 / 1e9,
            install_path.display()
        );
    }
    (removed_files, removed_bytes)
}

pub(super) fn discard_partial_install_for(install_path: &Path, game_id: &str) -> (u64, u64) {
    let Some(marker) = read_install_marker(install_path) else {
        log::info!(
            "Discard: no install marker at {}; leaving the folder untouched.",
            install_path.display()
        );
        return (0, 0);
    };
    let owned_dir = marker_owns_dir_for(&marker, game_id);

    let mut files = list_paths(install_path);
    if !owned_dir {
        files.retain(|f| {
            is_part_artifact(f) || f.file_name().is_some_and(|n| n == INSTALL_MARKER_FILE)
        });
    }

    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    for file in &files {
        let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(file) {
            Ok(()) => {
                removed_files += 1;
                removed_bytes += size;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Discard: could not remove {}: {e}", file.display()),
        }
    }

    if owned_dir {
        if let Err(e) = std::fs::remove_dir_all(install_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Discard: could not remove {}: {e}", install_path.display());
            }
        }
    }

    log::info!(
        "Discard: removed {removed_files} file(s), {:.2} GB from {} (owned={owned_dir}).",
        removed_bytes as f64 / 1e9,
        install_path.display()
    );
    (removed_files, removed_bytes)
}

pub(super) fn read_index_file(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    data.get("resource")
        .or_else(|| data.get("resources"))
        .and_then(|v| v.as_array())
        .map(|list| list.iter().map(|v| Entry::from_json(v, "")).collect())
        .unwrap_or_default()
}

pub(super) fn write_local_index(dir: &Path, files: &[Entry]) {
    if files.is_empty() {
        return;
    }
    let listed: Vec<Value> = files.iter().map(Entry::to_json).collect();
    let text = serde_json::to_string_pretty(&json!({ "resource": listed })).unwrap_or_default();
    match super::fs_util::write_atomic(&dir.join(LOCAL_INDEX_FILE), text.as_bytes()) {
        Ok(()) => log::info!("Local game resources index saved for quick repair."),
        Err(e) => log::error!("Failed to save local resources index: {e}"),
    }
}

// ------------ Install Record ------------
// What Peebify installed: the version, an unfinished update's target, the groups it holds, the archive its members come
// from, and each file's size, checksum and modified time. A file untouched since it was recorded skips its checksum
// before the next update, and a file the next build drops is removed. Installs from before this record are read from
// the scan record and the local index they kept, from the Sophon record, from the split zip record Endfield and the
// Girls' Frontline 2 client kept, or from the Brown Dust II file record.
pub(super) const INSTALL_RECORD_FILE: &str = "peebify-install.json";
const INSTALL_RECORD_FORMAT: &str = "peebify-install-v1";
const LOCAL_INDEX_FILE: &str = "LocalGameResources.json";
const SOPHON_RECORD_FILE: &str = "PeebifySophonManifest.json";
const SPLIT_RECORD_FILE: &str = "PeebifyManifest.json";
const BD2_RECORD_FILE: &str = "peebify_bd2_files.json";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Record {
    format: String,
    pub(super) version: String,
    #[serde(default)]
    pending: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) packs: Vec<Pack>,
    files: Vec<RecordEntry>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct RecordEntry {
    #[serde(alias = "dest")]
    path: String,
    size: u64,
    #[serde(alias = "md5")]
    hash: String,
    #[serde(default)]
    mtime: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct Triage {
    pub(super) trusted: HashSet<String>,
    pub(super) stale: HashSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Check,
    Trusted,
    Stale,
}

fn verdict(recorded: Option<&RecordEntry>, size: u64, hash: &str, on_disk: Option<(u64, u64)>) -> Verdict {
    let (Some(recorded), Some((len, mtime))) = (recorded, on_disk) else {
        return Verdict::Check;
    };
    let untouched = recorded.size == size && len == size && recorded.mtime == Some(mtime);
    if hash.is_empty() || recorded.hash.is_empty() || !untouched {
        Verdict::Check
    } else if recorded.hash.eq_ignore_ascii_case(hash) {
        Verdict::Trusted
    } else {
        Verdict::Stale
    }
}

impl Record {
    pub(super) fn load(dir: &Path) -> Self {
        let read = std::fs::read_to_string(dir.join(INSTALL_RECORD_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok());
        match read {
            Some(record) if record.format == INSTALL_RECORD_FORMAT => record,
            _ => Self::migrated(dir),
        }
    }

    fn migrated(dir: &Path) -> Self {
        if let Some(record) = read_sophon_record(dir).or_else(|| read_bd2_record(dir)) {
            return record;
        }
        let mut files = read_scan_record(dir);
        for entry in read_index_file(&dir.join(LOCAL_INDEX_FILE)) {
            files.entry(manifest_key(&entry.path)).or_insert_with(|| RecordEntry {
                path: entry.path.to_string(),
                size: entry.size,
                hash: entry.hash.expected(),
                mtime: None,
            });
        }
        let mut record = read_split_record(dir).unwrap_or_default();
        for file in std::mem::take(&mut record.files) {
            files.insert(manifest_key(&file.path), file);
        }
        record.files = files.into_values().collect();
        record
    }

    pub(super) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub(super) fn version_at(dir: &Path) -> Option<String> {
        Some(Self::load(dir).version).filter(|version| !version.is_empty())
    }

    pub(super) fn of(dir: &Path, version: &str, files: &[Entry]) -> Self {
        let packs = files
            .iter()
            .find_map(|file| match &file.source {
                Source::Member { packs, .. } => Some(packs.to_vec()),
                _ => None,
            })
            .unwrap_or_default();
        let files = files
            .iter()
            .filter_map(|file| {
                let (len, mtime) = on_disk_stamp(dir, &file.path)?;
                (len == file.size).then(|| RecordEntry {
                    path: file.path.to_string(),
                    size: file.size,
                    hash: file.hash.expected(),
                    mtime: Some(mtime),
                })
            })
            .collect();
        Self {
            format: INSTALL_RECORD_FORMAT.to_string(),
            version: version.trim().to_string(),
            packs,
            files,
            ..Self::default()
        }
    }

    pub(super) fn save(&mut self, dir: &Path) -> Result<(), String> {
        self.format = INSTALL_RECORD_FORMAT.to_string();
        let text = serde_json::to_string(self).map_err(|e| e.to_string())?;
        super::fs_util::write_atomic(&dir.join(INSTALL_RECORD_FILE), text.as_bytes())?;
        forget_older_records(dir);
        Ok(())
    }

    pub(super) fn unchanged(&self, files: &[Entry]) -> HashSet<String> {
        let recorded: HashMap<String, &RecordEntry> = self.files.iter().map(|f| (manifest_key(&f.path), f)).collect();
        files
            .iter()
            .filter(|file| {
                let hash = file.hash.expected();
                let key = manifest_key(&file.path);
                !hash.is_empty()
                    && recorded.get(&key).is_some_and(|r| r.size == file.size && r.hash.eq_ignore_ascii_case(&hash))
            })
            .map(|file| manifest_key(&file.path))
            .collect()
    }

    pub(super) fn vouches(&self, dir: &Path, files: &[Entry]) -> HashSet<String> {
        let unchanged = self.unchanged(files);
        files
            .iter()
            .map(|file| (manifest_key(&file.path), file))
            .filter(|(key, file)| {
                unchanged.contains(key) && on_disk_stamp(dir, &file.path).is_some_and(|(len, _)| len == file.size)
            })
            .map(|(key, _)| key)
            .collect()
    }

    pub(super) fn hold(dir: &Path, version: &str) {
        let mut record = Self::load(dir);
        if record.files.is_empty() {
            return;
        }
        record.pending = Some(version.to_string());
        if let Err(e) = record.save(dir) {
            log::warn!("Could not update {INSTALL_RECORD_FILE}: {e}");
        }
    }

    pub(super) fn entries(&self) -> Vec<Entry> {
        self.files
            .iter()
            .map(|f| Entry::new(f.path.as_str(), f.size, Hash::parse(&f.hash), ""))
            .collect()
    }

    pub(super) fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        let mut triage = Triage::default();
        if self.pending.is_some() || self.files.is_empty() {
            return triage;
        }
        let recorded: HashMap<String, &RecordEntry> =
            self.files.iter().map(|f| (manifest_key(&f.path), f)).collect();
        for file in files {
            let key = manifest_key(&file.path);
            let on_disk = on_disk_stamp(dir, &file.path);
            match verdict(recorded.get(&key).copied(), file.size, &file.hash.expected(), on_disk) {
                Verdict::Trusted => triage.trusted.insert(key),
                Verdict::Stale => triage.stale.insert(key),
                Verdict::Check => false,
            };
        }
        triage
    }
}

pub(super) fn prune_orphans(dir: &Path, record: &Record, next: &[Entry]) -> usize {
    if record.files.is_empty() || next.is_empty() {
        return 0;
    }
    let keep: HashSet<String> = next.iter().map(|e| manifest_key(&e.path)).collect();
    let mut removed = 0usize;
    for file in record.files.iter().filter(|f| !keep.contains(&manifest_key(&f.path))) {
        let Ok(path) = super::fs_util::safe_join(dir, &file.path) else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() != file.size {
            log::info!("Keeping {}: its size no longer matches the recorded install.", file.path);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                log::info!("Removed {}: it is no longer part of the game's file list.", file.path);
                super::fs_util::remove_empty_parents(&path, dir);
            }
            Err(e) => log::warn!("Could not remove {}: {e}", file.path),
        }
    }
    removed
}

pub(super) async fn hold_record(dir: &Path, version: &str) {
    let (dir, version) = (dir.to_path_buf(), version.to_string());
    if let Err(e) = tauri::async_runtime::spawn_blocking(move || Record::hold(&dir, &version)).await {
        log::warn!("Install record task panicked: {e}");
    }
}

pub(super) async fn prune_dropped(dir: &Path, next: Vec<Entry>) {
    let dir = dir.to_path_buf();
    let removed = tauri::async_runtime::spawn_blocking(move || prune_orphans(&dir, &Record::load(&dir), &next))
        .await
        .unwrap_or(0);
    if removed > 0 {
        log::info!("Removed {removed} file(s) this build no longer ships.");
    }
}

pub(super) async fn recorded(dir: &Path) -> Record {
    let dir = dir.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || Record::load(&dir)).await.unwrap_or_default()
}

pub(super) fn forget_record(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(INSTALL_RECORD_FILE));
    forget_older_records(dir);
}

fn forget_older_records(dir: &Path) {
    clear_scan_record(dir);
    for older in [SOPHON_RECORD_FILE, SPLIT_RECORD_FILE, BD2_RECORD_FILE] {
        let _ = std::fs::remove_file(dir.join(older));
    }
}

pub(super) async fn record_install(dir: &Path, manifest: &Manifest) {
    let (dir, version, groups, files) =
        (dir.to_path_buf(), manifest.version.clone(), manifest.groups.clone(), manifest.installed());
    let saved = tauri::async_runtime::spawn_blocking(move || Record { groups, ..Record::of(&dir, &version, &files) }.save(&dir))
        .await
        .map_err(|e| format!("install record task panicked: {e}"))
        .and_then(|saved| saved);
    match saved {
        Ok(()) => log::info!("Install record saved for quick repair and the next update."),
        Err(e) => log::warn!("Could not save {INSTALL_RECORD_FILE}: {e}"),
    }
}

// ------------ Scan Record ------------
// What an older Peebify kept of the files it checked, which the install record's migration still reads.
pub(super) const SCAN_RECORD_FILE: &str = "peebify-scan-record.json";
const SCAN_RECORD_FORMAT: &str = "peebify-scan-record-v1";

#[derive(Debug, serde::Deserialize)]
struct ScanRecord {
    format: String,
    files: Vec<RecordEntry>,
}

fn modified_nanos(meta: &std::fs::Metadata) -> Option<u64> {
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(since.as_nanos()).ok()
}

fn on_disk_stamp(install_path: &Path, dest: &str) -> Option<(u64, u64)> {
    let path = super::fs_util::safe_join(install_path, dest).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((meta.len(), modified_nanos(&meta)?))
}

pub(super) fn clear_scan_record(install_path: &Path) {
    match std::fs::remove_file(install_path.join(SCAN_RECORD_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("Could not remove {SCAN_RECORD_FILE}: {e}"),
    }
}

fn read_scan_record(install_path: &Path) -> HashMap<String, RecordEntry> {
    let Ok(text) = std::fs::read_to_string(install_path.join(SCAN_RECORD_FILE)) else {
        return HashMap::new();
    };
    match serde_json::from_str::<ScanRecord>(&text) {
        Ok(record) if record.format == SCAN_RECORD_FORMAT => record
            .files
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect(),
        _ => HashMap::new(),
    }
}

fn read_sophon_record(dir: &Path) -> Option<Record> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Applied {
        format: String,
        tag: String,
        categories: Vec<Category>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Category {
        matching_field: String,
        files: Vec<RecordEntry>,
    }
    let applied: Applied = serde_json::from_str(&std::fs::read_to_string(dir.join(SOPHON_RECORD_FILE)).ok()?).ok()?;
    (applied.format == "sophon-v1").then(|| Record {
        version: applied.tag.trim().to_string(),
        groups: applied.categories.iter().map(|c| c.matching_field.clone()).collect(),
        files: applied.categories.into_iter().flat_map(|c| c.files).collect(),
        ..Record::default()
    })
}

fn read_split_record(dir: &Path) -> Option<Record> {
    #[derive(serde::Deserialize)]
    struct Old {
        format: String,
        version: String,
        packs: Vec<Pack>,
        files: Vec<OldFile>,
    }
    #[derive(serde::Deserialize)]
    struct OldFile {
        path: String,
        size: u64,
        crc32: u32,
    }
    let old: Old = serde_json::from_str(&std::fs::read_to_string(dir.join(SPLIT_RECORD_FILE)).ok()?).ok()?;
    (old.format == "hypergryph-split-zip-v1").then(|| Record {
        version: old.version.trim().to_string(),
        packs: old.packs,
        files: (old.files.into_iter())
            .map(|f| RecordEntry { path: f.path, size: f.size, hash: Hash::Crc32(f.crc32).expected(), mtime: None })
            .collect(),
        ..Record::default()
    })
}

fn read_bd2_record(dir: &Path) -> Option<Record> {
    #[derive(serde::Deserialize)]
    struct Old {
        version: String,
        files: Vec<OldFile>,
    }
    #[derive(serde::Deserialize)]
    struct OldFile {
        path: String,
        size: u64,
        crc: String,
    }
    let old: Old = serde_json::from_str(&std::fs::read_to_string(dir.join(BD2_RECORD_FILE)).ok()?).ok()?;
    Some(Record {
        version: old.version.trim().to_string(),
        files: (old.files.into_iter())
            .filter(|f| !f.path.is_empty())
            .map(|f| RecordEntry {
                hash: Hash::Crc32(u32::from_str_radix(&f.crc, 16).unwrap_or(0)).expected(),
                path: f.path,
                size: f.size,
                mtime: None,
            })
            .collect(),
        ..Record::default()
    })
}

// ------------ Version File ------------
// launcherDownloadConfig.json, which the game's official launcher reads, names the installed version.
pub fn update_game_config_file(
    sealed: &Sealed,
    install_path: &Path,
    version: &str,
) -> Result<(), String> {
    write_game_config(sealed, install_path, version, None, None)
}

pub(super) fn write_game_config(
    _sealed: &Sealed,
    install_path: &Path,
    version: &str,
    app_id: Option<&str>,
    bundle: Option<&kuro::Bundle>,
) -> Result<(), String> {
    let version = version.trim();
    let bundles = match bundle {
        Some(bundle) => Some(json!({
            bundle.name.clone(): { "version": version, "state": "", "resourcePacks": bundle.packs }
        })),
        None => recorded_bundles(install_path, version),
    };
    let mut payload = match bundles {
        Some(bundles) => json!({ "version": version, "state": "", "bundles": bundles }),
        None => json!({ "version": version, "reUseVersion": "", "state": "" }),
    };
    if let Some(id) = app_id {
        payload["appId"] = json!(id);
    }
    let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    super::fs_util::write_atomic(&install_path.join(GAME_CONFIG_FILE), text.as_bytes())?;
    if let Some(bundle) = bundle {
        if let Err(e) = kuro::write_pack_records(install_path, &bundle.packs, version) {
            log::warn!("Could not record the installed resource packs: {e}");
        }
    }
    Ok(())
}

fn recorded_bundles(install_path: &Path, version: &str) -> Option<Value> {
    let text = std::fs::read_to_string(install_path.join(GAME_CONFIG_FILE)).ok()?;
    let mut bundles = serde_json::from_str::<Value>(&text).ok()?["bundles"].take();
    for bundle in bundles.as_object_mut()?.values_mut() {
        bundle["version"] = json!(version);
    }
    Some(bundles)
}

pub(super) fn undersized_files(install_path: &Path, listed: Vec<(String, u64)>) -> HashSet<String> {
    listed
        .into_iter()
        .filter(|(dest, size)| {
            crate::backend::fs_util::safe_join(install_path, dest)
                .is_ok_and(|path| FileValidator::check(&path, *size, "", None, |_| {}) != Ok(FileCheck::Valid))
        })
        .map(|(dest, _)| dest)
        .collect()
}

pub(super) fn remove_part_files(install_path: &Path, dests: &[String]) {
    for dest in dests {
        if let Ok(path) = crate::backend::fs_util::safe_join(install_path, dest) {
            discard_part(&path);
        }
    }
}
// ------------ Tests ------------
// File checks, the install marker, the install record with its migration and pruning, scan records and part sweeps.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;
    use std::time::Duration;

    fn res(dest: &str, size: u64) -> Entry {
        Entry::new(dest, size, Hash::parse("00000000000000000000000000000000"), "")
    }

    fn recorded(files: &[Entry]) -> Record {
        Record {
            files: files
                .iter()
                .map(|f| RecordEntry { path: f.path.to_string(), size: f.size, hash: f.hash.expected(), mtime: None })
                .collect(),
            ..Record::default()
        }
    }

    const HELLO_MD5: &str = "5d41402abc4b2a76b9719d911017c592";

    fn check(path: &Path, size: u64, md5: &str) -> FileCheck {
        FileValidator::check(path, size, md5, None, |_| {}).unwrap()
    }

    #[test]
    fn each_failure_reports_its_own_reason() {
        let dir = TempDir::new("reasons");
        let file = dir.0.join("hello.bin");
        std::fs::write(&file, b"hello").unwrap();

        assert_eq!(check(&dir.0.join("absent.bin"), 5, HELLO_MD5), FileCheck::Missing);
        assert_eq!(check(&file, 6, HELLO_MD5), FileCheck::SizeMismatch);
        assert_eq!(
            check(&file, 5, "00000000000000000000000000000000"),
            FileCheck::HashMismatch
        );
        assert_eq!(check(&file, 5, &HELLO_MD5.to_uppercase()), FileCheck::Valid);
    }

    #[test]
    fn a_crc32_checksum_is_checked_like_any_other() {
        let dir = TempDir::new("crc32");
        let file = dir.write("hello.bin", b"hello");
        let crc = crc32fast::hash(b"hello");
        assert_eq!(check(&file, 5, &format!("crc32:{crc}")), FileCheck::Valid);
        assert_eq!(check(&file, 5, &format!("crc32:{}", crc ^ 1)), FileCheck::HashMismatch);
    }

    #[test]
    fn a_resource_without_a_checksum_is_checked_by_size() {
        let dir = TempDir::new("no-md5");
        let file = dir.0.join("client.7z");
        std::fs::write(&file, b"hello").unwrap();

        assert_eq!(check(&file, 5, ""), FileCheck::Valid);
        assert_eq!(check(&file, 6, ""), FileCheck::SizeMismatch);
        assert_eq!(check(&dir.0.join("absent.7z"), 5, ""), FileCheck::Missing);
    }

    #[test]
    fn a_cancelled_hash_is_an_error_not_a_verdict() {
        let dir = TempDir::new("cancel");
        let file = dir.0.join("hello.bin");
        std::fs::write(&file, b"hello").unwrap();
        let cancelled = AtomicBool::new(true);

        let result = FileValidator::check(&file, 5, HELLO_MD5, Some(&cancelled), |_| {});
        assert_eq!(result, Err("Validation cancelled".to_string()));
    }

    #[test]
    fn discarding_in_a_foreign_folder_removes_part_sidecars_and_keeps_game_files() {
        let dir = TempDir::new("discard-foreign");
        dir.write(INSTALL_MARKER_FILE, br#"{"ownedDir": false}"#);
        let game_file = dir.write("Game/data.pak", b"live");
        let part = dir.write("Game/new.pak.part", b"partial");
        let sidecar = dir.write("Game/new.pak.part.segments.json", b"{}");

        let (removed, _) = discard_partial_install_for(dir.path(), "wuwa");

        assert_eq!(removed, 3);
        assert!(game_file.exists());
        assert!(!part.exists());
        assert!(!sidecar.exists());
        assert!(!dir.path().join(INSTALL_MARKER_FILE).exists());
    }

    #[test]
    fn another_games_owned_marker_does_not_let_a_discard_remove_the_folder() {
        let dir = TempDir::new("discard-other-game");
        dir.write(INSTALL_MARKER_FILE, br#"{"gameId": "hsr", "ownedDir": true}"#);
        let user_file = dir.write("ReShade/preset.ini", b"mine");
        let part = dir.write("Game/new.pak.part", b"partial");

        discard_partial_install_for(dir.path(), "wuwa");

        assert!(user_file.exists());
        assert!(!part.exists());
    }

    #[test]
    fn the_games_own_marker_lets_a_discard_remove_the_folder() {
        let dir = TempDir::new("discard-own-game");
        dir.write(INSTALL_MARKER_FILE, br#"{"gameId": "wuwa", "ownedDir": true}"#);
        dir.write("Game/data.pak", b"partial");

        discard_partial_install_for(dir.path(), "wuwa");

        assert!(!dir.path().exists());
    }

    #[test]
    fn preparing_over_another_games_marker_takes_it_over_without_owning_the_folder() {
        let dir = TempDir::new("prepare-other-game");
        dir.write(INSTALL_MARKER_FILE, br#"{"gameId": "hsr", "ownedDir": true}"#);

        prepare_install_dir(dir.path(), "wuwa").unwrap();

        let marker = read_install_marker(dir.path()).unwrap();
        assert_eq!(marker["gameId"], "wuwa");
        assert_eq!(marker["ownedDir"], false);
    }

    #[test]
    fn a_chunk_dropped_from_the_index_is_removed() {
        let dir = TempDir::new("dropped");
        let kept = dir.write("Client/Content/Paks/pakchunk0.pak", b"data");
        let dropped = dir.write("Client/Content/Paks/pakchunk73.pak", b"data");
        let record = recorded(&[
            res("Client/Content/Paks/pakchunk0.pak", 4),
            res("Client/Content/Paks/pakchunk73.pak", 4),
        ]);

        let removed = prune_orphans(dir.path(), &record, &[res("Client/Content/Paks/pakchunk0.pak", 4)]);

        assert_eq!(removed, 1);
        assert!(!dropped.exists());
        assert!(!dir.path().join("Client/Content/Paks/pakchunk73.pak").exists());
        assert!(kept.is_file());
    }

    #[test]
    fn a_file_we_never_recorded_is_never_touched() {
        let dir = TempDir::new("unrecorded");
        let stranger = dir.write("Client/Content/Paks/pakchunk9_old.pak", b"data");
        let locale = dir.write("Client/Binaries/Win64/locales/en-US.pak", b"data");
        dir.write("Client/Content/Paks/pakchunk0.pak", b"data");
        let record = recorded(&[res("Client/Content/Paks/pakchunk0.pak", 4)]);

        let removed = prune_orphans(dir.path(), &record, &[res("Client/Content/Paks/pakchunk0.pak", 4)]);

        assert_eq!(removed, 0);
        assert!(stranger.is_file(), "an unrecorded pak was deleted");
        assert!(locale.is_file(), "a WebView locale pak was deleted");
    }

    #[test]
    fn a_file_whose_size_changed_since_we_wrote_it_is_kept() {
        let dir = TempDir::new("resized");
        let touched = dir.write("Client/Content/Paks/pakchunk73.pak", b"grown-since-install");
        let record = recorded(&[res("Client/Content/Paks/pakchunk73.pak", 4)]);

        let removed = prune_orphans(dir.path(), &record, &[res("Client/Content/Paks/pakchunk0.pak", 4)]);

        assert_eq!(removed, 0);
        assert!(touched.is_file());
    }

    #[test]
    fn an_empty_list_or_no_record_prunes_nothing() {
        let dir = TempDir::new("empty-next");
        let p = dir.write("Client/Content/Paks/pakchunk0.pak", b"data");
        let record = recorded(&[res("Client/Content/Paks/pakchunk0.pak", 4)]);

        assert_eq!(prune_orphans(dir.path(), &record, &[]), 0);
        assert_eq!(prune_orphans(dir.path(), &Record::default(), &[res("other.pak", 4)]), 0);
        assert!(p.is_file(), "an empty file list must not wipe the install");
    }

    #[test]
    fn a_file_renamed_only_by_case_or_spelling_is_kept() {
        let dir = TempDir::new("case-rename");
        let live = dir.write("Client/Content/Paks/Foo.pak", b"data");
        let dropped = dir.write("Client/Content/Paks/pakchunk73.pak", b"data");
        let record = recorded(&[
            res(r"Client\Content\Paks\Foo.pak", 4),
            res("Client/Content/Paks/pakchunk73.pak", 4),
        ]);

        let removed = prune_orphans(dir.path(), &record, &[res(r"/client\Content/./Paks/FOO.pak", 4)]);

        assert_eq!(removed, 1);
        assert!(live.is_file(), "a case only rename deleted the live file");
        assert!(!dropped.exists());
    }

    const MD5_A: &str = "0cc175b9c0f1b6a831c399e269772661";
    const MD5_B: &str = "92eb5ffee6ae2fec3ad71c777531578f";

    #[test]
    fn only_an_untouched_file_at_its_recorded_checksum_skips_the_check() {
        let entry = RecordEntry { path: "client/a.pak".to_string(), size: 10, hash: MD5_A.to_string(), mtime: Some(77) };
        let at = |size, hash: &str, on_disk| verdict(Some(&entry), size, hash, on_disk);
        let upper = MD5_A.to_uppercase();
        assert_eq!(at(10, MD5_A, Some((10, 77))), Verdict::Trusted);
        assert_eq!(at(10, &upper, Some((10, 77))), Verdict::Trusted);
        assert_eq!(at(10, MD5_B, Some((10, 77))), Verdict::Stale);
        for (size, hash, on_disk) in [
            (10, "", Some((10, 77))),
            (11, MD5_A, Some((11, 77))),
            (10, MD5_A, Some((9, 77))),
            (10, MD5_A, Some((10, 78))),
            (10, MD5_B, Some((10, 78))),
            (10, MD5_A, None),
        ] {
            assert_eq!(at(size, hash, on_disk), Verdict::Check, "{size} {hash} {on_disk:?}");
        }
        assert_eq!(verdict(None, 10, MD5_A, Some((10, 77))), Verdict::Check);
        let unstamped = RecordEntry { mtime: None, ..entry.clone() };
        assert_eq!(verdict(Some(&unstamped), 10, MD5_A, Some((10, 77))), Verdict::Check);
    }

    #[test]
    fn the_planner_trusts_untouched_files_until_an_update_holds_the_record() {
        let dir = TempDir::new("install-record");
        dir.write("Client/a.pak", b"aaaaaaaaaa");
        dir.write("Client/b.pak", b"bbbbbbbbbb");
        let c = dir.write("Client/c.pak", b"cccccccccc");
        let entry = |path: &str, md5: &str| Entry::new(path, 10, Hash::parse(md5), "");
        let installed = [entry("Client/a.pak", MD5_A), entry("Client/b.pak", MD5_A), entry("Client/c.pak", MD5_A)];
        let mut record = Record::of(dir.path(), "1.0", &installed);
        record.save(dir.path()).unwrap();
        let later = std::fs::metadata(&c).unwrap().modified().unwrap() + Duration::from_secs(5);
        std::fs::OpenOptions::new().write(true).open(&c).unwrap().set_modified(later).unwrap();

        let next = [entry("client\\A.pak", MD5_A), entry("Client/b.pak", MD5_B), entry("Client/c.pak", MD5_A)];
        let triage = Record::load(dir.path()).triage(dir.path(), &next);
        assert_eq!(triage.trusted, HashSet::from([manifest_key("Client/a.pak")]));
        assert_eq!(triage.stale, HashSet::from([manifest_key("Client/b.pak")]));

        Record::hold(dir.path(), "1.1");
        let held = Record::load(dir.path());
        assert_eq!(held.pending.as_deref(), Some("1.1"));
        assert_eq!(held.files.len(), 3, "a held record still lists what to prune");
        assert!(held.triage(dir.path(), &next).trusted.is_empty());
    }

    #[test]
    fn a_chunked_install_trusts_files_recorded_at_their_checksum_and_size_even_mid_update() {
        let dir = TempDir::new("vouches");
        dir.write("Data/a.blk", &[0u8; 10]);
        dir.write("Data/b.blk", b"bbbb");
        let entry = |path: &str, size, md5: &str| Entry::new(path, size, Hash::parse(md5), "");
        let mut record = recorded(&[entry("Data/b.blk", 10, MD5_A)]);
        record.files.push(RecordEntry { path: "Data/a.blk".into(), size: 10, hash: MD5_A.to_uppercase(), mtime: None });
        record.pending = Some("2.0".to_string());

        let next = [entry("Data/a.blk", 10, MD5_A), entry("Data/b.blk", 10, MD5_A), entry("Data/c.blk", 10, MD5_A)];
        let both = HashSet::from([manifest_key("Data/a.blk"), manifest_key("Data/b.blk")]);
        assert_eq!(record.unchanged(&next), both, "a new file awaits the update");
        assert_eq!(record.vouches(dir.path(), &next), HashSet::from([manifest_key("Data/a.blk")]), "b is short on disk");
        for other in [entry("Data/a.blk", 10, MD5_B), entry("Data/a.blk", 11, MD5_A), entry("Data/a.blk", 10, "")] {
            assert!(record.unchanged(&[other]).is_empty());
        }
    }

    #[test]
    fn a_sophon_install_is_read_from_its_record_with_its_tag_and_categories() {
        let dir = TempDir::new("sophon-migration");
        let applied = json!({
            "format": "sophon-v1",
            "tag": " 5.3.0 ",
            "audioLanguages": ["ja-jp"],
            "categories": [
                { "matchingField": "game", "manifestId": "m1", "files": [{ "path": "GenshinImpact.exe", "size": 10, "md5": MD5_A }] },
                { "matchingField": "ja-jp", "manifestId": "m2", "files": [{ "path": "Audio/ja.pck", "size": 4, "md5": MD5_B }] },
            ],
        });
        dir.write(SOPHON_RECORD_FILE, applied.to_string().as_bytes());

        let mut record = Record::load(dir.path());
        assert_eq!((record.version.as_str(), record.groups.as_slice()), ("5.3.0", &["game".to_string(), "ja-jp".to_string()][..]));
        assert_eq!(record.unchanged(&[Entry::new("Audio/ja.pck", 4, Hash::parse(MD5_B), "")]).len(), 1);
        record.save(dir.path()).unwrap();
        assert!(!dir.path().join(SOPHON_RECORD_FILE).exists(), "the install record replaces the Sophon record");
        let saved = Record::load(dir.path());
        assert_eq!((saved.version, saved.groups, saved.files.len()), (record.version, record.groups, 2));

        dir.write(SOPHON_RECORD_FILE, br#"{"format":"other","tag":"2.0.0","categories":[]}"#);
        std::fs::remove_file(dir.path().join(INSTALL_RECORD_FILE)).unwrap();
        assert!(Record::load(dir.path()).version.is_empty(), "a record of another format is ignored");
    }

    #[test]
    fn an_older_install_is_read_from_its_scan_record_and_local_index() {
        let dir = TempDir::new("record-migration");
        dir.write("Client/a.pak", b"aaaaaaaaaa");
        let stamp = on_disk_stamp(dir.path(), "Client/a.pak").unwrap().1;
        let scanned = json!({ "format": SCAN_RECORD_FORMAT, "files": [{ "path": "client/a.pak", "size": 10, "hash": MD5_A, "mtime": stamp }] });
        dir.write(SCAN_RECORD_FILE, scanned.to_string().as_bytes());
        let listed = [
            Entry::new("Client/a.pak", 10, Hash::parse(MD5_A), "https://cdn.example/Client/a.pak"),
            Entry::new("Client/old.pak", 4, Hash::parse(MD5_B), ""),
        ];
        write_local_index(dir.path(), &listed);
        write_local_index(dir.path(), &[]);
        assert_eq!(read_index_file(&dir.path().join(LOCAL_INDEX_FILE)), listed, "an empty list is never written over it");

        let record = Record::load(dir.path());
        assert_eq!(record.files.len(), 2);
        let trusted = record.triage(dir.path(), &[Entry::new("Client/a.pak", 10, Hash::parse(MD5_A), "")]);
        assert_eq!(trusted.trusted.len(), 1, "the scan record's modified time carries over");
        let old = record.files.iter().find(|f| f.path == "Client/old.pak").unwrap();
        assert_eq!((old.size, old.mtime), (4, None));
        assert_eq!(record.entries().len(), 2);

        Record::of(dir.path(), "2.0", &[]).save(dir.path()).unwrap();
        assert!(!dir.path().join(SCAN_RECORD_FILE).exists(), "the install record replaces the scan record");
        assert!(Record::load(dir.path()).files.is_empty());
    }

    #[test]
    fn an_install_from_the_split_zip_record_keeps_its_files_checksums_and_archive() {
        let dir = TempDir::new("split-migration");
        let packs = json!([{ "url": "https://cdn.example/g.zip.001", "size": 9 }]);
        let old = json!({
            "format": "hypergryph-split-zip-v1", "version": " 1.0.2 ", "packs": packs,
            "files": [{ "path": "Data/a.bin", "size": 5, "crc32": 3 }],
        });
        dir.write(SPLIT_RECORD_FILE, old.to_string().as_bytes());
        write_local_index(dir.path(), &[Entry::new("Bundles/b.bundle", 4, Hash::parse(MD5_A), "")]);

        let mut record = Record::load(dir.path());
        assert_eq!((record.version.as_str(), record.packs.len(), record.files.len()), ("1.0.2", 1, 2));
        let listed = record.entries();
        assert_eq!(listed.iter().find(|f| &*f.path == "Data/a.bin").map(|f| f.hash), Some(Hash::Crc32(3)));
        record.save(dir.path()).unwrap();
        assert!(!dir.path().join(SPLIT_RECORD_FILE).exists(), "the install record replaces the split zip record");
        let saved = Record::load(dir.path());
        assert_eq!((saved.version, saved.packs, saved.files.len()), (record.version, record.packs, 2));

        forget_record(dir.path());
        std::fs::remove_file(dir.path().join(LOCAL_INDEX_FILE)).unwrap();
        dir.write(SPLIT_RECORD_FILE, br#"{"format":"other","version":"9","packs":[],"files":[]}"#);
        assert!(Record::load(dir.path()).is_empty(), "a record of another format is ignored");
        forget_record(dir.path());
        assert!(!dir.path().join(SPLIT_RECORD_FILE).exists() && !dir.path().join(INSTALL_RECORD_FILE).exists());
    }

    #[tokio::test]
    async fn a_manifest_that_only_lists_members_is_recorded_with_their_archive_and_adopted_installs_read_back() {
        let dir = TempDir::new("record-listed");
        let packs: Arc<[Pack]> = vec![Pack { url: "https://cdn.example/g.zip.001".into(), size: 9 }].into();
        let member = |path: &str, data: &[u8]| {
            dir.write(path, data);
            Entry::new(path, data.len() as u64, Hash::Crc32(crc32fast::hash(data)), "").member_of(&packs)
        };
        let manifest = Manifest {
            version: "2.0.0".to_string(),
            listed: vec![member("Data/a.bin", b"alpha"), member("Data/b.bin", b"bravo!")],
            containers: vec![super::super::manifest::Container::default()],
            adopts: true,
            ..Manifest::default()
        };
        assert_eq!((manifest.files().count(), manifest.installed().len()), (0, 2), "listed files are recorded, not fetched");

        record_install(dir.path(), &manifest).await;

        let record = super::recorded(dir.path()).await;
        assert_eq!((record.version.as_str(), record.packs.clone()), ("2.0.0", packs.to_vec()));
        let read: Vec<(String, Hash)> = record.entries().iter().map(|f| (f.path.to_string(), f.hash)).collect();
        assert!(read.contains(&("Data/b.bin".to_string(), Hash::Crc32(crc32fast::hash(b"bravo!")))), "{read:?}");
        assert_eq!(record.triage(dir.path(), &manifest.installed()).trusted.len(), 2);
    }

    #[test]
    fn the_size_sweep_flags_short_and_missing_files_and_parts_are_cleared() {
        let dir = TempDir::new("size-sweep");
        dir.write("Client/ok.pak", b"aaaa");
        dir.write("Client/short.pak", b"aa");
        let part = dir.write("Client/ok.pak.part", b"a");
        let short = undersized_files(
            dir.path(),
            vec![
                ("Client/ok.pak".to_string(), 4),
                ("Client/short.pak".to_string(), 4),
                ("Client/missing.pak".to_string(), 4),
            ],
        );
        assert_eq!(
            short,
            HashSet::from(["Client/short.pak".to_string(), "Client/missing.pak".to_string()])
        );
        remove_part_files(dir.path(), &["Client/ok.pak".to_string()]);
        assert!(!part.exists());
        assert!(dir.path().join("Client/ok.pak").exists());
    }

    #[test]
    fn finishing_a_file_clears_its_part_and_every_record_beside_it() {
        let dir = TempDir::new("part-records");
        let names = [
            "Client/a.pak.part",
            "Client/a.pak.part.segments.json",
            "Client/a.pak.part.segments.json.tmp",
            "Client/a.pak.part.id.json",
            "Client/a.pak.part.id.json.tmp",
        ];
        let written: Vec<PathBuf> = names.iter().map(|n| dir.write(n, b"x")).collect();
        let game_file = dir.write("Client/a.pak", b"data");

        remove_part_files(dir.path(), &["Client/a.pak".to_string()]);

        for path in &written {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
        assert!(game_file.exists());
    }

    #[test]
    fn the_sweep_removes_old_repair_and_unpack_temps_only() {
        let dir = TempDir::new("temp-sweep");
        let repair = dir.write("Client/a.pak.peebify_tmp", b"x");
        let nte = dir.write("Client/b.pak.peebify-tmp", b"x");
        let reconcile = dir.write("Game/c.bin.512.peebify-part", b"x");
        let game_file = dir.write("Client/a.pak", b"x");

        assert_eq!(sweep_orphaned_parts(dir.path(), Duration::from_secs(3600)).0, 0);
        assert_eq!(sweep_orphaned_parts(dir.path(), Duration::ZERO).0, 3);

        assert!(!repair.exists() && !nte.exists() && !reconcile.exists());
        assert!(game_file.exists());
    }
}
