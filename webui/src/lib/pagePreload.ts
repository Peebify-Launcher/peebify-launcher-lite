// ------------ Page Preload ------------
// Fetches what the pages show when they open, in the background while the launcher is idle, so a page opens at its
// final size instead of filling in and resizing: the mods and captures lists, the remembered reads the Settings tabs
// start from, and the install dialog of each game not yet installed. Runs again for each game picked, fetching only
// what that game has not loaded yet.
import { gameById, isManaged, type GameId } from "../data/games";
import { useUiStore } from "../store/uiStore";
import { useGamesStore } from "../store/gamesStore";
import { useModProfilesStore, useModsStore } from "../store/mods";
import { useMediaStore } from "../store/mediaStore";
import { useLibraryStore } from "../store/libraryStore";
import { preloadInstall } from "./installPreload";
import { reads, warmRemembered } from "./launcherHooks";
import { log } from "./log";

type Step = () => Promise<unknown>;

const IDLE_TIMEOUT_MS = 3000;

function launcherSteps(): Step[] {
  return [
    () => warmRemembered(reads.overlayConfig),
    () => warmRemembered(reads.overlayLoaderSettings),
    () => warmRemembered(reads.overlayStatus),
    async () => {
      if (!useMediaStore.getState().hydrated) await useMediaStore.getState().load();
    },
    ...installSteps(),
  ];
}

function installSteps(): Step[] {
  const { installed, pendingInstalls } = useGamesStore.getState();
  return useLibraryStore
    .getState()
    .visible.filter((id) => isManaged(id) && !installed.includes(id) && !pendingInstalls[id])
    .map((id) => () => preloadInstall(id));
}

function gameSteps(gameId: GameId): Step[] {
  const game = gameById(gameId);
  const installed = useGamesStore.getState().installed.includes(gameId);
  const steps: Step[] = [
    async () => {
      const mods = useModsStore.getState();
      if (mods.loadedGameId === gameId) return;
      await mods.refresh(gameId);
      await useModProfilesStore.getState().refresh(gameId);
    },
    () => warmRemembered(reads.modsStatus(gameId)),
    () => warmRemembered(reads.gameConfig(gameId)),
    () => warmRemembered(reads.steamInstall(gameId)),
    () => warmRemembered(reads.contentPacks(gameId)),
  ];
  if (game.fpsUnlock) steps.push(() => warmRemembered(reads.fpsUnlock(gameId)));
  if (game.resourceQualityChoice) steps.push(() => warmRemembered(reads.resourceQuality(gameId, true)));
  if (installed) {
    steps.push(() => warmRemembered(reads.installPathHealth(gameId)));
    if (isManaged(gameId) && game.voicePackChoice) steps.push(() => warmRemembered(reads.appliedVoicePacks(gameId)));
  }
  return steps;
}

function runIdle(steps: Step[], stopped: () => boolean): void {
  const next = () => {
    const step = steps.shift();
    if (!step || stopped()) return;
    void step()
      .catch((e: unknown) => log.debug("[preload] a page read failed:", e))
      .finally(() => {
        if (!stopped()) requestIdleCallback(next, { timeout: IDLE_TIMEOUT_MS });
      });
  };
  requestIdleCallback(next, { timeout: IDLE_TIMEOUT_MS });
}

export function preloadPageData(): () => void {
  let stopped = false;
  let run = 0;
  const forGame = (gameId: GameId) => {
    const mine = ++run;
    runIdle(gameSteps(gameId), () => stopped || mine !== run);
  };
  runIdle(launcherSteps(), () => stopped);
  forGame(useUiStore.getState().activeGameId);
  const unsubscribe = useUiStore.subscribe((s, p) => {
    if (s.activeGameId !== p.activeGameId) forGame(s.activeGameId);
  });
  return () => {
    stopped = true;
    unsubscribe();
  };
}
