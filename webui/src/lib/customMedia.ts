// ------------ Custom Media ------------
// Works out which icon or wallpaper to show for a game: the one the user picked, the one from the server, or the built-in
// default. Also turns local file paths into URLs the webview can load.
import type { SyntheticEvent } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { isTauri } from "./tauri";
import { gameById, type GameId } from "../data/games";
import type { MediaSlice, RemoteMedia } from "./ipc";

export function localFileSrc(p: string | null | undefined): string | null {
  if (!p) return null;
  return isTauri() ? convertFileSrc(p) : p;
}

function customSrc(slice: MediaSlice | undefined): string | null {
  if (!slice || slice.type !== "custom" || !slice.path) return null;
  return localFileSrc(slice.path);
}

const VIDEO_EXT = /\.(mp4|webm|mov|m4v)$/i;
const isVideoPath = (p: string): boolean => VIDEO_EXT.test(p);

export function iconSrcFor(id: string, icons: Record<string, MediaSlice>): string {
  return customSrc(icons[id]) || gameById(id as GameId).icon;
}

export function iconFallback(id: string): (e: SyntheticEvent<HTMLImageElement>) => void {
  return (e) => {
    const img = e.currentTarget;
    const fallback = gameById(id as GameId).icon;
    if (img.getAttribute("src") !== fallback) img.src = fallback;
  };
}

export function wallpaperFor(
  gameId: string,
  slice: MediaSlice | undefined,
  remote: RemoteMedia | undefined,
  animated: boolean,
): { src: string; video: boolean } {
  const custom = customSrc(slice);
  if (custom) {
    return { src: custom, video: !!slice?.path && isVideoPath(slice.path) };
  }
  const still =
    remote?.static ?? (remote?.wallpaper && !isVideoPath(remote.wallpaper) ? remote.wallpaper : null);
  const remotePath = (animated ? remote?.wallpaper : null) ?? still;
  const remoteSrc = localFileSrc(remotePath);
  if (remoteSrc) {
    return { src: remoteSrc, video: !!remotePath && isVideoPath(remotePath) };
  }
  return { src: gameById(gameId as GameId).wallpaperStatic, video: false };
}
