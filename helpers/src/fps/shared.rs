// ------------ FPS Shared Layout ------------
// The FPS unlocker shared memory: the launcher writes the target and background FPS, the helper and stub report
// status and errors, and the stub leaves a hint of where it found the value so the next launch can skip the scan.

use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::common::errbuf;

pub const SECTION_NAME: &str = "Local\\Peebify.FpsUnlock.v1";
pub const SIGNAL_EVENT_NAME: &str = "Local\\Peebify.FpsUnlock.v1.Signal";

pub const STUB_NAME: &str = "peebify_helpers.dll";

pub const SECTION_SIZE: usize = 4096;
pub const MAGIC: u32 = u32::from_le_bytes(*b"PFPS");
pub const LAYOUT_VERSION: u32 = 2;
pub const ERROR_CAPACITY: usize = 256;

pub const MIN_FPS: i32 = 10;
pub const MAX_FPS: i32 = 1000;

pub const STATUS_IDLE: u32 = 0;
pub const STATUS_LAUNCHING: u32 = 1;
pub const STATUS_INJECTING: u32 = 2;
pub const STATUS_ATTACHED: u32 = 3;
pub const STATUS_READY: u32 = 4;
pub const STATUS_FAILED: u32 = 5;

pub const STOP_NONE: u32 = 0;
pub const STOP_PAUSED: u32 = 1;
pub const STOP_ENDED: u32 = 2;

#[repr(C, align(8))]
pub struct Shared {
    pub magic: AtomicU32,
    pub layout_version: AtomicU32,

    pub target_fps: AtomicI32,
    pub background_fps: AtomicI32,
    pub stop: AtomicU32,
    pub _reserved_generation: AtomicU32,
    pub hint_rva: AtomicU64,
    pub hint_fingerprint: AtomicU64,

    pub game_pid: AtomicU32,

    pub status: AtomicU32,
    pub scan_micros: AtomicU32,
    pub resets: AtomicU32,
    pub found_rva: AtomicU64,
    pub found_fingerprint: AtomicU64,
    pub error: [AtomicU8; ERROR_CAPACITY],
}

const _: () = assert!(core::mem::size_of::<Shared>() <= SECTION_SIZE);

const _: () = {
    assert!(core::mem::offset_of!(Shared, stop) == 16);
    assert!(core::mem::offset_of!(Shared, _reserved_generation) == 20);
    assert!(core::mem::offset_of!(Shared, hint_rva) == 24);
    assert!(core::mem::offset_of!(Shared, game_pid) == 40);
    assert!(core::mem::offset_of!(Shared, found_rva) == 56);
    assert!(core::mem::offset_of!(Shared, error) == 72);
};

impl Shared {
    pub fn initialize(&self) {
        self.game_pid.store(0, Ordering::Relaxed);
        self.stop.store(STOP_NONE, Ordering::Release);
        self.status.store(STATUS_IDLE, Ordering::Relaxed);
        self.scan_micros.store(0, Ordering::Relaxed);
        self.resets.store(0, Ordering::Relaxed);
        self.found_rva.store(0, Ordering::Relaxed);
        self.found_fingerprint.store(0, Ordering::Relaxed);
        self.clear_error();
        self.layout_version.store(LAYOUT_VERSION, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
            && self.layout_version.load(Ordering::Relaxed) == LAYOUT_VERSION
    }

    pub fn is_configured(&self) -> bool {
        self.is_valid() && self.target_fps.load(Ordering::Relaxed) >= MIN_FPS
    }

    pub fn clear_error(&self) {
        errbuf::clear(&self.error);
    }

    pub fn fail(&self, message: &str) {
        errbuf::write(&self.error, message);
        self.status.store(STATUS_FAILED, Ordering::Release);
    }

    pub fn error_message(&self) -> String {
        errbuf::read(&self.error)
    }

    pub fn effective_fps(&self, foreground: bool) -> i32 {
        let background = self.background_fps.load(Ordering::Relaxed);
        let target = if foreground || background <= 0 {
            self.target_fps.load(Ordering::Relaxed)
        } else {
            background
        };
        target.clamp(MIN_FPS, MAX_FPS)
    }
}

pub fn fingerprint(time_date_stamp: u32, check_sum: u32, size_of_image: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for value in [time_date_stamp, check_sum, size_of_image] {
        hash ^= value as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash | 1
}

#[cfg(windows)]
impl crate::common::section::Layout for Shared {
    const NAME: &'static str = SECTION_NAME;
    const SIZE: usize = SECTION_SIZE;
    fn initialize(&self) {
        Shared::initialize(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Box<Shared> {
        Box::new(unsafe { core::mem::zeroed() })
    }

    #[test]
    fn only_a_section_with_a_published_target_is_configured() {
        let shared = blank();
        assert!(!shared.is_configured());
        shared.initialize();
        assert!(shared.is_valid());
        assert!(!shared.is_configured());
        shared.target_fps.store(MIN_FPS - 1, Ordering::Relaxed);
        assert!(!shared.is_configured());
        shared.target_fps.store(120, Ordering::Relaxed);
        assert!(shared.is_configured());
        assert_eq!(shared.effective_fps(true), 120);
        shared.magic.store(0, Ordering::Relaxed);
        assert!(!shared.is_configured());
    }
}
