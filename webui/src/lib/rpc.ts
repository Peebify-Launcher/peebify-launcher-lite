// ------------ RPC Calls ------------
// The lowest level calls into the backend. rpc is the plain call. rpcAction also shows an error notification if the
// action fails (unless given no label), and rpcRead is for quiet reads. act and ok keep only a reply that reports
// success, call is for callers that show the error themselves, and onEvent listens for a backend event.
import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isTauri } from "./tauri";
import { log } from "./log";
import { t, type TKey } from "./i18n";
import { useNotificationStore } from "../store/notifications";
import type { FpsUnlockState, ModProgress, OverlayStatus, QueueJob, RawVersions, RemoteMedia } from "./ipc";

export type Reply = { success?: boolean; error?: string; cancelled?: boolean };

export async function rpc<T = unknown>(channel: string, ...args: unknown[]): Promise<T> {
  if (!isTauri()) return undefined as T;
  return tauriInvoke<T>("rpc", { channel, args });
}

export const errorText = (detail: unknown): string | undefined =>
  detail instanceof Error ? detail.message : typeof detail === "string" && detail ? detail : undefined;

export function surfaceFailure(label: TKey | null, channel: string, detail: unknown, key?: string): void {
  log.warn(`[action] ${label ?? channel}:`, detail ?? "(no detail)");
  if (!label) return;
  const text = errorText(detail) ?? t("common.genericError");
  useNotificationStore.getState().push({ type: "error", title: t(label), text, key });
}

export async function attempt<T>(channel: string, args: unknown[], fail: (detail: unknown) => void) {
  try {
    const res = await rpc<T>(channel, ...args);
    const r = res as Reply | undefined;
    if (r && typeof r === "object" && r.success === false && !r.cancelled) fail(r.error);
    return res;
  } catch (e) {
    fail(e);
    return undefined;
  }
}

export async function rpcAction<T = unknown>(
  label: TKey | null,
  channel: string,
  ...args: unknown[]
): Promise<T | undefined> {
  if (!isTauri()) return undefined;
  return attempt<T>(channel, args, (detail) => surfaceFailure(label, channel, detail));
}

export async function rpcRead<T = unknown>(channel: string, ...args: unknown[]): Promise<T | undefined> {
  if (!isTauri()) return undefined;
  try {
    const res = await rpc<T>(channel, ...args);
    if (res && typeof res === "object" && "success" in res) {
      const r = res as Reply;
      if (!r.success) {
        if (!r.cancelled) log.warn(`[read] ${channel} failed:`, r.error ?? "(no detail)");
        return undefined;
      }
    }
    return res;
  } catch (e) {
    log.warn(`[read] ${channel} failed:`, e ?? "(no detail)");
    return undefined;
  }
}

export async function act<T = object>(label: TKey | null, channel: string, ...args: unknown[]) {
  const r = await rpcAction<T & Reply>(label, channel, ...args);
  return r?.success ? r : undefined;
}

export async function ok(label: TKey | null, channel: string, ...args: unknown[]): Promise<boolean> {
  return (await act(label, channel, ...args)) !== undefined;
}

type Called<T> = ({ ok: true } & T & Reply) | ({ ok: false; error: string } & Partial<T>);

export async function call<T extends object = object>(
  channel: string,
  errKey: TKey,
  ...args: unknown[]
): Promise<Called<T>> {
  const r = await rpc<(T & Reply) | undefined>(channel, ...args);
  return r?.success ? { ...r, ok: true } : { ...r, ok: false, error: r?.error || t(errKey) };
}

// ------------ Events ------------
// What each backend event carries.
export interface Events {
  "game-started": { gameId?: string } | null;
  "game-stopped": { gameId?: string } | null;
  "game-launch-failed": { gameId?: string; reason?: string } | null;
  "game-launch-waiting": { gameId?: string; reason?: string } | null;
  "custom-launcher-missing": { gameId?: string; path?: string } | null;
  "update-available": ({ gameId?: string } & RawVersions) | null;
  "installation-complete": { gameId?: string; version?: string | null } | null;
  "games-detected": { gameIds?: unknown } | null;
  "fps-unlock-status": { gameId?: string; state?: FpsUnlockState; error?: string | null } | null;
  "settings-changed": { key?: unknown } | null;
  "mods-load-failed": { gameId?: string; error?: string; message?: string } | null;
  "mods-notice": { gameId?: string; kind?: string; message?: string } | null;
  "mods-progress": Partial<ModProgress> | null;
  "mods-status-changed": { gameId?: string } | null;
  "mods-tools-updating": { gameId?: string } | null;
  "mod-ini-changed": { gameId?: string } | null;
  "wallpaper-media-updated": { media?: Record<string, RemoteMedia> } | null;
  "download-queue-state": { jobs?: QueueJob[] } | null;
  "network-status-changed": { isOnline?: boolean } | null;
  "renderer-power-save": boolean;
  "window-restored": unknown;
  "main-window-revealed": unknown;
  "window:maximized-changed": { maximized?: boolean } | null;
  "ui-scale-transition": { phase: "pin" | "start" | "end" | "fade-out" | "fade-in"; ratio: number; ms: number };
  "overlay-opened": unknown;
  "overlay-closed": unknown;
  "overlay-status": OverlayStatus | null;
  "overlay-hotkeys-registered": unknown;
  "overlay-fullscreen-blocked": unknown;
  "overlay-capture": { path?: string } | null;
  "overlay-capture-deleted": unknown;
  "overlay-record-starting": unknown;
  "overlay-record-started": { audioWarnings?: string[] } | null;
  "overlay-record-stopped": { path?: string; durationMs?: number; reason?: string | null } | null;
  "overlay-record-failed": { error?: string } | null;
  "overlay-record-cancelled": unknown;
}

export async function onEvent<K extends keyof Events>(
  name: K,
  cb: (payload: Events[K]) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  return listen<Events[K]>(name, (e) => cb(e.payload));
}

export function subscriptions() {
  let stopped = false;
  const offs: Array<() => void> = [];
  return {
    keep: (off: () => void) => {
      if (stopped) off();
      else offs.push(off);
    },
    stop: () => {
      stopped = true;
      offs.forEach((off) => off());
    },
    get stopped() {
      return stopped;
    },
  };
}
