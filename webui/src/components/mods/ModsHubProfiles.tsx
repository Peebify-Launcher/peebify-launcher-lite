// ------------ Mod Profiles ------------
// The profiles list in the Mods Library. Create, rename, duplicate, delete and pick a profile, and apply one so
// only its mods are switched on in the game.
import { useEffect } from "react";
import { Check, Copy, Layers, Pencil, Play, Plus, Trash2, X } from "lucide-react";
import type { GameId } from "../../data/games";
import { useModProfilesStore, useModsStore } from "../../store/mods";
import { useModalStore } from "../../store/modalStore";
import { useNotificationStore } from "../../store/notifications";
import { useT } from "../../lib/useT";
import { ActionButton } from "../ui/controls";
import { GroupBox } from "../ui/panels";
import { Hint } from "../ui/Tooltip";
import { useProfileEditor } from "./ModsHubRow";

const INPUT =
  "w-full rounded-[7px] border border-(--accent-b)/50 bg-white/[0.06] px-[10px] py-[7px] text-[12.5px] text-white outline-none placeholder:text-white/30";

function IconAction({
  children,
  label,
  onClick,
  disabled = false,
  danger = false,
}: {
  children: React.ReactNode;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
}) {
  return (
    <Hint tip={label}>
      <button
        type="button"
        aria-label={label}
        disabled={disabled}
        onClick={onClick}
        className={`inline-flex h-[30px] w-[30px] items-center justify-center rounded-ui border border-white/[0.08] bg-white/[0.04] transition duration-150 hover:bg-white/[0.1] disabled:cursor-not-allowed disabled:opacity-30 ${
          danger ? "text-danger-soft hover:border-[#ef4444]/40" : "text-white/55 hover:text-white/85"
        }`}
      >
        {children}
      </button>
    </Hint>
  );
}

export function ModsHubProfiles({
  gameId,
  canApply,
  selectedId,
  disabled,
  onSelect,
}: {
  gameId: GameId;
  canApply: boolean;
  selectedId: string | null;
  disabled: boolean;
  onSelect: (id: string) => void;
}) {
  const t = useT();
  const profiles = useModProfilesStore((s) => s.profiles);
  const activeId = useModProfilesStore((s) => s.activeId);
  const applying = useModProfilesStore((s) => s.applying);
  const loadedGameId = useModProfilesStore((s) => s.loadedGameId);
  const remove = useModProfilesStore((s) => s.remove);
  const duplicate = useModProfilesStore((s) => s.duplicate);
  const apply = useModProfilesStore((s) => s.apply);
  const refreshModsIfCurrent = useModsStore((s) => s.refreshIfCurrent);
  const openConfirm = useModalStore((s) => s.openConfirm);
  const push = useNotificationStore((s) => s.push);

  const loaded = loadedGameId === gameId;
  const selected = profiles.find((p) => p.id === selectedId) ?? null;
  const { mode, setMode, draft, setDraft, inputRef, commit } = useProfileEditor(gameId, selected, onSelect);

  useEffect(() => {
    setMode(null);
  }, [gameId, setMode]);
  const locked = disabled || !!applying;
  const missing = selected ? selected.modCount - selected.resolvedCount : 0;

  const onKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") void commit();
    if (e.key === "Escape") setMode(null);
  };

  const applySelected = async () => {
    if (!selected || applying) return;
    const result = await apply(gameId, selected.id);
    void refreshModsIfCurrent(gameId);
    if (!result) return;
    const changes = result.enabled.length + result.disabled.length;
    const parts: string[] = [];
    if (result.enabled.length) parts.push(t("mods.profiles.partOn", { count: result.enabled.length }));
    if (result.disabled.length) parts.push(t("mods.profiles.partOff", { count: result.disabled.length }));
    if (result.missing.length) parts.push(t("mods.profiles.partMissing", { count: result.missing.length }));
    const offNote = t("mods.profiles.offNote");
    const summary = canApply
      ? parts.join(", ") || t("mods.profiles.nothingNeeded")
      : parts.length
        ? t("mods.profiles.summaryWithNote", { summary: parts.join(", "), note: offNote })
        : offNote;
    push({
      type: result.failed.length ? "warning" : "success",
      title: changes
        ? t("mods.profiles.switchedTo", { name: selected.name })
        : t("mods.profiles.alreadyApplied", { name: selected.name }),
      text: result.failed.length
        ? result.failed.map((f) => `${f.folderName}: ${f.error}`).join("; ")
        : summary,
    });
  };

  const confirmDelete = () => {
    if (!selected) return;
    openConfirm({
      title: t("mods.profiles.deleteTitle", { name: selected.name }),
      message: t("mods.profiles.deleteMessage"),
      confirmLabel: t("mods.profiles.deleteConfirm"),
      danger: true,
      onConfirm: () => {
        void remove(gameId, selected.id);
      },
    });
  };

  return (
    <GroupBox divided className="self-start">
      <div className="flex items-center justify-between px-4 py-3">
        <span className="flex items-center gap-2 text-[12.5px] font-medium text-white/55">
          <Layers size={14} />
          {t("mods.profiles.title")}
        </span>
        <IconAction
          label={t("mods.profiles.newProfile")}
          disabled={locked || mode !== null}
          onClick={() => {
            setDraft("");
            setMode("new");
          }}
        >
          <Plus size={14} />
        </IconAction>
      </div>

      <div className="flex flex-col gap-[2px] p-[5px]">
        {!loaded ? (
          <div className="h-[34px] animate-pulse rounded-[7px] bg-white/[0.04]" />
        ) : (
          profiles.map((p) => {
            const isActive = p.id === activeId;
            const isSelected = selected?.id === p.id;
            if (mode === "rename" && isSelected) {
              return (
                <input
                  key={p.id}
                  ref={inputRef}
                  value={draft}
                  autoFocus
                  maxLength={60}
                  onChange={(e) => setDraft(e.target.value)}
                  onBlur={() => void commit()}
                  onKeyDown={onKey}
                  className={INPUT}
                />
              );
            }
            return (
              <button
                key={p.id}
                type="button"
                aria-pressed={isSelected}
                onClick={() => onSelect(p.id)}
                onDoubleClick={() => {
                  if (locked) return;
                  onSelect(p.id);
                  setDraft(p.name);
                  setMode("rename");
                }}
                className={`flex w-full items-center gap-2 rounded-[7px] px-[10px] py-[8px] text-left transition-colors ${
                  isSelected
                    ? "bg-white/[0.10] text-white"
                    : "text-white/70 hover:bg-white/[0.06] hover:text-white"
                }`}
              >
                <span className="min-w-0 flex-1 truncate text-[12.5px] font-medium">{p.name}</span>
                {isActive && (
                  <Hint tip={t("mods.profiles.active")} className="inline-flex shrink-0">
                    <span role="img" aria-label={t("mods.profiles.active")} className="text-(--accent-text)">
                      <Check size={13} strokeWidth={2.5} aria-hidden />
                    </span>
                  </Hint>
                )}
                <Hint
                  tip={t("mods.profiles.installedCount", { count: p.resolvedCount })}
                  className="inline-flex shrink-0"
                >
                  <span className="text-[11px] tabular-nums text-white/35">
                    {p.resolvedCount}
                    <span className="sr-only"> {t("mods.profiles.installedSuffix", { count: p.resolvedCount })}</span>
                  </span>
                </Hint>
              </button>
            );
          })
        )}
        {mode === "new" && (
          <input
            ref={inputRef}
            value={draft}
            autoFocus
            maxLength={60}
            placeholder={t("mods.profiles.namePlaceholder")}
            onChange={(e) => setDraft(e.target.value)}
            onBlur={() => void commit()}
            onKeyDown={onKey}
            className={INPUT}
          />
        )}
        {loaded && profiles.length === 0 && mode !== "new" && (
          <p className="px-[10px] py-2 text-[12px] text-white/40">{t("mods.profiles.none")}</p>
        )}
      </div>

      {selected && missing > 0 && (
        <p className="flex items-center gap-1.5 px-4 py-3 text-[12px] text-white/45">
          <X size={12} className="shrink-0" />
          {t("mods.profiles.missing", { count: missing })}
        </p>
      )}

      <div className="flex flex-wrap items-center gap-[6px] px-3 py-3">
        <ActionButton
          variant="accent"
          icon={<Play size={14} />}
          disabled={locked || !selected}
          onClick={() => void applySelected()}
        >
          {applying ? t("mods.profiles.applying") : t("common.apply")}
        </ActionButton>
        <span className="ml-auto flex gap-[6px]">
          <IconAction
            label={t("common.rename")}
            disabled={locked || !selected || mode !== null}
            onClick={() => {
              if (!selected) return;
              setDraft(selected.name);
              setMode("rename");
            }}
          >
            <Pencil size={14} />
          </IconAction>
          <IconAction
            label={t("common.duplicate")}
            disabled={locked || !selected}
            onClick={() => {
              if (selected) void duplicate(gameId, selected.id);
            }}
          >
            <Copy size={14} />
          </IconAction>
          <IconAction
            label={profiles.length > 1 ? t("common.delete") : t("mods.profiles.needOne")}
            danger
            disabled={locked || !selected || profiles.length <= 1}
            onClick={confirmDelete}
          >
            <Trash2 size={14} />
          </IconAction>
        </span>
      </div>
    </GroupBox>
  );
}
