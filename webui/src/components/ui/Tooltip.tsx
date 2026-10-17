// ------------ Tooltip ------------
// The launcher's own hover text. Hint wraps anything to give it a tooltip, IconButton is a button with one, and the
// portal draws it above everything else and keeps it inside the window. Shows on keyboard focus as well as hover.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ButtonHTMLAttributes, ReactNode, RefObject, SyntheticEvent } from "react";
import { createPortal } from "react-dom";

const TIP_CLAIM = "peebify:tooltip-claim";
const EDGE = 8;
const GAP = 12;

type Placement = "right" | "left" | "bottom" | "cursor";

export const WRAPPED_TIP = "block max-w-[360px] whitespace-normal";

const BUD_ORIGIN: Record<Placement, string> = {
  right: "origin-left",
  left: "origin-right",
  bottom: "origin-top",
  cursor: "origin-top-left",
};

export function TooltipPortal({
  x,
  y,
  placement = "right",
  children,
}: {
  x: number;
  y: number;
  placement?: Placement;
  children: ReactNode;
}) {
  const boxRef = useRef<HTMLSpanElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  useLayoutEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    const w = box.offsetWidth;
    const h = box.offsetHeight;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    let left: number;
    let top: number;
    if (placement === "right") {
      left = x + GAP;
      top = y - h / 2;
    } else if (placement === "left") {
      left = x - GAP - w;
      top = y - h / 2;
    } else if (placement === "bottom") {
      left = x - w / 2;
      top = y + GAP;
    } else {
      left = x + GAP;
      top = y + GAP;
      if (left + w > vw - EDGE) left = x - GAP - w;
      if (top + h > vh - EDGE) top = y - GAP - h;
    }
    left = Math.min(Math.max(left, EDGE), vw - EDGE - w);
    top = Math.min(Math.max(top, EDGE), vh - EDGE - h);
    setPos({ left, top });
  }, [x, y, placement, children]);

  return createPortal(
    <span
      ref={boxRef}
      role="tooltip"
      className={`tooltip-bud pointer-events-none fixed z-(--z-tooltip) whitespace-nowrap rounded-[7px] border border-white/10 bg-[rgba(20,20,26,0.95)] px-[10px] py-[6px] text-[12px] font-medium text-white shadow-lg ${BUD_ORIGIN[placement]}`}
      style={pos ? { left: pos.left, top: pos.top } : { left: -9999, top: -9999 }}
    >
      {children}
    </span>,
    document.body,
  );
}

export function Hint({
  tip,
  placement = "bottom",
  className = "inline-flex",
  children,
}: {
  tip?: ReactNode;
  placement?: Exclude<Placement, "cursor">;
  className?: string;
  children: ReactNode;
}) {
  const hint = useAnchoredTip<HTMLSpanElement>(placement);
  return (
    <span ref={hint.anchorRef} {...hint.bind} className={className}>
      {children}
      {tip && hint.shown && (
        <TooltipPortal x={hint.pos.x} y={hint.pos.y} placement={placement}>
          {tip}
        </TooltipPortal>
      )}
    </span>
  );
}

type TipHandlers = ReturnType<typeof useAnchoredTip>["bind"];

export function IconButton({
  label,
  tip = label,
  placement = "bottom",
  buttonRef,
  children,
  ...props
}: {
  label?: string;
  tip?: ReactNode;
  placement?: Exclude<Placement, "cursor">;
  buttonRef?: RefObject<HTMLButtonElement | null>;
} & ButtonHTMLAttributes<HTMLButtonElement>) {
  const hint = useAnchoredTip<HTMLButtonElement>(placement);
  const handlers = Object.fromEntries(
    Object.entries(hint.bind).map(([name, own]) => [
      name,
      (e: SyntheticEvent) => {
        (own as (e: SyntheticEvent) => void)(e);
        (props[name as keyof TipHandlers] as ((e: SyntheticEvent) => void) | undefined)?.(e);
      },
    ]),
  );
  return (
    <button
      {...props}
      {...handlers}
      aria-label={props["aria-label"] ?? label}
      ref={(el) => {
        hint.anchorRef.current = el;
        if (buttonRef) buttonRef.current = el;
      }}
    >
      {children}
      {tip && hint.shown && (
        <TooltipPortal x={hint.pos.x} y={hint.pos.y} placement={placement}>
          {tip}
        </TooltipPortal>
      )}
    </button>
  );
}

function useAnchoredTip<T extends HTMLElement>(placement: Exclude<Placement, "cursor"> = "right") {
  const anchorRef = useRef<T>(null);
  const pointer = useRef({ x: 0, y: 0 });
  const viaFocus = useRef(false);
  const [shown, setShown] = useState(false);
  const [pos, setPos] = useState({ x: 0, y: 0 });

  const place = useCallback(
    (rect: DOMRect) => {
      if (placement === "right") setPos({ x: rect.right, y: rect.top + rect.height / 2 });
      else if (placement === "left") setPos({ x: rect.left, y: rect.top + rect.height / 2 });
      else setPos({ x: rect.left + rect.width / 2, y: rect.bottom });
    },
    [placement],
  );

  const show = useCallback(
    (e: { clientX: number; clientY: number }) => {
      const rect = anchorRef.current?.getBoundingClientRect();
      if (!rect) return;
      pointer.current = { x: e.clientX, y: e.clientY };
      viaFocus.current = false;
      window.dispatchEvent(new CustomEvent(TIP_CLAIM, { detail: anchorRef.current }));
      place(rect);
      setShown(true);
    },
    [place],
  );

  const showFocus = useCallback(
    (e: { target: EventTarget }) => {
      if (!(e.target instanceof Element) || !e.target.matches(":focus-visible")) return;
      const rect = anchorRef.current?.getBoundingClientRect();
      if (!rect) return;
      viaFocus.current = true;
      window.dispatchEvent(new CustomEvent(TIP_CLAIM, { detail: anchorRef.current }));
      place(rect);
      setShown(true);
    },
    [place],
  );

  useEffect(() => {
    if (!shown) return;
    const track = () => {
      const rect = anchorRef.current?.getBoundingClientRect();
      const { x, y } = pointer.current;
      if (!rect || (!viaFocus.current && (x < rect.left || x > rect.right || y < rect.top || y > rect.bottom))) {
        setShown(false);
        return;
      }
      place(rect);
    };
    const yieldTip = (e: Event) => {
      if ((e as CustomEvent).detail !== anchorRef.current) setShown(false);
    };
    window.addEventListener("scroll", track, true);
    window.addEventListener("resize", track);
    window.addEventListener(TIP_CLAIM, yieldTip);
    return () => {
      window.removeEventListener("scroll", track, true);
      window.removeEventListener("resize", track);
      window.removeEventListener(TIP_CLAIM, yieldTip);
    };
  }, [shown, place]);

  return {
    anchorRef,
    shown,
    pos,
    placement,
    bind: {
      onPointerEnter: show,
      onPointerMove: (e: { clientX: number; clientY: number }) => {
        pointer.current = { x: e.clientX, y: e.clientY };
      },
      onPointerLeave: () => setShown(false),
      onPointerDown: () => setShown(false),
      onFocus: showFocus,
      onBlur: () => setShown(false),
    },
  };
}
