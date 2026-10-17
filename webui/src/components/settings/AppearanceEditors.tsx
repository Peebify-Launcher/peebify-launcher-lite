// ------------ Appearance Editors ------------
// The Appearance settings tab's custom sections: the home screen switches beside a preview of the launcher, the
// per-game wallpaper and icon cards with their previews, and the playtime chart colors with a sample chart.
import { useCallback, useMemo, useRef, useState, type ReactNode } from "react";
import { FolderOpen, RotateCcw } from "lucide-react";
import { GAMES, gameById, gameColor, isGameColor, type GameId } from "../../data/games";
import type { Setting } from "../../data/settings";
import { useLibraryStore } from "../../store/libraryStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { useRemoteMediaStore } from "../../store/remoteContentStore";
import { useSettingsStore } from "../../store/settingsStore";
import { useSessionUiStore } from "../../store/sessionUiStore";
import { selectWallpaperFile, selectGameIconFile, type MediaSlice, type RemoteMedia } from "../../lib/ipc";
import { iconFallback, iconSrcFor, wallpaperFor } from "../../lib/customMedia";
import { CrossfadeMedia } from "../ui/CrossfadeMedia";
import { useRendererSuspended, useWindowFocused } from "../../lib/launcherHooks";
import { useGamesStore } from "../../store/gamesStore";
import { ColorPopover } from "../ui/ColorPicker";
import { Hint } from "../ui/Tooltip";
import { ActionButton } from "../ui/controls";
import { GroupBox, GroupLabel } from "../ui/panels";
import { SettingRow } from "./SettingRow";
import { HomePreview } from "./HomePreview";
import { formatLocale } from "../../lib/i18n";
import { useT } from "../../lib/useT";

const CARD = "flex min-w-0 flex-col gap-[14px] rounded-[11px] border border-white/[0.08] bg-white/[0.05] p-5";
const STAGE = "relative overflow-hidden rounded-ui border border-white/[0.12] bg-[#0a0e14]";
const GLASS = "border border-white/10 bg-[rgba(18,18,22,0.6)] backdrop-blur-[20px] backdrop-saturate-[1.8]";

const COLOR_PRESETS = [
  ...new Set([...GAMES.map((g) => gameColor(g.id)), "#ffffff", "#f472b6", "#34d399", "#60a5fa"]),
].slice(0, 16);

const SAMPLE_WEEK_MINUTES: number[][] = [
  [46, 22],
  [30, 34, 12],
  [40, 18],
  [58, 18],
  [26, 20, 16],
  [48, 28, 10],
  [44, 36],
];

function stillWallpaper(gameId: GameId, slice: MediaSlice | undefined, remote: RemoteMedia | undefined) {
  const chosen = wallpaperFor(gameId, slice, remote, false);
  return chosen.video ? wallpaperFor(gameId, undefined, remote, false).src : chosen.src;
}

function useSidebarGames() {
  const visible = useLibraryStore((s) => s.visible);
  return useMemo(() => (visible.length ? visible.map(gameById) : GAMES), [visible]);
}

function Section({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section>
      <GroupLabel label={label} />
      {children}
    </section>
  );
}

export function HomeScreenGroup({ settings }: { settings: Setting[] }) {
  const t = useT();
  const gameId = useSessionUiStore((s) => s.settingsGameId);
  return (
    <Section label={t("settings.groups.homeScreen")}>
      <GroupBox className="flex flex-wrap items-stretch">
        <div className="flex flex-none flex-col gap-[10px] p-5">
          <HomePreview gameId={gameId} />
        </div>
        <div className="min-w-0 flex-[1_1_420px] border-l border-white/[0.06]">
          {settings.map((s) => (
            <SettingRow key={s.id} setting={s} />
          ))}
        </div>
      </GroupBox>
    </Section>
  );
}

function GamePicker({
  value,
  onChange,
  icons,
}: {
  value: GameId;
  onChange: (id: GameId) => void;
  icons: Record<string, MediaSlice>;
}) {
  const t = useT();
  const sidebar = useSidebarGames();
  const listed = sidebar.some((g) => g.id === value) ? sidebar : [...sidebar, gameById(value)];
  return (
    <div
      role="group"
      aria-label={t("settings.appearance.chooseGame")}
      className="mb-3 flex gap-[6px] overflow-x-auto p-[3px] [scrollbar-width:thin]"
    >
      {listed.map((g) => {
        const active = value === g.id;
        return (
          <Hint key={g.id} tip={g.name} className="inline-flex shrink-0">
            <button
              onClick={() => onChange(g.id)}
              aria-pressed={active}
              aria-label={g.name}
              className={`grid h-[42px] w-[42px] place-items-center rounded-ui border transition-colors ${
                active
                  ? "border-white/60 bg-white/[0.12]"
                  : "border-transparent opacity-60 hover:border-white/20 hover:bg-white/[0.06] hover:opacity-100"
              }`}
            >
              <img
                src={iconSrcFor(g.id, icons)}
                onError={iconFallback(g.id)}
                alt=""
                className="h-[30px] w-[30px] rounded-[7px] object-cover"
              />
            </button>
          </Hint>
        );
      })}
    </div>
  );
}

export function GameArtwork() {
  const t = useT();
  const gameId = useSessionUiStore((s) => s.settingsGameId);
  const setGameId = useSessionUiStore((s) => s.setSettingsGameId);
  const gameIcons = useCustomizationStore((s) => s.gameIcons);
  return (
    <Section label={`${t("settings.appearance.wallpaperAndIcon")} · ${gameById(gameId).name}`}>
      <GamePicker value={gameId} onChange={setGameId} icons={gameIcons} />
      <div className="flex flex-wrap items-stretch gap-4">
        <WallpaperCard gameId={gameId} />
        <IconCard gameId={gameId} />
      </div>
    </Section>
  );
}

function StageTag({ custom, side = "left-[10px]" }: { custom: boolean; side?: string }) {
  const t = useT();
  return (
    <span
      className={`absolute ${side} top-[10px] rounded-full px-[9px] py-1 text-[10px] font-semibold uppercase tracking-[0.08em] text-white/85 ${GLASS}`}
    >
      {custom ? t("settings.appearance.customTag") : t("settings.appearance.defaultTag")}
    </span>
  );
}

function WallpaperCard({ gameId }: { gameId: GameId }) {
  const t = useT();
  const suspended = useRendererSuspended();
  const focused = useWindowFocused();
  const gameRunning = useGamesStore((s) => s.runningGames.length > 0);
  const slice = useCustomizationStore((s) => s.wallpaper[gameId]);
  const setWallpaper = useCustomizationStore((s) => s.setWallpaper);
  const remote = useRemoteMediaStore((s) => s.media[gameId]);
  const animated = useSettingsStore((s) => s.values.animatedWallpaper) !== "false";
  const { src, video } = wallpaperFor(gameId, slice, remote, animated);
  const custom = slice?.type === "custom";
  const game = gameById(gameId);

  const pick = async () => {
    const p = await selectWallpaperFile();
    if (p) setWallpaper(gameId, { type: "custom", path: p });
  };

  return (
    <div className={`${CARD} flex-[3_1_460px]`}>
      <div>
        <h5 className="text-[14px] font-medium text-white">{t("settings.appearance.wallpaper")}</h5>
        <p className="mt-[2px] text-[12.5px] leading-snug text-white/55">
          {t("settings.appearance.customWallpaperDescription")}
        </p>
      </div>
      <div className={`${STAGE} aspect-video w-full`}>
        <CrossfadeMedia
          src={src}
          video={video}
          autoPlay={animated}
          playing={!suspended && (focused || !gameRunning)}
        />
        <StageTag custom={custom} />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-[10px]">
        <span className="text-[11.5px] text-white/55">
          {custom
            ? t("settings.appearance.customWallpaperFor", { game: game.name })
            : t("settings.appearance.defaultWallpaperFor", { game: game.name })}
        </span>
        <div className="flex gap-2">
          <ActionButton icon={<FolderOpen size={14} />} onClick={() => void pick()}>
            {t("settings.appearance.selectFile")}
          </ActionButton>
          <ActionButton
            icon={<RotateCcw size={14} />}
            disabled={!custom}
            onClick={() => setWallpaper(gameId, { type: "default", path: null })}
          >
            {t("common.reset")}
          </ActionButton>
        </div>
      </div>
    </div>
  );
}

function IconCard({ gameId }: { gameId: GameId }) {
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const setGameIcon = useCustomizationStore((s) => s.setGameIcon);
  const wallSlice = useCustomizationStore((s) => s.wallpaper[gameId]);
  const remote = useRemoteMediaStore((s) => s.media[gameId]);
  const sidebar = useSidebarGames();
  const rail = railWindow(sidebar.some((g) => g.id === gameId) ? sidebar : [...sidebar, gameById(gameId)], gameId);
  const custom = icons[gameId]?.type === "custom";
  const game = gameById(gameId);
  const [failed, setFailed] = useState<ReadonlySet<string>>(() => new Set());
  const iconSrc = iconSrcFor(gameId, icons);

  const pick = async () => {
    const p = await selectGameIconFile();
    if (p) setGameIcon(gameId, { type: "custom", path: p });
  };

  return (
    <div className={`${CARD} flex-[2_1_320px]`}>
      <div>
        <h5 className="text-[14px] font-medium text-white">{t("settings.appearance.gameIcon")}</h5>
        <p className="mt-[2px] text-[12.5px] leading-snug text-white/55">
          {t("settings.appearance.gameIconDescription")}
        </p>
      </div>
      <div className={`${STAGE} flex min-h-[200px] flex-auto`}>
        <div className="absolute inset-0 scale-110 blur-[6px] brightness-[.55]">
          <CrossfadeMedia src={stillWallpaper(gameId, wallSlice, remote)} video={false} />
        </div>
        <div
          aria-hidden="true"
          className="relative flex w-[52px] flex-none flex-col items-center justify-center gap-1 overflow-hidden border-r border-white/[0.07] bg-[rgba(14,14,18,0.7)] py-3"
        >
          {rail.map((g) => (
            <span
              key={g.id}
              className={`grid h-[34px] w-[34px] shrink-0 place-items-center rounded-[8px] transition-colors ${
                g.id === gameId ? "bg-white/[0.08]" : ""
              }`}
            >
              <img
                src={iconSrcFor(g.id, icons)}
                onError={iconFallback(g.id)}
                alt=""
                className="h-6 w-6 rounded-[6px] object-cover"
              />
            </span>
          ))}
        </div>
        <div className="relative flex flex-auto flex-col items-center justify-center gap-[10px] p-4">
          <div
            className="relative h-[84px] w-[84px] overflow-hidden rounded-[18px]"
            style={{ boxShadow: "0 0 0 1px rgba(255,255,255,.12), 0 10px 24px -8px rgba(0,0,0,.7)" }}
          >
            <CrossfadeMedia
              src={failed.has(iconSrc) ? game.icon : iconSrc}
              video={false}
              onError={(src) => setFailed((prev) => new Set(prev).add(src))}
            />
          </div>
          <span className="text-center text-[13px] font-medium text-white">{game.name}</span>
        </div>
        <StageTag custom={custom} side="right-[10px]" />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-[10px]">
        <span className="text-[11.5px] text-white/55">
          {custom ? t("settings.appearance.customIcon") : t("settings.appearance.defaultIcon")}
        </span>
        <div className="flex gap-2">
          <ActionButton icon={<FolderOpen size={14} />} onClick={() => void pick()}>
            {t("settings.appearance.selectIcon")}
          </ActionButton>
          <ActionButton
            icon={<RotateCcw size={14} />}
            disabled={!custom}
            onClick={() => setGameIcon(gameId, { type: "default", path: null })}
          >
            {t("common.reset")}
          </ActionButton>
        </div>
      </div>
    </div>
  );
}

const RAIL_WINDOW = 5;

function railWindow<T extends { id: GameId }>(games: T[], selected: GameId): T[] {
  const at = Math.max(0, games.findIndex((g) => g.id === selected));
  const start = Math.min(Math.max(0, at - Math.floor(RAIL_WINDOW / 2)), Math.max(0, games.length - RAIL_WINDOW));
  return games.slice(start, start + RAIL_WINDOW);
}

function weekdayLabels(): string[] {
  const fmt = new Intl.DateTimeFormat(formatLocale(), { weekday: "short" });
  return Array.from({ length: 7 }, (_, i) => fmt.format(new Date(2024, 0, 1 + i)));
}

export function GameColorsCard() {
  const t = useT();
  const overrides = useCustomizationStore((s) => s.playtimeColor);
  const setPlaytimeColor = useCustomizationStore((s) => s.setPlaytimeColor);
  const gameIcons = useCustomizationStore((s) => s.gameIcons);
  const listed = useSidebarGames();
  const days = weekdayLabels();
  const colorAt = (slot: number) => gameColor(listed[slot % listed.length].id, overrides);
  let nextSlot = 0;
  const week = SAMPLE_WEEK_MINUTES.map((day) => day.map((minutes) => [nextSlot++, minutes] as const));

  return (
    <Section label={t("settings.appearance.gameColors")}>
      <div className={`${CARD} flex-row flex-wrap gap-6`}>
        <div className="flex min-w-[260px] flex-[0_1_340px] flex-col gap-[10px]">
          <div>
            <h5 className="text-[14px] font-medium text-white">{t("settings.appearance.chartColors")}</h5>
            <p className="mt-[2px] text-[12.5px] leading-snug text-white/55">
              {t("settings.appearance.gameColorsDescription")}
            </p>
          </div>
          <div
            role="img"
            aria-label={t("settings.appearance.chartPreviewLabel")}
            className={`${STAGE} flex h-[170px] items-end gap-[10px] bg-black/30 px-4 pb-[26px] pt-4`}
          >
            {week.map((parts, i) => (
              <div key={i} className="relative flex flex-1 flex-col-reverse gap-[2px]">
                {parts.map(([slot, minutes], j) => (
                  <span
                    key={j}
                    className="block rounded-[3px] transition-colors duration-200"
                    style={{ height: minutes, background: colorAt(slot) }}
                  />
                ))}
                <span className="absolute inset-x-0 -bottom-[18px] text-center text-[10px] text-white/45">
                  {days[i]}
                </span>
              </div>
            ))}
          </div>
        </div>

        <div className="grid min-w-0 flex-[1_1_360px] content-start gap-x-4 gap-y-[2px] [grid-template-columns:repeat(auto-fill,minmax(250px,1fr))]">
          {listed.map((game) => (
            <GameColorRow
              key={game.id}
              gameId={game.id}
              name={game.name}
              icons={gameIcons}
              custom={isGameColor(overrides[game.id])}
              value={gameColor(game.id, overrides)}
              onChange={setPlaytimeColor}
            />
          ))}
        </div>
      </div>
    </Section>
  );
}

function GameColorRow({
  gameId,
  name,
  icons,
  custom,
  value,
  onChange,
}: {
  gameId: GameId;
  name: string;
  icons: Record<string, MediaSlice>;
  custom: boolean;
  value: string;
  onChange: (id: GameId, color: string | null) => void;
}) {
  const t = useT();
  const swatchRef = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);

  return (
    <div
      className={`flex items-center gap-3 rounded-[8px] px-2 py-[7px] transition-colors hover:bg-white/[0.04] ${open ? "bg-white/[0.04]" : ""}`}
    >
      <img
        src={iconSrcFor(gameId, icons)}
        onError={iconFallback(gameId)}
        alt=""
        className="h-[22px] w-[22px] shrink-0 rounded-[5px] object-cover"
      />
      <span className="min-w-0 flex-1 truncate text-[13px] text-white/80">{name}</span>
      <span className="shrink-0 font-mono text-[11.5px] uppercase text-white/55">{value}</span>
      <button
        ref={swatchRef}
        onClick={() => setOpen((o) => !o)}
        aria-label={t("settings.appearance.changeColor", { game: name })}
        aria-haspopup="dialog"
        aria-expanded={open}
        className={`h-[24px] w-[34px] shrink-0 rounded-[6px] border transition duration-150 hover:scale-[1.06] active:scale-[0.96] ${
          open ? "border-white/70 ring-2 ring-white/25" : "border-white/15"
        }`}
        style={{ background: value }}
      />
      <ColorPopover
        open={open}
        onClose={close}
        anchorRef={swatchRef}
        value={value}
        onChange={(hex) => onChange(gameId, hex)}
        presets={COLOR_PRESETS}
        label={t("settings.appearance.colorFor", { game: name })}
      />
      <Hint
        tip={custom ? t("settings.appearance.resetColorTip", { game: name }) : undefined}
        placement="left"
        className="flex shrink-0"
      >
        <button
          onClick={() => onChange(gameId, null)}
          disabled={!custom}
          aria-label={t("settings.appearance.resetColor", { game: name })}
          className="grid h-[24px] w-[24px] place-items-center rounded-[6px] border border-white/[0.08] bg-white/[0.04] text-white/60 transition-colors hover:bg-white/[0.1] hover:text-white disabled:cursor-not-allowed disabled:opacity-25"
        >
          <RotateCcw size={12} />
        </button>
      </Hint>
    </div>
  );
}
