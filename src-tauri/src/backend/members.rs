// ------------ Zip Members ------------
// Files that stay inside zip archives on a server, whole or split: the archive's index (central directory, zip64
// included) and each wanted file's bytes are read over HTTP range requests and inflated straight into the install by a
// few workers that retry what fails. Endfield and the Girls' Frontline 2 client are installed, updated and repaired this way.
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use peebify_transfer::control::RunGate;
use peebify_transfer::fsx::ContentHasher;
use peebify_transfer::progress::ProgressTracker;
use peebify_transfer::{JobHooks, Ledger, Sealed};
use tokio::runtime::Handle;

use super::engine::BeforeWrite;
use super::fs_util::{manifest_key, safe_join};
use super::manifest::{Entry, Hash, MemberLoc, Pack, Source};

const REMOTE_PROBE_CHUNK: u64 = 256 * 1024;
const RANGE_RETRIES: u32 = 3;
const RANGE_RETRY_DELAY_MS: u64 = 1500;
const IO_BUF: usize = 1024 * 1024;

// ------------ Reading Packs ------------
// A reader that makes the split packs look like one big file: it streams the same bytes over HTTP range requests, with
// retries and waiting out network outages, on the job's own pause, hold and cancel.
struct HttpRangeReader {
    reader: peebify_transfer::RangeReader,
    job: Arc<dyn JobHooks>,
    handle: Handle,
    packs: Arc<[Pack]>,
    starts: Vec<u64>,
    total: u64,
    pos: u64,
    buf: bytes::Bytes,
    buf_start: u64,
    stream: Option<RangeStream>,
    limit: Option<u64>,
}

pub(super) struct Detached {
    gate: RunGate,
    tracker: ProgressTracker,
}

impl Detached {
    pub(super) fn job() -> Arc<dyn JobHooks> {
        Arc::new(Self { gate: RunGate::new(), tracker: ProgressTracker::new() })
    }
}

impl JobHooks for Detached {
    fn gate(&self) -> &RunGate {
        &self.gate
    }

    fn tracker(&self) -> &ProgressTracker {
        &self.tracker
    }

    fn install_dir(&self) -> &Path {
        Path::new("")
    }

    fn reaches_home(&self) -> futures::future::BoxFuture<'static, bool> {
        Box::pin(super::http::reaches_home())
    }
}

const STREAM_BATCH: usize = 1024 * 1024;
const STREAM_AHEAD: usize = 8;
const CANCEL_POLL: Duration = Duration::from_millis(250);

type StreamItem = Result<bytes::Bytes, String>;

struct RangeStream {
    start: u64,
    next: u64,
    end: u64,
    rx: tokio::sync::mpsc::Receiver<StreamItem>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for RangeStream {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

enum RangeFailure {
    Fatal(io::Error),
    Gone(String),
    Retry(String),
}

trait BoundedRead: Read + Seek {
    fn bound(&mut self, _end: Option<u64>) {}
}

impl BoundedRead for HttpRangeReader {
    fn bound(&mut self, end: Option<u64>) {
        self.limit = end;
    }
}

fn range_end(pos: u64, chunk: u64, total: u64, limit: Option<u64>) -> u64 {
    let end = (pos + chunk).min(total);
    match limit {
        Some(limit) => end.min(limit).max(pos + 1).min(total),
        None => end,
    }
}

impl HttpRangeReader {
    fn open(packs: Arc<[Pack]>, job: Arc<dyn JobHooks>, handle: Handle) -> Result<Self, String> {
        if packs.is_empty() {
            return Err("no pack URLs to read from".to_string());
        }
        let mut starts = Vec::with_capacity(packs.len());
        let mut total = 0u64;
        for pack in packs.iter() {
            starts.push(total);
            total += pack.size;
        }
        Ok(Self {
            reader: peebify_transfer::RangeReader::new(super::http::shared_transfer()),
            job,
            handle,
            packs,
            starts,
            total,
            pos: 0,
            buf: bytes::Bytes::new(),
            buf_start: 0,
            stream: None,
            limit: None,
        })
    }

    fn locate(&self, pos: u64) -> usize {
        match self.starts.binary_search(&pos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    fn with_retries<T>(
        &self,
        mut attempt_once: impl FnMut() -> Result<T, RangeFailure>,
    ) -> io::Result<T> {
        let mut last_err = String::new();
        for attempt in 1..=RANGE_RETRIES {
            if self.job.gate().is_cancelled() {
                return Err(io::Error::other("cancelled"));
            }
            match attempt_once() {
                Ok(value) => return Ok(value),
                Err(RangeFailure::Fatal(e)) => return Err(e),
                Err(RangeFailure::Gone(message)) => return Err(io::Error::other(message)),
                Err(RangeFailure::Retry(message)) => last_err = message,
            }
            if attempt < RANGE_RETRIES {
                std::thread::sleep(Duration::from_millis(RANGE_RETRY_DELAY_MS * u64::from(attempt)));
            }
        }
        Err(io::Error::other(last_err))
    }

    fn failure(&self, error: String) -> RangeFailure {
        if self.job.gate().is_cancelled() {
            return RangeFailure::Fatal(io::Error::other("cancelled"));
        }
        if error == peebify_transfer::ranges::RANGE_IGNORED {
            return RangeFailure::Fatal(io::Error::other(error));
        }
        if super::http::permanent_client_status(&error).is_some() {
            return RangeFailure::Gone(error);
        }
        RangeFailure::Retry(error)
    }

    fn fetch_range(&self, part: usize, offset: u64, len: u64) -> io::Result<bytes::Bytes> {
        let url = &self.packs[part].url;
        self.with_retries(|| {
            self.handle
                .block_on(self.reader.fetch(&*self.job, url, offset, offset + len))
                .map_err(|e| self.failure(e))
        })
    }

    fn open_stream(&self, part: usize, start: u64, end: u64) -> Result<RangeStream, RangeFailure> {
        let offset = start - self.starts[part];
        let mut body = self
            .handle
            .block_on(self.reader.open(&*self.job, &self.packs[part].url, offset, offset + (end - start)))
            .map_err(|e| self.failure(e))?;
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamItem>(STREAM_AHEAD);
        let pump = self.handle.spawn(async move {
            let mut batch = bytes::BytesMut::new();
            loop {
                match body.next().await {
                    Ok(Some(chunk)) => {
                        let ready = if batch.is_empty() && chunk.len() >= STREAM_BATCH {
                            chunk
                        } else {
                            batch.extend_from_slice(&chunk);
                            if batch.len() < STREAM_BATCH {
                                continue;
                            }
                            batch.split().freeze()
                        };
                        if tx.send(Ok(ready)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        if !batch.is_empty() {
                            let _ = tx.send(Ok(batch.split().freeze())).await;
                        }
                        return;
                    }
                    Err(e) => {
                        if !batch.is_empty() && tx.send(Ok(batch.split().freeze())).await.is_err() {
                            return;
                        }
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        Ok(RangeStream {
            start,
            next: start,
            end,
            rx,
            pump,
        })
    }

    fn next_batch(&self, stream: &mut RangeStream) -> Result<bytes::Bytes, RangeFailure> {
        let gate = self.job.gate();
        let rx = &mut stream.rx;
        let item = self.handle.block_on(async {
            loop {
                match tokio::time::timeout(CANCEL_POLL, rx.recv()).await {
                    Ok(item) => return Some(item),
                    Err(_) if gate.is_cancelled() => return None,
                    Err(_) => {}
                }
            }
        });
        match item {
            None => Err(RangeFailure::Fatal(io::Error::other("cancelled"))),
            Some(Some(Ok(bytes))) => Ok(bytes),
            Some(Some(Err(message))) => Err(self.failure(message)),
            Some(None) => Err(RangeFailure::Retry(format!(
                "short range response ({} of {} bytes)",
                stream.next - stream.start,
                stream.end - stream.start
            ))),
        }
    }

    fn stream_fill(&mut self, pos: u64) -> io::Result<()> {
        let idx = self.locate(pos);
        let part_end = self.starts[idx] + self.packs[idx].size;
        let end = range_end(pos, self.total - pos, self.total, self.limit).min(part_end);
        let mut stream = self
            .stream
            .take()
            .filter(|s| s.next == pos && s.next < s.end);
        let (mut live, bytes) = self.with_retries(|| {
            let mut live = match stream.take() {
                Some(live) => live,
                None => self.open_stream(idx, pos, end)?,
            };
            let bytes = self.next_batch(&mut live)?;
            Ok((live, bytes))
        })?;
        live.next += bytes.len() as u64;
        if live.next < live.end {
            self.stream = Some(live);
        }
        self.buf = bytes;
        self.buf_start = pos;
        Ok(())
    }

    fn fill(&mut self, pos: u64) -> io::Result<()> {
        let sequential = !self.buf.is_empty() && pos == self.buf_start + self.buf.len() as u64;
        if sequential {
            return self.stream_fill(pos);
        }
        self.stream = None;
        let end = range_end(pos, REMOTE_PROBE_CHUNK, self.total, self.limit);
        let mut pieces: Vec<bytes::Bytes> = Vec::with_capacity(1);
        let mut cursor = pos;
        while cursor < end {
            let idx = self.locate(cursor);
            let part_end = self.starts[idx] + self.packs[idx].size;
            let want = end.min(part_end) - cursor;
            let local_off = cursor - self.starts[idx];
            let bytes = self.fetch_range(idx, local_off, want)?;
            cursor += bytes.len() as u64;
            pieces.push(bytes);
        }
        self.buf = match pieces.len() {
            1 => pieces.pop().unwrap_or_default(),
            _ => bytes::Bytes::from(pieces.concat()),
        };
        self.buf_start = pos;
        Ok(())
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.total || out.is_empty() {
            return Ok(0);
        }
        let in_buffer =
            self.pos >= self.buf_start && self.pos < self.buf_start + self.buf.len() as u64;
        if !in_buffer {
            self.fill(self.pos)?;
        }
        let offset = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf.len() - offset);
        out[..n].copy_from_slice(&self.buf[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = resolve_seek(self.pos, self.total, from)?;
        Ok(self.pos)
    }
}

fn resolve_seek(pos: u64, total: u64, from: SeekFrom) -> io::Result<u64> {
    let target: i128 = match from {
        SeekFrom::Start(offset) => offset as i128,
        SeekFrom::End(offset) => total as i128 + offset as i128,
        SeekFrom::Current(offset) => pos as i128 + offset as i128,
    };
    if target < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek before start",
        ));
    }
    Ok(target as u64)
}

// ------------ Zip Index Parsing ------------
// Reads the zip central directory (including zip64) to list every file in the packs, with its size and checksum.
#[derive(Debug, Clone)]
struct CdEntry {
    path: String,
    size: u64,
    csize: u64,
    crc32: u32,
    method: u16,
    flags: u16,
    header_start: u64,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_LOCATOR_SIG: u32 = 0x0706_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const CD_ENTRY_SIG: u32 = 0x0201_4b50;
const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;

const MAX_CENTRAL_DIRECTORY: u64 = 256 * 1024 * 1024;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const METHOD_DEFLATE64: u16 = 9;

fn rd_u16(buf: &[u8], off: usize) -> Option<u64> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?) as u64)
}
fn rd_u32(buf: &[u8], off: usize) -> Option<u64> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?) as u64)
}
fn rd_u64(buf: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?))
}

fn read_central_directory<R: Read + Seek>(reader: &mut R) -> Result<Vec<CdEntry>, String> {
    let err = |what: &str| format!("central directory parse failed: {what}");
    let total = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| err(&format!("seek to end: {e}")))?;

    let window = total.min(22 + 65_535);
    reader
        .seek(SeekFrom::Start(total - window))
        .map_err(|e| err(&format!("seek to tail: {e}")))?;
    let mut tail = vec![0u8; window as usize];
    reader
        .read_exact(&mut tail)
        .map_err(|e| err(&format!("read tail: {e}")))?;

    let mut eocd = None;
    for i in (0..tail.len().saturating_sub(21)).rev() {
        if rd_u32(&tail, i) != Some(EOCD_SIG as u64) {
            continue;
        }
        let comment_len = rd_u16(&tail, i + 20).unwrap_or(0) as usize;
        if i + 22 + comment_len == tail.len() {
            eocd = Some(i);
            break;
        }
        eocd.get_or_insert(i);
    }
    let eocd = eocd.ok_or_else(|| err("no end-of-central-directory signature"))?;

    let mut cd_count = rd_u16(&tail, eocd + 10).ok_or_else(|| err("truncated EOCD"))?;
    let mut cd_size = rd_u32(&tail, eocd + 12).ok_or_else(|| err("truncated EOCD"))?;
    let mut cd_offset = rd_u32(&tail, eocd + 16).ok_or_else(|| err("truncated EOCD"))?;

    if cd_count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        let loc = eocd
            .checked_sub(20)
            .ok_or_else(|| err("zip64 locator out of range"))?;
        if rd_u32(&tail, loc) != Some(EOCD64_LOCATOR_SIG as u64) {
            return Err(err("zip64 locator signature missing"));
        }
        let eocd64_pos = rd_u64(&tail, loc + 8).ok_or_else(|| err("truncated zip64 locator"))?;
        reader
            .seek(SeekFrom::Start(eocd64_pos))
            .map_err(|e| err(&format!("seek to zip64 EOCD: {e}")))?;
        let mut rec = [0u8; 56];
        reader
            .read_exact(&mut rec)
            .map_err(|e| err(&format!("read zip64 EOCD: {e}")))?;
        if rd_u32(&rec, 0) != Some(EOCD64_SIG as u64) {
            return Err(err("bad zip64 EOCD signature"));
        }
        cd_count = rd_u64(&rec, 32).ok_or_else(|| err("truncated zip64 EOCD"))?;
        cd_size = rd_u64(&rec, 40).ok_or_else(|| err("truncated zip64 EOCD"))?;
        cd_offset = rd_u64(&rec, 48).ok_or_else(|| err("truncated zip64 EOCD"))?;
    }

    if cd_size > MAX_CENTRAL_DIRECTORY {
        return Err(err(&format!(
            "central directory declares {cd_size} bytes, over the {MAX_CENTRAL_DIRECTORY}-byte limit"
        )));
    }
    if cd_offset.checked_add(cd_size).map(|end| end > total) != Some(false) {
        return Err(err("central directory extends past the archive"));
    }
    reader
        .seek(SeekFrom::Start(cd_offset))
        .map_err(|e| err(&format!("seek to CD: {e}")))?;
    let mut cd = vec![0u8; cd_size as usize];
    reader
        .read_exact(&mut cd)
        .map_err(|e| err(&format!("read CD: {e}")))?;

    let mut entries = Vec::with_capacity(cd_count.min(4_000_000) as usize);
    let mut records = 0u64;
    let mut pos = 0usize;
    while records < cd_count && pos + 46 <= cd.len() {
        if rd_u32(&cd, pos) != Some(CD_ENTRY_SIG as u64) {
            break;
        }
        records += 1;
        let flags = rd_u16(&cd, pos + 8).ok_or_else(|| err("truncated entry"))? as u16;
        let method = rd_u16(&cd, pos + 10).ok_or_else(|| err("truncated entry"))? as u16;
        let crc32 = rd_u32(&cd, pos + 16).ok_or_else(|| err("truncated entry"))? as u32;
        let mut csize = rd_u32(&cd, pos + 20).ok_or_else(|| err("truncated entry"))?;
        let mut size = rd_u32(&cd, pos + 24).ok_or_else(|| err("truncated entry"))?;
        let name_len = rd_u16(&cd, pos + 28).ok_or_else(|| err("truncated entry"))? as usize;
        let extra_len = rd_u16(&cd, pos + 30).ok_or_else(|| err("truncated entry"))? as usize;
        let comment_len = rd_u16(&cd, pos + 32).ok_or_else(|| err("truncated entry"))? as usize;
        let mut header_start = rd_u32(&cd, pos + 42).ok_or_else(|| err("truncated entry"))?;

        let name_start = pos + 46;
        let record_end = name_start + name_len + extra_len + comment_len;
        if record_end > cd.len() {
            return Err(err("entry record extends past the CD"));
        }
        let path =
            String::from_utf8_lossy(&cd[name_start..name_start + name_len]).replace('\\', "/");

        if size == 0xFFFF_FFFF || csize == 0xFFFF_FFFF || header_start == 0xFFFF_FFFF {
            let mut ep = name_start + name_len;
            let extra_end = ep + extra_len;
            while ep + 4 <= extra_end {
                let id = rd_u16(&cd, ep).ok_or_else(|| err("truncated extra"))?;
                let len = rd_u16(&cd, ep + 2).ok_or_else(|| err("truncated extra"))? as usize;
                if id == 0x0001 {
                    let mut fp = ep + 4;
                    if size == 0xFFFF_FFFF {
                        size = rd_u64(&cd, fp).ok_or_else(|| err("truncated zip64 extra"))?;
                        fp += 8;
                    }
                    if csize == 0xFFFF_FFFF {
                        csize = rd_u64(&cd, fp).ok_or_else(|| err("truncated zip64 extra"))?;
                        fp += 8;
                    }
                    if header_start == 0xFFFF_FFFF {
                        header_start =
                            rd_u64(&cd, fp).ok_or_else(|| err("truncated zip64 extra"))?;
                    }
                    break;
                }
                ep += 4 + len;
            }
        }

        pos = record_end;
        if path.ends_with('/') {
            continue;
        }
        entries.push(CdEntry {
            path,
            size,
            csize,
            crc32,
            method,
            flags,
            header_start,
        });
    }
    if records != cd_count {
        return Err(err(&format!(
            "truncated, parsed {records} of {cd_count} records"
        )));
    }
    if entries.is_empty() {
        return Err(err("no file entries found"));
    }
    Ok(entries)
}

struct CountingReader<R> {
    inner: R,
    count: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

// ------------ Extracting Files ------------
// Inflates one member of an archive into the install, checking its CRC32 as it goes, and the single file of a small zip.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Extracted {
    written: u64,
    downloaded: u64,
}

fn entry_reader<'r, R: BoundedRead>(
    reader: &'r mut R,
    entry: &CdEntry,
) -> Result<(Box<dyn Read + 'r>, std::rc::Rc<std::cell::Cell<u64>>), String> {
    if entry.flags & 0x1 != 0 {
        return Err(format!("'{}' is encrypted, which is unsupported", entry.path));
    }
    reader.bound(Some(
        entry
            .header_start
            .saturating_add(30 + 2 * 65_535)
            .saturating_add(entry.csize),
    ));
    reader
        .seek(SeekFrom::Start(entry.header_start))
        .map_err(|e| format!("seek to '{}': {e}", entry.path))?;
    let mut header = [0u8; 30];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("read local header of '{}': {e}", entry.path))?;
    if rd_u32(&header, 0) != Some(LOCAL_HEADER_SIG as u64) {
        return Err(format!("bad local header signature for '{}'", entry.path));
    }
    let name_len = rd_u16(&header, 26).expect("fixed-size header");
    let extra_len = rd_u16(&header, 28).expect("fixed-size header");
    reader.bound(Some(
        entry
            .header_start
            .saturating_add(30 + name_len + extra_len)
            .saturating_add(entry.csize),
    ));
    reader
        .seek(SeekFrom::Current((name_len + extra_len) as i64))
        .map_err(|e| format!("seek past header of '{}': {e}", entry.path))?;

    let consumed = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let bounded = CountingReader {
        inner: reader.by_ref().take(entry.csize),
        count: std::rc::Rc::clone(&consumed),
    };
    let decoder: Box<dyn Read + 'r> = match entry.method {
        METHOD_STORED => Box::new(bounded),
        METHOD_DEFLATE => Box::new(flate2::read::DeflateDecoder::new(bounded)),
        METHOD_DEFLATE64 => Box::new(deflate64::Deflate64Decoder::new(bounded)),
        other => {
            return Err(format!(
                "'{}' uses unsupported compression method {other}",
                entry.path
            ))
        }
    };
    Ok((decoder, consumed))
}

fn extract_one<R: BoundedRead>(
    reader: &mut R,
    entry: &CdEntry,
    install_dir: &Path,
    gate: &RunGate,
    progress: &dyn Fn(u64),
    wrote: &dyn Fn(u64),
    ledger: Option<&Ledger>,
) -> Result<Extracted, String> {
    let final_path = safe_join(install_dir, &entry.path)?;
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            super::fs_util::fmt_io(&format!("Could not create {}", parent.display()), &e)
        })?;
    }
    let tmp_path = {
        let mut name = final_path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.peebify-part", entry.header_start));
        final_path.with_file_name(name)
    };
    let (mut decoder, consumed) = entry_reader(reader, entry)?;

    let result = (|| -> Result<(u64, std::fs::File), String> {
        let mut out = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("failed to create {}: {e}", tmp_path.display()))?;
        let expected = Hash::Crc32(entry.crc32).expected();
        let mut hasher = ContentHasher::for_expected(&expected);
        let mut buf = vec![0u8; IO_BUF];
        let mut written = 0u64;
        let mut reported = 0u64;
        loop {
            checkpoint(gate)?;
            let n = decoder
                .read(&mut buf)
                .map_err(|e| format!("stream error for '{}': {e}", entry.path))?;
            let now = consumed.get();
            if now > reported {
                progress(now - reported);
                reported = now;
            }
            if n == 0 {
                break;
            }
            if written.saturating_add(n as u64) > entry.size {
                return Err(format!(
                    "'{}' decompressed to more than the {} bytes it declares",
                    entry.path, entry.size
                ));
            }
            hasher.update(&buf[..n]);
            io::Write::write_all(&mut out, &buf[..n])
                .map_err(|e| format!("write error for '{}': {e}", entry.path))?;
            wrote(n as u64);
            written += n as u64;
        }
        io::Write::flush(&mut out).map_err(|e| e.to_string())?;
        if written != entry.size {
            return Err(format!(
                "'{}' decompressed to {written} bytes, expected {}",
                entry.path, entry.size
            ));
        }
        if hasher.finish() != expected {
            return Err(format!("checksum mismatch after fetching '{}'", entry.path));
        }
        Ok((written, out))
    })();

    let (written, out) = match result {
        Ok(done) => done,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    };

    match ledger {
        Some(ledger) => ledger.complete_part(Some(Arc::new(out)), tmp_path, final_path, true, || {}),
        None => {
            drop(out);
            super::fs_util::finalize_replace(&tmp_path, &final_path)
                .map_err(|e| format!("failed to finalize {}: {e}", final_path.display()))?;
        }
    }
    Ok(Extracted {
        written,
        downloaded: consumed.get(),
    })
}

fn checkpoint(gate: &RunGate) -> Result<(), String> {
    while gate.is_stopped() && !gate.is_cancelled() {
        std::thread::sleep(Duration::from_millis(200));
    }
    match gate.is_cancelled() {
        true => Err(super::fs_util::CANCELLED_MSG.to_string()),
        false => Ok(()),
    }
}

impl BoundedRead for std::fs::File {}
impl BoundedRead for io::Cursor<Vec<u8>> {}

pub(super) fn read_member(archive: Vec<u8>, name: &str, max: u64) -> Result<Vec<u8>, String> {
    let mut reader = io::Cursor::new(archive);
    let entry = read_central_directory(&mut reader)?
        .into_iter()
        .find(|e| e.path == name)
        .ok_or_else(|| format!("zip archive has no {name}"))?;
    let mut out = Vec::new();
    let (decoder, _) = entry_reader(&mut reader, &entry)?;
    decoder.take(max).read_to_end(&mut out).map_err(|e| format!("{name} could not be unpacked: {e}"))?;
    Ok(out)
}

pub(super) fn extract_member(
    zip: &Path,
    dest: &str,
    install_dir: &Path,
    gate: &RunGate,
    ledger: &Ledger,
) -> Result<(), String> {
    let mut file = std::fs::File::open(zip)
        .map_err(|e| super::fs_util::fmt_io(&format!("Could not read {}", zip.display()), &e))?;
    let mut members = read_central_directory(&mut file)?;
    let (Some(mut entry), 0) = (members.pop(), members.len()) else {
        return Err(format!("{} should hold exactly one file", zip.display()));
    };
    entry.path = dest.to_string();
    extract_one(&mut file, &entry, install_dir, gate, &|_| {}, &|_| {}, Some(ledger)).map(|_| ())
}

// ------------ Member Executor ------------
// Fetches manifest entries that are members of an archive on a server: each is located in the archive's index, then
// inflated by one of a few workers, with its checksum checked as it is written. A worker that fails an entry requeues it.

pub(super) const MAX_EXTRACT_WORKERS: usize = 8;

fn extract_workers() -> usize {
    if super::http::shared_transfer().is_playing() {
        return 2;
    }
    peebify_transfer::cores().clamp(2, MAX_EXTRACT_WORKERS)
}
const ENTRY_REQUEUES: u32 = 2;
const ENTRY_RETRY_BASE_MS: u64 = 2000;

fn entry_retry_allowed(message: &str, attempt: u32) -> bool {
    use super::fs_util::FailureKind;

    if attempt >= ENTRY_REQUEUES {
        return false;
    }
    let msg = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| msg.contains(n));
    if has(&[
        "http 403",
        "http 404",
        "http error: 403",
        "http error: 404",
        "http error: 410",
        "ignored the range request",
        "bad local header signature",
        "unsupported compression",
        "is encrypted",
        "failed to finalize",
        "failed to create",
        "write error",
    ]) {
        return false;
    }
    let kind = super::fs_util::classify(message);
    if matches!(
        kind,
        FailureKind::Cancelled
            | FailureKind::DiskFull
            | FailureKind::Locked
            | FailureKind::AccessDenied
    ) {
        return false;
    }
    if has(&["checksum mismatch", "decompressed to"]) {
        return attempt == 0;
    }
    matches!(kind, FailureKind::Network)
        || has(&[
            "http 5",
            "http error: 5",
            "short range response",
            "range body error",
            "range request failed",
        ])
}

fn wait_before_retry(delay: Duration, gate: &RunGate) {
    let deadline = Instant::now() + delay;
    while !gate.is_cancelled() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(200)));
    }
}

fn remote_is_gone(message: &str) -> bool {
    message.contains("no pack URLs")
        || [403u16, 404, 410].iter().any(|status| {
            message.contains(&format!("HTTP {status} for "))
                || message.contains(&format!("HTTP Error: {status} for "))
        })
}

fn remote_unreachable(message: &str) -> bool {
    let error_status = message.match_indices("HTTP ").any(|(at, _)| {
        let rest = &message.as_bytes()[at + 5..];
        rest.len() >= 8
            && rest[..3].iter().all(u8::is_ascii_digit)
            && rest[3..].starts_with(b" for ")
    });
    error_status
        || message.contains("HTTP Error: ")
        || message.contains("Request error")
        || message.contains("Stream error")
        || message.contains("stalled")
        || message.contains("range request failed")
        || message.contains("range body error")
        || message.contains("cancelled")
}

fn index_failure(error: &str) -> String {
    if remote_is_gone(error) {
        format!("The packages are no longer hosted ({error}). Update the game, then repair.")
    } else if remote_unreachable(error) {
        format!("The download server could not be reached ({error}). Check your connection and try again.")
    } else {
        format!("The download server returned an unusable package index ({error}). Try again later.")
    }
}

pub(super) fn is_structural_failure(message: &str) -> bool {
    let msg = message.to_lowercase();
    [
        "central directory parse failed",
        "no pack urls",
        "ignored the range request",
        "bad local header signature",
        "unsupported compression",
        "is encrypted",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

fn disk_full_advice(install_dir: &Path, message: String) -> String {
    if !super::fs_util::is_disk_full(&message) || message.starts_with("Not enough disk space") {
        return message;
    }
    format!(
        "Not enough disk space in {}. Free up space and try again. ({message})",
        install_dir.display()
    )
}

impl CdEntry {
    fn member(self, packs: &Arc<[Pack]>) -> Entry {
        let at = MemberLoc { csize: self.csize, header_start: self.header_start, method: self.method, flags: self.flags };
        Entry {
            path: self.path.into(),
            size: self.size,
            hash: Hash::Crc32(self.crc32),
            source: Source::Member { packs: Arc::clone(packs), at: Some(at) },
            live: false,
        }
    }
}

pub(super) async fn listing(packs: &Arc<[Pack]>, job: &Arc<dyn JobHooks>) -> Result<Vec<Entry>, String> {
    let (packs, job, handle) = (Arc::clone(packs), Arc::clone(job), Handle::current());
    tauri::async_runtime::spawn_blocking(move || {
        let mut reader = HttpRangeReader::open(Arc::clone(&packs), job, handle)?;
        Ok(read_central_directory(&mut reader)?.into_iter().map(|e| e.member(&packs)).collect())
    })
    .await
    .map_err(|e| format!("package index task panicked: {e}"))?
}

fn locate(files: &[Entry], packs: &Arc<[Pack]>, job: &Arc<dyn JobHooks>, handle: &Handle) -> Result<Vec<CdEntry>, String> {
    let mut index: Option<HashMap<String, CdEntry>> = None;
    let mut wanted = Vec::with_capacity(files.len());
    for file in files {
        if let (Source::Member { at: Some(at), .. }, Hash::Crc32(crc32)) = (&file.source, file.hash) {
            wanted.push(CdEntry {
                path: file.path.to_string(),
                size: file.size,
                csize: at.csize,
                crc32,
                method: at.method,
                flags: at.flags,
                header_start: at.header_start,
            });
            continue;
        }
        if index.is_none() {
            let mut reader = HttpRangeReader::open(Arc::clone(packs), Arc::clone(job), handle.clone())?;
            let listed = read_central_directory(&mut reader).map_err(|e| index_failure(&e))?;
            index = Some(listed.into_iter().map(|e| (manifest_key(&e.path), e)).collect());
        }
        let mut found = index
            .as_ref()
            .and_then(|index| index.get(&manifest_key(&file.path)))
            .cloned()
            .ok_or_else(|| format!("{} is not in the hosted packages. Update the game, then repair.", file.path))?;
        found.path = file.path.to_string();
        wanted.push(found);
    }
    Ok(wanted)
}

pub(super) async fn fetch_members(
    install_dir: &Path,
    files: &[Entry],
    job: Arc<dyn JobHooks>,
    before_write: BeforeWrite,
) -> Result<Sealed, String> {
    let Some(Source::Member { packs, .. }) = files.first().map(|f| &f.source) else {
        return Ok(Sealed::nothing_written());
    };
    let (packs, files, dir, handle) = (Arc::clone(packs), files.to_vec(), install_dir.to_path_buf(), Handle::current());
    let target = dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let wanted = locate(&files, &packs, &job, &handle)?;
        extract_entries_parallel(&packs, wanted, &dir, &job, before_write, &handle)
    })
    .await
    .map_err(|e| format!("unpack task panicked: {e}"))?
    .map_err(|e| disk_full_advice(&target, e))
}

fn extract_entries_parallel(
    packs: &Arc<[Pack]>,
    mut wanted: Vec<CdEntry>,
    install_dir: &Path,
    job: &Arc<dyn JobHooks>,
    before_write: BeforeWrite,
    handle: &Handle,
) -> Result<Sealed, String> {
    wanted.sort_by_key(|e| e.header_start);
    let (gate, tracker) = (job.gate(), job.tracker());
    let workers = extract_workers().min(wanted.len()).max(1);
    let queue: parking_lot::Mutex<std::collections::VecDeque<(CdEntry, u32)>> =
        parking_lot::Mutex::new(wanted.into_iter().map(|entry| (entry, 0)).collect());
    let written = AtomicU64::new(0);
    let downloaded = AtomicU64::new(0);
    let first_error: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let ledger = Ledger::new(before_write);
    let checkpoints = handle.spawn(
        Arc::clone(&ledger).run(super::http::shared_transfer().config.checkpoint_interval),
    );

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let open = || HttpRangeReader::open(Arc::clone(packs), Arc::clone(job), handle.clone());
                let fail = |e: String| {
                    stop.store(true, Ordering::SeqCst);
                    let mut slot = first_error.lock();
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                };
                let mut reader = match open() {
                    Ok(reader) => reader,
                    Err(e) => return fail(e),
                };
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Some((entry, attempt)) = queue.lock().pop_front() else {
                        return;
                    };
                    let progress = |delta: u64| {
                        tracker.update_file_progress(&entry.path, delta as f64, false);
                        tracker.add_network(delta);
                    };
                    let wrote = |delta: u64| tracker.add_disk(delta);
                    let outcome = checkpoint(gate).and_then(|()| {
                        extract_one(&mut reader, &entry, install_dir, gate, &progress, &wrote, Some(&ledger))
                    });
                    match outcome {
                        Ok(extracted) => {
                            written.fetch_add(extracted.written, Ordering::SeqCst);
                            downloaded.fetch_add(extracted.downloaded, Ordering::SeqCst);
                            tracker.file_done(&entry.path);
                            job.verified(&entry.path);
                        }
                        Err(e) if !gate.is_cancelled() && entry_retry_allowed(&e, attempt) => {
                            log::warn!(
                                "Fetching '{}' failed ({e}), retry {}/{ENTRY_REQUEUES}",
                                entry.path,
                                attempt + 1
                            );
                            wait_before_retry(super::http::retry_delay(attempt + 1, ENTRY_RETRY_BASE_MS), gate);
                            reader = match open() {
                                Ok(reader) => reader,
                                Err(e) => return fail(e),
                            };
                            queue.lock().push_back((entry, attempt + 1));
                        }
                        Err(e) => return fail(e),
                    }
                }
            });
        }
    });

    checkpoints.abort();
    let sealed = handle.block_on(ledger.seal());
    if let Some(e) = first_error.into_inner() {
        return Err(e);
    }
    log::info!(
        "Unpacked {:.2} GB from {:.2} GB read.",
        peebify_transfer::progress::gib(written.into_inner() as f64),
        peebify_transfer::progress::gib(downloaded.into_inner() as f64)
    );
    sealed.map_err(|e| format!("failed to finalize the extracted files: {e}"))
}

// ------------ Tests ------------
// The zip index and member readers, the range reader across split packs, retry rules, and fetching members by range.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;
    use crate::backend::validator::{FileCheck, FileValidator};
    use io::Cursor;
    use std::sync::atomic::AtomicBool;

    fn put16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn put32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn put64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn build_zip(items: &[(&str, &[u8])], zip64: bool, comment: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in items {
            let offset = out.len() as u32;
            let crc = crc32fast::hash(data);
            put32(&mut out, LOCAL_HEADER_SIG);
            put16(&mut out, 20);
            put16(&mut out, 0);
            put16(&mut out, METHOD_STORED);
            put32(&mut out, 0);
            put32(&mut out, crc);
            put32(&mut out, data.len() as u32);
            put32(&mut out, data.len() as u32);
            put16(&mut out, name.len() as u16);
            put16(&mut out, 0);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            put32(&mut central, CD_ENTRY_SIG);
            put16(&mut central, 20);
            put16(&mut central, 20);
            put16(&mut central, 0);
            put16(&mut central, METHOD_STORED);
            put32(&mut central, 0);
            put32(&mut central, crc);
            put32(&mut central, data.len() as u32);
            put32(&mut central, data.len() as u32);
            put16(&mut central, name.len() as u16);
            put16(&mut central, 0);
            put16(&mut central, 0);
            put16(&mut central, 0);
            put16(&mut central, 0);
            put32(&mut central, 0);
            put32(&mut central, offset);
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u64;
        let cd_size = central.len() as u64;
        out.extend_from_slice(&central);
        if zip64 {
            let eocd64_pos = out.len() as u64;
            put32(&mut out, EOCD64_SIG);
            put64(&mut out, 44);
            put16(&mut out, 45);
            put16(&mut out, 45);
            put32(&mut out, 0);
            put32(&mut out, 0);
            put64(&mut out, items.len() as u64);
            put64(&mut out, items.len() as u64);
            put64(&mut out, cd_size);
            put64(&mut out, cd_offset);
            put32(&mut out, EOCD64_LOCATOR_SIG);
            put32(&mut out, 0);
            put64(&mut out, eocd64_pos);
            put32(&mut out, 1);
        }
        put32(&mut out, EOCD_SIG);
        put16(&mut out, 0);
        put16(&mut out, 0);
        let count = if zip64 { 0xFFFF } else { items.len() as u16 };
        put16(&mut out, count);
        put16(&mut out, count);
        put32(&mut out, if zip64 { 0xFFFF_FFFF } else { cd_size as u32 });
        put32(&mut out, if zip64 { 0xFFFF_FFFF } else { cd_offset as u32 });
        put16(&mut out, comment.len() as u16);
        out.extend_from_slice(comment);
        out
    }

    fn sample() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("Data/", b""),
            ("Data/a.bin", b"alpha contents"),
            ("Data/b.bin", b"bravo"),
            ("root.txt", b"charlie charlie charlie"),
        ]
    }

    #[test]
    fn central_directory_lists_files_and_skips_directories() {
        let zip = build_zip(&sample(), false, b"");
        let entries = read_central_directory(&mut Cursor::new(zip)).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["Data/a.bin", "Data/b.bin", "root.txt"]);
        assert_eq!(entries[0].size, 14);
        assert_eq!(entries[0].crc32, crc32fast::hash(b"alpha contents"));
    }

    #[test]
    fn central_directory_reads_zip64_locator() {
        let zip = build_zip(&sample(), true, b"");
        let entries = read_central_directory(&mut Cursor::new(zip)).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].path, "root.txt");
    }

    #[test]
    fn central_directory_rejects_a_corrupt_record() {
        let mut zip = build_zip(&sample(), false, b"");
        let cd_start = zip
            .windows(4)
            .position(|w| w == CD_ENTRY_SIG.to_le_bytes())
            .unwrap();
        let second = cd_start + 46 + "Data/".len();
        assert_eq!(&zip[second..second + 4], &CD_ENTRY_SIG.to_le_bytes());
        zip[second] = 0;
        let err = read_central_directory(&mut Cursor::new(zip)).unwrap_err();
        assert!(err.contains("parsed 1 of 4 records"), "{err}");
    }

    #[test]
    fn central_directory_rejects_an_oversized_size() {
        let mut zip = build_zip(&sample(), false, b"");
        let size_at = zip.len() - 22 + 12;
        zip[size_at..size_at + 4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
        let err = read_central_directory(&mut Cursor::new(zip)).unwrap_err();
        assert!(err.contains("over the"), "{err}");
    }

    #[test]
    fn central_directory_ignores_signature_inside_comment() {
        let mut comment = b"note ".to_vec();
        comment.extend_from_slice(&EOCD_SIG.to_le_bytes());
        comment.extend_from_slice(&[0xAB; 30]);
        let zip = build_zip(&sample(), false, &comment);
        let entries = read_central_directory(&mut Cursor::new(zip)).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn resolve_seek_rejects_negative_targets() {
        assert_eq!(resolve_seek(10, 100, SeekFrom::Current(-4)).unwrap(), 6);
        assert_eq!(resolve_seek(10, 100, SeekFrom::End(-1)).unwrap(), 99);
        assert!(resolve_seek(3, 100, SeekFrom::Current(-4)).is_err());
    }

    #[test]
    fn extract_one_writes_the_entry() {
        let temp = TempDir::new("extract");
        let zip = build_zip(&sample(), false, b"");
        let mut reader = Cursor::new(zip);
        let entries = read_central_directory(&mut reader).unwrap();
        let extracted = extract_one(&mut reader, &entries[2], &temp.0, &RunGate::new(), &|_| {}, &|_| {}, None).unwrap();
        assert_eq!(
            extracted,
            Extracted {
                written: 23,
                downloaded: 23
            }
        );
        assert_eq!(
            std::fs::read(temp.0.join("root.txt")).unwrap(),
            b"charlie charlie charlie"
        );
    }

    #[tokio::test]
    async fn a_zip_of_one_file_is_read_whole_or_unpacked_over_its_destination() {
        let temp = TempDir::new("one-member");
        let zip = build_zip(&[("inner.bin", b"payload payload")], false, b"");
        assert_eq!(read_member(zip.clone(), "inner.bin", 1 << 20).unwrap(), b"payload payload");
        assert_eq!(read_member(zip.clone(), "inner.bin", 4).unwrap(), b"payl", "unpacking stops at the limit");
        assert!(read_member(zip.clone(), "other.bin", 1 << 20).unwrap_err().contains("no other.bin"));

        let path = temp.0.join("one.zip");
        std::fs::write(&path, &zip).unwrap();
        let (gate, ledger) = (peebify_transfer::control::RunGate::new(), peebify_transfer::Ledger::new(None));
        extract_member(&path, "Runtime/inner.dll", &temp.0, &gate, &ledger).unwrap();
        ledger.seal().await.unwrap();
        assert_eq!(std::fs::read(temp.0.join("Runtime").join("inner.dll")).unwrap(), b"payload payload");

        std::fs::write(&path, build_zip(&sample(), false, b"")).unwrap();
        assert!(extract_member(&path, "x", &temp.0, &gate, &ledger).unwrap_err().contains("exactly one file"));
    }

    #[test]
    fn extract_one_reports_compressed_bytes_as_progress() {
        let temp = TempDir::new("deflate");
        let data = b"delta delta delta delta ".repeat(512);
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        io::Write::write_all(&mut encoder, &data).unwrap();
        let packed = encoder.finish().unwrap();
        let name = "packed.bin";
        let crc = crc32fast::hash(&data);
        let mut zip = Vec::new();
        put32(&mut zip, LOCAL_HEADER_SIG);
        put16(&mut zip, 20);
        put16(&mut zip, 0);
        put16(&mut zip, METHOD_DEFLATE);
        put32(&mut zip, 0);
        put32(&mut zip, crc);
        put32(&mut zip, packed.len() as u32);
        put32(&mut zip, data.len() as u32);
        put16(&mut zip, name.len() as u16);
        put16(&mut zip, 0);
        zip.extend_from_slice(name.as_bytes());
        zip.extend_from_slice(&packed);
        let entry = CdEntry {
            path: name.to_string(),
            size: data.len() as u64,
            csize: packed.len() as u64,
            crc32: crc,
            method: METHOD_DEFLATE,
            flags: 0,
            header_start: 0,
        };
        let streamed = std::cell::Cell::new(0u64);
        let on_disk = std::cell::Cell::new(0u64);

        let extracted = extract_one(
            &mut Cursor::new(zip),
            &entry,
            &temp.0,
            &RunGate::new(),
            &|n| streamed.set(streamed.get() + n),
            &|n| on_disk.set(on_disk.get() + n),
            None,
        )
        .unwrap();

        assert!(packed.len() < data.len());
        assert_eq!(
            extracted,
            Extracted {
                written: data.len() as u64,
                downloaded: packed.len() as u64
            }
        );
        assert_eq!(streamed.get(), packed.len() as u64);
        assert_eq!(on_disk.get(), data.len() as u64);
        assert_eq!(std::fs::read(temp.0.join(name)).unwrap(), data);
    }

    #[test]
    fn range_end_respects_the_entry_bound() {
        assert_eq!(range_end(0, 256, 10_000, None), 256);
        assert_eq!(range_end(9_900, 256, 10_000, None), 10_000);
        assert_eq!(range_end(100, 8_000, 10_000, Some(1_000)), 1_000);
        assert_eq!(range_end(1_000, 8_000, 10_000, Some(1_000)), 1_001);
    }

    type RangeLog = Arc<parking_lot::Mutex<Vec<(usize, u64, u64)>>>;

    fn range_server(parts: Vec<Vec<u8>>, cut: usize) -> (String, RangeLog) {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let parts = Arc::new(parts);
        let served_log = Arc::clone(&log);
        std::thread::spawn(move || {
            let cut_once = Arc::new(AtomicBool::new(false));
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let (parts, log, cut_once) =
                    (Arc::clone(&parts), Arc::clone(&served_log), Arc::clone(&cut_once));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let part: usize = line.split(' ').nth(1).unwrap()[1..].parse().unwrap();
                    let mut range = (0u64, 0u64);
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        let lower = header.to_ascii_lowercase();
                        if let Some(spec) = lower.strip_prefix("range: bytes=") {
                            let (a, b) = spec.trim().split_once('-').unwrap();
                            range = (a.parse().unwrap(), b.parse().unwrap());
                        }
                    }
                    log.lock().push((part, range.0, range.1 + 1));
                    let data = &parts[part];
                    let body = &data[range.0 as usize..=range.1 as usize];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        range.0,
                        range.1,
                        data.len()
                    );
                    let _ = conn.write_all(head.as_bytes());
                    let streamed = body.len() as u64 > REMOTE_PROBE_CHUNK;
                    if streamed && !cut_once.swap(true, Ordering::SeqCst) {
                        let _ = conn.write_all(&body[..cut.min(body.len())]);
                        let _ = conn.flush();
                        let _ = conn.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    let _ = conn.write_all(body);
                });
            }
        });
        (base, log)
    }

    #[test]
    fn failures_of_the_server_are_told_from_a_package_index_nobody_hosts_any_more() {
        let gone = "central directory parse failed: read tail: HTTP Error: 404 for URL https://cdn/a.zip.001";
        assert!(remote_is_gone(gone));
        assert!(!entry_retry_allowed(&format!("stream error for 'a': {gone}"), 0));
        let busy = "stream error for 'a': HTTP Error: 503 for URL https://cdn/a.zip.001";
        assert!(!remote_is_gone(busy));
        assert!(remote_unreachable(busy));
        assert!(entry_retry_allowed(busy, 0));
        for lost in [
            "read tail: Request error: error sending request",
            "stream error for 'a': Stream error: connection reset",
            "Download stalled. The connection went idle.",
        ] {
            assert!(remote_unreachable(lost), "{lost}");
            assert!(entry_retry_allowed(lost, 0), "{lost}");
        }
    }

    #[test]
    fn remote_reader_streams_across_parts_and_resumes_after_a_cut() {
        let data: Vec<u8> = (0..4_500_000u32).map(|i| (i % 251) as u8).collect();
        let split = 3_000_000usize;
        let cut = 1_200_000usize;
        let (base, log) = range_server(vec![data[..split].to_vec(), data[split..].to_vec()], cut);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let packs: Arc<[Pack]> = vec![
            Pack { url: format!("{base}/0").into(), size: split as u64 },
            Pack { url: format!("{base}/1").into(), size: (data.len() - split) as u64 },
        ]
        .into();
        let mut reader = HttpRangeReader::open(packs, Detached::job(), runtime.handle().clone()).unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        let mut out = vec![0u8; data.len() - 100];
        reader.read_exact(&mut out).unwrap();
        assert!(out == data[100..], "streamed bytes differ from the source");

        let log = log.lock().clone();
        let probe_end = 100 + REMOTE_PROBE_CHUNK;
        assert_eq!(log[0], (0, 100, probe_end));
        assert_eq!(log[1], (0, probe_end, split as u64));
        assert_eq!(log[2], (0, probe_end + cut as u64, split as u64));
        assert_eq!(log[3], (1, 0, (data.len() - split) as u64));
        assert_eq!(log.len(), 4);
    }

    #[test]
    fn entry_retry_only_covers_transient_failures() {
        assert!(entry_retry_allowed(
            "stream error for 'a': range request failed: connection reset",
            0
        ));
        assert!(entry_retry_allowed(
            "read local header of 'a': HTTP 503 for https://cdn/x",
            1
        ));
        assert!(!entry_retry_allowed(
            "read local header of 'a': HTTP 503 for https://cdn/x",
            ENTRY_REQUEUES
        ));
        assert!(!entry_retry_allowed("stream error for 'a': HTTP 404 for u", 0));
        assert!(!entry_retry_allowed(
            "stream error for 'a': CDN ignored the Range request (returned 200)",
            0
        ));
        assert!(!entry_retry_allowed("Operation cancelled by user.", 0));
        assert!(entry_retry_allowed("checksum mismatch after fetching 'a'", 0));
        assert!(!entry_retry_allowed("checksum mismatch after fetching 'a'", 1));
        assert!(!entry_retry_allowed("failed to finalize C:/x: locked", 0));
    }

    #[test]
    fn structural_failures_are_recognised() {
        assert!(is_structural_failure(
            "central directory parse failed: truncated, parsed 1 of 4 records"
        ));
        assert!(is_structural_failure(
            "stream error for 'a': CDN ignored the Range request (returned 200)"
        ));
        assert!(!is_structural_failure(
            "stream error for 'a': range request failed: timed out"
        ));
        assert!(!is_structural_failure("HTTP 403 for https://cdn/x"));
    }

    #[test]
    fn a_full_disk_is_explained_once() {
        let dir = Path::new("C:/Games/Endfield");
        let raw = "write error for 'a': There is not enough space on the disk. (os error 112)";
        let explained = disk_full_advice(dir, raw.to_string());
        assert!(explained.starts_with("Not enough disk space in "), "{explained}");
        assert!(explained.contains(raw));
        assert_eq!(
            super::super::fs_util::classify(&explained),
            super::super::fs_util::FailureKind::DiskFull
        );
        assert_eq!(disk_full_advice(dir, explained.clone()), explained);
        let other = "range request failed: timed out".to_string();
        assert_eq!(disk_full_advice(dir, other.clone()), other);
    }

    #[test]
    fn a_read_that_fails_is_worded_by_what_it_means_to_the_user() {
        for gone in ["404", "403", "410"] {
            let said = index_failure(&format!("central directory parse failed: read tail: HTTP {gone} for https://cdn/x"));
            assert!(said.contains("no longer hosted") && said.contains("Update the game"), "{said}");
        }
        for offline in ["read tail: range request failed: dns error", "read tail: HTTP 503 for https://cdn/x"] {
            assert!(index_failure(offline).contains("could not be reached"), "{offline}");
        }
        let unusable = index_failure("central directory parse failed: no end-of-central-directory signature");
        assert!(unusable.contains("unusable package index"), "{unusable}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_members_asked_for_are_extracted_and_a_record_can_name_one_without_its_place() {
        let dir = TempDir::new("members");
        let zip = build_zip(&sample(), false, b"");
        let cut = zip.len() / 2;
        let (base, _) = range_server(vec![zip[..cut].to_vec(), zip[cut..].to_vec()], 0);
        let packs: Arc<[Pack]> = vec![
            Pack { url: format!("{base}/0").into(), size: cut as u64 },
            Pack { url: format!("{base}/1").into(), size: (zip.len() - cut) as u64 },
        ]
        .into();
        let job = Detached::job();
        let listed = listing(&packs, &job).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|e| &*e.path).collect();
        assert_eq!(names, ["Data/a.bin", "Data/b.bin", "root.txt"]);
        assert_eq!(listed[0].hash, Hash::Crc32(crc32fast::hash(b"alpha contents")));
        assert!(matches!(listed[0].source, Source::Member { at: Some(_), .. }));

        let intact = dir.write("Data/a.bin", b"alpha contents");
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new().write(true).open(&intact).unwrap().set_modified(long_ago).unwrap();
        dir.write("Data/b.bin", b"BRAVO");
        let broken: Vec<Entry> = listed
            .iter()
            .filter(|e| {
                let path = dir.0.join(&*e.path);
                FileValidator::check(&path, e.size, &e.hash.expected(), None, |_| {}).unwrap() != FileCheck::Valid
            })
            .cloned()
            .collect();
        assert_eq!(broken.iter().map(|e| &*e.path).collect::<Vec<_>>(), ["Data/b.bin", "root.txt"]);

        fetch_members(&dir.0, &broken[..1], Arc::clone(&job), None).await.unwrap();
        fetch_members(&dir.0, &[broken[1].clone().member_of(&packs)], job, None).await.unwrap();

        assert_eq!(std::fs::read(dir.0.join("Data/b.bin")).unwrap(), b"bravo");
        assert_eq!(std::fs::read(dir.0.join("root.txt")).unwrap(), b"charlie charlie charlie");
        assert_eq!(std::fs::metadata(&intact).unwrap().modified().unwrap(), long_ago, "a member that checks out is left alone");
        let missing = Entry::new("Data/gone.bin", 1, Hash::Crc32(0), "").member_of(&packs);
        let error = fetch_members(&dir.0, &[missing], Detached::job(), None).await.unwrap_err();
        assert!(error.contains("not in the hosted packages"), "{error}");
    }
}
