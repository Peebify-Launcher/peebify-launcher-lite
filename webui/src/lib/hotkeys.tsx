// ------------ Hotkey Helpers ------------
// Turns a key press into a shortcut string like "Ctrl+Shift+P" for the shortcut pickers, and rejects
// combinations that can't be used as a global shortcut. Also shows a shortcut's keys, and keeps the overlay's own
// shortcuts paused while a picker listens. Used by the launcher's Overlay settings and the overlay itself.
import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from "react";
import { t } from "./i18n";
import { suspendOverlayHotkeys } from "./ipc";

const MODIFIER_KEYS: ReadonlySet<string> = new Set([
  "Control",
  "Alt",
  "AltGraph",
  "Shift",
  "Meta",
  "OS",
  "CapsLock",
  "NumLock",
  "ScrollLock",
]);
const NAMED_CODES = new Set([
  "Space",
  "Tab",
  "Insert",
  "Delete",
  "Home",
  "End",
  "PageUp",
  "PageDown",
  "ArrowUp",
  "ArrowDown",
  "ArrowLeft",
  "ArrowRight",
  "Enter",
  "Backspace",
]);

function functionKey(text: string): string | null {
  const match = /^F(\d{1,2})$/.exec(text);
  if (!match) return null;
  const n = Number(match[1]);
  return n >= 1 && n <= 24 ? `F${n}` : null;
}

function keyName(e: KeyboardEvent): string | null {
  if (/^[a-z]$/i.test(e.key)) return e.key.toUpperCase();
  const fn = functionKey(e.code) ?? functionKey(e.key);
  if (fn) return fn;
  const letter = /^Key([A-Z])$/.exec(e.code);
  if (letter) return letter[1];
  const digit = /^Digit(\d)$/.exec(e.code);
  if (digit) return digit[1];
  if (NAMED_CODES.has(e.code)) return e.code;
  return null;
}

export function acceleratorOf(e: KeyboardEvent): string | null {
  if (MODIFIER_KEYS.has(e.key)) return null;

  const key = keyName(e);
  if (!key) return null;

  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Win");

  const fn = functionKey(key) !== null;
  if (!parts.length && !fn) return null;
  parts.push(key);
  return parts.join("+");
}

export function captureHint(e: KeyboardEvent): string | null {
  if (MODIFIER_KEYS.has(e.key)) return null;
  return e.ctrlKey || e.altKey || e.shiftKey || e.metaKey
    ? t("actions.shortcut.blockedKey")
    : t("actions.shortcut.needsModifier");
}

function acceleratorKeys(accelerator: string | null | undefined): string[] {
  if (!accelerator) return [];
  return accelerator
    .split(/\+(?=.)/)
    .map((key) => key.trim())
    .filter(Boolean);
}

export function spacedAccelerator(accelerator: string): string {
  return acceleratorKeys(accelerator).join(" + ");
}

export function Keys({ accelerator, compact = false }: { accelerator: string; compact?: boolean }) {
  if (compact) {
    return (
      <span className="ov-mono rounded-[5px] bg-white/[0.09] px-[7px] py-[3px] text-[10.5px] text-white/[0.72]">
        {spacedAccelerator(accelerator)}
      </span>
    );
  }
  return acceleratorKeys(accelerator).map((key, i) => (
    <kbd
      key={i}
      className="inline-grid h-6 min-w-[26px] place-items-center rounded-[6px] border border-b-2 border-white/20 bg-white/[0.08] px-[7px] font-sans text-[12px] font-semibold leading-none"
    >
      {key}
    </kbd>
  ));
}

// ------------ Shortcut Capture ------------

export function useShortcutCapture(onEnd?: () => void) {
  const [capturing, setCapturing] = useState<string | null>(null);
  const onEndRef = useRef(onEnd);
  onEndRef.current = onEnd;

  const start = useCallback((id: string) => {
    setCapturing(id);
    void suspendOverlayHotkeys(true);
  }, []);

  const end = useCallback(() => {
    setCapturing(null);
    onEndRef.current?.();
    void suspendOverlayHotkeys(false);
  }, []);

  useEffect(() => () => void suspendOverlayHotkeys(false), []);

  useEffect(() => {
    if (!capturing) return;
    window.addEventListener("blur", end);
    const keepAlive = window.setInterval(() => void suspendOverlayHotkeys(true), 8000);
    return () => {
      window.removeEventListener("blur", end);
      window.clearInterval(keepAlive);
    };
  }, [capturing, end]);

  return { capturing, start, end };
}
