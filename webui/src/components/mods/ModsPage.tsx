// ------------ Mods Page ------------
// The Mods tab, with the mod support switch at its top.
import { useCallback, useEffect, useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import {
  AlertTriangle,
  Eye,
  FolderCog,
  FolderOpen,
  Lock,
  Puzzle,
  RefreshCw,
  Settings2,
  Timer,
} from "lucide-react";
import { fadeVariants } from "../../lib/motion";
import { useModsStore, useModProfilesStore } from "../../store/mods";
import { MOD_RISK_WARNING } from "../../lib/mods";
import { useT } from "../../lib/useT";
import { gameById, type GameId } from "../../data/games";
import { MODS_SETTINGS } from "../../data/settings";
import { useUiStore } from "../../store/uiStore";
import { useSessionUiStore } from "../../store/sessionUiStore";
import { useNotificationStore } from "../../store/notifications";
import { useModalStore } from "../../store/modalStore";
import {
  chooseModsPath,
  getLoaderSettings,
  openModsFolder,
  resetModsPath,
  setConfigValue,
  setGameModsEnabled,
  setLoaderSetting,
  type LoaderSetting,
} from "../../lib/ipc";
import { NumberField, SettingRow, SettingsExpander } from "../settings/SettingRow";
import { GamePickerGrid } from "../ui/GamePickerGrid";
import { GlassPage } from "../ui/GlassPage";
import { GroupBox, GroupLabel, SettingCard } from "../ui/panels";
import { ActionButton, SegmentedControl, Toggle } from "../ui/controls";
import { ModsHub, type BrowseSeed } from "./ModsHub";
import { BrowseMods } from "./BrowseMods";
import { ModsOptIn } from "./ModsOptIn";

// ------------ Page ------------
// The Mods tab. Shows the mod support switch, the game picker and setup for the mod tools (XXMI, the loader
// Peebify uses), then switches between the Library and Browse views. The mod support switch and the NSFW toggle
// live here too, since Settings has no Mods tab.
export function ModsPage() {
  const t = useT();
  const activeId = useUiStore((s) => s.activeGameId);
  const push = useNotificationStore((s) => s.push);
  const openConfirm = useModalStore((s) => s.openConfirm);
  const status = useModsStore((s) => s.status);
  const mods = useModsStore((s) => s.mods);
  const loadedGameId = useModsStore((s) => s.loadedGameId);
  const requestedGameId = useModsStore((s) => s.requestedGameId);
  const refresh = useModsStore((s) => s.refresh);
  const refreshIfCurrent = useModsStore((s) => s.refreshIfCurrent);
  const setMasterEnabled = useModsStore((s) => s.setMasterEnabled);

  const [shownId, setShownId] = useState(activeId);
  const game = gameById(shownId);
  const tab = useSessionUiStore((s) => s.modsTab);
  const setTab = useSessionUiStore((s) => s.setModsTab);
  const [rescanning, setRescanning] = useState(false);
  const [setupOpen, setSetupOpen] = useState<boolean | null>(null);
  const [browseSeed, setBrowseSeed] = useState<BrowseSeed | null>(null);
  const clearBrowseSeed = useCallback(() => setBrowseSeed(null), []);
  const [loader, setLoader] = useState<{ gameId: GameId; rows: LoaderSetting[] } | null>(null);
  const loaderSettings = loader?.gameId === shownId ? loader.rows : [];

  useEffect(() => {
    let cancelled = false;
    let swapped = false;
    const swap = () => {
      if (cancelled || swapped) return;
      swapped = true;
      setShownId(activeId);
      setSetupOpen(null);
    };
    void (async () => {
      try {
        const mods = await useModsStore.getState().prepare(activeId);
        const [commitProfiles, rows] = await Promise.all([
          useModProfilesStore.getState().prepare(activeId),
          mods.status.toolchainInstalled ? getLoaderSettings(activeId).then((r) => r.rows) : [],
        ]);
        if (cancelled) return;
        mods.commit();
        commitProfiles();
        setLoader({ gameId: activeId, rows });
        swap();
      } catch {
        if (!cancelled) void refresh(activeId);
      }
    })();
    const timer = window.setTimeout(swap, 2500);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [activeId, refresh]);

  const current = loadedGameId === shownId;

  useEffect(() => {
    const s = useModsStore.getState();
    if (s.loadedGameId !== null && s.loadedGameId !== activeId && s.requestedGameId !== activeId) {
      void refresh(activeId);
    }
  }, [loadedGameId, requestedGameId, activeId, refresh]);

  const toolsInstalled = status?.toolchainInstalled ?? false;

  useEffect(() => {
    if (!current) return;
    let cancelled = false;
    if (!toolsInstalled) {
      setLoader({ gameId: shownId, rows: [] });
      return;
    }
    void getLoaderSettings(shownId).then(({ rows }) => {
      if (!cancelled) setLoader({ gameId: shownId, rows });
    });
    return () => {
      cancelled = true;
    };
  }, [shownId, toolsInstalled, current]);

  const changeLoaderSetting = async (id: string, value: string) => {
    const gameId = shownId;
    if (!(await setLoaderSetting(gameId, id, value)).ok) return;
    const { rows } = await getLoaderSettings(gameId);
    setLoader((prev) => (prev?.gameId === gameId || prev === null ? { gameId, rows } : prev));
  };

  const master = status?.masterEnabled ?? false;
  const supported = status?.supportsMods ?? false;
  const gameOn = status?.gameEnabled ?? false;
  const modCapableIds = (status?.supportedGames ?? []).map((g) => g.id as GameId);
  const toolsInstalling = status?.toolsInstalling ?? false;
  const hasGameBanana = !!status?.supportedGames.find((g) => g.id === shownId)?.gameBananaId;
  const showSetup = setupOpen ?? !toolsInstalled;
  const library = tab === "installed" || !hasGameBanana;

  const variantLabel = status?.variant?.toUpperCase() ?? "XXMI";
  const versionLabel =
    status?.variant && status.versions[status.variant]
      ? `${variantLabel} ${status.versions[status.variant]}`
      : variantLabel;
  const setupSummary = !toolsInstalled
    ? t(toolsInstalling ? "mods.page.setup.summaryInstalling" : "mods.page.setup.summaryNotInstalled")
    : gameOn
      ? t("mods.page.setup.summaryOn", { version: versionLabel, game: game.name })
      : t("mods.page.setup.summaryOff", { version: versionLabel, game: game.name });

  const enableMaster = () =>
    void setConfigValue("behavior.modsEnabled", true).then(() => {
      setMasterEnabled(true);
      void refreshIfCurrent(activeId);
      push({ type: "success", title: t("mods.page.off.enabledTitle"), text: t("mods.page.off.enabledText") });
    });

  const toggleGame = async (on: boolean) => {
    await setGameModsEnabled(shownId, on);
    void refreshIfCurrent(shownId);
  };

  const rescanMods = async () => {
    if (rescanning) return;
    const gameId = shownId;
    setRescanning(true);
    try {
      await Promise.all([refresh(gameId), useModProfilesStore.getState().refresh(gameId)]);
    } finally {
      setRescanning(false);
    }
  };

  const announceFolder = async (path: string | undefined) => {
    if (!path) return;
    await refreshIfCurrent(activeId);
    const found = useModsStore.getState().status?.toolchainInstalled ?? false;
    push({
      title: t("mods.page.toasts.folderChangedTitle"),
      text: found
        ? t("mods.page.toasts.folderChangedFound", { path, variant: variantLabel })
        : t("mods.page.toasts.folderChangedMissing", { path, variant: variantLabel }),
    });
  };

  const oldFolder = status?.toolchainPath ?? "";

  const confirmChangeFolder = () =>
    openConfirm({
      title: t("mods.page.confirm.changeFolderTitle"),
      message: t("mods.page.confirm.changeFolderMessage", { folder: oldFolder }),
      confirmLabel: t("mods.page.confirm.changeFolderConfirm"),
      onConfirm: () => void chooseModsPath().then(announceFolder),
    });

  const confirmResetFolder = () =>
    openConfirm({
      title: t("mods.page.confirm.resetFolderTitle"),
      message: t("mods.page.confirm.resetFolderMessage", { folder: oldFolder }),
      confirmLabel: t("mods.page.confirm.resetFolderConfirm"),
      onConfirm: () => void resetModsPath().then(announceFolder),
    });

  return (
    <GlassPage title={t("common.mods")} subtitle={t("mods.page.subtitle")}>
      <AnimatePresence mode="wait" initial={false}>
      {!status ? null : !master ? (
        <m.div key="opt-in" custom={0.2} variants={fadeVariants} initial="initial" animate="animate" exit="exit">
          <ModsOptIn
            games={status.supportedGames.map((g) => ({ id: g.id as GameId, variant: g.variant }))}
            onTurnOn={enableMaster}
          />
        </m.div>
      ) : (
        <m.div key="mods" custom={0.2} variants={fadeVariants} initial="initial" animate="animate" exit="exit">
          <GamePickerGrid
            ids={modCapableIds.length ? modCapableIds : undefined}
            subtitleFor={(id) => {
              const g = status.supportedGames.find((s) => s.id === id);
              if (!g) return { text: t("mods.page.picker.noSupport"), tone: "off" };
              const variant = g.variant.toUpperCase();
              if (!g.installed) {
                const key = status.toolsInstalling ? "mods.page.picker.installing" : "mods.page.picker.notInstalled";
                return { text: t(key, { variant }), tone: "off" };
              }
              if (!g.enabled) return { text: t("mods.page.picker.ready", { variant }), tone: "off" };
              return { text: t("mods.page.picker.on", { variant }), tone: "on" };
            }}
          />

          <section>
            <GroupLabel label={t("settings.groups.modSupport")} />
            <GroupBox>
              <ModsPanel />
              {MODS_SETTINGS.map((s) => (
                <SettingRow key={s.id} setting={s} />
              ))}
              {!supported ? (
                <SettingCard
                  icon={<Puzzle size={18} />}
                  title={t("mods.page.unsupported.title", { game: game.name })}
                  description={t("mods.page.unsupported.noLoader")}
                />
              ) : (
                <SettingsExpander
                  icon={<Settings2 size={18} />}
                  title={t("mods.page.setup.title")}
                  summary={setupSummary}
                  open={showSetup}
                  onOpenChange={setSetupOpen}
                >
                  <SettingCard
                    icon={<Puzzle size={18} />}
                    title={t("mods.page.setup.useMods", { game: game.name })}
                    description={
                      !toolsInstalled
                        ? t("mods.page.setup.useModsNoTools")
                        : status.experimental
                          ? t("mods.page.setup.useModsExperimental")
                          : t("mods.page.setup.useModsDescription")
                    }
                    disabled={!toolsInstalled}
                    control={
                      <Toggle
                        checked={gameOn}
                        disabled={!toolsInstalled || !current}
                        ariaLabel={t("mods.page.setup.useMods", { game: game.name })}
                        onChange={(v) => void toggleGame(v)}
                      />
                    }
                  />
                  {status.forcesDx11 && gameOn && game.graphicsApiChoice && (
                    <SettingCard
                      icon={<Lock size={18} />}
                      title={t("mods.page.setup.dx11Title")}
                      description={t("mods.page.setup.dx11Description", { game: game.name })}
                    />
                  )}
                  <SettingCard
                    icon={<RefreshCw size={18} />}
                    title={t("mods.page.setup.rescanTitle")}
                    description={t("mods.page.setup.rescanDescription")}
                    control={
                      <ActionButton
                        icon={<RefreshCw size={15} className={rescanning ? "animate-spin" : ""} />}
                        disabled={rescanning || !current}
                        onClick={() => void rescanMods()}
                      >
                        {t("common.refresh")}
                      </ActionButton>
                    }
                  />
                  {loaderSettings
                    .filter((row) => row.kind === "bool" || row.kind === "millis")
                    .map((row) => (
                      <SettingCard
                        key={row.id}
                        icon={row.kind === "bool" ? <Eye size={18} /> : <Timer size={18} />}
                        title={row.label}
                        description={t("mods.page.setup.loaderSettingDescription", { description: row.description })}
                        control={
                          row.kind === "bool" ? (
                            <Toggle
                              checked={row.value === "1"}
                              disabled={!current}
                              ariaLabel={row.label}
                              onChange={(v) => void changeLoaderSetting(row.id, v ? "1" : "0")}
                            />
                          ) : (
                            <NumberField
                              value={row.value}
                              min={row.min ?? 0}
                              max={row.max ?? 10000}
                              unit={t("mods.page.setup.msUnit")}
                              zeroLabel="0"
                              disabled={!current}
                              ariaLabel={row.label}
                              onChange={(v) => void changeLoaderSetting(row.id, v)}
                            />
                          )
                        }
                      />
                    ))}
                  <SettingCard
                    icon={<FolderCog size={18} />}
                    title={t("mods.page.setup.folderTitle")}
                    description={
                      status.isDefault
                        ? t("mods.page.setup.folderDefault", { path: status.toolchainPath })
                        : status.toolchainPath
                    }
                    control={
                      <div className="flex items-center gap-2">
                        <ActionButton
                          icon={<FolderOpen size={15} />}
                          disabled={!toolsInstalled || !current}
                          onClick={() => void openModsFolder(shownId)}
                        >
                          {t("common.open")}
                        </ActionButton>
                        <ActionButton onClick={confirmChangeFolder}>{t("mods.page.setup.change")}</ActionButton>
                        <ActionButton
                          disabled={status.isDefault}
                          onClick={confirmResetFolder}
                        >
                          {t("common.reset")}
                        </ActionButton>
                      </div>
                    }
                  />
                </SettingsExpander>
              )}
            </GroupBox>
          </section>

          {supported && (
            <>
              <div className="mb-5 mt-6">
                <SegmentedControl
                  ariaLabel={t("mods.page.view.ariaLabel")}
                  options={[
                    {
                      value: "installed",
                      label: mods.length
                        ? t("mods.page.view.libraryCount", { count: mods.length })
                        : t("mods.page.view.library"),
                    },
                    ...(hasGameBanana ? [{ value: "browse", label: t("mods.page.view.browse") }] : []),
                  ]}
                  value={library ? "installed" : "browse"}
                  onChange={(v) => setTab(v as "installed" | "browse")}
                />
              </div>

              <AnimatePresence mode="wait" initial={false}>
                <m.div
                  key={library ? "library" : "browse"}
                  variants={fadeVariants}
                  initial="initial"
                  animate="animate"
                  exit="exit"
                >
                  {library ? (
                    <ModsHub
                      key={shownId}
                      gameId={shownId}
                      onBrowse={hasGameBanana ? () => setTab("browse") : undefined}
                      onOpenGameBanana={
                        hasGameBanana
                          ? (seed) => {
                              setBrowseSeed(seed);
                              setTab("browse");
                            }
                          : undefined
                      }
                    />
                  ) : (
                    <BrowseMods
                      key={shownId}
                      gameId={shownId}
                      initialMod={browseSeed}
                      onInitialModHandled={clearBrowseSeed}
                    />
                  )}
                </m.div>
              </AnimatePresence>
            </>
          )}
        </m.div>
      )}
      </AnimatePresence>
    </GlassPage>
  );
}

// ------------ Mods Panel ------------
// The mod support switch on the Mods page, with the warning that mods can get an account banned. It only shows
// while mods are on; switching it off brings back the opt-in panel, which is the one way to turn them on again.
function ModsPanel() {
  const t = useT();
  const master = useModsStore((s) => s.masterEnabled);
  const setMasterEnabled = useModsStore((s) => s.setMasterEnabled);

  const toggleMaster = (on: boolean) =>
    void setConfigValue("behavior.modsEnabled", on).then(() => setMasterEnabled(on));

  return (
    <SettingCard
      icon={<AlertTriangle size={18} />}
      title={t("settings.mods.enableModSupport")}
      description={t(MOD_RISK_WARNING)}
      control={
        <Toggle
          checked={master}
          ariaLabel={t("settings.mods.enableModSupport")}
          onChange={toggleMaster}
        />
      }
    />
  );
}
