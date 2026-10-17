// ------------ Browse Mods ------------
// The Browse view inside Mods. Searches and filters GameBanana for the selected game, opens a mod's page with
// its images and files, and installs or updates a mod in one click.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { AnimatePresence } from "framer-motion";
import {
  ArrowLeft,
  Check,
  ChevronLeft,
  ChevronRight,
  Download,
  ExternalLink,
  Eye,
  Heart,
  ShieldAlert,
  X,
} from "lucide-react";
import { type GameId, gameById } from "../../data/games";
import { useNotificationStore } from "../../store/notifications";
import { activeInstallFor, modInstallKey, useModsStore } from "../../store/mods";
import { useSettingsStore } from "../../store/settingsStore";
import { openExternal } from "../../lib/tauri";
import { fmtBytes, fmtCompact, fmtRelativeDays } from "../../lib/format";
import { useNow } from "../../lib/hooks";
import { t as translate, type TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import type { ModEntry } from "../../lib/ipc";
import {
  cancelGameBananaInstall,
  feedErrorText,
  gameBananaCategories,
  gameBananaFeed,
  gameBananaModProfile,
  installGameBananaMod,
  installLabel,
  updateGameBananaMod,
  type GameBananaCategory,
  type GameBananaFile,
  type GameBananaMod,
  type GameBananaProfile,
  type GameBananaSort,
} from "../../lib/gamebanana";
import { ActionButton } from "../ui/controls";
import { SearchField } from "../ui/SearchField";
import { Select } from "../ui/Select";
import { Hint, IconButton, WRAPPED_TIP } from "../ui/Tooltip";
import { Lightbox } from "./Lightbox";
import { ModProgressRow } from "./ModProgressRow";

const SORTS: { value: GameBananaSort; label: TKey }[] = [
  { value: "new", label: "mods.browse.sort.new" },
  { value: "updated", label: "mods.browse.sort.updated" },
  { value: "likes", label: "mods.browse.sort.likes" },
  { value: "views", label: "mods.browse.sort.views" },
  { value: "downloads", label: "mods.browse.sort.downloads" },
];

const SEARCH_SORTS: { value: GameBananaSort; label: TKey }[] = [
  { value: "relevance", label: "mods.browse.sort.relevance" },
  ...SORTS,
];

const ALL_CATEGORIES = "0";

const PER_PAGE_OPTIONS = ["16", "24", "32", "50"];

const DEFAULT_PER_PAGE = "16";

const NO_MODS: ModEntry[] = [];

const INSTALL_PERCENT = 90;

interface BrowsePlace {
  input: string;
  query: string;
  sort: GameBananaSort;
  searchSort: GameBananaSort;
  category: string;
  page: number;
}
const browsePlaces = new Map<GameId, BrowsePlace>();
const categoryCache = new Map<GameId, GameBananaCategory[]>();

function revealTop(el: HTMLElement | null) {
  if (!el) return;
  const scroller = el.closest<HTMLElement>(".overflow-y-auto");
  const top = scroller ? scroller.getBoundingClientRect().top : 0;
  if (el.getBoundingClientRect().top < top) el.scrollIntoView({ block: "start" });
}

export function BrowseMods({
  gameId,
  initialMod,
  onInitialModHandled,
}: {
  gameId: GameId;
  initialMod?: { modId: number; name: string; thumbnailUrl: string | null } | null;
  onInitialModHandled?: () => void;
}) {
  const t = useT();
  const push = useNotificationStore((s) => s.push);
  const now = useNow();
  const refreshInstalled = useModsStore((s) => s.refreshIfCurrent);
  const beginInstall = useModsStore((s) => s.beginInstall);
  const endInstall = useModsStore((s) => s.endInstall);
  const activeInstall = useModsStore((s) => activeInstallFor(s.installing, gameId));
  const installStarted = useModsStore(
    (s) => s.progress?.gameId === gameId && s.progress.percentage >= INSTALL_PERCENT,
  );
  const libraryMods = useModsStore((s) => (s.loadedGameId === gameId ? s.mods : NO_MODS));
  const perPage = useSettingsStore((s) => s.values.modsPerPage) ?? DEFAULT_PER_PAGE;
  const setSetting = useSettingsStore((s) => s.set);
  const game = gameById(gameId);

  const [place] = useState(() => browsePlaces.get(gameId));
  const [mods, setMods] = useState<GameBananaMod[]>([]);
  const [page, setPage] = useState(place?.page ?? 1);
  const [totalPages, setTotalPages] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [sort, setSort] = useState<GameBananaSort>(place?.sort ?? "new");
  const [searchSort, setSearchSort] = useState<GameBananaSort>(place?.searchSort ?? "relevance");
  const [category, setCategory] = useState(place?.category ?? ALL_CATEGORIES);
  const [categories, setCategories] = useState<GameBananaCategory[]>(
    () => categoryCache.get(gameId) ?? [],
  );
  const [input, setInput] = useState(place?.input ?? "");
  const [query, setQuery] = useState(place?.query ?? "");
  const [matches, setMatches] = useState({ shown: 0, total: 0, truncated: false });
  const [loading, setLoading] = useState(true);
  const [feedError, setFeedError] = useState<string | null>(null);
  const failed = feedError !== null;
  const feedMessage = feedErrorText(feedError ?? "");

  const [detail, setDetail] = useState<GameBananaProfile | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [heroIndex, setHeroIndex] = useState(0);
  const [lightboxOpen, setLightboxOpen] = useState(false);
  const [installedIds, setInstalledIds] = useState<Set<number>>(new Set());
  const [installError, setInstallError] = useState<{ fileId: number; text: string } | null>(null);
  const [cancelling, setCancelling] = useState(false);

  const rootRef = useRef<HTMLDivElement>(null);
  const feedToken = useRef(0);
  const shownPage = useRef<number | null>(null);
  const detailToken = useRef(0);
  const savedScroll = useRef<{ scroller: HTMLElement; top: number } | null>(null);

  useEffect(() => {
    if (categoryCache.has(gameId)) return;
    let live = true;
    void gameBananaCategories(gameId).then((list) => {
      if (list.length) categoryCache.set(gameId, list);
      if (live) setCategories(list);
    });
    return () => {
      live = false;
    };
  }, [gameId]);

  useEffect(() => {
    browsePlaces.set(gameId, { input, query, sort, searchSort, category, page });
  }, [gameId, input, query, sort, searchSort, category, page]);

  const searching = query !== "";
  const activeSort = searching ? searchSort : sort;

  const load = useCallback(async () => {
    const token = ++feedToken.current;
    setLoading(true);
    setFeedError(null);
    const result = await gameBananaFeed(
      gameId,
      page,
      activeSort,
      query,
      category === ALL_CATEGORIES ? null : Number(category),
      Number(perPage) || Number(DEFAULT_PER_PAGE),
    );
    if (token !== feedToken.current) return;
    setLoading(false);
    if ("error" in result) {
      setFeedError(result.error);
      setMods([]);
      return;
    }
    setMods(result.mods);
    setTotalPages(result.totalPages);
    setHasMore(result.hasMore);
    setMatches({
      shown: result.matches,
      total: result.totalMatches,
      truncated: result.truncated,
    });
    const pageChanged = shownPage.current !== null && shownPage.current !== page;
    shownPage.current = page;
    if (pageChanged) revealTop(rootRef.current);
  }, [gameId, page, activeSort, query, category, perPage]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const t = setTimeout(() => {
      setQuery((q) => {
        if (q === input.trim()) return q;
        setPage(1);
        return input.trim();
      });
    }, 400);
    return () => clearTimeout(t);
  }, [input]);

  const openSeed = async (modId: number, name: string, thumbnailUrl: string | null, extra?: Partial<GameBananaProfile>) => {
    const token = ++detailToken.current;
    setDetailLoading(true);
    setHeroIndex(0);
    setDetail({
      id: modId,
      name,
      profileUrl: null,
      version: null,
      likes: 0,
      views: 0,
      updatedAt: null,
      submitter: null,
      category: null,
      tagline: null,
      description: "",
      images: thumbnailUrl ? [{ url: thumbnailUrl, thumbUrl: thumbnailUrl }] : [],
      files: [],
      ...extra,
    });
    revealTop(rootRef.current);
    const full = await gameBananaModProfile(modId);
    if (token !== detailToken.current) return;
    setDetailLoading(false);
    if (full) setDetail(full);
    else setDetail((d) => (d ? { ...d, filesError: translate("mods.browse.loadFailed") } : d));
  };

  const openMod = (mod: GameBananaMod) => {
    const scroller = rootRef.current?.closest<HTMLElement>(".overflow-y-auto") ?? null;
    savedScroll.current = scroller ? { scroller, top: scroller.scrollTop } : null;
    return openSeed(mod.id, mod.name, mod.thumbnailUrl, {
      profileUrl: mod.profileUrl,
      version: mod.version,
      likes: mod.likes,
      views: mod.views,
      updatedAt: mod.updatedAt,
      submitter: mod.submitter,
      category: mod.category,
    });
  };

  const closeDetail = () => {
    detailToken.current++;
    setDetailLoading(false);
    setDetail(null);
  };

  useLayoutEffect(() => {
    if (detail) return;
    const saved = savedScroll.current;
    if (!saved) return;
    savedScroll.current = null;
    saved.scroller.scrollTop = saved.top;
  }, [detail]);

  useEffect(() => {
    if (!initialMod) return;
    onInitialModHandled?.();
    savedScroll.current = null;
    void openSeed(initialMod.modId, initialMod.name, initialMod.thumbnailUrl);
  }, [initialMod, onInitialModHandled]);

  const install = async (file: GameBananaFile) => {
    if (!detail) return;
    const key = modInstallKey(gameId, detail.id, file.id);
    if (activeInstall !== null || !beginInstall(key)) return;
    setInstallError(null);
    setCancelling(false);
    const label = installLabel(detail, file);
    let result: Awaited<ReturnType<typeof installGameBananaMod>>;
    try {
      result = await installGameBananaMod(
        gameId,
        detail.id,
        file.id,
        label,
        detail.images[0]?.thumbUrl ?? null,
      );
    } finally {
      endInstall(key);
      const mods = useModsStore.getState();
      if (mods.progress?.gameId === gameId) mods.setProgress(null);
    }
    if ("error" in result) {
      if (result.error) setInstallError({ fileId: file.id, text: result.error });
      return;
    }
    setInstalledIds((prev) => new Set(prev).add(file.id));
    push(
      result.alreadyInstalled
        ? {
            title: t("mods.browse.alreadyInstalledTitle"),
            text: t("mods.browse.alreadyInstalledText", { name: label, game: game.name }),
          }
        : {
            type: "success",
            title: t("mods.toast.installedTitle"),
            text: t("mods.toast.installedText", { name: label, game: game.name }),
          },
    );
  };

  const cancelInstall = async () => {
    setCancelling(true);
    await cancelGameBananaInstall(gameId);
  };

  const retryFiles = async () => {
    if (!detail) return;
    const token = ++detailToken.current;
    setDetailLoading(true);
    setDetail({ ...detail, filesError: null });
    const full = await gameBananaModProfile(detail.id);
    if (token !== detailToken.current) return;
    setDetailLoading(false);
    if (full) setDetail(full);
    else setDetail((d) => (d ? { ...d, filesError: t("mods.browse.noAnswer") } : d));
  };

  const installedCopies = libraryMods.filter(
    (m) => m.source.kind === "gamebanana" && m.source.gbModId === detail?.id,
  );
  const installedFileIds = new Set(installedCopies.map((m) => m.source.gbFileId));
  const otherFileInstalled =
    detail !== null &&
    [...installedFileIds].some((id) => id !== undefined && !detail.files.some((f) => f.id === id));

  const olderCopyOf = (file: GameBananaFile): ModEntry | undefined => {
    if (!detail || !file.family || file.dateAdded == null) return undefined;
    const added = file.dateAdded;
    return installedCopies.find((m) => {
      const old = detail.files.find((f) => f.id === m.source.gbFileId);
      return (
        old !== undefined &&
        old.id !== file.id &&
        old.family === file.family &&
        old.dateAdded != null &&
        old.dateAdded < added
      );
    });
  };

  const updateTo = async (file: GameBananaFile, copy: ModEntry) => {
    if (!detail) return;
    const key = modInstallKey(gameId, detail.id, file.id);
    if (activeInstall !== null || !beginInstall(key)) return;
    setInstallError(null);
    setCancelling(false);
    let folder: string | undefined;
    try {
      folder = await updateGameBananaMod(gameId, copy.modId, file.id);
    } finally {
      endInstall(key);
      const mods = useModsStore.getState();
      if (mods.progress?.gameId === gameId) mods.setProgress(null);
    }
    if (folder) {
      setInstalledIds((prev) => {
        const next = new Set(prev);
        if (copy.source.gbFileId !== undefined) next.delete(copy.source.gbFileId);
        return next.add(file.id);
      });
      push({
        type: "success",
        title: t("mods.toast.updatedTitle"),
        text: t("mods.browse.updatedText", { name: detail.name, file: file.fileName }),
      });
    } else {
      void refreshInstalled(gameId);
    }
  };

  const categoryOptions = [
    { value: ALL_CATEGORIES, label: t("mods.browse.allCategories") },
    ...categories.map((c) => ({ value: String(c.id), label: c.name })),
  ];

  if (detail) {
    const hero = detail.images[heroIndex] ?? detail.images[0];
    const statsKnown =
      detail.likes > 0 || detail.views > 0 || (!detailLoading && !detail.filesError);
    return (
      <div ref={rootRef} className="mx-auto max-w-[980px] scroll-mt-10">
        <div className="mb-4 flex items-start gap-3">
          <button
            type="button"
            onClick={closeDetail}
            aria-label={t("mods.browse.backToList")}
            className="mt-0.5 rounded-[8px] p-1.5 text-white/60 transition hover:bg-white/[0.08] hover:text-white"
          >
            <ArrowLeft size={18} />
          </button>
          <div className="min-w-0 flex-1">
            <Hint tip={detail.name} className="block">
              <h2 className="truncate text-[18px] font-semibold">{detail.name}</h2>
            </Hint>
            <p className="mt-0.5 truncate text-[12.5px] text-white/50">
              {[
                detail.submitter,
                detail.category,
                detail.version && `v${detail.version}`,
                fmtRelativeDays(detail.updatedAt, now),
              ]
                .filter(Boolean)
                .join(" · ")}
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-3 text-[12px] text-white/45">
            {statsKnown && (
              <>
                <span className="flex items-center gap-1">
                  <Heart size={12} /> {fmtCompact(detail.likes)}
                </span>
                <span className="flex items-center gap-1">
                  <Eye size={12} /> {fmtCompact(detail.views)}
                </span>
              </>
            )}
            {detail.profileUrl && (
              <IconButton
                type="button"
                label={t("mods.browse.openOnGameBanana")}
                onClick={() => void openExternal(detail.profileUrl!)}
                className="rounded-[8px] p-1.5 text-white/50 transition hover:bg-white/[0.08] hover:text-white"
              >
                <ExternalLink size={16} />
              </IconButton>
            )}
          </div>
        </div>

        {hero && (
          <div className="mb-4">
            <button
              type="button"
              onClick={() => setLightboxOpen(true)}
              aria-label={t("mods.browse.viewFullSize")}
              className="relative block w-full cursor-zoom-in overflow-hidden rounded-ui"
              style={{ background: "rgba(0,0,0,0.35)" }}
            >
              <HeroImage key={hero.url} src={hero.url} previewSrc={hero.heroUrl} />
              {detail.images.length > 1 && (
                <span className="absolute bottom-3 right-3 rounded-full bg-black/60 px-2.5 py-1 text-[11.5px] text-white/70">
                  {t("mods.imageCounter", { index: heroIndex + 1, total: detail.images.length })}
                </span>
              )}
            </button>
            {detail.images.length > 1 && (
              <div className="mt-2 flex gap-2 overflow-x-auto pb-1">
                {detail.images.map((img, i) => (
                  <button
                    key={img.thumbUrl + i}
                    type="button"
                    onClick={() => setHeroIndex(i)}
                    aria-pressed={i === heroIndex}
                    aria-label={t("mods.browse.showImage", { index: i + 1, total: detail.images.length })}
                    className={`h-[56px] w-[86px] shrink-0 overflow-hidden rounded-[8px] border transition ${
                      i === heroIndex
                        ? "border-(--accent-b)/70"
                        : "border-white/[0.08] opacity-60 hover:opacity-100"
                    }`}
                  >
                    <img src={img.thumbUrl} alt="" loading="lazy" className="h-full w-full object-cover" />
                  </button>
                ))}
              </div>
            )}
          </div>
        )}

        {detail.tagline && (
          <p className="mb-3 text-[13.5px] font-medium text-white/80">{detail.tagline}</p>
        )}
        {detail.description && (
          <p className="mb-5 whitespace-pre-line text-[13px] leading-[1.6] text-white/60">
            {detail.description}
          </p>
        )}

        <h3 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.07em] text-white/40">
          {t("common.downloads")}
        </h3>
        <ModProgressRow gameId={gameId} />
        {detailLoading && detail.files.length === 0 ? (
          <div className="flex flex-col gap-2">
            {[0, 1].map((i) => (
              <div key={i} className="h-[62px] animate-pulse rounded-ui bg-white/[0.04]" />
            ))}
          </div>
        ) : detail.files.length === 0 && detail.filesError ? (
          <div className="py-6 text-center">
            <p className="text-[13px] text-white/60">{t("mods.browse.filesError")}</p>
            <p className="mt-1 text-[12px] text-white/40">{detail.filesError}</p>
            <div className="mt-3 flex justify-center">
              <ActionButton onClick={() => void retryFiles()}>{t("common.retry")}</ActionButton>
            </div>
          </div>
        ) : detail.files.length === 0 ? (
          <p className="py-6 text-center text-[13px] text-white/45">
            {t("mods.browse.noFiles")}
          </p>
        ) : (
          <div className="flex flex-col gap-2">
            {otherFileInstalled && (
              <p className="text-[12px] text-white/55">
                {t("mods.browse.olderInstalled")}
              </p>
            )}
            {detail.files.map((file) => {
              const olderCopy = olderCopyOf(file);
              return (
                <div
                  key={file.id}
                  className="flex items-center justify-between gap-4 rounded-ui border border-white/[0.08] bg-white/[0.04] px-4 py-3"
                >
                  <div className="min-w-0">
                    <Hint tip={file.fileName} className="block">
                      <p className="truncate text-[13.5px] font-medium">{file.fileName}</p>
                    </Hint>
                    {(file.sizeBytes > 0 || file.version) && (
                      <p className="mt-0.5 truncate text-[12px] text-white/45">
                        {[
                          file.sizeBytes > 0 && fmtBytes(file.sizeBytes),
                          file.version && `v${file.version}`,
                        ]
                          .filter(Boolean)
                          .join(" · ")}
                      </p>
                    )}
                    {file.description && (
                      <Hint tip={<span className={WRAPPED_TIP}>{file.description}</span>} className="block">
                        <p className="mt-0.5 line-clamp-2 text-[12px] text-white/55">{file.description}</p>
                      </Hint>
                    )}
                    {!file.installable && (
                      <p className="mt-1.5 flex items-center gap-1.5 text-[12px] text-amber-300/90">
                        <ShieldAlert size={13} className="shrink-0" />
                        {t("mods.browse.virusFlagged", { result: file.avResult })}
                      </p>
                    )}
                    {olderCopy && file.installable && (
                      <p className="mt-1.5 text-[12px] text-white/55">
                        {t("mods.browse.olderVersion")}
                      </p>
                    )}
                    {installError?.fileId === file.id && (
                      <p className="mt-1.5 flex items-start gap-1.5 text-[12px] text-red-300/90">
                        <ShieldAlert size={13} className="mt-[2px] shrink-0" />
                        {installError.text}
                      </p>
                    )}
                  </div>
                  {installedIds.has(file.id) || installedFileIds.has(file.id) ? (
                    <span className="flex shrink-0 items-center gap-1.5 px-2 text-[12.5px] text-emerald-300/80">
                      <Check size={14} /> {t("common.installed")}
                    </span>
                  ) : activeInstall === modInstallKey(gameId, detail.id, file.id) ? (
                    installStarted ? (
                      <span className="shrink-0 px-2 text-[12.5px] text-white/55">{t("common.installing")}</span>
                    ) : (
                      <ActionButton
                        disabled={cancelling}
                        onClick={() => void cancelInstall()}
                        icon={<X size={15} />}
                      >
                        {cancelling ? t("common.cancelling") : t("common.cancel")}
                      </ActionButton>
                    )
                  ) : (
                    <ActionButton
                      variant="accent"
                      disabled={!file.installable || activeInstall !== null}
                      onClick={() => void (olderCopy ? updateTo(file, olderCopy) : install(file))}
                      icon={<Download size={15} />}
                    >
                      {olderCopy ? t("common.update") : t("common.install")}
                    </ActionButton>
                  )}
                </div>
              );
            })}
          </div>
        )}

        <AnimatePresence>
          {lightboxOpen && (
            <Lightbox
              title={detail.name}
              images={detail.images}
              index={heroIndex}
              onClose={() => setLightboxOpen(false)}
              onIndexChange={setHeroIndex}
            />
          )}
        </AnimatePresence>
      </div>
    );
  }

  return (
    <div ref={rootRef} className="scroll-mt-10">
      <div className="mb-4 flex items-center gap-2">
        <SearchField
          value={input}
          onChange={setInput}
          placeholder={t("mods.browse.searchPlaceholder", { game: game.name })}
          ariaLabel={t("mods.browse.searchLabel")}
        />
        <Select
          ariaLabel={t("mods.browse.category")}
          options={categoryOptions}
          value={category}
          onChange={(v) => {
            setCategory(v);
            setPage(1);
          }}
        />
        <Select
          ariaLabel={t("common.sort")}
          options={(searching ? SEARCH_SORTS : SORTS).map((o) => ({ value: o.value, label: t(o.label) }))}
          value={activeSort}
          onChange={(v) => {
            if (searching) setSearchSort(v as GameBananaSort);
            else setSort(v as GameBananaSort);
            setPage(1);
          }}
        />
        <Select
          ariaLabel={t("mods.browse.perPageLabel")}
          options={PER_PAGE_OPTIONS.map((n) => ({ value: n, label: t("mods.browse.perPage", { n }) }))}
          value={perPage}
          onChange={(v) => {
            setSetting("modsPerPage", v);
            setPage(1);
          }}
        />
      </div>

      <p className="mb-4 text-[12px] text-white/55">
        {!searching
          ? t("mods.browse.disclaimer")
          : matches.truncated
            ? t("mods.browse.truncated", { shown: matches.shown, total: matches.total, query })
            : t("mods.browse.allMatches", { query })}
      </p>

      <ModProgressRow gameId={gameId} />

      {failed ? (
        <div className="py-16 text-center">
          <p className="text-[13.5px] text-white/70">{feedMessage.title}</p>
          <p className="mt-1 text-[12.5px] text-white/45">{feedMessage.text}</p>
          <div className="mt-4 flex justify-center">
            <ActionButton onClick={() => void load()}>{t("common.retry")}</ActionButton>
          </div>
        </div>
      ) : loading && mods.length === 0 ? (
        <div
          className="grid gap-3"
          style={{ gridTemplateColumns: "repeat(auto-fill, minmax(240px, 1fr))" }}
        >
          {Array.from({ length: Number(perPage) || Number(DEFAULT_PER_PAGE) }, (_, i) => (
            <div key={i} className="overflow-hidden rounded-ui border border-white/[0.06] bg-white/[0.03]">
              <div className="aspect-[16/10] w-full animate-pulse bg-white/[0.05]" />
              <div className="p-3">
                <div className="h-[13px] w-3/4 animate-pulse rounded-[4px] bg-white/[0.06]" />
                <div className="mt-2 h-[11px] w-1/2 animate-pulse rounded-[4px] bg-white/[0.05]" />
              </div>
            </div>
          ))}
        </div>
      ) : mods.length === 0 ? (
        <p className="py-16 text-center text-[13px] text-white/45">
          {query ? t("mods.browse.nothingFound", { query }) : t("mods.browse.noMods")}
        </p>
      ) : (
        <div
          className={`grid gap-3 transition-opacity duration-150 ${
            loading ? "pointer-events-none opacity-50" : ""
          }`}
          style={{ gridTemplateColumns: "repeat(auto-fill, minmax(240px, 1fr))" }}
        >
          {mods.map((mod) => (
            <button
              key={mod.id}
              type="button"
              onClick={() => void openMod(mod)}
              className="group flex flex-col overflow-hidden rounded-ui border border-white/[0.08] bg-white/[0.04] text-left transition duration-150 hover:border-white/20 hover:bg-white/[0.07] active:scale-[0.99]"
            >
              <div className="relative aspect-[16/10] w-full overflow-hidden bg-black/30">
                <CardThumb src={mod.thumbnailUrl} />
              </div>
              <div className="min-w-0 p-3">
                <Hint tip={mod.name} className="block">
                  <p className="truncate text-[13px] font-medium">{mod.name}</p>
                </Hint>
                <p className="mt-0.5 truncate text-[11.5px] text-white/55">
                  {mod.submitter ?? t("mods.browse.unknownAuthor")}
                </p>
                <div className="mt-2 flex items-center gap-3 text-[11px] text-white/55">
                  <span className="flex items-center gap-1">
                    <Heart size={11} /> {fmtCompact(mod.likes)}
                  </span>
                  <span className="flex items-center gap-1">
                    <Eye size={11} /> {fmtCompact(mod.views)}
                  </span>
                  {mod.category && <span className="truncate">{mod.category}</span>}
                </div>
              </div>
            </button>
          ))}
        </div>
      )}

      {!failed && (mods.length > 0 || page > 1) && (
        <div className="mt-5 flex items-center justify-between">
          <span className="text-[12.5px] text-white/45">
            {totalPages > 1
              ? t("mods.browse.pageOf", { page, total: totalPages })
              : t("mods.browse.page", { page })}
          </span>
          <div className="flex gap-2">
            <ActionButton
              onClick={() => setPage((p) => Math.max(1, p - 1))}
              disabled={page <= 1 || loading}
              icon={<ChevronLeft size={15} />}
            >
              {t("common.previous")}
            </ActionButton>
            <ActionButton
              onClick={() => setPage((p) => p + 1)}
              disabled={loading || !hasMore}
              icon={<ChevronRight size={15} />}
            >
              {t("common.next")}
            </ActionButton>
          </div>
        </div>
      )}
    </div>
  );
}

function CardThumb({ src }: { src: string | null }) {
  const t = useT();
  const [loaded, setLoaded] = useState(false);
  const [imgFailed, setImgFailed] = useState(false);

  if (!src || imgFailed) {
    return (
      <div className="flex h-full items-center justify-center text-[12px] text-white/25">
        {t("mods.browse.noPreview")}
      </div>
    );
  }
  return (
    <>
      {!loaded && <div className="absolute inset-0 animate-pulse bg-white/[0.05]" />}
      <img
        src={src}
        alt=""
        loading="lazy"
        onLoad={() => setLoaded(true)}
        onError={() => setImgFailed(true)}
        className={`h-full w-full object-cover transition duration-200 group-hover:scale-[1.03] ${
          loaded ? "opacity-100" : "opacity-0"
        }`}
      />
    </>
  );
}

function HeroImage({ src, previewSrc }: { src: string; previewSrc?: string }) {
  const t = useT();
  const preview = previewSrc && previewSrc !== src ? previewSrc : null;
  const [loaded, setLoaded] = useState(false);
  const [imgFailed, setImgFailed] = useState(false);
  const [previewLoaded, setPreviewLoaded] = useState(false);
  const [previewFailed, setPreviewFailed] = useState(false);
  const showPreview = preview !== null && !loaded && !previewFailed;

  if (imgFailed && !showPreview) {
    return (
      <div className="flex min-h-[200px] items-center justify-center text-[12.5px] text-white/35">
        {t("mods.previewUnavailable")}
      </div>
    );
  }
  return (
    <div className="relative min-h-[200px]">
      {!loaded && !previewLoaded && (
        <div className="absolute inset-0 animate-pulse bg-white/[0.04]" />
      )}
      {showPreview && (
        <img
          src={preview}
          alt=""
          onLoad={() => setPreviewLoaded(true)}
          onError={() => setPreviewFailed(true)}
          className={`max-h-[420px] w-full object-contain ${
            previewLoaded ? "opacity-100" : "opacity-0"
          }`}
        />
      )}
      {!imgFailed && (
        <img
          src={src}
          alt=""
          onLoad={() => setLoaded(true)}
          onError={() => setImgFailed(true)}
          className={`max-h-[420px] w-full object-contain ${
            showPreview ? "absolute inset-0 h-full" : ""
          } ${previewLoaded ? "" : "transition-opacity duration-150"} ${
            loaded ? "opacity-100" : "opacity-0"
          }`}
        />
      )}
    </div>
  );
}
