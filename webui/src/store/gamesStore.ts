// ------------ Games Store ------------
// Which games are installed, running, launching, waiting on an install, or have an update available.
import { create } from "zustand";
import type { GameUpdateVersions, PendingInstall } from "../lib/ipc";
import type { ResourceQuality } from "../data/games";

interface GamesState {
  runningGames: string[];
  launchingGames: string[];
  launchingViaSteam: string[];
  installed: string[];
  pendingInstalls: Record<string, PendingInstall>;
  qualitySwitches: Record<string, ResourceQuality>;
  updates: string[];
  updateVersions: Record<string, GameUpdateVersions>;
  setRunningGames: (ids: string[]) => void;
  setLaunching: (id: string, launching: boolean, viaSteam?: boolean) => void;
  setInstalled: (ids: string[]) => void;
  setPendingInstalls: (pending: Record<string, PendingInstall>) => void;
  setQualitySwitches: (switches: Record<string, ResourceQuality>) => void;
  setUpdate: (id: string, available: boolean, versions?: GameUpdateVersions) => void;
}

export const useGamesStore = create<GamesState>((set) => ({
  runningGames: [],
  launchingGames: [],
  launchingViaSteam: [],
  installed: [],
  pendingInstalls: {},
  qualitySwitches: {},
  updates: [],
  updateVersions: {},
  setRunningGames: (ids) =>
    set((s) => ({
      runningGames: ids,
      launchingGames: s.launchingGames.some((x) => ids.includes(x))
        ? s.launchingGames.filter((x) => !ids.includes(x))
        : s.launchingGames,
      launchingViaSteam: s.launchingViaSteam.some((x) => ids.includes(x))
        ? s.launchingViaSteam.filter((x) => !ids.includes(x))
        : s.launchingViaSteam,
    })),
  setLaunching: (id, launching, viaSteam = false) =>
    set((s) => ({
      launchingGames: launching
        ? s.launchingGames.includes(id)
          ? s.launchingGames
          : [...s.launchingGames, id]
        : s.launchingGames.filter((x) => x !== id),
      launchingViaSteam:
        launching && viaSteam
          ? s.launchingViaSteam.includes(id)
            ? s.launchingViaSteam
            : [...s.launchingViaSteam, id]
          : s.launchingViaSteam.filter((x) => x !== id),
    })),
  setInstalled: (ids) =>
    set((s) => {
      const kept = s.updates.filter((x) => ids.includes(x));
      const versionIds = Object.keys(s.updateVersions);
      return {
        installed: ids,
        updates: kept.length === s.updates.length ? s.updates : kept,
        updateVersions: versionIds.every((id) => ids.includes(id))
          ? s.updateVersions
          : Object.fromEntries(
              Object.entries(s.updateVersions).filter(([id]) => ids.includes(id)),
            ),
      };
    }),
  setPendingInstalls: (pending) => set({ pendingInstalls: pending }),
  setQualitySwitches: (switches) => set({ qualitySwitches: switches }),
  setUpdate: (id, available, versions) =>
    set((s) => {
      let updateVersions = s.updateVersions;
      if (versions) {
        updateVersions = { ...s.updateVersions, [id]: versions };
      } else if (!available && id in s.updateVersions) {
        updateVersions = { ...s.updateVersions };
        delete updateVersions[id];
      }
      return {
        updates: available
          ? s.updates.includes(id)
            ? s.updates
            : [...s.updates, id]
          : s.updates.filter((x) => x !== id),
        updateVersions,
      };
    }),
}));
