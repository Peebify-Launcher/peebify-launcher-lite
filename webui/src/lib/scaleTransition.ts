// ------------ Scale Transition ------------
// Keeps the page from re-laying out while the Interface size animates: freezes the layout as a card, scales the card
// to the new size, then lets go once the window and the real zoom have been applied.
import { log } from "./log";
import { onEvent } from "./rpc";

const PINNED_CLASS = "ui-scale-pinned";
const EASING = "cubic-bezier(0.65, 0, 0.35, 1)";
const SAFETY_MS = 1500;
const END_FALLBACK_MS = 1000;
const FOCUS_BLUR_PX = 0.6;
const FOCUS_MS = 180;

let pinned = false;
let pinnedRatio = 1;
let watchFrame = 0;
let animation: Animation | null = null;
let safetyTimer = 0;
let veil: HTMLDivElement | null = null;

function watchZoom() {
  if (!pinned) return;
  if (Math.abs(window.devicePixelRatio - pinnedRatio) > 0.001) {
    unpin();
    focusIn();
    return;
  }
  watchFrame = requestAnimationFrame(watchZoom);
}

function pin() {
  window.clearTimeout(safetyTimer);
  if (pinned) return;
  pinned = true;
  pinnedRatio = window.devicePixelRatio;
  cancelAnimationFrame(watchFrame);
  watchFrame = requestAnimationFrame(watchZoom);
  const root = document.documentElement;
  root.style.setProperty("--ui-scale-pin-w", `${window.innerWidth}px`);
  root.style.setProperty("--ui-scale-pin-h", `${window.innerHeight}px`);
  root.classList.add(PINNED_CLASS);
}

function unpin() {
  window.clearTimeout(safetyTimer);
  cancelAnimationFrame(watchFrame);
  animation?.cancel();
  animation = null;
  if (!pinned) return;
  pinned = false;
  const root = document.documentElement;
  root.classList.remove(PINNED_CLASS);
  root.style.removeProperty("--ui-scale-pin-w");
  root.style.removeProperty("--ui-scale-pin-h");
}

function focusIn() {
  document.body.animate([{ filter: `blur(${FOCUS_BLUR_PX}px)` }, { filter: "blur(0px)" }], {
    duration: FOCUS_MS,
    easing: "ease-out",
  });
}

function start(ratio: number, ms: number) {
  if (!pinned) pin();
  animation?.cancel();
  animation = document.body.animate(
    [{ transform: "scale(1)" }, { transform: `scale(${ratio})` }],
    { duration: ms, easing: EASING, fill: "forwards" },
  );
  window.clearTimeout(safetyTimer);
  safetyTimer = window.setTimeout(unpin, ms + SAFETY_MS);
}

function fade(dark: boolean) {
  if (!veil) {
    veil = document.createElement("div");
    veil.setAttribute("aria-hidden", "true");
    Object.assign(veil.style, {
      position: "fixed",
      inset: "0",
      zIndex: "2147483647",
      pointerEvents: "none",
      background: "rgba(10, 10, 13, 0.9)",
      opacity: "0",
    } satisfies Partial<CSSStyleDeclaration>);
    document.body.appendChild(veil);
  }
  veil.style.transition = dark ? "opacity 100ms ease-in" : "opacity 180ms ease-out";
  veil.style.opacity = dark ? "1" : "0";
}

export function installScaleTransition(): void {
  void onEvent("ui-scale-transition", ({ phase, ratio, ms }) => {
    if (phase === "pin") pin();
    else if (phase === "start") start(ratio, ms);
    else if (phase === "end") {
      window.clearTimeout(safetyTimer);
      safetyTimer = window.setTimeout(() => {
        log.warn("[scale] the zoom change was not seen; released the layout by timer");
        unpin();
      }, END_FALLBACK_MS);
    }
    else if (phase === "fade-out") fade(true);
    else if (phase === "fade-in") fade(false);
  });
}
