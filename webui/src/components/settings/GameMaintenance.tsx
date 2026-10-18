// ------------ Game Maintenance ------------
// The per-game part of Settings, Games: updates, repair, folders and removal, the voice packs.
import { useEffect, useState, useCallback, useRef } from "react";
import {
  Wrench,
  FolderInput,
  Trash2,
  Cpu,
  Layers,
  RefreshCw,
  TerminalSquare,
  Gamepad2,
  Languages,
  Rocket,
  Power,
  CalendarClock,
  ListChecks,
  Volume2,
} from "lucide-react";
import { type GameId, type GraphicsApi, type ResourceQuality, GRAPHICS_APIS, supportsQuickRepair } from "../../data/games";
import {
  jobActiveBlocker,
  qualityLabel,
  useGameActions,
  useModsForceDx11,
  useResourceQuality,
} from "../../lib/gameActions";
import { fmtBytes } from "../../lib/format";
import { useCustomizationStore } from "../../store/customizationStore";
import { useNotificationStore } from "../../store/notifications";
import { useGamesStore } from "../../store/gamesStore";
import { verifyNeedsRepairFirst } from "../../lib/download";
import { SettingsGroup, SettingCard } from "../ui/panels";
import { SettingsExpander } from "./SettingRow";
import {
  repairGame,
  verifyGameIntegrity,
  getGameConfig,
  setCustomLauncher,
  setConfigValue,
  defaultGraphicsApi,
  RESOURCE_QUALITY_OPTIONS,
  downloadEnqueue,
  DEFAULT_VOICE_PACK,
  VOICE_PACK_OPTIONS,
  type GameUpdatePrefs,
  type VoicePackLanguage,
  setContentPacks,
  type ContentPack,
} from "../../lib/ipc";
import { ActionButton, Toggle, SegmentedControl } from "../ui/controls";
import { Hint } from "../ui/Tooltip";
import { fetchRemembered, reads, recall, rememberAnswer, useRemembered } from "../../lib/launcherHooks";
import { t, type TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";

// ------------ Maintenance ------------
// The per-game section of Settings, Games. Covers auto-update and update checks, repair and verify, opening
// folders, the install location, and moving or removing the game.
type GameConfig = Awaited<ReturnType<typeof getGameConfig>>;

const SCHEDULE_OPTS: { value: string; label: TKey; check?: TKey }[] = [
  { value: "off", label: "maintenance.schedule.never" },
  {
    value: "daily",
    label: "maintenance.schedule.daily",
    check: "maintenance.autoUpdateSummary.dailyCheck",
  },
  {
    value: "weekly",
    label: "maintenance.schedule.weekly",
    check: "maintenance.autoUpdateSummary.weeklyCheck",
  },
];

function qualitySizes(sizes: ReturnType<typeof useResourceQuality>["sizes"]): string {
  if (!sizes) return "";
  const parts = RESOURCE_QUALITY_OPTIONS.flatMap(({ value }) => {
    const size = sizes[value];
    return size
      ? [t("maintenance.qualitySize", { quality: qualityLabel(value), size: fmtBytes(size.installBytes) })]
      : [];
  });
  return parts.length ? ` ${parts.join(" · ")}.` : "";
}

const NO_PREFS: GameUpdatePrefs = {
  autoUpdate: false,
  autoUpdateOnStartup: false,
  autoUpdateSchedule: "off",
};

export function GameMaintenance({ gameId: activeId }: { gameId: GameId }) {
  const t = useT();
  const {
    game,
    managed,
    installed,
    partial,
    running,
    jobActive,
    steam,
    setSteam,
    steamCopy,
    uninstallBlocker,
    quickRepair,
    confirmFullRepair,
    locate,
    confirmMove,
    confirmUninstall,
  } = useGameActions(activeId);
  const modsForceDx11 = useModsForceDx11(activeId);
  const api = useCustomizationStore((s) => s.graphicsApi[activeId]) ?? defaultGraphicsApi(activeId);
  const setGraphicsApi = useCustomizationStore((s) => s.setGraphicsApi);
  const resourceQuality = useResourceQuality(activeId, { installed, jobActive }, { sizes: true });
  const voicePack = useCustomizationStore((s) => s.voicePack[activeId]) ?? DEFAULT_VOICE_PACK;
  const setVoicePack = useCustomizationStore((s) => s.setVoicePack);
  const push = useNotificationStore((s) => s.push);
  const toast = useNotificationStore((s) => s.toast);

  const maintainable = installed && managed;
  const moveBlocker = jobActive
    ? jobActiveBlocker()
    : running && installed && !steamCopy
      ? t("maintenance.closeGameFirst", { game: game.name })
      : null;
  const quickRepairable = supportsQuickRepair(activeId);

  const [healthTick, setHealthTick] = useState(0);
  const health = useRemembered(
    installed ? reads.installPathHealth(activeId) : null,
    [activeId, installed, jobActive, healthTick],
  );
  const pathProblem =
    health && (health.missing || health.tooDeep || health.notWritable) ? health.message : "";
  const installPath = installed ? (health?.path ?? "") : "";

  const [loadedPrefs, setLoadedPrefs] = useState<{ gameId: GameId; prefs: GameUpdatePrefs } | null>(() => {
    const cfg = recall(reads.gameConfig(activeId));
    return cfg ? { gameId: activeId, prefs: cfg.prefs } : null;
  });
  const prefsReady = loadedPrefs?.gameId === activeId;
  const prefs = loadedPrefs?.prefs ?? NO_PREFS;
  const [launchArgs, setLaunchArgs] = useState(() => recall(reads.gameConfig(activeId))?.launchArgs ?? "");
  const [savedArgs, setSavedArgs] = useState(launchArgs);
  const [customLauncher, setCustomLauncherPath] = useState(
    () => recall(reads.gameConfig(activeId))?.customLauncher ?? "",
  );
  useEffect(() => {
    let alive = true;
    void fetchRemembered(reads.gameConfig(activeId)).then((cfg) => {
      if (!alive) return;
      setLoadedPrefs({ gameId: activeId, prefs: cfg.prefs });
      setLaunchArgs(cfg.launchArgs);
      setSavedArgs(cfg.launchArgs);
      setCustomLauncherPath(cfg.customLauncher);
    });
    return () => {
      alive = false;
    };
  }, [activeId]);

  const rememberConfig = (change: (cfg: GameConfig) => GameConfig) => {
    const read = reads.gameConfig(activeId);
    const cfg = recall(read);
    if (cfg) rememberAnswer(read, change(cfg));
  };

  const updatePref = (key: keyof GameUpdatePrefs, value: boolean | string) => {
    if (!prefsReady) return;
    setLoadedPrefs((p) =>
      p && p.gameId === activeId ? { ...p, prefs: { ...p.prefs, [key]: value } } : p,
    );
    rememberConfig((cfg) => ({ ...cfg, prefs: { ...cfg.prefs, [key]: value } }));
    void setConfigValue(`games.${activeId}.${key}`, value);
  };

  const chooseCustomLauncher = (pick: boolean) => {
    void setCustomLauncher(activeId, pick).then((path) => {
      if (path === undefined) return;
      setCustomLauncherPath(path);
      rememberConfig((cfg) => ({ ...cfg, customLauncher: path }));
    });
  };

  const appliedVoices = useRemembered(
    installed && managed && game.voicePackChoice && !jobActive ? reads.appliedVoicePacks(activeId) : null,
    [activeId, installed, managed, game.voicePackChoice, jobActive],
  );

  const voicePending = !!appliedVoices && appliedVoices.length > 0 && !appliedVoices.includes(voicePack);

  const applyVoicePack = () => {
    const label = VOICE_PACK_OPTIONS.find((o) => o.value === voicePack)?.label ?? voicePack;
    void downloadEnqueue(activeId, () =>
      push({
        type: "progress",
        title: t("maintenance.voicePack.applyingTitle", { language: label }),
        text: t("maintenance.voicePack.applyingText"),
      }),
    );
  };

  const setLaunchViaSteam = (on: boolean) => {
    if (steam) setSteam({ ...steam, launchViaSteam: on });
    void setConfigValue(`games.${activeId}.launchViaSteam`, on);
  };

  const saveLaunchArgs = () => {
    const trimmed = launchArgs.trim();
    if (trimmed === savedArgs) return;
    setSavedArgs(trimmed);
    rememberConfig((cfg) => ({ ...cfg, launchArgs: trimmed }));
    void setConfigValue(`games.${activeId}.launchArgs`, trimmed);
  };

  const verifyIntegrity = async () => {
    const result = await verifyGameIntegrity(activeId, () =>
      toast({
        type: "progress",
        title: t("maintenance.verify.checkingTitle", { game: game.name }),
        text: t("maintenance.verify.checkingText"),
      }),
    );
    if (!result) return;
    const broken = result.brokenFiles;
    if (broken === 0) {
      push({
        type: "success",
        title: t("maintenance.verify.intactTitle", { game: game.name }),
        text: result.message ?? t("maintenance.verify.intactText"),
      });
      return;
    }
    const summary = result.message ?? t("maintenance.verify.brokenSummary", { count: broken });
    const updateReplaces =
      result.updatePending ?? useGamesStore.getState().updates.includes(activeId);
    if (updateReplaces && !verifyNeedsRepairFirst(result.message)) {
      push({
        type: "info",
        title: t("maintenance.verify.updateTitle", { game: game.name }),
        text:
          (result.updatePending && result.message) ||
          t("maintenance.verify.updateText", { count: broken }),
        action: {
          label: t("common.update"),
          run: () =>
            void downloadEnqueue(activeId, () =>
              push({
                type: "progress",
                title: t("maintenance.verify.updatingTitle", { game: game.name }),
                text: t("maintenance.verify.trackOnDownloads"),
              }),
            ),
        },
      });
      return;
    }
    push({
      type: "warning",
      title: t("maintenance.verify.brokenTitle", { game: game.name }),
      text: summary,
      action: {
        label: t("maintenance.verify.fullRepair"),
        run: () =>
          void repairGame(activeId, false).then((queued) => {
            if (!queued) return;
            push({
              type: "progress",
              title: t("maintenance.verify.repairingTitle", { game: game.name }),
              text: t("maintenance.verify.repairingText"),
            });
          }),
      },
    });
  };

  const scheduleCheck = SCHEDULE_OPTS.find((o) => o.value === prefs.autoUpdateSchedule)?.check;
  const scheduleOptions = SCHEDULE_OPTS.map((o) => ({ value: o.value, label: t(o.label) }));
  const autoUpdateSummary = !installed
    ? t("maintenance.autoUpdateSummary.installFirst")
    : !managed
      ? t("maintenance.autoUpdateSummary.ownLauncher")
      : !loadedPrefs
        ? "\u00a0"
        : !prefs.autoUpdate
          ? t("maintenance.autoUpdateSummary.off")
          : [
              prefs.autoUpdateOnStartup ? t("maintenance.autoUpdateSummary.checksOnStartup") : null,
              prefs.autoUpdateSchedule !== "off" && scheduleCheck ? t(scheduleCheck) : null,
            ]
              .filter(Boolean)
              .join(" · ") || t("maintenance.autoUpdateSummary.on");

  return (
    <div className="flex flex-col gap-7">
      <SettingsGroup label={t("maintenance.groups.maintenance")}>
        <SettingsExpander
          icon={<RefreshCw size={18} />}
          title={t("maintenance.autoUpdate.title", { game: game.name })}
          summary={autoUpdateSummary}
          description={t("maintenance.autoUpdate.description")}
          disabled={!maintainable}
          control={
            <Toggle
              checked={prefs.autoUpdate && maintainable}
              disabled={!maintainable || !prefsReady}
              ariaLabel={t("maintenance.autoUpdate.ariaLabel")}
              onChange={(v) => updatePref("autoUpdate", v)}
            />
          }
        >
          <SettingCard
            icon={<Power size={18} />}
            title={t("maintenance.checkOnStartup.title")}
            description={t("maintenance.checkOnStartup.description")}
            disabled={!prefs.autoUpdate || !prefsReady}
            control={
              <Toggle
                checked={prefs.autoUpdateOnStartup}
                disabled={!prefs.autoUpdate || !prefsReady}
                ariaLabel={t("maintenance.checkOnStartup.title")}
                onChange={(v) => updatePref("autoUpdateOnStartup", v)}
              />
            }
          />
          <SettingCard
            icon={<CalendarClock size={18} />}
            title={t("maintenance.scheduledCheck.title")}
            description={t("maintenance.scheduledCheck.description")}
            disabled={!prefs.autoUpdate || !prefsReady}
            control={
              <SegmentedControl
                ariaLabel={t("maintenance.scheduledCheck.title")}
                options={scheduleOptions}
                value={prefs.autoUpdateSchedule}
                onChange={(v) => updatePref("autoUpdateSchedule", v)}
              />
            }
          />
        </SettingsExpander>

        <SettingCard
          icon={<Wrench size={18} />}
          title={t("maintenance.repair.title")}
          description={
            maintainable && jobActive ? jobActiveBlocker() : t("maintenance.repair.description")
          }
          disabled={!maintainable}
          control={
            <div className="flex gap-2">
              {quickRepairable && (
                <Hint tip={t("maintenance.repair.quickTip")}>
                  <ActionButton onClick={quickRepair} disabled={!maintainable || jobActive}>
                    {t("maintenance.repair.quick")}
                  </ActionButton>
                </Hint>
              )}
              <Hint tip={t("maintenance.repair.fullTip")}>
                <ActionButton
                  onClick={confirmFullRepair}
                  disabled={!maintainable || jobActive}
                >
                  {t("maintenance.repair.full")}
                </ActionButton>
              </Hint>
              <Hint tip={t("maintenance.repair.verifyTip")}>
                <ActionButton
                  onClick={() => void verifyIntegrity()}
                  disabled={!maintainable || jobActive}
                >
                  {t("maintenance.repair.verify")}
                </ActionButton>
              </Hint>
            </div>
          }
        />

        <SettingCard
          danger={!!pathProblem && !moveBlocker}
          icon={<FolderInput size={18} />}
          title={t("maintenance.location.title")}
          description={
            moveBlocker ||
            pathProblem ||
            (installed && steamCopy
              ? t("maintenance.location.steamCopy")
              : t("maintenance.location.description"))
          }
          detail={
            installPath && (
              <Hint tip={installPath} className="mt-1 block max-w-[56ch] truncate font-mono text-[11.5px] text-white/55">
                {installPath}
              </Hint>
            )
          }
          control={
            <div className="flex gap-2">
              <ActionButton
                onClick={confirmMove}
                disabled={!installed || steamCopy || !!moveBlocker || !!health?.missing}
              >
                {t("maintenance.location.move")}
              </ActionButton>
              <ActionButton
                onClick={() => void locate().then(() => setHealthTick((t) => t + 1))}
                disabled={jobActive}
              >
                {t("maintenance.location.locate")}
              </ActionButton>
            </div>
          }
        />

        {game.voicePackChoice && (
          <SettingCard
            icon={<Languages size={18} />}
            title={t("maintenance.voiceLanguage.title")}
            description={
              voicePending
                ? t("maintenance.voiceLanguage.pending")
                : t("maintenance.voiceLanguage.description")
            }
            disabled={!installed}
            control={
              <div className="flex items-center gap-2">
                {voicePending && (
                  <ActionButton onClick={applyVoicePack} disabled={jobActive || running}>
                    {t("maintenance.voiceLanguage.applyNow")}
                  </ActionButton>
                )}
                <SegmentedControl
                  ariaLabel={t("maintenance.voiceLanguage.title")}
                  options={VOICE_PACK_OPTIONS}
                  value={voicePack}
                  onChange={(v) => setVoicePack(activeId, v as VoicePackLanguage)}
                />
              </div>
            }
          />
        )}

        <VoicePackPanel gameId={activeId} />
      </SettingsGroup>

      <SettingsGroup label={t("maintenance.groups.launchOptions")}>
        {game.graphicsApiChoice && (
          <SettingCard
            icon={<Cpu size={18} />}
            title="DirectX" // i18n-ignore
            description={
              customLauncher
                ? t("maintenance.customLauncherOverrides")
                : modsForceDx11
                  ? t("maintenance.directx.modsForceDx11", { game: game.name })
                  : t("maintenance.directx.description")
            }
            disabled={!installed || modsForceDx11 || !!customLauncher}
            control={
              <SegmentedControl
                ariaLabel={t("maintenance.directx.ariaLabel")}
                options={GRAPHICS_APIS}
                value={modsForceDx11 ? "dx11" : api}
                onChange={(v) => setGraphicsApi(activeId, v as GraphicsApi)}
              />
            }
          />
        )}
        {game.resourceQualityChoice && (
          <SettingCard
            icon={<Layers size={18} />}
            title={t("maintenance.resourceQuality.title")}
            description={
              customLauncher
                ? t("maintenance.customLauncherOverrides")
                : resourceQuality.pending
                  ? t("maintenance.resourceQuality.pending", {
                      installed: resourceQuality.installedLabel,
                      quality: qualityLabel(resourceQuality.quality),
                    })
                  : (installed
                      ? t("maintenance.resourceQuality.description")
                      : t("maintenance.resourceQuality.descriptionNotInstalled")) +
                    qualitySizes(resourceQuality.sizes)
            }
            disabled={!!customLauncher}
            control={
              <div className="flex items-center gap-2">
                {resourceQuality.pending && (
                  <ActionButton onClick={resourceQuality.apply} disabled={jobActive || running}>
                    {t("maintenance.voiceLanguage.applyNow")}
                  </ActionButton>
                )}
                {installed && (
                  <Hint tip={t("maintenance.resourceQuality.checkTip")}>
                    <ActionButton
                      onClick={() => void resourceQuality.check()}
                      disabled={jobActive || running || resourceQuality.checking}
                    >
                      {t("maintenance.resourceQuality.checkButton")}
                    </ActionButton>
                  </Hint>
                )}
                <SegmentedControl
                  ariaLabel={t("maintenance.resourceQuality.title")}
                  options={RESOURCE_QUALITY_OPTIONS}
                  value={resourceQuality.quality}
                  onChange={(v) => resourceQuality.setQuality(v as ResourceQuality)}
                />
              </div>
            }
          />
        )}
        {steam?.isSteamInstall && (
          <SettingCard
            icon={<Gamepad2 size={18} />}
            title={t("maintenance.steam.title")}
            description={
              steam.steamFound
                ? t("maintenance.steam.found", { game: game.name })
                : t("maintenance.steam.notFound", { game: game.name })
            }
            disabled={!steam.steamFound}
            control={
              <Toggle
                checked={steam.launchViaSteam && steam.steamFound}
                disabled={!steam.steamFound}
                ariaLabel={t("maintenance.steam.title")}
                onChange={setLaunchViaSteam}
              />
            }
          />
        )}
        <SettingCard
          icon={<TerminalSquare size={18} />}
          title={t("maintenance.launchArgs.title")}
          description={
            customLauncher
              ? t("maintenance.customLauncherOverrides")
              : t("maintenance.launchArgs.description")
          }
          disabled={!!customLauncher}
          control={
            <input
              type="text"
              value={launchArgs}
              disabled={!!customLauncher}
              spellCheck={false}
              placeholder={t("maintenance.launchArgs.placeholder")}
              aria-label={t("maintenance.launchArgs.title")}
              onChange={(e) => setLaunchArgs(e.target.value)}
              onBlur={saveLaunchArgs}
              onKeyDown={(e) => {
                if (e.key === "Enter") saveLaunchArgs();
              }}
              className="w-[220px] shrink-0 rounded-ui border border-white/[0.08] bg-black/30 px-[12px] py-[8px] text-[12.5px] text-white/85 placeholder:text-white/30 focus:border-(--accent-a)"
            />
          }
        />
        <SettingCard
          icon={<Rocket size={18} />}
          title={t("maintenance.customLauncher.title")}
          description={
            customLauncher
              ? t("maintenance.customLauncher.chosen", { path: customLauncher })
              : t("maintenance.customLauncher.description")
          }
          control={
            <div className="flex items-center gap-2">
              <ActionButton onClick={() => chooseCustomLauncher(true)}>
                {customLauncher ? t("maintenance.customLauncher.change") : t("maintenance.customLauncher.choose")}
              </ActionButton>
              <ActionButton disabled={!customLauncher} onClick={() => chooseCustomLauncher(false)}>
                {t("maintenance.customLauncher.clear")}
              </ActionButton>
            </div>
          }
        />
      </SettingsGroup>

      <SettingsGroup label={t("maintenance.groups.dangerZone")} danger>
        <SettingCard
          icon={<Trash2 size={18} />}
          title={partial ? t("maintenance.uninstall.discardTitle") : t("maintenance.uninstall.uninstallTitle")}
          description={
            ((installed || partial) && uninstallBlocker) ||
            (partial
              ? t("maintenance.uninstall.partial")
              : steamCopy
                ? t("maintenance.uninstall.steamCopy")
                : t("maintenance.uninstall.description"))
          }
          danger
          control={
            <ActionButton
              variant="danger"
              onClick={() => void confirmUninstall()}
              disabled={(!installed && !partial) || !!uninstallBlocker}
            >
              {partial ? t("maintenance.uninstall.discard") : t("common.uninstall")}
            </ActionButton>
          }
        />
      </SettingsGroup>
    </div>
  );
}

// ------------ Voice Packs ------------
// Lets you pick which spoken languages to install for a game. Only appears for games that offer voice packs. A
// newly picked pack downloads on the next update, or right away if the game is installed.
const knownSupport = new Map<string, boolean>();

const knownPacks = (gameId: string) => {
  const list = recall(reads.contentPacks(gameId));
  return list && !list.error ? list : null;
};

function VoicePackPanel({ gameId }: { gameId: string }) {
  const t = useT();
  const [packs, setPacks] = useState<ContentPack[] | null>(() => {
    const known = knownPacks(gameId);
    return known?.supported ? known.packs : null;
  });
  const [loading, setLoading] = useState(() => !knownPacks(gameId));
  const [failed, setFailed] = useState(false);
  const [busy, setBusy] = useState(false);
  const seq = useRef(0);

  const load = useCallback(() => {
    const id = ++seq.current;
    const read = reads.contentPacks(gameId);
    const known = knownPacks(gameId);
    setPacks(known?.supported ? known.packs : null);
    setLoading(!known);
    setFailed(false);
    void fetchRemembered(read).then((r) => {
      if (id !== seq.current) return;
      knownSupport.set(gameId, r.supported);
      setPacks(r.supported && !r.error ? r.packs : null);
      setFailed(r.error);
      setLoading(false);
    });
  }, [gameId]);

  useEffect(() => {
    const s = seq;
    load();
    return () => {
      s.current++;
    };
  }, [load]);

  if (loading) {
    if (!knownSupport.get(gameId)) return null;
    return (
      <SettingCard
        icon={<Languages size={18} />}
        title={t("common.voicePacks")}
        description={t("settings.voicePacks.loading")}
      />
    );
  }

  if (failed) {
    return (
      <SettingCard
        icon={<Languages size={18} />}
        title={t("common.voicePacks")}
        description={t("settings.voicePacks.loadFailed")}
        control={
          <ActionButton onClick={load} icon={<RefreshCw size={13} />}>
            {t("common.retry")}
          </ActionButton>
        }
      />
    );
  }

  if (!packs || packs.length === 0) return null;

  const selectedCount = packs.filter((p) => p.selected).length;

  const apply = async (next: ContentPack[]) => {
    const prev = packs;
    setPacks(next);
    setBusy(true);
    try {
      const chosen = next.filter((p) => p.selected).map((p) => p.tag);
      const saved = await setContentPacks(gameId, chosen.length === next.length ? null : chosen);
      if (saved) rememberAnswer(reads.contentPacks(gameId), { supported: true, packs: next, error: false });
      else setPacks(prev);
    } finally {
      setBusy(false);
    }
  };

  const toggle = (tag: string, on: boolean) =>
    void apply(packs.map((p) => (p.tag === tag ? { ...p, selected: on } : p)));

  const total = packs.filter((p) => p.selected).reduce((sum, p) => sum + p.bytes, 0);

  return (
    <SettingsExpander
      icon={<Languages size={18} />}
      title={t("common.voicePacks")}
      summary={
        selectedCount === 0
          ? t("settings.voicePacks.noneSelected")
          : t("settings.voicePacks.selectedSummary", {
              selected: selectedCount,
              total: packs.length,
              size: fmtBytes(total),
            })
      }
      description={t("settings.voicePacks.description")}
      control={
        <ActionButton
          onClick={() => void apply(packs.map((p) => ({ ...p, selected: true })))}
          disabled={busy || selectedCount === packs.length}
          icon={<ListChecks size={13} />}
        >
          {t("settings.voicePacks.selectAll")}
        </ActionButton>
      }
    >
      {packs.map((pack) => (
        <SettingCard
          key={pack.tag}
          icon={<Volume2 size={18} />}
          title={pack.language ?? pack.tag}
          description={
            pack.language
              ? t("settings.voicePacks.packDetailsTagged", {
                  tag: pack.tag,
                  size: fmtBytes(pack.bytes),
                  count: pack.files,
                })
              : t("settings.voicePacks.packDetails", { size: fmtBytes(pack.bytes), count: pack.files })
          }
          control={
            <Toggle
              checked={pack.selected}
              disabled={busy}
              ariaLabel={pack.language ?? pack.tag}
              onChange={(v) => toggle(pack.tag, v)}
            />
          }
        />
      ))}
    </SettingsExpander>
  );
}
