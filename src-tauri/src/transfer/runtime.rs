// ------------ Transfer Runtime ------------
// The connection budget, per-server caps, speed limit and clients that every running job shares.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::limiter::RateLimiter;
use crate::net::OutageGate;

pub const AUTO_CONNECTIONS: usize = 24;
pub const MAX_CONNECTIONS: usize = 64;
const PER_HOST_CONNECTIONS: usize = 16;
const MIN_HOST_CONNECTIONS: usize = 2;
const HOST_CUT_COOLDOWN: Duration = Duration::from_secs(5);
const HOST_GROW_AFTER: Duration = Duration::from_secs(30);
pub const GAMING_SLOW_CONNECTIONS: usize = 4;
pub const GAMING_SLOW_BYTES_PER_SEC: u64 = 2_000_000;

pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub stall_timeout: Duration,
    pub checkpoint_interval: Duration,
    pub split_min_left: u64,
    pub preallocate_min: u64,
    pub max_retries: u32,
    pub retry_base_ms: u64,
    pub network_recheck: Duration,
    pub network_probe_rounds: u32,
    pub host_outage_budget: Duration,
    pub hold_grace: Duration,
    #[doc(hidden)]
    pub skip_final_checkpoint: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            stall_timeout: Duration::from_secs(15),
            checkpoint_interval: crate::ledger::CHECKPOINT_INTERVAL,
            split_min_left: 32 << 20,
            preallocate_min: 64 << 20,
            max_retries: 10,
            retry_base_ms: 1000,
            network_recheck: Duration::from_secs(3),
            network_probe_rounds: 10,
            host_outage_budget: Duration::from_secs(5 * 60),
            hold_grace: Duration::from_secs(10),
            skip_final_checkpoint: false,
        }
    }
}

pub struct Budget {
    slots: Arc<Semaphore>,
    target: AtomicUsize,
    issued: AtomicUsize,
    owed: AtomicUsize,
}

pub struct Slot {
    permit: Option<OwnedSemaphorePermit>,
    budget: Arc<Budget>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let owed = self
            .budget
            .owed
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if owed.is_ok() {
            permit.forget();
            self.budget.issued.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Budget {
    pub fn new(size: usize) -> Arc<Self> {
        let size = size.max(1);
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(size)),
            target: AtomicUsize::new(size),
            issued: AtomicUsize::new(size),
            owed: AtomicUsize::new(0),
        })
    }

    pub fn size(&self) -> usize {
        self.target.load(Ordering::SeqCst)
    }

    pub async fn acquire(self: &Arc<Self>) -> Slot {
        let permit = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .expect("budget semaphores are never closed");
        Slot {
            permit: Some(permit),
            budget: Arc::clone(self),
        }
    }

    pub fn resize(&self, size: usize) {
        let size = size.max(1);
        let before = self.target.swap(size, Ordering::SeqCst);
        if size == before {
            return;
        }
        if size > before {
            let mut grow = size - before;
            let repaid = self
                .owed
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(grow)))
                .unwrap_or(0);
            grow -= repaid.min(grow);
            if grow > 0 {
                self.issued.fetch_add(grow, Ordering::SeqCst);
                self.slots.add_permits(grow);
            }
        } else {
            let shrink = before - size;
            let forgotten = self.slots.forget_permits(shrink);
            self.issued.fetch_sub(forgotten, Ordering::SeqCst);
            self.owed.fetch_add(shrink - forgotten, Ordering::SeqCst);
        }
    }
}

struct HostCap {
    budget: Arc<Budget>,
    changed_at: Mutex<Instant>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum WhileGaming {
    #[default]
    Off,
    Slow,
    Pause,
}

impl WhileGaming {
    pub fn parse(value: &str) -> Self {
        match value {
            "slow" => Self::Slow,
            "pause" => Self::Pause,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    pub speed_limit: u64,
    pub connections: usize,
    pub while_gaming: WhileGaming,
}

pub struct TransferRuntime {
    bulk: reqwest::Client,
    multiplexed: reqwest::Client,
    pub config: RuntimeConfig,
    budget: Arc<Budget>,
    hosts: Mutex<HashMap<String, Arc<HostCap>>>,
    per_host: AtomicUsize,
    pub limiter: RateLimiter,
    pub outage: OutageGate,
    h2_hosts: Mutex<HashSet<String>>,
    settings: Mutex<Settings>,
    playing: std::sync::atomic::AtomicBool,
    holding: std::sync::atomic::AtomicBool,
}

impl TransferRuntime {
    pub fn new(bulk: reqwest::Client, multiplexed: reqwest::Client, config: RuntimeConfig) -> Arc<Self> {
        Arc::new(Self {
            bulk,
            multiplexed,
            config,
            budget: Budget::new(AUTO_CONNECTIONS),
            hosts: Mutex::new(HashMap::new()),
            per_host: AtomicUsize::new(PER_HOST_CONNECTIONS),
            limiter: RateLimiter::new(),
            outage: OutageGate::new(),
            h2_hosts: Mutex::new(HashSet::new()),
            settings: Mutex::new(Settings::default()),
            playing: std::sync::atomic::AtomicBool::new(false),
            holding: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn client_for(&self, host: &str) -> &reqwest::Client {
        if self.h2_hosts.lock().contains(host) {
            &self.multiplexed
        } else {
            &self.bulk
        }
    }

    pub fn prefer_multiplexed(&self, host: &str) {
        if self.h2_hosts.lock().insert(host.to_string()) {
            log::warn!("transfer: {host} fails over HTTP/1.1, so it is reached over HTTP/2 from now on.");
        }
    }

    pub fn connections(&self) -> usize {
        self.budget.size()
    }

    pub fn apply_settings(&self, settings: Settings) {
        *self.settings.lock() = settings;
        self.recompute();
    }

    pub fn settings(&self) -> Settings {
        *self.settings.lock()
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::SeqCst);
        self.recompute();
    }

    fn recompute(&self) {
        let settings = *self.settings.lock();
        let playing = self.playing.load(Ordering::SeqCst);
        let mut connections = match settings.connections {
            0 => AUTO_CONNECTIONS,
            n => n.min(MAX_CONNECTIONS),
        };
        let mut speed = settings.speed_limit;
        let slow = playing && settings.while_gaming == WhileGaming::Slow;
        if slow {
            connections = connections.min(GAMING_SLOW_CONNECTIONS);
            speed = match speed {
                0 => GAMING_SLOW_BYTES_PER_SEC,
                s => s.min(GAMING_SLOW_BYTES_PER_SEC),
            };
        }
        let hold = playing && settings.while_gaming == WhileGaming::Pause;
        if self.holding.swap(hold, Ordering::SeqCst) != hold {
            if hold {
                crate::control::GAMING_HOLD.begin_after(self.config.hold_grace);
            } else {
                crate::control::GAMING_HOLD.end();
            }
        }
        self.budget.resize(connections);
        self.limiter.set_limit(speed);
        let per_host = match settings.connections {
            0 => connections.min(PER_HOST_CONNECTIONS),
            _ => connections,
        };
        let before = self.per_host.swap(per_host, Ordering::SeqCst);
        for cap in self.hosts.lock().values() {
            let size = cap.budget.size();
            if size > per_host || (size == before && per_host > before) {
                cap.budget.resize(per_host);
            }
        }
    }

    fn host_cap(&self, host: &str) -> Arc<HostCap> {
        let mut hosts = self.hosts.lock();
        Arc::clone(hosts.entry(host.to_string()).or_insert_with(|| {
            Arc::new(HostCap {
                budget: Budget::new(self.per_host.load(Ordering::SeqCst)),
                changed_at: Mutex::new(Instant::now()),
            })
        }))
    }

    pub async fn connection(&self, host: &str) -> (Slot, Slot) {
        let cap = self.host_cap(host);
        let host_slot = cap.budget.acquire().await;
        let slot = self.budget.acquire().await;
        (host_slot, slot)
    }

    pub fn host_overloaded(&self, host: &str) {
        let cap = self.host_cap(host);
        let mut changed = cap.changed_at.lock();
        if changed.elapsed() < HOST_CUT_COOLDOWN {
            return;
        }
        let size = cap.budget.size();
        let next = (size / 2).max(MIN_HOST_CONNECTIONS);
        if next < size {
            log::warn!("transfer: {host} is pushing back, using {next} connections to it instead of {size}.");
            cap.budget.resize(next);
        }
        *changed = Instant::now();
    }

    pub fn host_ok(&self, host: &str) {
        let cap = self.host_cap(host);
        let limit = self.per_host.load(Ordering::SeqCst);
        if cap.budget.size() >= limit {
            return;
        }
        let mut changed = cap.changed_at.lock();
        if changed.elapsed() >= HOST_GROW_AFTER {
            cap.budget.resize(cap.budget.size() + 1);
            *changed = Instant::now();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_budget_shrinks_as_slots_come_back_and_grows_at_once() {
        let budget = Budget::new(3);
        let a = budget.acquire().await;
        let b = budget.acquire().await;
        let c = budget.acquire().await;
        budget.resize(1);
        drop(a);
        drop(b);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), budget.acquire()).await.is_err(),
            "two returned slots paid what was owed"
        );
        drop(c);
        let d = tokio::time::timeout(Duration::from_millis(20), budget.acquire())
            .await
            .expect("one slot is left");
        budget.resize(2);
        let e = tokio::time::timeout(Duration::from_millis(20), budget.acquire())
            .await
            .expect("growing adds a slot right away");
        drop((d, e));
        assert_eq!(budget.size(), 2);
    }

    fn runtime() -> Arc<TransferRuntime> {
        crate::testing::install_crypto();
        TransferRuntime::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            RuntimeConfig {
                hold_grace: Duration::ZERO,
                ..RuntimeConfig::default()
            },
        )
    }

    #[tokio::test]
    async fn slow_while_gaming_caps_connections_and_speed_and_pause_sets_the_hold() {
        let _hold = crate::testing::hold_lock();
        let rt = runtime();
        rt.apply_settings(Settings {
            speed_limit: 0,
            connections: 0,
            while_gaming: WhileGaming::Slow,
        });
        assert_eq!(rt.connections(), AUTO_CONNECTIONS);
        assert_eq!(rt.limiter.limit(), 0);
        rt.set_playing(true);
        assert_eq!(rt.connections(), GAMING_SLOW_CONNECTIONS);
        assert_eq!(rt.limiter.limit(), GAMING_SLOW_BYTES_PER_SEC);
        assert!(!crate::control::GAMING_HOLD.is_held());

        rt.apply_settings(Settings {
            speed_limit: 1_000_000,
            connections: 8,
            while_gaming: WhileGaming::Pause,
        });
        assert_eq!(rt.connections(), 8);
        assert_eq!(rt.limiter.limit(), 1_000_000);
        assert!(crate::control::GAMING_HOLD.is_held());
        rt.set_playing(false);
        assert!(!crate::control::GAMING_HOLD.is_held());
    }

    #[tokio::test]
    async fn a_host_that_pushes_back_gets_fewer_connections() {
        let rt = runtime();
        let connections = || rt.host_cap("cdn").budget.size();
        assert_eq!(connections(), PER_HOST_CONNECTIONS);
        rt.hosts.lock().get("cdn").unwrap().changed_at.lock().clone_from(
            &(Instant::now() - HOST_CUT_COOLDOWN),
        );
        rt.host_overloaded("cdn");
        assert_eq!(connections(), PER_HOST_CONNECTIONS / 2);
        rt.host_overloaded("cdn");
        assert_eq!(connections(), PER_HOST_CONNECTIONS / 2, "cuts are spaced out");
    }
}
