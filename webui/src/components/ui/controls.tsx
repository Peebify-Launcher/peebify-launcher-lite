// ------------ Controls ------------
// The small input controls the launcher and the overlay share: buttons and filter chips, the on/off switch, and the
// segmented control with its arrow key handling.
import { useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { m } from "framer-motion";
import { edgeLead, edgeTrail, useReducedMotionSafe } from "../../lib/motion";
import { Hint } from "./Tooltip";

// ------------ Action Button ------------
// The standard rounded button used across pages, in neutral, danger (red) and accent styles, with an optional
// icon. The overlay uses the smaller compact size, which also has a dashed style. Also the filter chips.
type ActionVariant = "neutral" | "danger" | "accent" | "dashed";

const VARIANTS: Record<ActionVariant, string> = {
  neutral:
    "border-white/15 bg-white/[0.05] text-white hover:border-white/25 hover:bg-white/[0.12]",
  danger:
    "border-[#ef4444]/40 bg-[#ef4444]/[0.08] text-danger-soft hover:border-[#ef4444]/60 hover:bg-[#ef4444]/[0.16]",
  accent:
    "border-(--accent-b)/45 bg-(--accent-b)/12 text-(--accent-text) hover:border-(--accent-b)/60 hover:bg-(--accent-b)/22",
  dashed: "border-dashed border-white/20 bg-transparent text-white/65 hover:text-white",
};

const COMPACT_VARIANTS: Record<ActionVariant, string> = {
  ...VARIANTS,
  neutral: "border-white/15 bg-white/[0.06] text-white hover:bg-white/10",
  danger: "border-[#ef4444]/40 bg-[#ef4444]/[0.08] text-danger-soft hover:bg-[#ef4444]/[0.14]",
};

const FULL =
  "inline-flex select-none items-center gap-2 rounded-ui text-[13px] transition duration-150 active:scale-[0.97] disabled:opacity-40";

const SIZES = {
  md: `${FULL} px-4 py-[9px]`,
  sm: `${FULL} h-[34px] px-[14px]`,
  compact:
    "shrink-0 rounded-[8px] px-3 py-[7px] text-[12px] transition-colors duration-150 disabled:border-white/10 disabled:bg-white/[0.04] disabled:text-white/30",
};

export function ActionButton({
  children,
  onClick,
  variant = "neutral",
  disabled = false,
  icon,
  size = "md",
  type = "button",
}: {
  children: ReactNode;
  onClick?: () => void;
  variant?: ActionVariant;
  disabled?: boolean;
  icon?: ReactNode;
  size?: keyof typeof SIZES;
  type?: "button" | "submit";
}) {
  const skin = (size === "compact" ? COMPACT_VARIANTS : VARIANTS)[variant];
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      className={`border font-medium disabled:cursor-not-allowed ${SIZES[size]} ${skin}`}
    >
      {icon}
      {children}
    </button>
  );
}

export function Chip({
  active,
  onClick,
  disabled = false,
  tip,
  compact = false,
  children,
}: {
  active: boolean;
  onClick: () => void;
  disabled?: boolean;
  tip?: string;
  compact?: boolean;
  children: ReactNode;
}) {
  const skin = compact
    ? `rounded-[8px] px-3 text-[12px] disabled:cursor-not-allowed disabled:opacity-30 ${
        active
          ? "border-white/[0.22] bg-white/10 text-white"
          : "border-white/10 bg-white/[0.04] text-white/60 hover:text-white/85"
      }`
    : `rounded-ui px-[12px] text-[12.5px] ${
        active
          ? "border-white/[0.16] bg-white/[0.10] text-white"
          : "border-white/[0.08] text-white/55 hover:bg-white/[0.05] hover:text-white"
      }`;
  const chip = (
    <button
      type="button"
      aria-pressed={active}
      disabled={disabled}
      onClick={onClick}
      className={`border py-[7px] font-medium transition-colors duration-150 ${skin}`}
    >
      {children}
    </button>
  );
  return tip ? <Hint tip={tip}>{chip}</Hint> : chip;
}

// ------------ Toggle ------------
// The on/off switch used throughout the launcher. The full-size knob slides like a drop: its two edges move on two
// springs, so it stretches on the way and gathers as it lands, always inside its track. The overlay uses the
// smaller compact one, whose knob jumps instead of sliding.
const INSTANT = { duration: 0 };

const KNOB_OFF = { left: 3, right: 25 };
const KNOB_ON = { left: 25, right: 3 };

export function Toggle({
  checked,
  onChange,
  disabled = false,
  ariaLabel,
  compact = false,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  disabled?: boolean;
  ariaLabel?: string;
  compact?: boolean;
}) {
  const skin = compact
    ? `h-[26px] w-[46px] rounded-[8px] transition-[border-color,opacity] ${
        checked ? "border-white/30" : "border-white/15"
      } ${disabled ? "cursor-not-allowed opacity-45" : "cursor-pointer hover:border-white/40"}`
    : `h-[28px] w-[52px] rounded-ui transition-[border-color,transform] ${
        disabled
          ? "cursor-not-allowed border-white/15 opacity-50"
          : "cursor-pointer border-white/15 hover:border-white/30 active:scale-[0.97]"
      }`;
  const knob = compact
    ? `h-[18px] w-[20px] rounded-[6px] transition-colors duration-150 ${checked ? "bg-white" : "bg-white/[0.28]"}`
    : `absolute top-[3px] bottom-[3px] rounded-[7px] ${checked ? "bg-white" : "border border-white/20 bg-white/10"}`;
  const reduce = useReducedMotionSafe();

  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative flex shrink-0 items-center border bg-black/30 p-[3px] duration-150 ${skin}`}
    >
      {compact ? (
        <m.span initial={false} animate={{ x: checked ? 18 : 0 }} transition={INSTANT} className={knob} />
      ) : (
        <m.span
          initial={false}
          animate={checked ? KNOB_ON : KNOB_OFF}
          transition={
            reduce
              ? INSTANT
              : checked
                ? { right: edgeLead, left: edgeTrail }
                : { left: edgeLead, right: edgeTrail }
          }
          className={knob}
        />
      )}
    </button>
  );
}

// ------------ Radio Group Keys ------------
// Arrow, Home and End key handling for groups of radio-style buttons like the segmented controls.
export function radioGroupKeyTarget(key: string, index: number, count: number): number | null {
  if (count <= 0) return null;
  switch (key) {
    case "ArrowLeft":
    case "ArrowUp":
      return (index - 1 + count) % count;
    case "ArrowRight":
    case "ArrowDown":
      return (index + 1) % count;
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return null;
  }
}

function handleRadioGroupKey(
  e: KeyboardEvent<HTMLElement>,
  index: number,
  count: number,
): number | null {
  const next = radioGroupKeyTarget(e.key, index, count);
  if (next === null) return null;
  e.preventDefault();
  e.currentTarget.querySelectorAll<HTMLElement>('[role="radio"]')[next]?.focus();
  return next;
}

// ------------ Segmented Control ------------
// A short row of options where exactly one is picked, with a sliding highlight. Works with the arrow keys. The
// overlay uses the smaller compact one, whose options share the row's width and light up in place.
interface Option {
  value: string;
  label: string;
}

export function SegmentedControl({
  options,
  value,
  onChange,
  ariaLabel,
  disabled = false,
  width,
  compact = false,
  className = "",
}: {
  options: Option[];
  value: string;
  onChange: (value: string) => void;
  ariaLabel?: string;
  disabled?: boolean;
  width?: number;
  compact?: boolean;
  className?: string;
}) {
  const activeIndex = options.findIndex((o) => o.value === value);
  const focusIndex = activeIndex < 0 ? 0 : activeIndex;
  const reduce = useReducedMotionSafe();

  const groupRef = useRef<HTMLDivElement>(null);
  const [edges, setEdges] = useState<{ left: number; right: number; toRight: boolean } | null>(null);
  useLayoutEffect(() => {
    const group = groupRef.current;
    if (compact || !group) return;
    const measure = () => {
      const option = group.querySelectorAll<HTMLElement>('[role="radio"]')[activeIndex];
      if (!option) {
        setEdges(null);
        return;
      }
      const left = option.offsetLeft;
      const right = group.clientWidth - option.offsetLeft - option.offsetWidth;
      setEdges((prev) =>
        prev && prev.left === left && prev.right === right
          ? prev
          : { left, right, toRight: prev ? left > prev.left : true },
      );
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(group);
    return () => observer.disconnect();
  }, [activeIndex, compact, options.length]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (disabled) return;
    const next = handleRadioGroupKey(e, focusIndex, options.length);
    if (next !== null && options[next].value !== value) onChange(options[next].value);
  };

  return (
    <div
      ref={groupRef}
      role="radiogroup"
      aria-label={ariaLabel}
      aria-disabled={disabled || undefined}
      onKeyDown={onKeyDown}
      style={width ? { width } : undefined}
      className={`relative flex border border-white/15 bg-black/30 p-[3px] ${
        compact ? "min-w-0 gap-[4px] rounded-[8px]" : "gap-[3px] rounded-ui"
      } ${disabled ? "cursor-not-allowed opacity-50" : ""} ${className}`}
    >
      {!compact && edges && (
        <m.span
          aria-hidden
          className="pointer-events-none absolute top-[3px] bottom-[3px] rounded-[7px]"
          style={{ background: "rgba(255,255,255,0.1)", border: "1px solid rgba(255,255,255,0.2)" }}
          initial={false}
          animate={{ left: edges.left, right: edges.right }}
          transition={
            reduce
              ? { duration: 0 }
              : edges.toRight
                ? { right: edgeLead, left: edgeTrail }
                : { left: edgeLead, right: edgeTrail }
          }
        />
      )}
      {options.map((o, i) => {
        const active = o.value === value;
        return (
          <button
            key={o.value}
            type="button"
            role="radio"
            aria-checked={active}
            tabIndex={i === focusIndex ? 0 : -1}
            disabled={disabled}
            onClick={() => onChange(o.value)}
            className={
              compact
                ? `min-w-0 flex-auto truncate rounded-[6px] px-[10px] py-[7px] text-center text-[12px] transition-colors duration-150 ${
                    active
                      ? "bg-white/10 font-semibold text-white shadow-[inset_0_0_0_1px_rgba(255,255,255,0.2)]"
                      : "font-medium text-white/70 hover:bg-white/[0.06] hover:text-white"
                  }`
                : `relative rounded-[7px] px-[14px] py-[7px] ${width ? "flex-1" : ""} text-[13px] font-medium leading-none transition-colors disabled:cursor-not-allowed ${
                    active ? "text-white" : "text-white/70 enabled:hover:bg-white/[0.07] enabled:hover:text-white"
                  }`
            }
          >
            {compact ? o.label : <span className="relative z-[1]">{o.label}</span>}
          </button>
        );
      })}
    </div>
  );
}
