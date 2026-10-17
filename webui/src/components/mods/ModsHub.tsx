// ------------ Mods Library ------------
// The Library view inside Mods. Ties together the mod list and the profiles list for one game, and handles
// turning mods on and off, uninstalling, checking for updates, rescanning the mods folder and dropping in mod
// files.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowUpCircle, FolderOpen, RefreshCw, Upload } from "lucide-react";
import { updateGameBananaMod } from "../../lib/gamebanana";
import { activeInstallFor, useModsStore, useModProfilesStore } from "../../store/mods";
import { useModalStore } from "../../store/modalStore";
import { useNotificationStore } from "../../store/notifications";
import {
  deleteMod,
  importModArchive,
  openModsFolder,
  setModEnabled,
  type ModEntry,
} from "../../lib/ipc";
import { onFileDrop } from "../../lib/tauri";
import { useTauriEvent } from "../../lib/hooks";
import { gameById, type GameId } from "../../data/games";
import { ActionButton } from "../ui/controls";
import { ModsHubProfiles } from "./ModsHubProfiles";
import { ModsHubList } from "./ModsHubList";
import { useModProgress, useSelectedProfile } from "./ModsHubRow";
import { useT } from "../../lib/useT";

type Translate = ReturnType<typeof useT>;

function rescanSummary(before: ModEntry[], after: ModEntry[], t: Translate) {
  const previous = new Map(before.map((m) => [m.modId, m]));
  const current = new Set(after.map((m) => m.modId));
  const previousByFolder = new Map(before.map((m) => [m.folderName, m]));
  const added = after.filter((m) => !previous.has(m.modId)).length;
  const removed = before.filter((m) => !current.has(m.modId)).length;
  const switched = after.filter((m) => {
    const prev = previous.get(m.modId);
    return prev !== undefined && prev.enabled !== m.enabled;
  }).length;
  const renamed = after.filter((m) => {
    const prev = previousByFolder.get(m.folderName);
    return prev !== undefined && prev.name !== m.name;
  }).length;

  const bits: string[] = [];
  if (added) bits.push(t("mods.hub.rescan.newMods", { count: added }));
  if (removed) bits.push(t("mods.hub.rescan.goneMods", { count: removed }));
  if (switched) bits.push(t("mods.hub.rescan.switched", { count: switched }));
  if (renamed) bits.push(t("mods.hub.rescan.renamed", { count: renamed }));
  if (!bits.length) return null;
  return bits.length === 1
    ? bits[0]
    : t("mods.hub.rescan.listLast", {
        rest: bits.slice(0, -1).join(t("mods.hub.rescan.listSeparator")),
        last: bits[bits.length - 1],
      });
}

const stillInstalling = (t: Translate) =>
  ({
    type: "warning",
    title: t("mods.hub.stillInstallingTitle"),
    text: t("mods.hub.stillInstallingText"),
  }) as const;

export interface BrowseSeed {
  modId: number;
  name: string;
  thumbnailUrl: string | null;
}

export function ModsHub({
  gameId,
  onBrowse,
  onOpenGameBanana,
}: {
  gameId: GameId;
  onBrowse?: () => void;
  onOpenGameBanana?: (seed: BrowseSeed) => void;
}) {
  const t = useT();
  const openConfirm = useModalStore((s) => s.openConfirm);
  const push = useNotificationStore((s) => s.push);

  const status = useModsStore((s) => s.status);
  const mods = useModsStore((s) => s.mods);
  const modsError = useModsStore((s) => s.modsError);
  const loadedGameId = useModsStore((s) => s.loadedGameId);
  const refreshMods = useModsStore((s) => s.refresh);
  const refreshModsIfCurrent = useModsStore((s) => s.refreshIfCurrent);
  const setProgress = useModsStore((s) => s.setProgress);
  const patchModLocal = useModsStore((s) => s.patchModLocal);
  const updatesList = useModsStore((s) => s.updates);
  const updatesGameId = useModsStore((s) => s.updatesGameId);
  const checkingUpdates = useModsStore((s) => s.checkingUpdates);
  const checkUpdates = useModsStore((s) => s.checkUpdates);
  const dropUpdate = useModsStore((s) => s.dropUpdate);
  const beginInstall = useModsStore((s) => s.beginInstall);
  const endInstall = useModsStore((s) => s.endInstall);
  const downloadingKey = useModsStore((s) => activeInstallFor(s.installing, gameId));
  const [updatingModId, setUpdatingModId] = useState<string | null>(null);
  const [updatingAll, setUpdatingAll] = useState(false);

  const activeId = useModProfilesStore((s) => s.activeId);
  const applying = useModProfilesStore((s) => s.applying);
  const refreshProfiles = useModProfilesStore((s) => s.refresh);
  const refreshProfilesIfCurrent = useModProfilesStore((s) => s.refreshIfCurrent);
  const setMembers = useModProfilesStore((s) => s.setMembers);

  const [selectedProfileId, setSelectedProfileId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [rescanning, setRescanning] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [togglingModIds, setTogglingModIds] = useState<ReadonlySet<string>>(() => new Set());
  const togglingRef = useRef(new Set<string>());

  const game = gameById(gameId);
  const loaded = status !== null && loadedGameId === gameId;
  const toolsInstalled = loaded && status.toolchainInstalled;
  const canApply = toolsInstalled && status.gameEnabled;

  const refreshAll = useCallback(async () => {
    await Promise.all([refreshModsIfCurrent(gameId), refreshProfilesIfCurrent(gameId)]);
  }, [gameId, refreshModsIfCurrent, refreshProfilesIfCurrent]);

  useEffect(() => {
    setSelectedProfileId(null);
    void refreshProfiles(gameId);
  }, [gameId, refreshProfiles]);

  useModProgress(gameId, () => void refreshProfilesIfCurrent(gameId));

  const rescan = useCallback(async () => {
    setRescanning(true);
    const before = useModsStore.getState().mods;
    try {
      await Promise.all([refreshMods(gameId), refreshProfiles(gameId)]);
    } finally {
      setRescanning(false);
    }
    const changes = rescanSummary(before, useModsStore.getState().mods, t);
    if (changes)
      push({
        type: "success",
        title: t("mods.hub.rescan.updatedTitle"),
        text: t("mods.hub.rescan.updatedText", { changes }),
      });
    else
      useNotificationStore.getState().toast({
        title: t("mods.hub.rescan.nothingTitle"),
        text: t("mods.hub.rescan.nothingText", { game: game.name }),
      });
  }, [gameId, refreshMods, refreshProfiles, push, game.name, t]);

  const runImport = useCallback(
    async (paths?: string[]) => {
      const key = `${gameId}:import`;
      if (
        updatingModId !== null ||
        activeInstallFor(useModsStore.getState().installing, gameId) !== null ||
        !beginInstall(key)
      ) {
        push(stillInstalling(t));
        return;
      }
      setBusy(true);
      let result: Awaited<ReturnType<typeof importModArchive>>;
      try {
        result = await importModArchive(gameId, paths);
      } finally {
        endInstall(key);
        setBusy(false);
      }
      if (result === "cancelled") return;
      if (result) {
        const n = result.installed.length;
        push({
          type: result.failed.length ? "warning" : "success",
          title: n ? t("mods.hub.importedTitle", { count: n }) : t("mods.hub.nothingInstalled"),
          text: result.failed.length ? result.failed.join("; ") : result.installed.join(", "),
        });
        return;
      }
      await refreshAll();
    },
    [gameId, updatingModId, beginInstall, endInstall, push, refreshAll, t],
  );

  useTauriEvent(onFileDrop, (hovering, paths) => {
    setDragging(hovering);
    if (!paths.length) return;
    const archives = paths.filter((p) => /\.(zip|7z|rar)$/i.test(p));
    if (archives.length) void runImport(archives);
    else
      push({
        type: "warning",
        title: t("mods.hub.notModFileTitle"),
        text: t("mods.hub.notModFileText"),
      });
  });

  const profile = useSelectedProfile(selectedProfileId);

  const updates = useMemo(
    () =>
      new Map(
        (updatesGameId === gameId ? updatesList : [])
          .filter((u) => mods.some((m) => m.modId === u.modId))
          .map((u) => [u.modId, u] as const),
      ),
    [updatesList, updatesGameId, gameId, mods],
  );
  const hasGameBananaMods = mods.some((m) => m.source.kind === "gamebanana");

  const runCheckUpdates = async () => {
    const found = await checkUpdates(gameId, true);
    if (found === undefined) {
      push({
        type: "warning",
        title: t("mods.hub.checkFailedTitle"),
        text: t("mods.hub.checkFailedText"),
      });
      return;
    }
    const n = found.length;
    if (n)
      push({
        type: "success",
        title: t("mods.hub.updatesAvailable", { count: n }),
        text: found.map((u) => u.name).join(", "),
      });
    else
      useNotificationStore
        .getState()
        .toast({
          type: "success",
          title: t("mods.hub.allCurrentTitle"),
          text: t("mods.hub.allCurrentText", { game: game.name }),
        });
  };

  const takeUpdateLock = () => {
    const key = `${gameId}:update`;
    if (activeInstallFor(useModsStore.getState().installing, gameId) !== null || !beginInstall(key)) {
      push(stillInstalling(t));
      return null;
    }
    return key;
  };

  const updateOne = async (mod: ModEntry) => {
    if (updatingModId) return;
    const key = takeUpdateLock();
    if (!key) return;
    setUpdatingModId(mod.modId);
    let folder: string | undefined;
    try {
      folder = await updateGameBananaMod(gameId, mod.modId);
    } finally {
      endInstall(key);
      setUpdatingModId(null);
    }
    setProgress(null);
    if (folder) {
      dropUpdate(mod.modId);
      push({
        type: "success",
        title: t("mods.toast.updatedTitle"),
        text: t("mods.hub.updatedNewest", { name: mod.name }),
      });
      return;
    }
    await refreshAll();
  };

  const updateAll = async () => {
    if (updatingAll || updatingModId) return;
    const key = takeUpdateLock();
    if (!key) return;
    setUpdatingAll(true);
    let tried = 0;
    let done = 0;
    try {
      for (const entry of updates.values()) {
        const mod = mods.find((m) => m.modId === entry.modId);
        if (!mod) continue;
        setUpdatingModId(mod.modId);
        tried += 1;
        const folder = await updateGameBananaMod(gameId, mod.modId);
        if (folder) {
          dropUpdate(mod.modId);
          done += 1;
        }
      }
    } finally {
      endInstall(key);
      setUpdatingModId(null);
      setUpdatingAll(false);
    }
    setProgress(null);
    push({
      type: done ? "success" : "warning",
      title: done ? t("mods.hub.updatedAllTitle", { count: done }) : t("mods.hub.nothingUpdated"),
      text: done
        ? t("mods.hub.updatedAllText", { game: game.name })
        : t("mods.hub.updatedAllFailed"),
    });
    if (done < tried) await refreshAll();
  };

  const toggleEnabled = async (mod: ModEntry, on: boolean) => {
    if (togglingRef.current.has(mod.modId)) return;
    togglingRef.current.add(mod.modId);
    setTogglingModIds(new Set(togglingRef.current));
    patchModLocal(mod.modId, { enabled: on });
    let folderName: string | undefined;
    try {
      folderName = await setModEnabled(gameId, mod.folderName, on);
      if (folderName) patchModLocal(mod.modId, { enabled: on, folderName });
    } finally {
      togglingRef.current.delete(mod.modId);
      setTogglingModIds(new Set(togglingRef.current));
      if (!folderName) void refreshModsIfCurrent(gameId);
    }
  };

  const setMembership = async (mod: ModEntry, inProfile: boolean) => {
    if (!profile) return;
    setBusy(true);
    try {
      const ok = await setMembers(
        gameId,
        profile.id,
        inProfile ? [mod.modId] : [],
        inProfile ? [] : [mod.modId],
      );
      if (ok && profile.id === activeId && mod.enabled !== inProfile) {
        await toggleEnabled(mod, inProfile);
      }
    } finally {
      setBusy(false);
    }
  };

  const uninstall = (mod: ModEntry) =>
    openConfirm({
      title: t("mods.hub.uninstallTitle", { name: mod.name }),
      message: t("mods.hub.uninstallMessage"),
      confirmLabel: t("common.uninstall"),
      danger: true,
      onConfirm: () => {
        void (async () => {
          setBusy(true);
          let deleted = false;
          try {
            deleted = await deleteMod(gameId, mod.folderName, mod.modId);
          } finally {
            setBusy(false);
          }
          if (!deleted) await refreshAll();
        })();
      },
    });

  return (
    <div
      className={`rounded-ui transition ${
        dragging ? "outline outline-2 outline-(--accent-b)/60" : ""
      }`}
    >
      <div className="mb-4 flex flex-wrap items-center gap-2">
        <ActionButton
          icon={<Upload size={15} />}
          disabled={busy || downloadingKey !== null || updatingModId !== null}
          onClick={() => void runImport()}
        >
          {t("mods.hub.importMod")}
        </ActionButton>
        <ActionButton
          icon={<FolderOpen size={15} />}
          disabled={!toolsInstalled}
          onClick={() => void openModsFolder(gameId)}
        >
          {t("common.openFolder")}
        </ActionButton>
        <ActionButton
          icon={<RefreshCw size={15} className={rescanning ? "animate-spin" : ""} />}
          disabled={rescanning || !loaded}
          onClick={() => void rescan()}
        >
          {t("common.refresh")}
        </ActionButton>
        {hasGameBananaMods && (
          <ActionButton
            icon={<ArrowUpCircle size={15} className={checkingUpdates ? "animate-spin" : ""} />}
            disabled={checkingUpdates || !loaded || busy}
            onClick={() => void runCheckUpdates()}
          >
            {checkingUpdates ? t("common.checking") : t("common.checkForUpdates")}
          </ActionButton>
        )}
        {updates.size > 1 && (
          <ActionButton
            variant="accent"
            disabled={updatingAll || !!updatingModId || downloadingKey !== null || busy}
            onClick={() => void updateAll()}
          >
            {updatingAll ? t("common.updating") : t("mods.hub.updateAll", { count: updates.size })}
          </ActionButton>
        )}
      </div>

      <div className="grid gap-4 [grid-template-columns:240px_minmax(0,1fr)]">
        <ModsHubProfiles
          gameId={gameId}
          canApply={canApply}
          selectedId={profile?.id ?? null}
          disabled={busy || !loaded}
          onSelect={setSelectedProfileId}
        />
        <ModsHubList
          game={game}
          loaded={loaded}
          mods={mods}
          modsError={modsError}
          profile={profile}
          profileIsActive={profile !== null && profile.id === activeId}
          busy={busy || !!applying}
          togglingModIds={togglingModIds}
          downloadingKey={downloadingKey}
          updates={updates}
          updatingModId={updatingModId}
          onUpdate={(mod) => void updateOne(mod)}
          onBrowse={onBrowse}
          onRetryMods={() => void refreshMods(gameId)}
          onToggleEnabled={(mod, on) => void toggleEnabled(mod, on)}
          onAddToProfile={(mod) => void setMembership(mod, true)}
          onRemoveFromProfile={(mod) => void setMembership(mod, false)}
          onUninstall={uninstall}
          onOpenGameBanana={
            onOpenGameBanana
              ? (mod) => {
                  if (mod.source.gbModId != null) {
                    onOpenGameBanana({
                      modId: mod.source.gbModId,
                      name: mod.name,
                      thumbnailUrl: mod.thumbnailUrl,
                    });
                  }
                }
              : undefined
          }
        />
      </div>

      {dragging && (
        <div className="pointer-events-none fixed inset-0 z-(--z-banner) grid place-items-center">
          <div className="glass rounded-ui px-8 py-5 text-[15px] font-medium">
            {t("mods.hub.dropToImport")}
          </div>
        </div>
      )}
    </div>
  );
}
