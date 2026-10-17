// ------------ Game Manifests ------------
// What a game should hold: each file's path, size, checksum and source (a URL, pieces of a shared object, a zip, chunks
// of a chunk store, or a member of an archive on a server), its empty folders, and the archives some games ship instead
// of a file list. Each publisher builds one, and the install, repair and verify engines reconcile to it.
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use peebify_transfer::control::{FileEvent as Event, FileHooks as Hooks};
use peebify_transfer::files::SharedFile;
use peebify_transfer::part::with_suffix;
use peebify_transfer::{FileSpec, Sealed};
use serde_json::{json, Value};
use tauri::AppHandle;

use super::engine::{combine_url, note_write, BeforeWrite, JobCore, JobKind, Preview};
use super::extract::Extracted;
use super::fs_util::{md5_hex, safe_join, write_all_at, CRC32_PREFIX, CRC64_PREFIX};
use super::game_manager::local_game_version_at;
use super::game_profiles::InstallMode;
use super::validator::{update_game_config_file, FileCheck, Record, Triage};
use super::{bd2, bluepoch, dna, gf2, hypergryph, kuro, nte, sophon, yostar};

pub(super) const MAX_CHUNK_BYTES: u64 = 64 << 20;

#[derive(Default)]
pub(super) struct Manifest {
    pub(super) version: String,
    pub(super) entries: Vec<Entry>,
    pub(super) dirs: Vec<Box<str>>,
    pub(super) containers: Vec<Container>,
    pub(super) mirrors: Vec<String>,
    pub(super) groups: Vec<String>,
    pub(super) listed: Vec<Entry>,
    pub(super) adopts: bool,
}

impl Manifest {
    pub(super) fn files(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .chain(self.containers.iter().flat_map(|c| c.parts.iter()))
    }

    pub(super) fn installed(&self) -> Vec<Entry> {
        self.entries.iter().chain(&self.listed).cloned().collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub(super) path: Box<str>,
    pub(super) size: u64,
    pub(super) hash: Hash,
    pub(super) source: Source,
    pub(super) live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hash {
    None,
    Md5([u8; 16]),
    Crc32(u32),
    Crc64(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Source {
    Url(Box<str>),
    Segments { object: Box<str>, parts: Arc<[Segment]> },
    Zip { url: Box<str>, size: u64, hash: Hash },
    Chunks { store: Arc<Store>, chunks: Arc<[Chunk]> },
    Member { packs: Arc<[Pack]>, at: Option<MemberLoc> },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Pack {
    pub(super) url: Box<str>,
    pub(super) size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MemberLoc {
    pub(super) csize: u64,
    pub(super) header_start: u64,
    pub(super) method: u16,
    pub(super) flags: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Segment {
    pub(super) src: u64,
    pub(super) dst: u64,
    pub(super) size: u64,
    pub(super) md5: [u8; 16],
}

#[derive(Debug, PartialEq)]
pub(crate) struct Store {
    pub(super) prefix: Box<str>,
    pub(super) compressed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chunk {
    pub(super) name: Box<str>,
    pub(super) md5: [u8; 16],
    pub(super) offset: u64,
    pub(super) compressed_size: u64,
    pub(super) size: u64,
}

impl Store {
    pub(super) fn url(&self, chunk: &Chunk) -> String {
        format!("{}/{}", self.prefix.trim_end_matches('/'), chunk.name)
    }
}

#[derive(Default)]
pub(super) struct Container {
    pub(super) parts: Vec<Entry>,
    pub(super) unpack: Option<Arc<dyn Unpacker>>,
    pub(super) room: u64,
}

pub(super) trait Unpacker: Send + Sync {
    fn is_package(&self, name: &str) -> bool;

    fn unpack<'a>(
        &'a self,
        job: &'a Arc<JobCore>,
        dir: &'a Path,
        archive: &'a Path,
        wanted: Option<&'a HashSet<String>>,
    ) -> BoxFuture<'a, Result<Unpacked, String>>;
}

pub(super) struct Unpacked {
    pub(super) written: Extracted,
    pub(super) files: Vec<Entry>,
}

pub(super) struct Shape {
    pub(super) problems: Vec<String>,
    pub(super) published: Option<(String, u64)>,
    pub(super) note: String,
}

impl Hash {
    pub(super) fn parse(text: &str) -> Self {
        let text = text.trim();
        if let Some(value) = text.strip_prefix(CRC64_PREFIX) {
            return value.parse().map_or(Self::None, Self::Crc64);
        }
        if let Some(value) = text.strip_prefix(CRC32_PREFIX) {
            return value.parse().map_or(Self::None, Self::Crc32);
        }
        let mut md5 = [0u8; 16];
        match hex::decode_to_slice(text, &mut md5) {
            Ok(()) => Self::Md5(md5),
            Err(_) => Self::None,
        }
    }

    pub(super) fn expected(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Md5(md5) => hex::encode(md5),
            Self::Crc32(value) => format!("{CRC32_PREFIX}{value}"),
            Self::Crc64(value) => format!("{CRC64_PREFIX}{value}"),
        }
    }
}

impl Entry {
    pub(super) fn new(path: impl Into<Box<str>>, size: u64, hash: Hash, url: impl Into<Box<str>>) -> Self {
        Self {
            path: path.into(),
            size,
            hash,
            source: Source::Url(url.into()),
            live: false,
        }
    }

    pub(super) fn live(mut self) -> Self {
        self.live = true;
        self
    }

    pub(super) fn url(&self) -> &str {
        match &self.source {
            Source::Url(url) | Source::Zip { url, .. } => url,
            Source::Segments { object, .. } => object,
            Source::Chunks { .. } | Source::Member { .. } => "",
        }
    }

    pub(super) fn chunked(&self) -> bool {
        matches!(self.source, Source::Chunks { .. })
    }

    pub(super) fn in_place(&self) -> bool {
        matches!(self.source, Source::Chunks { .. } | Source::Segments { .. })
    }

    pub(super) fn fetch_size(&self) -> u64 {
        match &self.source {
            Source::Url(_) => self.size,
            Source::Zip { size, .. } => *size,
            Source::Segments { parts, .. } => parts.iter().map(|p| p.size).sum(),
            Source::Chunks { chunks, .. } => chunks.iter().map(|c| c.compressed_size).sum(),
            Source::Member { at, .. } => at.map_or(self.size, |at| at.csize),
        }
    }

    pub(super) fn member_of(self, packs: &Arc<[Pack]>) -> Self {
        Self { source: Source::Member { packs: Arc::clone(packs), at: None }, ..self }
    }

    pub(super) fn from_json(value: &Value, base: &str) -> Self {
        let dest = value.get("dest").and_then(Value::as_str).unwrap_or("");
        let url = match value.get("fullUrl").and_then(Value::as_str).filter(|u| !u.is_empty()) {
            Some(url) => url.to_string(),
            None if base.is_empty() => String::new(),
            None => combine_url(base, dest),
        };
        let md5 = value.get("md5").and_then(Value::as_str).unwrap_or("");
        Self::new(dest, json_u64(value.get("size")).unwrap_or(0), Hash::parse(md5), url)
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "dest": &*self.path,
            "size": self.size.to_string(),
            "md5": self.hash.expected(),
            "fullUrl": Some(self.url()).filter(|u| !u.is_empty()),
        })
    }
}

pub(super) fn json_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

pub(super) fn origin_of(parts: &[Entry]) -> Vec<String> {
    parts.iter().find_map(|p| super::engine::url_origin(p.url())).into_iter().collect()
}

pub(super) fn make_dirs(dir: &Path, dirs: &[Box<str>]) {
    for folder in dirs {
        match safe_join(dir, folder) {
            Ok(path) => {
                let _ = std::fs::create_dir_all(path);
            }
            Err(e) => log::warn!("Skipping a folder from the manifest: {e}"),
        }
    }
}

// ------------ File Executor ------------
// Entries from a URL, pieces of a shared object and zips go to the transfer core as files. Pieces are written straight
// into their file, which carries a marker until every piece is in; a zip is fetched beside its file, then unpacked.

pub(super) struct Specs {
    files: Vec<FileSpec>,
    zips: Vec<(PathBuf, Box<str>)>,
    empty: Vec<(Box<str>, u64)>,
}

pub(super) fn file_specs(dir: &Path, files: &[Entry], before_write: &BeforeWrite) -> Result<Specs, String> {
    let mut specs = Specs { files: Vec::new(), zips: Vec::new(), empty: Vec::new() };
    for entry in files {
        let (id, path) = (entry.path.to_string(), safe_join(dir, &entry.path)?);
        let spec = FileSpec { id: id.clone(), path: path.clone(), size: entry.size, ..Default::default() };
        match &entry.source {
            Source::Url(url) => {
                let (md5, url, live) = (entry.hash.expected(), url.to_string(), entry.live);
                specs.files.push(FileSpec { md5, url, live, ..spec });
            }
            Source::Zip { url, size, hash } => {
                let zip = with_suffix(&path, ".peebify-zip");
                specs.files.push(FileSpec { path: zip.clone(), size: *size, md5: hash.expected(), url: url.to_string(), ..spec });
                specs.zips.push((zip, entry.path.clone()));
            }
            Source::Segments { object, parts } => {
                let parts: Vec<&Segment> = parts.iter().filter(|p| p.size > 0).collect();
                if parts.is_empty() {
                    specs.empty.push((entry.path.clone(), entry.size));
                    continue;
                }
                let file = SharedFile::new(id, path, entry.size, parts.len(), Some(pending_marker(&spec.path)), before_write.clone());
                for part in parts {
                    specs.files.push(FileSpec {
                        id: spec.id.clone(),
                        path: spec.path.clone(),
                        size: part.size,
                        md5: hex::encode(part.md5),
                        url: object.to_string(),
                        source_offset: part.src,
                        target: peebify_transfer::files::Target::InPlace(Arc::clone(&file), part.dst),
                        ..Default::default()
                    });
                }
            }
            Source::Chunks { .. } | Source::Member { .. } => return Err(format!("{} is not fetched as a file", entry.path)),
        }
    }
    Ok(specs)
}

pub(super) async fn fetch_entries(
    dir: &Path,
    files: &[Entry],
    job: Arc<dyn peebify_transfer::JobHooks>,
    hooks: Arc<dyn Hooks>,
    before_write: BeforeWrite,
) -> Result<Sealed, String> {
    let (chunked, rest): (Vec<Entry>, Vec<Entry>) = files.iter().cloned().partition(Entry::chunked);
    let (members, rest): (Vec<Entry>, Vec<Entry>) = rest.into_iter().partition(|f| matches!(f.source, Source::Member { .. }));
    let mut sealed = Sealed::nothing_written();
    if !rest.is_empty() {
        sealed = fetch_files(dir, &rest, Arc::clone(&job), before_write.clone()).await?;
    }
    if !members.is_empty() {
        sealed = super::members::fetch_members(dir, &members, Arc::clone(&job), before_write.clone()).await?;
    }
    if !chunked.is_empty() {
        sealed = fetch_chunked(dir, &chunked, job, hooks, before_write).await?;
    }
    Ok(sealed)
}

async fn fetch_files(
    dir: &Path,
    files: &[Entry],
    job: Arc<dyn peebify_transfer::JobHooks>,
    before_write: BeforeWrite,
) -> Result<Sealed, String> {
    let specs = file_specs(dir, files, &before_write)?;
    for (path, size) in &specs.empty {
        note_write(&before_write);
        open_asset_file(dir, path, *size)?;
    }
    let ledger = peebify_transfer::Ledger::new(before_write);
    let report = peebify_transfer::download_files(&super::http::shared_transfer(), Arc::clone(&job), &ledger, specs.files).await?;
    log::info!(
        "Transferred {} file(s), {:.2} GB ({} range split(s), {:.2} GB hashed by reading back).",
        report.files,
        report.bytes as f64 / f64::from(1u32 << 30),
        report.splits,
        report.read_back as f64 / f64::from(1u32 << 30)
    );
    for (zip, name) in specs.zips {
        if job.gate().is_cancelled() {
            return Err("Download aborted by user.".to_string());
        }
        let (dir, ledger, job) = (dir.to_path_buf(), Arc::clone(&ledger), Arc::clone(&job));
        tauri::async_runtime::spawn_blocking(move || {
            let unpacked = super::members::extract_member(&zip, &name, &dir, job.gate(), &ledger);
            let _ = std::fs::remove_file(&zip);
            unpacked
        })
        .await
        .map_err(|e| format!("unpack task panicked: {e}"))??;
    }
    ledger.seal().await
}

fn pending_marker(path: &Path) -> PathBuf {
    with_suffix(path, ".peebify-pending")
}

pub(super) fn scan_segments(
    path: &Path,
    entry: &Entry,
    quick: bool,
    scratch: &mut Vec<u8>,
    cancelled: &AtomicBool,
    mut on_progress: impl FnMut(u64),
) -> Result<Option<(Entry, FileCheck)>, String> {
    let Source::Segments { object, parts } = &entry.source else {
        return Err(format!("{} has no pieces to check", entry.path));
    };
    let whole = |check| Ok(Some((entry.clone(), check)));
    let marker = pending_marker(path);
    let interrupted = marker.exists();
    let Ok(meta) = std::fs::metadata(path) else {
        return whole(FileCheck::Missing);
    };
    if !meta.is_file() || quick && !interrupted && meta.len() != entry.size {
        return whole(FileCheck::SizeMismatch);
    }
    if quick && !interrupted {
        return Ok(None);
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return whole(FileCheck::Unreadable);
    };
    scratch.resize(scratch.len().max(1 << 20), 0);
    let mut broken: Vec<Segment> = Vec::new();
    for part in parts.iter() {
        if cancelled.load(Ordering::SeqCst) {
            return Err("Validation cancelled".to_string());
        }
        if part.dst.saturating_add(part.size) > meta.len() {
            broken.push(*part);
            continue;
        }
        let mut hasher = Md5::new();
        let mut left = part.size;
        let read = file.seek(SeekFrom::Start(part.dst)).and_then(|_| {
            while left > 0 {
                let want = (scratch.len() as u64).min(left) as usize;
                file.read_exact(&mut scratch[..want])?;
                hasher.update(&scratch[..want]);
                on_progress(want as u64);
                left -= want as u64;
            }
            Ok(())
        });
        if read.is_err() {
            return whole(FileCheck::Unreadable);
        }
        if <[u8; 16]>::from(hasher.finalize()) != part.md5 {
            broken.push(*part);
        }
    }
    if broken.is_empty() && meta.len() != entry.size {
        match parts.last() {
            Some(last) => broken.push(*last),
            None => return whole(FileCheck::SizeMismatch),
        }
    }
    if broken.is_empty() {
        if interrupted {
            let _ = std::fs::remove_file(marker);
        }
        return Ok(None);
    }
    let source = Source::Segments { object: object.clone(), parts: broken.into() };
    Ok(Some((Entry { source, ..entry.clone() }, FileCheck::HashMismatch)))
}

// ------------ Chunk Executor ------------
// Entries that come from a chunk store are checked chunk by chunk, so only the chunks that fail come again. A chunk
// several files share is fetched once and written into each of them, in place.

pub(super) fn scan_chunks(
    path: &Path,
    entry: &Entry,
    scratch: &mut Vec<u8>,
    cancelled: &AtomicBool,
    mut on_progress: impl FnMut(u64),
) -> Result<Option<(Entry, FileCheck)>, String> {
    let Source::Chunks { store, chunks } = &entry.source else {
        return Err(format!("{} has no chunks to check", entry.path));
    };
    let whole = |check| Ok(Some((entry.clone(), check)));
    let Ok(meta) = std::fs::metadata(path) else {
        return whole(FileCheck::Missing);
    };
    if !meta.is_file() || meta.len() != entry.size {
        return whole(FileCheck::SizeMismatch);
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return whole(FileCheck::Unreadable);
    };
    let largest = chunks.iter().map(|c| usize::try_from(c.size).unwrap_or(usize::MAX)).max().unwrap_or(0);
    if scratch.len() < largest {
        scratch.resize(largest, 0);
    }
    let mut missing = Vec::new();
    for chunk in chunks.iter() {
        if cancelled.load(Ordering::SeqCst) {
            return Err("Validation cancelled".to_string());
        }
        let want = usize::try_from(chunk.size).unwrap_or(usize::MAX);
        let ok = file
            .seek(SeekFrom::Start(chunk.offset))
            .and_then(|_| file.read_exact(&mut scratch[..want]))
            .is_ok()
            && md5_hex(&scratch[..want]) == hex::encode(chunk.md5);
        if !ok {
            missing.push(chunk.clone());
        }
        on_progress(chunk.size);
    }
    if missing.is_empty() {
        return Ok(None);
    }
    let source = Source::Chunks { store: Arc::clone(store), chunks: missing.into() };
    Ok(Some((Entry { source, ..entry.clone() }, FileCheck::HashMismatch)))
}

fn open_asset_file(install_dir: &Path, name: &str, size: u64) -> Result<(PathBuf, std::fs::File), String> {
    let path = safe_join(install_dir, name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir(&path);
    }
    let file = super::fs_util::retry_while_locked(|| {
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
    })
    .map_err(|e| super::fs_util::fmt_io(&format!("Could not open {}", path.display()), &e))?;
    file.set_len(size).map_err(|e| {
        super::fs_util::explain_disk_full(&path, size, format!("Could not size {}: {e}", path.display()))
    })?;
    Ok((path, file))
}

struct Target {
    path: Box<str>,
    size: u64,
    missing: usize,
}

struct OpenAsset {
    file: Arc<std::fs::File>,
    path: PathBuf,
    remaining: AtomicUsize,
}

struct AssetFiles {
    open: Mutex<HashMap<usize, Arc<OpenAsset>>>,
    before_write: BeforeWrite,
}

impl AssetFiles {
    fn new(before_write: BeforeWrite) -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            before_write,
        }
    }

    fn acquire(&self, install_dir: &Path, target: &Target, index: usize) -> Result<Arc<OpenAsset>, String> {
        if let Some(existing) = self.open.lock().get(&index) {
            return Ok(Arc::clone(existing));
        }
        note_write(&self.before_write);
        let (path, file) = open_asset_file(install_dir, &target.path, target.size)?;
        let mut open = self.open.lock();
        if let Some(existing) = open.get(&index) {
            return Ok(Arc::clone(existing));
        }
        let handle = Arc::new(OpenAsset {
            file: Arc::new(file),
            path,
            remaining: AtomicUsize::new(target.missing),
        });
        open.insert(index, Arc::clone(&handle));
        Ok(handle)
    }

    fn release(&self, index: usize) -> Option<Arc<OpenAsset>> {
        self.open.lock().remove(&index)
    }
}

struct JobTarget {
    target_idx: usize,
    offset: u64,
    compressed_size: u64,
}

struct ChunkJob {
    url: String,
    name: Box<str>,
    compressed: bool,
    md5: [u8; 16],
    size: u64,
    fetch_size: u64,
    targets: Vec<JobTarget>,
}

fn build_chunk_jobs(files: &[Entry]) -> (Vec<Target>, Vec<ChunkJob>) {
    let mut targets: Vec<Target> = Vec::new();
    let mut jobs: Vec<ChunkJob> = Vec::new();
    let mut by_content: HashMap<([u8; 16], u64), usize> = HashMap::new();

    for file in files {
        let Source::Chunks { store, chunks } = &file.source else {
            continue;
        };
        let target_idx = targets.len();
        for chunk in chunks.iter() {
            let target = JobTarget {
                target_idx,
                offset: chunk.offset,
                compressed_size: chunk.compressed_size,
            };
            match by_content.entry((chunk.md5, chunk.size)) {
                std::collections::hash_map::Entry::Occupied(slot) => {
                    jobs[*slot.get()].targets.push(target);
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(jobs.len());
                    jobs.push(ChunkJob {
                        url: store.url(chunk),
                        name: chunk.name.clone(),
                        compressed: store.compressed,
                        md5: chunk.md5,
                        size: chunk.size,
                        fetch_size: chunk.compressed_size,
                        targets: vec![target],
                    });
                }
            }
        }
        targets.push(Target {
            path: file.path.clone(),
            size: file.size,
            missing: chunks.len(),
        });
    }
    (targets, jobs)
}

fn decode_chunk<'a>(job: &ChunkJob, raw: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, String> {
    let bytes: std::borrow::Cow<[u8]> = if job.compressed {
        let capacity = usize::try_from(job.size)
            .map_err(|_| format!("chunk {} size {} does not fit in memory", job.name, job.size))?;
        std::borrow::Cow::Owned(
            zstd::bulk::decompress(raw, capacity).map_err(|e| format!("chunk {} is not valid zstd: {e}", job.name))?,
        )
    } else {
        std::borrow::Cow::Borrowed(raw)
    };
    if bytes.len() as u64 != job.size {
        return Err(format!(
            "chunk {} decompressed to {} bytes, expected {}",
            job.name,
            bytes.len(),
            job.size
        ));
    }
    let got = md5_hex(&bytes);
    if got != hex::encode(job.md5) {
        return Err(format!(
            "chunk {} failed verification (got {got}, expected {})",
            job.name,
            hex::encode(job.md5)
        ));
    }
    Ok(bytes)
}

struct ChunkWriter {
    install_dir: PathBuf,
    jobs: Vec<ChunkJob>,
    targets: Vec<Target>,
    files: AssetFiles,
    hooks: Arc<dyn Hooks>,
    ledger: Arc<peebify_transfer::Ledger>,
}

impl peebify_transfer::ChunkSink for ChunkWriter {
    fn accept(&self, index: usize, raw: bytes::Bytes) -> Result<(), peebify_transfer::ChunkError> {
        use peebify_transfer::ChunkError;
        let job = &self.jobs[index];
        let bytes = decode_chunk(job, &raw).map_err(ChunkError::Corrupt)?;
        for t in &job.targets {
            let Some(target) = self.targets.get(t.target_idx) else {
                continue;
            };
            let open = self
                .files
                .acquire(&self.install_dir, target, t.target_idx)
                .map_err(ChunkError::Fatal)?;
            write_all_at(&open.file, &bytes, t.offset)
                .map_err(|e| ChunkError::Fatal(super::fs_util::fmt_io("Write error", &e)))?;
            let previous = open.remaining.fetch_sub(1, Ordering::SeqCst);
            self.hooks.event(Event::Written { delta: bytes.len() as u64 });
            self.hooks.event(Event::Bytes {
                path: &target.path,
                delta: t.compressed_size,
            });
            if previous <= 1 {
                if let Some(done) = self.files.release(t.target_idx) {
                    self.ledger.complete_in_place(Arc::clone(&done.file), done.path.clone());
                }
                self.hooks.event(Event::FileDone { path: &target.path });
            }
        }
        Ok(())
    }
}

pub(super) async fn fetch_chunked(
    install_dir: &Path,
    files: &[Entry],
    job: Arc<dyn peebify_transfer::JobHooks>,
    hooks: Arc<dyn Hooks>,
    before_write: BeforeWrite,
) -> Result<Sealed, String> {
    let empty: Vec<(Box<str>, u64)> = files
        .iter()
        .filter(|f| matches!(&f.source, Source::Chunks { chunks, .. } if chunks.is_empty()))
        .map(|f| (f.path.clone(), f.size))
        .collect();
    if !empty.is_empty() {
        let install_dir = install_dir.to_path_buf();
        let hooks = Arc::clone(&hooks);
        let before_write = before_write.clone();
        tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
            for (path, size) in empty {
                if hooks.is_cancelled() {
                    return Err("Operation cancelled by user.".to_string());
                }
                note_write(&before_write);
                open_asset_file(&install_dir, &path, size)?;
                hooks.event(Event::FileDone { path: &path });
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("file setup task failed: {e}"))??;
    }

    let (targets, jobs) = build_chunk_jobs(files);
    if jobs.is_empty() {
        return Ok(Sealed::nothing_written());
    }
    let unique = jobs.len();
    let occurrences: usize = jobs.iter().map(|j| j.targets.len()).sum();
    if occurrences > unique {
        log::info!(
            "{unique} unique chunks cover {occurrences} chunk slots, so {} duplicate downloads are skipped.",
            occurrences - unique
        );
    }

    let specs: Vec<peebify_transfer::ChunkSpec> = jobs
        .iter()
        .map(|j| peebify_transfer::ChunkSpec {
            id: j.name.to_string(),
            url: j.url.clone(),
            size_hint: j.fetch_size,
        })
        .collect();
    let ledger = peebify_transfer::Ledger::new(None);
    let writer = Arc::new(ChunkWriter {
        install_dir: install_dir.to_path_buf(),
        jobs,
        targets,
        files: AssetFiles::new(before_write),
        hooks,
        ledger: Arc::clone(&ledger),
    });
    let (report, sealed) = peebify_transfer::fetch_chunks(
        &super::http::shared_transfer(),
        job,
        &ledger,
        specs,
        writer as Arc<dyn peebify_transfer::ChunkSink>,
        MAX_CHUNK_BYTES,
    )
    .await?;
    log::info!(
        "Fetched {} chunk(s), {:.2} GB, {} fetched again after a damaged copy.",
        report.chunks,
        report.bytes as f64 / f64::from(1u32 << 30),
        report.refetched
    );
    Ok(sealed)
}

// ------------ Publishers ------------
// What each publisher supplies: its manifest, how its game repairs and verifies, and the steps around a reconcile only
// that game has (its version lookups, install preview, files written after an install). Adding a game adds a publisher.
pub(super) struct Cx<'a> {
    pub(super) app: &'a AppHandle,
    pub(super) profile: &'static Value,
    pub(super) dir: &'a Path,
    pub(super) purpose: JobKind,
    pub(super) job: Arc<dyn peebify_transfer::JobHooks>,
}

#[derive(Default)]
pub(super) struct Policy {
    pub(super) latest: bool,
    pub(super) checksums: bool,
    pub(super) records: bool,
    pub(super) partial_verify: bool,
    pub(super) nested: bool,
}

pub(super) trait Publisher: Send + Sync {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>>;

    fn latest<'a>(&'a self, profile: &'a Value, fresh: bool) -> BoxFuture<'a, Result<Value, String>>;

    fn preview<'a>(&'a self, profile: &'a Value, quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>>;

    fn write_space(&self, download: u64, install: u64) -> (u64, f64) {
        (download.max(install), 1.0)
    }

    fn installed_version(&self, dir: &Path) -> Option<String> {
        local_game_version_at(dir)
    }

    fn update_pending(&self, _latest: &Value, _dir: &Path) -> bool {
        false
    }

    fn policy(&self, _dir: &Path) -> Policy {
        Policy::default()
    }

    fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        Record::load(dir).triage(dir, files)
    }

    fn after_seal<'a>(
        &'a self,
        _job: &'a Arc<JobCore>,
        dir: &'a Path,
        manifest: &'a Manifest,
        sealed: &'a Sealed,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { update_game_config_file(sealed, dir, &manifest.version).map(|()| true) })
    }

    fn shape_check<'a>(&'a self, _cx: &'a Cx<'a>) -> Option<BoxFuture<'a, Shape>> {
        None
    }

    fn repair<'a>(&'a self, _job: &'a Arc<JobCore>, _dir: &'a Path, _start_ms: i64) -> Option<BoxFuture<'a, Result<(), String>>> {
        None
    }
}

pub(super) fn publisher(profile: &Value) -> Arc<dyn Publisher> {
    match InstallMode::of(profile) {
        InstallMode::Default | InstallMode::Unknown => Arc::new(kuro::Kuro::default()),
        InstallMode::Yostar => Arc::new(yostar::Yostar),
        InstallMode::Gf2 => Arc::new(gf2::Gf2),
        InstallMode::Bluepoch => Arc::new(bluepoch::Bluepoch),
        InstallMode::Hypergryph => Arc::new(hypergryph::Hypergryph),
        InstallMode::Sophon => Arc::new(sophon::Sophon),
        InstallMode::Netease => Arc::new(nte::Nte),
        InstallMode::Bd2 => Arc::new(bd2::Bd2::default()),
        InstallMode::Dna => Arc::new(dna::Dna),
    }
}

// ------------ Tests ------------
// Checksum parsing, file list entries, and the file, segment and chunk executors.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;

    #[test]
    fn checksums_round_trip_and_anything_else_checks_by_size() {
        let md5 = "5D41402ABC4B2A76B9719D911017C592";
        assert_eq!(Hash::parse(md5).expected(), md5.to_lowercase());
        assert_eq!(Hash::parse("crc64:1157254865370011820"), Hash::Crc64(1157254865370011820));
        assert_eq!(Hash::Crc64(0).expected(), "crc64:0");
        assert_eq!(Hash::parse("crc32:3421780262"), Hash::Crc32(3421780262));
        assert_eq!(Hash::Crc32(7).expected(), "crc32:7");
        for other in ["", "abc", "crc64:x", "crc32:-1", "crc32:4294967296", "zz41402abc4b2a76b9719d911017c592"] {
            assert_eq!(Hash::parse(other), Hash::None, "{other}");
        }
        assert_eq!(Hash::None.expected(), "");
    }

    #[test]
    fn each_game_repairs_and_verifies_the_way_it_always_did() {
        let policy = |game: &dyn Fn(&Path) -> Policy| {
            let p = game(Path::new("no-such-install"));
            (p.latest, p.checksums, p.records, p.partial_verify, p.nested)
        };
        assert_eq!(policy(&|d| kuro::Kuro::default().policy(d)), (false, true, true, false, false), "an install nothing recorded");
        assert_eq!(policy(&|d| yostar::Yostar.policy(d)), (false, false, true, false, false));
        assert_eq!(policy(&|d| gf2::Gf2.policy(d)), (false, true, true, true, false), "an install nothing recorded");
        assert_eq!(policy(&|d| bluepoch::Bluepoch.policy(d)), (false, false, false, true, true));
        assert_eq!(policy(&|d| nte::Nte.policy(d)), (true, false, true, false, false));
        assert_eq!(policy(&|d| sophon::Sophon.policy(d)), (true, true, true, false, true));
        assert_eq!(policy(&|d| hypergryph::Hypergryph.policy(d)), (true, true, true, false, true), "an install nothing recorded");
        assert_eq!(policy(&|d| bd2::Bd2::default().policy(d)), (false, true, true, true, false), "an install nothing recorded");
        assert_eq!(policy(&|d| dna::Dna.policy(d)), (false, true, true, false, false));
    }

    #[test]
    fn file_list_entries_read_either_size_and_their_own_url_first() {
        let listed = json!({ "dest": "Client/a.pak", "size": "12", "md5": "5d41402abc4b2a76b9719d911017c592" });
        let entry = Entry::from_json(&listed, "https://cdn.example/game/");
        assert_eq!((&*entry.path, entry.size), ("Client/a.pak", 12));
        assert_eq!(entry.url(), "https://cdn.example/game/Client/a.pak");
        let own = json!({ "dest": "b.pak", "size": 3, "fullUrl": "https://other.example/b.pak" });
        assert_eq!(Entry::from_json(&own, "https://cdn.example").url(), "https://other.example/b.pak");
        assert_eq!(Entry::from_json(&json!({ "dest": "c.pak" }), "").url(), "");
        assert_eq!(json_u64(Some(&json!(" 7 "))), Some(7));
        assert_eq!(json_u64(Some(&json!(null))), None);
        let shown = entry.to_json();
        assert_eq!(shown["size"], "12");
        assert_eq!(shown["fullUrl"], "https://cdn.example/game/Client/a.pak");
    }

    #[test]
    fn url_entries_become_transfer_specs_under_the_install() {
        let dir = Path::new(if cfg!(windows) { r"C:\game" } else { "/game" });
        let files = [
            Entry::new("Client/a.pak", 4, Hash::Crc64(9), "https://cdn.example/a.pak").live(),
            Entry::new("game.7z.001", 8, Hash::None, "https://cdn.example/game.7z.001"),
        ];
        let specs = file_specs(dir, &files, &None).unwrap().files;
        assert_eq!(specs[0].id, "Client/a.pak");
        assert_eq!(specs[0].path, dir.join("Client").join("a.pak"));
        assert_eq!((specs[0].size, specs[0].md5.as_str(), specs[0].live), (4, "crc64:9", true));
        assert_eq!((specs[1].md5.as_str(), specs[1].live), ("", false));
        assert_eq!(specs[1].url, "https://cdn.example/game.7z.001");
        assert!(file_specs(dir, &[Entry::new("../x", 1, Hash::None, "")], &None).is_err());
    }

    struct NoHooks;

    impl peebify_transfer::control::Control for NoHooks {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    impl Hooks for NoHooks {
        fn event(&self, _event: Event) {}
    }

    fn md5_of(bytes: &[u8]) -> [u8; 16] {
        let mut md5 = [0u8; 16];
        hex::decode_to_slice(md5_hex(bytes), &mut md5).unwrap();
        md5
    }

    fn chunked(name: &str, compressed: bool, parts: &[&[u8]]) -> Entry {
        let mut offset = 0u64;
        let chunks: Vec<Chunk> = parts
            .iter()
            .map(|bytes| {
                let chunk = Chunk {
                    name: hex::encode(md5_of(bytes)).into(),
                    md5: md5_of(bytes),
                    offset,
                    compressed_size: bytes.len() as u64,
                    size: bytes.len() as u64,
                };
                offset += bytes.len() as u64;
                chunk
            })
            .collect();
        let store = Arc::new(Store { prefix: "https://cdn.example.com/chunks/".into(), compressed });
        Entry {
            path: name.into(),
            size: offset,
            hash: Hash::Md5(md5_of(&parts.concat())),
            source: Source::Chunks { store, chunks: chunks.into() },
            live: false,
        }
    }

    fn chunk_offsets(entry: &Entry) -> Vec<u64> {
        match &entry.source {
            Source::Chunks { chunks, .. } => chunks.iter().map(|c| c.offset).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_chunked_entry_is_fetched_by_its_compressed_chunks() {
        let entry = chunked("Data/blob.blk", false, &[b"aaaa", b"bbbbbb"]);
        assert!(entry.chunked());
        assert_eq!(entry.url(), "");
        assert_eq!(entry.fetch_size(), 10);
        assert!(entry.to_json()["fullUrl"].is_null());
    }

    #[test]
    fn a_scan_narrows_a_chunked_file_to_the_chunks_that_fail() {
        let dir = TempDir::new("chunk-scan");
        let entry = chunked("Data/blob.blk", false, &[b"aaaa", b"bbbb", b"cccc"]);
        let path = dir.0.join("Data").join("blob.blk");
        let scan = |scratch: &mut Vec<u8>| scan_chunks(&path, &entry, scratch, &AtomicBool::new(false), |_| {}).unwrap();
        let mut scratch = Vec::new();

        let (missing, check) = scan(&mut scratch).unwrap();
        assert_eq!((chunk_offsets(&missing), check), (vec![0, 4, 8], FileCheck::Missing));
        dir.write("Data/blob.blk", b"aaaa");
        assert_eq!(scan(&mut scratch).unwrap().1, FileCheck::SizeMismatch);

        dir.write("Data/blob.blk", b"aaaa\0\0\0\0cccc");
        let (gap, check) = scan(&mut scratch).unwrap();
        assert_eq!((chunk_offsets(&gap), check), (vec![4], FileCheck::HashMismatch));
        assert_eq!((gap.size, gap.fetch_size()), (12, 4));

        dir.write("Data/blob.blk", b"aaaabbbbcccc");
        assert!(scan(&mut scratch).is_none());
    }

    fn segmented(path: &str, parts: &[(&[u8], u64)]) -> Entry {
        let whole = parts.iter().flat_map(|(bytes, _)| bytes.iter().copied()).collect::<Vec<u8>>();
        let mut dst = 0;
        let segments: Vec<Segment> = parts
            .iter()
            .map(|(bytes, src)| {
                let part = Segment { src: *src, dst, size: bytes.len() as u64, md5: md5_of(bytes) };
                dst += part.size;
                part
            })
            .collect();
        Entry {
            path: path.into(),
            size: whole.len() as u64,
            hash: Hash::Md5(md5_of(&whole)),
            source: Source::Segments { object: "https://cdn.example/Res/pak".into(), parts: segments.into() },
            live: true,
        }
    }

    fn pieces(entry: &Entry) -> Vec<(u64, u64)> {
        match &entry.source {
            Source::Segments { parts, .. } => parts.iter().map(|p| (p.src, p.dst)).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn pieces_and_zips_are_fetched_into_their_files_and_empty_files_are_only_made() {
        let dir = Path::new(if cfg!(windows) { r"C:\game" } else { "/game" });
        let zipped = Entry {
            source: Source::Zip { url: "https://cdn.example/a.dll.zip".into(), size: 3, hash: Hash::Md5([1; 16]) },
            ..Entry::new("NTEGlobal/a.dll", 9, Hash::None, "")
        };
        let files = [segmented("big.pak", &[(b"head", 0), (b"tail", 40)]), segmented("none.bin", &[]), zipped];
        let specs = file_specs(dir, &files, &None).unwrap();
        assert_eq!(specs.empty, vec![("none.bin".into(), 0)]);
        let ranges: Vec<_> = specs.files.iter().map(|s| (s.id.as_str(), s.size, s.source_offset)).collect();
        assert_eq!(ranges, vec![("big.pak", 4, 0), ("big.pak", 4, 40), ("NTEGlobal/a.dll", 3, 0)]);
        assert_eq!(specs.files[0].url, "https://cdn.example/Res/pak");
        assert_eq!(specs.files[2].path, dir.join("NTEGlobal").join("a.dll.peebify-zip"));
        assert_eq!(specs.zips[0].1.as_ref(), "NTEGlobal/a.dll");
        assert_eq!(files[0].fetch_size(), 8);
        assert!(!files[2].in_place());
    }

    #[test]
    fn a_scan_narrows_a_segmented_file_to_the_pieces_that_fail() {
        let dir = TempDir::new("segment-scan");
        let entry = segmented("big.pak", &[(b"first block", 0), (b"second block", 100)]);
        let path = dir.0.join("big.pak");
        let scan = |quick| scan_segments(&path, &entry, quick, &mut Vec::new(), &AtomicBool::new(false), |_| {}).unwrap();
        let narrowed = |found: Option<(Entry, FileCheck)>| found.map(|(e, check)| (pieces(&e), check));
        let second = vec![(100, 11)];

        assert_eq!(narrowed(scan(false)), Some((vec![(0, 0), (100, 11)], FileCheck::Missing)));
        dir.write("big.pak", b"first blockSECOND BLOCK");
        assert_eq!(narrowed(scan(false)), Some((second.clone(), FileCheck::HashMismatch)));
        assert_eq!(scan(true), None, "a quick check compares sizes only");
        dir.write("big.pak", b"first block");
        assert_eq!(narrowed(scan(false)), Some((second.clone(), FileCheck::HashMismatch)), "a short file keeps its good block");
        assert_eq!(scan(true).map(|(_, check)| check), Some(FileCheck::SizeMismatch));
        dir.write("big.pak", b"first blocksecond block and more");
        assert_eq!(narrowed(scan(false)), Some((second.clone(), FileCheck::HashMismatch)), "a long file is cut by its last block");

        dir.write("big.pak", b"first block\0\0\0\0\0\0\0\0\0\0\0\0");
        dir.write("big.pak.peebify-pending", b"");
        assert_eq!(narrowed(scan(true)), Some((second, FileCheck::HashMismatch)), "an unfinished file is checked block by block");
        assert!(pending_marker(&path).exists());
        dir.write("big.pak", b"first blocksecond block");
        assert_eq!(scan(true), None);
        assert!(!pending_marker(&path).exists(), "a whole file loses its marker");
        assert!(scan_segments(&path, &entry, false, &mut Vec::new(), &AtomicBool::new(true), |_| {}).is_err());
    }

    fn writer_for(dir: &TempDir, files: &[Entry]) -> (ChunkWriter, Arc<peebify_transfer::Ledger>) {
        let (targets, jobs) = build_chunk_jobs(files);
        let ledger = peebify_transfer::Ledger::new(None);
        let writer = ChunkWriter {
            install_dir: dir.0.clone(),
            jobs,
            targets,
            files: AssetFiles::new(None),
            hooks: Arc::new(NoHooks),
            ledger: Arc::clone(&ledger),
        };
        (writer, ledger)
    }

    #[tokio::test]
    async fn one_fetched_chunk_lands_in_every_file_that_holds_it_and_finished_files_wait_for_the_ledger() {
        use peebify_transfer::ChunkSink;
        let dir = TempDir::new("chunk-writer");
        let files = [chunked("a.bin", false, &[b"hello ", b"world"]), chunked("sub/b.bin", false, &[b"world", b"!!"])];
        let (writer, ledger) = writer_for(&dir, &files);
        assert_eq!(writer.jobs.len(), 3, "the shared chunk is fetched once");

        for (index, job) in writer.jobs.iter().enumerate() {
            let body = [&b"hello "[..], b"world", b"!!"].into_iter().find(|b| md5_of(b) == job.md5).unwrap();
            writer.accept(index, bytes::Bytes::copy_from_slice(body)).unwrap();
        }
        let waiting = ledger.has_pending();
        let open_after = writer.files.open.lock().len();
        ledger.seal().await.unwrap();

        assert_eq!(std::fs::read(dir.0.join("a.bin")).unwrap(), b"hello world");
        assert_eq!(std::fs::read(dir.0.join("sub").join("b.bin")).unwrap(), b"world!!");
        assert!(waiting, "finished files wait for the ledger instead of a flush each");
        assert_eq!(open_after, 0);
    }

    #[test]
    fn a_damaged_or_mismatched_chunk_is_sent_back_for_another_fetch() {
        use peebify_transfer::{ChunkError, ChunkSink};
        let dir = TempDir::new("chunk-damaged");
        let (writer, _ledger) = writer_for(&dir, &[chunked("a.bin", true, &[b"payload"])]);

        let garbage = writer.accept(0, bytes::Bytes::from_static(b"not zstd at all"));
        assert!(matches!(garbage, Err(ChunkError::Corrupt(ref why)) if why.contains("not valid zstd")), "{garbage:?}");

        let wrong = zstd::bulk::compress(b"PAYLOAD", 0).unwrap();
        let mismatch = writer.accept(0, bytes::Bytes::from(wrong));
        assert!(matches!(mismatch, Err(ChunkError::Corrupt(ref why)) if why.contains("failed verification")), "{mismatch:?}");

        let right = zstd::bulk::compress(b"payload", 0).unwrap();
        writer.accept(0, bytes::Bytes::from(right)).unwrap();
        assert_eq!(std::fs::read(dir.0.join("a.bin")).unwrap(), b"payload");
    }
}
