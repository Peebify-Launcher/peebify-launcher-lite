// ------------ HTTP and Network Helpers ------------
// The shared web client every part of the backend uses, plus helpers for size-capped reads, retries with backoff,
// conditional GETs and a small TTL cache. It also runs the background monitor that tells the UI when we go online or offline.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

pub use peebify_transfer::net::{
    connects, describe, host_and_port, note_protocol, permanent_client_status,
    retry_delay,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
pub const USER_AGENT: &str = concat!("PeebifyLauncher/", env!("CARGO_PKG_VERSION"));

pub fn builder() -> reqwest::ClientBuilder {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        build(
            builder()
                .timeout(HTTP_TIMEOUT)
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(15))
                .user_agent(USER_AGENT),
        )
    })
}

pub fn shared_transfer() -> std::sync::Arc<peebify_transfer::TransferRuntime> {
    static SHARED: OnceLock<std::sync::Arc<peebify_transfer::TransferRuntime>> = OnceLock::new();
    std::sync::Arc::clone(SHARED.get_or_init(|| {
        peebify_transfer::TransferRuntime::new(
            build(peebify_transfer::net::bulk_client(builder(), USER_AGENT)),
            build(peebify_transfer::net::multiplexed_client(builder(), USER_AGENT)),
            peebify_transfer::RuntimeConfig::default(),
        )
    }))
}

pub fn download_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        build(peebify_transfer::net::multiplexed_client(builder(), USER_AGENT).read_timeout(HTTP_TIMEOUT))
    })
}

fn build(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder.build().expect("reqwest client construction cannot fail with these options")
}

pub struct ToolJob {
    gate: peebify_transfer::control::RunGate,
    tracker: peebify_transfer::progress::ProgressTracker,
}

impl ToolJob {
    pub fn new(cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            gate: peebify_transfer::control::RunGate::sharing(cancelled, false),
            tracker: peebify_transfer::progress::ProgressTracker::new(),
        }
    }
}

impl peebify_transfer::JobHooks for ToolJob {
    fn gate(&self) -> &peebify_transfer::control::RunGate {
        &self.gate
    }

    fn tracker(&self) -> &peebify_transfer::progress::ProgressTracker {
        &self.tracker
    }

    fn install_dir(&self) -> &std::path::Path {
        std::path::Path::new("")
    }

    fn reaches_home(&self) -> futures::future::BoxFuture<'static, bool> {
        Box::pin(reaches_home())
    }

    fn can_reach(&self, url: &str) -> futures::future::BoxFuture<'static, bool> {
        let url = url.to_string();
        Box::pin(async move { can_reach(&url).await })
    }
}

pub const MAX_IN_MEMORY_RESPONSE: u64 = 32 * 1024 * 1024;
pub const MAX_FEED_RESPONSE: u64 = 8 * 1024 * 1024;

#[cfg(debug_assertions)]
pub fn is_safe_override_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => matches!(
            parsed.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
        ),
        _ => false,
    }
}

pub(crate) async fn read_capped(response: reqwest::Response, url: &str) -> Result<bytes::Bytes, String> {
    read_capped_to(response, url, MAX_IN_MEMORY_RESPONSE).await
}

pub(crate) async fn read_capped_to(
    response: reqwest::Response,
    url: &str,
    limit: u64,
) -> Result<bytes::Bytes, String> {
    use futures::StreamExt;

    if let Some(len) = response.content_length() {
        if len > limit {
            return Err(format!(
                "Refusing {url}: it declares {len} bytes, over the {limit}-byte limit."
            ));
        }
    }

    let mut collected = bytes::BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream error: {}", describe(&e)))?;
        if collected.len() as u64 + chunk.len() as u64 > limit {
            return Err(format!(
                "Refusing {url}: the response exceeded the {limit}-byte limit."
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected.freeze())
}

pub(crate) async fn read_text_capped(
    response: reqwest::Response,
    url: &str,
    limit: u64,
) -> Result<String, String> {
    let bytes = read_capped_to(response, url, limit).await?;
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes[..]);
    Ok(String::from_utf8_lossy(body).into_owned())
}

pub async fn get_text(url: &str) -> Result<String, String> {
    let response = client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request error: {}", describe(&e)))?;
    let status = response.status().as_u16();
    note_reachable();
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }
    let bytes = read_capped(response, url).await?;
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("Invalid UTF-8 from {url}: {e}"))
}

pub async fn get_json(url: &str) -> Result<Value, String> {
    let text = get_text(url).await?;
    serde_json::from_str(&text).map_err(|e| format!("Invalid JSON from {url}: {e}"))
}

pub async fn get_bytes(url: &str) -> Result<bytes::Bytes, String> {
    let response = download_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request error: {}", describe(&e)))?;
    let status = response.status().as_u16();
    note_reachable();
    note_protocol(url, response.version());
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }
    read_capped(response, url).await
}

pub async fn content_length(url: &str) -> Option<u64> {
    if let Some(length) = head_length(url).await {
        return Some(length);
    }
    ranged_length(url).await
}

async fn head_length(url: &str) -> Option<u64> {
    let response = download_client().head(url).send().await.ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

async fn ranged_length(url: &str) -> Option<u64> {
    let response = download_client()
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .ok()?;
    if response.status().as_u16() != 206 {
        return None;
    }
    let total = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(total_from_content_range);
    drop(response);
    total
}

fn total_from_content_range(value: &str) -> Option<u64> {
    let (_, total) = value.trim().rsplit_once('/')?;
    let total = total.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok().filter(|n| *n > 0)
}

pub async fn with_retry<T, F, Fut>(
    mut operation: F,
    max_retries: u32,
    base_delay_ms: u64,
    label: &str,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut attempt = 1u32;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if attempt >= max_retries {
                    return Err(e);
                }
                let delay = retry_delay(attempt, base_delay_ms);
                log::warn!(
                    "{label} failed (attempt {attempt}/{max_retries}): {e} — retrying in {}ms",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

async fn resolves_google() -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::lookup_host("google.com:443")).await,
        Ok(Ok(_))
    )
}

pub async fn reaches_home() -> bool {
    match host_and_port(&super::api_config::api_origin()) {
        Some((host, port)) => connects(&host, port).await,
        None => false,
    }
}

pub async fn is_online() -> bool {
    let (reaches_home, resolves) = tokio::join!(reaches_home(), resolves_google());
    reaches_home || resolves
}

pub async fn can_reach(url: &str) -> bool {
    match host_and_port(url) {
        Some((host, port)) => connects(&host, port).await,
        None => is_online().await,
    }
}

const ONLINE_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const OFFLINE_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const RESUME_SETTLE: Duration = Duration::from_secs(5);

static SETTLE_BEFORE_RECHECK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn monitor_wake() -> &'static tokio::sync::Notify {
    static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
    WAKE.get_or_init(tokio::sync::Notify::new)
}

pub fn note_reachable() {
    if !is_online_cached() {
        monitor_wake().notify_one();
    }
}

pub fn note_unreachable() {
    if is_online_cached() {
        monitor_wake().notify_one();
    }
}

pub fn recheck_after_resume() {
    SETTLE_BEFORE_RECHECK.store(true, std::sync::atomic::Ordering::SeqCst);
    monitor_wake().notify_one();
}

fn check_interval(is_online: bool) -> Duration {
    if is_online {
        ONLINE_CHECK_INTERVAL
    } else {
        OFFLINE_CHECK_INTERVAL
    }
}

fn online_state() -> &'static std::sync::atomic::AtomicBool {
    static STATE: OnceLock<std::sync::atomic::AtomicBool> = OnceLock::new();
    STATE.get_or_init(|| std::sync::atomic::AtomicBool::new(true))
}

pub fn is_online_cached() -> bool {
    online_state().load(std::sync::atomic::Ordering::SeqCst)
}

fn apply_status(app: &tauri::AppHandle, is_online: bool, first: bool) {
    use tauri::Emitter;

    let was_online = online_state().swap(is_online, std::sync::atomic::Ordering::SeqCst);
    if !first && was_online == is_online {
        return;
    }
    if !first {
        log::info!(
            "Network status changed: {}",
            if is_online { "ONLINE" } else { "OFFLINE" }
        );
        if is_online {
            on_reconnect(app);
        }
    }
    let _ = app.emit(
        "network-status-changed",
        serde_json::json!({ "isOnline": is_online }),
    );
}

fn on_reconnect(app: &tauri::AppHandle) {
    use tauri::Manager;

    if let Some(state) = app.try_state::<super::BackendState>() {
        state.game_updater.resume_skipped_startup_sweep();
    }
}

pub(super) async fn network_recheck(app: &tauri::AppHandle) -> Result<Value, String> {
    let online = is_online().await;
    apply_status(app, online, false);
    Ok(super::ok_with(serde_json::json!({ "isOnline": online })))
}

pub fn start_monitoring(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        log::info!("Connectivity monitoring started (every 5 minutes online, every 15 seconds offline)");
        let mut first = true;
        loop {
            let online = is_online().await;
            apply_status(&app, online, first);
            first = false;
            tokio::select! {
                _ = tokio::time::sleep(check_interval(online)) => {}
                _ = monitor_wake().notified() => {}
            }
            if SETTLE_BEFORE_RECHECK.swap(false, std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(RESUME_SETTLE).await;
            }
        }
    });
}

pub struct TtlCache<V> {
    entries: parking_lot::Mutex<Vec<(String, V, std::time::Instant)>>,
    ttl: Duration,
    max: usize,
}

impl<V: Clone> TtlCache<V> {
    pub const fn new(ttl: Duration, max: usize) -> Self {
        Self {
            entries: parking_lot::Mutex::new(Vec::new()),
            ttl,
            max,
        }
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.get_aged(key).and_then(|(v, fresh)| fresh.then_some(v))
    }

    pub fn get_aged(&self, key: &str) -> Option<(V, bool)> {
        let entries = self.entries.lock();
        let (_, value, at) = entries.iter().find(|(k, _, _)| k == key)?;
        Some((value.clone(), at.elapsed() < self.ttl))
    }

    pub fn remove(&self, key: &str) {
        self.entries.lock().retain(|(k, _, _)| k != key);
    }

    pub fn set(&self, key: &str, value: V) {
        let mut entries = self.entries.lock();
        let now = std::time::Instant::now();
        if let Some(slot) = entries.iter_mut().find(|(k, _, _)| k == key) {
            slot.1 = value;
            slot.2 = now;
            return;
        }
        if entries.len() >= self.max {
            entries.remove(0);
        }
        entries.push((key.to_string(), value, now));
    }
}

#[derive(Debug, Clone)]
pub enum Conditional {
    Unchanged,
    Fresh { text: String, etag: Option<String> },
}

fn strong_etag(etag: &str) -> &str {
    let etag = etag.trim();
    etag.strip_prefix("W/").unwrap_or(etag)
}

pub async fn get_text_conditional(url: &str, etag: Option<&str>) -> Result<Conditional, String> {
    let mut request = client().get(url);
    if let Some(etag) = etag.map(strong_etag).filter(|e| !e.is_empty()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Request error: {}", describe(&e)))?;

    let status = response.status().as_u16();
    note_reachable();
    if status == 304 {
        return Ok(Conditional::Unchanged);
    }
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }

    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(strong_etag)
        .filter(|e| !e.is_empty())
        .map(str::to_string);

    let text = read_text_capped(response, url, MAX_FEED_RESPONSE).await?;

    Ok(Conditional::Fresh { text, etag })
}

pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_range_total_is_read_after_the_slash() {
        assert_eq!(total_from_content_range("bytes 0-0/373564786"), Some(373_564_786));
        assert_eq!(total_from_content_range(" bytes 0-0/42 "), Some(42));
        assert_eq!(total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(total_from_content_range("bytes 0-0/0"), None);
        assert_eq!(total_from_content_range("bytes 0-0"), None);
        assert_eq!(total_from_content_range("bytes 0-0/abc"), None);
    }

    #[test]
    fn a_weak_etag_is_sent_as_its_strong_form() {
        assert_eq!(strong_etag("W/\"585f8b\""), "\"585f8b\"");
        assert_eq!(strong_etag(" \"585f8b\" "), "\"585f8b\"");
        assert_eq!(strong_etag("W/"), "");
    }

    #[tokio::test]
    async fn a_refused_connection_names_its_cause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("nothing listens on a port that was just released");
        let full = describe(&error);
        let short = peebify_transfer::net::describe_short(&error);
        assert!(full.starts_with(&error.to_string()), "{full}");
        assert!(full.len() > error.to_string().len(), "{full}");
        assert!(short.len() <= full.len(), "{short} vs {full}");
        assert_eq!(
            super::super::fs_util::classify(&format!("Request error: {full}")),
            super::super::fs_util::FailureKind::Network
        );
        assert!(!can_reach(&format!("http://127.0.0.1:{port}/")).await);
    }

    fn serve_once(response: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(&response);
        });
        format!("http://127.0.0.1:{port}/")
    }

    async fn fetch_capped(response: &[u8], limit: u64) -> Result<String, String> {
        let url = serve_once(response.to_vec());
        let response = client().get(&url).send().await.map_err(|e| describe(&e))?;
        read_text_capped(response, &url, limit).await
    }

    #[tokio::test]
    async fn capped_reads_refuse_bodies_over_the_limit() {
        let declared = b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n\
            0123456789012345678901234567890123456789012345678901234567890123";
        let error = fetch_capped(declared, 16).await.unwrap_err();
        assert!(error.contains("declares 64 bytes"), "{error}");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
            20\r\n01234567890123456789012345678901\r\n0\r\n\r\n";
        let error = fetch_capped(chunked, 16).await.unwrap_err();
        assert!(error.contains("exceeded the 16-byte limit"), "{error}");

        let fits = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n\xEF\xBB\xBF{\"a\"}";
        let text = fetch_capped(fits, 16).await.unwrap();
        assert_eq!(text, "{\"a\"}");
    }
}
