// ------------ Mod Row Parts ------------
// What the mod lists on the Mods page and in the overlay share: a mod's thumbnail, name and source, the titled
// list sections, dragging a mod between the profile and installed lists (with the ghost that follows the cursor),
// and following installs. The overlay uses the compact look of each.
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { Puzzle } from "lucide-react";
import { onModProgress, type ModEntry, type ModProfile, type ModProgress } from "../../lib/ipc";
import { fmtBytes } from "../../lib/format";
import type { TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import { useTauriEvent } from "../../lib/hooks";
import { useModProfilesStore, useModsStore } from "../../store/mods";
import { GroupLabel } from "../ui/panels";
import { Hint } from "../ui/Tooltip";

const SOURCE_LABEL: Record<string, TKey> = {
  gamebanana: "mods.row.source.gamebanana",
  archive: "mods.row.source.archive",
  manual: "mods.row.source.manual",
};

const STRIPES =
  "repeating-linear-gradient(135deg,rgba(255,255,255,0.09) 0 5px,rgba(255,255,255,0.02) 5px 10px)";

type DragSource = "profile" | "installed";

export type RowDragHandlers = Pick<
  React.DOMAttributes<HTMLDivElement>,
  "onPointerDown" | "onPointerMove" | "onPointerUp" | "onPointerCancel"
>;

export interface RowToggle {
  checked: boolean;
  ariaLabel: string;
  disabled: boolean;
  onChange: (on: boolean) => void;
}

export interface RowMenuItem {
  label: string;
  icon?: ReactNode;
  danger?: boolean;
  confirm?: string;
  onClick: () => void;
}

function Thumb({ url, dim, size }: { url: string | null; dim: boolean; size?: number }) {
  const [failed, setFailed] = useState(false);
  useEffect(() => setFailed(false), [url]);
  const image =
    url && !failed ? (
      <img
        src={url}
        alt=""
        loading="lazy"
        decoding="async"
        draggable={false}
        onError={() => setFailed(true)}
        className="h-full w-full object-cover"
      />
    ) : null;
  const faded = dim ? "opacity-50 grayscale" : "";
  if (size) {
    return (
      <span
        style={{ width: size, height: size, backgroundImage: image ? undefined : STRIPES }}
        className={`flex shrink-0 overflow-hidden rounded-[8px] border border-white/10 ${faded}`}
      >
        {image}
      </span>
    );
  }
  return (
    <span
      className={`grid h-[40px] w-[64px] shrink-0 place-items-center overflow-hidden rounded-[6px] bg-white/[0.06] ${faded}`}
    >
      {image ?? <Puzzle size={16} className="text-white/35" />}
    </span>
  );
}

export function RowBody({
  mod,
  badge,
  compact = false,
}: {
  mod: ModEntry;
  badge?: ReactNode;
  compact?: boolean;
}) {
  const t = useT();
  const source = SOURCE_LABEL[mod.source.kind];
  const meta = [
    source ? t(source) : mod.source.kind,
    mod.version ? `v${mod.version}` : "",
    mod.sizeBytes === null || mod.sizeBytes <= 0 ? "" : fmtBytes(mod.sizeBytes),
  ]
    .filter(Boolean)
    .join(", ");
  return (
    <>
      <Thumb url={mod.thumbnailUrl} dim={!mod.enabled} size={compact ? 36 : undefined} />
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-center gap-2">
          <Hint tip={mod.name} className="flex min-w-0">
            <span className={`truncate text-[13px] font-medium ${mod.enabled ? "text-white" : "text-white/55"}`}>
              {mod.name}
            </span>
          </Hint>
          {badge}
        </div>
        <div className={`truncate ${compact ? "text-[11px] text-white/40" : "text-[11.5px] text-white/55"}`}>
          {meta}
        </div>
      </div>
    </>
  );
}

// ------------ List Sections ------------

function Heading({
  title,
  count,
  compact = false,
}: {
  title: string;
  count: number | null;
  compact?: boolean;
}) {
  if (compact) {
    return (
      <div className="mb-2 flex items-baseline gap-2">
        <GroupLabel compact label={title} />
        {count !== null && <span className="ov-mono text-[11px] text-white/30">{count}</span>}
      </div>
    );
  }
  return (
    <h2 className="mb-2 flex items-baseline gap-2 text-[12.5px] font-medium text-white/55">
      {title}
      {count !== null && <span className="tabular-nums text-white/30">{count}</span>}
    </h2>
  );
}

export function Empty({ children, compact = false }: { children: ReactNode; compact?: boolean }) {
  return (
    <div
      className={`border border-dashed border-white/[0.12] bg-white/[0.02] text-center text-white/50 ${
        compact
          ? "rounded-[10px] px-4 py-5 text-[12.5px] leading-relaxed"
          : "flex flex-col items-center gap-3 rounded-ui px-6 py-8 text-[13px]"
      }`}
    >
      {children}
    </div>
  );
}

export function ModSection({
  title,
  count,
  tone,
  sectionRef,
  compact = false,
  children,
}: {
  title: string;
  count: number | null;
  tone?: string;
  sectionRef?: RefObject<HTMLElement | null>;
  compact?: boolean;
  children: ReactNode;
}) {
  const outline =
    tone === undefined
      ? undefined
      : compact
        ? `-mx-2 rounded-[12px] border border-dashed px-2 pb-2 pt-1 transition-colors duration-150 ${tone}`
        : `-m-2 rounded-ui border border-dashed p-2 transition-colors ${tone}`;
  return (
    <section ref={sectionRef} className={outline}>
      <Heading title={title} count={count} compact={compact} />
      {children}
    </section>
  );
}

// ------------ Dragging Between Lists ------------

const DRAG_THRESHOLD = 6;

interface DragState {
  modId: string;
  from: DragSource;
  over: boolean;
}

const ghostAt = (pos: { x: number; y: number }) => `translate(${pos.x + 14}px, ${pos.y + 14}px)`;

function DragGhost({
  mod,
  ghostRef,
  compact,
}: {
  mod: ModEntry;
  ghostRef: React.Ref<HTMLDivElement>;
  compact: boolean;
}) {
  const ghost = (
    <div
      ref={ghostRef}
      style={compact ? { background: "rgba(8,8,11,0.86)" } : undefined}
      className={`pointer-events-none fixed left-0 top-0 flex items-center gap-2 px-3 py-2 shadow-2xl ${
        compact
          ? "z-[130] max-w-[260px] rounded-[10px] border border-white/15"
          : "glass z-(--z-banner) max-w-[280px] rounded-ui will-change-transform"
      }`}
    >
      <Thumb url={mod.thumbnailUrl} dim={false} size={compact ? 28 : undefined} />
      <span className="truncate text-[12.5px] font-medium text-white">{mod.name}</span>
    </div>
  );
  return compact ? createPortal(ghost, document.body) : ghost;
}

export function useModDrag({
  mods,
  resetKey,
  canDrag,
  onDrop,
  onStart,
  compact = false,
}: {
  mods: ModEntry[];
  resetKey: string;
  canDrag: (mod: ModEntry, from: DragSource) => boolean;
  onDrop: (mod: ModEntry, from: DragSource) => void;
  onStart?: () => void;
  compact?: boolean;
}) {
  const [drag, setDrag] = useState<DragState | null>(null);
  const dropRef = useRef<HTMLElement>(null);
  const installedRef = useRef<HTMLElement>(null);
  const ghostRef = useRef<HTMLDivElement>(null);
  const pressRef = useRef<{ modId: string; from: DragSource; pointerId: number; x: number; y: number } | null>(
    null,
  );
  const startedRef = useRef(false);
  const posRef = useRef({ x: 0, y: 0 });
  const frameRef = useRef(0);

  const placeGhost = () => {
    frameRef.current = 0;
    if (ghostRef.current) ghostRef.current.style.transform = ghostAt(posRef.current);
  };

  const endDrag = useCallback(() => {
    pressRef.current = null;
    startedRef.current = false;
    if (frameRef.current) cancelAnimationFrame(frameRef.current);
    frameRef.current = 0;
    setDrag(null);
  }, []);

  useEffect(() => endDrag(), [resetKey, endDrag]);

  useEffect(
    () => () => {
      if (frameRef.current) cancelAnimationFrame(frameRef.current);
    },
    [],
  );

  const dragging = drag !== null;

  useLayoutEffect(() => {
    if (dragging && ghostRef.current) ghostRef.current.style.transform = ghostAt(posRef.current);
  });

  useEffect(() => {
    if (!dragging) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      endDrag();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [dragging, endDrag]);

  const inside = (el: HTMLElement | null, x: number, y: number) => {
    const r = el?.getBoundingClientRect();
    return !!r && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
  };
  const isOverTarget = (from: DragSource, x: number, y: number) =>
    inside(from === "installed" ? dropRef.current : installedRef.current, x, y);

  const handlersFor = (mod: ModEntry, from: DragSource): RowDragHandlers => ({
    onPointerDown: (e) => {
      if (e.button !== 0 || !canDrag(mod, from)) return;
      if ((e.target as HTMLElement).closest("button, input, a")) return;
      pressRef.current = { modId: mod.modId, from, pointerId: e.pointerId, x: e.clientX, y: e.clientY };
      startedRef.current = false;
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    onPointerMove: (e) => {
      const press = pressRef.current;
      if (!press || press.pointerId !== e.pointerId) return;
      if (!startedRef.current) {
        if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < DRAG_THRESHOLD) return;
        startedRef.current = true;
        onStart?.();
      }
      posRef.current = { x: e.clientX, y: e.clientY };
      if (!frameRef.current) frameRef.current = requestAnimationFrame(placeGhost);
      const over = isOverTarget(press.from, e.clientX, e.clientY);
      setDrag((prev) =>
        prev && prev.modId === press.modId && prev.from === press.from && prev.over === over
          ? prev
          : { modId: press.modId, from: press.from, over },
      );
    },
    onPointerUp: (e) => {
      const press = pressRef.current;
      if (!press || press.pointerId !== e.pointerId) return;
      const dropped = startedRef.current && isOverTarget(press.from, e.clientX, e.clientY);
      endDrag();
      if (dropped) onDrop(mod, press.from);
    },
    onPointerCancel: endDrag,
  });

  const tone = (armed: boolean) =>
    !armed
      ? "border-transparent"
      : drag?.over
        ? "border-(--accent-b) bg-(--accent-b)/10"
        : "border-(--accent-b)/50";
  const draggedMod = drag ? (mods.find((m) => m.modId === drag.modId) ?? null) : null;

  return {
    drag,
    dropRef,
    installedRef,
    handlersFor,
    profileTone: tone(drag?.from === "installed"),
    installedTone: tone(drag?.from === "profile"),
    ghost: draggedMod && <DragGhost mod={draggedMod} ghostRef={ghostRef} compact={compact} />,
  };
}

export function useModProgress(gameId: string, onInstalled: () => void, track = false): void {
  useTauriEvent(onModProgress, (p: ModProgress) => {
    if (p.gameId !== gameId) return;
    if (track) useModsStore.getState().setProgress(p.done ? null : p);
    if (p.done && !p.failed) onInstalled();
  });
}

// ------------ Profile Hooks ------------
// What the profiles list on the Mods page and the one in the overlay share.

export function useSelectedProfile(selectedId: string | null): ModProfile | null {
  const profiles = useModProfilesStore((s) => s.profiles);
  const activeId = useModProfilesStore((s) => s.activeId);
  return useMemo(
    () => profiles.find((p) => p.id === selectedId) ?? profiles.find((p) => p.id === activeId) ?? null,
    [profiles, selectedId, activeId],
  );
}

export const byName = (list: ModEntry[]): ModEntry[] => [...list].sort((a, b) => a.name.localeCompare(b.name));

export function useProfileMembers(mods: ModEntry[], profile: ModProfile | null, query: string) {
  const inProfile = useMemo(() => new Set(profile?.modIds ?? []), [profile]);
  const members = useMemo(
    () => byName(mods.filter((m) => inProfile.has(m.modId))),
    [mods, inProfile],
  );
  const shownMembers = useMemo(
    () => (query ? members.filter((m) => m.name.toLowerCase().includes(query)) : members),
    [members, query],
  );
  return { inProfile, members, shownMembers };
}

export function useProfileEditor(gameId: string, selected: ModProfile | null, onCreated: (id: string) => void) {
  const create = useModProfilesStore((s) => s.create);
  const rename = useModProfilesStore((s) => s.rename);
  const [mode, setMode] = useState<"new" | "rename" | null>(null);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (mode) inputRef.current?.select();
  }, [mode]);

  const commit = async () => {
    const name = draft.trim();
    const which = mode;
    setMode(null);
    if (!name) return;
    if (which === "new") {
      const created = await create(gameId, name);
      if (created) onCreated(created.id);
    } else if (which === "rename" && selected && name !== selected.name) {
      await rename(gameId, selected.id, name);
    }
  };

  return { mode, setMode, draft, setDraft, inputRef, commit };
}
