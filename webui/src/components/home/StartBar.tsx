// ------------ Start Bar ------------
// The Start button and the playtime pill under it, plus the job progress pieces (staged progress bar, kind badge)
// that the Downloads page shows too.
import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { AnimatePresence, m, useIsPresent, type Variants } from "framer-motion";
import { Play, Download, Pause, X, ChevronsUp, Loader2, Clock4 } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { useUiStore } from "../../store/uiStore";
import { useGamesStore } from "../../store/gamesStore";
import { useQueueStore } from "../../store/queueStore";
import { useModalStore } from "../../store/modalStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { iconFallback, iconSrcFor } from "../../lib/customMedia";
import { gameById, type GameId } from "../../data/games";
import { IconButton, Hint } from "../ui/Tooltip";
import {
  overallPercent,
  requestCancelJob,
  isActiveJob,
  phaseLabel,
  canPauseJob,
  canCancelJob,
  isJobDownloading,
  currentJob,
  startNowPauses,
  withKind,
  baseKindOf,
  stagesFor,
  stageIndexFor,
  stagePercent,
} from "../../lib/download";
import { qualityLabel } from "../../lib/gameActions";
import {
  launchGame,
  downloadEnqueue,
  pauseJob,
  resumeJob,
  prioritizeDownload,
  resumeInstall,
  type GameUpdateVersions,
  type QueueJob,
} from "../../lib/ipc";
import { warmInstall } from "../../lib/installPreload";
import { springFlow, springPour, useReducedMotionSafe } from "../../lib/motion";
import { fmtBytes, fmtSpeed, fmtEta, fmtDateTime, fmtHM } from "../../lib/format";
import { t, type TKey, formatLocale } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { ExpandUp } from "./BottomRightActions";
import { useLivePlaytime, usePlaytimeStore } from "../../store/playtimeStore";
import { rangeTotal } from "../../lib/playtime";
import { useNow } from "../../lib/hooks";
import { useTimeFormat } from "../../store/settingsStore";

// ------------ Start Button ------------
// The big button on the Home screen. Depending on the game it reads Install, Update, Play or Running, and while
// a download is going it turns into progress with pause and cancel.
type ButtonState = "install" | "resume" | "update" | "launch" | "starting" | "steamWait" | "running";

const BUTTON_LABELS: Record<ButtonState, TKey> = {
  install: "common.install",
  resume: "common.resume",
  update: "common.update",
  launch: "home.startBar.buttons.launch",
  starting: "home.startBar.buttons.starting",
  steamWait: "home.startBar.buttons.waitingForSteam",
  running: "home.startBar.buttons.running",
};

const JOB_BTN =
  "grid h-[28px] w-[28px] place-items-center rounded-[8px] text-white/70 transition-colors hover:bg-white/10 hover:text-white active:bg-white/[0.16]";

const PLAIN_VERSION = /^\d+(\.\d+)+$/;

function updateVersionHint(versions: GameUpdateVersions | undefined): string | null {
  const latest = versions?.latestVersion?.trim();
  if (!latest || !PLAIN_VERSION.test(latest)) return null;
  const current = versions?.currentVersion?.trim();
  if (!current) return t("home.startBar.updateTo", { version: latest });
  if (!PLAIN_VERSION.test(current) || current === latest) return null;
  return t("home.startBar.updateFromTo", { current, latest });
}

const EASE_SWAP = [0.2, 0.7, 0.2, 1] as const;

const iconSwap: Variants = {
  initial: { opacity: 0, scale: 0.55 },
  animate: { opacity: 1, scale: 1, transition: { duration: 0.28, ease: EASE_SWAP } },
  exit: { opacity: 0, scale: 0.55, transition: { duration: 0.22, ease: EASE_SWAP } },
};

const labelSwap: Variants = {
  initial: { opacity: 0, y: 6 },
  animate: { opacity: 1, y: 0, transition: { duration: 0.24, ease: EASE_SWAP } },
  exit: { opacity: 0, y: -6, transition: { duration: 0.18, ease: EASE_SWAP } },
};

const settleRipple: Variants = {
  initial: { scale: 0.15, opacity: 1 },
  animate: { scale: 1, opacity: 0, transition: { duration: 0.7, ease: EASE_SWAP } },
  exit: { opacity: 0, transition: { duration: 0.1 } },
};

function StateIcon({ state }: { state: ButtonState }) {
  if (state === "running") {
    return (
      <span className="flex h-[16px] items-center gap-[2.5px]">
        {[0, 1, 2].map((i) => (
          <span
            key={i}
            className="activity-bar h-[14px] w-[2.5px] rounded-[2px] bg-[#111]"
            style={{ animationDelay: `${i * 0.18}s` }}
          />
        ))}
      </span>
    );
  }
  if (state === "starting" || state === "steamWait") {
    return <Loader2 size={16} className="animate-spin" />;
  }
  if (state === "launch") return <Play size={16} fill="currentColor" strokeWidth={1.5} />;
  return <Download size={16} strokeWidth={2.4} />;
}

export function StartBar({ hidePlaytime = false }: { hidePlaytime?: boolean }) {
  const t = useT();
  const activeId = useUiStore((s) => s.activeGameId);
  const running = useGamesStore((s) => s.runningGames).includes(activeId);
  const installed = useGamesStore((s) => s.installed).includes(activeId);
  const pending = useGamesStore((s) => s.pendingInstalls)[activeId];
  const hasUpdate = useGamesStore((s) => s.updates).includes(activeId);
  const updateVersions = useGamesStore((s) => s.updateVersions[activeId]);
  const pendingSwitch = useGamesStore((s) => s.qualitySwitches[activeId]);
  const chosenQuality = useCustomizationStore((s) => s.resourceQuality[activeId]);
  const switchTo =
    pendingSwitch && (!chosenQuality || chosenQuality === pendingSwitch) ? pendingSwitch : undefined;
  const startRef = useRef<HTMLButtonElement>(null);
  const job = useQueueStore(
    useShallow((s) => s.jobs.find((j) => j.gameId === activeId && isActiveJob(j))),
  );
  const hasJob = !!job;
  const [landed, setLanded] = useState(hasJob);
  const [hadJob, setHadJob] = useState(hasJob);
  if (hasJob !== hadJob) {
    setHadJob(hasJob);
    setLanded(false);
  }
  const slotRef = useRef<HTMLDivElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const slot = slotRef.current;
    const card = cardRef.current;
    if (!hasJob || !slot || !card) return;
    const measure = () => slot.style.setProperty("--card-h", `${card.offsetHeight}px`);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(card);
    return () => observer.disconnect();
  }, [hasJob]);
  const holder = useQueueStore(
    useShallow((s) => {
      const current = currentJob(s.jobs.filter((j) => j.gameId !== activeId));
      return current
        ? {
            gameId: current.gameId,
            pauses: startNowPauses(current, baseKindOf(current)),
          }
        : undefined;
    }),
  );
  const game = gameById(activeId);
  const openInstall = useModalStore((s) => s.openInstall);
  const dialogOpen = useModalStore((s) => s.modal !== null);
  const [heldRaised, setHeldRaised] = useState(false);
  useLayoutEffect(() => {
    if (dialogOpen) {
      if (startRef.current?.matches(":hover")) setHeldRaised(true);
    } else {
      setHeldRaised(false);
    }
  }, [dialogOpen]);
  const needsInstall = !installed && !pending;
  useEffect(() => {
    if (needsInstall) warmInstall(activeId);
  }, [needsInstall, activeId]);
  const icons = useCustomizationStore((s) => s.gameIcons);

  const starting = useGamesStore((s) => s.launchingGames).includes(activeId);
  const waitingForSteam = useGamesStore((s) => s.launchingViaSteam).includes(activeId);

  const doLaunch = () => {
    const id = activeId;
    useGamesStore.getState().setLaunching(id, true);
    void launchGame(id).then(({ accepted, viaSteam }) => {
      const latest = useGamesStore.getState();
      if (!accepted || latest.runningGames.includes(id)) {
        latest.setLaunching(id, false);
      } else if (viaSteam && latest.launchingGames.includes(id)) {
        latest.setLaunching(id, true, true);
      }
    });
  };

  const renderJobCard = (job: QueueJob) => {
    const kind = baseKindOf(job);
    const effective = withKind(job, kind);
    const downloading = isJobDownloading(effective);
    const queued = job.phase === "queued";
    const holderName = holder ? gameById(holder.gameId as GameId).name : undefined;
    const startHint = !holder
      ? t("home.startBar.startNow")
      : holder.pauses
        ? t("home.startBar.startNowPauses", { game: holderName ?? "" })
        : t("home.startBar.moveToFront");
    const label =
      job.parked && holderName ? t("home.startBar.waitingFor", { game: holderName }) : phaseLabel(effective);
    const pauseHint = job.paused ? t("common.resume") : t("common.pause");

    const stats: string[] = [];
    if (downloading && job.speed > 0) {
      const network = fmtSpeed(job.networkSpeed);
      if (network) stats.push(network);
      if (job.etaSecs > 0) stats.push(t("home.startBar.timeLeft", { time: fmtEta(job.etaSecs) }));
    } else if (!queued && job.total > 0) {
      stats.push(`${fmtBytes(job.downloaded)} / ${fmtBytes(job.total)}`);
    }

    const tone = job.parked
      ? "text-white/45"
      : job.paused
        ? "text-amber-300/95"
        : queued
          ? "text-white/45"
          : "text-(--accent-text)";

    return (
      <div ref={cardRef} className="w-[414px]">
        <div className="glass job-card rounded-[11px] px-[15px] py-[14px]">
          <div className="flex items-center gap-[12px]">
            <img
              src={iconSrcFor(activeId, icons)}
              onError={iconFallback(activeId)}
              alt=""
              className="job-card-icon h-[40px] w-[40px] shrink-0 rounded-[10px] object-cover"
            />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-[8px]">
                <span className="truncate text-[13.5px] font-semibold leading-[1.25]">
                  {game.name}
                </span>
                <KindBadge kind={kind} compact />
              </div>
              <div className="mt-[3px] flex items-center gap-[6px] text-[11.5px] leading-[1.3]">
                <span className={`shrink-0 font-medium ${tone}`}>{label}</span>
                {stats.length > 0 && (
                  <>
                    <span className="shrink-0 text-white/20">·</span>
                    <span className="truncate tabular-nums text-white/50">{stats.join(" · ")}</span>
                  </>
                )}
              </div>
            </div>
            {!queued && (
              <span className="shrink-0 text-[17px] font-semibold tracking-[-0.01em] tabular-nums">
                {Math.round(overallPercent(job, kind))}%
              </span>
            )}
            <div className="flex shrink-0 items-center gap-[2px]">
              {queued ? (
                <IconButton label={startHint} onClick={() => prioritizeDownload(job.id)} className={JOB_BTN}>
                  <ChevronsUp size={14} />
                </IconButton>
              ) : (
                canPauseJob(effective) && (
                  <IconButton
                    label={pauseHint}
                    onClick={() => void (job.paused ? resumeJob(effective) : pauseJob(effective))}
                    className={JOB_BTN}
                  >
                    {job.paused ? <Play size={14} /> : <Pause size={14} />}
                  </IconButton>
                )
              )}
              {canCancelJob(effective) && (
                <IconButton
                  label={t("common.cancel")}
                  onClick={() => requestCancelJob(effective)}
                  className="grid h-[28px] w-[28px] place-items-center rounded-[8px] text-white/50 transition-colors hover:bg-red-500/15 hover:text-red-300"
                >
                  <X size={14} />
                </IconButton>
              )}
            </div>
          </div>

          <StagedProgress job={job} kind={kind} className="mt-[13px]" />
        </div>
      </div>
    );
  };

  let state: ButtonState = "install";
  let onClick: () => void = () => openInstall(activeId);

  if (installed) {
    if (hasUpdate) {
      state = "update";
      onClick = () => void downloadEnqueue(activeId);
    } else if (switchTo) {
      state = "resume";
      onClick = () => void downloadEnqueue(activeId);
    } else {
      state = "launch";
      onClick = doLaunch;
    }
  } else if (pending) {
    state = "resume";
    onClick = () => void resumeInstall(activeId, pending);
  }
  if (starting) {
    state = waitingForSteam ? "steamWait" : "starting";
    onClick = () => {};
  }
  if (running) {
    state = "running";
    onClick = () => {};
  }

  const idle = !running && !starting;
  const reduce = useReducedMotionSafe();
  const updateHint =
    state === "update"
      ? updateVersionHint(updateVersions)
      : state === "resume" && installed && switchTo
        ? t("home.startBar.finishSwitch", { quality: qualityLabel(switchTo) })
        : null;

  return (
    <m.div
      ref={slotRef}
      initial={false}
      animate={{ "--grow": hasJob ? 1 : 0 }}
      transition={reduce ? { duration: 0 } : { "--grow": hasJob ? springFlow : springPour }}
      onAnimationComplete={() => {
        if (hasJob) setLanded(true);
      }}
      className="grid items-end justify-items-start"
      style={{ "--slack": SLACK, height: SLOT_HEIGHT, gridTemplateRows: "minmax(0, 1fr)" } as CSSProperties}
    >
      <AnimatePresence initial={false}>
        {job ? (
          <StartMorph key="job" clipped={!(landed && hasJob)}>
            {renderJobCard(job)}
          </StartMorph>
        ) : (
          <m.div
            key="idle"
            variants={idleVariants}
            initial="initial"
            animate="animate"
            exit="exit"
            style={{ gridArea: "1 / 1" }}
          >
            <div className="flex items-end gap-[14px]">
              <IconButton
                tip={updateHint}
                placement="right"
                buttonRef={startRef}
                onClick={onClick}
                onPointerEnter={state === "install" ? () => warmInstall(activeId) : undefined}
                onFocus={state === "install" ? () => warmInstall(activeId) : undefined}
                disabled={!idle}
                className={`start-button relative z-[1] grid h-[46px] w-[200px] place-items-center overflow-hidden rounded-[12px] bg-white text-[15px] font-semibold tracking-[-0.005em] text-[#111] ${
                  idle ? "start-button-idle" : ""
                } ${idle && heldRaised ? "start-button-raised" : ""} ${
                  state === "starting" || state === "steamWait" ? "start-button-busy" : ""
                }`}
              >
                <AnimatePresence initial={false}>
                  <m.span
                    key={state}
                    initial="initial"
                    animate="animate"
                    exit="exit"
                    style={{ gridArea: "1 / 1" }}
                    className="pointer-events-none flex items-center gap-[10px]"
                  >
                    <m.span variants={iconSwap} className="grid h-[16px] w-[16px] place-items-center">
                      <StateIcon state={state} />
                    </m.span>
                    <m.span variants={labelSwap}>{t(BUTTON_LABELS[state])}</m.span>
                    {state === "running" && !reduce && (
                      <m.span
                        variants={settleRipple}
                        aria-hidden="true"
                        className="start-ripple pointer-events-none absolute left-1/2 top-1/2 -ml-[120px] -mt-[120px] h-[240px] w-[240px] rounded-full"
                      />
                    )}
                  </m.span>
                </AnimatePresence>
              </IconButton>

              {!hidePlaytime && <PlaytimeTracker />}
            </div>
          </m.div>
        )}
      </AnimatePresence>
    </m.div>
  );
}

// ------------ Start Morph ------------
// The Start slot holds the Start button or, while a job runs, its card, and is as tall as what shows. When a job
// starts the card grows out of the Start button's 200×46 shape, and when it ends (or the game is switched to one with
// none) the card pours back into that shape and fades, uncovering the button. The slot animates --grow, 0 → 1, and
// both the card's clip and the slot's height read it, so the News panel above, which sits on the slot, moves with the
// card's top edge from the first frame instead of waiting for the card to leave or jumping when it arrives.
//
// The clip starts as the button's rect (bottom left) and ends 48px outside the card (--slack, which only opens over the
// last fifth of the spring), so the card's drop shadow is not cut off once it has landed; the slot is as tall as the
// card's visible part, so it stops growing at that point. A clip-path or an opacity below 1 makes its element a
// backdrop root, so a glass card inside it would blur only what is inside the card and show no wallpaper. So neither
// sits on the wrapper: the clip (`.job-card` under `data-morph-clipped`, in index.css) and the fade (--fade, read by
// `.job-card`) are on the card's own element, which still blurs the page behind it. The clip is dropped once the card
// has landed (it clipped nothing by then), and comes back when the card starts pouring away.
const BUTTON_H = 46;
const SLACK = "calc(clamp(0, (var(--grow, 0) - 0.8) * 5, 1) * 48px)";
const SLOT_HEIGHT = `calc(var(--card-h, 111px) - max(0px, (1 - var(--grow, 0)) * (var(--card-h, 111px) - ${BUTTON_H}px) - var(--slack, 0px)))`;

const morphVariants: Variants = {
  initial: { "--fade": 0 },
  animate: (reduce: boolean) => ({ "--fade": 1, transition: { duration: reduce ? 0 : 0.18 } }),
  exit: (reduce: boolean) => ({
    "--fade": 0,
    transition: reduce ? { duration: 0 } : { duration: 0.2, delay: 0.3 },
  }),
};

const idleVariants: Variants = {
  initial: { opacity: 0 },
  animate: { opacity: 1, transition: { duration: 0.2, delay: 0.12 } },
  exit: { opacity: 0, transition: { duration: 0.16 } },
};

function StartMorph({ children, clipped }: { children: ReactNode; clipped: boolean }) {
  const reduce = useReducedMotionSafe();
  const present = useIsPresent();
  return (
    <m.div
      variants={morphVariants}
      custom={reduce}
      initial="initial"
      animate="animate"
      exit="exit"
      data-morph-clipped={clipped || !present}
      style={{ gridArea: "1 / 1" }}
      className="relative z-[2]"
    >
      {children}
    </m.div>
  );
}

// ------------ Playtime Pill ------------
// The Playtime pill under the Start button showing total time played. Opens upwards to show today, this week,
// this month, all time, average session and last played for the selected game.
function fmtLastPlayed(ts: number, nowMs: number): string {
  if (Number.isNaN(ts)) return t("format.never");
  const mins = Math.floor((nowMs - ts) / 60000);
  if (mins < 1) return t("format.justNowCapital");
  if (mins < 60) return t("format.minutesAgo", { n: mins });
  const today = new Date(nowMs);
  today.setHours(0, 0, 0, 0);
  const day = new Date(ts);
  day.setHours(0, 0, 0, 0);
  const days = Math.round((today.getTime() - day.getTime()) / 86_400_000);
  if (days <= 0) return t("format.hoursAgo", { n: Math.floor(mins / 60) });
  if (days === 1) return t("home.playtimeTracker.yesterday");
  return new Date(ts).toLocaleDateString(formatLocale(), {
    month: "short",
    day: "numeric",
    ...(day.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}),
  });
}

function PlaytimeTracker() {
  const t = useT();
  const now = useNow();
  const timeFormat = useTimeFormat();
  const activeId = useUiStore((s) => s.activeGameId);
  const game = gameById(activeId);
  const pill = usePlaytimeStore((s) => s.data.pill);
  const daily = usePlaytimeStore((s) => s.data.dashboard.daily);
  useLivePlaytime();

  const recent = {
    today: rangeTotal(daily, 1, activeId),
    week: rangeTotal(daily, 7, activeId),
    month: rangeTotal(daily, 30, activeId),
  };

  const p = pill[activeId];
  const allTime = p?.allTimeMinutes ?? 0;
  const sessions = p?.sessions ?? 0;
  const lastPlayed = p?.lastPlayed ? Date.parse(p.lastPlayed) : NaN;
  const rows: { label: string; value: string; title?: string }[] = [
    { label: t("common.today"), value: fmtHM(recent.today) },
    { label: t("common.thisWeek"), value: fmtHM(recent.week) },
    { label: t("home.playtimeTracker.thisMonth"), value: fmtHM(recent.month) },
    { label: t("common.allTime"), value: fmtHM(allTime) },
    { label: t("home.playtimeTracker.avgSession"), value: fmtHM(sessions > 0 ? allTime / sessions : 0) },
    {
      label: t("home.playtimeTracker.lastPlayed"),
      value: fmtLastPlayed(lastPlayed, now),
      title: Number.isNaN(lastPlayed) ? undefined : fmtDateTime(lastPlayed, timeFormat),
    },
  ];

  return (
    <ExpandUp
      width={200}
      // i18n-ignore
      triggerClass="expand-trigger flex h-[46px] w-full items-center gap-[8px] px-[16px] text-white"
      trigger={
        <>
          <Clock4 size={14} className="opacity-75" />
          <span className="text-[13px] font-medium text-white/70">{t("common.playtime")}</span>
          <span className="ml-auto whitespace-nowrap text-[14px] font-semibold">{fmtHM(allTime)}</span>
        </>
      }
    >
      <div className="px-[16px] pb-[12px] pt-[12px]">
        <div className="mb-[9px] text-[10px] font-medium uppercase tracking-[1px] text-white/40">
          {t("home.playtimeTracker.heading", { game: game.name })}
        </div>
        <div className="flex flex-col gap-[9px]">
          {rows.map((r, i) => (
            <div
              key={r.label}
              className={`flex items-center justify-between ${
                i === 3 ? "border-t border-white/[0.07] pt-[9px]" : ""
              }`}
            >
              <span className="text-[12px] text-white/55">{r.label}</span>
              <Hint tip={r.title} placement="left">
                <span className="whitespace-nowrap text-[13px] font-semibold">{r.value}</span>
              </Hint>
            </div>
          ))}
        </div>
      </div>
    </ExpandUp>
  );
}

// ------------ Staged Progress ------------
// A progress bar split into the steps of a job (downloading, unpacking and so on), so you can see which step is
// running and how far through it is. Dims while the job is paused or waiting for the network.
export function StagedProgress({
  job,
  kind,
  className = "",
}: {
  job: QueueJob;
  kind: string;
  className?: string;
}) {
  const t = useT();
  const stages = stagesFor(kind);
  const active = stageIndexFor(job, stages);
  const percent = stagePercent(job);
  const idle = job.paused || !!job.waitingNetwork || !!job.heldForGame;

  const fillFor = (index: number): number => {
    if (index < active) return 100;
    if (index > active) return 0;
    return percent;
  };

  return (
    <div className={className}>
      <div className="flex gap-[4px]">
        {stages.map((stage, index) => {
          const fill = fillFor(index);
          return (
            <div
              key={stage.id}
              style={{ flexGrow: stage.weight, flexBasis: 0 }}
              className="h-[4px] overflow-hidden rounded-full bg-black/[0.38] shadow-[inset_0_1px_1px_rgba(0,0,0,0.3)]"
            >
              <div
                className={`relative h-full overflow-hidden rounded-full transition-[width] duration-300 ease-linear ${
                  idle ? "bg-white/35" : "bg-white"
                }`}
                style={{ width: `${fill}%` }}
              >
                {index === active && !idle && fill > 0 && (
                  <span className="progress-shimmer" />
                )}
              </div>
            </div>
          );
        })}
      </div>
      <div className="mt-[9px] flex gap-[4px]">
        {stages.map((stage, index) => {
          const done = index < active;
          const current = index === active;
          return (
            <div
              key={stage.id}
              style={{ flexGrow: stage.weight, flexBasis: 0, minWidth: current ? "max-content" : 0 }}
              className="flex items-center gap-[6px]"
            >
              <span
                className={`h-[6px] w-[6px] shrink-0 rounded-full ${
                  current
                    ? idle
                      ? "bg-warning/95"
                      : "bg-white"
                    : done
                      ? "bg-white/45"
                      : "shadow-[inset_0_0_0_1px_rgba(255,255,255,0.25)]"
                }`}
              />
              <span
                className={`truncate text-[11.5px] tabular-nums ${
                  current ? "text-white" : done ? "text-white/45" : "text-white/[0.28]"
                }`}
              >
                {!current
                  ? stage.label
                  : job.paused
                    ? t("downloads.stage.paused", { stage: stage.label })
                    : job.waitingNetwork
                      ? t("downloads.stage.waiting", { stage: stage.label })
                      : job.heldForGame
                        ? t("downloads.stage.held", { stage: stage.label })
                        : t("downloads.stage.percent", { stage: stage.label, percent: Math.round(percent) })}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ------------ Kind Badge ------------
// The small label on a download saying what the job is: Install, Update, Repair, Verify, Move or Remove.
const LABELS: Record<string, TKey> = {
  install: "ui.kindBadge.install",
  download: "ui.kindBadge.install",
  update: "ui.kindBadge.update",
  repair: "ui.kindBadge.repair",
  verify: "ui.kindBadge.verify",
  move: "ui.kindBadge.move",
  uninstall: "ui.kindBadge.remove",
};

export function KindBadge({ kind, compact = false }: { kind: string; compact?: boolean }) {
  const t = useT();
  return (
    <span
      className={`flex shrink-0 items-center bg-white/10 font-semibold uppercase tracking-[0.08em] text-white/75 ${
        compact ? "rounded-[5px] px-[6px] py-[2px] text-[9px]" : "rounded-[6px] px-[7px] py-[3px] text-[10px]"
      }`}
    >
      {t(LABELS[kind] ?? LABELS.install)}
    </span>
  );
}
