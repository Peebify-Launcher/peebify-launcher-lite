// ------------ Overlay Entry ------------
// Boots the in-game overlay window, with its own small crash screen so a bug can't leave you stuck
// behind a dead overlay.
import React from "react";
import ReactDOM from "react-dom/client";
import OverlayApp from "./OverlayApp";
import { ErrorBoundary } from "../components/layout/ErrorBoundary";
import { ActionButton } from "../components/ui/controls";
import { closeOverlay } from "../lib/ipc";
import { installGlobalErrorCapture } from "../lib/log";
import { blockContextMenu } from "../lib/tauri";
import { useEscapeKey } from "../lib/hooks";
import { useT } from "../lib/useT";
import "../index.css";

installGlobalErrorCapture("overlay");
blockContextMenu();

const close = () => void closeOverlay();

function OverlayCrash() {
  const t = useT();
  useEscapeKey(close);

  return (
    <div className="ov-drawer flex h-full w-[420px] max-w-[92vw] flex-col justify-center gap-4 border-r border-white/[0.09] px-8 text-white">
      <div className="text-[15px] font-semibold">{t("overlay.crash.title")}</div>
      <p className="text-[12.5px] leading-relaxed text-white/60">
        {t("overlay.crash.body")}
      </p>
      <div className="flex gap-2">
        <ActionButton size="compact" onClick={() => window.location.reload()}>
          {t("overlay.crash.reload")}
        </ActionButton>
        <ActionButton size="compact" onClick={close}>
          {t("common.close")}
        </ActionButton>
      </div>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary scope="overlay" fallback={() => <OverlayCrash />}>
      <OverlayApp />
    </ErrorBoundary>
  </React.StrictMode>,
);
