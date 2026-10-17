// ------------ Slider ------------
// A slider with labelled tick marks and a live value. Drag or use the keyboard, and a dragged value is only
// saved when you let go. The overlay uses the compact one: a thin bar with no knob, value or ticks.
import { useCallback, useRef, useState } from "react";
import { m } from "framer-motion";
import { springJelly, useReducedMotionSafe } from "../../lib/motion";
import { useGlassEffects } from "../../store/settingsStore";
import { useT } from "../../lib/useT";

export function Slider({
  min,
  max,
  step,
  value,
  onChange,
  onDraft,
  unit = "",
  ticks = [],
  ariaLabel,
  disabled = false,
  compact = false,
}: {
  min: number;
  max: number;
  step: number;
  value: number;
  onChange: (value: number) => void;
  onDraft?: (value: number | null) => void;
  unit?: string;
  ticks?: string[];
  ariaLabel?: string;
  disabled?: boolean;
  compact?: boolean;
}) {
  const tr = useT();
  const trackRef = useRef<HTMLDivElement>(null);
  const draftRef = useRef<number | null>(null);
  const [draft, setDraft] = useState<number | null>(null);
  const reduce = useReducedMotionSafe();
  const effects = useGlassEffects();

  const clamp = useCallback((v: number) => Math.min(max, Math.max(min, v)), [min, max]);
  const snap = useCallback(
    (v: number) => clamp(Math.round(clamp(v) / step) * step),
    [clamp, step],
  );

  const fromClientX = useCallback(
    (clientX: number) => {
      const rect = trackRef.current?.getBoundingClientRect();
      if (!rect || rect.width === 0) return draftRef.current ?? value;
      const fraction = (clientX - rect.left) / rect.width;
      return snap(min + fraction * (max - min));
    },
    [min, max, snap, value],
  );

  const updateDraft = (next: number | null) => {
    if (next === draftRef.current) return;
    draftRef.current = next;
    setDraft(next);
    onDraft?.(next);
  };

  const commit = (v: number) => {
    if (v !== value) onChange(v);
  };

  const finish = () => {
    const next = draftRef.current;
    if (next === null) return;
    updateDraft(null);
    commit(next);
  };

  const handlePointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (disabled) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    updateDraft(fromClientX(e.clientX));
  };

  const handlePointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (draftRef.current === null || !e.currentTarget.hasPointerCapture(e.pointerId)) return;
    updateDraft(fromClientX(e.clientX));
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (disabled || draftRef.current !== null) return;
    if (e.key === "ArrowRight" || e.key === "ArrowUp") commit(snap(value + step));
    else if (e.key === "ArrowLeft" || e.key === "ArrowDown") commit(snap(value - step));
    else if (e.key === "PageUp") commit(snap(value + step * 5));
    else if (e.key === "PageDown") commit(snap(value - step * 5));
    else if (e.key === "Home") commit(min);
    else if (e.key === "End") commit(max);
    else return;
    e.preventDefault();
  };

  const lens = draft !== null && effects === "full";
  const shown = draft ?? value;
  const pct = max === min ? 0 : ((clamp(shown) - min) / (max - min)) * 100;
  const label = compact ? String(shown) : tr("ui.slider.value", { value: shown, unit });

  const track = {
    ref: trackRef,
    role: "slider",
    tabIndex: disabled ? -1 : 0,
    "aria-valuemin": min,
    "aria-valuemax": max,
    "aria-valuenow": shown,
    "aria-valuetext": label,
    "aria-label": ariaLabel,
    "aria-disabled": disabled || undefined,
    onPointerDown: handlePointerDown,
    onPointerMove: handlePointerMove,
    onPointerUp: finish,
    onLostPointerCapture: finish,
    onKeyDown: handleKeyDown,
  };

  if (compact) {
    return (
      <div {...track} className={`-my-2 py-2 ${disabled ? "cursor-not-allowed opacity-40" : "cursor-pointer"}`}>
        <div className="h-[4px] rounded-[2px] bg-white/[0.12]">
          <div className="h-[4px] rounded-[2px] bg-white" style={{ width: `${pct}%` }} />
        </div>
      </div>
    );
  }

  return (
    <div className="w-[220px]">
      <div className="mb-2 text-right text-[12.5px] font-medium tabular-nums text-white/85">{label}</div>
      <div {...track} className="progress-track relative h-[6px] cursor-pointer touch-none rounded-full">
        <div className="progress-fill absolute inset-y-0 left-0 rounded-full" style={{ width: `${pct}%` }} />
        <m.div
          className={`absolute h-[16px] w-[16px] rounded-full border shadow-[0_1px_4px_rgba(0,0,0,0.4)] transition-colors duration-150 ${
            lens
              ? "border-white/70 bg-white/15 backdrop-blur-[0.5px] backdrop-saturate-[1.8] backdrop-brightness-125"
              : "border-white/30 bg-white"
          }`}
          style={{ left: `${pct}%`, top: "50%", x: "-50%", y: "-50%" }}
          animate={{ scale: lens ? 1.75 : 1 }}
          whileTap={reduce || lens ? undefined : { scale: 1.15 }}
          transition={reduce ? { duration: 0 } : springJelly}
        />
      </div>
      <div className="mt-2 flex justify-between text-[11px] text-white/55">
        {ticks.map((t) => (
          <span key={t}>{t}</span>
        ))}
      </div>
    </div>
  );
}
