import { formatLocale, t } from "./i18n";

// ------------ Formatting ------------
// Turns raw numbers into text people can read: file sizes, speeds, time left, durations, and
// relative times like "5m ago".
export function fmtBytes(n: number): string {
  if (n >= 999.5e9) return t("format.tb", { n: (n / 1e12).toFixed(1).replace(/\.0$/, "") });
  if (n >= 999.5e6) return t("format.gb", { n: (n / 1e9).toFixed(1).replace(/\.0$/, "") });
  if (n >= 999.5e3) return t("format.mb", { n: Math.round(n / 1e6) });
  if (n >= 999.5) return t("format.kb", { n: Math.round(n / 1e3) });
  return t("format.b", { n });
}

export function fmtSpeed(n: number): string {
  if (n <= 0) return "";
  if (n >= 9.95e6) return t("format.mbps", { n: Math.round(n / 1e6) });
  if (n >= 999.5e3) return t("format.mbps", { n: (n / 1e6).toFixed(1) });
  return t("format.kbps", { n: Math.max(1, Math.round(n / 1e3)) });
}

export function fmtEta(secs: number): string {
  if (!Number.isFinite(secs) || secs <= 0) return "";
  const whole = Math.floor(secs);
  const h = Math.floor(whole / 3600);
  const m = Math.floor((whole % 3600) / 60);
  const s = whole % 60;
  if (h > 0) return t("format.hm", { h, m });
  return m > 0 ? t("format.ms", { m, s }) : t("format.s", { s });
}

export function fmtHM(min: number): string {
  const m = Math.round(min);
  if (!(m > 0)) return t("format.m", { m: 0 });
  if (m < 60) return t("format.m", { m });
  return t("format.hm", { h: Math.floor(m / 60), m: m % 60 });
}

export function fmtRelTime(ts: number, nowMs: number): string {
  if (!Number.isFinite(ts)) return "";
  const m = Math.floor((nowMs - ts) / 60000);
  if (m < 1) return t("format.justNow");
  if (m < 60) return t("format.minutesAgo", { n: m });
  const h = Math.floor(m / 60);
  if (h < 24) return t("format.hoursAgo", { n: h });
  const d = Math.floor(h / 24);
  if (d < 30) return t("format.daysAgo", { n: d });
  if (d < 365) return t("format.monthsAgo", { n: Math.floor(d / 30) });
  return t("format.yearsAgo", { n: Math.floor(d / 365) });
}

export type TimeFormat = "system" | "12" | "24";

function timeOpts(fmt: TimeFormat): Intl.DateTimeFormatOptions {
  if (fmt === "12") return { hour: "numeric", minute: "2-digit", hour12: true };
  if (fmt === "24") return { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
  return { hour: "numeric", minute: "2-digit" };
}

export function fmtClockTime(ts: number, fmt: TimeFormat): string {
  return new Date(ts).toLocaleTimeString(formatLocale(), timeOpts(fmt));
}

export function fmtDateTime(ts: number, fmt: TimeFormat): string {
  return new Date(ts).toLocaleString(formatLocale(), {
    year: "numeric",
    month: "numeric",
    day: "numeric",
    ...timeOpts(fmt),
  });
}

export function fmtRelativeDays(epochSeconds: number | null, nowMs: number): string {
  if (!epochSeconds) return "";
  const today = new Date(nowMs);
  today.setHours(0, 0, 0, 0);
  const then = new Date(epochSeconds * 1000);
  then.setHours(0, 0, 0, 0);
  const days = Math.round((today.getTime() - then.getTime()) / 86_400_000);
  if (days <= 0) return t("format.today");
  if (days === 1) return t("format.yesterday");
  if (days < 30) return t("format.daysAgo", { n: days });
  if (days < 365) return t("format.monthsAgo", { n: Math.floor(days / 30) });
  return t("format.yearsAgo", { n: Math.floor(days / 365) });
}

export function fmtDuration(ms: number | null | undefined): string {
  if (!ms || ms <= 0) return "";
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const mm = h > 0 ? String(m).padStart(2, "0") : String(m);
  return `${h > 0 ? `${h}:` : ""}${mm}:${String(s).padStart(2, "0")}`;
}

export function fmtCompact(n: number): string {
  if (n >= 999_500) {
    return t("format.millions", { n: (n / 1e6).toFixed(n >= 9_950_000 ? 0 : 1).replace(/\.0$/, "") });
  }
  if (n >= 1000) {
    return t("format.thousands", { n: (n / 1000).toFixed(n >= 9950 ? 0 : 1).replace(/\.0$/, "") });
  }
  return String(n);
}
