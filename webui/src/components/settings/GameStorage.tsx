// ------------ Game Storage ------------
// The Storage section on the Games settings tab: how much disk the selected game, its mods and its captures use.
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { HardDrive, Images, Package, RefreshCw } from "lucide-react";
import { type GameId, gameById } from "../../data/games";
import { fmtBytes } from "../../lib/format";
import { getGameStorage, openGameFolder, openScreenshotFolder, type GameStorage as Storage } from "../../lib/ipc";
import { isActiveJob } from "../../lib/download";
import { useGamesStore } from "../../store/gamesStore";
import { useQueueStore } from "../../store/queueStore";
import { useUiStore } from "../../store/uiStore";
import { useGameColor } from "../../store/customizationStore";
import { GroupBox, GroupLabel } from "../ui/panels";
import { ActionButton } from "../ui/controls";
import { useT } from "../../lib/useT";

// ------------ Measuring ------------
// A scan walks the whole install, so each game is measured once at a time and the result kept for the session. Settings
// measures every installed game in the background when it opens, so the bar has numbers before a game is picked.
const cache = new Map<GameId, Storage>();
const inflight = new Map<GameId, Promise<Storage | null>>();

function scan(gameId: GameId): Promise<Storage | null> {
  const running = inflight.get(gameId);
  if (running) return running;
  const next = getGameStorage(gameId)
    .then((storage) => {
      if (storage) cache.set(gameId, storage);
      return storage;
    })
    .finally(() => inflight.delete(gameId));
  inflight.set(gameId, next);
  return next;
}

const jobRunning = (gameId: GameId) =>
  useQueueStore.getState().jobs.some((j) => j.gameId === gameId && isActiveJob(j));

export async function preloadGameStorage(ids: readonly string[]): Promise<void> {
  for (const id of ids as readonly GameId[]) {
    if (cache.has(id) || jobRunning(id) || !useGamesStore.getState().installed.includes(id)) continue;
    await scan(id);
  }
}

// ------------ Section ------------
// Game files take the game's playtime color; the rest come from the same palette.
const SEGMENTS = [
  { key: "game", label: "settings.storage.gameFiles" },
  { key: "mods", label: "settings.storage.mods" },
  { key: "media", label: "settings.storage.media" },
  { key: "other", label: "settings.storage.other" },
] as const;

const FIXED_TONES = {
  mods: "#c084fc",
  media: "#ffa726",
  other: "rgba(148,163,184,0.55)",
} as const;

const FREE_TONE = "rgba(255,255,255,0.07)";

export function GameStorage({ gameId }: { gameId: GameId }) {
  const t = useT();
  const game = gameById(gameId);
  const color = useGameColor();
  const tones = { game: color(gameId), ...FIXED_TONES };
  const installed = useGamesStore((s) => s.installed.includes(gameId));
  const jobActive = useQueueStore((s) => s.jobs.some((j) => j.gameId === gameId && isActiveJob(j)));
  const [data, setData] = useState<{ gameId: GameId; storage: Storage } | null>(() => {
    const hit = cache.get(gameId);
    return hit ? { gameId, storage: hit } : null;
  });
  const [scanning, setScanning] = useState(false);
  const [tick, setTick] = useState(0);

  const shown = data?.gameId === gameId ? data.storage : (cache.get(gameId) ?? null);

  useEffect(() => {
    if (!installed || jobActive) return;
    let alive = true;
    setScanning(true);
    void scan(gameId).then((storage) => {
      if (!alive) return;
      setScanning(false);
      if (storage) setData({ gameId, storage });
    });
    return () => {
      alive = false;
      setScanning(false);
    };
  }, [gameId, installed, jobActive, tick]);

  const rescan = useCallback(() => setTick((n) => n + 1), []);

  if (!installed) {
    return (
      <section>
        <GroupLabel label={t("settings.storage.title")} />
        <GroupBox>
          <Row
            icon={<HardDrive size={16} />}
            title={t("settings.storage.nothingTitle")}
            description={t("settings.storage.nothingText", { game: game.name })}
          />
        </GroupBox>
      </section>
    );
  }

  const modsBytes = shown?.mods?.bytes ?? 0;
  const mediaBytes = shown?.media.bytes ?? 0;
  const gameBytes = shown?.gameBytes ?? 0;
  const totalBytes = gameBytes + modsBytes + mediaBytes;
  const driveTotal = shown?.total ?? 0;
  const driveFree = shown?.free ?? 0;
  const diskKnown = shown?.total != null && shown.free != null && driveTotal > 0;
  const usedBytes = diskKnown ? Math.max(totalBytes, driveTotal - driveFree) : totalBytes;
  const usedWidth = shown && usedBytes > 0 ? (diskKnown ? (usedBytes / driveTotal) * 100 : 100) : 0;
  const parts = { game: gameBytes, mods: modsBytes, media: mediaBytes };
  const segments = SEGMENTS.filter((s) => s.key !== "other" || diskKnown);

  const mediaText = !shown
    ? " "
    : shown.media.screenshots + shown.media.clips === 0
      ? t("settings.storage.noMedia")
      : [
          shown.media.screenshots > 0
            ? t("settings.storage.screenshots", { count: shown.media.screenshots })
            : null,
          shown.media.clips > 0 ? t("settings.storage.clips", { count: shown.media.clips }) : null,
        ]
          .filter(Boolean)
          .join(" · ");

  const modsText = !shown
    ? " "
    : !shown.mods
      ? t("settings.storage.modsUnsupported", { game: game.name })
      : shown.mods.installed === 0
        ? t("settings.storage.noMods")
        : t("settings.storage.modCount", { count: shown.mods.installed });

  const openMods = () => {
    const ui = useUiStore.getState();
    if (ui.activeGameId !== gameId) ui.setActiveGame(gameId);
    ui.setView("mods");
  };

  return (
    <section>
      <div className="mb-[10px] flex items-center justify-between gap-3 px-1">
        <h3 className="text-[12px] font-semibold uppercase tracking-[0.07em] text-white/55">
          {t("settings.storage.title")}
        </h3>
        <button
          type="button"
          onClick={rescan}
          disabled={scanning || jobActive}
          className="-my-1 inline-flex items-center gap-[6px] rounded-[8px] px-2 py-1 text-[12px] font-medium text-white/60 transition hover:bg-white/[0.06] hover:text-white disabled:cursor-default disabled:hover:bg-transparent disabled:hover:text-white/60"
        >
          <RefreshCw size={13} className={scanning ? "animate-spin" : undefined} />
          {t("settings.storage.rescan")}
        </button>
      </div>
      <GroupBox>
        <div className="px-5 pb-4 pt-[18px]">
          <div className="flex flex-wrap items-end justify-between gap-x-6 gap-y-2">
            <div>
              <div
                className="text-[26px] font-semibold leading-[1.1] tracking-[-0.01em] tabular-nums"
                style={{ fontFamily: "var(--font-display)" }}
              >
                {shown ? fmtBytes(totalBytes) : " "}
              </div>
              <div className="mt-1 text-[12.5px] leading-snug text-white/55">
                {shown?.drive
                  ? t("settings.storage.usedByOn", { game: game.name, drive: shown.drive })
                  : t("settings.storage.usedBy", { game: game.name })}
              </div>
            </div>
            {shown?.free != null && shown.total != null && (
              <div className="text-right text-[12.5px] leading-snug text-white/70 tabular-nums">
                {t("settings.storage.freeOf", { free: fmtBytes(shown.free), total: fmtBytes(shown.total) })}
              </div>
            )}
          </div>
          <div
            role="img"
            aria-label={t("settings.storage.breakdown", {
              game: fmtBytes(gameBytes),
              mods: fmtBytes(modsBytes),
              media: fmtBytes(mediaBytes),
              free: diskKnown ? fmtBytes(driveFree) : "?",
            })}
            className="mt-[14px] flex h-2 overflow-hidden rounded-full"
            style={{ background: FREE_TONE }}
          >
            <div
              className="flex h-full shrink-0 overflow-hidden rounded-full transition-[width] duration-300"
              style={{ width: `${usedWidth}%` }}
            >
              {(["game", "mods", "media"] as const).map((key) => {
                const bytes = parts[key];
                return (
                  <span
                    key={key}
                    className="h-full shrink-0 rounded-full transition-[width,min-width,margin] duration-300"
                    style={{
                      width: usedBytes > 0 ? `${(bytes / usedBytes) * 100}%` : 0,
                      minWidth: bytes > 0 ? 4 : 0,
                      marginRight: bytes > 0 ? 2 : 0,
                      background: tones[key],
                    }}
                  />
                );
              })}
              {diskKnown && (
                <span className="h-full min-w-0 flex-1 rounded-full" style={{ background: tones.other }} />
              )}
            </div>
          </div>
          <div className="mt-[10px] flex flex-wrap gap-x-[18px] gap-y-[6px] text-[11.5px] text-white/60">
            {segments.map((s) => (
              <span key={s.key} className="inline-flex items-center gap-[6px]">
                <span
                  className="h-2 w-2 rounded-[2px]"
                  style={{ background: tones[s.key] }}
                />
                {t(s.label)}
              </span>
            ))}
            {diskKnown && (
              <span className="inline-flex items-center gap-[6px]">
                <span
                  className="h-2 w-2 rounded-[2px]"
                  style={{ background: FREE_TONE, boxShadow: "inset 0 0 0 1px rgba(255,255,255,0.2)" }}
                />
                {t("settings.storage.free")}
              </span>
            )}
          </div>
        </div>
        <Row
          icon={<HardDrive size={16} />}
          title={t("settings.storage.gameFiles")}
          description={shown?.gamePath || " "}
          mono
          size={shown ? fmtBytes(gameBytes) : null}
          action={
            <ActionButton onClick={() => void openGameFolder(gameId)}>{t("common.open")}</ActionButton>
          }
        />
        <Row
          icon={<Package size={16} />}
          title={t("settings.storage.mods")}
          description={modsText}
          size={shown?.mods ? fmtBytes(modsBytes) : null}
          action={
            <ActionButton onClick={openMods} disabled={!shown?.mods}>
              {t("settings.storage.manage")}
            </ActionButton>
          }
        />
        <Row
          icon={<Images size={16} />}
          title={t("settings.storage.media")}
          description={mediaText}
          size={shown ? fmtBytes(mediaBytes) : null}
          action={
            <ActionButton onClick={() => void openScreenshotFolder(gameId)}>
              {t("common.open")}
            </ActionButton>
          }
        />
      </GroupBox>
    </section>
  );
}

function Row({
  icon,
  title,
  description,
  mono = false,
  size,
  action,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  mono?: boolean;
  size?: string | null;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-4 px-5 py-[14px]">
      <div className="flex min-w-0 items-center gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[8px] bg-white/[0.05] text-white/70">
          {icon}
        </span>
        <div className="min-w-0">
          <h5 className="text-[14px] font-medium text-white">{title}</h5>
          <p
            className={`mt-[2px] truncate leading-snug text-white/55 ${mono ? "font-mono text-[11.5px]" : "text-[12.5px]"}`}
          >
            {description}
          </p>
        </div>
      </div>
      {(size !== undefined || action) && (
        <div className="flex shrink-0 items-center gap-[14px]">
          {size !== undefined && (
            <span className="min-w-[64px] text-right text-[13px] font-medium tabular-nums text-white">
              {size ?? ""}
            </span>
          )}
          {action}
        </div>
      )}
    </div>
  );
}
