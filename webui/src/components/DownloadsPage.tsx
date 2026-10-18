// ------------ Downloads Page ------------
// The Downloads tab and the live speed graph on its active download.
import { memo, type ReactNode, useId } from "react";
import { AnimatePresence, m } from "framer-motion";
import { listItemVariants } from "../lib/motion";
import {
  FastForward,
  Pause,
  Play,
  X,
  PackageOpen,
  ChevronsUp,
  RotateCcw,
  CheckCircle2,
  XCircle,
  MinusCircle,
  Download,
  Wrench,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { GAMES } from "../data/games";
import { useQueueStore, jobEquals } from "../store/queueStore";
import { useCustomizationStore } from "../store/customizationStore";
import { useGamesStore } from "../store/gamesStore";
import { NOTIF_ICON_COLORS } from "../store/notifications";
import {
  useDownloadHistoryStore,
  type HistoryEntry,
  useSpeedHistoryStore,
  NO_SAMPLES,
  SPEED_WINDOW,
  type SpeedSamples,
} from "../store/downloads";
import { iconFallback, iconSrcFor } from "../lib/customMedia";
import {
  keepGoingWhilePlaying,
  pauseJob,
  resumeJob,
  prioritizeDownload,
  downloadEnqueue,
  repairGame,
  type QueueJob,
} from "../lib/ipc";
import {
  requestCancelJob,
  isActiveJob,
  phaseLabel,
  canPauseJob,
  canCancelJob,
  canRetryKind,
  currentJob,
  retryJob,
  startNowPauses,
  withKind,
  baseKindOf,
  verifyFoundBrokenFiles,
  overallPercent,
} from "../lib/download";
import { GlassPage } from "./ui/GlassPage";
import { KindBadge, StagedProgress } from "./home/StartBar";
import { Hint, IconButton, WRAPPED_TIP } from "./ui/Tooltip";
import { fmtBytes, fmtSpeed, fmtEta, fmtRelTime } from "../lib/format";
import { useNow } from "../lib/hooks";
import { hasKey, type TKey } from "../lib/i18n";
import { useT } from "../lib/useT";

// ------------ Page ------------
// The Downloads tab. Shows the job running right now with its progress, the queue behind it, games with updates
// waiting, and a history of finished installs, updates, repairs and moves. Pause, resume, cancel and
// move-to-front all live here.
const gameName = (id: string): string => GAMES.find((g) => g.id === id)?.name ?? id;

export function DownloadsPage() {
  const t = useT();
  const jobs = useQueueStore(useShallow((s) => s.jobs.filter(isActiveJob)));
  const hydrated = useQueueStore((s) => s.hydrated);
  const history = useDownloadHistoryStore((s) => s.entries);
  const clearHistory = useDownloadHistoryStore((s) => s.clear);
  const updates = useGamesStore((s) => s.updates);
  const activeGames = new Set(jobs.map((j) => j.gameId));
  const pendingUpdates = updates.filter((id) => !activeGames.has(id));

  const running = jobs.filter((j) => j.phase !== "queued");
  const queued = jobs.filter((j) => j.phase === "queued");
  const runningIds = running.map((j) => j.id).join();
  const queuedIds = queued.map((j) => j.id).join();
  const historyKeys = history.map((e) => e.key).join();
  const holder = currentJob(running);
  const holderName = holder ? gameName(holder.gameId) : undefined;
  const startHint =
    !holder || holder.paused
      ? t("downloads.queue.startNow")
      : startNowPauses(holder, baseKindOf(holder))
        ? t("downloads.queue.startNowPauses", { game: holderName ?? "" })
        : t("downloads.queue.moveToFront");
  const moving = running.filter((j) => !j.paused);
  const blockedBy = moving.length === 1 ? gameName(moving[0].gameId) : undefined;
  const pausedHolder = moving.length === 0 && holder?.paused ? holderName : undefined;
  const waitNote =
    moving.length === 0
      ? pausedHolder
        ? t("downloads.queue.holderPaused", { game: pausedHolder })
        : t("downloads.queue.waitingToStart")
      : blockedBy
        ? t("downloads.queue.startsAfter", { game: blockedBy })
        : t("downloads.queue.startsAfterAny");

  return (
    <GlassPage
      title={t("common.downloads")}
      subtitle={t("downloads.page.subtitle")}
    >
      {!hydrated ? null : jobs.length === 0 ? (
        <div className="flex items-center justify-center gap-3 rounded-ui border border-dashed border-white/15 bg-white/[0.04] px-6 py-10 text-[14px] text-white/55">
          <PackageOpen size={18} className="opacity-70" />
          {t("downloads.page.empty")}
        </div>
      ) : (
        <div className="flex flex-col gap-3">
          <AnimatePresence initial={false}>
            {running.map((job) => (
              <m.div
                key={job.id}
                layout
                layoutDependency={runningIds}
                variants={listItemVariants}
                initial="initial"
                animate="animate"
                exit="exit"
              >
                <JobCard
                  job={job}
                  kind={baseKindOf(job)}
                  waitingFor={job.parked ? holderName : undefined}
                />
              </m.div>
            ))}
          </AnimatePresence>
        </div>
      )}

      {queued.length > 0 && (
        <section className="mt-8">
          <SectionHeader
            title={t("downloads.queue.title")}
            right={
              blockedBy
                ? t("downloads.queue.countAfter", { count: queued.length, game: blockedBy })
                : pausedHolder
                  ? t("downloads.queue.countAfterPaused", { count: queued.length, game: pausedHolder })
                  : t("downloads.queue.count", { count: queued.length })
            }
          />
          <div className="flex flex-col gap-2">
            <AnimatePresence initial={false}>
              {queued.map((job, index) => (
                <m.div
                  key={job.id}
                  layout
                  layoutDependency={queuedIds}
                  variants={listItemVariants}
                  initial="initial"
                  animate="animate"
                  exit="exit"
                >
                  <QueuedRow
                    job={job}
                    kind={baseKindOf(job)}
                    position={index + 1}
                    note={index === 0 ? waitNote : t("downloads.queue.waitingToStart")}
                    startHint={startHint}
                  />
                </m.div>
              ))}
            </AnimatePresence>
          </div>
        </section>
      )}

      {hydrated && pendingUpdates.length > 0 && (
        <section className="mt-8">
          <SectionHeader
            title={t("downloads.updates.title")}
            right={t("downloads.updates.count", { count: pendingUpdates.length })}
          />
          <div className="flex flex-col gap-2">
            {pendingUpdates.map((id) => (
              <UpdateRow key={id} gameId={id} />
            ))}
          </div>
        </section>
      )}

      {history.length > 0 && (
        <section className="mt-8">
          <SectionHeader
            title={t("downloads.history.title")}
            right={
              <button
                onClick={clearHistory}
                className="rounded-ui px-[9px] py-[4px] text-[11.5px] font-medium text-white/55 transition-colors hover:bg-white/[0.06] hover:text-white/80"
              >
                {t("downloads.history.clear")}
              </button>
            }
          />
          <div className="flex flex-col gap-2">
            <AnimatePresence initial={false}>
              {history.map((entry) => (
                <m.div
                  key={entry.key}
                  layout
                  layoutDependency={historyKeys}
                  variants={listItemVariants}
                  initial="initial"
                  animate="animate"
                  exit="exit"
                >
                  <HistoryRow entry={entry} hasActiveJob={activeGames.has(entry.gameId)} />
                </m.div>
              ))}
            </AnimatePresence>
          </div>
        </section>
      )}
    </GlassPage>
  );
}

function SectionHeader({ title, right }: { title: string; right?: ReactNode }) {
  return (
    <div className="mb-[10px] flex items-center justify-between gap-3">
      <h2 className="text-[11px] font-semibold uppercase tracking-[0.09em] text-white/55">
        {title}
      </h2>
      {typeof right === "string" ? (
        <span className="text-[11.5px] text-white/55">{right}</span>
      ) : (
        right
      )}
    </div>
  );
}

const ICON_BTN =
  "grid h-[32px] w-[32px] place-items-center rounded-[9px] text-white/70 transition-colors hover:bg-white/10 hover:text-white active:bg-white/[0.16]";
const CANCEL_BTN =
  "grid h-[32px] w-[32px] place-items-center rounded-[9px] text-white/55 transition-colors hover:bg-danger/[0.15] hover:text-danger-soft active:bg-danger/25";

const JobCard = memo(
  function JobCard({
    job,
    kind,
    waitingFor,
  }: {
    job: QueueJob;
    kind: string;
    waitingFor?: string;
  }) {
    const t = useT();
    const icons = useCustomizationStore((s) => s.gameIcons);
    const samples = useSpeedHistoryStore((s) => s.byJob[job.id] ?? NO_SAMPLES);
    const effective = withKind(job, kind);
    const pauseHint = job.paused ? t("downloads.job.resume") : t("downloads.job.pause");

    const eta = fmtEta(job.etaSecs);
    const idle = job.paused || !!job.waitingNetwork || !!job.heldForGame;
    const stats = [
      job.total > 0
        ? t("downloads.job.progress", { done: fmtBytes(job.downloaded), total: fmtBytes(job.total) })
        : "",
      idle ? "" : fmtSpeed(job.networkSpeed || job.diskSpeed),
      !idle && eta ? t("downloads.job.timeLeft", { eta }) : "",
      job.waitingNetwork && !job.paused ? t("downloads.job.resumesAutomatically") : "",
      job.heldForGame && !job.paused ? t("downloads.job.resumesAfterGame") : "",
    ]
      .filter(Boolean)
      .join(" · ");
    const tone = job.parked ? "text-white/55" : "text-(--accent-text)";
    const label =
      job.parked && waitingFor ? t("downloads.job.waitingFor", { game: waitingFor }) : phaseLabel(job);

    return (
      <div className="job-card rounded-ui border border-white/[0.08] bg-white/[0.05] px-[18px] py-[16px]">
        <div className="flex items-center gap-[14px]">
          <img
            src={iconSrcFor(job.gameId, icons)}
            onError={iconFallback(job.gameId)}
            alt=""
            className="job-card-icon h-[44px] w-[44px] shrink-0 rounded-[11px] object-cover"
          />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-[9px]">
              <span className="truncate text-[14.5px] font-semibold">{gameName(job.gameId)}</span>
              <KindBadge kind={kind} />
            </div>
            <div className="mt-[3px] flex items-center gap-[6px] text-[12px] leading-[1.3]">
              <span className={`shrink-0 font-medium ${tone}`}>{label}</span>
              {stats && (
                <>
                  <span className="shrink-0 text-white/20">·</span>
                  <span className="truncate tabular-nums text-white/50">{stats}</span>
                </>
              )}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-[10px]">
            <span className="text-[20px] font-semibold tracking-[-0.01em] tabular-nums">
              {Math.round(overallPercent(job, kind))}%
            </span>
            {job.heldForGame && !job.paused && (
              <IconButton
                label={t("downloads.job.downloadAnyway")}
                onClick={() => void keepGoingWhilePlaying(effective)}
                className={ICON_BTN}
              >
                <FastForward size={15} />
              </IconButton>
            )}
            {canPauseJob(effective) && (
              <IconButton
                label={pauseHint}
                onClick={() => void (job.paused ? resumeJob(effective) : pauseJob(effective))}
                className={ICON_BTN}
              >
                {job.paused ? <Play size={15} /> : <Pause size={15} />}
              </IconButton>
            )}
            {canCancelJob(effective) && (
              <IconButton label={t("common.cancel")} onClick={() => requestCancelJob(effective)} className={CANCEL_BTN}>
                <X size={15} />
              </IconButton>
            )}
          </div>
        </div>

        <StagedProgress job={job} kind={kind} className="mt-[14px]" />

        {samples.network.length > 1 && (samples.network.some((v) => v > 0) || samples.disk.some((v) => v > 0)) && (
          <SpeedGraphs samples={samples} />
        )}
      </div>
    );
  },
  (prev, next) =>
    prev.kind === next.kind &&
    prev.waitingFor === next.waitingFor &&
    jobEquals(prev.job, next.job),
);

const QueuedRow = memo(
  function QueuedRow({
    job,
    kind,
    position,
    note,
    startHint,
  }: {
    job: QueueJob;
    kind: string;
    position: number;
    note: string;
    startHint: string;
  }) {
    const t = useT();
    const icons = useCustomizationStore((s) => s.gameIcons);
    const effective = withKind(job, kind);
    return (
      <div className="flex items-center gap-[12px] rounded-ui border border-white/[0.07] bg-white/[0.03] px-4 py-[11px]">
        <span className="grid h-[20px] w-[20px] shrink-0 place-items-center rounded-[6px] border border-white/[0.12] text-[11px] font-semibold tabular-nums text-white/45">
          {position}
        </span>
        <img
          src={iconSrcFor(job.gameId, icons)}
          onError={iconFallback(job.gameId)}
          alt=""
          className="h-[30px] w-[30px] shrink-0 rounded-[8px] object-cover"
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-[8px]">
            <span className="truncate text-[13px] font-medium">{gameName(job.gameId)}</span>
            <KindBadge kind={kind} />
          </div>
          <p className="mt-[2px] truncate text-[11.5px] text-white/55">{note}</p>
        </div>
        <IconButton label={startHint} onClick={() => prioritizeDownload(job.id)} className={ICON_BTN}>
          <ChevronsUp size={15} />
        </IconButton>
        {canCancelJob(effective) && (
          <IconButton label={t("common.cancel")} onClick={() => requestCancelJob(effective)} className={CANCEL_BTN}>
            <X size={15} />
          </IconButton>
        )}
      </div>
    );
  },
  (prev, next) =>
    prev.position === next.position &&
    prev.kind === next.kind &&
    prev.note === next.note &&
    prev.startHint === next.startHint &&
    jobEquals(prev.job, next.job),
);

function UpdateRow({ gameId }: { gameId: string }) {
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const running = useGamesStore((s) => s.runningGames).includes(gameId);
  const name = gameName(gameId);

  return (
    <div className="flex items-center gap-[12px] rounded-ui border border-white/[0.07] bg-white/[0.03] px-4 py-[11px]">
      <img
        src={iconSrcFor(gameId, icons)}
        onError={iconFallback(gameId)}
        alt=""
        className="h-[30px] w-[30px] shrink-0 rounded-[8px] object-cover"
      />
      <div className="min-w-0 flex-1">
        <span className="block truncate text-[13px] font-medium">{name}</span>
        <p className="mt-[2px] truncate text-[11.5px] text-white/55">
          {running ? t("downloads.updates.closeGame") : t("downloads.updates.ready")}
        </p>
      </div>
      <button
        onClick={() => void downloadEnqueue(gameId)}
        disabled={running}
        aria-label={t("downloads.updates.updateGame", { game: name })}
        className="flex shrink-0 items-center gap-[6px] rounded-ui border border-white/10 bg-white/[0.05] px-[10px] py-[6px] text-[11.5px] font-medium transition-colors hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40"
      >
        <Download size={12} /> {t("common.update")}
      </button>
    </div>
  );
}

const HISTORY_META: Record<
  HistoryEntry["phase"],
  { icon: typeof CheckCircle2; color: string; label: TKey }
> = {
  done: { icon: CheckCircle2, color: NOTIF_ICON_COLORS.success.fg, label: "common.done" },
  cancelled: { icon: MinusCircle, color: "rgba(255,255,255,.45)", label: "downloads.history.stopped" },
  error: { icon: XCircle, color: NOTIF_ICON_COLORS.error.fg, label: "common.failed" },
};

function outcomeText(entry: HistoryEntry, t: ReturnType<typeof useT>): string {
  const key = `downloads.outcome.${entry.phase}.${entry.kind}`;
  if (hasKey(key)) return t(key);
  if (entry.phase === "done") return t("downloads.outcome.finished");
  return entry.phase === "cancelled"
    ? t("downloads.outcome.otherCancelled", { kind: entry.kind })
    : t("downloads.outcome.otherFailed", { kind: entry.kind });
}

const HISTORY_BTN =
  "flex shrink-0 items-center gap-[6px] rounded-ui border border-white/10 bg-white/[0.05] px-[10px] py-[6px] text-[11.5px] font-medium transition-colors hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40";

const HistoryRow = memo(function HistoryRow({
  entry,
  hasActiveJob,
}: {
  entry: HistoryEntry;
  hasActiveJob: boolean;
}) {
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const now = useNow();
  const meta = HISTORY_META[entry.phase];
  const Icon = meta.icon;
  const installed = useGamesStore((s) => s.installed.includes(entry.gameId));
  const updatePending = useGamesStore((s) => s.updates.includes(entry.gameId));
  const repairCandidate =
    entry.phase === "error" &&
    entry.kind === "verify" &&
    installed &&
    !updatePending &&
    verifyFoundBrokenFiles(entry.error);
  const offerRetry = entry.phase === "error" && canRetryKind(entry.kind) && !repairCandidate;

  return (
    <div className="flex items-center gap-[12px] rounded-ui border border-white/[0.06] bg-white/[0.03] px-4 py-[10px]">
      <img
        src={iconSrcFor(entry.gameId, icons)}
        onError={iconFallback(entry.gameId)}
        alt=""
        className="h-[28px] w-[28px] shrink-0 rounded-[8px] object-cover"
      />
      <div className="min-w-0 flex-1">
        <p className="truncate text-[12.5px]">
          <span className="font-medium">{gameName(entry.gameId)}</span>
          <span className="text-white/55"> · {outcomeText(entry, t)}</span>
        </p>
        {entry.phase === "error" && entry.error && (
          <Hint tip={<span className={WRAPPED_TIP}>{entry.error}</span>} className="block">
            <p className="mt-[2px] truncate text-[11.5px] text-danger-soft/85">{entry.error}</p>
          </Hint>
        )}
        {entry.phase === "done" && entry.message && (
          <Hint tip={<span className={WRAPPED_TIP}>{entry.message}</span>} className="block">
            <p className="mt-[2px] truncate text-[11.5px] text-white/55">{entry.message}</p>
          </Hint>
        )}
      </div>
      <span className="shrink-0 text-[11.5px] text-white/55">
        {fmtRelTime(entry.finishedAt, now)}
      </span>
      <span
        className="flex w-[74px] shrink-0 items-center justify-end gap-[6px] text-[11.5px]"
        style={{ color: meta.color }}
      >
        <Icon size={13} />
        {t(meta.label)}
      </span>
      {repairCandidate && (
        <Hint
          tip={hasActiveJob ? t("downloads.history.activeJob") : t("downloads.history.repairHint")}
          className="flex shrink-0"
        >
          <button
            onClick={() => void repairGame(entry.gameId, false)}
            disabled={hasActiveJob}
            className={HISTORY_BTN}
          >
            <Wrench size={12} /> {t("downloads.history.repair")}
          </button>
        </Hint>
      )}
      {offerRetry && (
        <Hint
          tip={hasActiveJob ? t("downloads.history.activeJob") : t("downloads.history.retryHint")}
          className="flex shrink-0"
        >
          <button
            onClick={() => void retryJob(entry.gameId, entry.kind)}
            disabled={hasActiveJob}
            className={HISTORY_BTN}
          >
            <RotateCcw size={12} /> {t("common.retry")}
          </button>
        </Hint>
      )}
    </div>
  );
});

// ------------ Speed Graphs ------------
// The two live line graphs on the active download card, each showing the last minute with its current and peak
// speed: what comes in over the network, and what reaches the disk. Each has its own scale, since the disk is
// often far faster than the network.
const VIEW_H = 32;
const NETWORK_COLOR = "#5bc0eb";
const DISK_COLOR = "#a3e635";

function SpeedGraphs({ samples }: { samples: SpeedSamples }) {
  const t = useT();
  return (
    <div className="mt-4 grid grid-cols-2 gap-3 max-[760px]:grid-cols-1">
      <SpeedGraph label={t("downloads.speed.network")} color={NETWORK_COLOR} samples={samples.network} />
      <SpeedGraph label={t("downloads.speed.disk")} color={DISK_COLOR} samples={samples.disk} />
    </div>
  );
}

function SpeedGraph({ label, color, samples }: { label: string; color: string; samples: number[] }) {
  const t = useT();
  const gradientId = `speed-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const peak = samples.reduce((a, b) => Math.max(a, b), 0);
  const current = samples.length > 0 ? samples[samples.length - 1] : 0;
  const scale = Math.max(peak, 1) * 1.15;
  const x = (i: number) => ((SPEED_WINDOW - samples.length + i) / (SPEED_WINDOW - 1)) * 100;
  const y = (v: number) => VIEW_H - (Math.max(0, v) / scale) * VIEW_H;

  const points = samples.map((v, i) => `${x(i).toFixed(2)},${y(v).toFixed(2)}`);
  const line = points.length > 1 ? `M${points.join(" L")}` : "";
  const area = line
    ? `${line} L${x(samples.length - 1).toFixed(2)},${VIEW_H} L${x(0).toFixed(2)},${VIEW_H} Z`
    : "";
  const lastX = samples.length > 0 ? x(samples.length - 1) : 0;
  const lastY = samples.length > 0 ? y(samples[samples.length - 1]) : VIEW_H;

  return (
    <div className="rounded-ui border border-white/[0.07] bg-black/25 px-[13px] pb-[9px] pt-[9px]">
      <div className="flex items-baseline justify-between gap-3">
        <span className="flex items-center gap-[7px] text-[10.5px] font-semibold uppercase tracking-[0.09em] text-white/80">
          <span className="h-[7px] w-[7px] rounded-full" style={{ background: color }} />
          {label}
        </span>
        <span className="flex items-baseline gap-[9px]">
          <span className="text-[13px] font-semibold tabular-nums text-white">
            {fmtSpeed(current) || t("format.kbps", { n: 0 })}
          </span>
          {peak > 0 && (
            <span className="text-[11px] tabular-nums text-white/40">
              {t("downloads.speed.peak", { speed: fmtSpeed(peak) })}
            </span>
          )}
        </span>
      </div>
      <div className="relative mt-[8px] h-[50px]">
        <svg
          viewBox={`0 0 100 ${VIEW_H}`}
          preserveAspectRatio="none"
          className="h-full w-full overflow-visible"
          aria-hidden
        >
          <defs>
            <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor={color} stopOpacity={0.32} />
              <stop offset="100%" stopColor={color} stopOpacity={0} />
            </linearGradient>
          </defs>
          {area && <path d={area} fill={`url(#${gradientId})`} />}
          {line && (
            <path
              d={line}
              fill="none"
              stroke={color}
              strokeWidth={2}
              strokeLinecap="round"
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          )}
        </svg>
        {samples.length > 1 && (
          <span
            className="pointer-events-none absolute h-[5px] w-[5px] -translate-x-1/2 -translate-y-1/2 rounded-full"
            style={{ left: `${lastX}%`, top: `${(lastY / VIEW_H) * 100}%`, background: color }}
          />
        )}
      </div>
    </div>
  );
}
