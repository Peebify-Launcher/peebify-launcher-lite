// ------------ Playtime Page ------------
// The Playtime tab and its two small charts.
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { m } from "framer-motion";
import { ChevronDown, ChevronUp, Flame, Trash2, X } from "lucide-react";
import {
  type GameTotal,
  type ShareSlice,
  buildMiniDays,
  computeStreaks,
  endDayOffset,
  fmtDayHeading,
  fmtHoursShort,
  fmtTimeRange,
  groupSessionsByDay,
  MINI_DAYS,
  pctChange,
  rangeCutoff,
  RANGE_DAYS,
  RANGE_LABEL,
  rangeTotal,
  sessionEnd,
  totalsByGame,
  usesHour12,
  type SessionDay,
  type GameFilter,
  type PlaytimeRange,
  fmtDayTip,
  type MiniDay,
} from "../lib/playtime";
import { useCustomizationStore, useGameColor } from "../store/customizationStore";
import { gameById, GAMES, type GameId } from "../data/games";
import {
  deletePlaytimeSession,
  getOlderSessions,
  type SessionEntry,
} from "../lib/ipc";
import { pageVariants } from "../lib/motion";
import { fmtClockTime, fmtHM } from "../lib/format";
import { iconFallback, iconSrcFor } from "../lib/customMedia";
import { useNow } from "../lib/hooks";
import { useTimeFormat } from "../store/settingsStore";
import { useLivePlaytime, usePlaytimeStore } from "../store/playtimeStore";
import { useSessionUiStore } from "../store/sessionUiStore";
import { SegmentedControl } from "./ui/controls";
import { Hint, IconButton, TooltipPortal } from "./ui/Tooltip";
import { formatLocale, t } from "../lib/i18n";
import { useT } from "../lib/useT";

// ------------ Page ------------
// The Playtime tab. Shows totals, streaks and averages for the week, month or all time next to a list of every
// play session grouped by day. Can be filtered to one game, and individual sessions can be removed.
const RANGES: PlaytimeRange[] = ["Week", "Month", "All"];
const GROUPS_COLLAPSED = 6;
const GROUPS_STEP = 8;

type OlderSessions = {
  epoch: number;
  sessions: SessionEntry[];
  hasMore: boolean;
  removed: string[];
};

const fmtSessionClock = (min: number) =>
  min > 0 && min < 1 ? t("playtime.lessThanMinute") : fmtHM(min);

const sessionKey = (s: SessionEntry) => `${s.gameId}:${s.start}`;

const oldestStart = (sessions: SessionEntry[]) =>
  sessions.reduce((a, s) => Math.min(a, s.start), Infinity);

export function PlaytimePage() {
  const t = useT();
  const range = useSessionUiStore((s) => s.playtimeRange);
  const setRange = useSessionUiStore((s) => s.setPlaytimeRange);
  const storedFilter = useSessionUiStore((s) => s.playtimeGame);
  const setFilter = useSessionUiStore((s) => s.setPlaytimeGame);
  const dashboard = usePlaytimeStore((s) => s.data.dashboard);
  const hydrated = usePlaytimeStore((s) => s.hydrated);
  const epoch = usePlaytimeStore((s) => s.epoch);
  const updatePlaytime = usePlaytimeStore((s) => s.update);
  const [older, setOlder] = useState<OlderSessions | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  useNow();
  useLivePlaytime();

  const played = useMemo<GameTotal[]>(
    () =>
      dashboard.games
        .filter((t) => t.minutes > 0 && GAMES.some((g) => g.id === t.id))
        .map((t) => {
          const g = gameById(t.id as GameId);
          return {
            id: g.id,
            name: g.name,
            minutes: t.minutes,
            sessions: t.sessions,
            longestSessionMinutes: t.longestSessionMinutes,
          };
        })
        .sort((a, b) => b.minutes - a.minutes),
    [dashboard],
  );
  const filter: GameFilter =
    storedFilter !== "all" && !played.some((t) => t.id === storedFilter) ? "all" : storedFilter;

  const summary = (() => {
    const daily = dashboard.daily;
    const inFilter = filter === "all" ? played : played.filter((t) => t.id === filter);
    const allTime = inFilter.reduce((a, t) => a + t.minutes, 0);
    const sessions = inFilter.reduce((a, t) => a + t.sessions, 0);
    return {
      allTime,
      sessions,
      week: rangeTotal(daily, 7, filter),
      month: rangeTotal(daily, 30, filter),
      prevMonth: rangeTotal(daily, 30, filter, 30),
      avgSession: sessions > 0 ? allTime / sessions : 0,
      longestSession: inFilter.reduce((a, t) => Math.max(a, t.longestSessionMinutes), 0),
      miniDays: buildMiniDays(daily, MINI_DAYS, filter),
      streaks: computeStreaks(daily, filter),
    };
  })();

  const shareTotals: ShareSlice[] = (() => {
    const days = RANGE_DAYS[range];
    if (days <= 0) return played;
    const byGame = totalsByGame(dashboard.daily, days);
    return played
      .map((t) => ({ id: t.id, name: t.name, minutes: byGame[t.id] ?? 0 }))
      .filter((t) => t.minutes > 0)
      .sort((a, b) => b.minutes - a.minutes);
  })();

  const [shownDashboard, setShownDashboard] = useState(dashboard);
  if (shownDashboard !== dashboard) {
    setShownDashboard(dashboard);
    if (older?.epoch === epoch) {
      const floor = oldestStart(dashboard.recentSessions);
      const have = new Set(older.sessions.map(sessionKey));
      const carried = shownDashboard.recentSessions.filter(
        (s: SessionEntry) => !s.live && s.start < floor && !have.has(sessionKey(s)),
      );
      if (carried.length > 0) setOlder({ ...older, sessions: [...carried, ...older.sessions] });
    }
  }
  const olderLoaded =
    older?.epoch === epoch && dashboard.sessionsTruncated === true ? older : null;
  const sessions = useMemo<SessionEntry[]>(() => {
    const recent: SessionEntry[] = dashboard.recentSessions;
    if (!olderLoaded) return recent;
    const floor = oldestStart(recent);
    const seen = new Set<string>(olderLoaded.removed);
    const earlier = olderLoaded.sessions.filter((s) => {
      const key = sessionKey(s);
      if (s.start >= floor || seen.has(key)) return false;
      seen.add(key);
      return true;
    });
    return [...recent, ...earlier];
  }, [dashboard, olderLoaded]);
  const oldestLoaded = useMemo(() => oldestStart(sessions), [sessions]);
  const hasMore = olderLoaded ? olderLoaded.hasMore : dashboard.sessionsTruncated === true;
  const cutoff = rangeCutoff(range);
  const incomplete = hasMore && Number.isFinite(oldestLoaded) && oldestLoaded >= cutoff;

  const inRange = useMemo(
    () =>
      sessions.filter(
        (s) =>
          s.start >= cutoff &&
          (filter === "all" ? GAMES.some((g) => g.id === s.gameId) : s.gameId === filter),
      ),
    [sessions, filter, cutoff],
  );
  const groups = useMemo(() => groupSessionsByDay(inRange), [inRange]);
  const rangeMinutes = useMemo(() => inRange.reduce((a, s) => a + s.minutes, 0), [inRange]);

  const [shownGroups, setShownGroups] = useState(GROUPS_COLLAPSED);
  useEffect(() => setShownGroups(GROUPS_COLLAPSED), [range, filter]);

  const loadOlder = async () => {
    if (loadingOlder || !Number.isFinite(oldestLoaded)) return;
    const requestedFor = epoch;
    const loaded = sessions.slice(dashboard.recentSessions.length);
    setLoadingOlder(true);
    try {
      const page = await getOlderSessions(oldestLoaded);
      if (page && usePlaytimeStore.getState().epoch === requestedFor) {
        setOlder((o) => ({
          epoch: requestedFor,
          sessions: [...loaded, ...page.sessions],
          hasMore: page.hasMore,
          removed: o?.epoch === requestedFor ? o.removed : [],
        }));
      }
    } finally {
      setLoadingOlder(false);
    }
  };

  const removeSession = async (session: SessionEntry) => {
    if (!(await deletePlaytimeSession(session.gameId, session.start))) return;
    const key = sessionKey(session);
    setOlder((o) => o && { ...o, removed: [...o.removed, key] });
    await updatePlaytime();
  };

  const showEarlier = () => {
    setShownGroups((n) => n + GROUPS_STEP);
    if (incomplete && groups.length <= shownGroups + GROUPS_STEP) void loadOlder();
  };
  const earlierButton = (groups.length > shownGroups || incomplete) && (
    <MoreButton
      label={loadingOlder ? t("playtime.loadingEarlier") : t("playtime.showEarlier")}
      onClick={showEarlier}
    />
  );

  const lineVars = { count: inRange.length, time: fmtHM(rangeMinutes), days: RANGE_DAYS[range] };
  const summaryLine =
    range !== "All"
      ? incomplete
        ? t("playtime.summaryLine.latestInRange", lineVars)
        : t("playtime.summaryLine.inRange", lineVars)
      : incomplete
        ? t("playtime.summaryLine.latest", lineVars)
        : t("playtime.summaryLine.recorded", lineVars);

  return (
    <m.div variants={pageVariants} className="@container absolute inset-0">
      <div className="flex h-full flex-col overflow-y-auto @[820px]:flex-row @[820px]:overflow-hidden">
        <aside className="shrink-0 border-b border-white/[0.08] px-8 pb-7 pt-8 @[820px]:w-[326px] @[820px]:overflow-y-auto @[820px]:border-b-0 @[820px]:border-r">
          <div className="w-full max-w-[420px] @[820px]:max-w-none">
            {hydrated ? (
              <SummaryRail
                summary={summary}
                share={shareTotals}
                filter={filter}
                onPick={setFilter}
                range={range}
                onRange={setRange}
              />
            ) : (
              <RailSkeleton />
            )}
          </div>
        </aside>

        <section className="min-w-0 flex-1 px-8 pb-9 pt-8 @[820px]:overflow-y-auto @[820px]:px-9 @[820px]:pt-[30px]">
          <header className="mb-6 flex flex-wrap items-end justify-between gap-x-5 gap-y-3">
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-[10px]">
                <h1 className="text-[24px] font-semibold leading-tight">{t("playtime.sessionsTitle")}</h1>
                {filter !== "all" && (
                  <FilterChip
                    total={played.find((t) => t.id === filter)}
                    onClear={() => setFilter("all")}
                  />
                )}
              </div>
              <p className="mt-[6px] text-[13px] text-white/45">
                {hydrated ? summaryLine : t("playtime.loadingHistory")}
              </p>
            </div>
          </header>

          {!hydrated ? (
            <SessionsSkeleton />
          ) : groups.length === 0 ? (
            <>
              <EmptyState filtered={filter !== "all"} range={range} />
              {earlierButton}
            </>
          ) : (
            <>
              <div className="flex flex-col gap-[26px]">
                {groups.slice(0, shownGroups).map((g) => (
                  <DaySection
                    key={g.key}
                    group={g}
                    filter={filter}
                    onPick={setFilter}
                    onRemove={removeSession}
                  />
                ))}
              </div>
              {earlierButton ||
                (groups.length > GROUPS_COLLAPSED && (
                  <MoreButton
                    label={t("playtime.showLess")}
                    onClick={() => setShownGroups(GROUPS_COLLAPSED)}
                  />
                ))}
            </>
          )}
        </section>
      </div>
    </m.div>
  );
}

type Summary = {
  allTime: number;
  sessions: number;
  week: number;
  month: number;
  prevMonth: number;
  avgSession: number;
  longestSession: number;
  miniDays: ReturnType<typeof buildMiniDays>;
  streaks: ReturnType<typeof computeStreaks>;
};

function SummaryRail({
  summary,
  share,
  filter,
  onPick,
  range,
  onRange,
}: {
  summary: Summary;
  share: ShareSlice[];
  filter: GameFilter;
  onPick: (id: GameFilter) => void;
  range: PlaytimeRange;
  onRange: (range: PlaytimeRange) => void;
}) {
  const color = useGameColor();
  const t = useT();
  const hours = Math.floor(summary.allTime / 60);
  const big = hours >= 1 ? hours : Math.round(summary.allTime);
  const unit =
    hours >= 1
      ? t("playtime.summary.hours", { count: hours })
      : t("playtime.summary.minutes", { count: big });
  const delta = pctChange(summary.month, summary.prevMonth);
  const { current, longest } = summary.streaks;

  return (
    <>
      <RailLabel>{filter === "all" ? t("common.allTime") : gameById(filter as GameId).name}</RailLabel>
      <div className="mt-[10px] flex items-baseline gap-[10px]">
        <span className="font-display text-[56px] font-bold leading-none tracking-[-0.02em]">
          {big.toLocaleString(formatLocale())}
        </span>
        <span className="text-[17px] font-medium text-white/45">{unit}</span>
      </div>
      {delta !== null && (
        <div className="mt-[14px] flex items-center gap-[6px] text-[12.5px]">
          <span
            className={`flex items-center gap-[2px] font-semibold ${
              delta > 0 ? "text-emerald-400" : "text-rose-400"
            }`}
          >
            {delta > 0 ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
            {Math.min(Math.abs(delta), 999)}%
          </span>
          <span className="text-white/55">{t("playtime.summary.vsLastMonth")}</span>
        </div>
      )}

      <RailDivider />
      <div className="flex flex-col">
        <StatRow label={t("common.thisWeek")} value={fmtHM(summary.week)} />
        <StatRow label={t("playtime.summary.averageSession")} value={fmtHM(summary.avgSession)} />
        <StatRow label={t("playtime.summary.longestSession")} value={fmtHM(summary.longestSession)} />
        <StatRow
          label={t("playtime.summary.sessionsLogged")}
          value={summary.sessions.toLocaleString(formatLocale())}
        />
      </div>

      <RailDivider />
      <RailLabel>{t("playtime.summary.lastDays", { count: MINI_DAYS })}</RailLabel>
      <div className="mt-[14px]">
        <MiniBars days={summary.miniDays} />
      </div>
      <div className="mt-[14px] flex items-center gap-[7px] text-[12px] text-white/65">
        <Flame size={13} className={current > 0 ? "text-amber-400" : "text-white/30"} />
        <span>
          {t("playtime.summary.streak", { count: current })}
          {longest > 0 && (
            <span className="text-white/55"> {t("playtime.summary.best", { count: longest })}</span>
          )}
        </span>
      </div>

      <RailDivider />
      <RailLabel>{t("playtime.summary.shareOfPlay")}</RailLabel>
      <div className="mt-[14px]">
        <ShareBar totals={share} filter={filter} onPick={onPick} />
      </div>
      <div className="mt-[14px] flex flex-col">
        {share.map((t) => {
          const active = filter === t.id;
          return (
            <button
              key={t.id}
              onClick={() => onPick(active ? "all" : t.id)}
              aria-pressed={active}
              className={`-mx-[8px] flex items-center gap-[10px] rounded-[8px] px-[8px] py-[7px] text-left transition-colors ${
                active ? "bg-white/[0.08]" : "hover:bg-white/[0.05]"
              }`}
            >
              <span
                className="h-[7px] w-[7px] shrink-0 rounded-full"
                style={{ background: color(t.id) }}
              />
              <span
                className={`min-w-0 flex-1 truncate text-[13px] ${
                  active ? "text-white" : "text-white/70"
                }`}
              >
                {t.name}
              </span>
              <span className="shrink-0 text-[12.5px] font-medium text-white/45">
                {fmtHoursShort(t.minutes)}
              </span>
            </button>
          );
        })}
        {share.length === 0 && (
          <p className="py-2 text-[12.5px] text-white/55">{t("playtime.summary.nothingInRange")}</p>
        )}
      </div>

      <RailDivider />
      <SegmentedControl
        ariaLabel={t("playtime.rangeAria")}
        options={RANGES.map((r) => ({ value: r, label: RANGE_LABEL[r] }))}
        value={range}
        onChange={(v) => onRange(v as PlaytimeRange)}
        className="w-fit"
      />
    </>
  );
}

function RailLabel({ children }: { children: React.ReactNode }) {
  return (
    <h2 className="text-[11px] font-semibold uppercase tracking-[0.14em] text-white/55">
      {children}
    </h2>
  );
}

function RailDivider() {
  return <div className="my-[20px] h-px bg-white/[0.07]" />;
}

function StatRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-3 py-[9px]">
      <span className="truncate text-[13px] text-white/45">{label}</span>
      <span className="shrink-0 text-[13px] font-semibold text-white/90">{value}</span>
    </div>
  );
}

function FilterChip({ total, onClear }: { total?: GameTotal; onClear: () => void }) {
  const icons = useCustomizationStore((s) => s.gameIcons);
  const t = useT();
  if (!total) return null;
  return (
    <IconButton
      onClick={onClear}
      tip={t("playtime.filter.showAll")}
      aria-label={t("playtime.filter.showingOnly", { game: total.name })}
      className="flex items-center gap-[7px] rounded-full border border-white/15 bg-white/[0.06] py-[4px] pl-[5px] pr-[9px] text-[12px] font-medium text-white/75 transition-colors hover:bg-white/[0.11] hover:text-white"
    >
      <img
        src={iconSrcFor(total.id, icons)}
        onError={iconFallback(total.id)}
        alt=""
        className="h-[17px] w-[17px] rounded-full object-cover"
      />
      {total.name}
      <X size={12} className="text-white/45" />
    </IconButton>
  );
}

function MoreButton({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="mt-[22px] w-full rounded-ui border border-white/[0.09] bg-white/[0.03] py-[14px] text-[13px] font-medium text-white/60 transition-colors hover:bg-white/[0.07] hover:text-white"
    >
      {label}
    </button>
  );
}

function DaySection({
  group,
  filter,
  onPick,
  onRemove,
}: {
  group: SessionDay;
  filter: GameFilter;
  onPick: (id: GameFilter) => void;
  onRemove: (session: SessionEntry) => Promise<void>;
}) {
  const t = useT();
  return (
    <section>
      <div className="mb-[4px] flex items-center gap-[14px]">
        <h2 className="shrink-0 text-[13.5px] font-semibold text-white/85">
          {fmtDayHeading(group.date)}
        </h2>
        <div className="h-px min-w-[16px] flex-1 bg-white/[0.07]" />
        <span className="shrink-0 text-[11.5px] text-white/55">
          {t("playtime.day.sessions", { count: group.items.length })}
        </span>
        <Hint tip={t("playtime.day.startedThisDay")} className="inline-flex shrink-0">
          <span className="text-[12.5px] font-medium text-white/55">{fmtSessionClock(group.minutes)}</span>
        </Hint>
      </div>
      <div className="flex flex-col">
        {group.items.map((s, i) => (
          <SessionRow
            key={`${s.gameId}-${s.start}-${i}`}
            session={s}
            filter={filter}
            onPick={onPick}
            onRemove={onRemove}
          />
        ))}
      </div>
    </section>
  );
}

function SessionRow({
  session,
  filter,
  onPick,
  onRemove,
}: {
  session: SessionEntry;
  filter: GameFilter;
  onPick: (id: GameFilter) => void;
  onRemove: (session: SessionEntry) => Promise<void>;
}) {
  const g = gameById(session.gameId as GameId);
  const color = useGameColor();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const fmt = useTimeFormat();
  const t = useT();
  const picked = filter === g.id;
  const end = sessionEnd(session);
  const laterDays = endDayOffset(session.start, end);
  return (
    <div className="group -mx-[10px] flex items-center rounded-[10px] transition-colors hover:bg-white/[0.05]">
      <IconButton
        onClick={() => onPick(picked ? "all" : g.id)}
        aria-pressed={picked}
        tip={picked ? t("playtime.filter.showAll") : t("playtime.filter.showOnly", { game: g.name })}
        className="flex min-w-0 flex-1 items-center gap-[14px] rounded-[10px] py-[5px] pl-[10px] pr-[8px] text-left"
      >
        <span
          className={`${usesHour12(fmt) ? "w-[156px]" : "w-[120px]"} shrink-0 whitespace-nowrap text-[12.5px] tabular-nums text-white/45`}
        >
          {session.live
            ? t("playtime.session.toNow", { time: fmtClockTime(session.start, fmt) })
            : fmtTimeRange(session.start, end, fmt)}
          {!session.live && laterDays > 0 && (
            <sup className="ml-[2px] text-[9.5px] text-white/55">+{laterDays}</sup>
          )}
        </span>
        <span
          className="h-[7px] w-[7px] shrink-0 rounded-full"
          style={{ background: color(g.id) }}
        />
        <img
          src={iconSrcFor(g.id, icons)}
          onError={iconFallback(g.id)}
          alt=""
          className="h-[34px] w-[34px] shrink-0 rounded-[9px] object-cover"
        />
        <span className="min-w-0 flex-1 truncate text-[13.5px] font-medium text-white/90">
          {g.name}
        </span>
        <span className="shrink-0 text-[13.5px] font-semibold">
          {fmtSessionClock(session.minutes)}
        </span>
      </IconButton>
      {session.local !== false ? (
        <RemoveSessionButton onConfirm={() => onRemove(session)} />
      ) : (
        <span className="mr-[6px] w-[28px] shrink-0" aria-hidden />
      )}
    </div>
  );
}

function RemoveSessionButton({ onConfirm }: { onConfirm: () => Promise<void> }) {
  const [armed, setArmed] = useState(false);
  const [busy, setBusy] = useState(false);
  const t = useT();
  const label = armed ? t("playtime.session.removeConfirm") : t("playtime.session.remove");
  const disarm = () => {
    if (!busy) setArmed(false);
  };

  const onClick = async () => {
    if (!armed) {
      setArmed(true);
      return;
    }
    setBusy(true);
    try {
      await onConfirm();
    } finally {
      setBusy(false);
      setArmed(false);
    }
  };

  return (
    <IconButton
      label={label}
      onPointerLeave={disarm}
      onBlur={disarm}
      onClick={() => void onClick()}
      disabled={busy}
      className={`mr-[6px] grid h-[28px] shrink-0 place-items-center rounded-[7px] transition ${
        armed
          ? "bg-white px-[9px] text-[12px] font-semibold text-black"
          : "w-[28px] text-white/40 opacity-0 hover:bg-white/10 hover:text-white focus-visible:opacity-100 group-hover:opacity-100"
      }`}
    >
      {armed ? t("common.remove") : <Trash2 size={14} />}
    </IconButton>
  );
}

function EmptyState({ filtered, range }: { filtered: boolean; range: PlaytimeRange }) {
  const t = useT();
  return (
    <div className="rounded-ui border border-white/[0.08] bg-white/[0.03] px-6 py-12 text-center">
      <p className="text-[13.5px] font-medium text-white/70">{t("playtime.empty.title")}</p>
      <p className="mt-[6px] text-[12.5px] text-white/55">
        {filtered
          ? t("playtime.empty.filtered")
          : range === "All"
            ? t("playtime.empty.all")
            : t("playtime.empty.range", { count: RANGE_DAYS[range] })}
      </p>
    </div>
  );
}

function RailSkeleton() {
  return (
    <div className="animate-pulse" aria-hidden>
      <div className="h-[12px] w-[70px] rounded-full bg-white/[0.07]" />
      <div className="mt-[14px] h-[46px] w-[180px] rounded-[10px] bg-white/[0.07]" />
      <div className="mt-[26px] flex flex-col gap-[14px]">
        {[0, 1, 2, 3].map((i) => (
          <div key={i} className="h-[14px] rounded-full bg-white/[0.06]" />
        ))}
      </div>
      <div className="mt-[30px] h-[92px] rounded-ui bg-white/[0.06]" />
      <div className="mt-[30px] h-[9px] rounded-full bg-white/[0.07]" />
      <div className="mt-[18px] flex flex-col gap-[13px]">
        {[0, 1, 2, 3, 4].map((i) => (
          <div key={i} className="h-[14px] rounded-full bg-white/[0.06]" />
        ))}
      </div>
    </div>
  );
}

function SessionsSkeleton() {
  return (
    <div className="flex animate-pulse flex-col gap-[26px]" aria-hidden>
      {[0, 1, 2].map((g) => (
        <div key={g}>
          <div className="mb-[10px] h-[13px] w-[190px] rounded-full bg-white/[0.07]" />
          <div className="flex flex-col gap-[10px]">
            {[0, 1].map((i) => (
              <div key={i} className="h-[44px] rounded-[10px] bg-white/[0.05]" />
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

// ------------ Playtime Charts ------------
// The two small charts on the Playtime page: the day-by-day bars for recent play and the bar showing each game's
// share of the total. Hovering shows the exact times.
type Tip = { x: number; y: number; body: ReactNode };

function useTip() {
  const [tip, setTip] = useState<Tip | null>(null);
  const bind = (body: ReactNode) => ({
    onMouseEnter: (e: React.MouseEvent) => setTip({ x: e.clientX, y: e.clientY, body }),
    onMouseMove: (e: React.MouseEvent) =>
      setTip((cur) => (cur ? { ...cur, x: e.clientX, y: e.clientY } : cur)),
    onMouseLeave: () => setTip(null),
  });
  const portal = tip ? (
    <TooltipPortal x={tip.x} y={tip.y} placement="cursor">
      {tip.body}
    </TooltipPortal>
  ) : null;
  return { bind, portal };
}

function TipRow({ id, minutes }: { id: string; minutes: number }) {
  const color = useGameColor();
  return (
    <span className="flex items-center gap-[7px]">
      <span
        className="inline-block h-[2px] w-[10px] rounded-full"
        style={{ background: color(id) }}
      />
      <span className="font-semibold">{fmtHM(minutes)}</span>
      <span className="font-normal text-white/60">{gameById(id as GameId).name}</span>
    </span>
  );
}

function DayTip({ day }: { day: MiniDay }) {
  const t = useT();
  const rows = Object.entries(day.byGame).sort((a, b) => b[1] - a[1]);
  return (
    <span className="flex flex-col gap-[3px]">
      <span className="text-white/60">
        {fmtDayTip(day.date)} ·{" "}
        <span className="font-semibold text-white">
          {day.minutes > 0 ? fmtHM(day.minutes) : t("playtime.charts.noPlaytime")}
        </span>
      </span>
      {rows.map(([id, min]) => (
        <TipRow key={id} id={id} minutes={min} />
      ))}
    </span>
  );
}

function dayLabel(day: MiniDay): string {
  const date = fmtDayTip(day.date);
  if (day.minutes <= 0) return t("playtime.charts.dayNoPlaytime", { date });
  const rows = Object.entries(day.byGame)
    .filter(([, min]) => min > 0)
    .sort((a, b) => b[1] - a[1]);
  const time = fmtHM(day.minutes);
  if (rows.length > 1) {
    const games = rows
      .map(([id, min]) =>
        t("playtime.charts.gameTime", { game: gameById(id as GameId).name, time: fmtHM(min) }),
      )
      .join(", ");
    return t("playtime.charts.dayManyGames", { date, time, games });
  }
  if (rows.length === 1) {
    return t("playtime.charts.daySingleGame", {
      date,
      time,
      game: gameById(rows[0][0] as GameId).name,
    });
  }
  return t("playtime.charts.dayTotal", { date, time });
}

const BAR_AREA = 92;
const SEG_MIN = 3;
const SEG_GAP = 1;

function MiniBars({ days }: { days: MiniDay[] }) {
  const t = useT();
  const { bind, portal } = useTip();
  const color = useGameColor();
  const [hover, setHover] = useState<string | null>(null);
  const max = Math.max(...days.map((d) => d.minutes), 1);
  const first = days[0];

  return (
    <>
      <div
        role="group"
        aria-label={t("playtime.charts.lastDays", { count: days.length })}
        className="flex h-[92px] items-end justify-between gap-[4px]"
      >
        {days.map((day) => {
          const stack = Object.entries(day.byGame)
            .filter(([, min]) => min > 0)
            .sort((a, b) => b[1] - a[1]);
          const needed = stack.length * SEG_MIN + (stack.length - 1) * SEG_GAP;
          const height = Math.min(
            BAR_AREA,
            Math.max(needed, 5, (day.minutes / max) * BAR_AREA),
          );
          const dim = hover !== null && hover !== day.key;
          return (
            <div
              key={day.key}
              role="img"
              aria-label={dayLabel(day)}
              className="flex h-full min-w-0 max-w-[30px] flex-1 cursor-default items-end"
              onMouseOver={() => setHover(day.key)}
              onMouseOut={() => setHover((h) => (h === day.key ? null : h))}
              {...bind(<DayTip day={day} />)}
            >
              {stack.length > 0 ? (
                <div
                  className="flex w-full flex-col-reverse gap-[1px] overflow-hidden rounded-[3px] transition-opacity duration-150"
                  style={{ height: `${Math.round(height)}px`, opacity: dim ? 0.45 : 1 }}
                >
                  {stack.map(([id, min]) => (
                    <span
                      key={id}
                      className="block w-full"
                      style={{
                        flex: `${min} 1 0`,
                        minHeight: `${SEG_MIN}px`,
                        background: color(id),
                      }}
                    />
                  ))}
                </div>
              ) : (
                <div className="h-[2px] w-full rounded-full bg-white/[0.09]" />
              )}
            </div>
          );
        })}
      </div>
      <div className="mt-[9px] flex items-center justify-between text-[11px] text-white/55">
        <span>
          {first?.date.toLocaleDateString(formatLocale(), { day: "numeric", month: "short" }) ?? ""}
        </span>
        <span>{t("common.today")}</span>
      </div>
      {portal}
    </>
  );
}

function ShareBar({
  totals,
  filter,
  onPick,
}: {
  totals: ShareSlice[];
  filter: GameFilter;
  onPick: (id: GameFilter) => void;
}) {
  const { bind, portal } = useTip();
  const color = useGameColor();
  const t = useT();
  const sum = totals.reduce((a, slice) => a + slice.minutes, 0);
  if (sum <= 0) return <div className="h-[9px] w-full rounded-full bg-white/[0.07]" />;

  return (
    <>
      <div className="flex h-[9px] w-full overflow-hidden rounded-full bg-white/[0.07]">
        {totals.map((slice) => {
          const pct = (slice.minutes / sum) * 100;
          const dim = filter !== "all" && filter !== slice.id;
          return (
            <button
              key={slice.id}
              onClick={() => onPick(filter === slice.id ? "all" : slice.id)}
              aria-pressed={filter === slice.id}
              aria-label={t("playtime.charts.share", { game: slice.name, pct: Math.round(pct) })}
              className="h-full min-w-[3px] transition-opacity duration-150"
              style={{
                width: `${pct}%`,
                background: color(slice.id),
                opacity: dim ? 0.3 : 1,
              }}
              {...bind(
                <span className="flex items-center gap-[7px]">
                  <TipRow id={slice.id} minutes={slice.minutes} />
                  <span className="text-white/45">{Math.round(pct)}%</span>
                </span>,
              )}
            />
          );
        })}
      </div>
      {portal}
    </>
  );
}
