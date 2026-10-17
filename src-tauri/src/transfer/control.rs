// ------------ Pause, Cancel and the Gaming Hold ------------
// The controls a running download obeys: the user's pause and cancel, and the hold that stops game downloads while a game runs.
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

pub trait Control: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn gate(&self) -> Option<&RunGate> {
        None
    }
    fn is_paused(&self) -> bool {
        self.gate().is_some_and(RunGate::is_paused)
    }
    fn wait_if_paused(&self) {
        while self.is_paused() && !self.is_cancelled() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

pub async fn wait_while_paused<C: Control + ?Sized>(control: &C) {
    if let Some(gate) = control.gate() {
        gate.wait_while_paused().await;
        return;
    }
    while control.is_paused() && !control.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

pub enum FileEvent<'a> {
    Bytes { path: &'a str, delta: u64 },
    Written { delta: u64 },
    FileDone { path: &'a str },
}

pub trait FileHooks: Control {
    fn event(&self, event: FileEvent);
    fn status(&self, _message: &str) {}
    fn rebase_url(&self, _url: &str) -> Option<String> {
        None
    }
}

pub fn check_cancel(control: &dyn Control, message: &str) -> Result<(), String> {
    if control.is_cancelled() {
        return Err(message.to_string());
    }
    control.wait_if_paused();
    if control.is_cancelled() {
        return Err(message.to_string());
    }
    Ok(())
}

pub struct Hold {
    held: AtomicBool,
    epoch: AtomicU64,
    released: Notify,
    wanted: parking_lot::Mutex<u64>,
}

impl Hold {
    const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            released: Notify::const_new(),
            wanted: parking_lot::Mutex::new(0),
        }
    }

    pub fn begin_after(&'static self, grace: Duration) {
        let generation = {
            let mut wanted = self.wanted.lock();
            *wanted += 1;
            if grace.is_zero() {
                self.set(true);
                return;
            }
            *wanted
        };
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            let wanted = self.wanted.lock();
            if *wanted == generation {
                self.set(true);
            }
        });
    }

    pub fn end(&self) {
        let mut wanted = self.wanted.lock();
        *wanted += 1;
        self.set(false);
    }

    pub fn set(&self, held: bool) {
        let was = self.held.swap(held, Ordering::SeqCst);
        if held && !was {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
        if was && !held {
            self.released.notify_waiters();
        }
    }

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

pub static GAMING_HOLD: Hold = Hold::new();

pub struct RunGate {
    paused: AtomicBool,
    cancelled: Arc<AtomicBool>,
    pause_gate: Notify,
    cancel_gate: Notify,
    pause_epoch: AtomicU64,
    holdable: bool,
    hold_override: AtomicU64,
}

impl Default for RunGate {
    fn default() -> Self {
        Self::new()
    }
}

impl RunGate {
    pub fn new() -> Self {
        Self::build(true)
    }

    pub fn sharing(cancelled: Arc<AtomicBool>, holdable: bool) -> Self {
        Self {
            cancelled,
            ..Self::build(holdable)
        }
    }

    fn build(holdable: bool) -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            pause_gate: Notify::new(),
            cancel_gate: Notify::new(),
            pause_epoch: AtomicU64::new(0),
            holdable,
            hold_override: AtomicU64::new(0),
        }
    }

    pub fn pause_epoch(&self) -> u64 {
        self.pause_epoch.load(Ordering::SeqCst)
    }

    pub fn transfer_epoch(&self) -> u64 {
        self.pause_epoch().wrapping_add(GAMING_HOLD.epoch())
    }

    pub fn flag(&self) -> &Arc<AtomicBool> {
        &self.cancelled
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_held(&self) -> bool {
        self.holdable && GAMING_HOLD.is_held() && !self.hold_overridden()
    }

    pub fn is_stopped(&self) -> bool {
        self.is_paused() || self.is_held()
    }

    pub fn set_hold_override(&self, keep_going: bool) {
        let value = if keep_going { GAMING_HOLD.epoch() + 1 } else { 0 };
        self.hold_override.store(value, Ordering::SeqCst);
        if keep_going {
            GAMING_HOLD.released.notify_waiters();
        }
    }

    pub fn hold_overridden(&self) -> bool {
        self.hold_override.load(Ordering::SeqCst) == GAMING_HOLD.epoch() + 1
    }

    pub fn reset(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.cancelled.store(false, Ordering::SeqCst);
        self.hold_override.store(0, Ordering::SeqCst);
    }

    pub fn clear_paused(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn pause(&self) -> bool {
        let newly = !self.paused.swap(true, Ordering::SeqCst);
        if newly {
            self.pause_epoch.fetch_add(1, Ordering::SeqCst);
        }
        newly
    }

    pub fn resume(&self) -> bool {
        let was_paused = self.paused.swap(false, Ordering::SeqCst);
        if was_paused {
            self.pause_gate.notify_waiters();
        }
        was_paused
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel_gate.notify_waiters();
        self.pause_gate.notify_waiters();
        GAMING_HOLD.released.notify_waiters();
    }

    pub async fn wait_while_paused(&self) {
        loop {
            let resumed = self.pause_gate.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if !self.is_paused() || self.is_cancelled() {
                return;
            }
            resumed.await;
        }
    }

    pub async fn wait_while_stopped(&self) {
        loop {
            let resumed = self.pause_gate.notified();
            let released = GAMING_HOLD.released.notified();
            tokio::pin!(resumed);
            tokio::pin!(released);
            resumed.as_mut().enable();
            released.as_mut().enable();
            if !self.is_stopped() || self.is_cancelled() {
                return;
            }
            tokio::select! {
                _ = resumed => {}
                _ = released => {}
            }
        }
    }

    pub async fn cancellable<T, F>(&self, operation: F) -> Result<T, String>
    where
        F: Future<Output = Result<T, String>>,
    {
        tokio::pin!(operation);
        loop {
            let cancelled = self.cancel_gate.notified();
            tokio::pin!(cancelled);
            cancelled.as_mut().enable();
            if self.is_cancelled() {
                return Err("cancelled".to_string());
            }
            tokio::select! {
                outcome = &mut operation => return outcome,
                _ = cancelled => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_overridden_gate_ignores_the_hold_and_an_unheld_gate_never_sees_it() {
        let gate = RunGate::new();
        let mods = RunGate::sharing(Arc::default(), false);
        let epoch = gate.transfer_epoch();
        GAMING_HOLD.set(true);
        assert!(gate.is_held() && gate.is_stopped());
        assert!(!mods.is_held());
        assert_ne!(gate.transfer_epoch(), epoch);
        assert!(!gate.is_paused(), "the hold is not the user's pause");

        gate.set_hold_override(true);
        assert!(!gate.is_stopped());
        gate.wait_while_stopped().await;

        GAMING_HOLD.set(false);
        GAMING_HOLD.set(true);
        assert!(gate.is_held(), "keeping on lasts until that game closes, not through the next");
        GAMING_HOLD.set(false);
        gate.set_hold_override(true);
        gate.reset();
        assert!(!gate.hold_overridden());
    }

    #[test]
    fn the_hold_starts_after_its_grace_and_never_for_a_game_that_closes_within_it() {
        let _hold = crate::testing::hold_lock();
        let grace = Duration::from_millis(150);
        GAMING_HOLD.begin_after(grace);
        assert!(!GAMING_HOLD.is_held(), "downloads keep going through the grace");
        GAMING_HOLD.end();
        std::thread::sleep(grace * 2);
        assert!(!GAMING_HOLD.is_held(), "the game closed within the grace");

        GAMING_HOLD.begin_after(grace);
        GAMING_HOLD.end();
        GAMING_HOLD.begin_after(grace);
        std::thread::sleep(grace * 2);
        assert!(GAMING_HOLD.is_held(), "a game that kept running holds downloads");
        GAMING_HOLD.end();
        assert!(!GAMING_HOLD.is_held());
    }
}
