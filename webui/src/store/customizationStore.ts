// ------------ Customization Store ------------
// Per-game looks and settings: wallpaper, game icon, graphics API, download quality, voice pack and
// playtime color. Saves to the backend as you change them.
import { useCallback } from "react";
import { create } from "zustand";
import { gameColor, type GraphicsApi, type ResourceQuality } from "../data/games";
import {
  getCustomization,
  saveGameIcon,
  saveWallpaper,
  setConfigValue,
  type MediaSlice,
  type VoicePackLanguage,
} from "../lib/ipc";

interface CustomizationState {
  wallpaper: Record<string, MediaSlice>;
  gameIcons: Record<string, MediaSlice>;
  graphicsApi: Record<string, GraphicsApi>;
  resourceQuality: Record<string, ResourceQuality>;
  voicePack: Record<string, VoicePackLanguage>;
  playtimeColor: Record<string, string>;
  hydrated: boolean;
  hydrate: () => Promise<void>;
  setWallpaper: (gameId: string, slice: MediaSlice) => void;
  setGameIcon: (gameId: string, slice: MediaSlice) => void;
  setGraphicsApi: (gameId: string, api: GraphicsApi) => void;
  setResourceQuality: (gameId: string, quality: ResourceQuality) => Promise<void>;
  setVoicePack: (gameId: string, language: VoicePackLanguage) => Promise<void>;
  setPlaytimeColor: (gameId: string, color: string | null) => void;
}

const PLAYTIME_COLOR_PERSIST_DELAY_MS = 300;
const playtimeColorTimers = new Map<string, number>();

type MediaSection = "wallpaper" | "gameIcons";

const MEDIA_SAVE = { wallpaper: saveWallpaper, gameIcons: saveGameIcon };

async function saveMediaSlice(
  section: MediaSection,
  gameId: string,
  slice: MediaSlice,
  previous: MediaSlice | undefined,
): Promise<void> {
  if (await MEDIA_SAVE[section](gameId, slice)) return;
  useCustomizationStore.setState((s) => {
    if (s[section][gameId] !== slice) return {};
    const next = { ...s[section] };
    if (previous) next[gameId] = previous;
    else delete next[gameId];
    return section === "wallpaper" ? { wallpaper: next } : { gameIcons: next };
  });
}

export const useCustomizationStore = create<CustomizationState>((set, get) => ({
  wallpaper: {},
  gameIcons: {},
  graphicsApi: {},
  resourceQuality: {},
  voicePack: {},
  playtimeColor: {},
  hydrated: false,
  hydrate: async () => {
    try {
      const c = await getCustomization();
      set({
        wallpaper: c.wallpaper,
        gameIcons: c.gameIcons,
        graphicsApi: c.graphicsApi,
        resourceQuality: c.resourceQuality,
        voicePack: c.voicePack,
        playtimeColor: c.playtimeColor,
      });
    } finally {
      set({ hydrated: true });
    }
  },
  setWallpaper: (gameId, slice) => {
    const previous = get().wallpaper[gameId];
    set((s) => ({ wallpaper: { ...s.wallpaper, [gameId]: slice } }));
    void saveMediaSlice("wallpaper", gameId, slice, previous);
  },
  setGameIcon: (gameId, slice) => {
    const previous = get().gameIcons[gameId];
    set((s) => ({ gameIcons: { ...s.gameIcons, [gameId]: slice } }));
    void saveMediaSlice("gameIcons", gameId, slice, previous);
  },
  setGraphicsApi: (gameId, api) => {
    set((s) => ({ graphicsApi: { ...s.graphicsApi, [gameId]: api } }));
    void setConfigValue(`games.${gameId}.graphicsApi`, api);
  },
  setResourceQuality: (gameId, quality) => {
    set((s) => ({ resourceQuality: { ...s.resourceQuality, [gameId]: quality } }));
    return setConfigValue(`games.${gameId}.resourceQuality`, quality);
  },
  setPlaytimeColor: (gameId, color) => {
    set((s) => {
      const next = { ...s.playtimeColor };
      if (color) next[gameId] = color;
      else delete next[gameId];
      return { playtimeColor: next };
    });
    const pending = playtimeColorTimers.get(gameId);
    if (pending !== undefined) window.clearTimeout(pending);
    playtimeColorTimers.delete(gameId);
    if (!color) {
      void setConfigValue(`games.${gameId}.playtimeColor`, "");
      return;
    }
    playtimeColorTimers.set(
      gameId,
      window.setTimeout(() => {
        playtimeColorTimers.delete(gameId);
        void setConfigValue(`games.${gameId}.playtimeColor`, color);
      }, PLAYTIME_COLOR_PERSIST_DELAY_MS),
    );
  },
  setVoicePack: (gameId, language) => {
    set((s) => ({ voicePack: { ...s.voicePack, [gameId]: language } }));
    return setConfigValue(`games.${gameId}.voicePackLanguage`, language);
  },
}));

export function useGameColor(): (id: string) => string {
  const overrides = useCustomizationStore((s) => s.playtimeColor);
  return useCallback((id: string) => gameColor(id, overrides), [overrides]);
}
