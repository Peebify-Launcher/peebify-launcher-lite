// ------------ Mods Stores ------------
// The mods state the Mods page and the overlay share: the installed mods for the current game and its mod profiles.
import { create } from "zustand";
import {
  getLoaderSettings,
  getModsMasterEnabled,
  getModsStatus,
  listMods,
  type ModEntry,
  type ModProgress,
  type ModsStatus,
  applyModProfile,
  createModProfile,
  deleteModProfile,
  duplicateModProfile,
  listModProfiles,
  renameModProfile,
  setModProfileMembers,
  type ApplyProfileResult,
  type ModProfile,
} from "../lib/ipc";
import { backfillModThumbnails, checkModUpdates, type ModUpdate } from "../lib/gamebanana";

// ------------ Single Flight ------------
// One request per game at a time: a refresh asked for while one runs waits for it and then runs once more, and
// an answer that is already out of date is dropped.
interface Flight {
  done: Promise<void>;
  again: Promise<void> | null;
}

function singleFlight(
  flights: Map<string, Flight>,
  gameId: string,
  stillWanted: () => boolean,
  run: (stale: () => boolean) => Promise<void>,
): Promise<void> {
  const current = flights.get(gameId);
  if (current) {
    const next = async () => {
      if (stillWanted()) await singleFlight(flights, gameId, stillWanted, run);
    };
    return (current.again ??= current.done.then(next, next));
  }
  const flight = { again: null } as Flight;
  flight.done = run(() => flight.again !== null || !stillWanted()).finally(() =>
    flights.delete(gameId),
  );
  flights.set(gameId, flight);
  return flight.done.then(() => flight.again ?? undefined);
}

// ------------ Mods Store ------------
// The installed mods for the current game, the master mods switch, install progress, and update checks.
// Shared by the Mods page and the overlay. The overlay also counts, while it stays open, the mods changed that
// still need a reload in the game, and remembers the reload shortcut.
interface ModsState {
  status: ModsStatus | null;
  masterEnabled: boolean;
  mods: ModEntry[];
  modsError: boolean;
  loadedGameId: string | null;
  requestedGameId: string | null;
  progress: ModProgress | null;
  updates: ModUpdate[];
  updatesGameId: string | null;
  checkingUpdates: boolean;
  installing: Set<string>;
  refresh: (gameId: string) => Promise<void>;
  refreshIfCurrent: (gameId: string) => Promise<void>;
  prepare: (gameId: string) => Promise<{ status: ModsStatus; commit: () => void }>;
  applyFetched: (gameId: string, status: ModsStatus, listed: ModEntry[] | undefined) => void;
  beginInstall: (key: string) => boolean;
  endInstall: (key: string) => void;
  checkUpdates: (gameId: string, force: boolean) => Promise<ModUpdate[] | undefined>;
  dropUpdate: (modId: string) => void;
  hydrateMasterEnabled: () => Promise<void>;
  setMasterEnabled: (enabled: boolean) => void;
  setProgress: (progress: ModProgress | null) => void;
  setModsEnabledLocal: (folders: string[], enabled: boolean) => void;
  patchModLocal: (modId: string, patch: Partial<Pick<ModEntry, "enabled" | "folderName">>) => void;
  pending: Record<string, number>;
  reloadKeys: Record<string, string>;
  addPending: (gameId: string, count: number) => void;
  clearPending: (gameId: string) => void;
  loadReloadKey: (gameId: string) => Promise<void>;
}

let updateToken = 0;
const reloadKeysRequested = new Set<string>();
const backfilledGames = new Set<string>();
const autoCheckedGames = new Set<string>();

const modFlights = new Map<string, Flight>();

async function fetchMods(gameId: string) {
  const status = await getModsStatus(gameId);
  const listed = status.supportsMods && status.masterEnabled ? await listMods(gameId) : [];
  return { status, listed };
}

export function modInstallKey(gameId: string, gbModId: number, fileId: number): string {
  return `${gameId}:${gbModId}:${fileId}`;
}

export function activeInstallFor(installing: Set<string>, gameId: string): string | null {
  for (const key of installing) if (key.startsWith(`${gameId}:`)) return key;
  return null;
}

export const useModsStore = create<ModsState>((set, get) => ({
  status: null,
  masterEnabled: false,
  mods: [],
  modsError: false,
  loadedGameId: null,
  requestedGameId: null,
  progress: null,
  updates: [],
  updatesGameId: null,
  checkingUpdates: false,
  installing: new Set(),
  beginInstall: (key) => {
    if (get().installing.has(key)) return false;
    set((s) => ({ installing: new Set(s.installing).add(key) }));
    return true;
  },
  endInstall: (key) =>
    set((s) => {
      const installing = new Set(s.installing);
      installing.delete(key);
      return { installing };
    }),
  prepare: async (gameId) => {
    set({ requestedGameId: gameId });
    const { status, listed } = await fetchMods(gameId);
    return {
      status,
      commit: () => {
        if (get().requestedGameId === gameId) get().applyFetched(gameId, status, listed);
      },
    };
  },
  applyFetched: (gameId, status, listed) => {
    const mods = listed ?? [];
    set({
      status,
      masterEnabled: status.masterEnabled,
      mods,
      modsError: listed === undefined,
      loadedGameId: gameId,
    });

    const needsArt = mods.some((m) => m.source.kind === "gamebanana" && !m.thumbnailUrl);
    if (needsArt && !backfilledGames.has(gameId)) {
      backfilledGames.add(gameId);
      void backfillModThumbnails(gameId).then((updated) => {
        if (updated > 0) void get().refreshIfCurrent(gameId);
      });
    }

    const fromGameBanana = mods.some((m) => m.source.kind === "gamebanana");
    if (fromGameBanana && !autoCheckedGames.has(gameId)) {
      autoCheckedGames.add(gameId);
      void get().checkUpdates(gameId, false);
    }
  },
  refresh: (gameId) => {
    set({ requestedGameId: gameId });
    const wanted = () => get().requestedGameId === gameId;
    return singleFlight(modFlights, gameId, wanted, async (stale) => {
      const { status, listed } = await fetchMods(gameId);
      if (stale()) return;
      get().applyFetched(gameId, status, listed);
    });
  },
  refreshIfCurrent: async (gameId) => {
    if (get().requestedGameId !== gameId) return;
    await get().refresh(gameId);
  },
  checkUpdates: async (gameId, force) => {
    const token = ++updateToken;
    set({ checkingUpdates: true });
    const updates = await checkModUpdates(gameId, force);
    if (token !== updateToken) return updates;
    set({ checkingUpdates: false, ...(updates ? { updates, updatesGameId: gameId } : {}) });
    return updates;
  },
  dropUpdate: (modId) => set((s) => ({ updates: s.updates.filter((u) => u.modId !== modId) })),
  hydrateMasterEnabled: async () => set({ masterEnabled: await getModsMasterEnabled() }),
  setMasterEnabled: (enabled) => set({ masterEnabled: enabled }),
  setProgress: (progress) => set({ progress }),
  setModsEnabledLocal: (folders, enabled) =>
    set((s) => ({
      mods: s.mods.map((m) => (folders.includes(m.folderName) ? { ...m, enabled } : m)),
    })),
  patchModLocal: (modId, patch) =>
    set((s) => ({
      mods: s.mods.map((m) => (m.modId === modId ? { ...m, ...patch } : m)),
    })),
  pending: {},
  reloadKeys: {},
  addPending: (gameId, count) => {
    if (count <= 0) return;
    set((s) => ({ pending: { ...s.pending, [gameId]: (s.pending[gameId] ?? 0) + count } }));
  },
  clearPending: (gameId) => set((s) => ({ pending: { ...s.pending, [gameId]: 0 } })),
  loadReloadKey: async (gameId) => {
    if (reloadKeysRequested.has(gameId)) return;
    reloadKeysRequested.add(gameId);
    const { rows } = await getLoaderSettings(gameId);
    if (rows.length === 0) {
      reloadKeysRequested.delete(gameId);
      return;
    }
    const accelerator = rows.find((row) => row.id === "reloadMods")?.accelerator?.trim();
    if (!accelerator) return;
    set((s) => ({ reloadKeys: { ...s.reloadKeys, [gameId]: accelerator } }));
  },
}));

// ------------ Mod Profiles Store ------------
// A game's mod profiles and which one is active, plus create, rename, duplicate, delete and apply.
// Overlapping refreshes are merged into one request.
interface ModProfilesState {
  profiles: ModProfile[];
  activeId: string;
  loadedGameId: string | null;
  requestedGameId: string | null;
  applying: string | null;
  refresh: (gameId: string) => Promise<void>;
  prepare: (gameId: string) => Promise<() => void>;
  refreshIfCurrent: (gameId: string) => Promise<void>;
  create: (gameId: string, name: string) => Promise<ModProfile | null>;
  rename: (gameId: string, profileId: string, name: string) => Promise<void>;
  remove: (gameId: string, profileId: string) => Promise<void>;
  duplicate: (gameId: string, profileId: string) => Promise<void>;
  apply: (gameId: string, profileId: string) => Promise<ApplyProfileResult | undefined>;
  setMembers: (
    gameId: string,
    profileId: string,
    add: string[],
    remove: string[],
  ) => Promise<boolean>;
}

const profileFlights = new Map<string, Flight>();

function showing(s: ModProfilesState, gameId: string): boolean {
  return s.loadedGameId === gameId && s.requestedGameId === gameId;
}

export const useModProfilesStore = create<ModProfilesState>((set, get) => ({
  profiles: [],
  activeId: "",
  loadedGameId: null,
  requestedGameId: null,
  applying: null,

  prepare: async (gameId) => {
    set({ requestedGameId: gameId });
    const list = await listModProfiles(gameId);
    return () => {
      if (get().requestedGameId !== gameId) return;
      set({ profiles: list?.profiles ?? [], activeId: list?.activeId ?? "", loadedGameId: gameId });
    };
  },

  refresh: (gameId) => {
    set({ requestedGameId: gameId });
    const wanted = () => get().requestedGameId === gameId;
    return singleFlight(profileFlights, gameId, wanted, async (stale) => {
      const list = await listModProfiles(gameId);
      if (stale()) return;
      set({
        profiles: list?.profiles ?? [],
        activeId: list?.activeId ?? "",
        loadedGameId: gameId,
      });
    });
  },

  refreshIfCurrent: async (gameId) => {
    if (get().requestedGameId !== gameId) return;
    await get().refresh(gameId);
  },

  create: async (gameId, name) => {
    const created = await createModProfile(gameId, name);
    if (!created) return null;
    await get().refreshIfCurrent(gameId);
    return created;
  },

  rename: async (gameId, profileId, name) => {
    const previous = get().profiles;
    set((s) => ({
      profiles: s.profiles.map((p) => (p.id === profileId ? { ...p, name } : p)),
    }));
    const ok = await renameModProfile(gameId, profileId, name);
    if (!ok) set({ profiles: previous });
  },

  remove: async (gameId, profileId) => {
    const ok = await deleteModProfile(gameId, profileId);
    if (ok) await get().refreshIfCurrent(gameId);
  },

  duplicate: async (gameId, profileId) => {
    const copy = await duplicateModProfile(gameId, profileId);
    if (copy) await get().refreshIfCurrent(gameId);
  },

  apply: async (gameId, profileId) => {
    if (get().applying) return undefined;
    set({ applying: profileId });
    try {
      const result = await applyModProfile(gameId, profileId);
      if (result && showing(get(), gameId)) set({ activeId: result.activeId });
      await get().refreshIfCurrent(gameId);
      return result;
    } finally {
      set({ applying: null });
    }
  },

  setMembers: async (gameId, profileId, add, removeIds) => {
    const updated = await setModProfileMembers(gameId, profileId, add, removeIds);
    if (!updated) return false;
    if (showing(get(), gameId)) {
      set((s) => ({
        profiles: s.profiles.map((p) =>
          p.id === profileId
            ? {
                ...p,
                ...updated,
                modCount: updated.modCount ?? updated.modIds.length,
                resolvedCount: updated.resolvedCount ?? p.resolvedCount,
              }
            : p,
        ),
      }));
    }
    await get().refreshIfCurrent(gameId);
    return true;
  },
}));

