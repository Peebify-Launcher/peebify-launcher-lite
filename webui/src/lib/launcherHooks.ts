// ------------ Launcher Hooks ------------
// Hooks only the launcher window uses: window focus, renderer suspended, async loading and the remembered reads
// pages open with. Kept apart from hooks.ts, which the overlay loads up front, so the overlay does not load these too.
import { useEffect, useState, useSyncExternalStore, type DependencyList } from "react";
import { isTauri } from "./tauri";
import { useTauriEvent } from "./hooks";
import {
  getAppliedVoicePacks,
  getContentPacks,
  getFpsUnlock,
  getGameConfig,
  getInstallPathHealth,
  getLoaderSettings,
  getModsStatus,
  getOverlayConfig,
  getOverlayStatus,
  getRendererPowerSave,
  getResourceQuality,
  getSteamInstall,
  onRendererPowerSave,
  onWindowRestored,
} from "./ipc";
import { subscriptions } from "./rpc";

// ------------ Window Focus ------------
// A hook that says whether the launcher window currently has focus.

async function onFocusChanged(cb: (focused: boolean) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow().onFocusChanged(({ payload }) => cb(payload));
}

export function useWindowFocused(): boolean {
  const [focused, setFocused] = useState(() => typeof document === "undefined" || document.hasFocus());
  useTauriEvent(onFocusChanged, setFocused);
  return focused;
}

// ------------ Renderer Suspended ------------
// Tells components when the launcher window is hidden or in power saving mode, so they can stop
// animations and timers nobody can see.
let suspended = false;
const listeners = new Set<() => void>();
let stop: (() => void) | null = null;

function publish(s: boolean) {
  if (s === suspended) return;
  suspended = s;
  listeners.forEach((l) => l());
}

function start(): () => void {
  const subs = subscriptions();
  let heard = false;
  const apply = (s: boolean) => {
    heard = true;
    publish(s);
  };
  void Promise.all([
    onRendererPowerSave(apply).then(subs.keep),
    onWindowRestored(() => apply(false)).then(subs.keep),
  ])
    .then(getRendererPowerSave)
    .then((s) => {
      if (!subs.stopped && !heard) publish(s);
    });
  return subs.stop;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  stop ??= start();
  return () => {
    listeners.delete(listener);
    if (listeners.size > 0 || !stop) return;
    stop();
    stop = null;
    suspended = false;
  };
}

const getSnapshot = () => suspended;

export function useRendererSuspended(): boolean {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

// ------------ Async Load Hook ------------
// Loads a value when the dependencies change and drops answers that arrive after they changed again. Gives null
// until the answer for the current dependencies is in, or when `load` returns null because there is nothing to load.
const sameDeps = (a: DependencyList, b: DependencyList) =>
  a.length === b.length && a.every((v, i) => Object.is(v, b[i]));

export function useLoad<T>(load: () => Promise<T> | null, deps: DependencyList): T | null {
  const [loaded, setLoaded] = useState<{ deps: DependencyList; value: T } | null>(null);

  useEffect(() => {
    const pending = load();
    if (!pending) return;
    let alive = true;
    void pending.then((value) => {
      if (alive) setLoaded({ deps, value });
    });
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return loaded && sameDeps(loaded.deps, deps) ? loaded.value : null;
}

// ------------ Remembered Reads ------------
// Reads a page makes when it opens, with their last answer kept for the session under a key. A page starts from the
// remembered answer and still asks again, so it opens at its final size and quietly takes the fresh answer. The page
// preloader (pagePreload.ts) fills them in before the first visit.
export interface Remembered<T> {
  key: string;
  load: () => Promise<T>;
}

const answers = new Map<string, unknown>();
const latestRead = new Map<string, number>();
const warming = new Set<string>();
let readSeq = 0;

export function fetchRemembered<T>(read: Remembered<T>): Promise<T> {
  const seq = ++readSeq;
  latestRead.set(read.key, seq);
  return read.load().then((value) => {
    if (latestRead.get(read.key) === seq) answers.set(read.key, value);
    return value;
  });
}

export function recall<T>(read: Remembered<T>): T | null {
  return (answers.get(read.key) as T | undefined) ?? null;
}

export function rememberAnswer<T>(read: Remembered<T>, value: T): void {
  answers.set(read.key, value);
}

export async function warmRemembered(read: Remembered<unknown>): Promise<void> {
  if (answers.has(read.key) || warming.has(read.key)) return;
  warming.add(read.key);
  try {
    await fetchRemembered(read);
  } finally {
    warming.delete(read.key);
  }
}

export function useRemembered<T>(read: Remembered<T> | null, deps: DependencyList): T | null {
  const fresh = useLoad(() => (read ? fetchRemembered(read) : null), deps);
  return fresh ?? (read ? recall(read) : null);
}

export const reads = {
  steamInstall: (gameId: string) => ({ key: `steam-install:${gameId}`, load: () => getSteamInstall(gameId) }),
  installPathHealth: (gameId: string) => ({
    key: `install-path-health:${gameId}`,
    load: () => getInstallPathHealth(gameId),
  }),
  appliedVoicePacks: (gameId: string) => ({
    key: `applied-voice-packs:${gameId}`,
    load: () => getAppliedVoicePacks(gameId),
  }),
  resourceQuality: (gameId: string, withSizes: boolean) => ({
    key: `resource-quality:${gameId}:${withSizes}`,
    load: () => getResourceQuality(gameId, withSizes),
  }),
  gameConfig: (gameId: string) => ({ key: `game-config:${gameId}`, load: () => getGameConfig(gameId) }),
  contentPacks: (gameId: string) => ({ key: `content-packs:${gameId}`, load: () => getContentPacks(gameId) }),
  fpsUnlock: (gameId: string) => ({ key: `fps-unlock:${gameId}`, load: () => getFpsUnlock(gameId) }),
  modsStatus: (gameId: string) => ({ key: `mods-status:${gameId}`, load: () => getModsStatus(gameId) }),
  overlayConfig: { key: "overlay-config", load: getOverlayConfig },
  overlayStatus: { key: "overlay-status", load: getOverlayStatus },
  overlayLoaderSettings: { key: "overlay-loader-settings", load: () => getLoaderSettings() },
} satisfies Record<string, Remembered<unknown> | ((...args: never[]) => Remembered<unknown>)>;
