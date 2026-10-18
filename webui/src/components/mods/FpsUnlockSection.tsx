// ------------ FPS Unlock Section ------------
// The frame rate unlock settings on the Mods page.
import { useEffect, useState } from "react";
import { Gauge, MonitorOff, Target } from "lucide-react";
import {
  setFpsUnlock,
  onFpsUnlockStatus,
  onGameStopped,
  type FpsUnlock,
  type FpsUnlockPatch,
  type FpsUnlockState,
} from "../../lib/ipc";
import { fetchRemembered, reads, recall, rememberAnswer } from "../../lib/launcherHooks";
import { type TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { Collapse } from "../../lib/motion";
import { useTauriEvent } from "../../lib/hooks";
import { Slider } from "../ui/Slider";
import { GroupBox, GroupLabel, SettingCard } from "../ui/panels";
import { SegmentedControl, Toggle } from "../ui/controls";

// ------------ FPS Unlock ------------
// The frame rate unlock settings for games that cap at 60. Has the on/off switch, a target frame rate, a lower
// cap while tabbed out and a live status while the game starts. Shown on the Mods page for games that support it.
const MIN_FPS = 30;
const MAX_FPS = 360;
const FPS_STEP = 10;
const FPS_TICKS = [String(MIN_FPS), String(MAX_FPS)];

const BACKGROUND_PRESETS = ["off", "10", "30", "60"];

const STATUS: Partial<Record<FpsUnlockState, { label: TKey; tone: "accent" | "danger" }>> = {
  launching: { label: "settings.fpsUnlock.status.launching", tone: "accent" },
  attaching: { label: "settings.fpsUnlock.status.attaching", tone: "accent" },
  locating: { label: "settings.fpsUnlock.status.locating", tone: "accent" },
  ready: { label: "settings.fpsUnlock.status.ready", tone: "accent" },
  failed: { label: "common.failed", tone: "danger" },
};

function StatusPill({ state }: { state: FpsUnlockState }) {
  const t = useT();
  const status = STATUS[state];
  if (!status) return null;
  const danger = status.tone === "danger";
  return (
    <span
      className="rounded-full px-[8px] py-[3px] text-[10px] font-medium uppercase tracking-[0.08em]"
      style={
        danger
          ? {
              background: "rgba(239,68,68,0.14)",
              color: "#fca5a5",
              border: "1px solid rgba(239,68,68,0.28)",
            }
          : {
              background: "rgba(var(--accent-b-rgb), 0.14)",
              color: "var(--accent-text)",
              border: "1px solid rgba(var(--accent-b-rgb), 0.28)",
            }
      }
    >
      {t(status.label)}
    </span>
  );
}

export function FpsUnlockSection({ gameId, gameName }: { gameId: string; gameName: string }) {
  const t = useT();
  const [loaded, setLoaded] = useState<{ gameId: string; config: FpsUnlock } | null>(null);
  const shown = loaded?.gameId === gameId ? loaded.config : recall(reads.fpsUnlock(gameId));
  const ready = shown !== null;

  const setConfig = (change: (current: FpsUnlock) => FpsUnlock) =>
    setLoaded((current) => {
      const base = current?.gameId === gameId ? current.config : recall(reads.fpsUnlock(gameId));
      return base ? { gameId, config: change(base) } : current;
    });

  useEffect(() => {
    let alive = true;
    void fetchRemembered(reads.fpsUnlock(gameId)).then((next) => {
      if (alive) setLoaded({ gameId, config: next });
    });
    return () => {
      alive = false;
    };
  }, [gameId]);

  useTauriEvent(onFpsUnlockStatus, (status) => {
    if (status.gameId !== gameId) return;
    const attached = status.state !== "idle" && status.state !== "failed";
    setConfig((current) => ({
      ...current,
      session: { state: status.state, error: status.error },
      appliesNextLaunch: attached ? false : current.appliesNextLaunch,
    }));
  });

  useTauriEvent(onGameStopped, (id) => {
    if (id !== gameId) return;
    setConfig((current) => ({ ...current, appliesNextLaunch: false }));
  });

  if (ready && !shown.supported) return null;
  const config = shown?.supported ? shown : null;
  const enabled = !!config?.enabled;

  const update = (patch: FpsUnlockPatch) => {
    setConfig((current) => ({ ...current, ...patch }));
    if (shown) rememberAnswer(reads.fpsUnlock(gameId), { ...shown, ...patch });
    void setFpsUnlock(gameId, patch).then((appliesNextLaunch) => {
      if (appliesNextLaunch === undefined) return;
      setConfig((current) => ({ ...current, appliesNextLaunch }));
    });
  };

  const setTargetFps = (value: number) => {
    const clamped = Math.min(Math.max(value, MIN_FPS), MAX_FPS);
    if (clamped !== config?.targetFps) update({ targetFps: clamped });
  };

  const failure =
    ready && config?.session.state === "failed" ? config.session.error : null;

  return (
    <section className="mt-6">
      <GroupLabel label={t("settings.groups.fpsUnlock")} />
      <GroupBox>
        <SettingCard
          icon={<Gauge size={18} />}
          title={t("settings.fpsUnlock.title")}
          badge={ready && config ? <StatusPill state={config.session.state} /> : undefined}
          description={t("settings.fpsUnlock.description", { game: gameName })}
          control={
            <Toggle
              checked={enabled}
              disabled={!ready}
              ariaLabel={t("settings.fpsUnlock.title")}
              onChange={(on) => update({ enabled: on })}
            />
          }
        />

        <Collapse open={!!config && enabled}>
          {config && (
            <div className="bg-black/[0.14]">
              {failure && (
                <div className="px-5 py-3">
                  <p className="text-[12.5px] leading-snug text-danger-soft">{failure}</p>
                </div>
              )}

              {ready && config.appliesNextLaunch && (
                <div className="px-5 py-3">
                  <p className="text-[12.5px] leading-snug text-white/55">
                    {t("settings.fpsUnlock.nextLaunch", { game: gameName })}
                  </p>
                </div>
              )}

              <SettingCard
                icon={<Target size={18} />}
                title={t("settings.fpsUnlock.capTitle")}
                description={t("settings.fpsUnlock.capDescription")}
                disabled={!ready}
                control={
                  <Slider
                    min={MIN_FPS}
                    max={MAX_FPS}
                    step={FPS_STEP}
                    value={config.targetFps}
                    onChange={setTargetFps}
                    unit={t("settings.fpsUnlock.fpsUnit")}
                    ticks={FPS_TICKS}
                    ariaLabel={t("settings.fpsUnlock.capTitle")}
                  />
                }
              />

              <SettingCard
                icon={<MonitorOff size={18} />}
                title={t("settings.fpsUnlock.tabbedOut")}
                description={t("settings.fpsUnlock.tabbedOutDescription")}
                disabled={!ready}
                control={
                  <SegmentedControl
                    ariaLabel={t("settings.fpsUnlock.tabbedOutAria")}
                    options={BACKGROUND_PRESETS.map((value) => ({
                      value,
                      label: value === "off" ? t("settings.fpsUnlock.noCap") : value,
                    }))}
                    value={config.powerSave ? String(config.backgroundFps) : "off"}
                    onChange={(value) =>
                      value === "off"
                        ? update({ powerSave: false })
                        : update({ powerSave: true, backgroundFps: Number(value) })
                    }
                  />
                }
              />
            </div>
          )}
        </Collapse>
      </GroupBox>
    </section>
  );
}
