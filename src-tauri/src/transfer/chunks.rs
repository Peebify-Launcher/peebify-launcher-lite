// ------------ Chunk Transfers ------------
// Fetches small whole bodies such as Sophon chunks and hands each one to a sink that decodes and writes it, off the connection.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::files::{is_host_outage, JobHooks, CANCELLED};
use crate::fsx::{self, FailureKind};
use crate::ledger::{Ledger, Sealed};
use crate::net;
use crate::retry::{is_protocol_error, pushes_back, NetWait};
use crate::runtime::TransferRuntime;

const MEMORY_BUDGET_MIB: u32 = 256;
const MAX_REFETCHES: u32 = 3;
const IDLE_RECHECK: Duration = Duration::from_millis(200);

pub struct ChunkSpec {
    pub id: String,
    pub url: String,
    pub size_hint: u64,
}

#[derive(Debug)]
pub enum ChunkError {
    Corrupt(String),
    Fatal(String),
}

pub trait ChunkSink: Send + Sync + 'static {
    fn accept(&self, index: usize, body: bytes::Bytes) -> Result<(), ChunkError>;
}

#[derive(Debug, Default, Clone)]
pub struct ChunkReport {
    pub chunks: usize,
    pub bytes: u64,
    pub refetched: u64,
}

struct Retry {
    index: usize,
    corrupt: u32,
}

struct Chunks {
    rt: Arc<TransferRuntime>,
    hooks: Arc<dyn JobHooks>,
    specs: Vec<ChunkSpec>,
    sink: Arc<dyn ChunkSink>,
    next: AtomicUsize,
    retries: Mutex<VecDeque<Retry>>,
    pending: AtomicUsize,
    settled: tokio::sync::Notify,
    memory: Arc<Semaphore>,
    cpu: Arc<Semaphore>,
    abort: AtomicBool,
    failure: Mutex<Option<String>>,
    net_wait: NetWait,
    bytes: AtomicU64,
    refetched: AtomicU64,
    max_bytes: u64,
}

pub async fn fetch_chunks(
    rt: &Arc<TransferRuntime>,
    hooks: Arc<dyn JobHooks>,
    ledger: &Arc<Ledger>,
    specs: Vec<ChunkSpec>,
    sink: Arc<dyn ChunkSink>,
    max_bytes: u64,
) -> Result<(ChunkReport, Sealed), String> {
    let count = specs.len();
    let cpu = crate::runtime::cores().clamp(2, 8);
    let me = Arc::new(Chunks {
        rt: Arc::clone(rt),
        hooks,
        specs,
        sink,
        next: AtomicUsize::new(0),
        retries: Mutex::new(VecDeque::new()),
        pending: AtomicUsize::new(0),
        settled: tokio::sync::Notify::new(),
        memory: Arc::new(Semaphore::new(MEMORY_BUDGET_MIB as usize)),
        cpu: Arc::new(Semaphore::new(cpu)),
        abort: AtomicBool::new(false),
        failure: Mutex::new(None),
        net_wait: NetWait::default(),
        bytes: AtomicU64::new(0),
        refetched: AtomicU64::new(0),
        max_bytes,
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
                first_error.get_or_insert(format!("chunk worker panicked: {e}"));
            }
        }
    }
    me.drain().await;
    checkpoints.abort();
    let first_error = me.failure.lock().clone().or(first_error);
    if rt.config.skip_final_checkpoint {
        return Err(first_error.unwrap_or_else(|| "stopped before the final checkpoint".to_string()));
    }
    let sealed = ledger.seal().await;
    if let Some(e) = first_error {
        return Err(e);
    }
    let sealed = sealed?;
    Ok((
        ChunkReport {
            chunks: count,
            bytes: me.bytes.load(Ordering::Relaxed),
            refetched: me.refetched.load(Ordering::Relaxed),
        },
        sealed,
    ))
}

impl Chunks {
    fn is_cancelled(&self) -> bool {
        self.hooks.gate().is_cancelled()
    }

    fn fail(&self, error: String) {
        self.failure.lock().get_or_insert(error);
        self.abort.store(true, Ordering::SeqCst);
        self.settled.notify_waiters();
    }

    fn take(&self) -> Option<Retry> {
        if let Some(retry) = self.retries.lock().pop_front() {
            return Some(retry);
        }
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        (index < self.specs.len()).then_some(Retry { index, corrupt: 0 })
    }

    async fn drain(&self) {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if self.pending.load(Ordering::SeqCst) == 0 {
                return;
            }
            settled.await;
        }
    }

    async fn worker(self: Arc<Self>) -> Result<(), String> {
        loop {
            if self.abort.load(Ordering::SeqCst) {
                return Ok(());
            }
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if self.hooks.gate().is_stopped() {
                self.hooks.gate().wait_while_stopped().await;
                continue;
            }
            let Some(job) = self.take() else {
                if self.pending.load(Ordering::SeqCst) == 0 {
                    return Ok(());
                }
                let settled = self.settled.notified();
                tokio::select! {
                    _ = settled => {}
                    _ = tokio::time::sleep(IDLE_RECHECK) => {}
                }
                continue;
            };
            let spec = &self.specs[job.index];
            let mib = (spec.size_hint.div_ceil(1 << 20)).clamp(1, u64::from(MEMORY_BUDGET_MIB)) as u32;
            let Ok(memory) = Arc::clone(&self.memory).acquire_many_owned(mib).await else {
                return Ok(());
            };
            let body = match self.fetch(job.index).await {
                Ok(body) => body,
                Err(e) => {
                    if e != CANCELLED {
                        self.fail(e.clone());
                    }
                    return Err(e);
                }
            };
            self.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
            self.pending.fetch_add(1, Ordering::SeqCst);
            let me = Arc::clone(&self);
            tokio::spawn(async move {
                let outcome = me.decode(job.index, body).await;
                drop(memory);
                match outcome {
                    Ok(()) => {}
                    Err(ChunkError::Corrupt(why)) if job.corrupt < MAX_REFETCHES => {
                        log::warn!(
                            "Chunk {} was damaged ({why}), fetching it again ({}/{MAX_REFETCHES}).",
                            me.specs[job.index].id,
                            job.corrupt + 1
                        );
                        me.refetched.fetch_add(1, Ordering::Relaxed);
                        me.retries.lock().push_back(Retry {
                            index: job.index,
                            corrupt: job.corrupt + 1,
                        });
                    }
                    Err(ChunkError::Corrupt(why) | ChunkError::Fatal(why)) => me.fail(why),
                }
                me.pending.fetch_sub(1, Ordering::SeqCst);
                me.settled.notify_waiters();
            });
        }
    }

    async fn decode(&self, index: usize, body: bytes::Bytes) -> Result<(), ChunkError> {
        let _cpu = Arc::clone(&self.cpu)
            .acquire_owned()
            .await
            .map_err(|e| ChunkError::Fatal(e.to_string()))?;
        let sink = Arc::clone(&self.sink);
        tokio::task::spawn_blocking(move || sink.accept(index, body))
            .await
            .map_err(|e| ChunkError::Fatal(format!("chunk task panicked: {e}")))?
    }

    async fn fetch(&self, index: usize) -> Result<bytes::Bytes, String> {
        let spec = &self.specs[index];
        let mut direct = false;
        let mut attempt = 0u32;
        loop {
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if self.hooks.gate().is_stopped() {
                self.hooks.gate().wait_while_stopped().await;
                continue;
            }
            let url = if direct {
                spec.url.clone()
            } else {
                self.hooks.route(&spec.url)
            };
            let epoch = self.hooks.gate().transfer_epoch();
            let e = match self.fetch_once(&url, spec).await {
                Ok(body) => {
                    self.net_wait.clear(&url);
                    self.rt.host_ok(&net::url_host(&url));
                    return Ok(body);
                }
                Err(e) => e,
            };
            if self.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if net::permanent_client_status(&e).is_some() {
                if url != spec.url {
                    log::warn!("Chunk {}: the mirror refused it ({e}), using the publisher's own server.", spec.id);
                    direct = true;
                    continue;
                }
                if self.hooks.on_refused(&url) {
                    continue;
                }
                return Err(e);
            }
            if is_host_outage(&e) {
                if self.hooks.on_outage(&url) {
                    continue;
                }
                return Err(e);
            }
            if self.hooks.gate().is_stopped() {
                self.hooks.gate().wait_while_stopped().await;
                continue;
            }
            if self.hooks.gate().transfer_epoch() != epoch {
                continue;
            }
            if pushes_back(&e) {
                self.rt.host_overloaded(&net::url_host(&url));
            }
            if is_protocol_error(&e) {
                self.rt.prefer_multiplexed(&net::url_host(&url));
            }
            if fsx::classify(&e) == FailureKind::Network && !self.hooks.can_reach(&url).await {
                if let Err(waited) = self.net_wait.wait(&self.rt, self.hooks.as_ref(), &url).await {
                    if is_host_outage(&waited) && self.hooks.on_outage(&url) {
                        continue;
                    }
                    return Err(waited);
                }
                continue;
            }
            attempt += 1;
            if attempt >= self.rt.config.max_retries {
                return Err(e);
            }
            log::warn!("Chunk {} failed (attempt {attempt}/{}): {e}, retrying.", spec.id, self.rt.config.max_retries);
            tokio::time::sleep(net::retry_delay(attempt, self.rt.config.retry_base_ms)).await;
        }
    }

    async fn fetch_once(&self, url: &str, spec: &ChunkSpec) -> Result<bytes::Bytes, String> {
        let host = net::url_host(url);
        let slots = loop {
            tokio::select! {
                slots = self.rt.connection(&host) => break slots,
                _ = tokio::time::sleep(IDLE_RECHECK) => {
                    if self.is_cancelled() || self.abort.load(Ordering::SeqCst) {
                        return Err(CANCELLED.to_string());
                    }
                }
            }
        };
        let response = self
            .rt
            .client_for(&host)
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Request error: {}", net::describe(&e)))?;
        let status = response.status().as_u16();
        net::note_protocol(url, response.version());
        if status != 200 {
            return Err(format!("HTTP Error: {status} for URL {url}"));
        }
        if let Some(len) = response.content_length() {
            if len > self.max_bytes {
                return Err(format!("Refusing {url}: it declares {len} bytes, over the {}-byte limit.", self.max_bytes));
            }
        }
        let capacity = response.content_length().unwrap_or(spec.size_hint).min(self.max_bytes);
        let mut body = bytes::BytesMut::with_capacity(capacity as usize);
        let mut stream = response.bytes_stream();
        let stall = self.rt.config.stall_timeout;
        loop {
            if self.hooks.gate().is_stopped() {
                return Err("The download was paused.".to_string());
            }
            let chunk = match tokio::time::timeout(stall, stream.next()).await {
                Err(_) => return Err("Download stalled. The connection went idle.".to_string()),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(format!("Stream error: {}", net::describe(&e))),
                Ok(Some(Ok(chunk))) => chunk,
            };
            self.rt.limiter.consume(chunk.len() as u64).await;
            self.hooks.tracker().add_network(chunk.len() as u64);
            if body.len() as u64 + chunk.len() as u64 > self.max_bytes {
                return Err(format!("Refusing {url}: the response exceeded the {}-byte limit.", self.max_bytes));
            }
            body.extend_from_slice(&chunk);
        }
        drop(slots);
        Ok(body.freeze())
    }
}
