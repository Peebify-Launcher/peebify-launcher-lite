import { describe, expect, it, vi } from "vitest";

let inFlight = 0;
let peak = 0;
const push = vi.fn();

vi.mock("../lib/ipc", () => ({
  listCaptures: vi.fn(async () => ({ ok: true, items: [], folder: "" })),
  deleteCapture: vi.fn(async (path: string) => {
    inFlight += 1;
    peak = Math.max(peak, inFlight);
    await new Promise((r) => setTimeout(r, 1));
    inFlight -= 1;
    return path.includes("locked") ? { ok: false, error: "locked" } : { ok: true };
  }),
}));
vi.mock("./notifications", () => ({
  useNotificationStore: { getState: () => ({ push }) },
}));

const { useMediaStore } = await import("./mediaStore");

describe("mediaStore remove", () => {
  it("deletes a few at a time and counts every result", async () => {
    const paths = Array.from({ length: 23 }, (_, i) => (i === 5 ? "locked.png" : `${i}.png`));
    useMediaStore.setState({ selected: Object.fromEntries(paths.map((p) => [p, true])) });

    const deleted = await useMediaStore.getState().remove(paths);

    expect(deleted).toBe(22);
    expect(peak).toBeGreaterThan(1);
    expect(peak).toBeLessThanOrEqual(4);
    expect(push).toHaveBeenCalledTimes(1);
    expect(useMediaStore.getState().selected).toEqual({});
  });
});
