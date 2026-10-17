// ------------ Overlay Shared Layout ------------
// The overlay shared memory: hotkey bindings and press counters, helper status and errors, and whether the game is
// in front. The launcher and helper must agree on this layout exactly, so it is checked at compile time.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::common::errbuf;

pub const SECTION_NAME: &str = "Local\\Peebify.Overlay.v1";
pub const SIGNAL_TO_LAUNCHER: &str = "Local\\Peebify.Overlay.v1.Up";
pub const SIGNAL_TO_HELPER: &str = "Local\\Peebify.Overlay.v1.Down";

pub const SECTION_SIZE: usize = 16384;
pub const MAGIC: u32 = u32::from_le_bytes(*b"POVL");
pub const LAYOUT_VERSION: u32 = 3;
pub const ERROR_CAPACITY: usize = 256;

pub const STATUS_IDLE: u32 = 0;
pub const STATUS_STARTING: u32 = 1;
pub const STATUS_RUNNING: u32 = 2;
pub const STATUS_FAILED: u32 = 3;

#[repr(C, align(8))]
pub struct OverlayShared {
    pub magic: AtomicU32,
    pub layout_version: AtomicU32,
    pub status: AtomicU32,
    pub sample_seq: AtomicU32,

    pub game_pid: AtomicU32,
    pub helper_protocol: AtomicU32,
    pub _reserved_game_hwnd: AtomicU64,
    pub stop: AtomicU32,
    pub hotkey_vk: [AtomicU32; HOTKEY_COUNT],
    pub hotkey_mods: [AtomicU32; HOTKEY_COUNT],
    pub hotkey_generation: AtomicU32,
    pub _reserved_reload_epoch: AtomicU32,

    pub hotkey_epoch: [AtomicU32; HOTKEY_COUNT],

    pub error: [AtomicU8; ERROR_CAPACITY],

    pub hotkey_registered: AtomicU32,

    pub hotkey_suspend: AtomicU32,

    pub game_in_front: AtomicU32,
}

pub const HOTKEY_TOGGLE: usize = 0;
pub const HOTKEY_SHOT: usize = 1;
pub const HOTKEY_RECORD: usize = 2;
pub const HOTKEY_COUNT: usize = 3;

const _: () = assert!(core::mem::size_of::<OverlayShared>() <= SECTION_SIZE);

pub const HELPER_PROTOCOL: u32 = MAGIC
    ^ (core::mem::size_of::<OverlayShared>() as u32)
    ^ ((core::mem::offset_of!(OverlayShared, hotkey_epoch) as u32) << 16)
    ^ ((core::mem::offset_of!(OverlayShared, game_in_front) as u32) << 4);

const _: () = {
    assert!(core::mem::offset_of!(OverlayShared, magic) == 0);
    assert!(core::mem::offset_of!(OverlayShared, layout_version) == 4);
    assert!(core::mem::offset_of!(OverlayShared, status) == 8);
    assert!(core::mem::offset_of!(OverlayShared, sample_seq) == 12);
    assert!(core::mem::offset_of!(OverlayShared, game_pid) == 16);
    assert!(core::mem::offset_of!(OverlayShared, helper_protocol) == 20);
    assert!(core::mem::offset_of!(OverlayShared, _reserved_game_hwnd) == 24);
    assert!(core::mem::offset_of!(OverlayShared, stop) == 32);
    assert!(core::mem::offset_of!(OverlayShared, _reserved_reload_epoch) == 64);
    assert!(core::mem::offset_of!(OverlayShared, hotkey_epoch) == 68);
};

impl OverlayShared {
    pub fn initialize(&self) {
        self.status.store(STATUS_IDLE, Ordering::Relaxed);
        self.sample_seq.store(0, Ordering::Relaxed);
        self.game_pid.store(0, Ordering::Relaxed);
        self.stop.store(0, Ordering::Relaxed);
        self.hotkey_generation.store(0, Ordering::Relaxed);
        self.hotkey_registered.store(0, Ordering::Relaxed);
        self.helper_protocol.store(0, Ordering::Relaxed);
        self.game_in_front.store(0, Ordering::Relaxed);
        for i in 0..HOTKEY_COUNT {
            self.hotkey_vk[i].store(0, Ordering::Relaxed);
            self.hotkey_mods[i].store(0, Ordering::Relaxed);
            self.hotkey_epoch[i].store(0, Ordering::Relaxed);
        }
        self.clear_error();
        self.layout_version.store(LAYOUT_VERSION, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn reset_session(&self) {
        self.stop.store(0, Ordering::Relaxed);
        self.status.store(STATUS_IDLE, Ordering::Relaxed);
        self.helper_protocol.store(0, Ordering::Relaxed);
        self.hotkey_registered.store(0, Ordering::Relaxed);
        self.hotkey_suspend.store(0, Ordering::Relaxed);
        self.clear_error();
        self.layout_version.store(LAYOUT_VERSION, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
            && self.layout_version.load(Ordering::Relaxed) == LAYOUT_VERSION
    }

    pub fn clear_error(&self) {
        errbuf::clear(&self.error);
    }

    pub fn fail(&self, message: &str) {
        self.set_error(message);
        self.status.store(STATUS_FAILED, Ordering::Release);
    }

    pub fn set_error(&self, message: &str) {
        errbuf::write(&self.error, message);
    }

    pub fn error_message(&self) -> String {
        errbuf::read(&self.error)
    }
}

#[cfg(windows)]
impl crate::common::section::Layout for OverlayShared {
    const NAME: &'static str = SECTION_NAME;
    const SIZE: usize = SECTION_SIZE;
    fn initialize(&self) {
        OverlayShared::initialize(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_clears_what_the_last_helper_left_but_keeps_hotkey_epochs() {
        let shared: Box<OverlayShared> = Box::new(unsafe { core::mem::zeroed() });
        shared.initialize();
        shared.stop.store(1, Ordering::Relaxed);
        shared.status.store(STATUS_RUNNING, Ordering::Relaxed);
        shared.helper_protocol.store(HELPER_PROTOCOL, Ordering::Relaxed);
        shared.hotkey_registered.store(0b101, Ordering::Relaxed);
        shared.hotkey_suspend.store(1, Ordering::Relaxed);
        shared.game_in_front.store(0, Ordering::Relaxed);
        shared.hotkey_epoch[2].store(7, Ordering::Relaxed);
        shared.hotkey_vk[0].store(0x50, Ordering::Relaxed);
        shared.fail("old helper");

        shared.reset_session();

        assert_eq!(shared.stop.load(Ordering::Relaxed), 0);
        assert_eq!(shared.status.load(Ordering::Relaxed), STATUS_IDLE);
        assert_eq!(shared.helper_protocol.load(Ordering::Relaxed), 0);
        assert_eq!(shared.hotkey_registered.load(Ordering::Relaxed), 0);
        assert_eq!(shared.hotkey_suspend.load(Ordering::Relaxed), 0);
        assert_eq!(shared.game_in_front.load(Ordering::Relaxed), 0);
        assert_eq!(shared.error_message(), "");
        assert_eq!(shared.hotkey_epoch[2].load(Ordering::Relaxed), 7);
        assert_eq!(shared.hotkey_vk[0].load(Ordering::Relaxed), 0x50);
        assert!(shared.is_valid());
    }

    #[test]
    fn a_fresh_section_holds_no_shortcuts_until_the_game_is_in_front() {
        let shared: Box<OverlayShared> = Box::new(unsafe { core::mem::zeroed() });
        shared.game_in_front.store(1, Ordering::Relaxed);
        shared.initialize();
        assert_eq!(shared.game_in_front.load(Ordering::Relaxed), 0);
    }
}
