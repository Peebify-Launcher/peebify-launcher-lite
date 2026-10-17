// ------------ Overlay Hotkeys ------------
// Registers the overlay global hotkeys with Windows and handles their messages. Keys are let go while the game is not
// in front or while a binding is being captured, and each press bumps a counter the launcher watches.

use core::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_HOTKEY,
};

use crate::overlay::shared::{OverlayShared, HOTKEY_COUNT};
use crate::overlay::telemetry::log_line;

pub struct Registrations {
    registered: [bool; HOTKEY_COUNT],
    generation: u32,
    suspended: bool,
}

impl Default for Registrations {
    fn default() -> Self {
        Self {
            registered: [false; HOTKEY_COUNT],
            generation: u32::MAX,
            suspended: false,
        }
    }
}

impl Registrations {
    pub fn sync(&mut self, shared: &OverlayShared) {
        let reason = release_reason(
            shared.hotkey_suspend.load(Ordering::Acquire) != 0,
            shared.game_in_front.load(Ordering::Acquire) != 0,
        );
        let suspend = reason.is_some();
        if suspend != self.suspended {
            self.suspended = suspend;
            if let Some(reason) = reason {
                log_line(reason);
                self.unregister_all();
                return;
            }
            log_line("hotkeys resumed");
            self.generation = u32::MAX;
        }
        if self.suspended {
            return;
        }

        let generation = shared.hotkey_generation.load(Ordering::Acquire);
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        self.unregister_all();

        let mut bits = 0u32;
        let mut bound = Vec::new();
        for index in 0..HOTKEY_COUNT {
            let vk = shared.hotkey_vk[index].load(Ordering::Relaxed);
            if vk == 0 {
                continue;
            }
            let mods = shared.hotkey_mods[index].load(Ordering::Relaxed);
            let modifiers = (mods | MOD_NOREPEAT) as HOT_KEY_MODIFIERS;
            let ok = unsafe { RegisterHotKey(core::ptr::null_mut(), index as i32, modifiers, vk) };
            self.registered[index] = ok != 0;
            if ok != 0 {
                bits |= 1 << index;
                bound.push(format!("{index} {}", accelerator_label(vk, mods)));
            } else {
                let code = unsafe { GetLastError() };
                log_line(&format!(
                    "could not register hotkey {index} {} (vk {vk}, mods {mods}, error {code}). Another app may already own it",
                    accelerator_label(vk, mods)
                ));
            }
        }
        if !bound.is_empty() {
            log_line(&format!("hotkeys registered: {}", bound.join(", ")));
        }
        shared.hotkey_registered.store(bits, Ordering::Release);
    }

    pub fn unregister_all(&mut self) {
        for index in 0..HOTKEY_COUNT {
            if self.registered[index] {
                unsafe { UnregisterHotKey(core::ptr::null_mut(), index as i32) };
                self.registered[index] = false;
            }
        }
    }

    pub fn any_registered(&self) -> bool {
        self.registered.iter().any(|r| *r)
    }
}

fn release_reason(capturing: bool, game_in_front: bool) -> Option<&'static str> {
    if capturing {
        Some("hotkeys suspended while the overlay captures a binding")
    } else if !game_in_front {
        Some("hotkeys released while another app is in front")
    } else {
        None
    }
}

fn accelerator_label(vk: u32, mods: u32) -> String {
    const NAMES: [(u32, &str); 4] = [
        (0x0002, "Ctrl"),
        (0x0001, "Alt"),
        (0x0004, "Shift"),
        (0x0008, "Win"),
    ];
    let mut parts: Vec<String> = NAMES
        .iter()
        .filter(|(bit, _)| mods & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    parts.push(key_name(vk));
    parts.join("+")
}

fn key_name(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x41..=0x5A | 0x30..=0x39 => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x25 => "ArrowLeft".to_string(),
        0x26 => "ArrowUp".to_string(),
        0x27 => "ArrowRight".to_string(),
        0x28 => "ArrowDown".to_string(),
        0x0D => "Enter".to_string(),
        0x08 => "Backspace".to_string(),
        _ => format!("vk {vk}"),
    }
}

pub fn pump(shared: &OverlayShared) -> bool {
    let mut fired = false;
    let mut message: MSG = unsafe { core::mem::zeroed() };

    while unsafe { PeekMessageW(&mut message, core::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        if message.message == WM_HOTKEY {
            let index = message.wParam as usize;
            if index < HOTKEY_COUNT {
                shared.hotkey_epoch[index].fetch_add(1, Ordering::Release);
                log_line(&format!("hotkey {index} pressed"));
                fired = true;
            }
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    fired
}

pub fn allow_foreground(launcher_pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    if launcher_pid != 0 {
        unsafe { AllowSetForegroundWindow(launcher_pid) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_like_the_settings_accelerators() {
        assert_eq!(accelerator_label(0x50, 0x0001), "Alt+P");
        assert_eq!(accelerator_label(0x50, 0x0005), "Alt+Shift+P");
        assert_eq!(accelerator_label(0x53, 0x000F), "Ctrl+Alt+Shift+Win+S");
        assert_eq!(accelerator_label(0x79, 0), "F10");
        assert_eq!(accelerator_label(0x31, 0x0002), "Ctrl+1");
        assert_eq!(accelerator_label(0x22, 0x0001), "Alt+PageDown");
        assert_eq!(accelerator_label(0x26, 0x0001), "Alt+ArrowUp");
        assert_eq!(accelerator_label(0x0D, 0x0002), "Ctrl+Enter");
        assert_eq!(accelerator_label(0xBA, 0x0001), "Alt+vk 186");
    }

    #[test]
    fn keys_stay_registered_only_while_the_game_is_in_front_and_no_binding_is_captured() {
        assert_eq!(release_reason(false, true), None);
        assert!(release_reason(false, false).is_some_and(|r| r.contains("another app")));
        assert!(release_reason(true, true).is_some_and(|r| r.contains("captures")));
        assert!(release_reason(true, false).is_some_and(|r| r.contains("captures")));
    }
}
