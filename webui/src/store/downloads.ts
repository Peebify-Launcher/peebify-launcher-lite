// ------------ Download Stores ------------
// What the Downloads page shows beyond the live queue: the recent speed of each job and the jobs that finished.
// Only the launcher window uses these; the queue itself is in queueStore.ts, which the overlay loads too.
import { useEffect } from "react";
import { create } from "zustand";
import { useQueueStore } from "./queueStore";
import { isActiveJob } from "../lib/download";
import type { QueueJob } from "../lib/ipc";

// ------------ Speed History Store ------------
// Keeps the last minute of network and disk write speed per job, sampled every second, to draw the two graphs.
export const SPEED_WINDOW = 60;
const SAMPLE_MS = 1000;

export interface SpeedSamples {
  network: number[];
  disk: number[];
}

export interface SpeedSample {
  network: number;
  disk: number;
}

export const NO_SAMPLES: SpeedSamples = { network: [], disk: [] };

interface SpeedHistoryState {
  byJob: Record<string, SpeedSamples>;
  sample: (speeds: Record<string, SpeedSample>) => void;
}

export const useSpeedHistoryStore = create<SpeedHistoryState>((set) => ({
  byJob: {},
  sample: (speeds) =>
    set((s) => {
      const ids = Object.keys(speeds);
      if (ids.length === 0) {
        return Object.keys(s.byJob).length === 0 ? s : { byJob: {} };
      }
      const byJob: Record<string, SpeedSamples> = {};
      for (const id of ids) {
        const prev = s.byJob[id] ?? NO_SAMPLES;
        byJob[id] = {
          network: [...prev.network, speeds[id].network].slice(-SPEED_WINDOW),
          disk: [...prev.disk, speeds[id].disk].slice(-SPEED_WINDOW),
        };
      }
      return { byJob };
    }),
}));

function sampleActiveJobs(): void {
  const speeds: Record<string, SpeedSample> = {};
  for (const job of useQueueStore.getState().jobs) {
    if (!isActiveJob(job) || job.phase === "queued") continue;
    const idle = job.paused || !!job.waitingNetwork || !!job.heldForGame;
    speeds[job.id] = idle
      ? { network: 0, disk: 0 }
      : { network: Math.max(0, job.networkSpeed), disk: Math.max(0, job.diskSpeed) };
  }
  useSpeedHistoryStore.getState().sample(speeds);
}

export function useSpeedSampler(): void {
  useEffect(() => {
    let id: ReturnType<typeof setInterval> | undefined;
    const sync = (active: boolean) => {
      if (active && id === undefined) {
        id = setInterval(sampleActiveJobs, SAMPLE_MS);
      } else if (!active && id !== undefined) {
        clearInterval(id);
        id = undefined;
        useSpeedHistoryStore.getState().sample({});
      }
    };
    sync(useQueueStore.getState().jobs.some(isActiveJob));
    const unsub = useQueueStore.subscribe((s, p) => {
      if (s.jobs !== p.jobs) sync(s.jobs.some(isActiveJob));
    });
    return () => {
      unsub();
      if (id !== undefined) clearInterval(id);
    };
  }, []);
}

// ------------ Download History ------------
// Remembers the last few finished, cancelled or failed jobs so the Downloads page can still show them.
// Kept in the browser's local storage.
export interface HistoryEntry {
  key: string;
  gameId: string;
  kind: string;
  phase: "done" | "cancelled" | "error";
  error: string | null;
  message?: string | null;
  finishedAt: number;
}

const STORAGE_KEY = "peebify.downloadHistory";
const MAX_ENTRIES = 20;
const PHASES: readonly HistoryEntry["phase"][] = ["done", "cancelled", "error"];

function load(): HistoryEntry[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    if (!Array.isArray(parsed)) return [];
    return (parsed as (HistoryEntry & { total?: unknown })[]).map(({ total: _total, ...e }) =>
      PHASES.includes(e.phase) ? e : { ...e, phase: "error" as const },
    );
  } catch {
    return [];
  }
}

function save(entries: HistoryEntry[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(entries));
  } catch {
  }
}

interface HistoryState {
  entries: HistoryEntry[];
  add: (job: QueueJob) => void;
  clear: () => void;
}

export const useDownloadHistoryStore = create<HistoryState>((set) => ({
  entries: load(),
  add: (job) =>
    set((s) => {
      const entry: HistoryEntry = {
        key: `${job.id}-${Date.now()}`,
        gameId: job.gameId,
        kind: job.kind,
        phase: job.phase === "done" || job.phase === "cancelled" ? job.phase : "error",
        error: job.error,
        message: job.message ?? null,
        finishedAt: Date.now(),
      };
      const entries = [entry, ...s.entries].slice(0, MAX_ENTRIES);
      save(entries);
      return { entries };
    }),
  clear: () => {
    save([]);
    set({ entries: [] });
  },
}));
