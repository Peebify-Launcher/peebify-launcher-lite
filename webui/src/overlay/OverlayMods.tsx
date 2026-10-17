// ------------ Overlay Mods ------------
// The Mods tab of the overlay: switch installed mods on and off, drag them into profiles, apply a profile,
// and remove mods without leaving the game. Built from the Mods page's row parts in their compact look, with a
// menu that opens inside the row instead of popping up.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Search } from "lucide-react";
import { activeInstallFor, useModsStore, useModProfilesStore } from "../store/mods";
import { useNotificationStore } from "../store/notifications";
import { deleteMod, setModsEnabledBulk, type ModEntry } from "../lib/ipc";
import { ActionButton, Chip, Toggle } from "../components/ui/controls";
import { Hint } from "../components/ui/Tooltip";
import { ProgressBar, GroupBox, GroupLabel } from "../components/ui/panels";
import {
  byName,
  Empty,
  ModSection,
  RowBody,
  useModDrag,
  useModProgress,
  useProfileEditor,
  useProfileMembers,
  useSelectedProfile,
  type RowDragHandlers,
  type RowMenuItem,
  type RowToggle,
} from "../components/mods/ModsHubRow";
import type { PanelProps } from "./OverlayApp";
import { spacedAccelerator } from "../lib/hotkeys";
import { Trans, useT } from "../lib/useT";

const INPUT =
  "w-[200px] rounded-[8px] border border-white/[0.28] bg-black/30 px-3 py-[6px] text-[12px] text-white outline-none placeholder:text-white/30";

type RowMenuState = { key: string; step: string } | null;

function ProfileRow({
  mod,
  dragging,
  dragHandlers,
  busy,
  toggle,
  onRemove,
}: {
  mod: ModEntry;
  dragging: boolean;
  dragHandlers: RowDragHandlers;
  busy: boolean;
  toggle: RowToggle;
  onRemove: () => void;
}) {
  const t = useT();
  return (
    <div
      {...dragHandlers}
      className={`flex cursor-grab touch-none select-none items-center gap-3 px-4 py-[10px] transition-opacity active:cursor-grabbing ${
        dragging ? "opacity-40" : ""
      }`}
    >
      <RowBody compact mod={mod} />
      <ActionButton size="compact" disabled={busy} onClick={onRemove}>
        {t("common.remove")}
      </ActionButton>
      <Toggle compact {...toggle} />
    </div>
  );
}

function InstalledRow({
  mod,
  canDrag,
  dragging,
  dragHandlers,
  busy,
  toggle,
  items,
  menu,
  onMenu,
}: {
  mod: ModEntry;
  canDrag: boolean;
  dragging: boolean;
  dragHandlers: RowDragHandlers;
  busy: boolean;
  toggle: RowToggle;
  items: RowMenuItem[];
  menu: RowMenuState;
  onMenu: (next: RowMenuState) => void;
}) {
  const t = useT();
  const open = menu?.key === mod.modId ? menu : null;
  const asking = open && items.find((item) => item.label === open.step);
  const close = () => onMenu(null);
  return (
    <div className={`transition-opacity ${dragging ? "opacity-40" : ""}`}>
      <div
        {...dragHandlers}
        className={`flex touch-none select-none items-center gap-3 px-4 py-[10px] ${
          canDrag ? "cursor-grab active:cursor-grabbing" : ""
        }`}
      >
        <RowBody compact mod={mod} />
        <Toggle compact {...toggle} />
        <button
          type="button"
          aria-label={t("mods.row.more", { name: mod.name })}
          aria-expanded={open !== null}
          disabled={busy}
          onClick={() => onMenu(open ? null : { key: mod.modId, step: "menu" })}
          className={`grid h-[26px] w-[26px] shrink-0 place-items-center rounded-[7px] border text-[14px] leading-none transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-40 ${
            open
              ? "border-white/[0.22] bg-white/10 text-white"
              : "border-white/10 bg-white/[0.04] text-white/55 hover:text-white/85"
          }`}
        >
          &middot;&middot;&middot;
        </button>
      </div>
      {open && (
        <div className="flex flex-wrap items-center gap-2 border-t border-white/[0.055] bg-black/20 px-4 py-2">
          {asking ? (
            <>
              <span className="min-w-0 flex-1 text-[12px] text-white/70">{asking.confirm}</span>
              <ActionButton size="compact" variant="danger" onClick={asking.onClick}>
                {t("common.yes")}
              </ActionButton>
              <ActionButton size="compact" onClick={close}>
                {t("common.no")}
              </ActionButton>
            </>
          ) : (
            <>
              {items.map((item) => (
                <ActionButton
                  key={item.label}
                  size="compact"
                  variant={item.danger ? "danger" : "neutral"}
                  disabled={!item.danger && busy}
                  onClick={() => {
                    if (item.confirm) {
                      onMenu({ key: mod.modId, step: item.label });
                      return;
                    }
                    close();
                    item.onClick();
                  }}
                >
                  {item.label}
                </ActionButton>
              ))}
              <span className="ml-auto">
                <ActionButton size="compact" onClick={close}>
                  {t("common.close")}
                </ActionButton>
              </span>
            </>
          )}
        </div>
      )}
    </div>
  );
}

export function OverlayMods({ gameId, modsUsable, mods: status }: PanelProps) {
  const t = useT();
  const mods = useModsStore((s) => s.mods);
  const progress = useModsStore((s) => s.progress);
  const refreshMods = useModsStore((s) => s.refresh);
  const refreshModsIfCurrent = useModsStore((s) => s.refreshIfCurrent);
  const setLocal = useModsStore((s) => s.setModsEnabledLocal);
  const downloadingKey = useModsStore((s) => activeInstallFor(s.installing, gameId));
  const loaded = useModsStore((s) => s.loadedGameId) === gameId;
  const pending = useModsStore((s) => s.pending[gameId] ?? 0);
  const reloadKey = useModsStore((s) => s.reloadKeys[gameId] ?? "F10");
  const addPending = useModsStore((s) => s.addPending);
  const clearPending = useModsStore((s) => s.clearPending);
  const push = useNotificationStore((s) => s.push);

  const profiles = useModProfilesStore((s) => s.profiles);
  const activeId = useModProfilesStore((s) => s.activeId);
  const applying = useModProfilesStore((s) => s.applying);
  const profilesLoaded = useModProfilesStore((s) => s.loadedGameId) === gameId;
  const refreshProfiles = useModProfilesStore((s) => s.refresh);
  const refreshProfilesIfCurrent = useModProfilesStore((s) => s.refreshIfCurrent);
  const apply = useModProfilesStore((s) => s.apply);
  const duplicate = useModProfilesStore((s) => s.duplicate);
  const remove = useModProfilesStore((s) => s.remove);
  const setMembers = useModProfilesStore((s) => s.setMembers);

  const [search, setSearch] = useState("");
  const [busy, setBusy] = useState(false);
  const [toggling, setToggling] = useState<Set<string>>(() => new Set());
  const [selectedProfileId, setSelectedProfileId] = useState<string | null>(null);
  const profile = useSelectedProfile(selectedProfileId);
  const { mode, setMode, draft, setDraft, inputRef: draftInput, commit: commitDraft } = useProfileEditor(
    gameId,
    profile,
    setSelectedProfileId,
  );
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [rowMenu, setRowMenu] = useState<RowMenuState>(null);

  const filter = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/") return;
      const target = e.target as HTMLElement | null;
      if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA") return;
      e.preventDefault();
      filter.current?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    setSelectedProfileId(null);
    setMode(null);
    setConfirmDelete(false);
    setRowMenu(null);
    setSearch("");
    if (!modsUsable) return;
    void refreshMods(gameId);
    void refreshProfiles(gameId);
  }, [gameId, modsUsable, refreshMods, refreshProfiles, setMode]);

  const refreshAll = useCallback(async () => {
    await Promise.all([refreshModsIfCurrent(gameId), refreshProfilesIfCurrent(gameId)]);
  }, [gameId, refreshModsIfCurrent, refreshProfilesIfCurrent]);

  const locked = busy || !!applying || !loaded || !profilesLoaded;
  const progressPercent = progress ? Math.max(0, Math.min(100, progress.percentage)) : 0;
  useModProgress(gameId, () => void refreshAll(), true);

  const installed = useMemo(() => byName(mods), [mods]);
  const query = search.trim().toLowerCase();
  const { inProfile, members, shownMembers } = useProfileMembers(mods, profile, query);
  const visible = useMemo(
    () => (query ? installed.filter((m) => m.name.toLowerCase().includes(query)) : installed),
    [installed, query],
  );

  const warnFailed = (title: string, failed: { folderName: string; error: string }[]) => {
    if (failed.length === 0) return;
    push({
      type: "warning",
      title,
      text: failed.map((f) => `${f.folderName}: ${f.error}`).join("; "),
    });
  };

  const toggle = async (mod: ModEntry, on: boolean) => {
    if (toggling.has(mod.modId)) return;
    setToggling((s) => new Set(s).add(mod.modId));
    setLocal([mod.folderName], on);
    let changed = 0;
    try {
      const result = await setModsEnabledBulk(gameId, [mod.folderName], on);
      if (result) {
        changed = result.changed.length;
        addPending(gameId, changed);
        warnFailed(
          t(on ? "overlay.mods.switchOnFailed" : "overlay.mods.switchOffFailed", { name: mod.name }),
          result.failed,
        );
      }
    } finally {
      setToggling((s) => {
        const next = new Set(s);
        next.delete(mod.modId);
        return next;
      });
    }
    if (changed === 0) void refreshModsIfCurrent(gameId);
  };

  const setMembership = async (mod: ModEntry, joining: boolean) => {
    if (!profile) return;
    setBusy(true);
    try {
      await setMembers(
        gameId,
        profile.id,
        joining ? [mod.modId] : [],
        joining ? [] : [mod.modId],
      );
    } finally {
      setBusy(false);
    }
  };

  const drag = useModDrag({
    mods,
    resetKey: `${gameId}:${modsUsable}:${profile?.id}`,
    canDrag: (mod, from) => !locked && !!profile && !(from === "installed" && inProfile.has(mod.modId)),
    onDrop: (mod, from) => void setMembership(mod, from === "installed"),
    onStart: () => setRowMenu(null),
    compact: true,
  });

  const uninstall = async (mod: ModEntry) => {
    setRowMenu(null);
    setBusy(true);
    try {
      await deleteMod(gameId, mod.folderName, mod.modId);
    } finally {
      setBusy(false);
    }
    await refreshAll();
  };

  const onDraftKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key !== "Enter" && e.key !== "Escape") return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Enter") void commitDraft();
    else setMode(null);
  };

  const canApply = !!status?.toolchainInstalled && !!status?.gameEnabled;

  const applySelected = async () => {
    if (!profile || applying) return;
    const result = await apply(gameId, profile.id);
    void refreshModsIfCurrent(gameId);
    if (!result) return;
    if (canApply) addPending(gameId, result.enabled.length + result.disabled.length);
    warnFailed(t("overlay.mods.profileSwitchFailed", { profile: profile.name }), result.failed);
  };

  const deleteSelected = async () => {
    if (!profile) return;
    setConfirmDelete(false);
    await remove(gameId, profile.id);
    setSelectedProfileId(null);
  };

  const toggleFor = (mod: ModEntry): RowToggle =>
    !profile || profile.id === activeId
      ? {
          checked: mod.enabled,
          ariaLabel: t("overlay.mods.useMod", { name: mod.name }),
          disabled: locked || toggling.has(mod.modId),
          onChange: (on) => void toggle(mod, on),
        }
      : {
          checked: inProfile.has(mod.modId),
          ariaLabel: t("mods.list.toggleInclude", { name: mod.name, profile: profile.name }),
          disabled: locked,
          onChange: (on) => void setMembership(mod, on),
        };

  const itemsFor = (mod: ModEntry): RowMenuItem[] => {
    const items: RowMenuItem[] = [];
    if (profile) {
      const member = inProfile.has(mod.modId);
      items.push({
        label: member ? t("mods.row.removeFromProfile") : t("mods.row.addTo", { profile: profile.name }),
        onClick: () => void setMembership(mod, !member),
      });
    }
    items.push({
      label: t("common.uninstall"),
      danger: true,
      confirm: t("overlay.mods.uninstallConfirm"),
      onClick: () => void uninstall(mod),
    });
    return items;
  };

  if (!modsUsable) {
    if (!status) return null;
    return (
      <p className="max-w-[70ch] text-[13px] leading-relaxed text-white/45">
        {t("overlay.mods.modSupportOff")}
      </p>
    );
  }

  const missing = profile ? profile.modCount - profile.resolvedCount : 0;

  const loaderLabel =
    status?.toolchainInstalled && status.variant
      ? t("overlay.mods.loaderLoaded", {
          loader: `${status.variant.toUpperCase()}${
            status.versions?.[status.variant] ? ` ${status.versions[status.variant]}` : ""
          }`,
        })
      : t("overlay.mods.loaderNotInstalled", {
          loader: (status?.variant ?? "xxmi").toUpperCase(),
        });

  return (
    <div className="flex flex-col gap-[18px]">
      <div className="flex flex-col gap-[10px]">
        <div className="flex items-center justify-between gap-3">
          <GroupLabel compact label={t("overlay.mods.profiles")} />
          <div className="flex shrink-0 items-center gap-2 rounded-[8px] border border-white/15 bg-white/[0.07] px-[11px] py-[6px]">
            <span
              className={`h-[6px] w-[6px] rounded-full ${
                status?.toolchainInstalled ? "bg-white" : "bg-white/35"
              }`}
            />
            <span className="text-[11.5px] font-medium">{loaderLabel}</span>
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {!profilesLoaded ? (
            <span className="text-[12px] text-white/40">{t("common.loading")}</span>
          ) : (
            profiles.map((p) => {
              const isSelected = profile?.id === p.id;
              if (mode === "rename" && isSelected) {
                return (
                  <input
                    key={p.id}
                    ref={draftInput}
                    value={draft}
                    autoFocus
                    maxLength={60}
                    onChange={(e) => setDraft(e.target.value)}
                    onBlur={() => void commitDraft()}
                    onKeyDown={onDraftKey}
                    className={INPUT}
                  />
                );
              }
              return (
                <Chip
                  key={p.id}
                  compact
                  active={isSelected}
                  disabled={!!applying}
                  onClick={() => {
                    setSelectedProfileId(p.id);
                    setConfirmDelete(false);
                    setRowMenu(null);
                  }}
                >
                  <span className="flex items-center gap-[7px]">
                    {p.id === activeId && (
                      <Hint tip={t("overlay.mods.activeProfile")} className="inline-flex shrink-0">
                        <span className="h-[6px] w-[6px] rounded-full bg-white" />
                      </Hint>
                    )}
                    <span>{p.name}</span>
                    <span className="ov-mono text-[10px] opacity-60">{p.resolvedCount}</span>
                  </span>
                </Chip>
              );
            })
          )}
          {mode === "new" && (
            <input
              ref={draftInput}
              value={draft}
              autoFocus
              maxLength={60}
              placeholder={t("overlay.mods.profileName")}
              onChange={(e) => setDraft(e.target.value)}
              onBlur={() => void commitDraft()}
              onKeyDown={onDraftKey}
              className={INPUT}
            />
          )}
          {profilesLoaded && profiles.length === 0 && mode !== "new" && (
            <span className="text-[12px] text-white/40">{t("overlay.mods.noProfiles")}</span>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <ActionButton size="compact" disabled={locked || !profile} onClick={() => void applySelected()}>
            {applying ? t("overlay.mods.applying") : t("overlay.mods.apply")}
          </ActionButton>
          <span className="ml-auto flex flex-wrap items-center gap-2">
            {confirmDelete && profile ? (
              <>
                <span className="text-[12px] text-white/70">
                  {t("overlay.mods.deleteProfileConfirm", { profile: profile.name })}
                </span>
                <ActionButton
                  size="compact"
                  variant="danger"
                  disabled={locked}
                  onClick={() => void deleteSelected()}
                >
                  {t("common.yes")}
                </ActionButton>
                <ActionButton size="compact" onClick={() => setConfirmDelete(false)}>
                  {t("common.no")}
                </ActionButton>
              </>
            ) : (
              <>
                <ActionButton
                  size="compact"
                  variant="dashed"
                  disabled={locked || mode !== null}
                  onClick={() => {
                    setDraft("");
                    setMode("new");
                  }}
                >
                  {t("overlay.mods.newProfile")}
                </ActionButton>
                <ActionButton
                  size="compact"
                  disabled={locked || !profile || mode !== null}
                  onClick={() => {
                    if (!profile) return;
                    setDraft(profile.name);
                    setMode("rename");
                  }}
                >
                  {t("overlay.mods.rename")}
                </ActionButton>
                <ActionButton
                  size="compact"
                  disabled={locked || !profile}
                  onClick={() => {
                    if (profile) void duplicate(gameId, profile.id);
                  }}
                >
                  {t("overlay.mods.duplicate")}
                </ActionButton>
                <ActionButton
                  size="compact"
                  variant="danger"
                  disabled={locked || !profile || profiles.length <= 1}
                  onClick={() => setConfirmDelete(true)}
                >
                  {t("common.delete")}
                </ActionButton>
              </>
            )}
          </span>
        </div>

        {profile && missing > 0 && (
          <p className="text-[12px] text-white/45">
            {t("overlay.mods.missing", { count: missing })}
          </p>
        )}
      </div>

      {pending > 0 && (
        <div className="flex items-start gap-3 rounded-[10px] border border-[#f59e0b]/30 bg-[#f59e0b]/[0.08] px-4 py-3 text-[12.5px] leading-relaxed text-warning">
          <span className="min-w-0 flex-1">
            <Trans
              k="overlay.mods.pending"
              vars={{ count: pending, key: spacedAccelerator(reloadKey) }}
              tags={{
                key: (chunk) => (
                  <span className="ov-mono rounded-[5px] bg-white/[0.12] px-[6px] py-[2px] text-white/85">
                    {chunk}
                  </span>
                ),
              }}
            />
          </span>
          <ActionButton size="compact" onClick={() => clearPending(gameId)}>
            {t("common.done")}
          </ActionButton>
        </div>
      )}

      {progress && !downloadingKey && (
        <div className="rounded-[10px] border border-white/[0.08] bg-white/[0.04] px-4 py-3">
          <div className="mb-2 flex items-center justify-between text-[12px] text-white/60">
            <span className="truncate">{progress.message}</span>
            <span className="ov-mono ml-3 shrink-0">{Math.round(progressPercent)}%</span>
          </div>
          <div className="flex">
            <ProgressBar compact value={progressPercent} className="flex-1" />
          </div>
        </div>
      )}

      {loaded && mods.length > 0 && (
        <label className="flex items-center gap-[9px] rounded-[9px] border border-white/[0.12] bg-black/[0.28] px-3 py-[8px]">
          <Search size={14} className="shrink-0 text-white/45" />
          <input
            ref={filter}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && search) {
                e.preventDefault();
                setSearch("");
              }
            }}
            placeholder={t("overlay.mods.filter", { count: installed.length })}
            className="w-full bg-transparent text-[12.5px] text-white outline-none placeholder:text-white/40"
          />
          <span className="ov-mono ml-auto shrink-0 rounded-[5px] bg-white/[0.08] px-[6px] py-[2px] text-[10.5px] text-white/55">
            /
          </span>
        </label>
      )}

      <ModSection
        compact
        title={t("mods.list.inProfile.title")}
        count={loaded && profile ? members.length : null}
        tone={drag.profileTone}
        sectionRef={drag.dropRef}
      >
        {!loaded || !profilesLoaded ? (
          <p className="text-[12.5px] text-white/45">{t("common.loading")}</p>
        ) : !profile ? (
          <Empty compact>{t("overlay.mods.pickProfile")}</Empty>
        ) : members.length === 0 ? (
          <Empty compact>{t("overlay.mods.profileEmpty")}</Empty>
        ) : shownMembers.length === 0 ? (
          <Empty compact>{t("overlay.mods.noMatch")}</Empty>
        ) : (
          <GroupBox compact divided>
            {shownMembers.map((mod) => (
              <ProfileRow
                key={mod.modId}
                mod={mod}
                dragging={drag.drag?.modId === mod.modId}
                dragHandlers={drag.handlersFor(mod, "profile")}
                busy={locked}
                toggle={toggleFor(mod)}
                onRemove={() => void setMembership(mod, false)}
              />
            ))}
          </GroupBox>
        )}
      </ModSection>

      <ModSection
        compact
        title={t("mods.list.installed.title")}
        count={loaded ? installed.length : null}
        tone={drag.installedTone}
        sectionRef={drag.installedRef}
      >
        {!loaded ? (
          <p className="text-[12.5px] text-white/45">{t("common.loading")}</p>
        ) : mods.length === 0 ? (
          <Empty compact>{t("overlay.mods.noneInstalled")}</Empty>
        ) : visible.length === 0 ? (
          <Empty compact>{t("overlay.mods.noMatch")}</Empty>
        ) : (
          <GroupBox compact divided>
            {visible.map((mod) => (
              <InstalledRow
                key={mod.modId}
                mod={mod}
                canDrag={!!profile && !inProfile.has(mod.modId)}
                dragging={drag.drag?.modId === mod.modId}
                dragHandlers={drag.handlersFor(mod, "installed")}
                busy={locked}
                toggle={toggleFor(mod)}
                items={itemsFor(mod)}
                menu={rowMenu}
                onMenu={setRowMenu}
              />
            ))}
          </GroupBox>
        )}
      </ModSection>

      {drag.ghost}
    </div>
  );
}
