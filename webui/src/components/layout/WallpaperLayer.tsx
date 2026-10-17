// ------------ Wallpaper Layer ------------
// The full-window background for the selected game. Uses the user's custom wallpaper if set, otherwise the
// game's own, and falls back to a still image if a video fails to load. Videos pause when the launcher is hidden
// or a game is running. Also the boot gate, which keeps the window hidden until the first wallpaper has painted.
import { useState } from "react";
import { useUiStore } from "../../store/uiStore";
import { useSettingsStore } from "../../store/settingsStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { useRemoteMediaStore } from "../../store/remoteContentStore";
import { gameById } from "../../data/games";
import { wallpaperFor } from "../../lib/customMedia";
import { useRendererSuspended, useWindowFocused } from "../../lib/launcherHooks";
import { useGamesStore } from "../../store/gamesStore";
import { CrossfadeMedia, type Wave } from "../ui/CrossfadeMedia";
import { log } from "../../lib/log";
import { signalReady, type CssRect } from "../../lib/tauri";
import { onEvent } from "../../lib/rpc";
import { useReducedMotionSafe } from "../../lib/motion";

// ------------ Game Switch Wave ------------
// Switching games from the rail spreads the new wallpaper out from the icon you clicked, and the news banner with
// it. The rail notes the point just before it switches; each of those reads the wave from its own position, with
// the radius that covers the window, so they share one front. Any other change (a new animated wallpaper arriving,
// a custom one picked in settings, the banner slideshow) crossfades as before.
let pendingWave: { x: number; y: number; at: number } | null = null;
const WAVE_WINDOW_MS = 1500;

export function startGameSwitchWave(point: { x: number; y: number }): void {
  pendingWave = { ...point, at: Date.now() };
}

export function gameSwitchWave(el?: Element | null): Wave | null {
  if (!pendingWave || Date.now() - pendingWave.at > WAVE_WINDOW_MS) return null;
  const { x, y, at } = pendingWave;
  const r = Math.hypot(Math.max(x, window.innerWidth - x), Math.max(y, window.innerHeight - y));
  const box = el?.getBoundingClientRect();
  return { x: x - (box?.left ?? 0), y: y - (box?.top ?? 0), r, at };
}

export function WallpaperLayer() {
  const game = gameById(useUiStore((s) => s.activeGameId));
  const suspended = useRendererSuspended();
  const onHome = useUiStore((s) => s.activeView === "home");
  const focused = useWindowFocused();
  const gameRunning = useGamesStore((s) => s.runningGames.length > 0);
  const animated = useSettingsStore((s) => s.values.animatedWallpaper) !== "false";
  const slice = useCustomizationStore((s) => s.wallpaper[game.id]);
  const remote = useRemoteMediaStore((s) => s.media[game.id]);

  const settingsReady = useSettingsStore((s) => s.hydrated);
  const customizationReady = useCustomizationStore((s) => s.hydrated);
  const cacheReady = useRemoteMediaStore((s) => s.hydrated);
  const ready = settingsReady && customizationReady && cacheReady;

  const [failedSources, setFailedSources] = useState<ReadonlySet<string>>(() => new Set());
  const reduce = useReducedMotionSafe();

  const candidates = [
    wallpaperFor(game.id, slice, remote, animated),
    wallpaperFor(game.id, undefined, remote, false),
    { src: game.wallpaperStatic, video: false },
  ];
  const { src: mediaSrc, video: useVideo } =
    candidates.find((c) => !failedSources.has(c.src)) ?? candidates[candidates.length - 1];

  const onMediaError = (src: string) => {
    setFailedSources((prev) => (prev.has(src) ? prev : new Set(prev).add(src)));
  };

  return (
    <div className="absolute inset-0">
      <div className="absolute inset-0" style={{ background: game.wallpaperFallback }} />

      {ready && (
        <CrossfadeMedia
          src={mediaSrc}
          video={useVideo}
          autoPlay={animated}
          playing={!suspended && onHome && (focused || !gameRunning)}
          onReady={markWallpaperPainted}
          onError={onMediaError}
          wave={reduce ? undefined : gameSwitchWave}
        />
      )}

      <div
        className="absolute inset-0"
        style={{
          background:
            "linear-gradient(180deg, rgba(8,8,12,.28) 0%, rgba(8,8,12,0) 30%, rgba(8,8,12,.45) 100%)",
        }}
      />
    </div>
  );
}

// ------------ Boot Gate ------------
// Keeps the launcher window hidden until the first wallpaper and the saved data are ready, so it never flashes
// an empty window. If something is slow it shows up anyway after a short wait.
const FIRST_PAINT_CAP_MS = 1500;

let revealed = false;
let resolveWallpaper: (() => void) | undefined;

const wallpaperPainted = new Promise<void>((resolve) => {
  resolveWallpaper = resolve;
});

function markWallpaperPainted(): void {
  resolveWallpaper?.();
  resolveWallpaper = undefined;
}

function reveal(): void {
  if (revealed) return;
  revealed = true;
  const pouring = clipToStartButton();
  if (pouring) awaitShown();
  void signalReady().then((from) => {
    if (pouring && from) setPourRect(from);
  });
}

// ------------ Window Pour ------------
// On a cold start the window pours out of the Start button: it is clipped to the button's shape while still hidden,
// and opens once the backend has shown it. When the backend hands over another rect to pour from, it pours out of
// that instead. If it comes up another way (minimized, in the tray) or is already visible (a reload), the clip is
// dropped, so the launcher can never stay hidden.
const POUR_MS = 600;
const POUR_FAILSAFE_MS = 3000;

function setPourRect(rect: CssRect): boolean {
  const root = document.querySelector<HTMLElement>("[data-window-root]");
  if (!root) return false;
  root.style.setProperty("--pour-t", `${rect.top}px`);
  root.style.setProperty("--pour-r", `${root.clientWidth - rect.left - rect.width}px`);
  root.style.setProperty("--pour-b", `${root.clientHeight - rect.top - rect.height}px`);
  root.style.setProperty("--pour-l", `${rect.left}px`);
  return true;
}

function clipToStartButton(): boolean {
  if (document.visibilityState === "visible") return false;
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return false;
  const button = document.querySelector(".start-button")?.getBoundingClientRect();
  const w = window.innerWidth;
  const h = window.innerHeight;
  const from: CssRect =
    button && button.width > 0
      ? { left: button.left, top: button.top, width: button.width, height: button.height }
      : { left: w / 2 - 23, top: h / 2 - 23, width: 46, height: 46 };
  if (!setPourRect(from)) return false;
  document.documentElement.classList.add("window-pour");
  return true;
}

function endPour(): void {
  document.documentElement.classList.remove("window-pour", "window-pouring");
}

function awaitShown(): void {
  let done = false;
  const offs: Array<() => void> = [];
  const finish = (open: boolean) => {
    if (done) return;
    done = true;
    window.clearTimeout(failsafe);
    window.removeEventListener("focus", onFocus);
    offs.forEach((off) => off());
    if (!open) {
      endPour();
      return;
    }
    document.documentElement.classList.add("window-pouring");
    window.setTimeout(endPour, POUR_MS + 100);
  };
  const onFocus = () => finish(true);
  const failsafe = window.setTimeout(() => finish(false), POUR_FAILSAFE_MS);
  window.addEventListener("focus", onFocus);
  void onEvent("main-window-revealed", () => finish(true)).then((off) => (done ? off() : offs.push(off)));
  void onEvent("window-restored", () => finish(false)).then((off) => (done ? off() : offs.push(off)));
}

export function revealWhenPainted(hydration: Promise<unknown>): void {
  const cap = new Promise<void>((resolve) => {
    window.setTimeout(resolve, FIRST_PAINT_CAP_MS);
  });
  const painted = Promise.all([hydration, wallpaperPainted]).then(
    () => undefined,
    (e: unknown) => {
      log.warn("[boot] hydration failed:", e);
    },
  );
  void Promise.race([painted, cap]).then(reveal);
}
