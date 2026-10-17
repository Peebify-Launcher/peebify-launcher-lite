// ------------ Durability Ledger ------------
// Flushes finished downloads to disk in batches and only then renames them and records progress, so a crash never trusts lost bytes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
pub const CHECKPOINT_BYTES: u64 = 1 << 30;
const CHECKPOINT_ITEMS: usize = 2048;
const FLUSH_THREADS: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct Sealed(());

impl Sealed {
    pub fn nothing_written() -> Self {
        Self(())
    }
}

pub trait InProgress: Send + Sync {
    fn path(&self) -> &Path;
    fn handle(&self) -> Option<Arc<std::fs::File>>;
    fn snapshot(&self) -> Option<Box<dyn FnOnce() + Send>>;
}

enum Pending {
    Part {
        handle: Option<Arc<std::fs::File>>,
        part: PathBuf,
        dest: PathBuf,
        live: bool,
        cleanup: Box<dyn FnOnce() + Send>,
    },
    InPlace {
        handle: Arc<std::fs::File>,
        path: PathBuf,
    },
    Marker {
        path: PathBuf,
    },
    Adopted {
        path: PathBuf,
    },
}

impl Pending {
    fn flush_target(&self) -> Option<(PathBuf, Option<Arc<std::fs::File>>)> {
        match self {
            Self::Part { handle: Some(h), part, .. } => Some((part.clone(), Some(Arc::clone(h)))),
            Self::InPlace { handle, path } => Some((path.clone(), Some(Arc::clone(handle)))),
            Self::Adopted { path } => Some((path.clone(), None)),
            _ => None,
        }
    }
}

pub type BeforeLiveRename = Arc<dyn Fn() + Send + Sync>;

pub struct Ledger {
    pending: Mutex<Vec<Pending>>,
    in_progress: Mutex<HashMap<u64, Arc<dyn InProgress>>>,
    next_id: AtomicU64,
    written: AtomicU64,
    pending_count: AtomicUsize,
    wake: tokio::sync::Notify,
    running: Arc<tokio::sync::Mutex<()>>,
    failure: Mutex<Option<String>>,
    before_live_rename: Option<BeforeLiveRename>,
    checkpoints: AtomicU64,
}

pub struct ProgressToken {
    ledger: Arc<Ledger>,
    id: u64,
}

impl Drop for ProgressToken {
    fn drop(&mut self) {
        self.ledger.in_progress.lock().remove(&self.id);
    }
}

impl Ledger {
    pub fn new(before_live_rename: Option<BeforeLiveRename>) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            in_progress: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            written: AtomicU64::new(0),
            pending_count: AtomicUsize::new(0),
            wake: tokio::sync::Notify::new(),
            running: Arc::new(tokio::sync::Mutex::new(())),
            failure: Mutex::new(None),
            before_live_rename,
            checkpoints: AtomicU64::new(0),
        })
    }

    pub fn track(self: &Arc<Self>, file: Arc<dyn InProgress>) -> ProgressToken {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.in_progress.lock().insert(id, file);
        ProgressToken {
            ledger: Arc::clone(self),
            id,
        }
    }

    pub fn note_written(&self, bytes: u64) {
        let before = self.written.fetch_add(bytes, Ordering::Relaxed);
        if before < CHECKPOINT_BYTES && before + bytes >= CHECKPOINT_BYTES {
            self.wake.notify_one();
        }
    }

    pub fn complete_part(
        &self,
        handle: Option<Arc<std::fs::File>>,
        part: PathBuf,
        dest: PathBuf,
        live: bool,
        cleanup: impl FnOnce() + Send + 'static,
    ) {
        self.push(Pending::Part {
            handle,
            part,
            dest,
            live,
            cleanup: Box::new(cleanup),
        });
    }

    pub fn complete_in_place(&self, handle: Arc<std::fs::File>, path: PathBuf) {
        self.push(Pending::InPlace { handle, path });
    }

    pub fn clear_marker(&self, path: PathBuf) {
        self.push(Pending::Marker { path });
    }

    pub fn adopt_file(&self, path: PathBuf) {
        self.push(Pending::Adopted { path });
    }

    pub fn adopt_tree(&self, root: &Path, skip: &[&Path]) -> usize {
        let mut adopted = Vec::new();
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if !skip.iter().any(|s| *s == path) {
                        folders.push(path);
                    }
                } else if kind.is_file() {
                    adopted.push(Pending::Adopted { path });
                }
            }
        }
        let count = adopted.len();
        self.pending.lock().extend(adopted);
        self.pending_count.fetch_add(count, Ordering::Relaxed);
        count
    }

    fn push(&self, item: Pending) {
        self.pending.lock().push(item);
        if self.pending_count.fetch_add(1, Ordering::Relaxed) + 1 >= CHECKPOINT_ITEMS {
            self.wake.notify_one();
        }
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }

    pub fn checkpoints(&self) -> u64 {
        self.checkpoints.load(Ordering::Relaxed)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    pub async fn run(self: Arc<Self>, interval: Duration) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.wake.notified() => {}
            }
            if let Err(e) = self.checkpoint().await {
                log::warn!("A durability checkpoint failed: {e}");
                self.failure.lock().get_or_insert(e);
            }
        }
    }

    pub async fn checkpoint(self: &Arc<Self>) -> Result<(), String> {
        let running = Arc::clone(&self.running).lock_owned().await;
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _running = running;
            me.checkpoint_blocking()
        })
        .await
        .map_err(|e| format!("checkpoint task panicked: {e}"))?
    }

    pub async fn seal(self: &Arc<Self>) -> Result<Sealed, String> {
        self.checkpoint().await?;
        if let Some(e) = self.failure() {
            return Err(e);
        }
        Ok(Sealed(()))
    }

    fn checkpoint_blocking(&self) -> Result<(), String> {
        let items: Vec<Pending> = std::mem::take(&mut *self.pending.lock());
        self.pending_count.fetch_sub(items.len(), Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        let running: Vec<Arc<dyn InProgress>> =
            self.in_progress.lock().values().map(Arc::clone).collect();
        if items.is_empty() && running.is_empty() {
            return Ok(());
        }

        let records: Vec<Box<dyn FnOnce() + Send>> =
            running.iter().filter_map(|f| f.snapshot()).collect();
        let mut targets: Vec<(PathBuf, Option<Arc<std::fs::File>>)> =
            items.iter().filter_map(Pending::flush_target).collect();
        targets.extend(
            running
                .iter()
                .filter_map(|f| f.handle().map(|h| (f.path().to_path_buf(), Some(h)))),
        );
        flush_all(&targets);
        drop(targets);

        let mut first_error: Option<String> = None;
        for item in items {
            match item {
                Pending::Part {
                    handle,
                    part,
                    dest,
                    live,
                    cleanup,
                } => {
                    drop(handle);
                    if live {
                        if let Some(note) = &self.before_live_rename {
                            note();
                        }
                    }
                    match crate::fsx::finalize_replace(&part, &dest) {
                        Ok(()) => cleanup(),
                        Err(e) => {
                            log::warn!("{} stays a verified part: {e}", part.display());
                            first_error.get_or_insert(e);
                        }
                    }
                }
                Pending::InPlace { handle, .. } => drop(handle),
                Pending::Adopted { .. } => {}
                Pending::Marker { path } => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        for record in records {
            record();
        }
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn flush_all(targets: &[(PathBuf, Option<Arc<std::fs::File>>)]) {
    if targets.is_empty() {
        return;
    }
    let cursor = AtomicUsize::new(0);
    let fell_back = AtomicUsize::new(0);
    let unopened = AtomicUsize::new(0);
    let volumes: Mutex<HashMap<std::ffi::OsString, Arc<std::fs::File>>> = Mutex::new(HashMap::new());
    std::thread::scope(|scope| {
        for _ in 0..FLUSH_THREADS.min(targets.len()) {
            scope.spawn(|| loop {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                let Some((path, handle)) = targets.get(i) else {
                    return;
                };
                let handle = match handle {
                    Some(h) => Arc::clone(h),
                    None => match std::fs::OpenOptions::new().write(true).open(path) {
                        Ok(f) => Arc::new(f),
                        Err(e) => {
                            if unopened.fetch_add(1, Ordering::Relaxed) == 0 {
                                log::warn!("Could not open {} to flush it: {e}", path.display());
                            }
                            continue;
                        }
                    },
                };
                if write_back(&handle).is_err() {
                    fell_back.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = handle.sync_data() {
                        log::warn!("Could not flush {}: {e}", path.display());
                    }
                }
                volumes.lock().entry(volume_key(path)).or_insert(handle);
            });
        }
    });
    for handle in volumes.into_inner().values() {
        if let Err(e) = handle.sync_data() {
            log::warn!("Could not empty the drive cache: {e}");
        }
    }
    let fell_back = fell_back.into_inner();
    if fell_back > 0 {
        log::debug!("{fell_back} file(s) were flushed one at a time because their drive cannot batch.");
    }
    let unopened = unopened.into_inner();
    if unopened > 1 {
        log::warn!("{unopened} written file(s) could not be opened to flush them.");
    }
}

fn volume_key(path: &Path) -> std::ffi::OsString {
    match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) => prefix.as_os_str().to_ascii_uppercase(),
        _ => std::ffi::OsString::new(),
    }
}

#[cfg(windows)]
fn write_back(file: &std::fs::File) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct IoStatusBlock {
        status: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtFlushBuffersFileEx(
            file: *mut c_void,
            flags: u32,
            parameters: *const c_void,
            parameters_size: u32,
            io_status: *mut IoStatusBlock,
        ) -> i32;
    }

    const FLUSH_FLAGS_NO_SYNC: u32 = 0x0000_0002;
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtFlushBuffersFileEx(
            file.as_raw_handle(),
            FLUSH_FLAGS_NO_SYNC,
            std::ptr::null(),
            0,
            &mut io_status,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "NtFlushBuffersFileEx failed with {:#x}",
            status as u32
        )))
    }
}

#[cfg(not(windows))]
fn write_back(file: &std::fs::File) -> std::io::Result<()> {
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct Growing {
        path: PathBuf,
        handle: Arc<std::fs::File>,
        recorded: Arc<Mutex<Vec<u64>>>,
    }

    impl InProgress for Growing {
        fn path(&self) -> &Path {
            &self.path
        }
        fn handle(&self) -> Option<Arc<std::fs::File>> {
            Some(Arc::clone(&self.handle))
        }
        fn snapshot(&self) -> Option<Box<dyn FnOnce() + Send>> {
            let len = self.handle.metadata().map(|m| m.len()).unwrap_or(0);
            let recorded = Arc::clone(&self.recorded);
            Some(Box::new(move || recorded.lock().push(len)))
        }
    }

    #[tokio::test]
    async fn parts_are_renamed_only_at_a_checkpoint_and_records_follow_the_flush() {
        let dir = crate::testing::scratch_dir("ledger");
        let dest = dir.join("a.pak");
        let part = crate::part::part_path_for(&dest);
        std::fs::write(&dest, b"old").unwrap();
        std::fs::write(&part, b"new").unwrap();
        let marker = dir.join(".pending");
        std::fs::write(&marker, b"").unwrap();
        let noted = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&noted);
        let ledger = Ledger::new(Some(Arc::new(move || seen.store(true, Ordering::SeqCst))));

        let growing_path = dir.join("b.pak.part");
        let growing = Arc::new(std::fs::File::create(&growing_path).unwrap());
        crate::fsx::write_all_at(&growing, b"12345", 0).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let token = ledger.track(Arc::new(Growing {
            path: growing_path,
            handle: Arc::clone(&growing),
            recorded: Arc::clone(&recorded),
        }));

        let cleaned = Arc::new(AtomicBool::new(false));
        let did_clean = Arc::clone(&cleaned);
        ledger.complete_part(None, part.clone(), dest.clone(), true, move || {
            did_clean.store(true, Ordering::SeqCst)
        });
        ledger.clear_marker(marker.clone());
        let before = std::fs::read(&dest).unwrap();
        let marker_before = marker.exists();

        ledger.checkpoint().await.unwrap();
        let after = std::fs::read(&dest).unwrap();
        drop(token);
        let sealed = ledger.seal().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(before, b"old");
        assert!(marker_before);
        assert_eq!(after, b"new");
        assert!(!part.exists());
        assert!(!marker.exists());
        assert!(noted.load(Ordering::SeqCst), "a live file was about to change");
        assert!(cleaned.load(Ordering::SeqCst));
        assert_eq!(*recorded.lock(), vec![5]);
        assert!(sealed.is_ok());
        assert_eq!(ledger.checkpoints(), 1, "an empty seal does no work");
    }

    #[tokio::test]
    async fn adopted_files_are_flushed_at_the_seal_and_skipped_folders_left_alone() {
        let dir = crate::testing::scratch_dir("ledger-adopt");
        std::fs::create_dir_all(dir.join("Data/sub")).unwrap();
        std::fs::create_dir_all(dir.join("download")).unwrap();
        std::fs::write(dir.join("game.exe"), b"exe").unwrap();
        std::fs::write(dir.join("Data/sub/a.pak"), b"pak").unwrap();
        std::fs::write(dir.join("download/archive.zip"), b"zip").unwrap();
        let ledger = Ledger::new(None);
        let skip = dir.join("download");
        let adopted = ledger.adopt_tree(&dir, &[skip.as_path()]);
        ledger.adopt_file(dir.join("gone.bin"));
        let pending = ledger.has_pending();
        let sealed = ledger.seal().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(adopted, 2);
        assert!(pending);
        assert!(sealed.is_ok(), "a file that vanished is not a failure");
        assert!(!ledger.has_pending());
    }

    #[tokio::test]
    async fn a_failed_rename_keeps_the_part_and_refuses_the_seal() {
        let dir = crate::testing::scratch_dir("ledger-fail");
        let dest = dir.join("missing-dir").join("a.pak");
        let part = dir.join("a.pak.part");
        std::fs::write(&part, b"new").unwrap();
        let ledger = Ledger::new(None);
        ledger.complete_part(None, part.clone(), dest, false, || {});
        let sealed = ledger.seal().await;
        let kept = part.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(sealed.is_err());
        assert!(kept);
    }
}
