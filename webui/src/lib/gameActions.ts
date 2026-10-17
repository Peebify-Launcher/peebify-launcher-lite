// ------------ Game Actions ------------
// The shared logic behind a game's buttons: update, repair, move, uninstall, locate an existing install, and
// pick the download quality. Used by the home page and by settings.
import { useCallback, useEffect, useState } from "react";
import { fetchRemembered, reads, recall, rememberAnswer, useRemembered } from "./launcherHooks";
import { gameById, isManaged, supportsQuickRepair, type GameId, type ResourceQuality } from "../data/games";
import { useGamesStore } from "../store/gamesStore";
import { useCustomizationStore } from "../store/customizationStore";
import { useQueueStore } from "../store/queueStore";
import { useNotificationStore } from "../store/notifications";
import { useModalStore } from "../store/modalStore";
import { isActiveJob } from "./download";
import { log } from "./log";
import { t } from "./i18n";
import { useT } from "./useT";
import {
  checkGameUpdate,
  checkResourceQuality,
  DEFAULT_RESOURCE_QUALITY,
  downloadEnqueue,
  getModsStatus,
  getSteamInstall,
  listMods,
  locateGameInstall,
  moveGame,
  repairGame,
  RESOURCE_QUALITY_OPTIONS,
  uninstallGame,
  type QualityCheck,
  type ResourceQualityInfo,
  type SteamInstall,
} from "./ipc";

export const qualityLabel = (q: ResourceQuality): string =>
  RESOURCE_QUALITY_OPTIONS.find((o) => o.value === q)?.label ?? q;

const joinLabels = (qualities: ResourceQuality[]): string =>
  qualities
    .map(qualityLabel)
    .reduce((first, second) => (first ? t("actions.game.joinAnd", { first, second }) : second), "");

export function describeQualityCheck(check: QualityCheck, game: string): { type: "success" | "warning"; title: string; text: string } {
  const quality = qualityLabel(check.selected);
  const launch = qualityLabel(check.launchWith);
  const found = check.files[check.selected];
  const whole = !!found && found.total > 0 && found.present === found.total;
  if (whole && check.launchWith === check.selected) {
    return {
      type: "success",
      title: t("maintenance.resourceQuality.check.okTitle", { game, quality }),
      text: t("maintenance.resourceQuality.check.okText", { quality }),
    };
  }
  if (whole) {
    return {
      type: "warning",
      title: t("maintenance.resourceQuality.check.behindTitle", { game, quality, launch }),
      text: t("maintenance.resourceQuality.check.behindText", { quality, launch }),
    };
  }
  if (found && found.present > 0) {
    return {
      type: "warning",
      title: t("maintenance.resourceQuality.check.incompleteTitle", { game, quality }),
      text: t("maintenance.resourceQuality.check.incompleteText", {
        quality,
        present: found.present,
        total: found.total,
      }),
    };
  }
  const others = check.onDisk.filter((q) => q !== check.selected);
  return {
    type: "warning",
    title: t("maintenance.resourceQuality.check.missingTitle", { game, quality }),
    text: others.length
      ? t("maintenance.resourceQuality.check.missingText", { quality, installed: joinLabels(others) })
      : t("maintenance.resourceQuality.check.missingNoneText", { quality }),
  };
}

export function useResourceQuality(
  gameId: GameId,
  { installed, jobActive }: { installed: boolean; jobActive: boolean },
  { sizes: withSizes = false, initial = null }: { sizes?: boolean; initial?: ResourceQualityInfo | null } = {},
) {
  const game = gameById(gameId);
  const quality =
    useCustomizationStore((s) => s.resourceQuality[gameId]) ?? DEFAULT_RESOURCE_QUALITY;
  const setResourceQuality = useCustomizationStore((s) => s.setResourceQuality);
  const push = useNotificationStore((s) => s.push);
  const toast = useNotificationStore((s) => s.toast);
  const [info, setInfo] = useState<{ gameId: GameId; data: ResourceQualityInfo } | null>(
    initial ? { gameId, data: initial } : null,
  );
  const [checking, setChecking] = useState(false);
  const [reload, setReload] = useState(0);
  useEffect(() => {
    if (!game.resourceQualityChoice) return;
    let alive = true;
    void fetchRemembered(reads.resourceQuality(gameId, withSizes)).then((data) => {
      if (alive) setInfo({ gameId, data });
    });
    return () => {
      alive = false;
    };
  }, [gameId, installed, jobActive, game.resourceQualityChoice, withSizes, reload]);

  const data = info?.gameId === gameId ? info.data : recall(reads.resourceQuality(gameId, withSizes));
  const onDisk = data?.installed ?? [];
  const pending = installed && isManaged(gameId) && onDisk.length > 0 && !onDisk.includes(quality);

  const check = useCallback(async () => {
    if (checking) return;
    setChecking(true);
    toast({
      type: "progress",
      title: t("maintenance.resourceQuality.check.checkingTitle", { game: game.name }),
      text: t("maintenance.resourceQuality.check.checkingText", { quality: qualityLabel(quality) }),
    });
    const result = await checkResourceQuality(gameId);
    setChecking(false);
    setReload((n) => n + 1);
    push(
      result.ok
        ? describeQualityCheck(result.check, game.name)
        : {
            type: "warning",
            title: t("maintenance.resourceQuality.check.failedTitle", { game: game.name }),
            text: result.error,
          },
    );
  }, [checking, gameId, game.name, quality, push, toast]);

  const apply = useCallback(() => {
    const from = joinLabels(onDisk);
    void downloadEnqueue(gameId, () =>
      push({
        type: "progress",
        title: t("actions.game.switchingQuality.title", {
          game: game.name,
          quality: qualityLabel(quality),
        }),
        text: t("actions.game.switchingQuality.text", { quality: qualityLabel(quality), from }),
      }),
    );
  }, [gameId, game.name, quality, onDisk, push]);

  return {
    quality,
    setQuality: (q: ResourceQuality) => setResourceQuality(gameId, q),
    installedLabel: onDisk.map(qualityLabel).join(" + "),
    loaded: data !== null,
    sizes: data?.sizes ?? null,
    pending,
    apply,
    check,
    checking,
  };
}

export function useModsForceDx11(gameId: GameId): boolean {
  const s = useRemembered(reads.modsStatus(gameId), [gameId]);
  return !!s && s.masterEnabled && s.gameEnabled && s.forcesDx11;
}

function useSteamInstall(gameId: GameId, installed: boolean) {
  const [state, setState] = useState<{ gameId: GameId; info: SteamInstall } | null>(null);
  useEffect(() => {
    let alive = true;
    void fetchRemembered(reads.steamInstall(gameId)).then((info) => {
      if (alive) setState({ gameId, info });
    });
    return () => {
      alive = false;
    };
  }, [gameId, installed]);
  const setSteam = useCallback(
    (info: SteamInstall) => {
      rememberAnswer(reads.steamInstall(gameId), info);
      setState({ gameId, info });
    },
    [gameId],
  );
  return [state?.gameId === gameId ? state.info : recall(reads.steamInstall(gameId)), setSteam] as const;
}

async function countMods(gameId: GameId): Promise<number> {
  try {
    const status = await getModsStatus(gameId);
    return status.supportsMods ? ((await listMods(gameId))?.length ?? 0) : 0;
  } catch (e) {
    log.warn(`Could not count the mods for ${gameId}:`, e);
    return 0;
  }
}

function uninstallMessage(name: string, steamCopy: boolean, modCount: number): string {
  if (steamCopy) {
    return modCount
      ? t("actions.game.uninstall.steamMods", { game: name, count: modCount })
      : t("actions.game.uninstall.steam", { game: name });
  }
  return modCount
    ? t("actions.game.uninstall.filesMods", { count: modCount })
    : t("actions.game.uninstall.files");
}

export const jobActiveBlocker = (): string => t("actions.game.jobActiveBlocker");

export const linkedUncheckedText = (): string => t("actions.game.located.linkedUnchecked");

const SKIP_LINKED_CHECK: ReadonlySet<GameId> = new Set<GameId>(["bd2", "re1999"]);

export function checkLinkedInstall(gameId: GameId): boolean {
  if (SKIP_LINKED_CHECK.has(gameId)) return false;
  void repairGame(gameId, supportsQuickRepair(gameId));
  return true;
}

export function useGameActions(gameId: GameId) {
  const t = useT();
  const game = gameById(gameId);
  const managed = isManaged(gameId);
  const installed = useGamesStore((s) => s.installed.includes(gameId));
  const hasPending = useGamesStore((s) => !!s.pendingInstalls[gameId]);
  const partial = !installed && hasPending;
  const running = useGamesStore((s) => s.runningGames.includes(gameId));
  const jobActive = useQueueStore((s) => s.jobs.some((j) => j.gameId === gameId && isActiveJob(j)));
  const push = useNotificationStore((s) => s.push);
  const toast = useNotificationStore((s) => s.toast);
  const openConfirm = useModalStore((s) => s.openConfirm);
  const [steam, setSteam] = useSteamInstall(gameId, installed);
  const steamCopy = installed && !!steam?.isSteamInstall;

  const uninstallBlocker = running
    ? t("actions.game.closeFirst", { game: game.name })
    : jobActive
      ? t("actions.game.jobActiveBlocker")
      : null;

  const checkUpdates = async () => {
    toast({
      type: "progress",
      title: t("actions.game.checking.title", { game: game.name }),
      text: t("actions.game.checking.text"),
    });
    const result = await checkGameUpdate(gameId, true);
    if (!result.ok) {
      push({ type: "warning", title: t("actions.game.checkFailed"), text: result.error });
      return;
    }
    const { available } = result;
    useGamesStore.getState().setUpdate(gameId, available, result);
    if (!available) {
      toast({
        type: "success",
        title: t("actions.game.upToDate.title"),
        text: t("actions.game.upToDate.text", { game: game.name }),
      });
      return;
    }
    push({
      type: "info",
      title: t("actions.game.updateAvailable.title"),
      text: t("actions.game.updateAvailable.text", { game: game.name }),
    });
  };

  const quickRepair = () => {
    void repairGame(gameId, true).then((queued) => {
      if (!queued) return;
      push({
        type: "progress",
        title: t("actions.game.quickRepair.title", { game: game.name }),
        text: t("actions.game.quickRepair.text"),
      });
    });
  };

  const confirmFullRepair = () =>
    openConfirm({
      title: t("actions.game.fullRepair.title", { game: game.name }),
      message: t("actions.game.fullRepair.message"),
      confirmLabel: t("actions.game.fullRepair.confirm"),
      onConfirm: () => {
        void repairGame(gameId, false).then((queued) => {
          if (!queued) return;
          push({
            type: "progress",
            title: t("actions.game.repairing.title", { game: game.name }),
            text: t("actions.game.repairing.text"),
          });
        });
      },
    });

  const locate = async () => {
    const r = await locateGameInstall(gameId);
    if (r.ok) {
      void useCustomizationStore.getState().hydrate();
      const ids = useGamesStore.getState().installed;
      if (!ids.includes(gameId)) useGamesStore.getState().setInstalled([...ids, gameId]);
      setSteam(await getSteamInstall(gameId));
      const checking = managed && checkLinkedInstall(gameId);
      push({
        type: "success",
        title: t("actions.game.located.title", { game: game.name }),
        text: checking
          ? t("actions.game.located.checking")
          : managed
            ? t("actions.game.located.linkedUnchecked")
            : t("actions.game.located.linked"),
      });
    } else if (r.error) {
      push({
        type: "warning",
        title: t("actions.game.locateFailed", { game: game.name }),
        text: r.error,
      });
    }
  };

  const confirmMove = () =>
    openConfirm({
      title: t("actions.game.move.title", { game: game.name }),
      message: t("actions.game.move.message"),
      confirmLabel: t("actions.game.move.confirm"),
      onConfirm: () => void moveGame(gameId),
    });

  const confirmDiscard = () =>
    openConfirm({
      title: t("actions.game.discard.title", { game: game.name }),
      message: t("actions.game.discard.message"),
      confirmLabel: t("actions.game.discard.confirm"),
      danger: true,
      onConfirm: () => {
        push({
          type: "progress",
          title: t("actions.game.discarding.title", { game: game.name }),
          text: t("actions.game.discarding.text"),
        });
        void uninstallGame(gameId).then((ok) => {
          if (!ok) return;
          const store = useGamesStore.getState();
          store.setPendingInstalls(
            Object.fromEntries(Object.entries(store.pendingInstalls).filter(([id]) => id !== gameId)),
          );
        });
      },
    });

  const confirmUninstall = async () => {
    if (partial) {
      confirmDiscard();
      return;
    }
    const modCount = await countMods(gameId);
    openConfirm({
      title: t("actions.game.uninstall.title", { game: game.name }),
      message: uninstallMessage(game.name, steamCopy, modCount),
      confirmLabel: t("common.uninstall"),
      danger: true,
      onConfirm: () => {
        push({
          type: "progress",
          title: t("actions.game.uninstalling.title", { game: game.name }),
          text: steamCopy
            ? t("actions.game.uninstalling.textSteam")
            : t("actions.game.uninstalling.text"),
        });
        void uninstallGame(gameId);
      },
    });
  };

  return {
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
    checkUpdates,
    quickRepair,
    confirmFullRepair,
    locate,
    confirmMove,
    confirmUninstall,
  };
}
