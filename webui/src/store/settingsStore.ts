// ------------ Settings Store ------------
// The launcher's settings values. Changes show up at once and are saved to the backend, and roll back
// if saving fails.
import { create } from "zustand";
import { SETTINGS_DEFAULTS } from "../data/settingsDefaults";
import { setLanguagePreference } from "../lib/i18n";
import { getSettings, setSetting } from "../lib/ipc";
import type { TimeFormat } from "../lib/format";

export function changedSettingKey(payload: unknown): string | null {
  const key = (payload as { key?: unknown } | null | undefined)?.key;
  return typeof key === "string" && key ? key : null;
}

export function changeTouchesSettings(payload: unknown): boolean {
  const key = changedSettingKey(payload);
  return key === null || !key.includes(".") || key.startsWith("behavior.");
}

interface SettingsState {
  values: Record<string, string>;
  hydrated: boolean;
  set: (id: string, value: string) => void;
  hydrate: () => Promise<void>;
}

export const useSettingsStore = create<SettingsState>((set, get) => ({
  values: { ...SETTINGS_DEFAULTS },
  hydrated: false,
  set: (id, value) => {
    const previous = get().values[id] ?? SETTINGS_DEFAULTS[id];
    set((state) => ({ values: { ...state.values, [id]: value } }));
    void setSetting(id, value).then((saved) => {
      if (!saved) set((state) => ({ values: { ...state.values, [id]: previous } }));
    });
  },
  hydrate: async () => {
    try {
      const remote = await getSettings();
      if (remote && Object.keys(remote).length > 0) {
        set((state) => ({ values: { ...state.values, ...remote } }));
      }
    } finally {
      set({ hydrated: true });
    }
  },
}));

setLanguagePreference(useSettingsStore.getState().values.language);
useSettingsStore.subscribe((state, prev) => {
  if (state.values.language !== prev.values.language) setLanguagePreference(state.values.language);
});

export function useTimeFormat(): TimeFormat {
  const value = useSettingsStore((s) => s.values.timeFormat);
  return value === "12" || value === "24" ? value : "system";
}

export const GLASS_EFFECTS = ["full", "balanced", "lite"] as const;
export type GlassEffects = (typeof GLASS_EFFECTS)[number];

export function useGlassEffects(): GlassEffects {
  const value = useSettingsStore((s) => s.values.glassEffects);
  return GLASS_EFFECTS.find((tier) => tier === value) ?? "full";
}
