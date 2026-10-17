// ------------ Mods Library Lists ------------
// The mod lists inside the Library: the mods in the chosen profile and every mod installed. Has search and
// sorting, and mods can be dragged between the two.
import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowUpCircle,
  Layers,
  ListMinus,
  ListPlus,
  Loader2,
  MoreHorizontal,
  Puzzle,
  Search,
  Trash2,
} from "lucide-react";
import type { ModEntry, ModProfile } from "../../lib/ipc";
import type { ModUpdate } from "../../lib/gamebanana";
import type { TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import type { Game } from "../../data/games";
import { ActionButton, Toggle } from "../ui/controls";
import { PopoverMenu, menuIcon, menuRow } from "../ui/PopoverMenu";
import { GroupBox } from "../ui/panels";
import { SearchField } from "../ui/SearchField";
import { Select } from "../ui/Select";
import { Hint } from "../ui/Tooltip";
import { ModProgressRow } from "./ModProgressRow";
import {
  Empty,
  ModSection,
  RowBody,
  useModDrag,
  useProfileMembers,
  type RowDragHandlers,
  type RowMenuItem,
  type RowToggle,
} from "./ModsHubRow";

type SortKey = "name" | "newest" | "largest";

const SORTS: { value: SortKey; label: TKey }[] = [
  { value: "name", label: "mods.list.sort.name" },
  { value: "newest", label: "mods.list.sort.newest" },
  { value: "largest", label: "mods.list.sort.largest" },
];

function Skeleton({ rows }: { rows: number }) {
  const t = useT();
  return (
    <div className="grid animate-pulse gap-2" aria-label={t("mods.list.loading")}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="h-[58px] rounded-ui border border-white/[0.08] bg-white/[0.04]" />
      ))}
    </div>
  );
}

function sortMods(list: ModEntry[], sort: SortKey): ModEntry[] {
  const sorted = [...list];
  if (sort === "name") sorted.sort((a, b) => a.name.localeCompare(b.name));
  else if (sort === "newest")
    sorted.sort((a, b) => {
      if (!a.installedAt && !b.installedAt) return a.name.localeCompare(b.name);
      if (!a.installedAt) return 1;
      if (!b.installedAt) return -1;
      return b.installedAt.localeCompare(a.installedAt);
    });
  else sorted.sort((a, b) => (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0));
  return sorted;
}

// ------------ Mod Rows ------------
// Each row shows the mod and an on/off switch, plus an Update badge and button and a menu with things like Remove
// from profile and Uninstall.

function UpdateBadge({ update }: { update: ModUpdate }) {
  const t = useT();
  return (
    <Hint
      tip={update.version ? t("mods.row.updateVersionTitle", { version: update.version }) : t("mods.row.updateTitle")}
      className="inline-flex shrink-0"
    >
      <span className="rounded-full border border-(--accent-b)/50 bg-(--accent-b)/15 px-2 py-[1px] text-[10.5px] font-medium text-white/85">
        {update.version ? t("mods.row.updateBadgeVersion", { version: update.version }) : t("common.update")}
      </span>
    </Hint>
  );
}

function UpdateButton({
  updating,
  disabled,
  onClick,
}: {
  updating: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  const t = useT();
  return (
    <ActionButton
      variant="accent"
      icon={updating ? <Loader2 size={14} className="animate-spin" /> : <ArrowUpCircle size={14} />}
      disabled={updating || disabled}
      onClick={onClick}
    >
      {updating ? t("common.updating") : t("common.update")}
    </ActionButton>
  );
}

function RowMenu({ label, items }: { label: string; items: RowMenuItem[] }) {
  const [open, setOpen] = useState(false);
  const btnRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        className="grid h-[30px] w-[30px] shrink-0 place-items-center rounded-ui border border-white/[0.08] bg-white/[0.04] text-white/55 transition hover:bg-white/[0.1] hover:text-white/85"
      >
        <MoreHorizontal size={15} />
      </button>
      <PopoverMenu
        open={open}
        onClose={() => setOpen(false)}
        anchorRef={btnRef}
        width={220}
        estimatedHeight={items.length * 42 + 12}
      >
        {items.map((item) => (
          <button
            key={item.label}
            type="button"
            role="menuitem"
            onClick={item.onClick}
            className={menuRow(item.danger)}
          >
            <span className={menuIcon(item.danger)}>{item.icon}</span>
            <span className="flex-1 truncate">{item.label}</span>
          </button>
        ))}
      </PopoverMenu>
    </>
  );
}

function ModRow({
  mod,
  update,
  updating,
  anyUpdating,
  draggable,
  dragging,
  dragHandlers,
  busy,
  toggle,
  menu,
  onUpdate,
}: {
  mod: ModEntry;
  update: ModUpdate | null;
  updating: boolean;
  anyUpdating: boolean;
  draggable: boolean;
  dragging: boolean;
  dragHandlers: RowDragHandlers;
  busy: boolean;
  toggle: RowToggle;
  menu: RowMenuItem[];
  onUpdate: () => void;
}) {
  const t = useT();
  return (
    <div
      {...dragHandlers}
      className={`flex touch-none select-none items-center gap-3 px-3 py-[9px] transition-opacity ${
        draggable ? "cursor-grab active:cursor-grabbing" : ""
      } ${dragging ? "opacity-40" : ""}`}
    >
      <RowBody mod={mod} badge={update && <UpdateBadge update={update} />} />
      {update && <UpdateButton updating={updating} disabled={busy || anyUpdating} onClick={onUpdate} />}
      <Toggle {...toggle} />
      <RowMenu label={t("mods.row.more", { name: mod.name })} items={menu} />
    </div>
  );
}

// ------------ Lists ------------

export function ModsHubList({
  game,
  loaded,
  mods,
  modsError,
  profile,
  profileIsActive,
  busy,
  togglingModIds,
  downloadingKey,
  updates,
  updatingModId,
  onBrowse,
  onRetryMods,
  onToggleEnabled,
  onAddToProfile,
  onRemoveFromProfile,
  onUninstall,
  onOpenGameBanana,
  onUpdate,
}: {
  game: Game;
  loaded: boolean;
  mods: ModEntry[];
  modsError: boolean;
  profile: ModProfile | null;
  profileIsActive: boolean;
  busy: boolean;
  togglingModIds: ReadonlySet<string>;
  downloadingKey: string | null;
  updates: Map<string, ModUpdate>;
  updatingModId: string | null;
  onBrowse?: () => void;
  onRetryMods: () => void;
  onToggleEnabled: (mod: ModEntry, on: boolean) => void;
  onAddToProfile: (mod: ModEntry) => void;
  onRemoveFromProfile: (mod: ModEntry) => void;
  onUninstall: (mod: ModEntry) => void;
  onOpenGameBanana?: (mod: ModEntry) => void;
  onUpdate: (mod: ModEntry) => void;
}) {
  const t = useT();
  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<SortKey>("name");

  useEffect(() => setSearch(""), [game.id, profile?.id]);

  const query = search.trim().toLowerCase();
  const { inProfile, members, shownMembers } = useProfileMembers(mods, profile, query);
  const visible = useMemo(
    () => sortMods(query ? mods.filter((m) => m.name.toLowerCase().includes(query)) : mods, sort),
    [mods, query, sort],
  );

  const drag = useModDrag({
    mods,
    resetKey: `${game.id}:${profile?.id}`,
    canDrag: (mod, from) => !busy && !!profile && !(from === "installed" && inProfile.has(mod.modId)),
    onDrop: (mod, from) => (from === "installed" ? onAddToProfile(mod) : onRemoveFromProfile(mod)),
  });

  const anyUpdating = updatingModId !== null || downloadingKey !== null;
  const toggleFor = (mod: ModEntry): RowToggle =>
    !profile || profileIsActive
      ? {
          checked: mod.enabled,
          ariaLabel: t("mods.list.toggleEnable", { name: mod.name }),
          disabled: busy || togglingModIds.has(mod.modId),
          onChange: (on) => onToggleEnabled(mod, on),
        }
      : {
          checked: inProfile.has(mod.modId),
          ariaLabel: t("mods.list.toggleInclude", { name: mod.name, profile: profile.name }),
          disabled: busy,
          onChange: (on) => (on ? onAddToProfile(mod) : onRemoveFromProfile(mod)),
        };

  const menuFor = (mod: ModEntry, inProfileList: boolean): RowMenuItem[] => {
    const menu: RowMenuItem[] = [];
    if (profile && !busy) {
      if (inProfileList || inProfile.has(mod.modId)) {
        menu.push({
          label: t("mods.row.removeFromProfile"),
          icon: <ListMinus size={14} />,
          onClick: () => onRemoveFromProfile(mod),
        });
      } else {
        menu.push({
          label: t("mods.row.addTo", { profile: profile.name }),
          icon: <ListPlus size={14} />,
          onClick: () => onAddToProfile(mod),
        });
      }
    }
    if (onOpenGameBanana && mod.source.gbModId != null) {
      menu.push({ label: t("mods.row.showInBrowse"), icon: <Search size={14} />, onClick: () => onOpenGameBanana(mod) });
    }
    menu.push({ label: t("common.uninstall"), icon: <Trash2 size={14} />, danger: true, onClick: () => onUninstall(mod) });
    return menu;
  };

  const rowsFor = (list: ModEntry[], from: "profile" | "installed") => (
    <GroupBox divided>
      {list.map((mod) => (
        <ModRow
          key={mod.modId}
          mod={mod}
          update={updates.get(mod.modId) ?? null}
          updating={updatingModId === mod.modId}
          anyUpdating={anyUpdating}
          draggable={from === "profile" || (!!profile && !inProfile.has(mod.modId))}
          dragging={drag.drag?.modId === mod.modId}
          dragHandlers={drag.handlersFor(mod, from)}
          busy={busy}
          toggle={toggleFor(mod)}
          menu={menuFor(mod, from === "profile")}
          onUpdate={() => onUpdate(mod)}
        />
      ))}
    </GroupBox>
  );

  return (
    <div className="flex min-w-0 flex-col gap-6">
      <ModProgressRow gameId={game.id} />

      {loaded && !modsError && mods.length > 0 && (
        <div className="-mb-2 flex flex-wrap items-center gap-2">
          <SearchField
            value={search}
            onChange={setSearch}
            placeholder={t("mods.list.searchPlaceholder")}
            ariaLabel={t("mods.list.searchLabel")}
            className="min-w-[200px] flex-1"
          />
          <Select
            ariaLabel={t("common.sort")}
            options={SORTS.map((o) => ({ value: o.value, label: t(o.label) }))}
            value={sort}
            onChange={(v) => setSort(v as SortKey)}
          />
        </div>
      )}

      <ModSection
        title={t("mods.list.inProfile.title")}
        count={loaded && !modsError && profile ? members.length : null}
        tone={drag.profileTone}
        sectionRef={drag.dropRef}
      >
        {!loaded ? (
          <Skeleton rows={1} />
        ) : !profile ? (
          <Empty>
            <Layers size={20} className="opacity-60" />
            <p>{t("mods.list.inProfile.pickProfile")}</p>
          </Empty>
        ) : modsError ? (
          <Empty>
            <p>{t("mods.list.inProfile.readError")}</p>
          </Empty>
        ) : members.length === 0 ? (
          <Empty>
            <Layers size={20} className="opacity-60" />
            <p>{t("mods.list.inProfile.empty")}</p>
          </Empty>
        ) : shownMembers.length === 0 ? (
          <Empty>
            <p>{t("mods.list.noMatches")}</p>
          </Empty>
        ) : (
          rowsFor(shownMembers, "profile")
        )}
      </ModSection>

      <ModSection
        title={t("mods.list.installed.title")}
        count={loaded && !modsError ? mods.length : null}
        tone={drag.installedTone}
        sectionRef={drag.installedRef}
      >
        {!loaded ? (
          <Skeleton rows={3} />
        ) : modsError ? (
          <Empty>
            <AlertTriangle size={20} className="opacity-60" />
            <p className="max-w-[46ch]">
              {t("mods.list.installed.readError", { game: game.name })}
            </p>
            <ActionButton onClick={onRetryMods}>{t("common.retry")}</ActionButton>
          </Empty>
        ) : mods.length === 0 ? (
          <Empty>
            <Puzzle size={20} className="opacity-60" />
            <p className="max-w-[46ch]">
              {onBrowse
                ? t("mods.list.installed.emptyBrowse", { game: game.name })
                : t("mods.list.installed.empty", { game: game.name })}
            </p>
            {onBrowse && <ActionButton onClick={onBrowse}>{t("mods.list.installed.browseMods")}</ActionButton>}
          </Empty>
        ) : visible.length === 0 ? (
          <Empty>
            <p>{t("mods.list.noMatches")}</p>
          </Empty>
        ) : (
          rowsFor(visible, "installed")
        )}
      </ModSection>

      {drag.ghost}
    </div>
  );
}
