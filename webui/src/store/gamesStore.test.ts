import { beforeEach, describe, expect, it } from "vitest";
import type { GameUpdateVersions } from "../lib/ipc";
import { useGamesStore } from "./gamesStore";

const versions = {} as GameUpdateVersions;

describe("gamesStore.setInstalled", () => {
  beforeEach(() => {
    useGamesStore.setState({ installed: [], updates: [], updateVersions: {} });
  });

  it("drops the update state of a game that is no longer installed", () => {
    const store = useGamesStore.getState();
    store.setInstalled(["genshin", "hsr"]);
    store.setUpdate("genshin", true, versions);
    store.setUpdate("hsr", true, versions);

    useGamesStore.getState().setInstalled(["hsr"]);

    const s = useGamesStore.getState();
    expect(s.updates).toEqual(["hsr"]);
    expect(Object.keys(s.updateVersions)).toEqual(["hsr"]);
  });

  it("keeps the same update arrays when nothing was uninstalled", () => {
    useGamesStore.getState().setInstalled(["hsr"]);
    useGamesStore.getState().setUpdate("hsr", true, versions);
    const before = useGamesStore.getState();

    useGamesStore.getState().setInstalled(["hsr", "genshin"]);

    const after = useGamesStore.getState();
    expect(after.updates).toBe(before.updates);
    expect(after.updateVersions).toBe(before.updateVersions);
  });
});
