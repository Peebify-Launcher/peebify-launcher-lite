// ------------ Shared Hooks ------------
// Small React hooks the launcher window and the overlay both use: backend events, closing popups on an outside
// click or Escape, and the shared "5m ago" clock. Hooks only the launcher window uses are in launcherHooks.ts.
import { useCallback, useEffect, useRef, useSyncExternalStore, type RefObject } from "react";
import { onEvent, subscriptions, type Events } from "./rpc";

// ------------ Backend Event Hooks ------------
// Subscribe a component to backend events and clean up when it goes away. The handlers always see the latest
// props and state, so changing them doesn't resubscribe.

export function useTauriEvent<A extends unknown[]>(
  subscribe: (cb: (...args: A) => void) => Promise<() => void>,
  handler: (...args: A) => void,
): void {
  const handlerRef = useRef(handler);

  useEffect(() => {
    handlerRef.current = handler;
  });

  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    void subscribe((...args) => handlerRef.current(...args)).then((f) => {
      if (cancelled) f();
      else un = f;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, [subscribe]);
}

type Handlers = { [K in keyof Events]?: (payload: Events[K]) => void };

export function useTauriEvents(handlers: Handlers): void {
  const handlersRef = useRef(handlers);
  const names = Object.keys(handlers).join(" ");

  useEffect(() => {
    handlersRef.current = handlers;
  });

  useEffect(() => {
    const subs = subscriptions();
    for (const name of names.split(" ") as (keyof Events)[]) {
      const handle = (payload: unknown) => (handlersRef.current[name] as ((p: unknown) => void) | undefined)?.(payload);
      void onEvent(name, handle).then(subs.keep);
    }
    return subs.stop;
  }, [names]);
}

// ------------ Click Outside and Escape ------------
// Hooks for closing popups when you click elsewhere, press Escape, scroll or resize the window. When popups are
// stacked, Escape only closes the top one.
type ElementRef = RefObject<HTMLElement | null>;

export function useClickOutside(
  ref: ElementRef | ElementRef[],
  onOutside: () => void,
  active = true,
  ignore?: (target: HTMLElement) => boolean,
): void {
  const latest = useRef({ ref, onOutside, ignore });
  latest.current = { ref, onOutside, ignore };

  useEffect(() => {
    if (!active) return;
    const handler = (e: MouseEvent) => {
      const { ref: current, onOutside: cb, ignore: skip } = latest.current;
      const refs = Array.isArray(current) ? current : [current];
      const target = e.target as HTMLElement;
      if (refs.some((r) => r.current?.contains(target))) return;
      if (skip?.(target)) return;
      cb();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [active]);
}

type EscapeLayer = { current: (e: KeyboardEvent) => void };

const escapeLayers: EscapeLayer[] = [];

function onEscapeKey(e: KeyboardEvent) {
  if (e.key !== "Escape") return;
  const top = escapeLayers[escapeLayers.length - 1];
  if (top) top.current(e);
}

export function useEscapeKey(onEscape: (e: KeyboardEvent) => void, active = true): () => boolean {
  const latest = useRef(onEscape);
  latest.current = onEscape;

  useEffect(() => {
    if (!active) return;
    const layer = latest;
    escapeLayers.push(layer);
    if (escapeLayers.length === 1) window.addEventListener("keydown", onEscapeKey);
    return () => {
      const at = escapeLayers.lastIndexOf(layer);
      if (at >= 0) escapeLayers.splice(at, 1);
      if (escapeLayers.length === 0) window.removeEventListener("keydown", onEscapeKey);
    };
  }, [active]);

  return useCallback(() => escapeLayers[escapeLayers.length - 1] === latest, []);
}

export function useDismissOnViewportChange(ref: ElementRef, dismiss: () => void, active: boolean): void {
  const latest = useRef(dismiss);
  latest.current = dismiss;

  useEffect(() => {
    if (!active) return;
    const onScroll = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) latest.current();
    };
    const onResize = () => latest.current();
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onResize);
    };
  }, [active, ref]);
}

// ------------ Shared Clock ------------
// One shared timer that ticks every 30 seconds so every "5m ago" label on screen updates together.
// It pauses while the window is hidden.
const TICK_MS = 30_000;
const listeners = new Set<() => void>();
let now = Date.now();
let timer: number | undefined;
let suspended = false;

function tick(): void {
  now = Date.now();
  listeners.forEach((l) => l());
}

function startTicker(): void {
  if (timer !== undefined || suspended || listeners.size === 0) return;
  timer = window.setInterval(tick, TICK_MS);
}

function stopTicker(): void {
  if (timer === undefined) return;
  window.clearInterval(timer);
  timer = undefined;
}

export function setNowTickerSuspended(value: boolean): void {
  if (suspended === value) return;
  suspended = value;
  if (value) {
    stopTicker();
    return;
  }
  if (listeners.size > 0) tick();
  startTicker();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  if (listeners.size === 1) {
    now = Date.now();
    startTicker();
  }
  return () => {
    listeners.delete(cb);
    if (listeners.size === 0) stopTicker();
  };
}

export function useNow(): number {
  return useSyncExternalStore(subscribe, () => now);
}
