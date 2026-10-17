// ------------ News Panel ------------
// The news card at the bottom left of Home. Shows the game's banner slideshow plus its latest notices and news,
// fetched from the game's own feed and refreshed every ten minutes, with last time's copy shown while the first
// fetch runs. Clicking an item opens it in the browser.
import { useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import { Newspaper } from "lucide-react";
import { fadeVariants, useReducedMotionSafe } from "../../lib/motion";
import { gameSwitchWave } from "../layout/WallpaperLayer";
import { useUiStore } from "../../store/uiStore";
import { useLibraryStore } from "../../store/libraryStore";
import { gameById, type GameId } from "../../data/games";
import { useRendererSuspended } from "../../lib/launcherHooks";
import { getNewsData, getSavedNewsData, type NewsData, type NewsEntry, type NewsSlide } from "../../lib/ipc";
import { openExternal } from "../../lib/tauri";
import { Hint } from "../ui/Tooltip";
import { CrossfadeMedia } from "../ui/CrossfadeMedia";
import { formatLocale } from "../../lib/i18n";
import { useT } from "../../lib/useT";

const bannerSrc = (s: NewsSlide): string => s.url || "";

const entryKey = (i: NewsEntry): string => `${i.jumpUrl ?? ""}|${i.content ?? ""}`;

const DATE_AHEAD_MS = 2 * 86_400_000;
const NEWS_REFRESH_MS = 10 * 60_000;

type NewsDate = { year: number; month: number; day: number };

const parseNewsDate = (time: string | undefined, today: Date): NewsDate | null => {
  const match = /^(?:(\d{4})[-/.])?(\d{1,2})[-/.](\d{1,2})$/.exec((time ?? "").trim());
  if (!match) return null;
  const month = Number(match[2]);
  const day = Number(match[3]);
  if (month < 1 || month > 12 || day < 1 || day > 31) return null;
  let year = match[1] ? Number(match[1]) : today.getFullYear();
  if (!match[1]) {
    const t0 = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
    const at = (y: number) => new Date(y, month - 1, day).getTime();
    if (at(year) > t0 + DATE_AHEAD_MS) year -= 1;
    else if (at(year + 1) <= t0 + DATE_AHEAD_MS) year += 1;
  }
  return { year, month, day };
};

const dateRank = (time: string | undefined, today: Date): number => {
  const d = parseNewsDate(time, today);
  return d ? d.year * 10000 + d.month * 100 + d.day : -1;
};

const fmtNewsDate = (time: string | undefined, today: Date): string => {
  const d = parseNewsDate(time, today);
  if (!d) return time ?? "";
  return new Date(d.year, d.month - 1, d.day).toLocaleDateString(formatLocale(), {
    month: "short",
    day: "numeric",
    ...(d.year !== today.getFullYear() ? { year: "numeric" } : {}),
  });
};

const newsCache = new Map<string, NewsData>();

// ------------ Preload ------------
// Once the shown game's news is in, the other games on the rail fetch theirs in the background, one at a time, and
// download their first banner, so switching games finds the news and banner ready for the switch wave.
const PRELOAD_GAP_MS = 400;
let preloading = false;

function warmBanner(data: NewsData): void {
  const first = data.slideshow?.[0];
  if (!first?.url) return;
  const img = new Image();
  img.referrerPolicy = "no-referrer";
  img.src = bannerSrc(first);
}

async function preloadNews(gameIds: string[]): Promise<void> {
  if (preloading) return;
  preloading = true;
  try {
    for (const id of gameIds) {
      if (newsCache.has(id)) continue;
      const data = await getNewsData(id).catch(() => null);
      if (data) {
        newsCache.set(id, data);
        warmBanner(data);
      }
      await new Promise((resolve) => window.setTimeout(resolve, PRELOAD_GAP_MS));
    }
  } finally {
    preloading = false;
  }
}

export function NewsPanel() {
  const activeGameId = useUiStore((s) => s.activeGameId);
  const t = useT();

  const [data, setData] = useState<NewsData>(() => newsCache.get(activeGameId) ?? {});
  const [loading, setLoading] = useState(!newsCache.has(activeGameId));
  const [slide, setSlide] = useState(0);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const suspended = useRendererSuspended();
  const lastFetch = useRef(0);
  const shownGame = useRef<string | null>(null);
  const bannerRef = useRef<HTMLDivElement>(null);
  const reduce = useReducedMotionSafe();

  useEffect(() => {
    let alive = true;
    const cached = newsCache.get(activeGameId);
    if (shownGame.current !== activeGameId) {
      shownGame.current = activeGameId;
      setData(cached ?? {});
      setSlide(0);
    }
    setLoading(!cached);
    setFailed(false);
    lastFetch.current = Date.now();
    const fail = () => {
      if (!alive) return;
      setFailed(!cached);
      setLoading(false);
    };
    if (!cached) {
      void getSavedNewsData(activeGameId).then((saved) => {
        if (!alive || !saved || newsCache.has(activeGameId)) return;
        setData(saved);
        setLoading(false);
      });
    }
    void getNewsData(activeGameId)
      .then((d) => {
        if (!d) return fail();
        newsCache.set(activeGameId, d);
        if (alive) {
          setData(d);
          setSlide((s) => (s < (d.slideshow?.length ?? 0) ? s : 0));
          setLoading(false);
        }
      })
      .catch(fail);
    return () => {
      alive = false;
    };
  }, [activeGameId, attempt]);

  const railGames = useLibraryStore((s) => s.visible);
  useEffect(() => {
    if (loading || suspended) return;
    void preloadNews(railGames.filter((id) => id !== activeGameId));
  }, [loading, suspended, railGames, activeGameId]);

  useEffect(() => {
    if (suspended) return;
    const refreshIfStale = () => {
      if (Date.now() - lastFetch.current >= NEWS_REFRESH_MS) setAttempt((n) => n + 1);
    };
    refreshIfStale();
    const timer = setInterval(refreshIfStale, NEWS_REFRESH_MS);
    return () => clearInterval(timer);
  }, [suspended]);

  const slides = data.slideshow ?? [];
  const items = useMemo(() => {
    const notices = data.guidance?.notice?.contents ?? [];
    const seen = new Set(notices.map(entryKey));
    const news = (data.guidance?.news?.contents ?? []).filter((i) => !seen.has(entryKey(i)));
    const today = new Date();
    return [...notices, ...news]
      .map((item, order) => ({
        item,
        order,
        rank: dateRank(item.time, today),
      }))
      .sort((a, b) => b.rank - a.rank || a.order - b.order);
  }, [data]);

  useEffect(() => {
    if (hovered || focused || suspended || slides.length <= 1) return;
    const timer = setInterval(() => setSlide((s) => (s + 1) % slides.length), 10000);
    return () => clearInterval(timer);
  }, [hovered, focused, suspended, slides.length, activeGameId]);

  const banner = slides[slide] ?? slides[0];
  const today = new Date();
  const bannerLabelled = !!banner && !banner.jumpUrl;

  return (
    <div className="glass flex w-[414px] flex-col overflow-hidden rounded-[11px] shadow-2xl">
      <div
        ref={bannerRef}
        className="relative h-[162px] overflow-hidden bg-black/30"
        onMouseEnter={() => setHovered(true)}
        onMouseLeave={() => setHovered(false)}
        onFocus={() => setFocused(true)}
        onBlur={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocused(false);
        }}
      >
        <div
          role={bannerLabelled ? "img" : undefined}
          aria-hidden={bannerLabelled ? undefined : true}
          aria-label={
            bannerLabelled ? t("home.news.banner", { n: slides.indexOf(banner) + 1, total: slides.length }) : undefined
          }
          className="absolute inset-0 h-full w-full"
        >
          <CrossfadeMedia
            src={banner ? bannerSrc(banner) : gameById(activeGameId as GameId).wallpaperStatic}
            video={false}
            wave={reduce ? undefined : () => gameSwitchWave(bannerRef.current)}
          />
        </div>
        {banner?.jumpUrl && (
          <button
            aria-label={t("home.news.openBanner", { n: slides.indexOf(banner) + 1, total: slides.length })}
            onClick={() => banner.jumpUrl && void openExternal(banner.jumpUrl)}
            className="absolute inset-0 h-full w-full"
          />
        )}

        {slides.length > 1 && (
          <div className="absolute bottom-[4px] right-[6px] z-[2] flex">
            {slides.map((_, i) => (
              <button
                key={i}
                aria-label={t("home.news.showBanner", { n: i + 1 })}
                aria-current={i === slide || undefined}
                onClick={() => setSlide(i)}
                className="flex h-[12px] items-center px-[4px]"
              >
                <span
                  className="h-[4px] rounded-[2px] transition-all"
                  style={{
                    width: i === slide ? 14 : 4,
                    background: i === slide ? "#fff" : "rgba(255,255,255,.5)",
                  }}
                />
              </button>
            ))}
          </div>
        )}
      </div>

      <div className="flex items-center gap-[8px] border-b border-white/[0.07] px-[15px] py-[10px]">
        <Newspaper size={14} className="shrink-0 text-white/45" />
        <h3 className="text-[12.5px] font-semibold text-white/85">{t("home.news.title")}</h3>
      </div>

      <div className="px-[9px] pb-[10px] pt-[4px]">
        <div className="h-[132px] overflow-y-auto">
          <AnimatePresence mode="wait" initial={false}>
            <m.div key={`${activeGameId}:${loading}:${failed}`} variants={fadeVariants} initial="initial" animate="animate" exit="exit">
              {loading ? (
                <div role="status" className="animate-pulse px-[7px] py-[10px]">
                  <span className="sr-only">{t("home.news.loading")}</span>
                  {[0, 1, 2].map((i) => (
                    <div key={i} aria-hidden className="mb-[10px] h-[13px] rounded-[4px] bg-white/[0.07] last:mb-0" style={{ width: `${88 - i * 14}%` }} />
                  ))}
                </div>
              ) : items.length === 0 && failed ? (
                <div className="flex flex-col items-center gap-[8px] px-[7px] py-[12px]">
                  <p className="text-center text-[12px] text-white/40">{t("home.news.failed")}</p>
                  <button
                    onClick={() => setAttempt((n) => n + 1)}
                    className="rounded-[6px] border border-white/[0.12] px-[10px] py-[3px] text-[11.5px] font-medium text-white/75 transition-colors hover:bg-white/[0.08]"
                  >
                    {t("common.retry")}
                  </button>
                </div>
              ) : items.length === 0 ? (
                <p className="px-[7px] py-[16px] text-center text-[12px] text-white/40">{t("home.news.empty")}</p>
              ) : (
                items.map(({ item }, idx) => {
                  const date = fmtNewsDate(item.time, today);
                  const key = `${entryKey(item)}|${idx}`;
                  const body = (
                    <>
                      <span className="min-w-0 truncate text-[12.5px] font-medium leading-[1.3]">{item.content}</span>
                      <span className="shrink-0 text-[10.5px] text-white/[0.42]">{date}</span>
                    </>
                  );
                  const rowClass =
                    "flex h-[33px] w-full items-center justify-between gap-3 rounded-[7px] border-b border-white/[0.06] px-[7px] text-left last:border-b-0";
                  const url = item.jumpUrl;
                  return (
                    <Hint key={key} tip={item.content} className="block w-full">
                      {url ? (
                        <button
                          onClick={() => void openExternal(url)}
                          className={`${rowClass} transition-colors hover:bg-white/[0.05]`}
                        >
                          {body}
                        </button>
                      ) : (
                        <div className={rowClass}>{body}</div>
                      )}
                    </Hint>
                  );
                })
              )}
            </m.div>
          </AnimatePresence>
        </div>
      </div>
    </div>
  );
}
