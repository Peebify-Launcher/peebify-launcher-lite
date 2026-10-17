// ------------ Toast Layer ------------
// The small pop-up messages that appear for a few seconds when something happens. They pause while hovered or
// focused, and errors stay a bit longer. Clicking one opens the notification panel. Each one buds out of the
// notification bell and goes back into it, so you can see where it went.
import { useEffect, useRef, useState, type FocusEvent } from "react";
import { AnimatePresence, m } from "framer-motion";
import { X } from "lucide-react";
import { springDrip, springPour, useReducedMotionSafe } from "../../lib/motion";
import { notificationBellRef } from "./WindowControls";
import {
  useNotificationStore,
  type Notification,
  NOTIF_ICONS,
  NOTIF_ICON_COLORS,
} from "../../store/notifications";
import { useT } from "../../lib/useT";

const TOAST_MS = 5000;
const ERROR_TOAST_MS = 8000;
const MIN_RESUME_MS = 700;

type OpenHandler = (id: string) => void;

const openPanel: OpenHandler = () => useNotificationStore.getState().setOpen(true);

function bellOffset(): { x: number; y: number; scale: number; opacity: number } | null {
  const bell = notificationBellRef.current?.getBoundingClientRect();
  if (!bell || bell.width === 0) return null;
  return {
    x: bell.left + bell.width / 2 - (window.innerWidth - COLUMN_RIGHT),
    y: bell.top + bell.height / 2 - COLUMN_TOP,
    scale: 0.12,
    opacity: 0,
  };
}

const COLUMN_RIGHT = 14;
const COLUMN_TOP = 56;
const SLIDE_IN = { opacity: 0, x: 24 };

function Toast({ n, onOpen }: { n: Notification; onOpen: OpenHandler }) {
  const t = useT();
  const reduce = useReducedMotionSafe();
  const [from] = useState(() => (reduce ? null : bellOffset()));
  const duration = n.type === "error" ? ERROR_TOAST_MS : TOAST_MS;
  const timer = useRef<number | undefined>(undefined);
  const remaining = useRef(duration);
  const armedAt = useRef(0);
  const hovered = useRef(false);
  const focused = useRef(false);

  const arm = (ms: number) => {
    armedAt.current = Date.now();
    timer.current = window.setTimeout(
      () => useNotificationStore.getState().dismissToast(n.id),
      ms,
    );
  };

  useEffect(() => {
    armedAt.current = Date.now();
    timer.current = window.setTimeout(
      () => useNotificationStore.getState().dismissToast(n.id),
      duration,
    );
    return () => window.clearTimeout(timer.current);
  }, [n.id, duration]);

  const pause = () => {
    window.clearTimeout(timer.current);
    remaining.current = Math.max(0, remaining.current - (Date.now() - armedAt.current));
  };
  const resume = () => arm(Math.max(remaining.current, MIN_RESUME_MS));

  const hold = (flag: { current: boolean }, on: boolean) => {
    const wasHeld = hovered.current || focused.current;
    flag.current = on;
    const isHeld = hovered.current || focused.current;
    if (!wasHeld && isHeld) pause();
    else if (wasHeld && !isHeld) resume();
  };

  const onBlur = (e: FocusEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) hold(focused, false);
  };

  const Icon = NOTIF_ICONS[n.type];
  const c = NOTIF_ICON_COLORS[n.type];

  return (
    <m.div
      layout
      style={{ transformOrigin: "100% 0%" }}
      initial={from ?? SLIDE_IN}
      animate={{
        opacity: 1,
        x: 0,
        y: 0,
        scale: 1,
        transition: from
          ? { default: springDrip, opacity: { duration: 0.14 } }
          : { duration: 0.2, ease: "easeOut" },
      }}
      exit={{
        ...(from ?? SLIDE_IN),
        transition: from
          ? { default: springPour, opacity: { duration: 0.18, delay: 0.16 } }
          : { duration: 0.18 },
      }}
      onMouseEnter={() => hold(hovered, true)}
      onMouseLeave={() => hold(hovered, false)}
      onFocus={() => hold(focused, true)}
      onBlur={onBlur}
      className="glass pointer-events-auto flex w-[330px] gap-[10px] rounded-ui px-[12px] py-[11px] shadow-2xl"
    >
      <span
        className="mt-[1px] grid h-[26px] w-[26px] shrink-0 place-items-center rounded-md"
        style={{ background: c.bg, color: c.fg }}
      >
        <Icon size={14} />
      </span>
      <div className="min-w-0 flex-1">
        <button
          onClick={() => onOpen(n.id)}
          className="block w-full text-left"
        >
          <div className="text-[13px] font-medium leading-tight">{n.title}</div>
          <div className="mt-[2px] break-words text-[12px] leading-snug text-white/65">{n.text}</div>
        </button>
        {n.action && (
          <button
            onClick={() => {
              useNotificationStore.getState().dismissToast(n.id);
              n.action?.run();
            }}
            className="mt-[8px] rounded-md bg-white/10 px-[10px] py-[4px] text-[12px] font-medium text-white transition-colors hover:bg-white/[0.16]"
          >
            {n.action.label}
          </button>
        )}
      </div>
      <button
        onClick={() => useNotificationStore.getState().dismissToast(n.id)}
        aria-label={t("common.dismiss")}
        className="grid h-[22px] w-[22px] shrink-0 place-items-center self-start rounded-md text-white/45 transition-colors hover:bg-white/10 hover:text-white"
      >
        <X size={12} />
      </button>
    </m.div>
  );
}

export function ToastLayer({ onOpen = openPanel }: { onOpen?: OpenHandler }) {
  const toasts = useNotificationStore((s) => s.toasts);
  const errors = toasts.filter((n) => n.type === "error");
  const others = toasts.filter((n) => n.type !== "error");
  return (
    <div className="pointer-events-none absolute right-[14px] top-[56px] z-(--z-toast) flex flex-col items-end">
      <div role="alert" className="flex flex-col items-end gap-2 [&:not(:empty)]:mb-2">
        <AnimatePresence initial={false}>
          {errors.map((n) => (
            <Toast key={n.id} n={n} onOpen={onOpen} />
          ))}
        </AnimatePresence>
      </div>
      <div aria-live="polite" className="flex flex-col items-end gap-2">
        <AnimatePresence initial={false}>
          {others.map((n) => (
            <Toast key={n.id} n={n} onOpen={onOpen} />
          ))}
        </AnimatePresence>
      </div>
    </div>
  );
}
