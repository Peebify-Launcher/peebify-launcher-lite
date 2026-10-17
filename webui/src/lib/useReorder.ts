// ------------ Drag Reorder ------------
// Drag to reorder a vertical list (the game list in the sidebar and in settings), with edge scrolling and
// a keyboard option using Alt and the arrow keys.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useReducedMotionSafe } from "./motion";
import type { TKey } from "./i18n";

const DRAG_THRESHOLD_PX = 4;
const EDGE_SCROLL_ZONE_PX = 46;
const EDGE_SCROLL_SPEED_PX = 12;
const SETTLE_MS = 170;
const SHIFT_MS = 180;
const SWAP_MS = 260;
const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
const LIFT_SCALE = 1.02;

export const REORDER_KEY_HINT: TKey = "games.reorderHint";

type Phase = "drag" | "settle" | "land";

interface Slot {
  top: number;
  bottom: number;
}

interface Gesture<T> {
  id: T;
  startY: number;
  moved: boolean;
  from: number;
  previewedTo: number;
  slots: Slot[];
  step: number;
  scroller: HTMLElement | null;
  scrollTop: number;
}

interface DragState<T> {
  id: T;
  from: number;
  to: number;
  offset: number;
  step: number;
  phase: Phase;
}

interface ReorderItemProps {
  ref: (el: HTMLElement | null) => void;
  onPointerDown: (e: React.PointerEvent) => void;
  onClickCapture: (e: React.MouseEvent) => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  style: React.CSSProperties;
}

interface ReorderAnnouncement<T> {
  id: T;
  position: number;
  total: number;
}

interface Reorder<T extends string> {
  dragId: T | null;
  announcement: ReorderAnnouncement<T> | null;
  containerRef: (el: HTMLElement | null) => void;
  itemProps: (id: T) => ReorderItemProps;
}

function shiftFor<T extends string>(state: DragState<T>, index: number): number {
  if (state.to > state.from) return index > state.from && index <= state.to ? -state.step : 0;
  if (state.to < state.from) return index >= state.to && index < state.from ? state.step : 0;
  return 0;
}

function translatedY(el: HTMLElement): number {
  const value = getComputedStyle(el).transform;
  if (!value || value === "none") return 0;
  const parts = value.slice(value.indexOf("(") + 1, -1).split(",");
  const y = Number(parts[value.startsWith("matrix3d") ? 13 : 5]);
  return Number.isFinite(y) ? y : 0;
}

function scrollerFor(el: HTMLElement | null): HTMLElement | null {
  for (let node = el; node; node = node.parentElement) {
    if (node.scrollHeight <= node.clientHeight + 1) continue;
    const overflow = getComputedStyle(node).overflowY;
    if (overflow === "auto" || overflow === "scroll" || overflow === "overlay") return node;
  }
  return null;
}

export function useReorder<T extends string>(
  ids: T[],
  onReorder: (id: T, toIndex: number) => void,
  onPreview?: (order: T[] | null) => void,
): Reorder<T> {
  const elements = useRef(new Map<T, HTMLElement>());
  const container = useRef<HTMLElement | null>(null);
  const gesture = useRef<Gesture<T> | null>(null);
  const suppressClick = useRef(false);
  const settleTimer = useRef<number | null>(null);
  const refocusId = useRef<T | null>(null);
  const [announcement, setAnnouncement] = useState<ReorderAnnouncement<T> | null>(null);
  const latestIds = useRef(ids);
  latestIds.current = ids;

  const reduceMotion = useReducedMotionSafe();
  const reduceMotionRef = useRef(reduceMotion);
  reduceMotionRef.current = reduceMotion;

  const onReorderRef = useRef(onReorder);
  onReorderRef.current = onReorder;

  const onPreviewRef = useRef(onPreview);
  onPreviewRef.current = onPreview;

  const clearPreview = useCallback(() => {
    onPreviewRef.current?.(null);
  }, []);

  const publishPreview = useCallback(
    (id: T, from: number, to: number) => {
      if (!onPreviewRef.current) return;
      if (from === to) {
        clearPreview();
        return;
      }
      const order = [...latestIds.current];
      order.splice(from, 1);
      order.splice(to, 0, id);
      onPreviewRef.current(order);
    },
    [clearPreview],
  );

  const [drag, setDrag] = useState<DragState<T> | null>(null);
  const dragRef = useRef<DragState<T> | null>(null);

  const apply = useCallback((next: DragState<T> | null) => {
    dragRef.current = next;
    setDrag(next);
  }, []);

  const swapTimer = useRef<number | null>(null);
  const swapping = useRef<HTMLElement[]>([]);
  const prevIds = useRef<T[]>(ids);

  const cancelSwap = useCallback(() => {
    if (swapTimer.current !== null) {
      clearTimeout(swapTimer.current);
      swapTimer.current = null;
    }
    for (const el of swapping.current) {
      el.style.transition = "";
      el.style.transform = "";
    }
    swapping.current = [];
  }, []);

  useLayoutEffect(() => {
    const before = prevIds.current;
    const after = latestIds.current;
    prevIds.current = after;
    const same = before.length === after.length && before.every((id, i) => id === after[i]);
    const refocus = refocusId.current;
    refocusId.current = null;
    if (!same && refocus !== null) {
      const el = elements.current.get(refocus);
      const target = el?.querySelector<HTMLElement>("button, [tabindex]") ?? el;
      target?.focus({ preventScroll: true });
      el?.scrollIntoView({ block: "nearest" });
    }
    if (dragRef.current || reduceMotionRef.current) return;
    if (same) return;

    const rows = after.map((id) => elements.current.get(id));
    let step = 0;
    for (let i = 1; i < rows.length && step === 0; i++) {
      const a = rows[i - 1];
      const b = rows[i];
      if (a && b) step = b.offsetTop - a.offsetTop;
    }
    if (step === 0) return;

    const moving: [HTMLElement, number][] = [];
    for (let i = 0; i < after.length; i++) {
      const el = rows[i];
      const was = before.indexOf(after[i]);
      if (!el || was < 0 || was === i) continue;
      moving.push([el, (was - i) * step + translatedY(el)]);
    }
    if (moving.length === 0) return;

    cancelSwap();
    swapping.current = moving.map(([el]) => el);
    for (const [el, offset] of moving) {
      el.style.transition = "none";
      el.style.transform = `translate3d(0, ${offset}px, 0)`;
    }
    requestAnimationFrame(() => {
      for (const el of swapping.current) {
        el.style.transition = `transform ${SWAP_MS}ms ${EASE}`;
        el.style.transform = "";
      }
    });
    swapTimer.current = window.setTimeout(cancelSwap, SWAP_MS + 60);
  });

  const measure = useCallback((active: Gesture<T>) => {
    cancelSwap();
    const list = latestIds.current;
    const from = list.indexOf(active.id);
    if (from < 0 || list.length < 2) return false;
    const slots: Slot[] = [];
    for (const id of list) {
      const el = elements.current.get(id);
      if (!el) return false;
      const rect = el.getBoundingClientRect();
      slots.push({ top: rect.top, bottom: rect.bottom });
    }
    active.from = from;
    active.slots = slots;
    active.step =
      from < slots.length - 1 ? slots[from + 1].top - slots[from].top : slots[from].top - slots[from - 1].top;
    active.scroller = scrollerFor(container.current);
    active.scrollTop = active.scroller?.scrollTop ?? 0;
    return true;
  }, [cancelSwap]);

  const edgeScroll = useCallback((active: Gesture<T>, clientY: number) => {
    const box = active.scroller;
    if (!box) return;
    const rect = box.getBoundingClientRect();
    if (clientY < rect.top + EDGE_SCROLL_ZONE_PX) {
      box.scrollTop -= EDGE_SCROLL_SPEED_PX;
    } else if (clientY > rect.bottom - EDGE_SCROLL_ZONE_PX) {
      box.scrollTop += EDGE_SCROLL_SPEED_PX;
    }
  }, []);

  const land = useCallback(() => {
    settleTimer.current = null;
    const state = dragRef.current;
    if (!state) return;
    if (state.to === state.from) {
      apply(null);
      clearPreview();
      return;
    }
    apply({ ...state, offset: 0, phase: "land" });
    onReorderRef.current(state.id, state.to);
    clearPreview();
    requestAnimationFrame(() => {
      if (dragRef.current?.phase === "land") apply(null);
    });
  }, [apply, clearPreview]);

  const finish = useCallback(() => {
    const active = gesture.current;
    gesture.current = null;
    const state = dragRef.current;
    if (!active?.moved || !state) {
      apply(null);
      clearPreview();
      return;
    }
    if (reduceMotionRef.current) {
      land();
      return;
    }
    const { slots, from } = active;
    const rest =
      state.to === from
        ? 0
        : state.to > from
          ? slots[state.to].bottom - slots[from].bottom
          : slots[state.to].top - slots[from].top;
    apply({ ...state, offset: rest, phase: "settle" });
    settleTimer.current = window.setTimeout(land, SETTLE_MS);
  }, [apply, clearPreview, land]);

  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      const active = gesture.current;
      if (!active) return;
      if (!active.moved) {
        if (Math.abs(e.clientY - active.startY) < DRAG_THRESHOLD_PX) return;
        if (!measure(active)) {
          gesture.current = null;
          return;
        }
        active.moved = true;
        suppressClick.current = true;
      }
      e.preventDefault();
      edgeScroll(active, e.clientY);

      const { slots, from } = active;
      const scrolled = (active.scroller?.scrollTop ?? 0) - active.scrollTop;
      const offset = Math.min(
        Math.max(e.clientY - active.startY + scrolled, slots[0].top - slots[from].top),
        slots[slots.length - 1].bottom - slots[from].bottom,
      );
      const stepped = active.step > 0 ? Math.round(offset / active.step) : 0;
      const to = Math.max(0, Math.min(slots.length - 1, from + stepped));
      apply({ id: active.id, from, to, offset, step: active.step, phase: "drag" });
      if (to !== active.previewedTo) {
        active.previewedTo = to;
        publishPreview(active.id, from, to);
      }
    };
    const onUp = () => {
      if (gesture.current) finish();
    };

    window.addEventListener("pointermove", onMove, { passive: false });
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [apply, edgeScroll, finish, measure, publishPreview]);

  useEffect(
    () => () => {
      if (swapTimer.current !== null) clearTimeout(swapTimer.current);
      if (settleTimer.current === null) return;
      clearTimeout(settleTimer.current);
      const state = dragRef.current;
      if (state && state.to !== state.from) onReorderRef.current(state.id, state.to);
    },
    [],
  );

  const itemProps = useCallback(
    (id: T): ReorderItemProps => {
      const state = drag;
      const held = !!state && state.id === id && state.phase !== "land";
      const style: React.CSSProperties = { touchAction: "none" };

      if (state?.phase === "land") {
        style.transform = "none";
        style.transition = "none";
      } else if (state && held) {
        style.transform = `translate3d(0, ${state.offset}px, 0) scale(${
          state.phase === "drag" ? LIFT_SCALE : 1
        })`;
        style.transition = state.phase === "drag" ? "none" : `all ${SETTLE_MS}ms ${EASE}`;
        style.position = "relative";
        style.zIndex = 30;
        style.cursor = "grabbing";
        style.willChange = "transform";
      } else if (state) {
        style.transform = `translate3d(0, ${shiftFor(state, latestIds.current.indexOf(id))}px, 0)`;
        style.transition = `transform ${SHIFT_MS}ms ${EASE}`;
        style.pointerEvents = "none";
        style.willChange = "transform";
      }

      return {
        ref: (el) => {
          if (el) elements.current.set(id, el);
          else elements.current.delete(id);
        },
        onPointerDown: (e) => {
          if (e.button !== 0 || latestIds.current.length < 2) return;
          if (settleTimer.current !== null) {
            clearTimeout(settleTimer.current);
            land();
          }
          gesture.current = {
            id,
            startY: e.clientY,
            moved: false,
            from: 0,
            previewedTo: -1,
            slots: [],
            step: 0,
            scroller: null,
            scrollTop: 0,
          };
          suppressClick.current = false;
        },
        onClickCapture: (e) => {
          if (!suppressClick.current) return;
          suppressClick.current = false;
          e.preventDefault();
          e.stopPropagation();
        },
        onKeyDown: (e) => {
          if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
          if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
          if (gesture.current || dragRef.current) return;
          const list = latestIds.current;
          const from = list.indexOf(id);
          const to = from + (e.key === "ArrowUp" ? -1 : 1);
          if (from < 0 || to < 0 || to >= list.length) return;
          e.preventDefault();
          refocusId.current = id;
          onReorderRef.current(id, to);
          setAnnouncement({ id, position: to + 1, total: list.length });
        },
        style,
      };
    },
    [drag, land],
  );

  return {
    dragId: drag && drag.phase === "drag" ? drag.id : null,
    announcement,
    containerRef: (el) => {
      container.current = el;
    },
    itemProps,
  };
}
