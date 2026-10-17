// ------------ Session UI Store ------------
// Remembers which tab, range and game you had open on the Settings, Mods and Playtime pages while the
// launcher is running, so coming back to a page puts things back how you left them.
import { create } from "zustand";
import type { SettingsCategory } from "../data/settings";
import type { GameFilter, PlaytimeRange } from "../lib/playtime";
import type { GameId } from "../data/games";
import { useUiStore } from "./uiStore";

interface SessionUiState {
  settingsCat: SettingsCategory;
  settingsGameId: GameId;
  modsTab: "installed" | "browse";
  playtimeRange: PlaytimeRange;
  playtimeGame: GameFilter;
  setSettingsCat: (cat: SettingsCategory) => void;
  setSettingsGameId: (id: GameId) => void;
  setModsTab: (tab: "installed" | "browse") => void;
  setPlaytimeRange: (range: PlaytimeRange) => void;
  setPlaytimeGame: (game: GameFilter) => void;
}

export const useSessionUiStore = create<SessionUiState>((set) => ({
  settingsCat: "general",
  settingsGameId: useUiStore.getState().activeGameId,
  modsTab: "installed",
  playtimeRange: "Month",
  playtimeGame: "all",
  setSettingsCat: (settingsCat) => set({ settingsCat }),
  setSettingsGameId: (settingsGameId) => set({ settingsGameId }),
  setModsTab: (modsTab) => set({ modsTab }),
  setPlaytimeRange: (playtimeRange) => set({ playtimeRange }),
  setPlaytimeGame: (playtimeGame) => set({ playtimeGame }),
}));

useUiStore.subscribe((s, prev) => {
  if (s.activeGameId !== prev.activeGameId) {
    useSessionUiStore.setState({ settingsGameId: s.activeGameId });
  }
});
