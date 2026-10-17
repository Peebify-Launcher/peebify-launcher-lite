// ------------ Window Helpers ------------
// Small wrappers over the desktop shell: minimize, close, open a link, file drag and drop, and
// a check for whether we are running inside the app at all.
import { invoke } from "@tauri-apps/api/core";
import { log } from "./log";

export const isTauri = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function windowRpc<T = unknown>(channel: string): Promise<T | undefined> {
  try {
    return await invoke<T>("rpc", { channel, args: [] });
  } catch (e) {
    log.warn(`[window] ${channel} failed:`, e);
    return undefined;
  }
}

export interface CssRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export async function minimizeWindow(): Promise<void> {
  if (!isTauri()) return;
  await windowRpc("minimize-window");
}

export async function closeWindow(): Promise<void> {
  if (!isTauri()) return;
  await windowRpc("close-window");
}

export async function openExternal(url: string): Promise<void> {
  if (!url) return;
  if (!isTauri()) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  const { rpcAction } = await import("./rpc");
  await rpcAction("actions.failed.openLink", "open-external-url", url);
}

export async function signalReady(): Promise<CssRect | null> {
  if (!isTauri()) return null;
  const reply = await windowRpc<{ revealFrom?: CssRect | null }>("window-ready-to-show");
  return reply?.revealFrom ?? null;
}

export async function onFileDrop(cb: (hovering: boolean, dropped: string[]) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  const un = await getCurrentWebview().onDragDropEvent((event) => {
    const payload = event.payload as { type: string; paths?: string[] };
    if (payload.type === "over") cb(true, []);
    else cb(false, (payload.type === "drop" && payload.paths) || []);
  });
  return un as unknown as () => void;
}

const EDITABLE = 'input, textarea, [contenteditable]:not([contenteditable="false"])';

export function blockContextMenu(): void {
  window.addEventListener(
    "contextmenu",
    (event) => {
      const target = event.target;
      if (target instanceof Element && target.closest(EDITABLE)) return;
      event.preventDefault();
    },
    true,
  );
}
