// ------------ Waiting Out Failures ------------
// Waits out a lost connection once for every stream, and notices when one server stays down while the internet works.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::files::{host_unreachable_message, JobHooks, CANCELLED};
use crate::net;
use crate::runtime::TransferRuntime;

#[derive(Default)]
pub(crate) struct NetWait {
    down_since: Mutex<HashMap<String, Instant>>,
}

impl NetWait {
    pub(crate) fn clear(&self, url: &str) {
        let mut down = self.down_since.lock();
        if !down.is_empty() {
            down.remove(&net::url_host(url));
        }
    }

    fn down_for(&self, host: &str) -> Duration {
        let now = Instant::now();
        let mut down = self.down_since.lock();
        let since = *down.entry(host.to_string()).or_insert(now);
        now.saturating_duration_since(since)
    }

    pub(crate) async fn wait(&self, rt: &TransferRuntime, hooks: &dyn JobHooks, url: &str) -> Result<(), String> {
        let waited = rt
            .outage
            .single_flight(|| async {
                hooks.network_waiting(true);
                let outcome = self.wait_inner(rt, hooks, url).await;
                hooks.network_waiting(false);
                outcome
            })
            .await;
        match waited {
            Some(outcome) => outcome,
            None if hooks.gate().is_cancelled() => Err(CANCELLED.to_string()),
            None => Ok(()),
        }
    }

    async fn wait_inner(&self, rt: &TransferRuntime, hooks: &dyn JobHooks, url: &str) -> Result<(), String> {
        log::warn!("Download paused, waiting for the internet connection to return.");
        let host = net::url_host(url);
        let mut rounds = 0u32;
        while !hooks.gate().is_cancelled() {
            tokio::time::sleep(rt.config.network_recheck).await;
            if hooks.can_reach(url).await {
                log::info!("Internet connection restored, resuming download.");
                self.clear(url);
                return Ok(());
            }
            if hooks.reaches_home().await {
                let down_for = self.down_for(&host);
                if down_for >= rt.config.host_outage_budget {
                    log::warn!(
                        "{host} has been unreachable for {}s while the internet connection works.",
                        down_for.as_secs()
                    );
                    return Err(host_unreachable_message(&host));
                }
            } else {
                self.down_since.lock().remove(&host);
            }
            rounds += 1;
            if rounds >= rt.config.network_probe_rounds {
                log::info!("The connection probe to {host} still fails, so the download itself is tried again.");
                return Ok(());
            }
        }
        Err(CANCELLED.to_string())
    }
}

pub(crate) async fn next_attempt(
    rt: &TransferRuntime,
    hooks: &dyn JobHooks,
    net_wait: &NetWait,
    url: &str,
    error: String,
    epoch: u64,
    attempt: &mut u32,
) -> Result<(), String> {
    use crate::files::is_host_outage;
    use crate::fsx::{classify, FailureKind};

    if hooks.gate().is_cancelled() {
        return Err(CANCELLED.to_string());
    }
    if net::permanent_client_status(&error).is_some() {
        return if hooks.on_refused(url) { Ok(()) } else { Err(error) };
    }
    if is_host_outage(&error) {
        return if hooks.on_outage(url) { Ok(()) } else { Err(error) };
    }
    if hooks.gate().is_stopped() {
        hooks.gate().wait_while_stopped().await;
        return Ok(());
    }
    if hooks.gate().transfer_epoch() != epoch {
        return Ok(());
    }
    let host = net::url_host(url);
    if pushes_back(&error) {
        rt.host_overloaded(&host);
    }
    if is_protocol_error(&error) {
        rt.prefer_multiplexed(&host);
    }
    if classify(&error) == FailureKind::Network && !hooks.can_reach(url).await {
        return match net_wait.wait(rt, hooks, url).await {
            Err(waited) if is_host_outage(&waited) && hooks.on_outage(url) => Ok(()),
            outcome => outcome,
        };
    }
    *attempt += 1;
    if *attempt >= rt.config.max_retries {
        return Err(error);
    }
    log::warn!(
        "A request to {host} failed (attempt {attempt}/{}): {error}, retrying.",
        rt.config.max_retries
    );
    tokio::time::sleep(net::retry_delay(*attempt, rt.config.retry_base_ms)).await;
    Ok(())
}

pub(crate) fn pushes_back(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    error.starts_with("HTTP Error: 429")
        || error.starts_with("HTTP Error: 503")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
}

pub(crate) fn is_protocol_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("invalid http version") || lower.contains("received http/2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_down_since_long_ago_runs_out_its_budget_and_clearing_starts_over() {
        let wait = NetWait::default();
        let host = "cdn.example.com";
        assert_eq!(wait.down_for(host), Duration::ZERO);
        wait.down_since
            .lock()
            .insert(host.to_string(), Instant::now() - Duration::from_secs(600));
        assert!(wait.down_for(host) >= Duration::from_secs(600));
        assert_eq!(wait.down_for("other.example.com"), Duration::ZERO);
        wait.clear("https://cdn.example.com/a.pak");
        assert!(wait.down_for(host) < Duration::from_secs(1));
    }
}
