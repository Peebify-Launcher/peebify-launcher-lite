// ------------ Logging ------------
// Sends the UI's log lines to the launcher's log file so problems can be found later. Batches them,
// retries if the backend is busy, and also catches uncaught errors.
import { invoke } from "@tauri-apps/api/core";
import { isTauri } from "./tauri";

type Level = "debug" | "info" | "warn" | "error";

interface LogLine {
  level: Level;
  message: string;
  t: number;
  w?: string;
}

const MAX_QUEUE = 500;
const BASE_DELAY_MS = 300;
const MAX_DELAY_MS = 5000;

const queue: LogLine[] = [];
let flushing = false;
let flushTimer: ReturnType<typeof setTimeout> | null = null;
let retryDelay = BASE_DELAY_MS;
let dropped = 0;
let source: string | undefined;

function fmt(parts: unknown[]): string {
  return parts
    .map((p) => {
      if (p instanceof Error) return p.stack || p.message || String(p);
      if (typeof p === "object" && p !== null) {
        try {
          return JSON.stringify(p);
        } catch {
          return String(p);
        }
      }
      return String(p);
    })
    .join(" ");
}

function trimQueue(): void {
  const excess = queue.length - MAX_QUEUE;
  if (excess <= 0) return;
  queue.splice(0, excess);
  dropped += excess;
}

function scheduleFlush(delay = BASE_DELAY_MS): void {
  if (flushTimer) return;
  flushTimer = setTimeout(() => {
    flushTimer = null;
    void flush();
  }, delay);
}

async function flush(): Promise<void> {
  if (flushing || (queue.length === 0 && dropped === 0)) return;
  if (!isTauri()) {
    queue.length = 0;
    return;
  }
  flushing = true;
  let failed = false;
  const batch = queue.splice(0, queue.length);
  if (dropped > 0) {
    // i18n-ignore
    batch.unshift({ level: "warn", message: `dropped ${dropped} log lines`, t: Date.now(), w: source });
  }
  const droppedInBatch = dropped;
  try {
    const res = (await invoke("rpc", { channel: "log-message", args: [batch] })) as
      | { success?: boolean; error?: string }
      | undefined;
    if (res && res.success === false) throw new Error(res.error || "log-message rejected");
    dropped -= droppedInBatch;
    retryDelay = BASE_DELAY_MS;
  } catch {
    failed = true;
    queue.unshift(...(droppedInBatch > 0 ? batch.slice(1) : batch));
    trimQueue();
    retryDelay = Math.min(retryDelay * 2, MAX_DELAY_MS);
  } finally {
    flushing = false;
    if (queue.length || dropped > 0) scheduleFlush(failed ? retryDelay : BASE_DELAY_MS);
  }
}

function push(level: Level, parts: unknown[]): void {
  const message = fmt(parts);
  const c = console[level] || console.log;
  try {
    c.call(console, "[ui]", message);
  } catch {}
  if (!isTauri()) return;
  queue.push({ level, message, t: Date.now(), w: source });
  trimQueue();
  scheduleFlush(retryDelay);
}

export const log = {
  debug: (...a: unknown[]) => push("debug", a),
  info: (...a: unknown[]) => push("info", a),
  warn: (...a: unknown[]) => push("warn", a),
  error: (...a: unknown[]) => push("error", a),
  flush,
};

export function installGlobalErrorCapture(windowTag: string): void {
  source = windowTag;
  window.addEventListener("error", (e) => {
    log.error(
      "window.onerror:",
      e.message,
      `${e.filename || "?"}:${e.lineno || 0}:${e.colno || 0}`,
      e.error || "",
    );
  });
  window.addEventListener("unhandledrejection", (e) => {
    log.error("unhandledrejection:", e.reason ?? "(no reason)");
  });
  log.debug(`Global error capture installed in the ${windowTag} window.`);
}
