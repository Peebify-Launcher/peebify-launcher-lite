// ------------ Capture Viewer ------------
// The full-size capture viewer, which the overlay opens too, and its Delete button.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ChevronLeft, ChevronRight, ExternalLink, FolderOpen, Trash2, X, Loader2 } from "lucide-react";
import { openCapture, revealCapture, type MediaItem } from "../../lib/ipc";
import { onEvent } from "../../lib/rpc";
import { iconFallback, iconSrcFor, localFileSrc } from "../../lib/customMedia";
import { fmtBytes, fmtDateTime, fmtDuration, fmtRelTime } from "../../lib/format";
import { useNow, useTauriEvent, useEscapeKey } from "../../lib/hooks";
import { useAutoClear } from "./media";
import { useTimeFormat } from "../../store/settingsStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { useFocusTrap } from "../../lib/useFocusTrap";
import { gameMeta } from "../../data/games";
import type { TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { useMediaStore } from "../../store/mediaStore";
import { useModalStore } from "../../store/modalStore";
import { ActionButton } from "../ui/controls";

// ------------ Viewer ------------
// The full-size viewer that opens over the gallery for a screenshot or clip. Has previous and next arrows, file
// details, and buttons to open the file, show it in its folder or delete it. The overlay's Captures tab opens it with
// `overlay`: a one-line summary instead of the details panel, errors shown in place, a Delete that asks by being
// pressed twice, and it closes along with the overlay.
const onOverlayClosed = (cb: () => void) => onEvent("overlay-closed", () => cb());
const noEvents = async (_cb: () => void) => () => {};

function ViewerButton({
  onClick,
  danger = false,
  overlay = false,
  children,
}: {
  onClick: () => void;
  danger?: boolean;
  overlay?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`flex shrink-0 items-center gap-[6px] border px-3 py-[7px] text-[12px] font-medium transition-colors duration-150 ${
        overlay ? "rounded-[8px]" : "rounded-ui"
      } ${
        danger
          ? "border-[#ef4444]/40 bg-[#ef4444]/[0.08] text-danger-soft hover:bg-[#ef4444]/[0.16]"
          : "border-white/15 bg-white/[0.06] text-white hover:bg-white/[0.12]"
      }`}
    >
      {children}
    </button>
  );
}

function Arrow({
  side,
  onClick,
  children,
}: {
  side: "left" | "right";
  onClick: () => void;
  children: React.ReactNode;
}) {
  const t = useT();
  return (
    <button
      type="button"
      aria-label={side === "left" ? t("gallery.viewer.previous") : t("common.next")}
      onClick={onClick}
      className={`absolute top-1/2 grid h-[42px] w-[42px] -translate-y-1/2 place-items-center rounded-full bg-white/[0.08] text-white/70 hover:bg-white/[0.16] hover:text-white ${
        side === "left" ? "left-5" : "right-5"
      }`}
    >
      {children}
    </button>
  );
}

function Info({ label, value }: { label: string; value: string }) {
  if (!value) return null;
  return (
    <div>
      <dt className="text-[10.5px] font-medium uppercase tracking-[0.08em] text-white/35">{label}</dt>
      <dd className="mt-[2px] break-words text-[12.5px] text-white/85">{value}</dd>
    </div>
  );
}

function OverlaySummary({
  item,
  resolution,
  index,
  total,
}: {
  item: MediaItem;
  resolution: string;
  index: number;
  total: number;
}) {
  const t = useT();
  const now = useNow();
  return (
    <div className="ov-mono mt-[2px] truncate text-[11px] text-white/45">
      {fmtRelTime(item.takenAt, now)} · {item.sizeBytes > 0 ? fmtBytes(item.sizeBytes) : ""}
      {resolution ? ` · ${resolution}` : ""} · {t("overlay.viewer.position", { index: index + 1, total })}
    </div>
  );
}

interface LoadedMeta {
  path: string;
  resolution: string;
  durationMs: number | null;
}

export function MediaViewer({
  items,
  path,
  onPath,
  onClose,
  overlay,
}: {
  items: MediaItem[];
  path: string;
  onPath: (path: string) => void;
  onClose: () => void;
  overlay?: { onDelete: (path: string) => void };
}) {
  const timeFormat = useTimeFormat();
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);
  const [meta, setMeta] = useState<LoadedMeta | null>(null);
  const [notice, setNotice] = useState<{ path: string; text: string } | null>(null);
  const [armed, setArmed] = useState<string | null>(null);
  const video = useRef<HTMLVideoElement>(null);
  const root = useRef<HTMLDivElement>(null);

  const index = useMemo(() => {
    const found = items.findIndex((i) => i.path === path);
    return found < 0 ? 0 : found;
  }, [items, path]);
  const current = items[index];

  const step = useCallback(
    (delta: number) => {
      if (items.length === 0) return;
      const next = (index + delta + items.length) % items.length;
      setNotice(null);
      onPath(items[next].path);
    },
    [items, index, onPath],
  );

  const isTop = useEscapeKey(onClose);
  useFocusTrap(root, true, true);
  useTauriEvent(overlay ? onOverlayClosed : noEvents, onClose);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
      if (!isTop()) return;
      const target = e.target instanceof Element ? e.target : null;
      if (target?.closest("video, input, textarea, select, [role=menu], [role=slider]")) return;
      e.preventDefault();
      e.stopPropagation();
      step(e.key === "ArrowLeft" ? -1 : 1);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [isTop, step]);

  useAutoClear(notice, setNotice, 5000);
  useAutoClear(armed, setArmed, 3000);

  useEffect(() => {
    const element = video.current;
    return () => {
      if (!element) return;
      element.pause();
      element.removeAttribute("src");
      element.load();
    };
  }, [path]);

  if (!current) return null;

  const source = localFileSrc(current.path) ?? undefined;
  const game = gameMeta(current.gameId);
  const loaded = meta?.path === current.path ? meta : null;
  const nextAfterDelete = items[index + 1]?.path ?? items[index - 1]?.path ?? null;

  const act = (call: typeof openCapture, fallback: TKey) => {
    const at = current.path;
    setNotice(null);
    void call(at, !!overlay).then((r) => {
      if (!r.ok) setNotice({ path: at, text: r.error ?? t(fallback) });
    });
  };

  return createPortal(
    <div
      ref={root}
      data-viewer-root
      role="dialog"
      aria-modal="true"
      aria-label={current.name}
      tabIndex={-1}
      className={`fixed inset-0 ${overlay ? "z-[120]" : "z-(--z-viewer)"} flex flex-col outline-none`}
      style={{ background: "#07070b" }}
    >
      <header
        className={`flex shrink-0 items-center border-b border-white/[0.08] px-5 py-3 ${
          overlay ? "gap-3" : "drag-region gap-2"
        }`}
      >
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13.5px] font-medium">{current.name}</div>
          {overlay ? (
            <OverlaySummary item={current} resolution={loaded?.resolution ?? ""} index={index} total={items.length} />
          ) : (
            <div className="mt-[2px] truncate text-[11px] text-white/45">
              {t("gallery.viewer.subtitle", {
                game: game.name,
                date: fmtDateTime(current.takenAt, timeFormat),
                n: index + 1,
                total: items.length,
              })}
            </div>
          )}
          {overlay && notice?.path === current.path && (
            <div role="alert" className="mt-[2px] truncate text-[11px] text-danger-soft">
              {notice.text}
            </div>
          )}
        </div>
        <ViewerButton overlay={!!overlay} onClick={() => act(openCapture, "overlay.viewer.openFailed")}>
          <ExternalLink size={14} />
          {t("gallery.open")}
        </ViewerButton>
        <ViewerButton overlay={!!overlay} onClick={() => act(revealCapture, "overlay.viewer.revealFailed")}>
          <FolderOpen size={14} />
          {t("gallery.showInFolder")}
        </ViewerButton>
        {overlay ? (
          <ViewerButton
            overlay
            danger
            onClick={() => {
              if (armed !== current.path) {
                setArmed(current.path);
                return;
              }
              setArmed(null);
              overlay.onDelete(current.path);
            }}
          >
            <Trash2 size={14} />
            {armed === current.path ? t("overlay.viewer.deleteConfirm") : t("common.delete")}
          </ViewerButton>
        ) : (
          <DeleteButton
            items={[current]}
            onDeleted={() => {
              if (nextAfterDelete) onPath(nextAfterDelete);
              else onClose();
            }}
          />
        )}
        <button
          type="button"
          aria-label={t("common.close")}
          onClick={onClose}
          className="ml-1 grid h-[30px] w-[30px] shrink-0 place-items-center rounded-[8px] text-white/60 hover:bg-white/[0.08] hover:text-white"
        >
          <X size={18} />
        </button>
      </header>

      <div className="flex min-h-0 flex-1">
        <div className="relative flex min-h-0 min-w-0 flex-1 items-center justify-center p-6">
          {items.length > 1 && (
            <Arrow side="left" onClick={() => step(-1)}>
              <ChevronLeft size={22} />
            </Arrow>
          )}

          {current.kind === "clip" ? (
            <video
              ref={video}
              key={current.path}
              src={source}
              controls
              autoPlay
              onLoadedMetadata={(e) => {
                const el = e.currentTarget;
                setMeta({
                  path: current.path,
                  resolution: `${el.videoWidth} x ${el.videoHeight}`,
                  durationMs: Number.isFinite(el.duration) ? el.duration * 1000 : null,
                });
              }}
              className="max-h-full max-w-full rounded-[10px]"
            />
          ) : (
            <img
              key={current.path}
              src={source}
              alt={current.name}
              draggable={false}
              onLoad={(e) =>
                setMeta({
                  path: current.path,
                  resolution: `${e.currentTarget.naturalWidth} x ${e.currentTarget.naturalHeight}`,
                  durationMs: null,
                })
              }
              className="max-h-full max-w-full rounded-[10px] object-contain"
            />
          )}

          {items.length > 1 && (
            <Arrow side="right" onClick={() => step(1)}>
              <ChevronRight size={22} />
            </Arrow>
          )}
        </div>

        {!overlay && (
          <aside className="w-[264px] shrink-0 overflow-y-auto border-l border-white/[0.08] px-5 py-5">
            <dl className="flex flex-col gap-3">
              <div className="flex items-center gap-2">
                {game.icon && current.gameId && (
                  <img
                    src={iconSrcFor(current.gameId, icons)}
                    onError={iconFallback(current.gameId)}
                    alt=""
                    className="h-6 w-6 rounded-[6px] object-cover"
                  />
                )}
                <Info label={t("gallery.viewer.info.game")} value={game.name} />
              </div>
              <Info label={t("gallery.viewer.info.taken")} value={fmtDateTime(current.takenAt, timeFormat)} />
              <Info label={t("gallery.viewer.info.size")} value={current.sizeBytes > 0 ? fmtBytes(current.sizeBytes) : ""} />
              <Info label={t("gallery.viewer.info.resolution")} value={loaded?.resolution ?? ""} />
              {current.kind === "clip" && (
                <Info label={t("gallery.viewer.info.duration")} value={fmtDuration(loaded?.durationMs ?? null)} />
              )}
              <Info
                label={t("gallery.viewer.info.kind")}
                value={current.kind === "clip" ? t("gallery.clip") : t("gallery.screenshot")}
              />
              <Info label={t("gallery.viewer.info.file")} value={current.path} />
            </dl>
          </aside>
        )}
      </div>
    </div>,
    document.body,
  );
}

// ------------ Delete Capture Button ------------
// The red Delete button for one or many captures. Asks first, then sends the files to the Recycle Bin.
export function DeleteButton({
  items,
  onDeleted,
}: {
  items: MediaItem[];
  onDeleted?: () => void;
}) {
  const [pending, setPending] = useState(false);
  const remove = useMediaStore((s) => s.remove);
  const openConfirm = useModalStore((s) => s.openConfirm);
  const t = useT();

  const confirm = () => {
    const count = items.length;
    if (count === 0) return;
    openConfirm({
      title: t("gallery.delete.title", { count }),
      message:
        count === 1
          ? t("gallery.delete.messageOne")
          : t("gallery.delete.messageMany"),
      confirmLabel: t("common.delete"),
      danger: true,
      onConfirm: () => {
        setPending(true);
        void remove(items.map((i) => i.path))
          .then((deleted) => {
            if (deleted > 0) onDeleted?.();
          })
          .finally(() => setPending(false));
      },
    });
  };

  return (
    <ActionButton
      variant="danger"
      disabled={pending || items.length === 0}
      icon={pending ? <Loader2 size={14} className="animate-spin" /> : <Trash2 size={14} />}
      onClick={confirm}
    >
      {t("common.delete")}
    </ActionButton>
  );
}
