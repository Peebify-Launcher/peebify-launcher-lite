// ------------ Network Helpers ------------
// Client settings for file bodies, error descriptions, retry delays and the gate that waits out a lost connection once.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

const MAX_RETRY_DELAY_MS: u64 = 30_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub fn bulk_client(builder: reqwest::ClientBuilder, user_agent: &str) -> reqwest::ClientBuilder {
    builder
        .http1_only()
        .connect_timeout(CONNECT_TIMEOUT)
        .no_gzip()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .user_agent(user_agent)
}

pub fn multiplexed_client(builder: reqwest::ClientBuilder, user_agent: &str) -> reqwest::ClientBuilder {
    builder
        .connect_timeout(CONNECT_TIMEOUT)
        .no_gzip()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(90))
        .http2_adaptive_window(true)
        .user_agent(user_agent)
}

const INTERRUPTED_CAUSE: &str = "the connection was interrupted";

fn cause_text(cause: &(dyn std::error::Error + 'static)) -> String {
    let text = cause.to_string();
    if text.to_ascii_lowercase().contains("cancel") {
        INTERRUPTED_CAUSE.to_string()
    } else {
        text
    }
}

fn causes(error: &reqwest::Error) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause_text(cause);
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
        source = cause.source();
    }
    out
}

fn join_causes(head: String, causes: &[String]) -> String {
    let mut text = head;
    for cause in causes {
        if !text.contains(cause.as_str()) {
            text.push_str(": ");
            text.push_str(cause);
        }
    }
    text
}

pub fn describe(error: &reqwest::Error) -> String {
    join_causes(error.to_string(), &causes(error))
}

pub fn describe_short(error: &reqwest::Error) -> String {
    let inner: Vec<String> = causes(error).pop().into_iter().collect();
    join_causes(error.to_string(), &inner)
}

pub fn note_protocol(url: &str, version: reqwest::Version) {
    static SEEN: OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let host = url_host(url);
    let key = format!("{host} {version:?}");
    if SEEN
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .insert(key)
    {
        log::info!("transfer: {host} answered over {version:?}");
    }
}

pub fn url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "unknown host".to_string())
}

pub fn retry_delay(attempt: u32, base_ms: u64) -> Duration {
    let exp = base_ms
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(10))
        .min(MAX_RETRY_DELAY_MS);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter = 0.5 + f64::from(nanos % 1000) / 1000.0;
    Duration::from_millis((exp as f64 * jitter) as u64)
}

pub fn permanent_client_status(error: &str) -> Option<u16> {
    let rest = error
        .strip_prefix("HTTP Error: ")
        .or_else(|| error.strip_prefix("HTTP "))?;
    let code: u16 = rest.split_whitespace().next()?.parse().ok()?;
    ((400..500).contains(&code) && !matches!(code, 408 | 425 | 429)).then_some(code)
}

pub fn host_and_port(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = match parsed.host()? {
        url::Host::Domain(domain) => domain.to_string(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    };
    Some((host, parsed.port_or_known_default()?))
}

pub async fn connects(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

pub struct OutageGate {
    held: tokio::sync::Mutex<()>,
    epoch: AtomicU64,
}

impl Default for OutageGate {
    fn default() -> Self {
        Self::new()
    }
}

impl OutageGate {
    pub const fn new() -> Self {
        Self {
            held: tokio::sync::Mutex::const_new(()),
            epoch: AtomicU64::new(0),
        }
    }

    pub async fn single_flight<T, F, Fut>(&self, wait: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let seen = self.epoch.load(Ordering::SeqCst);
        let _held = self.held.lock().await;
        if self.epoch.load(Ordering::SeqCst) != seen {
            return None;
        }
        let out = wait().await;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        Some(out)
    }

    pub fn is_waiting(&self) -> bool {
        self.held.try_lock().is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_permanent_client_errors_stop_the_retries() {
        for (error, code) in [
            ("HTTP Error: 404 for URL https://cdn.example.com/a.pak", 404),
            ("HTTP Error: 403 for URL https://cdn.example.com/a.pak", 403),
            ("HTTP Error: 410 for URL https://cdn.example.com/a.pak", 410),
            ("HTTP 404 for https://cdn.example.com/a.pak", 404),
        ] {
            assert_eq!(permanent_client_status(error), Some(code), "{error}");
        }
        for error in [
            "HTTP Error: 408 for URL https://cdn.example.com/a.pak",
            "HTTP Error: 425 for URL https://cdn.example.com/a.pak",
            "HTTP Error: 429 for URL https://cdn.example.com/a.pak",
            "HTTP Error: 500 for URL https://cdn.example.com/a.pak",
            "HTTP Error: 503 for URL https://cdn.example.com/a.pak",
            "HTTP 206 for https://cdn.example.com/a.pak",
            "Request error: operation timed out",
            "Stream error: connection reset",
            "Write error: HTTP 404",
            "HTTP Error: abc",
        ] {
            assert_eq!(permanent_client_status(error), None, "{error}");
        }
    }

    #[test]
    fn causes_are_appended_once_and_never_read_as_a_cancel() {
        let causes = vec![
            "client error (Connect)".to_string(),
            "dns error".to_string(),
            "dns error".to_string(),
        ];
        assert_eq!(
            join_causes("error sending request".to_string(), &causes),
            "error sending request: client error (Connect): dns error"
        );
        let canceled = std::io::Error::other("operation was canceled");
        let text = join_causes("error sending request".to_string(), &[cause_text(&canceled)]);
        assert_eq!(
            crate::fsx::classify(&format!("Request error: {text}")),
            crate::fsx::FailureKind::Network
        );
    }

    #[test]
    fn a_probe_target_comes_from_the_url_host() {
        assert_eq!(
            host_and_port("https://autopatch.example.com/a/b.pak"),
            Some(("autopatch.example.com".to_string(), 443))
        );
        assert_eq!(
            host_and_port("http://127.0.0.1:3010/x"),
            Some(("127.0.0.1".to_string(), 3010))
        );
        assert_eq!(host_and_port("http://[::1]/x"), Some(("::1".to_string(), 80)));
        assert_eq!(host_and_port("not a url"), None);
    }

    #[tokio::test]
    async fn only_the_first_worker_waits_out_an_outage() {
        let gate = std::sync::Arc::new(OutageGate::new());
        let waits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        let first = {
            let gate = std::sync::Arc::clone(&gate);
            let waits = std::sync::Arc::clone(&waits);
            tokio::spawn(async move {
                gate.single_flight(|| async {
                    waits.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                })
                .await
            })
        };
        started_rx.await.unwrap();
        let second = {
            let gate = std::sync::Arc::clone(&gate);
            let waits = std::sync::Arc::clone(&waits);
            tokio::spawn(async move {
                gate.single_flight(|| async {
                    waits.fetch_add(1, Ordering::SeqCst);
                })
                .await
            })
        };
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        release_tx.send(()).unwrap();

        assert_eq!(first.await.unwrap(), Some(()));
        assert_eq!(second.await.unwrap(), None);
        assert_eq!(waits.load(Ordering::SeqCst), 1);
        assert_eq!(gate.single_flight(|| async { 7 }).await, Some(7));
    }
}
