// ------------ Playtime Stats ------------
// Math and formatting behind the playtime page: totals per range, streaks, percent change, daily bars,
// and grouping sessions by day. Also the small shapes the playtime charts pass around.
import { fmtClockTime, type TimeFormat } from "./format";
import { formatLocale, t } from "./i18n";
import type { DailyMap, SessionEntry } from "./ipc";
import type { GameId } from "../data/games";

export interface ShareSlice {
  id: GameId;
  name: string;
  minutes: number;
}

export interface GameTotal extends ShareSlice {
  sessions: number;
  longestSessionMinutes: number;
}

export type GameFilter = "all" | string;
export type PlaytimeRange = "Week" | "Month" | "All";

export const RANGE_DAYS: Record<PlaytimeRange, number> = { Week: 7, Month: 30, All: 0 };
export const RANGE_LABEL: Record<PlaytimeRange, string> = {
  get Week() {
    return t("games.playtime.range.week");
  },
  get Month() {
    return t("games.playtime.range.month");
  },
  get All() {
    return t("common.allTime");
  },
};

export const MINI_DAYS = 14;

function dateKey(d: Date): string {
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${mm}-${dd}`;
}

function daysAgo(n: number): Date {
  const d = new Date();
  d.setHours(12, 0, 0, 0);
  d.setDate(d.getDate() - n);
  return d;
}

function parseKey(key: string): Date {
  const [y, m, d] = key.split("-").map(Number);
  return new Date(y, (m || 1) - 1, d || 1, 12);
}

function dayMinutes(daily: DailyMap, key: string, filter: GameFilter): number {
  const day = daily[key];
  if (!day) return 0;
  if (filter === "all") return Object.values(day).reduce((a, b) => a + b, 0);
  return day[filter] ?? 0;
}

export function rangeTotal(
  daily: DailyMap,
  days: number,
  filter: GameFilter,
  offsetDays = 0,
): number {
  let total = 0;
  for (let i = offsetDays; i < offsetDays + days; i++) {
    total += dayMinutes(daily, dateKey(daysAgo(i)), filter);
  }
  return total;
}

export function totalsByGame(daily: DailyMap, days: number): Record<string, number> {
  const out: Record<string, number> = {};
  for (let i = 0; i < days; i++) {
    for (const [id, min] of Object.entries(daily[dateKey(daysAgo(i))] ?? {})) {
      out[id] = (out[id] ?? 0) + min;
    }
  }
  return out;
}

export function pctChange(current: number, previous: number): number | null {
  if (previous <= 0) return null;
  const pct = Math.round(((current - previous) / previous) * 100);
  return pct === 0 ? null : pct;
}

export interface MiniDay {
  key: string;
  date: Date;
  minutes: number;
  byGame: Record<string, number>;
}

export function buildMiniDays(daily: DailyMap, days: number, filter: GameFilter): MiniDay[] {
  return Array.from({ length: days }, (_, i) => {
    const date = daysAgo(days - 1 - i);
    const key = dateKey(date);
    const byGame: Record<string, number> = {};
    let minutes = 0;
    for (const [id, min] of Object.entries(daily[key] ?? {})) {
      if (filter !== "all" && id !== filter) continue;
      byGame[id] = min;
      minutes += min;
    }
    return { key, date, minutes, byGame };
  });
}

interface Streaks {
  current: number;
  longest: number;
}

export function computeStreaks(daily: DailyMap, filter: GameFilter): Streaks {
  const activeAt = (i: number) => dayMinutes(daily, dateKey(daysAgo(i)), filter) > 0;
  let longest = 0;
  let run = 0;
  for (let i = 364; i >= 0; i--) {
    run = activeAt(i) ? run + 1 : 0;
    if (run > longest) longest = run;
  }
  let current = 0;
  for (let i = activeAt(0) ? 0 : 1; i < 365 && activeAt(i); i++) current++;
  return { current, longest };
}

export function rangeCutoff(range: PlaytimeRange): number {
  const days = RANGE_DAYS[range];
  if (days <= 0) return -Infinity;
  const from = daysAgo(days - 1);
  from.setHours(0, 0, 0, 0);
  return from.getTime();
}

interface DayGroup<T> {
  key: string;
  date: Date;
  items: T[];
}

export function groupByDay<T>(items: readonly T[], at: (item: T) => number): DayGroup<T>[] {
  const groups = new Map<string, DayGroup<T>>();
  for (const item of items) {
    const key = dateKey(new Date(at(item)));
    let group = groups.get(key);
    if (!group) groups.set(key, (group = { key, date: parseKey(key), items: [] }));
    group.items.push(item);
  }
  return [...groups.values()];
}

export type SessionDay = DayGroup<SessionEntry> & { minutes: number };

export const groupSessionsByDay = (sessions: SessionEntry[]): SessionDay[] =>
  groupByDay([...sessions].sort((a, b) => b.start - a.start), (s) => s.start).map((day) => ({
    ...day,
    minutes: day.items.reduce((sum, s) => sum + s.minutes, 0),
  }));

export function fmtHoursShort(min: number): string {
  if (min <= 0) return t("games.playtime.hoursShort", { h: 0 });
  const h = Math.round(min / 60);
  return h < 1 ? t("games.playtime.lessThanHour") : t("games.playtime.hoursShort", { h });
}

export const fmtDayTip = (d: Date) =>
  d.toLocaleDateString(formatLocale(), { weekday: "short", day: "numeric", month: "short" });

export function fmtDayHeading(date: Date): string {
  const today = daysAgo(0);
  const day = new Date(date);
  day.setHours(12, 0, 0, 0);
  const diff = Math.round((today.getTime() - day.getTime()) / 86400000);
  if (diff === 0) return t("games.playtime.todayHeading", { date: fmtDayTip(day) });
  if (diff === 1) return t("games.playtime.yesterdayHeading", { date: fmtDayTip(day) });
  if (day.getFullYear() !== today.getFullYear()) {
    return day.toLocaleDateString(formatLocale(), {
      weekday: "short",
      day: "numeric",
      month: "short",
      year: "numeric",
    });
  }
  return fmtDayTip(day);
}

export function sessionEnd(s: SessionEntry): number {
  return typeof s.end === "number" && s.end >= s.start ? s.end : s.start + s.minutes * 60000;
}

export function endDayOffset(startMs: number, endMs: number): number {
  const day = (ms: number) => parseKey(dateKey(new Date(ms))).getTime();
  return Math.max(0, Math.round((day(endMs) - day(startMs)) / 86400000));
}

let systemHour12: { locale: string | undefined; hour12: boolean } | undefined;

export function usesHour12(fmt: TimeFormat): boolean {
  if (fmt !== "system") return fmt === "12";
  const locale = formatLocale();
  if (!systemHour12 || systemHour12.locale !== locale) {
    const hour12 =
      new Intl.DateTimeFormat(locale, { hour: "numeric" }).resolvedOptions().hour12 === true;
    systemHour12 = { locale, hour12 };
  }
  return systemHour12.hour12;
}

export function fmtTimeRange(startMs: number, endMs: number, fmt: TimeFormat): string {
  return t("games.playtime.timeRange", {
    start: fmtClockTime(startMs, fmt),
    end: fmtClockTime(endMs, fmt),
  });
}
