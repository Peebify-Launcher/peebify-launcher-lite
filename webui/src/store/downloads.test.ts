import { describe, expect, it, vi } from "vitest";
import type { QueueJob } from "../lib/ipc";

const STORAGE_KEY = "peebify.downloadHistory";
const storage = new Map<string, string>();

vi.stubGlobal("localStorage", {
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => void storage.set(key, value),
  removeItem: (key: string) => void storage.delete(key),
});

storage.set(
  STORAGE_KEY,
  JSON.stringify([
    {
      key: "job-1-1700000000000",
      gameId: "genshin",
      kind: "install",
      phase: "done",
      error: null,
      message: null,
      finishedAt: 1700000000000,
      total: 123456789,
    },
  ]),
);

const { useDownloadHistoryStore } = await import("./downloads");

describe("downloadHistoryStore", () => {
  it("loads an old entry that still carries `total`", () => {
    expect(useDownloadHistoryStore.getState().entries).toEqual([
      {
        key: "job-1-1700000000000",
        gameId: "genshin",
        kind: "install",
        phase: "done",
        error: null,
        message: null,
        finishedAt: 1700000000000,
      },
    ]);
  });

  it("does not save `total` again when a new entry is added", () => {
    useDownloadHistoryStore.getState().add({
      id: "job-2",
      gameId: "hsr",
      kind: "update",
      phase: "error",
      error: "disk full",
      message: null,
    } as QueueJob);

    const saved = JSON.parse(storage.get(STORAGE_KEY) ?? "[]") as Record<string, unknown>[];
    expect(saved).toHaveLength(2);
    expect(saved[0]).toMatchObject({ gameId: "hsr", phase: "error", error: "disk full" });
    for (const entry of saved) expect(entry).not.toHaveProperty("total");
  });
});
