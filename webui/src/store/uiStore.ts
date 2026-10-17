// ------------ UI Store ------------
// The current page, active game, app version, online state and window state. The page and game are
// remembered across restarts.
import { create } from "zustand";
import { GAMES, type GameId } from "../data/games";
import type { BuildType } from "../lib/ipc";

export type View = "home" | "playtime" | "gallery" | "mods" | "settings" | "downloads";

const VIEWS: View[] = ["home", "playtime", "gallery", "mods", "settings", "downloads"];
const VIEW_KEY = "peebify.activeView";
const GAME_KEY = "peebify.activeGame";

function loadView(): View {
  try {
    const raw = localStorage.getItem(VIEW_KEY);
    return VIEWS.includes(raw as View) ? (raw as View) : "home";
  } catch {
    return "home";
  }
}

function loadGame(): GameId {
  try {
    const raw = localStorage.getItem(GAME_KEY);
    return GAMES.some((g) => g.id === raw) ? (raw as GameId) : GAMES[0].id;
  } catch {
    return GAMES[0].id;
  }
}

function remember(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
  }
}

interface UiState {
  activeGameId: GameId;
  activeView: View;
  appVersion: string;
  buildType: BuildType;
  isOnline: boolean;
  maximized: boolean;
  setActiveGame: (id: GameId) => void;
  setView: (view: View) => void;
  setAppVersion: (version: string) => void;
  setBuildType: (buildType: BuildType) => void;
  setOnline: (online: boolean) => void;
  setMaximized: (maximized: boolean) => void;
}

export const useUiStore = create<UiState>((set) => ({
  activeGameId: loadGame(),
  activeView: loadView(),
  appVersion: "",
  buildType: "stable",
  isOnline: true,
  maximized: false,
  setActiveGame: (id) => {
    remember(GAME_KEY, id);
    set({ activeGameId: id });
  },
  setView: (view) => {
    remember(VIEW_KEY, view);
    set({ activeView: view });
  },
  setAppVersion: (version) => set({ appVersion: version }),
  setBuildType: (buildType) => set({ buildType }),
  setOnline: (online) => set({ isOnline: online }),
  setMaximized: (maximized) => set({ maximized }),
}));
