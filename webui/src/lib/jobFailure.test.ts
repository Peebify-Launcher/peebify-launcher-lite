import { beforeEach, describe, expect, it } from "vitest";
import { reportJobFailure, watchQueuedJob, type QueueJob } from "./ipc";
import { jobFailureKey, useNotificationStore } from "../store/notifications";
import { useQueueStore } from "../store/queueStore";

const job = (gameId: string, kind: string, phase: string, error: string | null = null) =>
  ({
    id: gameId,
    gameId,
    kind,
    baseKind: kind,
    phase,
    total: 0,
    downloaded: 0,
    percent: 0,
    speed: 0,
    networkSpeed: 0,
    diskSpeed: 0,
    etaSecs: 0,
    paused: false,
    error,
  }) as QueueJob;

const queueNotice = (gameId: string, error: string) =>
  useNotificationStore.getState().push({
    type: "error",
    title: "Genshin Impact update failed",
    text: error,
    key: jobFailureKey(gameId),
  });

const errors = () => useNotificationStore.getState().items.filter((n) => n.type === "error");

describe("a failed queued job", () => {
  beforeEach(() => {
    useNotificationStore.setState({ items: [], toasts: [], open: false });
    useQueueStore.setState({ jobs: [] });
  });

  it("is told once, by the queue, when the job reached the queue", () => {
    let queued = 0;
    const watch = watchQueuedJob("genshin", ["install", "update"], () => (queued += 1));
    useQueueStore.setState({ jobs: [job("genshin", "update", "queued")] });
    useQueueStore.setState({ jobs: [job("genshin", "update", "downloading")] });
    useQueueStore.setState({ jobs: [job("genshin", "update", "error", "Disk full.")] });
    queueNotice("genshin", "Disk full.");
    reportJobFailure("actions.failed.download", "genshin", "Download failed: Disk full.", watch.taken());
    watch.stop();

    expect(queued).toBe(1);
    expect(errors()).toHaveLength(1);
    expect(errors()[0].title).toBe("Genshin Impact update failed");
  });

  it("is told once when the reply arrives before the queue's last event", () => {
    reportJobFailure("actions.failed.moveGame", "genshin", "Move failed: Access denied.", false);
    queueNotice("genshin", "Access denied.");

    expect(errors()).toHaveLength(1);
  });

  it("is told by the reply when the backend refused it before queueing", () => {
    let queued = 0;
    const watch = watchQueuedJob("genshin", ["move"], () => (queued += 1));
    reportJobFailure("actions.failed.moveGame", "genshin", "Close Genshin Impact first.", watch.taken());
    watch.stop();

    expect(queued).toBe(0);
    expect(errors()).toHaveLength(1);
    expect(errors()[0].text).toBe("Close Genshin Impact first.");
  });

  it("does not take a job of that kind that was already running as its own", () => {
    useQueueStore.setState({ jobs: [job("genshin", "move", "downloading")] });
    const watch = watchQueuedJob("genshin", ["move"]);
    useQueueStore.setState({ jobs: [job("genshin", "move", "downloading")] });
    expect(watch.taken()).toBe(false);
    watch.stop();
  });

  it("keeps failures of different games apart", () => {
    reportJobFailure("actions.failed.download", "genshin", "Offline.", false);
    reportJobFailure("actions.failed.download", "hsr", "Server error.", false);
    expect(errors()).toHaveLength(2);
  });
});
