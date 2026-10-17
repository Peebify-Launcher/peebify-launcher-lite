// ------------ Library Store ------------
// Which games are shown in the sidebar and in what order, and whether first run setup is done.
import { create } from "zustand";
import { GAMES, type GameId } from "../data/games";
import { getLibrary, saveLibrary } from "../lib/ipc";
import { useUiStore } from "./uiStore";

const ALL_IDS = GAMES.map((g) => g.id);

const inCanonicalOrder = (ids: GameId[]) => ALL_IDS.filter((id) => ids.includes(id));

interface LibraryPreview {
  source: string;
  order: GameId[];
}

interface LibraryState {
  visible: GameId[];
  preview: LibraryPreview | null;
  setupComplete: boolean;
  hydrated: boolean;
  hydrate: () => Promise<void>;
  show: (id: GameId) => void;
  hide: (id: GameId) => void;
  move: (id: GameId, toIndex: number) => void;
  setPreview: (source: string, order: GameId[] | null) => void;
  finishSetup: (ids: GameId[]) => void;
}

export const useLibraryStore = create<LibraryState>((set, get) => {
  const persist = (visible: GameId[], setupComplete: boolean) => {
    set({ visible, setupComplete, preview: null });
    void saveLibrary({ visible, setupComplete });
    keepActiveGameVisible(visible);
  };

  return {
    visible: [],
    preview: null,
    setupComplete: false,
    hydrated: false,
    hydrate: async () => {
      try {
        const library = await getLibrary();
        set({
          visible: library.visible,
          setupComplete: library.setupComplete,
        });
        keepActiveGameVisible(library.visible);
      } finally {
        set({ hydrated: true });
      }
    },
    show: (id) => {
      const { visible, setupComplete } = get();
      if (visible.includes(id)) return;
      persist([...visible, id], setupComplete);
    },
    hide: (id) => {
      const { visible, setupComplete } = get();
      if (visible.length <= 1 || !visible.includes(id)) return;
      persist(
        visible.filter((x) => x !== id),
        setupComplete,
      );
    },
    move: (id, toIndex) => {
      const { visible, setupComplete } = get();
      const from = visible.indexOf(id);
      if (from < 0) return;
      const target = Math.max(0, Math.min(visible.length - 1, toIndex));
      if (from === target) return;
      const next = [...visible];
      next.splice(from, 1);
      next.splice(target, 0, id);
      persist(next, setupComplete);
    },
    setPreview: (source, order) => {
      const current = get().preview;
      if (!order) {
        if (current?.source === source) set({ preview: null });
        return;
      }
      set({ preview: { source, order } });
    },
    finishSetup: (ids) => {
      const picked = inCanonicalOrder(ids);
      persist(picked.length > 0 ? picked : ALL_IDS, true);
    },
  };
});

function keepActiveGameVisible(visible: GameId[]) {
  if (visible.length === 0) return;
  const ui = useUiStore.getState();
  if (!visible.includes(ui.activeGameId)) ui.setActiveGame(visible[0]);
}
