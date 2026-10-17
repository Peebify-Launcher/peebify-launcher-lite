// ------------ Settings Page ------------
// The Settings tab with its tab strip and the game banner at the top of the Games tab.
import { useEffect, useMemo, useRef, type KeyboardEvent, type ReactNode, useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import { fadeVariants, pageVariants, springThumb } from "../../lib/motion";
import {
  SETTINGS_SECTIONS,
  type Setting,
  type SettingsCategory,
} from "../../data/settings";
import { useSessionUiStore } from "../../store/sessionUiStore";
import { GameMaintenance } from "./GameMaintenance";
import { GameStorage, preloadGameStorage } from "./GameStorage";
import { useGamesStore } from "../../store/gamesStore";
import { LibraryPanel } from "./LibraryPanel";
import { OverlayPanel } from "./OverlayPanel";
import { SettingsGroup } from "../ui/panels";
import { SettingRow } from "./SettingRow";
import { GameArtwork, GameColorsCard, HomeScreenGroup } from "./AppearanceEditors";
import { AboutCredits, AboutHero } from "./AboutCredits";
import { useT } from "../../lib/useT";
import type { TKey } from "../../lib/i18n";
import { type GameId, gameById, isManaged } from "../../data/games";
import { iconSrcFor, wallpaperFor } from "../../lib/customMedia";
import { useCustomizationStore } from "../../store/customizationStore";
import { useRemoteMediaStore } from "../../store/remoteContentStore";
import { CrossfadeMedia } from "../ui/CrossfadeMedia";

// ------------ Page ------------
// The Settings tab: a row of tabs (from the settings table) with the matching settings underneath.
export function SettingsPage() {
  const t = useT();
  const cat = useSessionUiStore((s) => s.settingsCat);
  const setCat = useSessionUiStore((s) => s.setSettingsCat);
  const settingsGameId = useSessionUiStore((s) => s.settingsGameId);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: 0 });
  }, [cat]);

  useEffect(() => {
    const first = useSessionUiStore.getState().settingsGameId;
    const installed = useGamesStore.getState().installed.filter((id) => id !== first);
    void preloadGameStorage([first, ...installed]);
  }, []);

  const tabItems = useMemo<TabItem[]>(
    () =>
      SETTINGS_SECTIONS.flatMap(({ category, tab }) =>
        tab ? [{ id: category, label: t(tab.label), icon: <tab.icon size={16} /> }] : [],
      ),
    [t],
  );

  const section = SETTINGS_SECTIONS.find((s) => s.category === cat)!;

  const groups = useMemo(() => {
    const out: { name?: TKey; items: Setting[] }[] = [];
    for (const s of section.settings) {
      const last = out[out.length - 1];
      if (last && last.name === s.group) last.items.push(s);
      else out.push({ name: s.group, items: [s] });
    }
    return out;
  }, [section]);

  const renderGroups = (include: (name: TKey | undefined) => boolean = () => true) =>
    groups.filter((g) => include(g.name)).map((g, gi) => (
      <SettingsGroup
        key={g.name ?? gi}
        label={g.name ? t(g.name) : undefined}
        danger={g.items.every((s) => s.kind === "action" && s.danger)}
      >
        {g.items.map((s) => (
          <SettingRow key={s.id} setting={s} />
        ))}
      </SettingsGroup>
    ));

  const body = () => {
    switch (cat) {
      case "overlay":
        return <OverlayPanel />;
      case "launcher":
        return (
          <>
            <AboutHero />
            <AboutCredits />
            {renderGroups()}
          </>
        );
      case "appearance": {
        const home = "settings.groups.homeScreen";
        const sidebar = "settings.groups.sidebar";
        const sidebarItems = groups.find((g) => g.name === sidebar)?.items ?? [];
        return (
          <>
            <HomeScreenGroup settings={groups.find((g) => g.name === home)?.items ?? []} />
            <SettingsGroup
              label={t(sidebar)}
              hint={t("settings.appearance.sidebarHint")}
            >
              {sidebarItems.map((s) => (
                <SettingRow key={s.id} setting={s} />
              ))}
            </SettingsGroup>
            <GameArtwork />
            <GameColorsCard />
            {renderGroups((name) => name !== home && name !== sidebar)}
          </>
        );
      }
      default:
        return renderGroups();
    }
  };

  return (
    <m.div variants={pageVariants} className="absolute inset-0 flex flex-col">
      <div className="shrink-0 px-10 pt-6">
        <h1 className="mb-4 text-[24px] font-semibold leading-tight">{t("common.settings")}</h1>
        <SettingsTabStrip
          items={tabItems}
          active={cat}
          onSelect={(id) => setCat(id as SettingsCategory)}
        />
      </div>

      <div
        ref={scrollRef}
        role="tabpanel"
        id={SETTINGS_PANEL_ID}
        aria-labelledby={settingsTabId(cat)}
        className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-10 pb-10 pt-6"
      >
        <AnimatePresence mode="wait" initial={false}>
          <m.div key={cat} variants={fadeVariants} initial="initial" animate="animate" exit="exit">
            <div className="mx-auto max-w-[1260px]">
              {cat !== "games" && (
                <div className="mb-6 border-b border-white/10 pb-4">
                  <h2 className="text-[18px] font-semibold">{t(section.title)}</h2>
                  <p className="mt-1 text-[14px] text-white/55">{t(section.subtitle)}</p>
                </div>
              )}

              {cat === "games" ? (
                <div className="flex flex-col gap-7 pb-4">
                  <GameBanner gameId={settingsGameId} />
                  <div className="flex items-start gap-7">
                    <div className="w-[292px] shrink-0 max-[1100px]:w-[240px]">
                      <LibraryPanel />
                    </div>
                    <div className="flex min-w-0 flex-1 flex-col gap-7">
                      <GameStorage gameId={settingsGameId} />
                      <GameMaintenance gameId={settingsGameId} />
                    </div>
                  </div>
                </div>
              ) : (
                <div className="flex flex-col gap-7 pb-4">{body()}</div>
              )}
            </div>
          </m.div>
        </AnimatePresence>
      </div>
    </m.div>
  );
}

// ------------ Settings Tab Strip ------------
// The row of tabs at the top of Settings. Works with the mouse and with the arrow, Home and End keys.
interface TabItem {
  id: string;
  label: string;
  icon: ReactNode;
}

const SETTINGS_PANEL_ID = "settings-panel";
const settingsTabId = (id: string): string => `settings-tab-${id}`;

function SettingsTabStrip({
  items,
  active,
  onSelect,
}: {
  items: TabItem[];
  active: string;
  onSelect: (id: string) => void;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);

  const onKeyDown = (e: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const last = items.length - 1;
    let next: number;
    switch (e.key) {
      case "ArrowRight":
        next = index === last ? 0 : index + 1;
        break;
      case "ArrowLeft":
        next = index === 0 ? last : index - 1;
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = last;
        break;
      default:
        return;
    }
    e.preventDefault();
    onSelect(items[next].id);
    refs.current[next]?.focus();
  };

  return (
    <div
      role="tablist"
      aria-orientation="horizontal"
      className="flex items-center gap-1 border-b border-white/[0.08]"
    >
      {items.map((it, index) => {
        const on = it.id === active;
        return (
          <button
            key={it.id}
            ref={(el) => {
              refs.current[index] = el;
            }}
            type="button"
            role="tab"
            id={settingsTabId(it.id)}
            aria-controls={SETTINGS_PANEL_ID}
            aria-selected={on}
            tabIndex={on ? 0 : -1}
            onClick={() => onSelect(it.id)}
            onKeyDown={(e) => onKeyDown(e, index)}
            className={`relative flex items-center gap-[7px] whitespace-nowrap px-[14px] pb-[11px] pt-[9px] text-[13px] transition-colors duration-150 ${
              on ? "font-semibold text-white" : "font-medium text-white/60 hover:text-white/80"
            }`}
          >
            {it.icon}
            {it.label}
            {on && (
              <m.span
                layoutId="settings-tab-underline"
                className="absolute inset-x-0 -bottom-px h-[2px] bg-white"
                transition={springThumb}
              />
            )}
          </button>
        );
      })}
    </div>
  );
}

// ------------ Game Banner ------------
// The header of the Games settings tab: the selected game's icon, name and artwork, crossfading between games.
const ART_MASK = "linear-gradient(90deg, transparent 0%, rgba(0,0,0,0.55) 38%, #000 70%)";

function GameBanner({ gameId }: { gameId: GameId }) {
  const t = useT();
  const game = gameById(gameId);
  const icons = useCustomizationStore((s) => s.gameIcons);
  const slice = useCustomizationStore((s) => s.wallpaper[gameId]);
  const remote = useRemoteMediaStore((s) => s.media[gameId]);

  const [failed, setFailed] = useState<ReadonlySet<string>>(() => new Set());
  const onMediaError = (src: string) =>
    setFailed((prev) => (prev.has(src) ? prev : new Set(prev).add(src)));

  const artSrc =
    [wallpaperFor(gameId, slice, remote, false), wallpaperFor(gameId, undefined, remote, false)]
      .filter((c) => !c.video)
      .map((c) => c.src)
      .find((src) => !failed.has(src)) ?? game.wallpaperStatic;
  const customIcon = iconSrcFor(gameId, icons);
  const iconSrc = failed.has(customIcon) ? game.icon : customIcon;

  return (
    <section
      aria-label={game.name}
      className="relative flex min-h-[176px] flex-col justify-end overflow-hidden rounded-[11px] border border-white/10"
      style={{
        background: "rgba(18,18,22,0.72)",
        backdropFilter: "blur(30px) saturate(180%)",
        WebkitBackdropFilter: "blur(30px) saturate(180%)",
      }}
    >
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-y-0 right-0 w-[62%] [&_img]:object-[72%_28%]"
        style={{ maskImage: ART_MASK, WebkitMaskImage: ART_MASK }}
      >
        <CrossfadeMedia src={artSrc} video={false} onError={onMediaError} />
      </div>
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-0"
        style={{ background: "linear-gradient(180deg, rgba(10,12,16,0) 35%, rgba(10,12,16,0.72) 100%)" }}
      />
      <div className="relative flex flex-wrap items-end justify-between gap-x-6 gap-y-4 p-[22px]">
        <div className="flex min-w-0 items-center gap-[18px]">
          <div
            aria-hidden="true"
            className="relative h-[72px] w-[72px] shrink-0 overflow-hidden rounded-[16px]"
            style={{ boxShadow: "0 0 0 1px rgba(255,255,255,0.12), 0 6px 14px -6px rgba(0,0,0,0.6)" }}
          >
            <CrossfadeMedia src={iconSrc} video={false} onError={onMediaError} />
          </div>
          <div className="min-w-0">
            <h2
              className="truncate text-[28px] font-semibold leading-[1.15] tracking-[-0.01em] text-white"
              style={{ fontFamily: "var(--font-display)" }}
            >
              {game.name}
            </h2>
            <p className="mt-[6px] text-[13px] leading-[18px] text-white/70">
              {isManaged(gameId)
                ? t("maintenance.selectedGame.managedByPeebify")
                : t("maintenance.selectedGame.managedByOwn")}
            </p>
          </div>
        </div>
      </div>
    </section>
  );
}
