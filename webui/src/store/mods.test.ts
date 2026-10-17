import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ModEntry, ModsStatus } from "../lib/ipc";

const status = { supportsMods: true, masterEnabled: true } as ModsStatus;
const listed = new Map<string, { mods: ModEntry[] } | undefined>();
const pending = new Map<string, () => void>();

vi.mock("../lib/ipc", () => ({
  getModsMasterEnabled: vi.fn(async () => true),
  getModsStatus: vi.fn(
    (gameId: string) =>
      new Promise<ModsStatus>((resolve) => pending.set(gameId, () => resolve(status))),
  ),
  listMods: vi.fn(async (gameId: string) => listed.get(gameId)?.mods),
}));
vi.mock("../lib/gamebanana", () => ({
  backfillModThumbnails: vi.fn(async () => 0),
  checkModUpdates: vi.fn(async () => undefined),
}));

const { useModsStore } = await import("./mods");

const mod = (modId: string) =>
  ({ modId, name: modId, folderName: modId, enabled: true, source: { kind: "archive" } }) as ModEntry;

async function settle(gameId: string) {
  pending.get(gameId)?.();
  pending.delete(gameId);
  await new Promise((r) => setTimeout(r, 0));
}

describe("modsStore refresh", () => {
  beforeEach(() => {
    listed.clear();
    pending.clear();
    useModsStore.setState({ mods: [], modsError: false, loadedGameId: null, requestedGameId: null });
  });

  it("keeps the newer game when an older refresh finishes last", async () => {
    listed.set("wuwa", { mods: [mod("a")] });
    listed.set("zzz", { mods: [mod("b")] });
    const first = useModsStore.getState().refresh("wuwa");
    const second = useModsStore.getState().refresh("zzz");
    await settle("zzz");
    await settle("wuwa");
    await Promise.all([first, second]);

    const s = useModsStore.getState();
    expect(s.loadedGameId).toBe("zzz");
    expect(s.mods.map((m) => m.modId)).toEqual(["b"]);
  });

  it("refreshIfCurrent ignores a game the store has moved away from", async () => {
    listed.set("zzz", { mods: [] });
    const load = useModsStore.getState().refresh("zzz");
    await settle("zzz");
    await load;

    await useModsStore.getState().refreshIfCurrent("wuwa");
    expect(pending.has("wuwa")).toBe(false);
    expect(useModsStore.getState().requestedGameId).toBe("zzz");
  });

  it("flags a failed mods read instead of showing an empty folder", async () => {
    listed.set("wuwa", undefined);
    const load = useModsStore.getState().refresh("wuwa");
    await settle("wuwa");
    await load;

    const s = useModsStore.getState();
    expect(s.modsError).toBe(true);
    expect(s.mods).toEqual([]);
  });
});
