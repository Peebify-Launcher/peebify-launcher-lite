// ------------ Download Jobs ------------
// Helpers for download queue jobs: is it still active, can it be paused or cancelled, what to call it on
// screen, how to retry or cancel it, and how far along its stages it is.
import {
  downloadEnqueue,
  lastRepairMode,
  repairGame,
  resumeInstall,
  verifyGameIntegrity,
  downloadCancel,
  type QueueJob,
} from "./ipc";
import { useGamesStore } from "../store/gamesStore";
import { useModalStore } from "../store/modalStore";
import { gameById, supportsQuickRepair, type GameId } from "../data/games";
import { t, type TKey } from "./i18n";

// ------------ Job Helpers ------------
// Is a job still active, can it be paused or cancelled, what to call it on screen, and how to retry it after
// a failure.
export const TERMINAL_PHASES = ["done", "cancelled", "deferred", "error"];
export const isActiveJob = (j: QueueJob): boolean => !TERMINAL_PHASES.includes(j.phase);

export const baseKindOf = (j: QueueJob): string => j.baseKind || j.kind;

const PHASE_LABELS: Record<string, TKey> = {
  verifying: "stages.phase.verifying",
  scanning: "stages.phase.checking",
  downloading: "stages.phase.downloading",
  extracting: "stages.phase.extracting",
  finalizing: "stages.phase.finalizing",
  installing: "stages.phase.finalizing",
  validating: "stages.phase.finalizing",
  repairing: "stages.phase.repairing",
  moving: "stages.phase.finalizing",
};

export function phaseLabel(j: QueueJob): string {
  if (j.parked) return t("stages.phase.waiting");
  if (j.paused) return t("stages.phase.paused");
  if (j.waitingNetwork) return t("stages.phase.waitingNetwork");
  if (j.heldForGame) return t("stages.phase.heldForGame");
  if (j.phase === "queued") return t("stages.phase.queued");
  if (j.phase === "done") return t("stages.phase.done");
  if (j.phase === "cancelled") return t("stages.phase.cancelled");
  if (j.phase === "deferred") return t("stages.phase.deferred");
  if (j.phase === "error") return t("stages.phase.error");
  if (j.kind === "move") {
    return t(j.phase === "moving" ? "stages.phase.removingOldCopy" : "stages.phase.transferring");
  }
  if (j.kind === "uninstall") return t("stages.phase.removing");
  if (j.kind === "verify") return t("stages.phase.verifying");
  if (j.kind === "repair") {
    const mapped = PHASE_LABELS[j.phase];
    return t(!mapped || mapped === "stages.phase.downloading" ? "stages.phase.verifying" : mapped);
  }
  const mapped = PHASE_LABELS[j.phase];
  return mapped ? t(mapped) : j.phase;
}

const isDownloadKind = (kind: string): boolean =>
  kind === "install" || kind === "update";
const isUnpausablePhase = (phase: string): boolean => phase === "extracting" || phase === "finalizing";
export const canPauseJob = (j: QueueJob): boolean =>
  (isDownloadKind(j.kind) || j.kind === "repair") && !isUnpausablePhase(j.phase) && !j.parked;
export const canCancelJob = (j: QueueJob): boolean =>
  isDownloadKind(j.kind) ||
  j.kind === "repair" ||
  j.kind === "verify" ||
  (j.kind === "move" && j.phase !== "moving");
export const isJobDownloading = (j: QueueJob): boolean =>
  isDownloadKind(j.kind) && j.phase === "downloading" && !j.paused;

export const withKind = (j: QueueJob, kind: string): QueueJob =>
  j.kind === kind ? j : { ...j, kind };

export const currentJob = (jobs: QueueJob[]): QueueJob | undefined =>
  jobs.find((j) => isActiveJob(j) && j.phase !== "queued" && !j.parked);

export const startNowPauses = (current: QueueJob | undefined, kind: string | undefined): boolean =>
  !!current && !!kind && (isDownloadKind(kind) || kind === "repair") && !isUnpausablePhase(current.phase);

export const canRetryKind = (kind: string): boolean =>
  isDownloadKind(kind) || kind === "repair" || kind === "verify";

const BROKEN_FILES_PREFIX = /^\d+ files? needs? repair\./;
const BROKEN_FILES_ERRORS = [
  /^\d+ files? needs? repair\. Run a repair to replace (?:it|them)\.$/,
  /\. Repair replaces them\.$/,
];

export const verifyFoundBrokenFiles = (error: string | null | undefined): boolean =>
  !!error && BROKEN_FILES_ERRORS.some((re) => re.test(error));

export const verifyNeedsRepairFirst = (message: string | null | undefined): boolean =>
  !!message && BROKEN_FILES_PREFIX.test(message);

export async function retryJob(gameId: string, kind: string): Promise<void> {
  if (kind === "repair") {
    const mode = lastRepairMode(gameId);
    await repairGame(gameId, mode ? mode === "quick" : supportsQuickRepair(gameId as GameId));
    return;
  }
  if (kind === "verify") {
    await verifyGameIntegrity(gameId);
    return;
  }
  if (!isDownloadKind(kind)) return;
  const { installed, pendingInstalls } = useGamesStore.getState();
  if (installed.includes(gameId)) {
    await downloadEnqueue(gameId);
    return;
  }
  const pending = pendingInstalls[gameId];
  if (pending) {
    await resumeInstall(gameId, pending);
    return;
  }
  useModalStore.getState().openInstall(gameId as GameId);
}

// ------------ Job Stages ------------
// Splits a download, repair, move or verify job into named stages (like Download then Finalize) and works
// out which stage and how much overall progress a job is at.
interface JobStage {
  id: string;
  label: string;
  weight: number;
}

const stage = (id: string, label: TKey, weight: number): JobStage => ({
  id,
  get label() {
    return t(label);
  },
  weight,
});

const DOWNLOAD_STAGES: JobStage[] = [
  stage("check", "stages.check", 18),
  stage("download", "common.download", 58),
  stage("finalize", "stages.finalize", 24),
];

const REPAIR_STAGES: JobStage[] = [
  stage("scan", "stages.scan", 16),
  stage("verify", "stages.verify", 46),
  stage("replace", "stages.replace", 38),
];

const MOVE_STAGES: JobStage[] = [stage("transfer", "stages.transfer", 100)];

const VERIFY_STAGES: JobStage[] = [stage("verify", "stages.verify", 100)];

const REMOVE_STAGES: JobStage[] = [stage("remove", "common.remove", 100)];

export function stagesFor(kind: string): JobStage[] {
  if (kind === "repair") return REPAIR_STAGES;
  if (kind === "move") return MOVE_STAGES;
  if (kind === "verify") return VERIFY_STAGES;
  if (kind === "uninstall") return REMOVE_STAGES;
  return DOWNLOAD_STAGES;
}

const PHASE_STAGES: Record<string, string[]> = {
  scanning: ["check", "verify"],
  downloading: ["download", "transfer"],
  repairing: ["replace", "finalize"],
  verifying: ["verify", "finalize"],
  validating: ["verify", "finalize"],
  extracting: ["finalize", "replace"],
  finalizing: ["finalize"],
  installing: ["finalize", "replace"],
  moving: ["transfer", "finalize"],
};

const PREPARING_STAGES = ["check", ...PHASE_STAGES.downloading];

export function stageIndexFor(job: QueueJob, stages: JobStage[]): number {
  if (job.phase === "queued") return -1;
  if (job.phase === "done") return stages.length;
  const preparing = job.phase === "downloading" && job.total <= 0;
  for (const id of preparing ? PREPARING_STAGES : (PHASE_STAGES[job.phase] ?? [])) {
    const index = stages.findIndex((s) => s.id === id);
    if (index !== -1) return index;
  }
  return 0;
}

export function stagePercent(job: QueueJob): number {
  return Math.max(0, Math.min(100, job.percent));
}

export function overallPercent(job: QueueJob, kind: string): number {
  const stages = stagesFor(kind);
  const index = stageIndexFor(job, stages);
  if (index < 0) return 0;
  const total = stages.reduce((sum, s) => sum + s.weight, 0);
  if (total <= 0) return 0;
  if (index >= stages.length) return 100;
  let done = 0;
  for (let i = 0; i < index; i += 1) done += stages[i].weight;
  done += (stages[index].weight * stagePercent(job)) / 100;
  return (done / total) * 100;
}

// ------------ Cancel Job ------------
// Cancels a download or other queue job. Installs ask first, because cancelling throws away the files
// downloaded so far.
export function requestCancelJob(job: QueueJob): void {
  const kind = baseKindOf(job);
  const discardsFiles = kind === "install";

  if (!discardsFiles) {
    void downloadCancel(job.id, kind);
    return;
  }

  useModalStore.getState().openConfirm({
    title: t("actions.cancelInstall.title", { game: gameById(job.gameId as GameId).name }),
    message: t("actions.cancelInstall.message"),
    confirmLabel: t("actions.cancelInstall.confirm"),
    cancelLabel: t("actions.cancelInstall.keep"),
    danger: true,
    onConfirm: () => void downloadCancel(job.id, kind),
  });
}
