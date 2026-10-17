// ------------ Launcher Entry ------------
// Boots the main launcher window: error logging, the React root, and a crash screen with a reload button
// in case the whole UI falls over.
import React from "react";
import ReactDOM from "react-dom/client";
import { Minus, X } from "lucide-react";
import App from "./App";
import { ErrorBoundary, retryOrReload } from "./components/layout/ErrorBoundary";
import { installGlobalErrorCapture } from "./lib/log";
import { installScaleTransition } from "./lib/scaleTransition";
import { blockContextMenu, closeWindow, minimizeWindow } from "./lib/tauri";
import { openLogsFolder } from "./lib/ipc";
import { useUiStore } from "./store/uiStore";
import { useT } from "./lib/useT";
import "./index.css";

installGlobalErrorCapture("main");
installScaleTransition();
blockContextMenu();

function crashContext(): Record<string, unknown> {
  const ui = useUiStore.getState();
  return { view: ui.activeView, activeGameId: ui.activeGameId, version: ui.appVersion || "unknown" };
}

function RootCrashScreen({ onRetry }: { onRetry: () => void }) {
  const t = useT();
  return (
    <div className="relative flex h-screen w-screen flex-col items-center justify-center gap-4 bg-[#0d0d10] text-white">
      <div className="drag-region absolute inset-x-0 top-0 h-[32px]" />
      <div className="absolute right-[14px] top-[12px] flex items-center gap-1">
        <button
          aria-label={t("common.minimize")}
          className="grid h-[30px] w-[30px] place-items-center rounded-[7px] transition-colors hover:bg-white/10"
          onClick={() => void minimizeWindow()}
        >
          <Minus size={15} />
        </button>
        <button
          aria-label={t("common.close")}
          className="grid h-[30px] w-[30px] place-items-center rounded-[7px] transition-colors hover:bg-white/10"
          onClick={() => void closeWindow()}
        >
          <X size={15} />
        </button>
      </div>
      <div className="text-lg font-semibold">{t("crash.title")}</div>
      <div className="max-w-[420px] text-center text-sm text-white/60">{t("crash.body")}</div>
      <div className="flex items-center gap-2">
        <button
          className="rounded-lg bg-white/10 px-4 py-2 text-sm hover:bg-white/20"
          onClick={() => retryOrReload(onRetry)}
        >
          {t("common.tryAgain")}
        </button>
        <button
          className="rounded-lg bg-white/5 px-4 py-2 text-sm text-white/80 hover:bg-white/15"
          onClick={() => void openLogsFolder()}
        >
          {t("common.openLogsFolder")}
        </button>
      </div>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary
      scope="main"
      context={crashContext}
      fallback={(reset) => <RootCrashScreen onRetry={reset} />}
    >
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
