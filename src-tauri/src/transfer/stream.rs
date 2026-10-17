// ------------ Package Downloads ------------
// One body of unknown size written to a file, for mods and mod tools, on the shared connections, speed limit and retry rules.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::files::{JobHooks, CANCELLED};
use crate::fsx;
use crate::net;
use crate::part::PartIdentity;
use crate::retry::{next_attempt, NetWait};
use crate::runtime::TransferRuntime;

const POLL: Duration = Duration::from_millis(250);
const WRITE_BUFFER: usize = 1 << 20;

pub type Refusal<'a> = &'a (dyn Fn(u64, u64) -> Option<String> + Sync);

pub async fn download_stream<F: FnMut(u64, u64)>(
    rt: &Arc<TransferRuntime>,
    hooks: &dyn JobHooks,
    url: &str,
    dest: &Path,
    refusal: Refusal<'_>,
    on_progress: &mut F,
) -> Result<u64, String> {
    let net_wait = NetWait::default();
    let mut attempt = 0u32;
    let mut resume: Option<(u64, String)> = None;
    loop {
        let epoch = hooks.gate().transfer_epoch();
        match fetch_once(rt, hooks, url, dest, refusal, &mut resume, on_progress).await {
            Ok(written) => return Ok(written),
            Err(Failure::Final(e)) => return Err(e),
            Err(Failure::Again(e)) => next_attempt(rt, hooks, &net_wait, url, e, epoch, &mut attempt).await?,
        }
    }
}

enum Failure {
    Final(String),
    Again(String),
}

async fn fetch_once<F: FnMut(u64, u64)>(
    rt: &Arc<TransferRuntime>,
    hooks: &dyn JobHooks,
    url: &str,
    dest: &Path,
    refusal: Refusal<'_>,
    resume: &mut Option<(u64, String)>,
    on_progress: &mut F,
) -> Result<u64, Failure> {
    let again = Failure::Again;
    let host = net::url_host(url);
    let slots = loop {
        tokio::select! {
            slots = rt.connection(&host) => break slots,
            _ = tokio::time::sleep(POLL) => {
                if hooks.gate().is_cancelled() {
                    return Err(Failure::Final(CANCELLED.to_string()));
                }
            }
        }
    };
    let mut request = rt.client_for(&host).get(url);
    if let Some((from, validator)) = resume.as_ref() {
        request = request
            .header(reqwest::header::RANGE, format!("bytes={from}-"))
            .header(reqwest::header::IF_RANGE, validator.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|e| again(format!("Request error: {}", net::describe(&e))))?;
    net::note_protocol(url, response.version());
    let status = response.status().as_u16();
    let from = match (status, resume.as_ref()) {
        (206, Some((from, _))) => {
            let starts = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim_start().starts_with(&format!("bytes {from}-")));
            if !starts {
                *resume = None;
                return Err(again(format!("Stream error: unexpected Content-Range for {url}")));
            }
            *from
        }
        (200, _) => 0,
        _ => return Err(again(format!("HTTP Error: {status} for URL {url}"))),
    };
    let announced = response.content_length().map_or(0, |len| from + len);
    if let Some(refused) = refusal(0, announced) {
        return Err(Failure::Final(refused));
    }
    let validator = PartIdentity::default()
        .with_validators(response.headers())
        .if_range()
        .map(str::to_string);

    let file = if from > 0 {
        tokio::fs::OpenOptions::new().append(true).open(dest).await
    } else {
        tokio::fs::File::create(dest).await
    }
    .map_err(|e| Failure::Final(fsx::fmt_io(&format!("Could not create {}", dest.display()), &e)))?;
    let mut file = tokio::io::BufWriter::with_capacity(WRITE_BUFFER, file);
    let mut written = from;
    let mut stream = response.bytes_stream();
    let stall = rt.config.stall_timeout;
    let mut idle = Duration::ZERO;
    let outcome: Result<(), Failure> = loop {
        if hooks.gate().is_cancelled() {
            break Err(Failure::Final(CANCELLED.to_string()));
        }
        let wait = POLL.min(stall.saturating_sub(idle)).max(Duration::from_millis(1));
        let chunk = match tokio::time::timeout(wait, stream.next()).await {
            Err(_) => {
                idle += wait;
                if idle >= stall {
                    break Err(again("Download stalled. The connection went idle.".to_string()));
                }
                continue;
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(again(format!("Stream error: {}", net::describe(&e)))),
            Ok(Some(Ok(chunk))) => chunk,
        };
        idle = Duration::ZERO;
        if let Some(refused) = refusal(written + chunk.len() as u64, announced) {
            break Err(Failure::Final(refused));
        }
        rt.limiter.consume(chunk.len() as u64).await;
        if let Err(e) = file.write_all(&chunk).await {
            break Err(Failure::Final(fsx::fmt_io("Write error", &e)));
        }
        written += chunk.len() as u64;
        on_progress(written, announced);
    };
    drop(stream);
    drop(slots);
    let flushed = file.flush().await;
    match outcome {
        Ok(()) => {
            flushed.map_err(|e| Failure::Final(fsx::fmt_io("Write error", &e)))?;
            if announced > 0 && written < announced {
                *resume = validator.map(|v| (written, v));
                return Err(again(format!(
                    "Stream error: the connection closed after {written} of {announced} bytes"
                )));
            }
            Ok(written)
        }
        Err(Failure::Again(e)) => {
            *resume = match (flushed, validator) {
                (Ok(()), Some(v)) if written > 0 => Some((written, v)),
                _ => None,
            };
            Err(Failure::Again(e))
        }
        Err(final_error) => Err(final_error),
    }
}
