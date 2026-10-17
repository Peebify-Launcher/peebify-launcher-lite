// ------------ Playtime Store ------------
// Playtime totals and sessions from the backend. Refreshes about once a minute while a game is running.
import { useEffect } from "react";
import { create } from "zustand";
import { getPlaytime, EMPTY_PLAYTIME, type PlaytimeData } from "../lib/ipc";
import { useRendererSuspended } from "../lib/launcherHooks";
import { useGamesStore } from "./gamesStore";

const LIVE_REFRESH_MS = 60_000;

interface PlaytimeState {
  data: PlaytimeData;
  hydrated: boolean;
  epoch: number;
  hydrate: () => Promise<void>;
  refresh: () => Promise<void>;
  update: () => Promise<void>;
  refreshAfterSession: () => void;
}

let inflight: Promise<void> | null = null;

export const usePlaytimeStore = create<PlaytimeState>((set, get) => {
  const read = async (reset: boolean): Promise<void> => {
    try {
      const data = await getPlaytime();
      set((s) => ({ data, epoch: reset ? s.epoch + 1 : s.epoch }));
    } finally {
      set({ hydrated: true });
    }
  };

  return {
    data: EMPTY_PLAYTIME,
    hydrated: false,
    epoch: 0,

    hydrate: () => {
      inflight ??= read(false).finally(() => {
        inflight = null;
      });
      return inflight;
    },

    refresh: () => read(true),

    update: () => read(false),

    refreshAfterSession: () => {
      void get().update();
    },
  };
});

export function useLivePlaytime(): void {
  const running = useGamesStore((s) => s.runningGames.length > 0);
  const suspended = useRendererSuspended();
  useEffect(() => {
    if (!running || suspended) return;
    const update = () => void usePlaytimeStore.getState().update();
    update();
    const t = setInterval(update, LIVE_REFRESH_MS);
    return () => clearInterval(t);
  }, [running, suspended]);
}
