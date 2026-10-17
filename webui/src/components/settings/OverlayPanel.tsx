// ------------ Overlay Panel ------------
// The Overlay tab of Settings. Turns the in-game overlay on, sets its shortcuts, and covers screenshot and
// recording options including which audio (even Discord voice) goes into clips.
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  Check,
  FolderOpen,
  Gamepad2,
  Info,
  Keyboard,
  MessageCircle,
  Mic,
  Volume2,
  type LucideIcon,
} from "lucide-react";
import {
  chooseCaptureFolder,
  openCaptureFolder,
  resetCaptureFolder,
  setLoaderSetting,
  setOverlayHotkey,
  setSetting,
  toggledAudioTracks,
  type LoaderSetting,
  type OverlayConfig,
} from "../../lib/ipc";
import { SETTINGS_SECTIONS, type Setting } from "../../data/settings";
import { changedSettingKey, useSettingsStore } from "../../store/settingsStore";
import { useNotificationStore } from "../../store/notifications";
import { SettingsGroup, SettingCard } from "../ui/panels";
import { ActionButton } from "../ui/controls";
import { Hint } from "../ui/Tooltip";
import { SettingRow, settingOn } from "./SettingRow";
import { acceleratorOf, captureHint, Keys, useShortcutCapture } from "../../lib/hotkeys";
import { useTauriEvents } from "../../lib/hooks";
import { fetchRemembered, reads, recall, rememberAnswer } from "../../lib/launcherHooks";
import { GAMES } from "../../data/games";
import { useT } from "../../lib/useT";
import type { TKey } from "../../lib/i18n";

const OVERLAY = SETTINGS_SECTIONS.find((s) => s.category === "overlay")!.settings;
const byId = (id: string): Setting => OVERLAY.find((s) => s.id === id)!;
const inGroup = (group: TKey, except: string[] = []): Setting[] =>
  OVERLAY.filter((s) => s.group === group && !except.includes(s.id));

const MASTER = byId("overlayEnabled");
const SHOT_ROWS = inGroup("common.screenshots");
const REC_ROWS = inGroup("settings.groups.recording");

const TRACK_ICONS: Record<string, LucideIcon> = {
  desktop: Volume2,
  mic: Mic,
  game: Gamepad2,
  discord: MessageCircle,
};

function Note({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-start gap-[10px] bg-black/[0.12] py-3 pl-[68px] pr-5 text-[12.5px] leading-snug text-white/55">
      <span className="max-w-[72ch]">{children}</span>
    </div>
  );
}

function KeyCapture({
  label,
  accelerator,
  capturing,
  onStart,
  onEnd,
  onKey,
}: {
  label: string;
  accelerator: string | null;
  capturing: boolean;
  onStart: () => void;
  onEnd: () => void;
  onKey: (e: React.KeyboardEvent) => void;
}) {
  const t = useT();
  return (
    <button
      type="button"
      aria-label={`${label}: ${accelerator || t("settings.overlay.notSet")}`}
      onClick={() => (capturing ? onEnd() : onStart())}
      onKeyDown={(e) => {
        if (capturing) onKey(e);
      }}
      onBlur={() => capturing && onEnd()}
      className={`inline-flex min-h-[34px] items-center gap-[5px] rounded-ui border px-2 py-1 text-white transition duration-150 ${
        capturing
          ? "border-white bg-white/[0.12]"
          : "border-white/15 bg-white/[0.05] hover:border-white/25 hover:bg-white/[0.09]"
      }`}
    >
      {capturing ? (
        <span className="px-[6px] text-[12.5px] font-medium">{t("settings.overlay.pressShortcut")}</span>
      ) : accelerator ? (
        <Keys accelerator={accelerator} />
      ) : (
        <span className="px-[6px] text-[12.5px] font-medium text-white/70">{t("settings.overlay.notSet")}</span>
      )}
    </button>
  );
}

function ShortcutRow({
  title,
  description,
  accelerator,
  defaultAccelerator,
  capturing,
  disabled,
  onStart,
  onEnd,
  onKey,
  onReset,
}: {
  title: string;
  description?: string;
  accelerator: string | null;
  defaultAccelerator: string | null;
  capturing: boolean;
  disabled: boolean;
  onStart: () => void;
  onEnd: () => void;
  onKey: (e: React.KeyboardEvent) => void;
  onReset: () => void;
}) {
  const t = useT();
  const changed = !!defaultAccelerator && accelerator !== defaultAccelerator;
  return (
    <SettingCard
      icon={<Keyboard size={18} />}
      title={title}
      description={description}
      disabled={disabled}
      control={
        <div className="flex gap-2">
          {changed && <ActionButton onClick={onReset}>{t("common.reset")}</ActionButton>}
          <KeyCapture
            label={title}
            accelerator={accelerator}
            capturing={capturing}
            onStart={onStart}
            onEnd={onEnd}
            onKey={onKey}
          />
        </div>
      }
    />
  );
}

export function OverlayPanel() {
  const t = useT();
  const [config, setConfig] = useState<OverlayConfig | null>(() => recall(reads.overlayConfig) ?? null);
  const configRef = useRef<OverlayConfig | null>(config);
  const [failed, setFailed] = useState(false);
  const { capturing, start, end: endCapture } = useShortcutCapture();
  const hinted = useRef(false);
  const [loaderKeys, setLoaderKeys] = useState<LoaderSetting[]>(
    () => recall(reads.overlayLoaderSettings)?.rows.filter((row) => row.kind === "key") ?? [],
  );
  const [loaderGameId, setLoaderGameId] = useState<string | null>(
    () => recall(reads.overlayLoaderSettings)?.gameId ?? null,
  );
  const [windowError, setWindowError] = useState<string | null>(
    () => recall(reads.overlayStatus)?.windowError || null,
  );
  const push = useNotificationStore((s) => s.push);
  const toast = useNotificationStore((s) => s.toast);
  const values = useSettingsStore((s) => s.values);
  const set = useSettingsStore((s) => s.set);

  const enabled = settingOn(MASTER, values);

  const load = useCallback(async () => {
    const loaded = await fetchRemembered(reads.overlayConfig);
    configRef.current = loaded ?? null;
    setConfig(loaded ?? null);
    setFailed(!loaded);
    const loader = await fetchRemembered(reads.overlayLoaderSettings);
    setLoaderGameId(loader.gameId);
    setLoaderKeys(loader.rows.filter((row) => row.kind === "key"));
  }, []);

  const loadStatus = useCallback(async () => {
    const status = await fetchRemembered(reads.overlayStatus);
    setWindowError(status?.windowError || null);
  }, []);

  useEffect(() => {
    void load();
    void loadStatus();
  }, [load, loadStatus]);

  useTauriEvents({
    "settings-changed": (payload) => {
      const key = changedSettingKey(payload);
      if (
        key === null ||
        key.startsWith("overlay") ||
        key.startsWith("behavior.overlay") ||
        key === "launchAction" ||
        key === "behavior.launchAction"
      ) {
        void load();
      }
    },
    "mod-ini-changed": () => void load(),
    "overlay-status": (payload) => {
      const error = (payload as { windowError?: unknown } | null)?.windowError;
      if (typeof error === "string" && error) setWindowError(error);
      else void loadStatus();
    },
  });

  const startCapture = (id: string) => {
    hinted.current = false;
    start(id);
  };

  const bind = async (
    id: string,
    e: React.KeyboardEvent,
    apply: (id: string, accelerator: string) => Promise<void>,
  ) => {
    e.preventDefault();
    if (e.key === "Escape") {
      endCapture();
      return;
    }
    const accelerator = acceleratorOf(e);
    const hint = accelerator ? null : captureHint(e);
    if (hint && !hinted.current) {
      hinted.current = true;
      toast({ type: "warning", title: t("settings.overlay.pickDifferent"), text: hint });
    }
    if (!accelerator) return;
    endCapture();
    await apply(id, accelerator);
  };

  const applyHotkey = async (id: string, accelerator: string) => {
    const result = await setOverlayHotkey(id, accelerator);
    if (result.warning) {
      push({ type: "warning", title: t("settings.overlay.savedWithCatch"), text: result.warning });
    }
    if (result.ok) await load();
  };

  const loaderGameName = GAMES.find((g) => g.id === loaderGameId)?.name;

  const applyLoader = async (id: string, accelerator: string) => {
    const result = await setLoaderSetting(loaderGameId, id, accelerator, true);
    if (!result.ok) {
      push({
        type: "error",
        title: t("settings.overlay.notSaved"),
        text: result.error ?? t("settings.overlay.loaderRejected"),
      });
      return;
    }
    await load();
  };

  const toggleTrack = async (id: string) => {
    const latest = configRef.current;
    if (!latest) return;
    const next = toggledAudioTracks(latest.selectedAudioTracks, id);
    const updated = { ...latest, selectedAudioTracks: next };
    configRef.current = updated;
    setConfig(updated);
    rememberAnswer(reads.overlayConfig, updated);
    if (!(await setSetting("overlayAudioTracks", next.join(",")))) await load();
  };

  const shotRows = SHOT_ROWS.filter(
    (s) => s.id !== "overlayShotQuality" || values.overlayShotFormat === "jpeg",
  );

  return (
    <div className="flex flex-col gap-7">
      <SettingsGroup label={t("common.overlay")}>
        <SettingRow
          setting={MASTER}
          badge={
            enabled && (
              <Hint
                tip={
                  <span className="block max-w-[280px] whitespace-normal leading-snug">
                    {t("settings.overlay.borderlessDescription")}
                  </span>
                }
              >
                <button
                  type="button"
                  aria-label={t("settings.overlay.borderlessDescription")}
                  className="grid h-5 w-5 place-items-center rounded-full text-white/55 transition-colors hover:bg-white/10 hover:text-white"
                >
                  <Info size={15} />
                </button>
              </Hint>
            )
          }
        />
        {config?.launchAction === "close" && (
          <SettingCard
            danger
            icon={<AlertTriangle size={18} />}
            title={t("settings.overlay.closesTitle")}
            description={t("settings.overlay.closesDescription")}
            control={
              <ActionButton
                onClick={() => set("launchAction", "minimize")}
              >
                {t("settings.overlay.minimizeInstead")}
              </ActionButton>
            }
          />
        )}
        {enabled && windowError && (
          <SettingCard
            danger
            icon={<AlertTriangle size={18} />}
            title={t("settings.overlay.windowErrorTitle")}
            description={t("settings.overlay.windowErrorDescription", { error: windowError })}
          />
        )}
        {!enabled && <Note>{t("settings.overlay.featuresHint")}</Note>}
      </SettingsGroup>

      {config && (
        <SettingsGroup
          label={t("settings.groups.shortcuts")}
          hint={t("settings.overlay.shortcutsDescription")}
        >
          {config.hotkeys.map((hk) => (
            <ShortcutRow
              key={hk.id}
              title={hk.label}
              accelerator={hk.accelerator}
              defaultAccelerator={hk.default}
              capturing={capturing === hk.id}
              disabled={!enabled}
              onStart={() => startCapture(hk.id)}
              onEnd={endCapture}
              onKey={(e) => void bind(hk.id, e, applyHotkey)}
              onReset={() => void applyHotkey(hk.id, hk.default)}
            />
          ))}
          {loaderKeys.map((row) => (
            <ShortcutRow
              key={row.id}
              title={row.label}
              description={
                loaderGameName
                  ? t("settings.overlay.loaderKeyDescription", {
                      description: row.description,
                      game: loaderGameName,
                    })
                  : t("settings.overlay.loaderKeyDescriptionActiveGame", {
                      description: row.description,
                    })
              }
              accelerator={row.accelerator ?? null}
              defaultAccelerator={row.defaultAccelerator ?? null}
              capturing={capturing === row.id}
              disabled={!enabled}
              onStart={() => startCapture(row.id)}
              onEnd={endCapture}
              onKey={(e) => void bind(row.id, e, applyLoader)}
              onReset={() => void applyLoader(row.id, row.defaultAccelerator ?? "")}
            />
          ))}
        </SettingsGroup>
      )}

      <SettingsGroup label={t("common.screenshots")}>
        {shotRows.map((s) => (
          <SettingRow key={s.id} setting={s} />
        ))}
      </SettingsGroup>

      <SettingsGroup label={t("settings.groups.recording")}>
        {REC_ROWS.map((s) => (
          <SettingRow key={s.id} setting={s} />
        ))}
        {config && (
          <>
            <div
              inert={!enabled}
              className={`flex items-start justify-between gap-4 px-5 py-4 transition-colors duration-150 ${enabled ? "" : "pointer-events-none opacity-50"}`}
            >
              <div className="flex min-w-0 items-center gap-3">
                <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[8px] bg-white/[0.05] text-white/70">
                  <Volume2 size={18} />
                </span>
                <div className="min-w-0 text-left">
                  <h5 className="text-[14px] font-medium text-white">{t("settings.overlay.audio")}</h5>
                  <p className="mt-1 max-w-[56ch] text-[12.5px] leading-snug text-white/55">
                    {t("settings.overlay.audioDescription")}
                  </p>
                </div>
              </div>
              <div
                role="group"
                aria-label={t("settings.overlay.audio")}
                className="flex max-w-[460px] flex-wrap justify-end gap-2"
              >
                {config.audioTracks.map((track) => {
                  const on = config.selectedAudioTracks.includes(track.id);
                  const Icon = TRACK_ICONS[track.id] ?? Volume2;
                  return (
                    <Hint
                      key={track.id}
                      tip={track.available ? undefined : t("settings.overlay.trackUnavailable")}
                      className="inline-flex"
                    >
                      <button
                        type="button"
                        role="checkbox"
                        aria-checked={on}
                        disabled={!track.available}
                        onClick={() => void toggleTrack(track.id)}
                        className={`inline-flex h-[34px] items-center gap-2 rounded-ui border pl-[11px] pr-[13px] text-[13px] font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-40 ${
                          on
                            ? "border-white/50 bg-white/[0.12] text-white"
                            : "border-white/[0.12] text-white/65 hover:border-white/25 hover:text-white disabled:hover:border-white/[0.12] disabled:hover:text-white/65"
                        }`}
                      >
                        <Icon size={14} />
                        {track.label.replace(" only", "")}
                        {on && <Check size={13} />}
                      </button>
                    </Hint>
                  );
                })}
              </div>
            </div>
            {!config.perAppAudio && <Note>{t("settings.overlay.perAppNeedsWindows11")}</Note>}
          </>
        )}
      </SettingsGroup>

      <SettingsGroup label={t("settings.groups.files")}>
        {config && (
          <SettingCard
            icon={<FolderOpen size={18} />}
            title={t("settings.overlay.capturesFolder")}
            description={config.captureFolder}
            control={
              <div className="flex gap-2">
                <ActionButton onClick={() => void openCaptureFolder()}>
                  {t("common.open")}
                </ActionButton>
                {!config.captureFolderIsDefault && (
                  <ActionButton onClick={() => void resetCaptureFolder().then(load)}>
                    {t("common.reset")}
                  </ActionButton>
                )}
                <ActionButton
                  onClick={() =>
                    void chooseCaptureFolder().then((picked) => {
                      if (picked) {
                        void load();
                        push({
                          type: "success",
                          title: t("settings.overlay.capturesFolderChanged"),
                          text: picked,
                        });
                      }
                    })
                  }
                >
                  {t("settings.overlay.change")}
                </ActionButton>
              </div>
            }
          />
        )}
        {failed && (
          <SettingCard
            icon={<Info size={18} />}
            title={t("settings.overlay.readFailedTitle")}
            description={t("settings.overlay.readFailedDescription")}
          />
        )}
      </SettingsGroup>
    </div>
  );
}
