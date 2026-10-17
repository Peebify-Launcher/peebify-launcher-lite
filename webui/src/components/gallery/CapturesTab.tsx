// ------------ Gallery ------------
// The Gallery page with its capture tiles and the bar for bulk actions on selected captures.
import { useCallback, useEffect, useMemo, useRef, useState, memo } from "react";
import { AnimatePresence, m } from "framer-motion";
import {
  CheckSquare,
  FolderOpen,
  Images,
  RefreshCw,
  Settings,
  ExternalLink,
  X,
  Check,
  Film,
  Image as ImageIcon,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { useMediaStore, type KindFilter } from "../../store/mediaStore";
import { useSettingsStore } from "../../store/settingsStore";
import { useSessionUiStore } from "../../store/sessionUiStore";
import { useUiStore } from "../../store/uiStore";
import {
  onCapturesChanged,
  openCaptureFolder,
  type MediaItem,
  openCapture,
  revealCapture,
} from "../../lib/ipc";
import { reads, useRemembered } from "../../lib/launcherHooks";
import { Hint } from "../ui/Tooltip";
import { useEscapeKey, useTauriEvent } from "../../lib/hooks";
import { fadeVariants } from "../../lib/motion";
import { GAMES, gameMeta } from "../../data/games";
import { ActionButton, Chip } from "../ui/controls";
import { GlassPage } from "../ui/GlassPage";
import { Select } from "../ui/Select";
import { MediaViewer, DeleteButton } from "./MediaViewer";
import { fmtDayHeading, groupByDay } from "../../lib/playtime";
import type { TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { LazyClip, tileMedia } from "./media";
import { iconFallback, iconSrcFor } from "../../lib/customMedia";
import { useCustomizationStore } from "../../store/customizationStore";

// ------------ Gallery Page ------------
// The Gallery tab. Lists the screenshots and clips the overlay saved, grouped by day, with filters for game and
// type, a select mode for bulk actions, and a viewer to open any capture.
const PAGE = 96;

const KIND_CHIPS: { value: KindFilter; label: TKey }[] = [
  { value: "all", label: "gallery.kinds.all" },
  { value: "screenshot", label: "common.screenshots" },
  { value: "clip", label: "gallery.kinds.clips" },
];

const noop = () => {};
const takenAt = (item: MediaItem) => item.takenAt;

const KEY_IGNORE =
  "input, textarea, select, [contenteditable=true], [role=menu], [role=listbox], [aria-expanded=true]";

function CaptureHint() {
  const overlayOn = useSettingsStore((s) => s.values.overlayEnabled === "true");
  const setView = useUiStore((s) => s.setView);
  const setSettingsCat = useSessionUiStore((s) => s.setSettingsCat);
  const t = useT();
  const config = useRemembered(reads.overlayConfig, []);
  const shotKey = config?.hotkeys.find((h) => h.id === "overlayShotHotkey")?.accelerator || "Alt+S";

  return (
    <>
      <p className="max-w-[52ch] text-[12.5px] text-white/45">
        {overlayOn
          ? t("gallery.hintOverlayOn", { key: shotKey })
          : t("gallery.hintOverlayOff", { key: shotKey })}
      </p>
      <ActionButton
        icon={<Settings size={14} />}
        onClick={() => {
          setSettingsCat("overlay");
          setView("settings");
        }}
      >
        {t("gallery.openOverlaySettings")}
      </ActionButton>
    </>
  );
}

function OpenFolderButton() {
  const folder = useMediaStore((s) => s.folder);
  const t = useT();
  return (
    <Hint tip={folder || undefined}>
      <ActionButton icon={<FolderOpen size={14} />} onClick={() => void openCaptureFolder()}>
        {t("gallery.openFolder")}
      </ActionButton>
    </Hint>
  );
}

function CapturesTab() {
  const { items, hydrated, loading, error, kind, gameId, selectMode, selected } = useMediaStore(
    useShallow((s) => ({
      items: s.items,
      hydrated: s.hydrated,
      loading: s.loading,
      error: s.error,
      kind: s.kind,
      gameId: s.gameId,
      selectMode: s.selectMode,
      selected: s.selected,
    })),
  );
  const load = useMediaStore((s) => s.load);
  const refresh = useMediaStore((s) => s.refresh);
  const setKind = useMediaStore((s) => s.setKind);
  const setGame = useMediaStore((s) => s.setGame);
  const setSelectMode = useMediaStore((s) => s.setSelectMode);
  const toggleSelected = useMediaStore((s) => s.toggleSelected);
  const selectMany = useMediaStore((s) => s.selectMany);
  const clearSelection = useMediaStore((s) => s.clearSelection);
  const t = useT();
  const gameOptions = useMemo(
    () => [
      { value: "all", label: t("gallery.allGames") },
      ...GAMES.map((g) => ({ value: g.id, label: g.name })),
    ],
    [t],
  );

  const [viewing, setViewing] = useState<string | null>(null);
  const viewedIndex = useRef(0);
  const [limit, setLimit] = useState(PAGE);
  const anchor = useRef<string | null>(null);
  const sentinel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void load();
  }, [load]);
  const missedChange = useRef(false);
  useTauriEvent(onCapturesChanged, () => {
    if (document.visibilityState === "hidden") missedChange.current = true;
    else refresh();
  });
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === "hidden" || !missedChange.current) return;
      missedChange.current = false;
      refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, [refresh]);

  useEffect(() => setLimit(PAGE), [kind, gameId]);

  const visible = useMemo(
    () =>
      items.filter(
        (i) => (kind === "all" || i.kind === kind) && (gameId === "all" || i.gameId === gameId),
      ),
    [items, kind, gameId],
  );
  const shown = useMemo(() => visible.slice(0, limit), [visible, limit]);
  const groups = useMemo(() => groupByDay(shown, takenAt), [shown]);
  const dayPaths = useMemo(() => {
    const byDay = new Map<string, string[]>();
    for (const group of groupByDay(visible, takenAt)) {
      const paths = group.items.map((i) => i.path);
      const known = byDay.get(group.key);
      if (known) known.push(...paths);
      else byDay.set(group.key, paths);
    }
    return byDay;
  }, [visible]);
  const selectedItems = useMemo(() => visible.filter((i) => selected[i.path]), [visible, selected]);
  const hasMore = visible.length > shown.length;

  useEffect(() => {
    if (!viewing) return;
    const at = visible.findIndex((i) => i.path === viewing);
    if (at >= 0) {
      viewedIndex.current = at;
      return;
    }
    const fallback = visible[Math.min(viewedIndex.current, visible.length - 1)];
    setViewing(fallback ? fallback.path : null);
  }, [viewing, visible]);

  useEffect(() => {
    const el = sentinel.current;
    if (!el || !hasMore) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) setLimit((l) => l + PAGE);
      },
      { root: el.closest(".overflow-y-auto"), rootMargin: "500px" },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [hasMore, groups.length]);

  const keysActive = selectMode && !viewing;
  const isTopLayer = useEscapeKey(noop, keysActive);

  useEffect(() => {
    if (!keysActive) return;
    const onKey = (e: KeyboardEvent) => {
      if (!isTopLayer()) return;
      const target = e.target instanceof Element ? e.target : null;
      if (target?.closest(KEY_IGNORE)) return;
      if (e.key === "Escape") {
        setSelectMode(false);
        return;
      }
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "a") {
        e.preventDefault();
        selectMany(visible.map((i) => i.path));
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [keysActive, isTopLayer, setSelectMode, selectMany, visible]);

  const onToggle = useCallback(
    (path: string, range: boolean) => {
      const from = anchor.current;
      anchor.current = path;
      if (range && from && from !== path) {
        const a = visible.findIndex((i) => i.path === from);
        const b = visible.findIndex((i) => i.path === path);
        if (a >= 0 && b >= 0) {
          window.getSelection()?.removeAllRanges();
          selectMany(visible.slice(Math.min(a, b), Math.max(a, b) + 1).map((i) => i.path));
          return;
        }
      }
      toggleSelected(path);
    },
    [visible, selectMany, toggleSelected],
  );
  const nothingCaptured = kind === "all" && gameId === "all";

  return (
    <>
      <div className="mb-5 flex flex-wrap items-center gap-3">
        <div className="flex flex-wrap gap-[6px]">
          {KIND_CHIPS.map((c) => (
            <Chip key={c.value} active={kind === c.value} onClick={() => setKind(c.value)}>
              {t(c.label)}
            </Chip>
          ))}
        </div>
        <Select
          ariaLabel={t("gallery.gameFilter")}
          value={gameId}
          onChange={setGame}
          options={gameOptions}
        />
        <span className="ml-auto text-[12px] tabular-nums text-white/40">
          {hydrated ? t("gallery.itemCount", { count: visible.length }) : ""}
        </span>
        <OpenFolderButton />
        <ActionButton
          icon={<CheckSquare size={14} />}
          variant={selectMode ? "accent" : "neutral"}
          onClick={() => setSelectMode(!selectMode)}
        >
          {selectMode ? t("common.done") : t("gallery.select")}
        </ActionButton>
      </div>

      {error && (
        <div className="mb-4 flex items-center gap-3 rounded-ui border border-danger/30 bg-danger/[0.08] px-4 py-3 text-[12.5px] text-danger-soft">
          <span className="min-w-0 flex-1">{error}</span>
          <ActionButton
            icon={<RefreshCw size={14} className={loading ? "animate-spin" : undefined} />}
            disabled={loading}
            onClick={() => void load()}
          >
            {t("common.retry")}
          </ActionButton>
        </div>
      )}

      {!hydrated ? (
        <div
          role="status"
          aria-label={t("gallery.loadingCaptures")}
          className="grid gap-3 [grid-template-columns:repeat(auto-fill,minmax(200px,1fr))]"
        >
          {Array.from({ length: 8 }, (_, i) => (
            <div key={i} className="aspect-video animate-pulse rounded-ui bg-white/[0.05]" />
          ))}
        </div>
      ) : error && visible.length === 0 ? null : visible.length === 0 ? (
        <div className="flex flex-col items-center justify-center gap-3 rounded-ui border border-dashed border-white/15 bg-white/[0.04] px-6 py-12 text-center">
          <Images size={22} className="opacity-60" />
          <p className="max-w-[52ch] text-[13.5px] text-white/60">
            {nothingCaptured ? t("gallery.nothingCaptured") : t("gallery.noMatches")}
          </p>
          {nothingCaptured && <CaptureHint />}
        </div>
      ) : (
        <AnimatePresence initial={false}>
          <m.div
            key={`${kind}-${gameId}`}
            variants={fadeVariants}
            initial="initial"
            animate="animate"
            className="flex flex-col gap-7 pb-24"
          >
            {groups.map((group) => (
              <section key={group.key}>
                <div className="sticky top-0 z-[2] -mx-2 mb-3 flex items-baseline gap-2 rounded-[8px] bg-[rgba(12,12,16,0.92)] px-2 py-2">
                  <h2 className="text-[14px] font-semibold text-white/90">{fmtDayHeading(group.date)}</h2>
                  <span className="text-[11.5px] tabular-nums text-white/35">
                    {dayPaths.get(group.key)?.length ?? group.items.length}
                  </span>
                  {selectMode && (
                    <button
                      type="button"
                      onClick={() =>
                        selectMany(dayPaths.get(group.key) ?? group.items.map((i) => i.path))
                      }
                      className="ml-auto text-[11.5px] font-medium text-white/50 hover:text-white"
                    >
                      {t("gallery.selectDay")}
                    </button>
                  )}
                </div>
                <div className="grid gap-3 [grid-template-columns:repeat(auto-fill,minmax(200px,1fr))]">
                  {group.items.map((item) => (
                    <MediaTile
                      key={item.path}
                      item={item}
                      selectMode={selectMode}
                      selected={!!selected[item.path]}
                      onOpen={setViewing}
                      onToggle={onToggle}
                    />
                  ))}
                </div>
              </section>
            ))}
            {hasMore && (
              <div ref={sentinel} className="flex items-center justify-center py-4">
                <ActionButton onClick={() => setLimit((l) => l + PAGE)}>{t("gallery.loadMore")}</ActionButton>
              </div>
            )}
          </m.div>
        </AnimatePresence>
      )}

      <AnimatePresence>
        {selectMode && selectedItems.length > 0 && (
          <SelectionBar
            key="selection"
            items={selectedItems}
            visibleCount={visible.length}
            onSelectAll={() => selectMany(visible.map((i) => i.path))}
            onClear={clearSelection}
          />
        )}
      </AnimatePresence>

      {viewing && (
        <MediaViewer
          items={visible}
          path={viewing}
          onPath={setViewing}
          onClose={() => setViewing(null)}
        />
      )}
    </>
  );
}

export function GalleryPage() {
  const t = useT();
  return (
    <GlassPage title={t("common.gallery")} subtitle={t("gallery.subtitle")}>
      <CapturesTab />
    </GlassPage>
  );
}

// ------------ Selection Bar ------------
// The floating bar at the bottom of the gallery while captures are selected. Shows how many are selected and
// offers select all, open, show in folder, delete and clear.
function SelectionBar({
  items,
  visibleCount,
  onSelectAll,
  onClear,
}: {
  items: MediaItem[];
  visibleCount: number;
  onSelectAll: () => void;
  onClear: () => void;
}) {
  const t = useT();
  const single = items.length === 1 ? items[0] : null;

  return (
    <m.div
      initial={{ opacity: 0, y: 16 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: 16 }}
      transition={{ duration: 0.16 }}
      className="pointer-events-none fixed bottom-6 left-[60px] right-0 z-(--z-banner) flex justify-center px-6"
    >
      <div className="glass pointer-events-auto flex max-w-full flex-wrap items-center gap-2 rounded-ui px-3 py-2 shadow-2xl">
        <span className="px-2 text-[13px] font-medium tabular-nums text-white/85">
          {t("gallery.selection.selected", { count: items.length })}
        </span>
        {items.length < visibleCount && (
          <ActionButton icon={<CheckSquare size={14} />} onClick={onSelectAll}>
            {t("gallery.selection.selectAll", { count: visibleCount })}
          </ActionButton>
        )}
        {single && (
          <>
            <ActionButton
              icon={<ExternalLink size={14} />}
              onClick={() => void openCapture(single.path)}
            >
              {t("gallery.open")}
            </ActionButton>
            <ActionButton
              icon={<FolderOpen size={14} />}
              onClick={() => void revealCapture(single.path)}
            >
              {t("gallery.showInFolder")}
            </ActionButton>
          </>
        )}
        <DeleteButton items={items} onDeleted={onClear} />
        <button
          type="button"
          aria-label={t("gallery.selection.clear")}
          onClick={onClear}
          className="grid h-[34px] w-[34px] place-items-center rounded-ui text-white/60 transition-colors hover:bg-white/[0.08] hover:text-white"
        >
          <X size={16} />
        </button>
      </div>
    </m.div>
  );
}

// ------------ Capture Tile ------------
// One screenshot or clip in the gallery grid. Click to open it, or to tick it while in select mode (shift-click
// selects a range). Clips only load their preview when they scroll into view.
const MediaTile = memo(function MediaTile({
  item,
  selectMode,
  selected,
  onOpen,
  onToggle,
}: {
  item: MediaItem;
  selectMode: boolean;
  selected: boolean;
  onOpen: (path: string) => void;
  onToggle: (path: string, range: boolean) => void;
}) {
  const media = tileMedia(item);
  const game = gameMeta(item.gameId);
  const icons = useCustomizationStore((s) => s.gameIcons);
  const t = useT();

  return (
    <div
      className={`group relative overflow-hidden rounded-ui border bg-black/40 transition-[border-color,box-shadow] duration-150 has-[>button:focus-visible]:outline-2 has-[>button:focus-visible]:outline-offset-2 has-[>button:focus-visible]:outline-(--accent-a) ${
        selected
          ? "border-(--accent-b)/70 shadow-[0_0_0_1px_rgba(var(--accent-b-rgb),0.5)]"
          : "border-white/[0.08] hover:border-white/20"
      }`}
    >
      <button
        type="button"
        onClick={(e) => (selectMode ? onToggle(item.path, e.shiftKey) : onOpen(item.path))}
        aria-label={selectMode ? t("gallery.tile.select", { name: item.name }) : t("gallery.tile.open", { name: item.name })}
        aria-pressed={selectMode ? selected : undefined}
        className={`relative block aspect-video w-full ${selectMode ? "cursor-pointer" : "cursor-zoom-in"}`}
      >
        {media.src ? (
          media.video ? (
            <LazyClip src={media.src} />
          ) : (
            <img
              src={media.src}
              alt={item.name}
              loading="lazy"
              decoding="async"
              draggable={false}
              className="h-full w-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
            />
          )
        ) : (
          <span className="grid h-full w-full place-items-center text-white/25">
            {item.kind === "clip" ? <Film size={26} /> : <ImageIcon size={26} />}
          </span>
        )}
        <span className="pointer-events-none absolute inset-x-0 bottom-0 h-[46px] bg-gradient-to-t from-black/70 to-transparent" />
        {selectMode && selected && <span className="absolute inset-0 bg-(--accent-b)/15" />}
      </button>

      <div className="pointer-events-none absolute left-[7px] top-[7px] flex items-center gap-[5px]">
        {selectMode ? (
          <span
            className={`grid h-[20px] w-[20px] place-items-center rounded-[6px] border ${
              selected
                ? "accent-grad border-transparent text-(--on-accent)"
                : "border-white/50 bg-black/45"
            }`}
          >
            {selected && <Check size={13} strokeWidth={3} />}
          </span>
        ) : (
          item.kind === "clip" && (
            <span className="rounded-[5px] bg-black/65 px-[6px] py-[2px] text-[9.5px] font-semibold uppercase tracking-[0.08em] text-white/85">
              {t("gallery.clip")}
            </span>
          )
        )}
      </div>

      <div className="pointer-events-none absolute bottom-[7px] left-[8px] flex max-w-[65%] items-center gap-[6px]">
        {game.icon && item.gameId && (
          <img
            src={iconSrcFor(item.gameId, icons)}
            onError={iconFallback(item.gameId)}
            alt=""
            className="h-[16px] w-[16px] rounded-[4px] object-cover"
          />
        )}
        <span className="truncate text-[11px] text-white/80 opacity-0 transition-opacity duration-150 group-focus-within:opacity-100 group-hover:opacity-100">
          {game.name}
        </span>
      </div>
    </div>
  );
});
