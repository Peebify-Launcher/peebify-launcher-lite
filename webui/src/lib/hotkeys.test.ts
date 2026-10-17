import type { KeyboardEvent } from "react";
import { describe, expect, it } from "vitest";
import { acceleratorOf, captureHint, spacedAccelerator } from "./hotkeys";

function press(key: string, code: string, mods: Partial<KeyboardEvent> = {}): KeyboardEvent {
  return {
    key,
    code,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    metaKey: false,
    ...mods,
  } as KeyboardEvent;
}

describe("acceleratorOf", () => {
  it("takes function keys alone and other keys with a modifier", () => {
    expect(acceleratorOf(press("F9", "F9"))).toBe("F9");
    expect(acceleratorOf(press("k", "KeyK", { ctrlKey: true, shiftKey: true }))).toBe(
      "Ctrl+Shift+K",
    );
    expect(acceleratorOf(press("k", "KeyK"))).toBeNull();
  });
});

describe("spacedAccelerator", () => {
  it("keeps the plus key as a key of its own", () => {
    expect(spacedAccelerator("Ctrl+Shift+S")).toBe("Ctrl + Shift + S");
    expect(spacedAccelerator("Ctrl++")).toBe("Ctrl + +");
  });
});

describe("captureHint", () => {
  it("stays quiet while a modifier or lock key is held", () => {
    for (const [key, code] of [
      ["Control", "ControlLeft"],
      ["Alt", "AltLeft"],
      ["AltGraph", "AltRight"],
      ["Shift", "ShiftLeft"],
      ["Meta", "MetaLeft"],
      ["CapsLock", "CapsLock"],
      ["NumLock", "NumLock"],
      ["ScrollLock", "ScrollLock"],
    ]) {
      expect(acceleratorOf(press(key, code))).toBeNull();
      expect(captureHint(press(key, code))).toBeNull();
    }
  });

  it("asks for a modifier when a bare key is pressed", () => {
    expect(captureHint(press("k", "KeyK"))).toBe("Add Ctrl, Alt or Shift, or use a function key.");
  });

  it("says the key itself is unusable when a modifier was already held", () => {
    expect(captureHint(press("`", "Backquote", { ctrlKey: true }))).toMatch(
      /^That key cannot be used/,
    );
  });
});
