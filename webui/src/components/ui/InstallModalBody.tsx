// ------------ Install Dialog ------------
// The contents of the install dialog, a two-pane layout: game art with the version and a live disk summary on the
// left; install folder, quality and voice options and the actions on the right. Games installed through Steam get
// a short two-step guide instead. Everything it shows is preloaded before it opens so it never resizes. Can also
// find a game that is already on this PC.
﻿import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  FolderOpen,
  HardDrive,
  Loader2,
  FolderSearch,
  ScanSearch,
  ExternalLink,
} from "lucide-react";
import { gameById, isManaged, type GameId, type ResourceQuality } from "../../data/games";
import { useNotificationStore } from "../../store/notifications";
import { useGamesStore } from "../../store/gamesStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { openExternal } from "../../lib/tauri";
import { fmtBytes } from "../../lib/format";
import type { TKey } from "../../lib/i18n";
import { useT } from "../../lib/useT";
import {
  checkLinkedInstall,
  linkedUncheckedText,
  useResourceQuality,
} from "../../lib/gameActions";
import { SegmentedControl, Toggle, ActionButton } from "./controls";
import { Hint } from "./Tooltip";
import { peekInstall, preloadInstall, refreshInstall, type InstallSnapshot } from "../../lib/installPreload";
import {
  getDiskSpace,
  getInstallPreview,
  getContentPacks,
  setContentPacks,
  selectInstallDirectory,
  startDownload,
  locateGameInstall,
  detectDefaultGameInstall,
  DEFAULT_VOICE_PACK,
  DEFAULT_RESOURCE_QUALITY,
  RESOURCE_QUALITY_OPTIONS,
  VOICE_PACK_OPTIONS,
  type ContentPack,
  type InstallPreview,
  type VoicePackLanguage,
} from "../../lib/ipc";

const INVALID_PATH_CHARS = /[<>:"/\\|?*]/g;

const SECTION_LABEL = "text-[11px] font-medium uppercase tracking-[0.6px] text-white/55";
const CARD = "rounded-ui border border-white/[0.08] bg-white/[0.04]";
const TEXT_LINK =
  "flex items-center gap-[5px] text-white/75 transition-colors duration-150 hover:text-white disabled:cursor-not-allowed disabled:opacity-50";

const QUALITY_DESCRIPTIONS: Record<ResourceQuality, TKey> = {
  sd: "install.quality.sd",
  hd: "install.quality.hd",
  uhd: "install.quality.uhd",
};

function folderNameFor(name: string): string {
  return name.replace(INVALID_PATH_CHARS, "").trim().replace(/[.\s]+$/, "");
}

function withGameFolder(picked: string, folder: string): string {
  if (!folder) return picked;
  const trimmed = picked.replace(/[\\/]+$/, "");
  const last = trimmed.split(/[\\/]/).pop() ?? "";
  if (last.toLowerCase() === folder.toLowerCase()) return trimmed;
  return `${trimmed}\\${folder}`;
}

function leafOf(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("\\"), trimmed.lastIndexOf("/"));
  return cut >= 0 ? trimmed.slice(cut + 1) : trimmed;
}

function parentOf(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.slice(0, trimmed.length - leafOf(trimmed).length);
}

function driveOf(path: string): string {
  return /^[A-Za-z]:/.test(path) ? path.slice(0, 2).toUpperCase() : "";
}

function fmtSize(bytes: number | null, approximate: boolean): string {
  if (bytes === null || bytes <= 0) return "—";
  return `${approximate ? "~" : ""}${fmtBytes(bytes)}`;
}

function previewReason(error: string | null, t: ReturnType<typeof useT>): string {
  if (error && error.length <= 100 && /[.!]$/.test(error) && !/https?:|[{}]|HTTP|status|error/i.test(error))
    return error;
  return t("install.details.unreachable");
}

function Skeleton({ width }: { width: number }) {
  return <span className="h-[11px] animate-pulse rounded-full bg-white/10" style={{ width }} />;
}

function SummaryLine({
  label,
  value,
  pending,
}: {
  label: string;
  value: string;
  pending: boolean;
}) {
  return (
    <div className="flex items-center justify-between">
      <span className="text-white/55">{label}</span>
      {pending ? <Skeleton width={56} /> : <span className="font-medium">{value}</span>}
    </div>
  );
}

function SpaceBar({ usedPct, installPct }: { usedPct: number; installPct: number }) {
  return (
    <div className="flex h-[6px] overflow-hidden rounded-full bg-white/10">
      <div className="bg-white/30" style={{ width: `${usedPct}%` }} />
      <div
        className="transition-[width] duration-200"
        style={{
          width: `${installPct}%`,
          background: "linear-gradient(135deg, var(--accent-a), var(--accent-b))",
        }}
      />
    </div>
  );
}

function StepRow({
  step,
  title,
  text,
  onClick,
}: {
  step: number;
  title: string;
  text: string;
  onClick?: () => void;
}) {
  const body = (
    <>
      <span className="grid size-[24px] shrink-0 place-items-center rounded-full bg-white/10 text-[12px] font-semibold">
        {step}
      </span>
      <div className="min-w-0 flex-1">
        <div className="text-[13px] font-medium">{title}</div>
        <div className="text-[12px] text-white/55">{text}</div>
      </div>
    </>
  );
  const base = `flex items-center gap-[12px] px-[12px] py-[11px] text-left ${CARD}`;
  if (!onClick) return <div className={base}>{body}</div>;
  return (
    <button
      type="button"
      onClick={onClick}
      className={`${base} transition duration-150 hover:bg-white/[0.08] active:scale-[0.99]`}
    >
      {body}
      <ExternalLink size={14} className="shrink-0 text-white/55" />
    </button>
  );
}

export function InstallModalBody({ gameId, onClose }: { gameId: GameId; onClose: () => void }) {
  const t = useT();
  const game = gameById(gameId);
  const managed = isManaged(gameId);
  const push = useNotificationStore((s) => s.push);
  const [seed] = useState(() => (managed ? peekInstall(gameId) : null));
  const [path, setPath] = useState(seed?.path ?? "");
  const [maxRootLength, setMaxRootLength] = useState(seed?.maxRootLength ?? 0);
  const [folderName, setFolderName] = useState(seed?.folderName ?? "");
  const [free, setFree] = useState(seed?.free ?? 0);
  const [totalSpace, setTotalSpace] = useState(seed?.total ?? 0);
  const [loading, setLoading] = useState(managed && !seed);
  const [starting, setStarting] = useState(false);
  const [detecting, setDetecting] = useState(false);
  const [preview, setPreview] = useState<InstallPreview | null>(seed?.preview ?? null);
  const [previewFailed, setPreviewFailed] = useState(seed?.previewFailed ?? false);
  const [previewError, setPreviewError] = useState<string | null>(seed?.previewError ?? null);
  const [refreshing, setRefreshing] = useState(false);
  const [packs, setPacks] = useState<ContentPack[] | null>(seed?.packs ?? null);
  const [packsFailed, setPacksFailed] = useState(seed?.packsFailed ?? false);
  const packsRef = useRef<ContentPack[] | null>(seed?.packs ?? null);
  const packSaves = useRef<Promise<void>>(Promise.resolve());
  const packSavesPending = useRef(0);
  const changed = useRef(false);
  const voice = useCustomizationStore((s) => s.voicePack[gameId]) ?? DEFAULT_VOICE_PACK;
  const setVoicePack = useCustomizationStore((s) => s.setVoicePack);
  const resourceQuality = useResourceQuality(
    gameId,
    { installed: false, jobActive: false },
    { sizes: true, initial: seed?.quality ?? null },
  );
  const [qualityBusy, setQualityBusy] = useState(false);
  const previewRequest = useRef(0);
  const packsRequest = useRef(0);

  const rootLength = path.replace(/[\\/]+$/, "").length;
  const tooDeep = maxRootLength > 0 && rootLength > maxRootLength;

  const applySnapshot = useCallback((snap: InstallSnapshot) => {
    setPath(snap.path);
    setMaxRootLength(snap.maxRootLength);
    setFolderName(snap.folderName);
    setFree(snap.free);
    setTotalSpace(snap.total);
    setPreview(snap.preview);
    setPreviewFailed(snap.previewFailed);
    setPreviewError(snap.previewError);
    packsRef.current = snap.packs;
    setPacks(snap.packs);
    setPacksFailed(snap.packsFailed);
    setLoading(false);
  }, []);

  useEffect(() => {
    if (!seed?.path) return;
    let alive = true;
    void getDiskSpace(seed.path).then((disk) => {
      if (!alive || !disk) return;
      setFree(disk.free);
      setTotalSpace(disk.total);
    });
    return () => {
      alive = false;
    };
  }, [seed]);

  useEffect(() => {
    if (!managed || seed) return;
    let alive = true;
    preloadInstall(gameId)
      .then((snap) => {
        if (alive) applySnapshot(snap);
      })
      .catch(() => {
        if (!alive) return;
        setPreviewFailed(true);
        setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [gameId, managed, seed, applySnapshot]);

  useEffect(
    () => () => {
      if (changed.current) refreshInstall(gameId);
    },
    [gameId],
  );

  const loadPreview = useCallback(
    async (fromBlank = false) => {
      const token = ++previewRequest.current;
      if (fromBlank) {
        setPreview(null);
        setPreviewFailed(false);
        setPreviewError(null);
      }
      setRefreshing(true);
      const { preview: p, error } = await getInstallPreview(gameId);
      if (token !== previewRequest.current) return;
      setRefreshing(false);
      if (p && !p.notSupported) {
        setPreview(p);
        setPreviewFailed(false);
        setPreviewError(null);
      } else {
        setPreview(null);
        setPreviewError(error);
        setPreviewFailed(true);
      }
    },
    [gameId],
  );

  const loadPacks = useCallback(async () => {
    if (!game.contentPackChoice) return;
    const token = ++packsRequest.current;
    setPacksFailed(false);
    const r = await getContentPacks(gameId);
    if (token !== packsRequest.current || !r.supported) return;
    if (r.error) setPacksFailed(true);
    else {
      packsRef.current = r.packs;
      setPacks(r.packs);
    }
  }, [gameId, game.contentPackChoice]);

  const retryDetails = async () => {
    await loadPreview(true);
    if (!packsRef.current) await loadPacks();
  };

  const togglePack = (tag: string, on: boolean) => {
    const current = packsRef.current;
    if (!current) return;
    const next = current.map((p) => (p.tag === tag ? { ...p, selected: on } : p));
    packsRef.current = next;
    setPacks(next);
    changed.current = true;
    const chosen = next.filter((p) => p.selected).map((p) => p.tag);
    packSavesPending.current += 1;
    packSaves.current = packSaves.current.then(async () => {
      const saved = await setContentPacks(gameId, chosen.length === next.length ? null : chosen);
      packSavesPending.current -= 1;
      if (packSavesPending.current > 0) return;
      if (!saved) await loadPacks();
      await loadPreview();
    });
  };

  const pickVoice = async (language: VoicePackLanguage) => {
    if (language === voice) return;
    changed.current = true;
    await setVoicePack(gameId, language);
    await loadPreview();
  };

  const pickQuality = async (quality: ResourceQuality) => {
    if (quality === resourceQuality.quality || qualityBusy) return;
    changed.current = true;
    setQualityBusy(true);
    try {
      await resourceQuality.setQuality(quality);
      await loadPreview();
    } finally {
      setQualityBusy(false);
    }
  };

  const pickFolder = async () => {
    const p = await selectInstallDirectory();
    if (!p) return;
    const target = withGameFolder(p, folderName || folderNameFor(game.name));
    setPath(target);
    const disk = await getDiskSpace(target);
    if (disk) {
      setFree(disk.free);
      setTotalSpace(disk.total);
    }
  };

  const begin = () => {
    if (!path || starting || tooDeep) return;
    setStarting(true);
    void startDownload(path, gameId, () =>
      push({
        type: "progress",
        title: t("install.toast.installingTitle", { game: game.name }),
        text: t("install.toast.installingText"),
      }),
    );
    onClose();
  };

  const linkGame = async (title: string, text: string) => {
    void useCustomizationStore.getState().hydrate();
    const installed = useGamesStore.getState().installed;
    if (!installed.includes(gameId)) useGamesStore.getState().setInstalled([...installed, gameId]);
    const checking = managed && checkLinkedInstall(gameId);
    push({
      type: "success",
      title,
      text: checking ? text : managed ? linkedUncheckedText() : t("install.toast.linked"),
    });
    onClose();
  };

  const locate = async () => {
    const r = await locateGameInstall(gameId);
    if (r.ok) {
      await linkGame(
        t("install.toast.locatedTitle", { game: game.name }),
        t("install.toast.locatedText"),
      );
    } else if (r.error) {
      push({ type: "warning", title: t("install.toast.locateFailed", { game: game.name }), text: r.error });
    }
  };

  const detectDefault = async () => {
    if (detecting) return;
    setDetecting(true);
    try {
      const r = await detectDefaultGameInstall(gameId);
      if (r.ok) {
        await linkGame(
          t("install.toast.foundTitle", { game: game.name }),
          t("install.toast.foundText"),
        );
      } else if (r.error) {
        push({ type: "warning", title: t("install.toast.findFailed", { game: game.name }), text: r.error });
      }
    } finally {
      setDetecting(false);
    }
  };

  const betaNote = (note: string) => (
    <div className="flex items-start gap-[9px] rounded-ui border border-warning/30 bg-warning/[0.08] px-[12px] py-[10px]">
      <AlertTriangle size={15} className="mt-[2px] shrink-0 text-warning" />
      <p className="text-[12.5px] leading-[1.5] text-white/85">{note}</p>
    </div>
  );

  const installBytes = preview?.installBytes ?? preview?.downloadBytes ?? null;
  const requiredBytes = preview?.requiredBytes ?? installBytes;
  const shortOnSpace = requiredBytes !== null && free > 0 && free < requiredBytes;
  const includes = preview?.parts.map((p) => p.label).join(" · ") ?? "";
  const summaryPending = !preview && !previewFailed;
  const freeAfter = installBytes !== null && free > 0 ? Math.max(0, free - installBytes) : null;
  const usedPct = totalSpace > 0 ? ((totalSpace - free) / totalSpace) * 100 : 0;
  const installPct = totalSpace > 0 && installBytes !== null ? (installBytes / totalSpace) * 100 : 0;
  const drive = driveOf(path);
  const driveText =
    free > 0 && totalSpace > 0
      ? drive
        ? t("install.drive.freeOnDrive", { drive, free: fmtBytes(free), total: fmtBytes(totalSpace) })
        : t("install.drive.free", { free: fmtBytes(free), total: fmtBytes(totalSpace) })
      : t("install.drive.checking");

  const identity = (
    <div className="relative flex flex-col gap-[10px]">
      <img
        src={game.icon}
        alt=""
        className="size-[44px] rounded-ui object-cover shadow-[0_6px_16px_rgba(0,0,0,0.5)]"
      />
      <div>
        <div className="font-display text-[19px] font-semibold leading-[1.2]">{game.name}</div>
        <div className="mt-[3px] text-[12px] text-white/60">
          {!managed ? (
            t("install.distributedSteam")
          ) : preview?.version ? (
            t("install.version", { version: preview.version })
          ) : summaryPending ? (
            <Skeleton width={72} />
          ) : (
            t("install.versionUnavailable")
          )}
        </div>
      </div>
    </div>
  );

  const sidebar = (
    <aside
      className="relative flex w-[250px] shrink-0 flex-col justify-end gap-[16px] p-[20px]"
      style={{
        backgroundImage: `url(${game.wallpaperStatic}), ${game.wallpaperFallback}`,
        backgroundSize: "cover",
        backgroundPosition: "center",
      }}
    >
      <div
        aria-hidden
        className="absolute inset-0"
        style={{
          background:
            "linear-gradient(180deg, rgba(10,10,14,0.1) 0%, rgba(10,10,14,0.55) 45%, rgba(10,10,14,0.95) 100%)",
        }}
      />
      {identity}
      {managed && (
        <div
          className={`relative flex flex-col gap-[9px] border-t border-white/[0.12] pt-[14px] text-[12.5px] tabular-nums transition-opacity duration-150 ${refreshing ? "opacity-60" : ""}`}
        >
          <SummaryLine
            label={t("install.summary.download")}
            value={preview ? fmtSize(preview.downloadBytes, preview.approximate) : "—"}
            pending={summaryPending}
          />
          {(!preview || preview.installBytes !== null) && (
            <SummaryLine
              label={t("install.summary.sizeOnDisk")}
              value={preview ? fmtSize(preview.installBytes, preview.approximate) : "—"}
              pending={summaryPending}
            />
          )}
          <SummaryLine
            label={t("install.summary.freeAfter")}
            value={freeAfter !== null ? fmtBytes(freeAfter) : "—"}
            pending={summaryPending}
          />
          <SpaceBar usedPct={usedPct} installPct={installPct} />
          <div
            className={`flex items-center gap-[6px] text-[11.5px] ${shortOnSpace ? "text-warning" : "text-white/50"}`}
          >
            <HardDrive size={12} className="shrink-0" />
            <span className="min-w-0">{driveText}</span>
          </div>
        </div>
      )}
    </aside>
  );

  if (!managed) {
    const steamUrl = game.installHelpUrl;
    return (
      <div className="flex">
        {sidebar}
        <div className="flex min-w-0 flex-1 flex-col gap-[16px] p-[22px]">
          <h2 className="text-[16px] font-semibold leading-[1.25]">
          {t("install.title", { game: game.name })}
        </h2>
          <p className="text-[13px] leading-[1.55] text-white/65">
            {game.betaNote ??
              t("install.steam.body", { game: game.name })}
          </p>
          <div className="flex flex-col gap-[8px]">
            <StepRow
              step={1}
              title={t("install.steam.stepInstall")}
              text={t("install.steam.stepInstallText", { game: game.name })}
              onClick={
                steamUrl
                  ? () => {
                      void openExternal(steamUrl);
                    }
                  : undefined
              }
            />
            <StepRow
              step={2}
              title={t("install.locateInstall")}
              text={t("install.steam.stepLocateText")}
            />
          </div>
          <div className="mt-[4px] flex justify-end gap-[10px]">
            <ActionButton onClick={onClose}>{t("common.cancel")}</ActionButton>
            <ActionButton variant="accent" onClick={() => void locate()}>
              {t("install.locateInstall")}
            </ActionButton>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex">
      {sidebar}
      <div className="flex min-w-0 flex-1 flex-col gap-[16px] p-[22px]">
        <h2 className="text-[16px] font-semibold leading-[1.25]">
          {t("install.title", { game: game.name })}
        </h2>

        <div className="flex flex-col gap-[7px]">
          <div className={SECTION_LABEL}>{t("install.folder.label")}</div>
          <div className="flex items-center gap-[8px]">
            <Hint tip={path || undefined} className="flex min-w-0 flex-1">
              <div className="flex min-w-0 flex-1 items-center rounded-ui border border-white/[0.08] bg-black/30 px-[12px] py-[9px] text-[12.5px]">
                {loading || !path ? (
                  <span className="truncate text-white/80">
                    {loading ? t("install.folder.resolving") : t("install.folder.none")}
                  </span>
                ) : (
                  <>
                    <span className="truncate text-white/45">{parentOf(path)}</span>
                    <span className="shrink-0 text-white/90">{leafOf(path)}</span>
                  </>
                )}
              </div>
            </Hint>
            <button
              onClick={() => void pickFolder()}
              className={`flex shrink-0 items-center gap-[7px] px-[12px] py-[9px] text-[12.5px] font-medium transition duration-150 active:scale-[0.97] hover:bg-white/[0.08] ${CARD}`}
            >
              <FolderOpen size={14} /> {t("install.folder.browse")}
            </button>
          </div>
          <div className="flex flex-wrap items-center gap-x-[14px] gap-y-[4px] text-[12px] text-white/50">
            <span>{t("install.folder.alreadyInstalled")}</span>
            <button onClick={() => void locate()} className={TEXT_LINK}>
              <FolderSearch size={13} /> {t("install.folder.locateExisting")}
            </button>
            <button disabled={detecting} onClick={() => void detectDefault()} className={TEXT_LINK}>
              {detecting ? <Loader2 size={13} className="animate-spin" /> : <ScanSearch size={13} />}
              {t("install.folder.checkDefault")}
            </button>
          </div>
        </div>

        {game.betaNote && betaNote(game.betaNote)}

        {game.resourceQualityChoice && (
          <div className="flex flex-col gap-[7px]">
            <div className={SECTION_LABEL}>{t("install.quality.title")}</div>
            <div role="radiogroup" aria-label={t("install.quality.title")} className="grid grid-cols-3 gap-[8px]">
              {RESOURCE_QUALITY_OPTIONS.map(({ value, label }) => {
                const active = value === resourceQuality.quality;
                const size = resourceQuality.sizes?.[value];
                const share =
                  size && free > 0 ? Math.min(100, (size.installBytes / free) * 100) : 0;
                return (
                  <button
                    key={value}
                    role="radio"
                    aria-checked={active}
                    disabled={qualityBusy}
                    onClick={() => void pickQuality(value)}
                    className={`flex flex-col gap-[2px] rounded-ui border px-[12px] pb-[12px] pt-[11px] text-left transition duration-150 active:scale-[0.98] disabled:cursor-wait ${
                      active
                        ? "border-(--accent-a) bg-white/[0.09]"
                        : "border-white/[0.08] bg-white/[0.04] hover:bg-white/[0.08]"
                    }`}
                  >
                    <div className="flex items-center justify-between gap-[6px]">
                      <span className="text-[14px] font-semibold">{label}</span>
                      {value === DEFAULT_RESOURCE_QUALITY && (
                        <span className="rounded-full bg-white/[0.12] px-[7px] py-px text-[10.5px] font-medium text-white/80">
                          {t("install.quality.default")}
                        </span>
                      )}
                    </div>
                    <div className="min-h-[34px] text-[11.5px] leading-[1.4] text-white/55">
                      {t(QUALITY_DESCRIPTIONS[value])}
                    </div>
                    {size || resourceQuality.loaded ? (
                      <>
                        <div className="mt-[6px] text-[15px] font-semibold tabular-nums">
                          {size ? fmtBytes(size.installBytes) : "—"}
                        </div>
                        <div className="mt-[5px] h-[4px] overflow-hidden rounded-full bg-white/10">
                          <div
                            className="h-full rounded-full"
                            style={{
                              width: `${share}%`,
                              background: active
                                ? "linear-gradient(135deg, var(--accent-a), var(--accent-b))"
                                : "rgba(255,255,255,0.35)",
                            }}
                          />
                        </div>
                        <div className="mt-[3px] text-[11px] tabular-nums text-white/50">
                          {size ? t("install.quality.download", { size: fmtBytes(size.downloadBytes) }) : "—"}
                        </div>
                      </>
                    ) : (
                      <>
                        <div className="mt-[6px] h-[15px] w-[56px] animate-pulse rounded-full bg-white/10" />
                        <div className="mt-[5px] h-[4px] rounded-full bg-white/10" />
                        <div className="mt-[3px] h-[9px] w-[72px] animate-pulse rounded-full bg-white/10" />
                      </>
                    )}
                  </button>
                );
              })}
            </div>
            <div className="text-[11.5px] leading-[1.5] text-white/50">
              {t("install.quality.note")}
            </div>
          </div>
        )}

        {game.voicePackChoice && (
          <div className={`flex items-center justify-between gap-[12px] px-[12px] py-[9px] ${CARD}`}>
            <div>
              <div className="text-[12.5px] font-medium">{t("install.voice.title")}</div>
              <div className="text-[11.5px] text-white/50">{t("install.voice.hint")}</div>
            </div>
            <SegmentedControl
              ariaLabel={t("install.voice.aria")}
              options={VOICE_PACK_OPTIONS}
              value={voice}
              onChange={(v) => void pickVoice(v as VoicePackLanguage)}
            />
          </div>
        )}

        {packs && packs.length > 0 && (
          <div className="flex flex-col gap-[7px]">
            <div className="flex items-baseline justify-between">
              <span className={SECTION_LABEL}>{t("common.voicePacks")}</span>
              <span className="text-[11.5px] text-white/50">{t("install.voice.changeLater")}</span>
            </div>
            <div className="grid grid-cols-2 gap-[8px]">
              {packs.map((pack) => (
                <div
                  key={pack.tag}
                  className={`flex items-center justify-between gap-[8px] px-[12px] py-[8px] ${CARD}`}
                >
                  <div className="min-w-0">
                    <div className="truncate text-[12.5px] font-medium">{pack.language ?? pack.tag}</div>
                    <div className="truncate text-[11px] tabular-nums text-white/50">
                      {pack.language ? `${pack.tag} · ${fmtBytes(pack.bytes)}` : fmtBytes(pack.bytes)}
                    </div>
                  </div>
                  <Toggle
                    checked={pack.selected}
                    ariaLabel={pack.language ?? pack.tag}
                    onChange={(v) => togglePack(pack.tag, v)}
                  />
                </div>
              ))}
            </div>
          </div>
        )}
        {game.contentPackChoice && !packs && !packsFailed && (
          <div className="flex flex-col gap-[7px]">
            <div className="flex items-baseline justify-between">
              <span className={SECTION_LABEL}>{t("common.voicePacks")}</span>
              <span className="text-[11.5px] text-white/50">{t("install.voice.changeLater")}</span>
            </div>
            <div className="grid grid-cols-2 gap-[8px]">
              {[0, 1, 2, 3].map((i) => (
                <div key={i} className={`h-[50px] animate-pulse ${CARD}`} />
              ))}
            </div>
          </div>
        )}
        {!packs && packsFailed && (
          <div className={`flex items-center justify-between gap-[10px] px-[12px] py-[8px] ${CARD}`}>
            <span className="text-[12.5px] text-white/50">{t("install.voice.loadFailed")}</span>
            <button
              onClick={() => void loadPacks()}
              className="shrink-0 text-[12.5px] font-medium text-white/60 underline-offset-2 transition-colors duration-150 hover:text-white hover:underline"
            >
              {t("common.retry")}
            </button>
          </div>
        )}

        {summaryPending && (
          <div className="py-[3px]">
            <Skeleton width={180} />
          </div>
        )}
        {(includes || previewFailed) && (
          <div className="text-[11.5px] leading-[1.5] text-white/55">
            {previewFailed ? (
              <>
                {t("install.details.unavailable", { reason: previewReason(previewError, t) })}{" "}
                <button
                  onClick={() => void retryDetails()}
                  className="font-medium text-white/60 underline-offset-2 transition-colors duration-150 hover:text-white hover:underline"
                >
                  {t("common.retry")}
                </button>
              </>
            ) : (
              t("install.details.includes", { parts: includes })
            )}
          </div>
        )}

        {shortOnSpace && (
          <div className="flex items-start gap-[9px] rounded-ui border border-warning/30 bg-warning/[0.08] px-[12px] py-[10px]">
            <AlertTriangle size={15} className="mt-[1px] shrink-0 text-warning" />
            <p className="text-[12.5px] leading-[1.5] text-white/85">
              {t("install.shortOnSpace", { required: fmtBytes(requiredBytes), free: fmtBytes(free) })}
            </p>
          </div>
        )}
        {tooDeep && (
          <div className="flex items-start gap-[9px] rounded-ui border border-danger/30 bg-danger/[0.08] px-[12px] py-[10px]">
            <AlertTriangle size={15} className="mt-[1px] shrink-0 text-danger-soft" />
            <p className="text-[12.5px] leading-[1.5] text-white/85">
              {t("install.tooDeep", {
                count: rootLength - maxRootLength,
                game: game.name,
                max: maxRootLength,
                root: "C:\\",
              })}
            </p>
          </div>
        )}

        <div className="flex justify-end gap-[10px]">
          <ActionButton onClick={onClose}>{t("common.cancel")}</ActionButton>
          <ActionButton
            variant="accent"
            disabled={!path || loading || starting || tooDeep}
            onClick={begin}
            icon={starting ? <Loader2 size={14} className="animate-spin" /> : undefined}
          >
            {t("common.install")}
          </ActionButton>
        </div>
      </div>
    </div>
  );
}
