// ------------ Launcher Window ------------
// The main launcher window. Hooks up every backend event (games starting and stopping, the download queue,
// updates, network, wallpapers), shows the sidebar, wallpaper and current page, and pops toasts and modals.
import { Suspense, lazy, useEffect, useMemo, type ComponentType } from "react";
import { useUiStore, type View } from "./store/uiStore";
import { useGamesStore } from "./store/gamesStore";
import {
  GLASS_EFFECTS,
  changedSettingKey,
  changeTouchesSettings,
  useGlassEffects,
  useSettingsStore,
} from "./store/settingsStore";
import { useQueueStore } from "./store/queueStore";
import { useSpeedSampler, useDownloadHistoryStore } from "./store/downloads";
import { jobFailureKey, useNotificationStore } from "./store/notifications";
import { useCustomizationStore } from "./store/customizationStore";
import { useGameLinksStore, useRemoteMediaStore } from "./store/remoteContentStore";
import { usePlaytimeStore } from "./store/playtimeStore";
import { useModsStore, useModProfilesStore } from "./store/mods";
import { useLibraryStore } from "./store/libraryStore";
import { GAMES, gameById, type GameId, type ResourceQuality } from "./data/games";
import { qualityLabel } from "./lib/gameActions";
import {
  appInfo,
  applyActiveGame,
  listGames,
  onGameStarted,
  onGameStopped,
  onGameLaunchFailed,
  getRunningGameIds,
  onModsLoadFailed,
  onModsNotice,
  onModProgress,
  downloadState,
  onQueueState,
  checkGameUpdate,
  onGameUpdateAvailable,
  onWallpaperMediaUpdated,
  onNetworkStatusChanged,
  getNetworkStatus,
  onInstallationComplete,
  onGamesDetected,
  onWindowRestored,
  onWindowMaximizedChanged,
  getWindowMaximized,
  openLogsFolder,
  openLeftoverFolder,
  getConfigNotice,
  downloadEnqueue,
} from "./lib/ipc";
import { onEvent, subscriptions } from "./lib/rpc";
import { baseKindOf, TERMINAL_PHASES } from "./lib/download";
import { AnimatePresence, LazyMotion, MotionConfig, m } from "framer-motion";
import { useModalStore } from "./store/modalStore";
import { revealWhenPainted, WallpaperLayer } from "./components/layout/WallpaperLayer";
import { log } from "./lib/log";
import { hasKey, t, type TKey } from "./lib/i18n";
import { useT } from "./lib/useT";
import { useRendererSuspended } from "./lib/launcherHooks";
import { useTauriEvents, setNowTickerSuspended } from "./lib/hooks";
import { Sidebar } from "./components/layout/Sidebar";
import { NotificationPanel } from "./components/layout/NotificationPanel";
import { ToastLayer } from "./components/layout/ToastLayer";
import { OfflineBanner } from "./components/layout/Banners";
import { WindowControls } from "./components/layout/WindowControls";
import { GameHome } from "./components/home/Home";
import { ModalRoot } from "./components/ui/Modal";
import { ErrorBoundary, retryOrReload } from "./components/layout/ErrorBoundary";
import { ActionButton } from "./components/ui/controls";

const motionFeatures = import("./lib/motionFeatures").then((mod) => mod.default);
const loadMotionFeatures = () => motionFeatures;

const FirstRunSetup = lazy(() =>
  import("./components/layout/FirstRunSetup").then((mod) => ({ default: mod.FirstRunSetup })),
);

const SETUP_BACKDROP = (
  <div
    className="absolute inset-0 z-(--z-setup)"
    style={{
      background: "rgba(10,10,13,0.86)",
      backdropFilter: "blur(28px) saturate(160%)",
      WebkitBackdropFilter: "blur(28px) saturate(160%)",
    }}
  />
);

function announceUnfinishedSwitches(switches: Record<string, ResourceQuality>) {
  for (const [id, quality] of Object.entries(switches)) {
    const game = gameById(id as GameId);
    const label = qualityLabel(quality);
    useNotificationStore.getState().push({
      type: "info",
      title: t("actions.game.switchUnfinished.title", { game: game.name, quality: label }),
      text: t("actions.game.switchUnfinished.text", { quality: label }),
      action: { label: t("common.resume"), run: () => void downloadEnqueue(id) },
    });
  }
}

let configNotice: Promise<boolean> | null = null;

function announceConfigNotice(): Promise<boolean> {
  configNotice ??= getConfigNotice().then((notice) => {
    if (!notice) return false;
    const { saveBlocked, ...note } = notice;
    useNotificationStore.getState().push(note);
    if (!saveBlocked) return false;
    useModalStore.getState().openConfirm({
      title: note.title,
      message: note.text,
      confirmLabel: t("common.ok"),
      cancelLabel: null,
      onConfirm: () => {},
    });
    return true;
  });
  return configNotice;
}

function kindText(key: string, fallback: TKey, game: string, kind: string): string {
  return t(hasKey(key) ? key : fallback, { game, kind });
}

const dismissToast = (id: string) => useNotificationStore.getState().dismissToast(id);

const openDownloadsAction = () => ({
  label: t("app.openDownloads"),
  run: () => useUiStore.getState().setView("downloads"),
});

const REPAIR_NEEDS_UPDATE = "An update is available. Update the game first, then repair."; // i18n-ignore

// ------------ Pages ------------
// Home is in the main bundle; every other page is its own chunk. The chunks load in the background once the
// launcher is idle after starting (preloadPages), and a page whose chunk is in renders straight away instead of
// through Suspense, so opening it never shows an empty frame that then fills in.
const PAGE_LOADERS = {
  playtime: () => import("./components/PlaytimePage").then((m) => m.PlaytimePage),
  downloads: () => import("./components/DownloadsPage").then((m) => m.DownloadsPage),
  gallery: () => import("./components/gallery/CapturesTab").then((m) => m.GalleryPage),
  mods: () => import("./components/mods/ModsPage").then((m) => m.ModsPage),
  settings: () => import("./components/settings/SettingsPage").then((m) => m.SettingsPage),
} satisfies Record<Exclude<View, "home">, () => Promise<ComponentType>>;
type ChunkPage = keyof typeof PAGE_LOADERS;

const loadedPages: Partial<Record<View, ComponentType>> = { home: GameHome };
const pageLoads = new Map<ChunkPage, Promise<ComponentType>>();

function loadPage(page: ChunkPage): Promise<ComponentType> {
  let load = pageLoads.get(page);
  if (!load) {
    load = PAGE_LOADERS[page]().then(
      (component) => (loadedPages[page] = component),
      (e: unknown) => {
        pageLoads.delete(page);
        throw e;
      },
    );
    pageLoads.set(page, load);
  }
  return load;
}

const lazyPages = new Map<ChunkPage, ComponentType>();

function pageFor(view: View): ComponentType {
  const loaded = loadedPages[view];
  if (loaded) return loaded;
  const page = view as ChunkPage;
  let waiting = lazyPages.get(page);
  if (!waiting) {
    waiting = lazy(() => loadPage(page).then((component) => ({ default: component })));
    lazyPages.set(page, waiting);
  }
  return waiting;
}

function preloadPages(): () => void {
  const queue = (Object.keys(PAGE_LOADERS) as ChunkPage[]).filter((page) => !loadedPages[page]);
  let handle = 0;
  let stopped = false;
  let stopData: (() => void) | null = null;
  const next = () => {
    if (stopped) return;
    const page = queue.shift();
    if (!page) {
      void import("./lib/pagePreload")
        .then((mod) => {
          if (!stopped) stopData = mod.preloadPageData();
        })
        .catch((e: unknown) => log.debug("[pages] could not load the page preloader:", e));
      return;
    }
    loadPage(page)
      .catch((e: unknown) => log.debug(`[pages] could not preload ${page}:`, e))
      .finally(() => {
        if (!stopped) handle = requestIdleCallback(next, { timeout: 2000 });
      });
  };
  handle = requestIdleCallback(next, { timeout: 2000 });
  return () => {
    stopped = true;
    cancelIdleCallback(handle);
    stopData?.();
  };
}

function pageCrashContext(): Record<string, unknown> {
  const ui = useUiStore.getState();
  return { view: ui.activeView, activeGameId: ui.activeGameId, version: ui.appVersion || "unknown" };
}

function PageError({ view, onRetry }: { view: string; onRetry: () => void }) {
  const t = useT();
  const setView = useUiStore((s) => s.setView);
  return (
    <div className="flex h-full w-full flex-col items-center justify-center gap-4 px-6 text-white">
      <div className="text-lg font-semibold">{t("crash.page.title")}</div>
      <div className="max-w-[420px] text-center text-sm text-white/60">{t("crash.page.body")}</div>
      <div className="flex items-center gap-2">
        <ActionButton onClick={() => retryOrReload(onRetry)}>{t("common.tryAgain")}</ActionButton>
        {view !== "home" && (
          <ActionButton onClick={() => setView("home")}>{t("crash.page.backHome")}</ActionButton>
        )}
        <ActionButton onClick={() => void openLogsFolder()}>{t("common.openLogsFolder")}</ActionButton>
      </div>
    </div>
  );
}

export default function App() {
  const view = useUiStore((s) => s.activeView);
  const setupOpen = useLibraryStore((s) => s.hydrated && !s.setupComplete);
  const rendererSuspended = useRendererSuspended();
  const maximized = useUiStore((s) => s.maximized);

  useSpeedSampler();

  useEffect(() => {
    document.documentElement.classList.toggle("renderer-suspended", rendererSuspended);
    setNowTickerSuspended(rendererSuspended);
  }, [rendererSuspended]);

  const effects = useGlassEffects();
  useEffect(() => {
    for (const tier of GLASS_EFFECTS) {
      document.documentElement.classList.toggle(`effects-${tier}`, tier === effects);
    }
  }, [effects]);

  useEffect(() => preloadPages(), []);

  useEffect(() => {
    void applyActiveGame(useUiStore.getState().activeGameId);
    return useUiStore.subscribe((s, p) => {
      if (s.activeGameId !== p.activeGameId) void applyActiveGame(s.activeGameId);
    });
  }, []);

  useTauriEvents({
    "settings-changed": (payload) => {
      const key = changedSettingKey(payload);
      if (changeTouchesSettings(payload)) void useSettingsStore.getState().hydrate();
      if (key === null || key === "modsEnabled" || key === "behavior.modsEnabled") {
        void useModsStore.getState().hydrateMasterEnabled();
      }
      if (key === null) {
        void useCustomizationStore.getState().hydrate();
        void useLibraryStore.getState().hydrate();
      } else if (key.startsWith("games.") && key.endsWith(".voicePackLanguage")) {
        void useCustomizationStore.getState().hydrate();
      }
    },
  });

  useEffect(() => {
    revealWhenPainted(
      Promise.all([
        useSettingsStore.getState().hydrate(),
        useCustomizationStore.getState().hydrate(),
        useRemoteMediaStore.getState().hydrate(),
      ]),
    );
    void announceConfigNotice();
    void useGameLinksStore.getState().hydrate();
    void usePlaytimeStore.getState().hydrate();
    void useModsStore.getState().hydrateMasterEnabled();
    const libraryHydrated = useLibraryStore.getState().hydrate();
    void appInfo()
      .then((info) => {
        useUiStore.getState().setAppVersion(info.version);
        useUiStore.getState().setBuildType(info.buildType);
      })
      .catch((e) => log.warn("appInfo failed (version badge will show defaults):", e));

    const refreshInstalled = (onlyGameIds?: string[], atStartup = false) =>
      void listGames().then((games) => {
        const installedIds = games.filter((g) => g.installed).map((g) => g.id);
        useGamesStore.getState().setInstalled(installedIds);
        useGamesStore.getState().setPendingInstalls(
          Object.fromEntries(
            games.flatMap((g) => (g.pendingInstall ? [[g.id, g.pendingInstall] as const] : [])),
          ),
        );
        const switches = Object.fromEntries(
          games.flatMap((g) => (g.installed && g.qualitySwitch ? [[g.id, g.qualitySwitch] as const] : [])),
        );
        useGamesStore.getState().setQualitySwitches(switches);
        if (atStartup) announceUnfinishedSwitches(switches);
        const toCheck = onlyGameIds
          ? installedIds.filter((id) => onlyGameIds.includes(id))
          : installedIds;
        for (const id of toCheck) {
          void checkGameUpdate(id).then((u) => {
            if (u.ok) useGamesStore.getState().setUpdate(id, u.available, u);
          });
        }
      });
    refreshInstalled(undefined, true);
    void downloadState().then((s) => useQueueStore.getState().setJobs(s.jobs));

    const subs = subscriptions();
    const { keep } = subs;
    let runningEpoch = 0;
    const seedRunningGames = () => {
      const epoch = runningEpoch;
      void getRunningGameIds(GAMES.map((g) => g.id)).then((ids) => {
        if (ids && epoch === runningEpoch) useGamesStore.getState().setRunningGames(ids);
      });
    };
    const startedSub = onGameStarted((id) => {
      if (!id) return;
      runningEpoch += 1;
      const ids = useGamesStore.getState().runningGames;
      if (!ids.includes(id)) useGamesStore.getState().setRunningGames([...ids, id]);
    });
    void startedSub.then(keep);
    const stoppedSub = onGameStopped((id) => {
      runningEpoch += 1;
      const ids = useGamesStore.getState().runningGames;
      useGamesStore.getState().setRunningGames(id ? ids.filter((x) => x !== id) : []);
      usePlaytimeStore.getState().refreshAfterSession();
    });
    void stoppedSub.then(keep);
    const failedSub = onGameLaunchFailed((id, reason) => {
      if (id) {
        runningEpoch += 1;
        const ids = useGamesStore.getState().runningGames;
        useGamesStore.getState().setRunningGames(ids.filter((x) => x !== id));
        useGamesStore.getState().setLaunching(id, false);
      }
      const game = GAMES.find((g) => g.id === id);
      useNotificationStore.getState().push({
        type: "warning",
        title: game
          ? t("app.launchFailed.title", { game: game.name })
          : t("app.launchFailed.titleUnknown"),
        text: reason || t("app.launchFailed.text"),
      });
    });
    void failedSub.then(keep);
    void Promise.all([startedSub, stoppedSub, failedSub]).then(seedRunningGames);
    void onModsLoadFailed((id, error, message) => {
      const game = GAMES.find((g) => g.id === id);
      useNotificationStore.getState().push({
        type: "warning",
        title: t("app.modsLoadFailed.title"),
        text:
          message ||
          (game
            ? t("app.modsLoadFailed.text", { game: game.name, error })
            : t("app.modsLoadFailed.textUnknown", { error })
          ).trim(),
      });
    }).then(keep);
    void onModsNotice((kind, message) => {
      useNotificationStore.getState().push({
        type: kind === "settings" ? "warning" : "info",
        title: t(kind === "settings" ? "app.modsNotice.settingsTitle" : "app.modsNotice.tidiedTitle"),
        text: message,
      });
    }).then(keep);
    void onEvent("custom-launcher-missing", (p) => {
      const game = GAMES.find((g) => g.id === p?.gameId);
      useNotificationStore.getState().push({
        type: "warning",
        title: t("app.customLauncherMissing.title"),
        text: p?.path
          ? t("app.customLauncherMissing.text", {
              path: p.path,
              game: game?.name ?? t("app.theGame"),
            })
          : t("app.customLauncherMissing.textNoPath", { game: game?.name ?? t("app.theGame") }),
      });
    }).then(keep);
    void onEvent("game-launch-waiting", (p) => {
      const id = p?.gameId;
      if (id) useGamesStore.getState().setLaunching(id, false);
      const game = GAMES.find((g) => g.id === id);
      useNotificationStore.getState().push({
        type: "warning",
        title: game
          ? t("app.launchWaiting.title", { game: game.name })
          : t("app.launchWaiting.titleUnknown"),
        text: p?.reason || t("app.launchWaiting.text"),
      });
    }).then(keep);
    void onEvent("mods-tools-updating", (p) => {
      const game = GAMES.find((g) => g.id === p?.gameId);
      useNotificationStore.getState().push({
        type: "progress",
        title: t("app.modToolsUpdating.title"),
        text: game
          ? t("app.modToolsUpdating.text", { game: game.name })
          : t("app.modToolsUpdating.textUnknown"),
      });
    }).then(keep);
    void onEvent("overlay-fullscreen-blocked", () => {
      const title = t("app.overlayBlocked.title");
      const notes = useNotificationStore.getState();
      if (notes.items.some((n) => n.title === title && !n.read)) return;
      notes.push({
        type: "warning",
        title,
        text: t("app.overlayBlocked.text"),
      });
    }).then(keep);
    void onQueueState((s) => {
      useQueueStore.getState().setJobs(s.jobs);
    }).then(keep);

    let queuePhases: Record<string, string> = {};
    keep(
      useQueueStore.subscribe((state) => {
        const next: Record<string, string> = {};
        const finishedGames: string[] = [];
        for (const j of state.jobs) {
          next[j.id] = j.phase;
          const wasActive =
            queuePhases[j.id] !== undefined && !TERMINAL_PHASES.includes(queuePhases[j.id]);
          if (wasActive && TERMINAL_PHASES.includes(j.phase)) {
            const kind = baseKindOf(j);
            if (j.phase !== "deferred") useDownloadHistoryStore.getState().add({ ...j, kind });
            finishedGames.push(j.gameId);
            const name = GAMES.find((g) => g.id === j.gameId)?.name ?? j.gameId;
            if (j.phase === "done" && kind !== "verify") {
              const gameId = j.gameId;
              const leftovers = j.warning && (kind === "uninstall" || kind === "move");
              useNotificationStore.getState().push({
                type: j.warning ? "warning" : "success",
                title: kindText(`app.jobDone.title.${kind}`, "app.jobDone.titleOther", name, kind),
                text: j.message || kindText(`app.jobDone.text.${kind}`, "app.jobDone.textOther", name, kind),
                action: leftovers
                  ? { label: t("common.openFolder"), run: () => void openLeftoverFolder(gameId) }
                  : undefined,
              });
            } else if (j.phase === "error" && kind !== "verify") {
              const gameId = j.gameId;
              const needsUpdate = kind === "repair" && j.error === REPAIR_NEEDS_UPDATE;
              useNotificationStore.getState().push({
                type: "error",
                title: kindText(`app.jobFailed.title.${kind}`, "app.jobFailed.titleOther", name, kind),
                text: j.error || t("common.genericError"),
                action: needsUpdate
                  ? { label: t("common.update"), run: () => void downloadEnqueue(gameId) }
                  : openDownloadsAction(),
                key: jobFailureKey(gameId),
              });
            }
          }
        }
        queuePhases = next;
        if (finishedGames.length > 0) refreshInstalled(finishedGames);
      }),
    );
    void onGameUpdateAvailable((id, versions) =>
      useGamesStore.getState().setUpdate(id, true, versions),
    ).then(keep);
    void onWallpaperMediaUpdated((m) => useRemoteMediaStore.getState().apply(m)).then(keep);
    let networkEventSeen = false;
    void onNetworkStatusChanged((online) => {
      networkEventSeen = true;
      useUiStore.getState().setOnline(online);
    }).then((f) => {
      keep(f);
      void getNetworkStatus().then((online) => {
        if (online !== null && !networkEventSeen) useUiStore.getState().setOnline(online);
      });
    });
    let maximizedEventSeen = false;
    void onWindowMaximizedChanged((maximized) => {
      maximizedEventSeen = true;
      useUiStore.getState().setMaximized(maximized);
    }).then((f) => {
      keep(f);
      void getWindowMaximized().then((maximized) => {
        if (!maximizedEventSeen) useUiStore.getState().setMaximized(maximized);
      });
    });
    void onInstallationComplete((id) => refreshInstalled(id ? [id] : undefined)).then(keep);
    void onGamesDetected((ids) => {
      if (!ids.length) {
        refreshInstalled([]);
        return;
      }
      refreshInstalled(ids);
      void libraryHydrated
        .then(() => {
          const library = useLibraryStore.getState();
          if (!library.setupComplete) return;
          for (const g of GAMES) if (ids.includes(g.id)) library.show(g.id);
        })
        .catch((e) => log.warn("Could not add the detected games to the library:", e));
      const names = ids.map((id) => GAMES.find((g) => g.id === id)?.name ?? id);
      useNotificationStore.getState().push({
        type: "success",
        title: t("app.gamesDetected.title", { count: names.length }),
        text: t("app.gamesDetected.text", { count: names.length, names: names.join(", ") }),
      });
    }).then(keep);
    void onEvent("mods-status-changed", (payload) => {
      const changed = payload?.gameId;
      const mods = useModsStore.getState();
      const target = mods.requestedGameId ?? mods.loadedGameId ?? useUiStore.getState().activeGameId;
      if (!changed || changed === target) void mods.refresh(target);
      const profiles = useModProfilesStore.getState();
      const profilesTarget = profiles.requestedGameId ?? profiles.loadedGameId;
      if (profilesTarget && (!changed || changed === profilesTarget)) {
        void profiles.refresh(profilesTarget);
      }
    }).then(keep);
    void onModProgress((p) => {
      const mods = useModsStore.getState();
      mods.setProgress(p.done ? null : p);
      if (p.done && !p.failed && p.gameId === (mods.requestedGameId ?? mods.loadedGameId)) {
        void mods.refresh(p.gameId);
      }
    }).then(keep);
    let lastRestoreRefresh = 0;
    void onWindowRestored(() => {
      const now = Date.now();
      if (now - lastRestoreRefresh < 2000) return;
      lastRestoreRefresh = now;
      void downloadState().then((s) => useQueueStore.getState().setJobs(s.jobs));
      refreshInstalled();
      seedRunningGames();
    }).then(keep);

    return subs.stop;
  }, []);

  const Page = useMemo(() => pageFor(view), [view]);

  return (
    <LazyMotion features={loadMotionFeatures} strict>
      <MotionConfig reducedMotion="user">
        <div
          data-window-root
          className={`relative h-screen w-screen overflow-hidden ${maximized ? "" : "rounded-ui"}`}
        >
        <div
          className="drag-region absolute top-0 z-(--z-chrome) h-[32px]"
          style={{ left: 60, right: 158 }}
        />

        <WallpaperLayer />
        <div className="relative z-10 flex h-full" inert={setupOpen}>
          <Sidebar />
          <main className="relative min-w-0 flex-1">
            <AnimatePresence>
              {view !== "home" && (
                <m.div
                  key="glass-sheet"
                  className="glass-strong pointer-events-none absolute inset-0"
                  initial={{ opacity: 0 }}
                  animate={{ opacity: 1, transition: { duration: 0.2 } }}
                  exit={{ opacity: 0, transition: { duration: 0.15 } }}
                />
              )}
            </AnimatePresence>

            <AnimatePresence initial={false}>
              <m.div
                key={view}
                className="absolute inset-0"
                initial="initial"
                animate="animate"
                exit="exit"
              >
                <ErrorBoundary
                  key={view}
                  scope={`page ${view}`}
                  context={pageCrashContext}
                  fallback={(reset) => <PageError view={view} onRetry={reset} />}
                >
                  <Suspense fallback={null}>
                    <Page />
                  </Suspense>
                </ErrorBoundary>
              </m.div>
            </AnimatePresence>
          </main>
        </div>

          {!setupOpen && <WindowControls />}
          <div
            className="pointer-events-none absolute inset-0"
            style={setupOpen ? { zIndex: "calc(var(--z-setup) + 1)" } : undefined}
          >
            <div className="pointer-events-none absolute left-[60px] right-[158px] top-[12px] z-(--z-banner) flex flex-col items-center gap-[8px] px-[16px]">
              <OfflineBanner />
            </div>
            <ToastLayer onOpen={setupOpen ? dismissToast : undefined} />
          </div>
          <NotificationPanel />
          <ModalRoot />
          <AnimatePresence>
            {setupOpen && (
              <Suspense key="setup" fallback={SETUP_BACKDROP}>
                <FirstRunSetup />
              </Suspense>
            )}
          </AnimatePresence>
        </div>
      </MotionConfig>
    </LazyMotion>
  );
}
