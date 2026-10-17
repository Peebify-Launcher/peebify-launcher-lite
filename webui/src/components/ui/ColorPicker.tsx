// ------------ Color Picker ------------
// The launcher's own color picker, shown as a small popover next to a swatch: a saturation/brightness field, a hue
// strip, a hex box, an eyedropper, a before/after swatch and preset colors. Changes apply live as you drag.
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type PointerEvent,
  type RefObject,
} from "react";
import { Check, Pipette, Undo2 } from "lucide-react";
import { useClickOutside, useDismissOnViewportChange, useEscapeKey } from "../../lib/hooks";
import { DropPanel } from "./PopoverMenu";
import { useT } from "../../lib/useT";
import { Hint } from "./Tooltip";

interface Hsv {
  h: number;
  s: number;
  v: number;
}

const PANEL_WIDTH = 256;
const PANEL_HEIGHT_ESTIMATE = 360;
const EDGE = 8;

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

function parseHex(input: string): string | null {
  const raw = input.trim().replace(/^#/, "").toLowerCase();
  if (/^[0-9a-f]{6}$/.test(raw)) return `#${raw}`;
  if (/^[0-9a-f]{3}$/.test(raw)) return `#${[...raw].map((c) => c + c).join("")}`;
  const rgb = /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/.exec(input.trim());
  if (rgb) {
    return `#${rgb
      .slice(1, 4)
      .map((n) => clamp(Number(n), 0, 255).toString(16).padStart(2, "0"))
      .join("")}`;
  }
  return null;
}

function hexToRgb(hex: string): [number, number, number] {
  const full = parseHex(hex) ?? "#888888";
  return [1, 3, 5].map((i) => parseInt(full.slice(i, i + 2), 16)) as [number, number, number];
}

function hexToHsv(hex: string): Hsv {
  const [r, g, b] = hexToRgb(hex).map((n) => n / 255);
  const max = Math.max(r, g, b);
  const d = max - Math.min(r, g, b);
  let h = 0;
  if (d) {
    if (max === r) h = ((g - b) / d) % 6;
    else if (max === g) h = (b - r) / d + 2;
    else h = (r - g) / d + 4;
    h = (h * 60 + 360) % 360;
  }
  return { h, s: max ? d / max : 0, v: max };
}

function hsvToHex({ h, s, v }: Hsv): string {
  const f = (n: number) => {
    const k = (n + h / 60) % 6;
    return Math.round((v - v * s * Math.max(0, Math.min(k, 4 - k, 1))) * 255);
  };
  return `#${[f(5), f(3), f(1)].map((n) => n.toString(16).padStart(2, "0")).join("")}`;
}

function inkFor(hex: string): string {
  const [r, g, b] = hexToRgb(hex).map((n) => {
    const c = n / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.4 ? "#121216" : "#ffffff";
}

type EyeDropperCtor = new () => { open: () => Promise<{ sRGBHex: string }> };
const EyeDropper = (window as unknown as { EyeDropper?: EyeDropperCtor }).EyeDropper;

function useDrag(onMove: (x: number, y: number) => void) {
  const [dragging, setDragging] = useState(false);
  const report = (e: PointerEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    onMove(clamp((e.clientX - r.left) / r.width, 0, 1), clamp((e.clientY - r.top) / r.height, 0, 1));
  };
  return {
    dragging,
    bind: {
      onPointerDown: (e: PointerEvent<HTMLElement>) => {
        if (e.button !== 0) return;
        e.currentTarget.setPointerCapture(e.pointerId);
        e.currentTarget.focus({ preventScroll: true });
        setDragging(true);
        report(e);
      },
      onPointerMove: (e: PointerEvent<HTMLElement>) => {
        if (e.currentTarget.hasPointerCapture(e.pointerId)) report(e);
      },
      onPointerUp: () => setDragging(false),
      onPointerCancel: () => setDragging(false),
    },
  };
}

const THUMB =
  "pointer-events-none absolute size-[18px] -translate-x-1/2 -translate-y-1/2 rounded-full border-[2.5px] border-white shadow-[0_0_0_1px_rgba(0,0,0,0.35),0_2px_8px_rgba(0,0,0,0.45)] transition-transform duration-100";

const SECTION_LABEL = "text-[10.5px] font-medium uppercase tracking-[0.6px] text-white/45";

function ColorPanel({
  value,
  onChange,
  presets,
  label,
  fieldRef,
}: {
  value: string;
  onChange: (hex: string) => void;
  presets: string[];
  label: string;
  fieldRef: RefObject<HTMLDivElement | null>;
}) {
  const t = useT();
  const [original] = useState(value);
  const [state, setState] = useState(() => ({ hex: value, hsv: hexToHsv(value) }));
  let hsv = state.hsv;
  if (state.hex !== value) {
    hsv = hexToHsv(value);
    setState({ hex: value, hsv });
  }
  const [draft, setDraft] = useState<string | null>(null);

  const apply = (next: Hsv) => {
    const hex = hsvToHex(next);
    setState({ hex, hsv: next });
    setDraft(null);
    if (hex !== value) onChange(hex);
  };
  const applyHex = (hex: string) => {
    setState({ hex, hsv: hexToHsv(hex) });
    setDraft(null);
    if (hex !== value) onChange(hex);
  };

  const field = useDrag((x, y) => apply({ ...hsv, s: x, v: 1 - y }));
  const hue = useDrag((x) => apply({ ...hsv, h: Math.min(x * 360, 359.9) }));

  const onFieldKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 0.1 : 0.01;
    const moves: Record<string, [number, number]> = {
      ArrowLeft: [-step, 0],
      ArrowRight: [step, 0],
      ArrowUp: [0, step],
      ArrowDown: [0, -step],
    };
    const move = moves[e.key];
    if (!move) return;
    e.preventDefault();
    apply({ ...hsv, s: clamp(hsv.s + move[0], 0, 1), v: clamp(hsv.v + move[1], 0, 1) });
  };

  const onHueKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 10 : 1;
    const delta = { ArrowLeft: -step, ArrowDown: -step, ArrowRight: step, ArrowUp: step }[e.key];
    if (delta === undefined) return;
    e.preventDefault();
    apply({ ...hsv, h: (hsv.h + delta + 360) % 360 });
  };

  const pickFromScreen = async () => {
    if (!EyeDropper) return;
    try {
      const { sRGBHex } = await new EyeDropper().open();
      const hex = parseHex(sRGBHex);
      if (hex) applyHex(hex);
    } catch {
    }
  };

  const commitDraft = () => {
    if (draft === null) return;
    const hex = parseHex(draft);
    if (hex) applyHex(hex);
    else setDraft(null);
  };

  const pure = hsvToHex({ h: hsv.h, s: 1, v: 1 });
  const changed = original !== value;

  return (
    <div className="flex flex-col gap-[12px] p-[12px]">
      <div
        ref={fieldRef}
        role="slider"
        tabIndex={0}
        aria-label={t("ui.colorPicker.field", { label })}
        aria-valuetext={t("ui.colorPicker.fieldValue", {
          saturation: Math.round(hsv.s * 100),
          brightness: Math.round(hsv.v * 100),
        })}
        aria-valuenow={Math.round(hsv.s * 100)}
        onKeyDown={onFieldKey}
        {...field.bind}
        className="relative h-[150px] cursor-crosshair touch-none rounded-[8px] outline-none ring-white/60 focus-visible:ring-2"
        style={{
          background: `linear-gradient(to top, #000, transparent), linear-gradient(to right, #fff, ${pure})`,
        }}
      >
        <div className="pointer-events-none absolute inset-0 rounded-[8px] ring-1 ring-inset ring-white/10" />
        <span
          className={`${THUMB} ${field.dragging ? "scale-[1.2]" : ""}`}
          style={{ left: `${hsv.s * 100}%`, top: `${(1 - hsv.v) * 100}%`, background: value }}
        />
      </div>

      <div
        role="slider"
        tabIndex={0}
        aria-label={t("ui.colorPicker.hue", { label })}
        aria-valuemin={0}
        aria-valuemax={360}
        aria-valuenow={Math.round(hsv.h)}
        onKeyDown={onHueKey}
        {...hue.bind}
        className="relative mx-[9px] h-[12px] cursor-pointer touch-none rounded-full outline-none ring-white/60 focus-visible:ring-2"
        style={{
          background:
            "linear-gradient(to right, #f00 0%, #ff0 16.66%, #0f0 33.33%, #0ff 50%, #00f 66.66%, #f0f 83.33%, #f00 100%)",
        }}
      >
        <span
          className={`${THUMB} top-1/2 ${hue.dragging ? "scale-[1.2]" : ""}`}
          style={{ left: `${(hsv.h / 360) * 100}%`, background: pure }}
        />
      </div>

      <div className="flex items-center gap-[8px]">
        <Hint tip={changed ? t("ui.colorPicker.revertHint") : undefined} className="flex shrink-0">
          <div className="flex h-[34px] w-[46px] overflow-hidden rounded-[8px] ring-1 ring-inset ring-white/15">
            <button
              type="button"
              aria-label={t("ui.colorPicker.revert")}
              disabled={!changed}
              onClick={() => applyHex(original)}
              className="group grid flex-1 place-items-center disabled:cursor-default"
              style={{ background: original }}
            >
              {changed && (
                <Undo2
                  size={12}
                  className="opacity-0 transition-opacity duration-150 group-hover:opacity-90"
                  style={{ color: inkFor(original) }}
                />
              )}
            </button>
            <span className="flex-1" style={{ background: value }} />
          </div>
        </Hint>
        <label className="flex h-[34px] min-w-0 flex-1 items-center gap-[2px] rounded-[8px] border border-white/10 bg-black/30 px-[10px] font-mono text-[12.5px] transition-colors focus-within:border-white/30">
          <span className="text-white/40">#</span>
          <input
            value={draft ?? value.replace(/^#/, "").toUpperCase()}
            onChange={(e) => setDraft(e.target.value)}
            onBlur={commitDraft}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitDraft();
            }}
            spellCheck={false}
            maxLength={7}
            aria-label={t("ui.colorPicker.hex", { label })}
            className="min-w-0 flex-1 bg-transparent uppercase text-white outline-none"
          />
        </label>
        {EyeDropper && (
          <Hint tip={t("ui.colorPicker.eyedropper")} placement="bottom" className="flex shrink-0">
            <button
              type="button"
              aria-label={t("ui.colorPicker.eyedropper")}
              onClick={() => void pickFromScreen()}
              className="grid size-[34px] place-items-center rounded-[8px] border border-white/10 bg-white/[0.04] text-white/70 transition duration-150 hover:bg-white/[0.1] hover:text-white active:scale-[0.95]"
            >
              <Pipette size={14} />
            </button>
          </Hint>
        )}
      </div>

      <div className="flex flex-col gap-[7px]">
        <div className={SECTION_LABEL}>{t("ui.colorPicker.presets")}</div>
        <div className="grid grid-cols-8 gap-[6px]">
          {presets.map((preset) => {
            const active = preset === value;
            return (
              <button
                key={preset}
                type="button"
                aria-label={t("ui.colorPicker.usePreset", { color: preset.toUpperCase() })}
                aria-pressed={active}
                onClick={() => applyHex(preset)}
                className={`grid aspect-square place-items-center rounded-[6px] ring-inset transition duration-150 hover:scale-[1.1] active:scale-[0.95] ${
                  active ? "ring-2 ring-white" : "ring-1 ring-white/15"
                }`}
                style={{ background: preset }}
              >
                {active && <Check size={12} strokeWidth={3} style={{ color: inkFor(preset) }} />}
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}

export function ColorPopover({
  open,
  onClose,
  anchorRef,
  value,
  onChange,
  presets,
  label,
}: {
  open: boolean;
  onClose: () => void;
  anchorRef: RefObject<HTMLElement | null>;
  value: string;
  onChange: (hex: string) => void;
  presets: string[];
  label: string;
}) {
  const panelRef = useRef<HTMLDivElement>(null);
  const fieldRef = useRef<HTMLDivElement>(null);
  const [style, setStyle] = useState<CSSProperties>({});

  useClickOutside([anchorRef, panelRef], onClose, open);
  useEscapeKey(() => {
    onClose();
    anchorRef.current?.focus({ preventScroll: true });
  }, open);

  useLayoutEffect(() => {
    if (!open) return;
    const rect = anchorRef.current?.getBoundingClientRect();
    if (!rect) return;
    const flipUp =
      rect.bottom + PANEL_HEIGHT_ESTIMATE + EDGE > window.innerHeight && rect.top > PANEL_HEIGHT_ESTIMATE;
    setStyle({
      position: "fixed",
      width: PANEL_WIDTH,
      left: clamp(rect.right - PANEL_WIDTH, EDGE, window.innerWidth - PANEL_WIDTH - EDGE),
      ...(flipUp ? { bottom: window.innerHeight - rect.top + 6 } : { top: rect.bottom + 6 }),
    });
  }, [open, anchorRef]);

  useEffect(() => {
    if (open) fieldRef.current?.focus({ preventScroll: true });
  }, [open]);
  useDismissOnViewportChange(panelRef, onClose, open);

  return (
    <DropPanel open={open} panelRef={panelRef} style={style} role="dialog" ariaLabel={label}>
      <ColorPanel value={value} onChange={onChange} presets={presets} label={label} fieldRef={fieldRef} />
    </DropPanel>
  );
}
