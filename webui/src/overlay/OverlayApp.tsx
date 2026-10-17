// ------------ Overlay Drawer ------------
// The in-game overlay: a drawer that slides in over the game with tabs for mods, the mods gallery,
// captures and settings. It follows whichever game is running.
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, LazyMotion, MotionConfig, m } from "framer-motion";
import { X } from "lucide-react";
import {
  closeOverlay,
  getModsStatus,
  getOverlayConfig,
  getOverlayStatus,
  type ModsStatus,
} from "../lib/ipc";
import { changedSettingKey, changeTouchesSettings, useSettingsStore } from "../store/settingsStore";
import { useModsStore, useModProfilesStore } from "../store/mods";
import { useNotificationStore } from "../store/notifications";
import { gameById, type GameId } from "../data/games";
import { OverlayMods } from "./OverlayMods";
import { OverlayCaptures } from "./OverlayCaptures";
import { OverlaySettings } from "./OverlaySettings";
import { ToastLayer } from "../components/layout/ToastLayer";
import { IconButton } from "../components/ui/Tooltip";
import { spacedAccelerator } from "../lib/hotkeys";
import { EASE_OUT_SOFT } from "../lib/motion";
import { fmtHM } from "../lib/format";
import type { TKey } from "../lib/i18n";
import { useEscapeKey, useTauriEvents } from "../lib/hooks";
import { useT } from "../lib/useT";

const loadMotionFeatures = () => import("../lib/motionFeatures").then((mod) => mod.default);

const loadGallery = () => import("../components/mods/BrowseMods");
const BrowseMods = lazy(() => loadGallery().then((mod) => ({ default: mod.BrowseMods })));

type PanelId = "mods" | "gallery" | "captures" | "settings";

export interface PanelProps {
  gameId: GameId;
  modsUsable: boolean;
  mods: ModsStatus | null;
}

const ICON = {
  mods: (
    <>
      <rect x="3" y="3" width="7" height="7" rx="1.5" />
      <rect x="14" y="3" width="7" height="7" rx="1.5" />
      <rect x="3" y="14" width="7" height="7" rx="1.5" />
      <rect x="14" y="14" width="7" height="7" rx="1.5" />
    </>
  ),
  gallery: (
    <>
      <rect x="3" y="3" width="18" height="18" rx="2" />
      <circle cx="9" cy="9" r="1.6" />
      <path d="m21 15-4.35-4.35a2 2 0 0 0-2.83 0L6 21" />
    </>
  ),
  captures: (
    <>
      <path d="M3 8h4l2-3h6l2 3h4v12H3z" />
      <circle cx="12" cy="13" r="3.4" />
    </>
  ),
  settings: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 2.5V5M12 19v2.5M2.5 12H5M19 12h2.5M5.4 5.4 7 7M17 17l1.6 1.6M18.6 5.4 17 7M7 17l-1.6 1.6" />
    </>
  ),
} as const;

const PANELS: { id: PanelId; label: TKey; title: TKey }[] = [
  { id: "mods", label: "common.mods", title: "common.mods" },
  { id: "gallery", label: "overlay.app.modsGallery", title: "overlay.app.modsGallery" },
  { id: "captures", label: "common.captures", title: "common.captures" },
  { id: "settings", label: "common.settings", title: "common.settings" },
];

function NavIcon({ id }: { id: PanelId }) {
  return (
    <span className="grid h-[18px] w-[18px] place-items-center">
      <svg
        width="16"
        height="16"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        {ICON[id]}
      </svg>
    </span>
  );
}

function OverlayGallery({ gameId, modsUsable, mods: status }: PanelProps) {
  const t = useT();
  if (!modsUsable) {
    if (!status) return null;
    return (
      <p className="max-w-[70ch] text-[13px] leading-relaxed text-white/45">
        {t("overlay.gallery.modsOff")}
      </p>
    );
  }
  return (
    <Suspense fallback={null}>
      <BrowseMods key={gameId} gameId={gameId} />
    </Suspense>
  );
}

function holdsText(target: EventTarget | null): boolean {
  if (target instanceof HTMLTextAreaElement) return target.value !== "";
  return (
    target instanceof HTMLInputElement &&
    (target.type === "text" || target.type === "search") &&
    target.value !== ""
  );
}

export default function OverlayApp() {
  const t = useT();
  const [gameId, setGameId] = useState<GameId | null>(null);
  const [startedMs, setStartedMs] = useState<number | null>(null);
  const [mods, setMods] = useState<ModsStatus | null>(null);
  const [active, setActive] = useState<PanelId>("mods");
  const [openKey, setOpenKey] = useState("Alt + P");
  const [, setTick] = useState(0);
  const hydrate = useSettingsStore((s) => s.hydrate);
  const gameRef = useRef<GameId | null>(null);
  const loadToken = useRef(0);
  const drawerOpen = useRef(true);
  const [drawerShown, setDrawerShown] = useState(() => document.visibilityState !== "hidden");
  const navRef = useRef<HTMLDivElement>(null);
  const backdropPress = useRef(false);

  const loadOpenKey = useCallback(async () => {
    const config = await getOverlayConfig();
    const toggle = config?.hotkeys?.find((h) => h.id === "overlayHotkey");
    if (toggle?.accelerator) setOpenKey(spacedAccelerator(toggle.accelerator));
  }, []);

  const loadGame = useCallback(async (refreshStores: boolean) => {
    const token = ++loadToken.current;
    const status = await getOverlayStatus();
    if (token !== loadToken.current) return;
    const id = (status?.gameId || null) as GameId | null;
    gameRef.current = id;
    setGameId(id);
    setStartedMs(typeof status?.sessionStartedMs === "number" ? status.sessionStartedMs : null);
    if (!id) {
      setMods(null);
      return;
    }
    const next = await getModsStatus(id);
    if (token !== loadToken.current) return;
    setMods(next);
    if (next.supportsMods) void useModsStore.getState().loadReloadKey(id);
    if (!refreshStores || !next.masterEnabled || !next.gameEnabled || !next.supportsMods) return;
    void useModsStore.getState().refresh(id);
    void useModProfilesStore.getState().refresh(id);
  }, []);

  useEffect(() => {
    void hydrate();
    void loadOpenKey();
    void loadGame(false);
    void loadGallery();
  }, [hydrate, loadOpenKey, loadGame]);

  useEffect(() => {
    const timer = window.setInterval(() => setTick((n) => n + 1), 30000);
    return () => window.clearInterval(timer);
  }, []);

  useTauriEvents({
    "settings-changed": (payload) => {
      if (changeTouchesSettings(payload)) void hydrate();
      const key = changedSettingKey(payload);
      if (key === null || key === "overlayHotkey" || key === "behavior.overlayHotkey") {
        void loadOpenKey();
      }
      if (key === null || key === "modsEnabled" || key === "behavior.modsEnabled") {
        void loadGame(false);
      }
    },
    "overlay-closed": () => {
      drawerOpen.current = false;
      setDrawerShown(false);
    },
    "overlay-opened": () => {
      drawerOpen.current = true;
      setDrawerShown(true);
      (document.activeElement as HTMLElement | null)?.blur();
      navRef.current
        ?.querySelector<HTMLElement>('[aria-current="page"]')
        ?.focus({ preventScroll: true });
      void loadOpenKey();
      void loadGame(true);
    },
    "mods-status-changed": (payload) => {
      const changed = (payload as { gameId?: string } | null)?.gameId;
      if (changed && changed !== gameRef.current) return;
      if (!drawerOpen.current || document.visibilityState === "hidden") return;
      void loadGame(true);
    },
  });

  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === "visible") setDrawerShown(true);
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, []);

  const modsUsable = !!mods?.masterEnabled && !!mods?.gameEnabled && !!mods?.supportsMods;

  const modTabsVisible = mods === null || (mods.masterEnabled && mods.supportsMods);
  const panels = useMemo(
    () =>
      modTabsVisible
        ? PANELS
        : PANELS.filter((panel) => panel.id !== "mods" && panel.id !== "gallery"),
    [modTabsVisible],
  );

  useEffect(() => {
    if (!panels.some((panel) => panel.id === active)) {
      setActive(panels[0].id);
    }
  }, [panels, active]);

  const close = () => void closeOverlay();

  const isTop = useEscapeKey((e) => {
    if (e.defaultPrevented || holdsText(e.target)) return;
    e.preventDefault();
    close();
  });

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || !isTop()) return;
      const index = Number.parseInt(e.key, 10) - 1;
      if (Number.isNaN(index) || index < 0 || index >= panels.length) return;
      const target = e.target as HTMLElement | null;
      if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA") return;
      e.preventDefault();
      setActive(panels[index].id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [isTop, panels]);

  const game = gameId ? gameById(gameId) : null;
  const subtitle = game
    ? startedMs !== null
      ? t("overlay.app.sessionSubtitle", {
          game: game.name,
          elapsed: fmtHM(Math.floor(Math.max(0, Date.now() - startedMs) / 60000)),
        })
      : game.name
    : t("overlay.app.noGame");

  const Panel = {
    mods: OverlayMods,
    gallery: OverlayGallery,
    captures: OverlayCaptures,
    settings: OverlaySettings,
  }[active];

  const titleKey = panels.find((p) => p.id === active)?.title;
  const title = titleKey ? t(titleKey) : "";

  return (
    <LazyMotion features={loadMotionFeatures} strict>
      <MotionConfig reducedMotion="user">
        <div
          className="fixed inset-0 flex"
          style={{
            background:
              "linear-gradient(90deg, rgba(4,4,7,0.46) 0%, rgba(4,4,7,0.26) 38%, rgba(4,4,7,0.16) 100%)",
          }}
          onPointerDown={(e) => {
            backdropPress.current = e.target === e.currentTarget;
          }}
          onClick={(e) => {
            const pressed = backdropPress.current;
            backdropPress.current = false;
            if (pressed && e.target === e.currentTarget) close();
          }}
        >
          <m.aside
            initial={{ x: -40, opacity: 0 }}
            animate={drawerShown ? { x: [-40, 0], opacity: [0, 1] } : { x: -40, opacity: 0 }}
            transition={drawerShown ? { duration: 0.22, ease: EASE_OUT_SOFT } : { duration: 0 }}
            className="ov-drawer flex h-full w-[min(92vw,clamp(880px,48vw,1240px))] border-r border-white/[0.09]"
          >
            <nav
              aria-label={t("overlay.app.sections")}
              className="ov-rail flex min-h-0 w-[268px] shrink-0 flex-col border-r border-white/[0.07] px-4 pb-[18px] pt-6"
            >
              <div className="flex items-center gap-[11px] px-2 pb-[18px]">
                <img
                  src="/icons/app.png"
                  alt="Peebify" // i18n-ignore
                  className="h-[30px] w-[30px] rounded-[8px] object-contain"
                />
                <div className="min-w-0">
                  <div className="text-[14px] font-semibold tracking-[-0.01em]">
                    {t("overlay.app.title")}
                  </div>
                  <div className="truncate text-[12px] text-white/45">{subtitle}</div>
                </div>
              </div>

              <div
                ref={navRef}
                className="ov-scroll flex min-h-0 flex-1 flex-col gap-[2px] overflow-y-auto"
              >
                {panels.map((panel, index) => {
                  const on = panel.id === active;
                  return (
                    <button
                      key={panel.id}
                      type="button"
                      aria-current={on ? "page" : undefined}
                      onClick={() => setActive(panel.id)}
                      className={`flex w-full items-center gap-[11px] rounded-[9px] border px-[11px] py-[10px] text-left text-[13.5px] transition-colors duration-150 ${
                        on
                          ? "border-white/[0.14] bg-white/10 font-semibold text-white"
                          : "border-transparent font-medium text-white/[0.62] hover:bg-white/[0.05] hover:text-white/85"
                      }`}
                    >
                      <NavIcon id={panel.id} />
                      <span className="flex-1">{t(panel.label)}</span>
                      <span className="ov-mono text-[10.5px] text-white/30">{index + 1}</span>
                    </button>
                  );
                })}
              </div>

              <div className="mt-auto flex flex-col gap-[9px] border-t border-white/[0.07] px-2 pt-4">
                <div className="flex items-center justify-between text-[11.5px] text-white/[0.42]">
                  <span>{t("overlay.app.openClose")}</span>
                  <span className="ov-mono rounded-[5px] bg-white/[0.08] px-[6px] py-[2px] text-white/70">
                    {openKey}
                  </span>
                </div>
                <div className="flex items-center justify-between text-[11.5px] text-white/[0.42]">
                  <span>{t("common.close")}</span>
                  <span className="ov-mono rounded-[5px] bg-white/[0.08] px-[6px] py-[2px] text-white/70">
                    {"Esc" /* i18n-ignore: key name */}
                  </span>
                </div>
              </div>
            </nav>

            <div className="ov-content relative flex min-w-0 flex-1 flex-col">
              <ToastLayer onOpen={(id) => useNotificationStore.getState().dismissToast(id)} />
              <header className="flex shrink-0 items-center justify-between gap-4 border-b border-white/[0.07] px-[clamp(30px,1.6vw,44px)] pb-4 pt-[26px]">
                <h3 className="font-display text-[21px] font-semibold">{title}</h3>
                <IconButton
                  type="button"
                  aria-label={t("overlay.app.closeOverlay")}
                  tip={t("overlay.app.closeTitle")}
                  onClick={close}
                  className="-mr-2 grid h-[32px] w-[32px] shrink-0 place-items-center rounded-[8px] text-white/55 transition-colors duration-150 hover:bg-white/[0.08] hover:text-white"
                >
                  <X size={18} />
                </IconButton>
              </header>
              <div className="ov-scroll min-h-0 flex-1 overflow-y-auto px-[clamp(30px,1.6vw,44px)] pb-[26px] pt-5">
                <AnimatePresence mode="wait" initial={false}>
                  <m.div
                    key={active}
                    initial={{ opacity: 0, y: 6 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -6 }}
                    transition={{ duration: 0.15 }}
                  >
                    {gameId ? (
                      <Panel gameId={gameId} modsUsable={modsUsable} mods={mods} />
                    ) : (
                      <p className="text-[13px] text-white/45">{t("overlay.app.noGameBody")}</p>
                    )}
                  </m.div>
                </AnimatePresence>
              </div>
            </div>
          </m.aside>
        </div>
      </MotionConfig>
    </LazyMotion>
  );
}
