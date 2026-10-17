// ------------ Home Preview ------------
// The Appearance tab's preview of the Home screen: the real screens' sizes and positions drawn at the default window
// size and scaled down as a whole, with the Home screen switches applied.
import { Bell, Clock4, ChevronUp, Minus, Newspaper, Play, Settings, Square, Users, X } from "lucide-react";
import { GAMES, type GameId } from "../../data/games";
import { CHANNEL_LABEL } from "../../lib/ipc";
import { fmtHM } from "../../lib/format";
import { iconFallback, iconSrcFor, wallpaperFor } from "../../lib/customMedia";
import { useCustomizationStore } from "../../store/customizationStore";
import { useLibraryStore } from "../../store/libraryStore";
import { usePlaytimeStore } from "../../store/playtimeStore";
import { useRemoteMediaStore } from "../../store/remoteContentStore";
import { useSettingsStore } from "../../store/settingsStore";
import { useUiStore } from "../../store/uiStore";
import { CrossfadeMedia } from "../ui/CrossfadeMedia";
import { RAIL_STYLE, useRailNav } from "../layout/Sidebar";
import { BRAND_PATHS } from "../home/Home";
import { useT } from "../../lib/useT";

const WIN_W = 1280;
const WIN_H = 720;
const PREVIEW_W = 480;
const SCALE = PREVIEW_W / WIN_W;

const SOCIALS = ["discord", "youtube", "x"];

export function HomePreview({ gameId }: { gameId: GameId }) {
  const t = useT();
  const values = useSettingsStore((s) => s.values);
  const slice = useCustomizationStore((s) => s.wallpaper[gameId]);
  const remote = useRemoteMediaStore((s) => s.media[gameId]);
  const chosen = wallpaperFor(gameId, slice, remote, false);
  const still = chosen.video ? wallpaperFor(gameId, undefined, remote, false).src : chosen.src;
  const shown = (hideKey: string) => values[hideKey] !== "true";

  return (
    <div
      role="img"
      aria-label={t("settings.appearance.homePreviewLabel")}
      className="relative box-content shrink-0 overflow-hidden rounded-ui border border-white/[0.12] bg-[#0a0e14]"
      style={{ width: PREVIEW_W, height: WIN_H * SCALE }}
    >
      <div
        aria-hidden="true"
        inert
        className="pointer-events-none absolute left-0 top-0 origin-top-left text-white"
        style={{ width: WIN_W, height: WIN_H, transform: `scale(${SCALE})` }}
      >
        <div className="absolute inset-0">
          <CrossfadeMedia src={still} video={false} />
          <div
            className="absolute inset-0"
            style={{
              background:
                "linear-gradient(180deg, rgba(8,8,12,.28) 0%, rgba(8,8,12,0) 30%, rgba(8,8,12,.45) 100%)",
            }}
          />
        </div>

        <div className="relative z-10 flex h-full">
          <PreviewRail gameId={gameId} />
          <main className="relative min-w-0 flex-1">
            {shown("hideSocials") && <Socials />}
            {shown("hideBottomRightButtons") && <QuickActions />}
            <div className="absolute bottom-[22px] left-[18px] z-[6] flex flex-col items-start gap-[12px]">
              {shown("hideNewsPanel") && <News banner={still} />}
              <StartBar gameId={gameId} showPlaytime={shown("hidePlaytime")} />
            </div>
          </main>
        </div>

        <div className="glass absolute right-[14px] top-[12px] z-20 flex items-center rounded-ui px-[6px] py-[2px]">
          {[Bell, Minus, Square, X].map((Icon, i) => (
            <span key={i} className="grid h-[30px] w-[30px] place-items-center rounded-[7px]">
              <Icon size={i === 2 ? 14 : 15} />
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}

function PreviewRail({ gameId }: { gameId: GameId }) {
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const committed = useLibraryStore((s) => s.visible);
  const appVersion = useUiStore((s) => s.appVersion);
  const buildType = useUiStore((s) => s.buildType);
  const nav = useRailNav();
  const showVersion = useSettingsStore((s) => s.values.hideSidebarVersion !== "true");
  const visible = committed.length ? committed : GAMES.map((g) => g.id);

  return (
    <aside className="relative z-20 flex w-[60px] flex-col" style={RAIL_STYLE}>
      <div className="flex min-h-0 flex-1 flex-col items-center px-0 pb-[11px] pt-[13px]">
        <img src="/icons/app.png" alt="" className="mb-[14px] h-[32px] w-[32px] shrink-0 rounded-ui object-contain" />
        <nav className="flex shrink-0 flex-col items-center gap-[6px]">
          {nav.map(({ view, icon: Icon }) => {
            const active = view === "home";
            return (
              <span
                key={view}
                className={`relative grid h-[40px] w-[40px] place-items-center rounded-ui ${
                  active ? "bg-white/[0.08] text-white" : "text-white/60"
                }`}
              >
                {active && (
                  <span className="accent-grad absolute left-[-10px] top-[9px] h-[22px] w-[3px] rounded-r-[3px]" />
                )}
                <Icon size={18} strokeWidth={2} />
              </span>
            );
          })}
        </nav>
        <div className="mt-[11px] h-px w-[26px] shrink-0 bg-white/10" />
        <div
          className="rail-scroll scrollbar-none flex min-h-0 flex-1 flex-col items-center gap-[6px] overflow-hidden py-[12px]"
        >
          {visible.map((id) => {
            const active = id === gameId;
            return (
              <span
                key={id}
                className={`relative grid h-[40px] w-[40px] shrink-0 place-items-center rounded-ui ${
                  active ? "bg-white/[0.08] text-white" : "text-white/60"
                }`}
              >
                {active && (
                  <span className="accent-grad absolute left-[-10px] top-[11px] h-[18px] w-[2px] rounded-r-[2px] opacity-80" />
                )}
                <img
                  src={iconSrcFor(id, icons)}
                  onError={iconFallback(id)}
                  alt=""
                  className="h-[34px] w-[34px] rounded-[9px] object-cover"
                />
              </span>
            );
          })}
        </div>
        {showVersion && (
          <div className="mt-auto flex shrink-0 flex-col items-center gap-[5px]">
            <span className="text-[9px] font-medium uppercase tracking-[0.1em] text-white/50">
              {CHANNEL_LABEL[buildType]}
            </span>
            {appVersion && <span className="text-[9px] tracking-[0.06em] text-white/50">v{appVersion}</span>}
          </div>
        )}
      </div>
      <span className="sr-only">{t("settings.appearance.homePreviewLabel")}</span>
    </aside>
  );
}

function Socials() {
  return (
    <div
      className="glass absolute right-0 top-[45%] z-[5] flex w-[56px] -translate-y-1/2 flex-col overflow-hidden py-[7px]"
      style={{ borderRadius: "11px 0 0 11px", borderRight: "none" }}
    >
      {SOCIALS.map((id) => (
        <span
          key={id}
          className="mx-[6px] my-[2px] flex h-[44px] shrink-0 items-center gap-[12px] rounded-[8px] px-[10px] text-white/90"
        >
          <span className="grid h-[24px] w-[24px] shrink-0 place-items-center">
            <svg viewBox="0 0 24 24" className="h-[22px] w-[22px]" fill="currentColor" aria-hidden>
              <path d={BRAND_PATHS[id]} />
            </svg>
          </span>
        </span>
      ))}
    </div>
  );
}

function News({ banner }: { banner: string }) {
  const t = useT();
  const titleWidths = ["72%", "58%", "66%"];
  return (
    <div className="glass flex w-[414px] flex-col overflow-hidden rounded-[11px] shadow-2xl">
      <div className="relative h-[162px] overflow-hidden bg-black/30">
        <img src={banner} alt="" className="absolute inset-0 h-full w-full object-cover" />
        <div className="absolute bottom-[4px] right-[6px] z-[2] flex">
          {[0, 1, 2].map((i) => (
            <span key={i} className="flex h-[12px] items-center px-[4px]">
              <span
                className="h-[4px] rounded-[2px]"
                style={{ width: i === 0 ? 14 : 4, background: i === 0 ? "#fff" : "rgba(255,255,255,.5)" }}
              />
            </span>
          ))}
        </div>
      </div>
      <div className="flex items-center gap-[8px] border-b border-white/[0.07] px-[15px] py-[10px]">
        <Newspaper size={14} className="shrink-0 text-white/45" />
        <h3 className="text-[12.5px] font-semibold text-white/85">{t("home.news.title")}</h3>
      </div>
      <div className="px-[9px] pb-[10px] pt-[4px]">
        <div className="min-h-[106px]">
          {titleWidths.map((w, i) => (
            <div
              key={i}
              className="flex h-[33px] w-full items-center justify-between gap-3 rounded-[7px] border-b border-white/[0.06] px-[7px] last:border-b-0"
            >
              <span className="h-[10px] rounded-[3px] bg-white/[0.55]" style={{ width: w }} />
              <span className="h-[8px] w-[34px] shrink-0 rounded-[3px] bg-white/[0.3]" />
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function StartBar({ gameId, showPlaytime }: { gameId: GameId; showPlaytime: boolean }) {
  const t = useT();
  const allTime = usePlaytimeStore((s) => s.data.pill[gameId]?.allTimeMinutes ?? 0);
  return (
    <div className="flex items-end gap-[14px]">
      <span className="start-button start-button-idle relative z-[1] grid h-[46px] w-[200px] place-items-center rounded-[12px] bg-white text-[15px] font-semibold tracking-[-0.005em] text-[#111]">
        <span className="flex items-center gap-[10px]">
          <span className="grid h-[16px] w-[16px] place-items-center">
            <Play size={16} fill="currentColor" strokeWidth={1.5} />
          </span>
          <span>{t("home.startBar.buttons.launch")}</span>
        </span>
      </span>
      {showPlaytime && (
        <Pop width={200}>
          <span className="flex h-[46px] w-full items-center gap-[8px] px-[16px] text-white">
            <Clock4 size={14} className="opacity-75" />
            <span className="text-[13px] font-medium text-white/70">{t("common.playtime")}</span>
            <span className="ml-auto whitespace-nowrap text-[14px] font-semibold">{fmtHM(allTime)}</span>
          </span>
        </Pop>
      )}
    </div>
  );
}

function QuickActions() {
  const t = useT();
  return (
    <div className="absolute bottom-[22px] right-4 z-[6] flex items-end gap-[10px]">
      <Pop width={236}>
        <MenuTrigger icon={Users} label={t("home.communityTools.title")} />
      </Pop>
      <Pop width={236}>
        <MenuTrigger icon={Settings} label={t("home.quickSettings.title")} />
      </Pop>
    </div>
  );
}

function MenuTrigger({ icon: Icon, label }: { icon: typeof Users; label: string }) {
  return (
    <span className="flex h-[46px] w-full items-center gap-[10px] px-[13px] text-white">
      <span className="grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] bg-white/[0.07] text-white/70">
        <Icon size={15} />
      </span>
      <span className="flex-1 truncate text-left text-[13px] font-medium">{label}</span>
      <span className="shrink-0 text-white/45">
        <ChevronUp size={16} />
      </span>
    </span>
  );
}

function Pop({ width, children }: { width: number; children: React.ReactNode }) {
  return (
    <div className="relative" style={{ width, height: 46 }}>
      <div className="glass absolute inset-x-0 bottom-0 flex flex-col overflow-hidden rounded-ui shadow-2xl">
        {children}
      </div>
    </div>
  );
}
