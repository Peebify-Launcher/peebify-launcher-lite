import { describe, expect, it } from "vitest";
import {
  fmtBytes,
  fmtCompact,
  fmtDuration,
  fmtEta,
  fmtHM,
  fmtRelTime,
  fmtRelativeDays,
  fmtSpeed,
} from "./format";

describe("fmtBytes", () => {
  it("uses decimal units and trims a trailing .0 on gigabytes", () => {
    expect(fmtBytes(512)).toBe("512 B");
    expect(fmtBytes(1500)).toBe("2 KB");
    expect(fmtBytes(25_400_000)).toBe("25 MB");
    expect(fmtBytes(3_000_000_000)).toBe("3 GB");
    expect(fmtBytes(3_450_000_000)).toBe("3.5 GB");
    expect(fmtBytes(10_000_000_000)).toBe("10 GB");
  });

  it("moves up a unit when rounding would reach a thousand", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(999)).toBe("999 B");
    expect(fmtBytes(999_400)).toBe("999 KB");
    expect(fmtBytes(999_500)).toBe("1 MB");
    expect(fmtBytes(3_000_000)).toBe("3 MB");
    expect(fmtBytes(999_400_000)).toBe("999 MB");
    expect(fmtBytes(999_950_000)).toBe("1 GB");
  });

  it("switches to terabytes instead of showing a thousand gigabytes", () => {
    expect(fmtBytes(999_400_000_000)).toBe("999.4 GB");
    expect(fmtBytes(999_500_000_000)).toBe("1 TB");
    expect(fmtBytes(1_834_200_000_000)).toBe("1.8 TB");
    expect(fmtBytes(3_500_000_000_000)).toBe("3.5 TB");
  });
});

describe("fmtSpeed", () => {
  it("is empty when idle and never shows zero kilobytes", () => {
    expect(fmtSpeed(0)).toBe("");
    expect(fmtSpeed(200)).toBe("1 KB/s");
    expect(fmtSpeed(450_000)).toBe("450 KB/s");
    expect(fmtSpeed(2_500_000)).toBe("2.5 MB/s");
    expect(fmtSpeed(42_000_000)).toBe("42 MB/s");
  });

  it("moves up a unit or drops the decimal when rounding would reach the next step", () => {
    expect(fmtSpeed(999_400)).toBe("999 KB/s");
    expect(fmtSpeed(999_600)).toBe("1.0 MB/s");
    expect(fmtSpeed(9_940_000)).toBe("9.9 MB/s");
    expect(fmtSpeed(9_970_000)).toBe("10 MB/s");
    expect(fmtSpeed(10_000_000)).toBe("10 MB/s");
  });
});

describe("fmtEta", () => {
  it("drops fractions and shows minutes only when there are any", () => {
    expect(fmtEta(0)).toBe("");
    expect(fmtEta(9.9)).toBe("9s");
    expect(fmtEta(125)).toBe("2m 5s");
  });

  it("switches to hours and minutes at an hour and is empty for nonsense", () => {
    expect(fmtEta(-3)).toBe("");
    expect(fmtEta(Number.POSITIVE_INFINITY)).toBe("");
    expect(fmtEta(Number.NaN)).toBe("");
    expect(fmtEta(59)).toBe("59s");
    expect(fmtEta(61)).toBe("1m 1s");
    expect(fmtEta(3599)).toBe("59m 59s");
    expect(fmtEta(3600)).toBe("1h 0m");
    expect(fmtEta(3661)).toBe("1h 1m");
    expect(fmtEta(18_000)).toBe("5h 0m");
  });
});

describe("fmtHM", () => {
  it("rounds to whole minutes", () => {
    expect(fmtHM(0)).toBe("0m");
    expect(fmtHM(0.4)).toBe("0m");
    expect(fmtHM(59.6)).toBe("1h 0m");
    expect(fmtHM(135)).toBe("2h 15m");
  });

  it("drops the hour part under an hour", () => {
    expect(fmtHM(45)).toBe("45m");
    expect(fmtHM(59.4)).toBe("59m");
    expect(fmtHM(65)).toBe("1h 5m");
  });

  it("treats negative or non-finite input as zero", () => {
    expect(fmtHM(-5)).toBe("0m");
    expect(fmtHM(Number.NaN)).toBe("0m");
  });
});

describe("fmtRelTime", () => {
  const now = 10_000_000_000;
  it("steps from minutes to hours to days", () => {
    expect(fmtRelTime(now - 30_000, now)).toBe("just now");
    expect(fmtRelTime(now - 5 * 60_000, now)).toBe("5m ago");
    expect(fmtRelTime(now - 3 * 3_600_000, now)).toBe("3h ago");
    expect(fmtRelTime(now - 50 * 3_600_000, now)).toBe("2d ago");
  });

  it("goes on to months and years", () => {
    const day = 86_400_000;
    expect(fmtRelTime(now - 29 * day, now)).toBe("29d ago");
    expect(fmtRelTime(now - 45 * day, now)).toBe("1mo ago");
    expect(fmtRelTime(now - 212 * day, now)).toBe("7mo ago");
    expect(fmtRelTime(now - 400 * day, now)).toBe("1y ago");
    expect(fmtRelTime(now - 800 * day, now)).toBe("2y ago");
  });
});

describe("fmtRelativeDays", () => {
  const nowMs = 1_000_000 * 86_400_000;
  const daysAgo = (d: number) => nowMs / 1000 - d * 86_400;
  it("names today and yesterday then counts days, months and years", () => {
    expect(fmtRelativeDays(null, nowMs)).toBe("");
    expect(fmtRelativeDays(daysAgo(0), nowMs)).toBe("today");
    expect(fmtRelativeDays(daysAgo(1), nowMs)).toBe("yesterday");
    expect(fmtRelativeDays(daysAgo(12), nowMs)).toBe("12d ago");
    expect(fmtRelativeDays(daysAgo(95), nowMs)).toBe("3mo ago");
    expect(fmtRelativeDays(daysAgo(800), nowMs)).toBe("2y ago");
  });

  it("counts calendar days in local time rather than 24 hour spans", () => {
    const at = (d: number, h: number) => new Date(2026, 2, d, h).getTime();
    const secs = (ms: number) => ms / 1000;
    expect(fmtRelativeDays(secs(at(14, 23)), at(15, 9))).toBe("yesterday");
    expect(fmtRelativeDays(secs(at(15, 0)), at(15, 23))).toBe("today");
    expect(fmtRelativeDays(secs(at(13, 23)), at(15, 5))).toBe("2d ago");
  });
});

describe("fmtDuration", () => {
  it("pads seconds always and minutes only when there are hours", () => {
    expect(fmtDuration(null)).toBe("");
    expect(fmtDuration(0)).toBe("");
    expect(fmtDuration(65_000)).toBe("1:05");
    expect(fmtDuration(3_723_000)).toBe("1:02:03");
  });
});

describe("fmtCompact", () => {
  it("keeps one decimal below ten thousand", () => {
    expect(fmtCompact(999)).toBe("999");
    expect(fmtCompact(1_250)).toBe("1.3k");
    expect(fmtCompact(12_600)).toBe("13k");
  });

  it("switches to millions and never shows 10.0k or 1000k", () => {
    expect(fmtCompact(1_000)).toBe("1k");
    expect(fmtCompact(9_999)).toBe("10k");
    expect(fmtCompact(999_499)).toBe("999k");
    expect(fmtCompact(999_999)).toBe("1M");
    expect(fmtCompact(1_500_000)).toBe("1.5M");
    expect(fmtCompact(2_345_000)).toBe("2.3M");
    expect(fmtCompact(12_400_000)).toBe("12M");
  });
});
