import { describe, expect, it } from "vitest";
import { radioGroupKeyTarget } from "./controls";

describe("radioGroupKeyTarget", () => {
  it("wraps arrow keys around both ends", () => {
    expect(radioGroupKeyTarget("ArrowRight", 2, 3)).toBe(0);
    expect(radioGroupKeyTarget("ArrowDown", 0, 3)).toBe(1);
    expect(radioGroupKeyTarget("ArrowLeft", 0, 3)).toBe(2);
    expect(radioGroupKeyTarget("ArrowUp", 2, 3)).toBe(1);
  });

  it("jumps to the ends with Home and End", () => {
    expect(radioGroupKeyTarget("Home", 2, 3)).toBe(0);
    expect(radioGroupKeyTarget("End", 0, 3)).toBe(2);
  });

  it("ignores other keys and empty groups", () => {
    expect(radioGroupKeyTarget("Enter", 1, 3)).toBeNull();
    expect(radioGroupKeyTarget("ArrowRight", 0, 0)).toBeNull();
  });
});
