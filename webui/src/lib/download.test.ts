import { describe, expect, it } from "vitest";
import type { QueueJob } from "./ipc";
import {
  phaseLabel,
  verifyFoundBrokenFiles,
  overallPercent,
  stageIndexFor,
  stagePercent,
  stagesFor,
} from "./download";

const job = (over: Partial<QueueJob>): QueueJob => ({
  id: "j",
  gameId: "g",
  kind: "install",
  phase: "downloading",
  total: 1000,
  downloaded: 400,
  percent: 40,
  speed: 0,
  networkSpeed: 0,
  diskSpeed: 0,
  etaSecs: 0,
  paused: false,
  error: null,
  ...over,
});

describe("verifyFoundBrokenFiles", () => {
  it("matches the backend's broken-files verify errors", () => {
    expect(verifyFoundBrokenFiles("1 file needs repair. Run a repair to replace it.")).toBe(true);
    expect(verifyFoundBrokenFiles("3 files need repair. Run a repair to replace them.")).toBe(true);
    expect(
      verifyFoundBrokenFiles(
        "2 of 900 Brown Dust II files do not match version 1.2.3. Repair replaces them.",
      ),
    ).toBe(true);
  });

  it("leaves other verify failures to Retry", () => {
    expect(verifyFoundBrokenFiles(null)).toBe(false);
    expect(verifyFoundBrokenFiles("Game path not set.")).toBe(false);
    expect(
      verifyFoundBrokenFiles(
        "3 files differ from version 1.2.0. An update is available, and it replaces these files.",
      ),
    ).toBe(false);
  });
});

describe("phaseLabel", () => {
  it("says a job held for a running game is paused while you play", () => {
    expect(phaseLabel(job({ heldForGame: true }))).toBe("Paused while you play");
    expect(phaseLabel(job({ heldForGame: true, paused: true }))).toBe("Paused");
  });
});

const stageJob = (kind: string, phase: string, percent = 0, total = 0): QueueJob => ({
  id: "j",
  gameId: "g",
  kind,
  phase,
  total,
  downloaded: 0,
  percent,
  speed: 0,
  networkSpeed: 0,
  diskSpeed: 0,
  etaSecs: 0,
  paused: false,
  error: null,
});

const indexOf = (kind: string, phase: string): number =>
  stageIndexFor(stageJob(kind, phase), stagesFor(kind));

describe("stageIndexFor", () => {
  it("keeps a repair that is still fetching on the Scan stage", () => {
    expect(indexOf("repair", "downloading")).toBe(0);
    expect(indexOf("repair", "validating")).toBe(1);
    expect(indexOf("repair", "verifying")).toBe(1);
    expect(indexOf("repair", "repairing")).toBe(2);
  });

  it("maps install and move phases to their own stages", () => {
    expect(indexOf("install", "scanning")).toBe(0);
    expect(stageIndexFor(stageJob("install", "downloading", 0, 1024), stagesFor("install"))).toBe(1);
    expect(indexOf("install", "validating")).toBe(2);
    expect(indexOf("install", "extracting")).toBe(2);
    expect(indexOf("move", "downloading")).toBe(0);
    expect(indexOf("move", "moving")).toBe(0);
  });

  it("handles queued and done", () => {
    expect(indexOf("repair", "queued")).toBe(-1);
    expect(indexOf("repair", "done")).toBe(3);
  });

  it("shows a download's check as its own stage before Download", () => {
    const stages = stagesFor("update");
    expect(stages.map((s) => s.id)).toEqual(["check", "download", "finalize"]);
    expect(indexOf("update", "scanning")).toBe(0);
    expect(indexOf("update", "downloading")).toBe(0);
    expect(stageIndexFor(stageJob("update", "downloading", 0, 1024), stages)).toBe(1);
    expect(indexOf("repair", "scanning")).toBe(1);
    expect(indexOf("verify", "scanning")).toBe(0);
  });
});

describe("overallPercent", () => {
  it("never moves a repair backwards from fetching to validating", () => {
    const fetching = overallPercent(stageJob("repair", "downloading", 100), "repair");
    const validating = overallPercent(stageJob("repair", "validating", 0), "repair");
    expect(fetching).toBeLessThanOrEqual(validating);
  });

  it("starts a repair's Replace stage where its file check ended", () => {
    const checked = overallPercent(stageJob("repair", "verifying", 100), "repair");
    const replacing = overallPercent(stageJob("repair", "repairing", 0), "repair");
    expect(replacing).toBeCloseTo(checked);
    expect(replacing).toBeLessThan(100);
  });

  it("counts a download's check and carries on from it into the download", () => {
    const checking = overallPercent(stageJob("update", "scanning", 50, 1024), "update");
    const checked = overallPercent(stageJob("update", "scanning", 100, 1024), "update");
    const started = overallPercent(stageJob("update", "downloading", 0, 1024), "update");
    expect(checking).toBeGreaterThan(0);
    expect(checked).toBeCloseTo(started);
    expect(started).toBeLessThan(100);
    expect(stagePercent(stageJob("update", "scanning", 90, 1024))).toBe(90);
  });
});
