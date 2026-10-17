// ------------ Setting Row ------------
// One settings row drawn from the settings table, and the controls only settings rows use: the number field and the
// expander that opens to show more settings.
import { useSyncExternalStore, type ReactNode, useEffect, useState, useId } from "react";
import { Loader2, ChevronDown } from "lucide-react";
import {
  ALL_SETTINGS,
  settingText,
  type Setting,
  type SettingOption,
} from "../../data/settings";
import { useSettingsStore } from "../../store/settingsStore";
import { useModsStore } from "../../store/mods";
import { useModalStore } from "../../store/modalStore";
import { useNotificationStore } from "../../store/notifications";
import { openLogsFolder, wipeLauncherData } from "../../lib/ipc";
import { t } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { SettingCard } from "../ui/panels";
import { SegmentedControl, Toggle, ActionButton } from "../ui/controls";
import { Select } from "../ui/Select";
import { Collapse } from "../../lib/motion";

// ------------ Row ------------
// Draws one setting from the settings list as the right kind of control (switch, dropdown, number or button),
// and runs the buttons such as Open logs and Clear launcher data.
let wiping = false;
const wipeListeners = new Set<() => void>();
const setWiping = (on: boolean) => {
  wiping = on;
  wipeListeners.forEach((l) => l());
};
const subscribeWiping = (l: () => void) => {
  wipeListeners.add(l);
  return () => void wipeListeners.delete(l);
};

async function clearLauncherData() {
  if (wiping) return;
  setWiping(true);
  useNotificationStore.getState().toast({
    title: t("settings.clearData.toastTitle"),
    text: t("settings.clearData.toastText"),
  });
  try {
    await wipeLauncherData();
  } finally {
    setWiping(false);
  }
}

function onAction(id: string) {
  const { openConfirm } = useModalStore.getState();
  if (id === "openLogs") void openLogsFolder();
  else if (id === "clearData") {
    if (wiping) return;
    openConfirm({
      title: t("settings.clearData.confirmTitle"),
      message: t("settings.clearData.confirmMessage"),
      confirmLabel: t("settings.items.clearData.button"),
      danger: true,
      onConfirm: () => void clearLauncherData(),
    });
  }
}

export function settingOn(s: Setting, values: Record<string, string>): boolean {
  if (s.kind !== "toggle") return false;
  const v = values[s.id];
  return s.invert ? v !== "true" : v === "true";
}

const optionsFor = (options: SettingOption[], tr: typeof t, current?: string, unit?: string) => {
  const list = options.map((o) => ({ value: o.value, label: settingText(o.label, tr) }));
  if (current && !list.some((o) => o.value === current)) {
    list.push({ value: current, label: unit ? `${current} ${unit}` : current });
  }
  return list;
};

export function SettingRow({ setting: s, badge }: { setting: Setting; badge?: ReactNode }) {
  const t = useT();
  const values = useSettingsStore((st) => st.values);
  const set = useSettingsStore((st) => st.set);
  const modsEnabled = useModsStore((st) => st.masterEnabled);
  const clearing = useSyncExternalStore(subscribeWiping, () => wiping);

  const parentDef = s.parent ? ALL_SETTINGS.find((x) => x.id === s.parent) : undefined;
  const parentOff = !!parentDef && parentDef.kind === "toggle" && values[s.parent!] !== "true";
  const disabled =
    parentOff || !!s.disableWhen?.(values) || (s.id === "showNsfwMods" && !modsEnabled);
  const title = settingText(s.title, t);
  const description = t(s.description);

  let control: ReactNode;
  switch (s.kind) {
    case "toggle":
      control = (
        <Toggle
          checked={settingOn(s, values)}
          disabled={disabled}
          ariaLabel={title}
          onChange={(v) => set(s.id, s.invert ? (v ? "false" : "true") : v ? "true" : "false")}
        />
      );
      break;
    case "segmented":
      control = (
        <SegmentedControl
          ariaLabel={title}
          options={optionsFor(s.options, t, values[s.id])}
          value={values[s.id]}
          disabled={disabled}
          width={s.width}
          onChange={(v) => set(s.id, v)}
        />
      );
      break;
    case "select":
      control = (
        <Select
          ariaLabel={title}
          options={optionsFor(s.options, t, values[s.id], s.unit)}
          value={values[s.id]}
          disabled={disabled}
          onChange={(v) => set(s.id, v)}
        />
      );
      break;
    case "number":
      control = (
        <NumberField
          value={values[s.id]}
          min={s.min}
          max={s.max}
          unit={s.unit}
          disabled={disabled}
          ariaLabel={title}
          onChange={(v) => set(s.id, v)}
        />
      );
      break;
    case "action": {
      const busy = s.id === "clearData" && clearing;
      control = (
        <ActionButton
          variant={s.danger ? "danger" : "neutral"}
          icon={busy ? <Loader2 size={15} className="animate-spin" /> : undefined}
          disabled={busy}
          onClick={() => onAction(s.id)}
        >
          {busy ? t("settings.clearData.clearing") : settingText(s.button, t)}
        </ActionButton>
      );
      break;
    }
  }

  return (
    <SettingCard
      icon={<s.icon size={18} />}
      title={title}
      description={description}
      badge={badge}
      disabled={disabled}
      danger={s.kind === "action" && !!s.danger}
      control={control}
      nested={!!s.indent}
    />
  );
}

// ------------ Number Field ------------
// A number box with a unit label. Typing is only applied when you leave the box or press Enter, values are
// clamped to the allowed range, and empty means "off" where that applies.
export function NumberField({
  value,
  onChange,
  min,
  max,
  unit,
  zeroLabel = "",
  disabled = false,
  ariaLabel,
}: {
  value: string;
  onChange: (value: string) => void;
  min: number;
  max: number;
  unit: string;
  zeroLabel?: string;
  disabled?: boolean;
  ariaLabel?: string;
}) {
  const external = value === "0" || !/^\d+$/.test(value) ? "" : value;
  const [draft, setDraft] = useState(external);

  useEffect(() => {
    setDraft(external);
  }, [external]);

  const commit = () => {
    if (draft === "" && zeroLabel === "") {
      setDraft(external);
      return;
    }
    if (draft === "" || (zeroLabel !== "" && Number(draft) === 0)) {
      setDraft("");
      if (value !== "0") onChange("0");
      return;
    }
    const clamped = String(Math.min(Math.max(Number(draft), min), max));
    setDraft(clamped);
    if (clamped !== value) onChange(clamped);
  };

  return (
    <div
      className={`flex shrink-0 items-center gap-2 rounded-ui border border-white/15 bg-black/30 py-[8px] pl-[14px] pr-[11px] text-[13px] font-medium transition-colors focus-within:border-white/30 ${
        disabled ? "pointer-events-none opacity-50" : ""
      }`}
    >
      <input
        type="text"
        inputMode="numeric"
        spellCheck={false}
        aria-label={ariaLabel}
        disabled={disabled}
        value={draft}
        placeholder={zeroLabel}
        onChange={(e) => setDraft(e.target.value.replace(/\D/g, "").slice(0, 5))}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") e.currentTarget.blur();
        }}
        className="w-[76px] bg-transparent text-white outline-none placeholder:text-white/45"
      />
      <span className="shrink-0 text-white/45">{draft === "" ? "" : unit}</span>
    </div>
  );
}

// ------------ Settings Expander ------------
// A settings row that opens to reveal more settings underneath, with a one-line summary while closed.
export function SettingsExpander({
  icon,
  title,
  summary,
  description,
  control,
  disabled = false,
  open: openProp,
  onOpenChange,
  children,
}: {
  icon: ReactNode;
  title: string;
  summary?: string;
  description?: string;
  control?: ReactNode;
  disabled?: boolean;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  children: ReactNode;
}) {
  const [openState, setOpenState] = useState(false);
  const open = openProp ?? openState;
  const bodyId = useId();
  const shown = open && !disabled;
  const toggle = () => {
    if (openProp === undefined) setOpenState(!open);
    onOpenChange?.(!open);
  };
  const line = shown ? (description ?? summary) : summary;

  return (
    <div inert={disabled} className={disabled ? "pointer-events-none opacity-50" : ""}>
      <div
        onClick={(e) => {
          if (!(e.target as HTMLElement).closest("button, input, a, [role='switch']")) toggle();
        }}
        className="group flex cursor-pointer items-center justify-between gap-4 px-5 py-4 transition-colors hover:bg-white/[0.04]"
      >
        <button
          type="button"
          onClick={toggle}
          aria-expanded={shown}
          aria-controls={bodyId}
          className="flex min-w-0 flex-1 items-center gap-3 text-left"
        >
          <span
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[8px] bg-white/[0.05]"
            style={{ color: "rgba(255,255,255,0.7)" }}
          >
            {icon}
          </span>
          <span className="min-w-0">
            <span className="flex items-center gap-2">
              <span className="text-[14px] font-medium" style={{ color: "#fff" }}>
                {title}
              </span>
            </span>
            {line && (
              <span
                className={`mt-1 block max-w-[56ch] text-[12.5px] leading-snug text-white/55 ${shown ? "" : "truncate"}`}
              >
                {line}
              </span>
            )}
          </span>
        </button>
        <div className="flex shrink-0 items-center gap-2">
          {control}
          <button
            type="button"
            onClick={toggle}
            tabIndex={-1}
            aria-hidden="true"
            className="grid h-8 w-8 place-items-center text-white/50 transition-colors group-hover:text-white"
          >
            <ChevronDown
              size={16}
              className={`transition-transform duration-200 ${shown ? "rotate-180" : ""}`}
            />
          </button>
        </div>
      </div>
      <div id={bodyId}>
        <Collapse open={shown} duration={0.24}>
          <div className="bg-black/[0.14]">{children}</div>
        </Collapse>
      </div>
    </div>
  );
}
