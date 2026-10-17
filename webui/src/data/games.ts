// ------------ Games ------------
// Every game the launcher knows about, the types that describe one, and each game's chart color.
import { t } from "../lib/i18n";

// ------------ Game Types ------------
// The list of supported game ids and the shape of a game entry, plus the graphics API and quality choices.
export type GameId =
  | "wuwa"
  | "zzz"
  | "hsr"
  | "nte"
  | "endfield"
  | "genshin"
  | "hi3"
  | "pgr"
  | "gf2"
  | "gf1"
  | "re1999"
  | "bd2"
  | "arknights"
  | "bluearchive"
  | "dna";

export type GraphicsApi = "dx11" | "dx12";

export const GRAPHICS_APIS: { value: GraphicsApi; label: string }[] = [
  { value: "dx11", label: "DX11" }, // i18n-ignore
  { value: "dx12", label: "DX12" }, // i18n-ignore
];

export type ResourceQuality = "sd" | "hd" | "uhd";

export interface Game {
  id: GameId;
  short: string;
  name: string;
  icon: string;
  wallpaperStatic: string;
  wallpaperFallback: string;
  installHelpUrl?: string;
  managed?: boolean;
  betaNote?: string;
  graphicsApiChoice?: boolean;
  graphicsApiDefault?: GraphicsApi;
  resourceQualityChoice?: boolean;
  voicePackChoice?: boolean;
  contentPackChoice?: boolean;
  supportsQuickRepair?: boolean;
  fpsUnlock?: boolean;
}

// ------------ Game List ------------
// Every game the launcher knows about: its name, icon, wallpaper fallback, and which extras it supports
// (graphics API choice, voice packs, quality, FPS unlock and so on). Add a game here and the UI picks it up.
export const GAMES: Game[] = [
  {
    id: "wuwa",
    short: "WW",
    name: "Wuthering Waves",
    icon: "/icons/wuwa.webp",
    wallpaperStatic: "/icons/wallpaper_wuwa_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 12%, rgba(45,212,191,.45), rgba(45,212,191,0) 55%), linear-gradient(160deg,#0c2a2b 0%,#0a0e14 70%)",
    graphicsApiChoice: true,
    graphicsApiDefault: "dx12",
    resourceQualityChoice: true,
  },
  {
    id: "zzz",
    short: "ZZ",
    name: "Zenless Zone Zero",
    icon: "/icons/zzz.webp",
    wallpaperStatic: "/icons/wallpaper_zzz_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(255,212,0,.4), transparent 55%), linear-gradient(160deg,#241f08 0%,#0c0a08 70%)",
    graphicsApiChoice: true,
    voicePackChoice: true,
  },
  {
    id: "hsr",
    short: "HSR",
    name: "Honkai: Star Rail",
    icon: "/icons/honkaistarrail.webp",
    wallpaperStatic: "/icons/wallpaper_honkaistarrail_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(150,130,255,.45), transparent 55%), linear-gradient(160deg,#181433 0%,#0a0a14 70%)",
    voicePackChoice: true,
  },
  {
    id: "nte",
    short: "NtE",
    name: "Neverness to Everness",
    icon: "/icons/nte.webp",
    wallpaperStatic: "/icons/wallpaper_nte_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(255,93,158,.42), transparent 55%), linear-gradient(160deg,#2a1020 0%,#0d0810 70%)",
    contentPackChoice: true,
  },
  {
    id: "endfield",
    short: "AE",
    name: "Arknights: Endfield",
    icon: "/icons/endfield.webp",
    wallpaperStatic: "/icons/wallpaper_endfield_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(255,196,0,.38), transparent 55%), linear-gradient(160deg,#26200a 0%,#0d0c08 70%)",
    supportsQuickRepair: false,
  },
  {
    id: "genshin",
    short: "GI",
    name: "Genshin Impact",
    icon: "/icons/genshin.webp",
    wallpaperStatic: "/icons/wallpaper_genshin_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(120,180,255,.42), transparent 55%), linear-gradient(160deg,#101c33 0%,#0a0d14 70%)",
    voicePackChoice: true,
    fpsUnlock: true,
  },
  {
    id: "hi3",
    short: "HI3",
    name: "Honkai Impact 3rd",
    icon: "/icons/honkai3d.webp",
    wallpaperStatic: "/icons/wallpaper_hi3_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(192,132,252,.42), transparent 55%), linear-gradient(160deg,#1d1233 0%,#0b0912 70%)",
    voicePackChoice: true,
  },
  {
    id: "pgr",
    short: "PGR",
    name: "Punishing: Gray Raven",
    icon: "/icons/punishing.webp",
    wallpaperStatic: "/icons/wallpaper_pgr_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(255,107,87,.4), transparent 55%), linear-gradient(160deg,#2a1410 0%,#0f0a09 70%)",
    graphicsApiChoice: true,
  },
  {
    id: "gf2",
    short: "GF2",
    name: "Girls' Frontline 2: Exilium",
    icon: "/icons/gfl2.webp",
    wallpaperStatic: "/icons/wallpaper_gfl2_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(214,92,72,.36), transparent 55%), linear-gradient(160deg,#241312 0%,#0c0a0a 70%)",
  },
  {
    id: "gf1",
    short: "GF",
    name: "Girls' Frontline",
    icon: "/icons/gf1.webp",
    wallpaperStatic: "/icons/wallpaper_gf1_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(198,64,78,.34), transparent 55%), linear-gradient(160deg,#1f171a 0%,#0b0a0b 70%)",
    installHelpUrl: "https://store.steampowered.com/app/3887700/Girls_Frontline/",
    managed: false,
    get betaNote() {
      return t("games.notes.steamOnly", { game: "Girls' Frontline" });
    },
  },
  {
    id: "re1999",
    short: "R1999",
    name: "Reverse: 1999",
    icon: "/icons/re1999.webp",
    wallpaperStatic: "/icons/wallpaper_re1999_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(201,162,39,.38), transparent 55%), linear-gradient(160deg,#241f14 0%,#0c0a09 70%)",
    supportsQuickRepair: false,
    get betaNote() {
      return t("games.notes.re1999");
    },
  },
  {
    id: "bd2",
    short: "BD2",
    name: "Brown Dust II",
    icon: "/icons/bd2.webp",
    wallpaperStatic: "/icons/wallpaper_bd2_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(133,121,201,.42), transparent 55%), linear-gradient(160deg,#1d1930 0%,#0b0a12 70%)",
    supportsQuickRepair: false,
    get betaNote() {
      return t("games.notes.bd2");
    },
  },
  {
    id: "arknights",
    short: "AK",
    name: "Arknights",
    icon: "/icons/arknights.webp",
    wallpaperStatic: "/icons/wallpaper_arknights_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(59,130,246,.36), transparent 55%), linear-gradient(160deg,#111827 0%,#0a0b0e 70%)",
    graphicsApiChoice: true,
  },
  {
    id: "bluearchive",
    short: "BA",
    name: "Blue Archive",
    icon: "/icons/bluearchive.webp",
    wallpaperStatic: "/icons/wallpaper_bluearchive_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(56,189,248,.36), transparent 55%), linear-gradient(160deg,#10243a 0%,#080d14 70%)",
    installHelpUrl: "https://store.steampowered.com/app/3557620/Blue_Archive/",
    managed: false,
    get betaNote() {
      return t("games.notes.steamOnly", { game: "Blue Archive" });
    },
  },
  {
    id: "dna",
    short: "DNA",
    name: "Duet Night Abyss",
    icon: "/icons/dna.webp",
    wallpaperStatic: "/icons/wallpaper_dna_noneanimated.webp",
    wallpaperFallback:
      "radial-gradient(120% 95% at 80% 10%, rgba(201,169,110,.34), transparent 55%), linear-gradient(160deg,#141a2e 0%,#090a12 70%)",
    graphicsApiChoice: true,
    graphicsApiDefault: "dx12",
    supportsQuickRepair: false,
    get betaNote() {
      return t("games.notes.dna");
    },
  },
];

export const gameById = (id: GameId): Game =>
  GAMES.find((g) => g.id === id) ?? GAMES[0];

export function gameMeta(id: string | null): { name: string; icon: string | null } {
  if (!id) return { name: t("games.other"), icon: null };
  const game = GAMES.find((g) => g.id === id);
  return game ? { name: game.name, icon: game.icon } : { name: id, icon: null };
}

export const isManaged = (id: GameId): boolean => gameById(id).managed !== false;
export const supportsQuickRepair = (id: GameId): boolean =>
  gameById(id).supportsQuickRepair !== false;

// ------------ Game Colors ------------
// The default chart color for each game, plus helpers to read a custom color someone picked in settings.
const GAME_COLORS: Record<GameId, string> = {
  wuwa: "#2dd4bf",
  zzz: "#fcd34d",
  hsr: "#9a8cff",
  nte: "#ff5d9e",
  endfield: "#ffa726",
  genshin: "#5bc0eb",
  hi3: "#c084fc",
  pgr: "#ff6b57",
  gf2: "#a3e635",
  gf1: "#e11d48",
  re1999: "#c9a227",
  bd2: "#94a3b8",
  arknights: "#3b82f6",
  bluearchive: "#38bdf8",
  dna: "#c9a96e",
};

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

export function isGameColor(value: string | undefined | null): value is string {
  return !!value && HEX.test(value.trim());
}

export function gameColor(
  id: GameId | string,
  overrides?: Record<string, string>,
): string {
  const custom = overrides?.[id];
  if (isGameColor(custom)) return custom.trim().toLowerCase();
  return GAME_COLORS[id as GameId] ?? "#888888";
}
