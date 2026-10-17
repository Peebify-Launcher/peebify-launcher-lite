// ------------ Queue Store ------------
// The live download queue as last sent by the backend. Skips updates that changed nothing so the
// page doesn't redraw for no reason.
import { create } from "zustand";
import type { QueueJob } from "../lib/ipc";

interface QueueStoreState {
  jobs: QueueJob[];
  hydrated: boolean;
  setJobs: (jobs: QueueJob[]) => void;
}

export const jobEquals = (a: QueueJob, b: QueueJob): boolean =>
  a.id === b.id &&
  a.gameId === b.gameId &&
  a.kind === b.kind &&
  (a.baseKind ?? null) === (b.baseKind ?? null) &&
  a.phase === b.phase &&
  a.total === b.total &&
  a.downloaded === b.downloaded &&
  a.percent === b.percent &&
  a.speed === b.speed &&
  a.networkSpeed === b.networkSpeed &&
  a.diskSpeed === b.diskSpeed &&
  a.etaSecs === b.etaSecs &&
  a.paused === b.paused &&
  !!a.parked === !!b.parked &&
  !!a.waitingNetwork === !!b.waitingNetwork &&
  !!a.heldForGame === !!b.heldForGame &&
  (a.message ?? null) === (b.message ?? null) &&
  !!a.warning === !!b.warning &&
  a.error === b.error;

const jobsEqual = (a: QueueJob[], b: QueueJob[]): boolean =>
  a.length === b.length && a.every((job, i) => jobEquals(job, b[i]));

export const useQueueStore = create<QueueStoreState>((set, get) => ({
  jobs: [],
  hydrated: false,
  setJobs: (jobs) => {
    if (get().hydrated && jobsEqual(get().jobs, jobs)) return;
    set({ jobs, hydrated: true });
  },
}));
