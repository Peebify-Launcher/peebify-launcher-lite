// ------------ Overlay Settings ------------
// The Settings tab of the overlay: overlay shortcuts you can rebind, the mod loader's reload key, and
// the status of the overlay helper.
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle } from "lucide-react";
import {
  getLoaderSettings,
  getOverlayConfig,
  getOverlayStatus,
  setLoaderSetting,
  setOverlayHotkey,
  type LoaderSetting,
  type OverlayHotkey,
  type OverlayStatus,
} from "../lib/ipc";
import { rpc } from "../lib/rpc";
import { changedSettingKey, useSettingsStore } from "../store/settingsStore";
import { GroupBox, GroupLabel, SettingCard } from "../components/ui/panels";
import { Toggle } from "../components/ui/controls";
import type { PanelProps } from "./OverlayApp";
import { acceleratorOf, captureHint, spacedAccelerator, useShortcutCapture } from "../lib/hotkeys";
import { MOD_RISK_WARNING } from "../lib/mods";
import type { TKey } from "../lib/i18n";
import { useTauriEvents } from "../lib/hooks";
import { useT } from "../lib/useT";

function shortcutLabel(
  t: ReturnType<typeof useT>,
  label: string,
  accelerator: string,
  capturing: boolean,
): string {
  if (capturing) return t("overlay.settings.shortcutCapturing", { label });
  if (!accelerator) return t("overlay.settings.shortcutUnset", { label });
  return t("overlay.settings.shortcutCurrent", { label, shortcut: spacedAccelerator(accelerator) });
}

const HELPER_NOTE: Record<string, TKey | ""> = {
  running: "",
  starting: "overlay.settings.helperStarting",
  failed: "overlay.settings.helperFailed",
  mismatched: "overlay.settings.helperMismatched",
  idle: "overlay.settings.helperIdle",
};

export function OverlaySettings({ gameId, modsUsable }: PanelProps) {
  const t = useT();
  const values = useSettingsStore((s) => s.values);
  const set = useSettingsStore((s) => s.set);
  const [hotkeys, setHotkeys] = useState<OverlayHotkey[]>([]);
  const [loaderKeys, setLoaderKeys] = useState<LoaderSetting[]>([]);
  const [status, setStatus] = useState<OverlayStatus | null>(null);
  const [rejected, setRejected] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);
  const [confirmMods, setConfirmMods] = useState(false);
  const { capturing, start, end: endCapture } = useShortcutCapture(() => setRejected(null));

  const refresh = useCallback(async () => {
    const config = await getOverlayConfig();
    setHotkeys(config?.hotkeys ?? []);
    setStatus((await getOverlayStatus()) ?? {});
    const { rows } = await getLoaderSettings(gameId);
    setLoaderKeys(rows.filter((row) => row.kind === "key"));
  }, [gameId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useTauriEvents({
    "overlay-status": () => void refresh(),
    "overlay-opened": () => void refresh(),
    "overlay-hotkeys-registered": () => void refresh(),
    "mod-ini-changed": (payload) => {
      const changed = (payload as { gameId?: string } | null)?.gameId;
      if (!changed || changed === gameId) void refresh();
    },
    "settings-changed": (payload) => {
      const key = changedSettingKey(payload);
      if (key === null || key.startsWith("overlay") || key.startsWith("behavior.overlay")) {
        void refresh();
      }
    },
  });

  const startCapture = (id: string) => {
    setRejected(null);
    setWarning(null);
    start(id);
  };

  const apply = async (id: string, accelerator: string) => {
    endCapture();
    const result = await setOverlayHotkey(id, accelerator, true);
    if (!result.ok) {
      setRejected(result.error ?? t("overlay.settings.shortcutRejected"));
      return;
    }
    setRejected(null);
    setWarning(result.warning);
    await refresh();
  };

  const applyLoader = async (id: string, accelerator: string) => {
    endCapture();
    const result = await setLoaderSetting(gameId, id, accelerator, true);
    if (!result.ok) {
      setRejected(result.error ?? t("overlay.settings.loaderRejected"));
      return;
    }
    setRejected(null);
    await refresh();
  };

  const bind = (id: string, e: React.KeyboardEvent, loader = false) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      endCapture();
      return;
    }
    const accelerator = acceleratorOf(e);
    if (!accelerator) {
      const hint = captureHint(e);
      if (hint) setRejected(hint);
      return;
    }
    void (loader ? applyLoader(id, accelerator) : apply(id, accelerator));
  };

  const blocked = status?.unregistered ?? [];
  const invalid = status?.invalid ?? [];
  const state = status ? String(status.state ?? "idle") : null;
  const noteKey = state && state !== "running" ? HELPER_NOTE[state] : "";
  const note = noteKey ? t(noteKey) : "";

  const shortcutRow = (
    row: { id: string; label: string; description?: string },
    accelerator: string,
    fallback: string | null,
    loader: boolean,
  ) => {
    const active = capturing === row.id;
    return (
      <div key={row.id} className="flex items-center justify-between gap-4 px-4 py-3">
        <div className="min-w-0">
          <div className="text-[12.5px] text-white/[0.78]">{row.label}</div>
          {row.description && (
            <div className="mt-[3px] max-w-[54ch] text-[11.5px] leading-[1.45] text-white/55">
              {row.description}
            </div>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {fallback !== null && (
            <button
              type="button"
              aria-label={t("overlay.settings.resetShortcut", { label: row.label })}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => void (loader ? applyLoader(row.id, fallback) : apply(row.id, fallback))}
              className="rounded-[6px] px-2 py-1 text-[11px] text-white/55 hover:text-white"
            >
              {t("overlay.settings.reset")}
            </button>
          )}
          <button
            type="button"
            aria-label={shortcutLabel(t, row.label, accelerator, active)}
            onClick={() => (active ? endCapture() : startCapture(row.id))}
            onKeyDown={(e) => {
              if (active) bind(row.id, e, loader);
            }}
            onBlur={() => {
              if (active) endCapture();
            }}
            className={`ov-mono rounded-[6px] px-[9px] py-1 text-[11px] transition-colors duration-150 ${
              active
                ? "bg-white/[0.16] text-white ring-1 ring-white/30"
                : !loader && blocked.includes(accelerator)
                  ? "bg-[#ef4444]/15 text-danger-soft hover:bg-[#ef4444]/25"
                  : "bg-white/[0.08] text-white/75 hover:bg-white/[0.14]"
            }`}
          >
            {active ? t("settings.overlay.pressShortcut") : spacedAccelerator(accelerator)}
          </button>
        </div>
      </div>
    );
  };

  return (
    <div className="flex flex-col gap-3">
      {(note || blocked.length > 0 || invalid.length > 0) && (
        <div className="flex items-start gap-3 rounded-[10px] border border-[#f59e0b]/30 bg-[#f59e0b]/[0.08] px-4 py-3">
          <AlertTriangle size={16} className="mt-[2px] shrink-0 text-warning" />
          <div className="text-[12.5px] leading-relaxed text-warning">
            {note && <p>{note}</p>}
            {blocked.length > 0 && (
              <p>
                {t("overlay.settings.blocked", {
                  count: blocked.length,
                  keys: blocked.map(spacedAccelerator).join(", "),
                })}
              </p>
            )}
            {invalid.length > 0 && (
              <p>
                {t("overlay.settings.invalid", {
                  count: invalid.length,
                  keys: invalid.map(spacedAccelerator).join(", "),
                })}
              </p>
            )}
            {status?.error && <p className="mt-1 text-white/50">{String(status.error)}</p>}
          </div>
        </div>
      )}

      <section className="flex flex-col gap-2">
        <GroupLabel compact label={t("common.general")} />
        <GroupBox compact divided>
          <SettingCard
            compact
            title={t("overlay.settings.modSupport")}
            description={t("overlay.settings.modSupportDescription")}
            control={
              <Toggle
                compact
                checked={values.modsEnabled === "true"}
                ariaLabel={t("overlay.settings.modSupport")}
                onChange={(v) => {
                  if (v) {
                    setConfirmMods(true);
                    return;
                  }
                  setConfirmMods(false);
                  set("modsEnabled", "false");
                }}
              />
            }
          />
          {confirmMods && values.modsEnabled !== "true" && (
            <div className="flex flex-col gap-3 bg-[#ef4444]/[0.06] px-4 py-3">
              <p className="text-[12px] leading-relaxed text-danger-soft">{t(MOD_RISK_WARNING)}</p>
              <div className="flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setConfirmMods(false)}
                  className="rounded-[8px] px-[13px] py-[7px] text-[12px] text-white/60 hover:text-white"
                >
                  {t("common.cancel")}
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setConfirmMods(false);
                    set("modsEnabled", "true");
                  }}
                  className="rounded-[8px] border border-[#ef4444]/40 bg-[#ef4444]/15 px-[13px] py-[7px] text-[12px] font-medium text-danger-soft hover:bg-[#ef4444]/25"
                >
                  {t("overlay.settings.enableMods")}
                </button>
              </div>
            </div>
          )}
          <SettingCard
            compact
            title={t("overlay.settings.nsfw")}
            description={t("overlay.settings.nsfwDescription")}
            control={
              <Toggle
                compact
                checked={values.showNsfwMods === "true"}
                disabled={!modsUsable}
                ariaLabel={t("overlay.settings.nsfw")}
                onChange={(v) => set("showNsfwMods", String(v))}
              />
            }
          />
        </GroupBox>
      </section>

      <section className="flex flex-col gap-2">
        <GroupLabel compact label={t("overlay.settings.overlayShortcuts")} />
        <GroupBox compact divided>
          {hotkeys.map((hotkey) =>
            shortcutRow(
              hotkey,
              hotkey.accelerator,
              hotkey.accelerator !== hotkey.default ? hotkey.default : null,
              false,
            ),
          )}
        </GroupBox>
      </section>

      {modsUsable && loaderKeys.length > 0 && (
        <section className="flex flex-col gap-2">
          <GroupLabel compact label={t("overlay.settings.loaderShortcuts")} />
          <GroupBox compact divided>
            {loaderKeys.map((row) =>
              shortcutRow(
                {
                  ...row,
                  description: t("overlay.settings.loaderShortcutDescription", {
                    description: row.description,
                  }),
                },
                row.accelerator ?? "",
                row.defaultAccelerator && (row.accelerator ?? "") !== row.defaultAccelerator
                  ? row.defaultAccelerator
                  : null,
                true,
              ),
            )}
          </GroupBox>
        </section>
      )}

      {capturing && (
        <p className="text-[11.5px] leading-relaxed text-white/55">
          {t("overlay.settings.capturingNote")}
        </p>
      )}

      {rejected && (
        <div
          role="alert"
          className="rounded-[10px] border border-[#ef4444]/30 bg-[#ef4444]/[0.08] px-4 py-3 text-[12.5px] text-danger-soft"
        >
          {rejected}
        </div>
      )}

      {warning && (
        <div className="flex items-start gap-3 rounded-[10px] border border-[#f59e0b]/30 bg-[#f59e0b]/[0.08] px-4 py-3">
          <AlertTriangle size={16} className="mt-[2px] shrink-0 text-warning" />
          <p className="text-[12.5px] leading-relaxed text-warning">{warning}</p>
        </div>
      )}

      <div className="flex items-center justify-between gap-4 rounded-[10px] border border-white/[0.08] bg-white/[0.045] px-4 py-[14px]">
        <div>
          <div className="text-[13px] font-medium">{t("overlay.settings.everythingElse")}</div>
          <div className="mt-[3px] text-[11.5px] text-white/55">
            {t("overlay.settings.everythingElseDescription")}
          </div>
        </div>
        <button
          type="button"
          onClick={() => void rpc("overlay-open-launcher")}
          className="shrink-0 rounded-[8px] border border-white/15 bg-white/[0.06] px-[13px] py-[7px] text-[12px] font-medium hover:bg-white/10"
        >
          {t("overlay.settings.openLauncher")}
        </button>
      </div>
    </div>
  );
}
