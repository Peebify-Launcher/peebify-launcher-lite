// ------------ Remote Ranges ------------
// Byte ranges of a remote archive fetched on demand, for readers that seek through it on the server, on the shared connections and speed limit.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};

use crate::files::{JobHooks, CANCELLED};
use crate::net;
use crate::retry::{next_attempt, NetWait};
use crate::runtime::{Slot, TransferRuntime};

pub const RANGE_IGNORED: &str = "CDN ignored the Range request (returned 200)";
const POLL: Duration = Duration::from_millis(250);

pub struct RangeReader {
    rt: Arc<TransferRuntime>,
    net_wait: NetWait,
}

pub struct RangeBody {
    stream: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    _slots: (Slot, Slot),
    rt: Arc<TransferRuntime>,
    cancelled: Arc<AtomicBool>,
    left: u64,
}

impl RangeBody {
    pub fn left(&self) -> u64 {
        self.left
    }

    pub async fn next(&mut self) -> Result<Option<bytes::Bytes>, String> {
        if self.left == 0 {
            return Ok(None);
        }
        let stall = self.rt.config.stall_timeout;
        let mut idle = Duration::ZERO;
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(CANCELLED.to_string());
            }
            let wait = POLL.min(stall.saturating_sub(idle)).max(Duration::from_millis(1));
            match tokio::time::timeout(wait, self.stream.next()).await {
                Err(_) => {
                    idle += wait;
                    if idle >= stall {
                        return Err("Download stalled. The connection went idle.".to_string());
                    }
                }
                Ok(None) => {
                    return Err(format!(
                        "Stream error: the connection closed with {} bytes of the range left",
                        self.left
                    ))
                }
                Ok(Some(Err(e))) => return Err(format!("Stream error: {}", net::describe(&e))),
                Ok(Some(Ok(chunk))) => {
                    if chunk.len() as u64 > self.left {
                        return Err("Stream error: the server sent more than the range".to_string());
                    }
                    self.rt.limiter.consume(chunk.len() as u64).await;
                    self.left -= chunk.len() as u64;
                    return Ok(Some(chunk));
                }
            }
        }
    }
}

impl RangeReader {
    pub fn new(rt: Arc<TransferRuntime>) -> Self {
        Self {
            rt,
            net_wait: NetWait::default(),
        }
    }

    pub async fn open(&self, hooks: &dyn JobHooks, url: &str, start: u64, end: u64) -> Result<RangeBody, String> {
        let mut attempt = 0u32;
        loop {
            let epoch = hooks.gate().transfer_epoch();
            match self.open_once(hooks, url, start, end).await {
                Ok(body) => {
                    self.net_wait.clear(url);
                    return Ok(body);
                }
                Err(e) if e == RANGE_IGNORED => return Err(e),
                Err(e) => next_attempt(&self.rt, hooks, &self.net_wait, url, e, epoch, &mut attempt).await?,
            }
        }
    }

    pub async fn fetch(&self, hooks: &dyn JobHooks, url: &str, start: u64, end: u64) -> Result<bytes::Bytes, String> {
        let mut attempt = 0u32;
        loop {
            let epoch = hooks.gate().transfer_epoch();
            let read = async {
                let mut body = self.open_once(hooks, url, start, end).await?;
                let mut out = bytes::BytesMut::with_capacity((end - start) as usize);
                while let Some(piece) = body.next().await? {
                    out.extend_from_slice(&piece);
                }
                Ok::<_, String>(out.freeze())
            };
            match read.await {
                Ok(bytes) => {
                    self.net_wait.clear(url);
                    return Ok(bytes);
                }
                Err(e) if e == RANGE_IGNORED => return Err(e),
                Err(e) => next_attempt(&self.rt, hooks, &self.net_wait, url, e, epoch, &mut attempt).await?,
            }
        }
    }

    async fn open_once(&self, hooks: &dyn JobHooks, url: &str, start: u64, end: u64) -> Result<RangeBody, String> {
        let host = net::url_host(url);
        let slots = loop {
            tokio::select! {
                slots = self.rt.connection(&host) => break slots,
                _ = tokio::time::sleep(POLL) => {
                    if hooks.gate().is_cancelled() {
                        return Err(CANCELLED.to_string());
                    }
                }
            }
        };
        let response = self
            .rt
            .client_for(&host)
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={start}-{}", end - 1))
            .send()
            .await
            .map_err(|e| format!("Request error: {}", net::describe(&e)))?;
        net::note_protocol(url, response.version());
        match response.status().as_u16() {
            206 => {}
            200 => return Err(RANGE_IGNORED.to_string()),
            status => return Err(format!("HTTP Error: {status} for URL {url}")),
        }
        let starts_right = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.trim_start().starts_with(&format!("bytes {start}-")));
        if !starts_right {
            return Err(format!("Stream error: unexpected Content-Range for {url}"));
        }
        Ok(RangeBody {
            stream: Box::pin(response.bytes_stream()),
            _slots: slots,
            rt: Arc::clone(&self.rt),
            cancelled: Arc::clone(hooks.gate().flag()),
            left: end - start,
        })
    }
}
