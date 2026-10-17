// ------------ Overlay Captures ------------
// The Captures tab of the overlay: screenshot and recording settings, the record state, and a browsable
// list of your screenshots and clips grouped by game, opened in the gallery's viewer.
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, X } from "lucide-react";
import {
  chooseCaptureFolder,
  deleteCapture,
  getCaptureSupport,
  getOverlayConfig,
  getOverlayStatus,
  listCaptures,
  openCaptureFolder,
  resetCaptureFolder,
  toggledAudioTracks,
  type MediaItem,
  type OverlayAudioTrack,
  type OverlayRecEstimate,
} from "../lib/ipc";
import { rpc } from "../lib/rpc";
import { useSettingsStore } from "../store/settingsStore";
import { fmtBytes, fmtDuration, fmtRelTime } from "../lib/format";
import { useNow, useTauriEvents } from "../lib/hooks";
import { Keys } from "../lib/hotkeys";
import { Chip, SegmentedControl, Toggle } from "../components/ui/controls";
import { GroupBox, GroupLabel } from "../components/ui/panels";
import { Slider } from "../components/ui/Slider";
import { LazyClip, tileMedia, useAutoClear } from "../components/gallery/media";
import { GAMES } from "../data/games";
import { useCustomizationStore } from "../store/customizationStore";
import { iconFallback, iconSrcFor } from "../lib/customMedia";
import type { TKey } from "../lib/i18n";
import { Trans, useT } from "../lib/useT";

const UNGROUPED = "__ungrouped";
const PAGE = 60;
const PENDING_TIMEOUT_MS = 15_000;

const FOLDER_BUTTON =
  "rounded-[8px] border border-white/15 bg-white/[0.06] px-[13px] py-[7px] text-[12px] font-medium hover:bg-white/10";

const csv = (value: string) =>
  value
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

const loadViewer = () => import("../components/gallery/MediaViewer");
const MediaViewer = lazy(() => loadViewer().then((mod) => ({ default: mod.MediaViewer })));

type GameGroup = { id: string; name: string; known: boolean; items: MediaItem[] };

type Notice = { text: string; warning?: boolean };

const FILTER_LABEL: Record<"All" | "Screenshots" | "Clips", TKey> = {
  All: "overlay.captures.filterAll",
  Screenshots: "common.screenshots",
  Clips: "overlay.captures.filterClips",
};

function groupByGame(list: MediaItem[]): GameGroup[] {
  const byGame = new Map<string, MediaItem[]>();
  for (const capture of list) {
    const key = capture.gameId ?? UNGROUPED;
    const items = byGame.get(key);
    if (items) items.push(capture);
    else byGame.set(key, [capture]);
  }

  const ordered: GameGroup[] = [];
  for (const game of GAMES) {
    const items = byGame.get(game.id);
    if (items?.length) {
      ordered.push({ id: game.id, name: game.name, known: true, items });
    }
  }
  for (const [id, items] of byGame) {
    if (id !== UNGROUPED && !GAMES.some((game) => game.id === id)) {
      ordered.push({ id, name: id, known: false, items });
    }
  }
  const rest = byGame.get(UNGROUPED);
  if (rest?.length) ordered.push({ id: UNGROUPED, name: "", known: false, items: rest });
  return ordered;
}

export function OverlayCaptures() {
  const t = useT();
  const values = useSettingsStore((s) => s.values);
  const set = useSettingsStore((s) => s.set);
  const icons = useCustomizationStore((s) => s.gameIcons);
  const hydrateIcons = useCustomizationStore((s) => s.hydrate);
  const [captures, setCaptures] = useState<MediaItem[]>([]);
  const [supported, setSupported] = useState<boolean | null>(null);
  const [recordingSupported, setRecordingSupported] = useState(true);
  const [folder, setFolder] = useState("");
  const [folderIsDefault, setFolderIsDefault] = useState(true);
  const [tracks, setTracks] = useState<OverlayAudioTrack[]>([]);
  const [filter, setFilter] = useState<"All" | "Screenshots" | "Clips">("All");
  const [error, setError] = useState<Notice | null>(null);
  const [viewerError, setViewerError] = useState<string | null>(null);
  const [viewing, setViewing] = useState<string | null>(null);
  const viewedIndex = useRef(0);
  const viewingRef = useRef<string | null>(null);
  viewingRef.current = viewing;
  const [recording, setRecording] = useState(false);
  const [pending, setPending] = useState<"starting" | "stopping" | null>(null);
  const [since, setSince] = useState(0);
  const [shotHotkey, setShotHotkey] = useState("");
  const [recHotkey, setRecHotkey] = useState("");
  const [discordClients, setDiscordClients] = useState<{ id: string; label: string }[]>([]);
  const [estimate, setEstimate] = useState<OverlayRecEstimate | null>(null);
  const [shooting, setShooting] = useState(false);
  const [armed, setArmed] = useState<string | null>(null);
  const [limit, setLimit] = useState(PAGE);
  const [qualityDraft, setQualityDraft] = useState<number | null>(null);
  const drawerOpen = useRef(true);
  const now = useNow();

  useAutoClear(armed, setArmed, 3000);

  const load = useCallback(async () => {
    setCaptures((await listCaptures()).items);
  }, []);

  const loadConfig = useCallback(async () => {
    const config = await getOverlayConfig();
    setFolder(config?.captureFolder ?? "");
    setFolderIsDefault(config?.captureFolderIsDefault ?? true);
    setTracks(config?.audioTracks ?? []);
    setEstimate(config?.recEstimate ?? null);
    const hotkeys = config?.hotkeys ?? [];
    setShotHotkey(hotkeys.find((h) => h.id === "overlayShotHotkey")?.accelerator ?? "");
    setRecHotkey(hotkeys.find((h) => h.id === "overlayRecHotkey")?.accelerator ?? "");
    const discord = await rpc<{ clients?: { id: string; label: string }[] }>(
      "discord-audio-clients",
    );
    setDiscordClients(discord?.clients ?? []);
  }, []);

  const loadRecording = useCallback(async () => {
    const status = await getOverlayStatus();
    setRecording(status?.recording === true);
    setSince(status?.recordingSinceMs ?? 0);
  }, []);

  useEffect(() => {
    void loadViewer();
    void getCaptureSupport().then((s) => {
      setSupported(s.supported);
      setRecordingSupported(s.recordingSupported);
    });
    void load();
    void loadConfig();
    void loadRecording();
    void hydrateIcons();
  }, [load, loadConfig, loadRecording, hydrateIcons]);

  const loadIfOpen = () => {
    if (drawerOpen.current && document.visibilityState !== "hidden") void load();
  };

  useTauriEvents({
    "overlay-capture": loadIfOpen,
    "overlay-capture-deleted": loadIfOpen,
    "overlay-closed": () => {
      drawerOpen.current = false;
    },
    "overlay-opened": () => {
      drawerOpen.current = true;
      void load();
      void loadConfig();
      void loadRecording();
      void hydrateIcons();
    },
    "overlay-record-starting": () => setPending("starting"),
    "overlay-record-started": (payload) => {
      const detail = payload as { audioWarnings?: string[] } | undefined;
      const warnings = detail?.audioWarnings ?? [];
      setError(warnings.length ? { text: warnings.join(" "), warning: true } : null);
      setPending(null);
      void loadRecording();
    },
    "overlay-record-stopped": (payload) => {
      const detail = payload as { reason?: string | null } | undefined;
      if (detail?.reason) setError({ text: detail.reason, warning: true });
      setPending(null);
      void loadRecording();
      loadIfOpen();
    },
    "overlay-record-failed": (payload) => {
      const detail = payload as { error?: string } | undefined;
      setError({ text: detail?.error ?? t("overlay.captures.recordingFailed") });
      setPending(null);
      void loadRecording();
    },
    "overlay-record-cancelled": () => {
      setPending(null);
      void loadRecording();
    },
  });

  useEffect(() => {
    if (!pending) return;
    const timer = window.setTimeout(() => {
      setPending(null);
      void loadRecording();
    }, PENDING_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [pending, loadRecording]);

  const closeViewer = useCallback(() => {
    setViewing(null);
    setViewerError(null);
  }, []);

  useAutoClear(viewerError, setViewerError, 5000);

  const selectedTracks = csv(values.overlayAudioTracks ?? "desktop,mic");
  const discordPicked = csv(values.overlayAudioDiscord ?? "");

  const toggleTrack = (id: string) => set("overlayAudioTracks", toggledAudioTracks(selectedTracks, id).join(","));

  const toggleDiscordClient = (id: string) => {
    const all = discordClients.map((c) => c.id);
    const effective = discordPicked.length ? discordPicked : all;
    const next = effective.includes(id)
      ? effective.filter((x) => x !== id)
      : [...effective, id];
    if (next.length === 0) return;
    set("overlayAudioDiscord", next.length === all.length ? "" : next.join(","));
  };

  const discordChosen = (id: string) => discordPicked.length === 0 || discordPicked.includes(id);

  const reloadFolder = () => {
    void loadConfig();
    void load();
  };

  const remove = async (path: string) => {
    const result = await deleteCapture(path);
    if (!result.ok) {
      const text = result.error ?? t("ipc.captures.deleteFailed");
      if (viewingRef.current) setViewerError(text);
      else setError({ text });
      return;
    }
    if (viewingRef.current === path) {
      const at = inViewerOrder.findIndex((c) => c.path === path);
      const next =
        (at >= 0 ? inViewerOrder[at + 1] ?? inViewerOrder[at - 1] : undefined) ??
        inViewerOrder.find((c) => c.path !== path);
      setViewing(next ? next.path : null);
    }
    void load();
  };

  const askRemove = (path: string) => {
    if (armed !== path) {
      setArmed(path);
      return;
    }
    setArmed(null);
    void remove(path);
  };

  const quality = Number.parseInt(values.overlayShotQuality, 10) || 90;
  const res = values.overlayRecRes;
  const fps = values.overlayRecFps;
  const codec = values.overlayRecCodec;
  const recQuality = values.overlayRecQuality;

  const takeScreenshot = async () => {
    if (shooting) return;
    setShooting(true);
    setError(null);
    try {
      const result = await rpc<{ success?: boolean; error?: string }>("overlay-screenshot");
      if (result?.success === false) {
        setError({ text: result.error ?? t("overlay.captures.screenshotFailed") });
      }
    } catch (e) {
      setError({
        text: e instanceof Error ? e.message : String(e || t("overlay.captures.screenshotFailed")),
      });
    } finally {
      setShooting(false);
    }
  };

  const toggleRecording = async () => {
    if (pending) return;
    setPending(recording ? "stopping" : "starting");
    try {
      const result = await rpc<{ success?: boolean; error?: string }>("overlay-record");
      if (result?.success === false) {
        setPending(null);
        setError({ text: result.error ?? t("overlay.captures.recordingStartFailed") });
      }
    } catch (e) {
      setPending(null);
      setError({
        text: e instanceof Error ? e.message : String(e || t("overlay.captures.recordingStartFailed")),
      });
    }
  };

  const megabytesPerMinute = estimateMegabytesPerMinute(
    estimate,
    res,
    fps,
    codec,
    recQuality,
    selectedTracks.length > 0,
  );

  const shown = useMemo(
    () =>
      captures.filter((c) =>
        filter === "All" ? true : filter === "Clips" ? c.kind === "clip" : c.kind !== "clip",
      ),
    [captures, filter],
  );
  const stored = captures.reduce((total, c) => total + c.sizeBytes, 0);
  const paged = useMemo(() => shown.slice(0, limit), [shown, limit]);
  const totals = useMemo(() => {
    const counts = new Map<string, number>();
    for (const capture of shown) {
      const key = capture.gameId ?? UNGROUPED;
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
    return counts;
  }, [shown]);

  const groups = useMemo(() => groupByGame(paged), [paged]);
  const inViewerOrder = useMemo(
    () => groupByGame(shown).flatMap((group) => group.items),
    [shown],
  );

  useEffect(() => {
    if (!viewing) return;
    const at = shown.findIndex((c) => c.path === viewing);
    if (at >= limit) setLimit(Math.ceil((at + 1) / PAGE) * PAGE);
  }, [viewing, shown, limit]);

  useEffect(() => {
    if (!viewing) return;
    const at = inViewerOrder.findIndex((c) => c.path === viewing);
    if (at >= 0) {
      viewedIndex.current = at;
      return;
    }
    const fallback = inViewerOrder[Math.min(viewedIndex.current, inViewerOrder.length - 1)];
    setViewing(fallback ? fallback.path : null);
  }, [viewing, inViewerOrder]);

  if (supported === false) {
    return (
      <p className="max-w-[70ch] text-[13px] leading-relaxed text-white/55">
        {t("overlay.captures.unsupported")}
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-stretch gap-3">
        <GroupBox compact className="flex min-w-0 flex-1 flex-col gap-3 px-[18px] py-4">
          <div className="flex items-center justify-between">
            <div className="text-[13px] font-semibold">{t("common.screenshots")}</div>
            {shotHotkey && <Keys compact accelerator={shotHotkey} />}
          </div>

          <button
            type="button"
            onClick={() => void takeScreenshot()}
            disabled={shooting}
            className="flex items-center justify-center gap-[9px] rounded-[9px] border border-white/[0.16] bg-white/[0.08] px-4 py-[11px] text-[13px] font-semibold text-white transition-colors duration-150 hover:bg-white/[0.13] disabled:opacity-60"
          >
            {shooting ? t("overlay.captures.takingScreenshot") : t("overlay.captures.takeScreenshot")}
          </button>

          <div>
            <GroupLabel compact label={t("overlay.captures.format")} />
            <SegmentedControl
              compact
              className="mt-[7px]"
              options={[
                { value: "png", label: "PNG" }, // i18n-ignore
                { value: "jpeg", label: "JPEG" }, // i18n-ignore
              ]}
              value={values.overlayShotFormat}
              onChange={(v) => set("overlayShotFormat", v)}
              ariaLabel={t("overlay.captures.screenshotFormat")}
            />
          </div>

          <div>
            <div className="flex items-center justify-between">
              <GroupLabel compact label={t("overlay.captures.quality")} />
              <span className="ov-mono text-[11px] text-white/60">{qualityDraft ?? quality}</span>
            </div>
            <div className="mt-[9px]">
              <Slider
                compact
                min={40}
                max={100}
                step={1}
                value={quality}
                disabled={values.overlayShotFormat === "png"}
                onChange={(v) => set("overlayShotQuality", String(v))}
                onDraft={setQualityDraft}
                ariaLabel={t("overlay.captures.screenshotQuality")}
              />
            </div>
            {values.overlayShotFormat === "png" && (
              <p className="mt-[6px] text-[11px] text-white/55">
                {t("overlay.captures.pngLossless")}
              </p>
            )}
          </div>

          <div className="flex items-center justify-between gap-3 py-2">
            <span className="text-[12px] text-white/75">{t("overlay.captures.showMessage")}</span>
            <Toggle
              compact
              checked={values.overlayShotToast === "true"}
              ariaLabel={t("overlay.captures.showMessage")}
              onChange={(v) => set("overlayShotToast", String(v))}
            />
          </div>

        </GroupBox>

        <GroupBox compact className="flex min-w-0 flex-[1.35] flex-col gap-3 px-[18px] py-4">
          <div className="flex items-center justify-between">
            <div className="text-[13px] font-semibold">{t("overlay.captures.videoRecording")}</div>
            {recHotkey && <Keys compact accelerator={recHotkey} />}
          </div>

          <button
            type="button"
            onClick={() => void toggleRecording()}
            disabled={pending !== null || (!recordingSupported && !recording)}
            className={`flex items-center justify-center gap-[9px] rounded-[9px] border px-4 py-[11px] text-[13px] font-semibold transition-colors duration-150 disabled:opacity-60 ${
              recording
                ? "border-[#ef4444]/40 bg-[#ef4444]/[0.14] text-danger-soft hover:bg-[#ef4444]/20"
                : "border-white/[0.16] bg-white/[0.08] text-white hover:bg-white/[0.13]"
            }`}
          >
            <span
              aria-hidden="true"
              className={`h-[9px] w-[9px] rounded-full ${
                recording ? "animate-pulse bg-[#ef4444]" : "bg-white/60"
              }`}
            />
            {pending === "starting"
              ? t("overlay.captures.starting")
              : pending === "stopping"
                ? t("overlay.captures.saving")
                : recording
                  ? (
                      <Trans
                        k="overlay.captures.stopRecording"
                        tags={{ elapsed: () => <RecordingElapsed since={since} /> }}
                      />
                    )
                  : t("overlay.captures.startRecording")}
          </button>

          {!recordingSupported && (
            <p className="text-[11px] leading-relaxed text-white/55">
              {t("overlay.captures.needsMediaFoundation")}
            </p>
          )}

          <div className="flex flex-col gap-3">
            <Setting label={t("overlay.captures.resolution")}>
              <SegmentedControl
                compact
                options={[
                  { value: "720", label: "720p" },
                  { value: "1080", label: "1080p" },
                  { value: "1440", label: "1440p" },
                  { value: "native", label: t("overlay.captures.native") },
                ]}
                value={res}
                onChange={(v) => set("overlayRecRes", v)}
                ariaLabel={t("overlay.captures.recordingResolution")}
              />
            </Setting>
            <Setting label={t("overlay.captures.frameRate")}>
              <SegmentedControl
                compact
                options={[
                  { value: "30", label: "30" },
                  { value: "60", label: "60" },
                  { value: "120", label: "120" },
                ]}
                value={fps}
                onChange={(v) => set("overlayRecFps", v)}
                ariaLabel={t("overlay.captures.recordingFrameRate")}
              />
            </Setting>
            <Setting label={t("overlay.captures.quality")}>
              <SegmentedControl
                compact
                options={[
                  { value: "efficient", label: t("overlay.captures.qualityEfficient") },
                  { value: "balanced", label: t("overlay.captures.qualityBalanced") },
                  { value: "high", label: t("overlay.captures.qualityHigh") },
                ]}
                value={recQuality}
                onChange={(v) => set("overlayRecQuality", v)}
                ariaLabel={t("overlay.captures.recordingQuality")}
              />
            </Setting>
            <Setting label={t("overlay.captures.encoder")}>
              <SegmentedControl
                compact
                options={[
                  { value: "h264", label: "H.264" }, // i18n-ignore
                  { value: "hevc", label: "HEVC" }, // i18n-ignore
                  { value: "av1", label: "AV1" }, // i18n-ignore
                ]}
                value={codec}
                onChange={(v) => set("overlayRecCodec", v)}
                ariaLabel={t("overlay.captures.encoder")}
              />
            </Setting>
            {megabytesPerMinute !== null && (
              <p className="text-[11px] leading-[1.45] text-white/55">
                {t("overlay.captures.estimate", { mb: megabytesPerMinute })}
              </p>
            )}
          </div>

          <div>
            <GroupLabel compact label={t("overlay.captures.audioTracks")} />
            <div className="mt-[7px] flex flex-wrap gap-[7px]">
              {tracks.map((track) => (
                <Chip
                  key={track.id}
                  compact
                  active={selectedTracks.includes(track.id)}
                  disabled={!track.available}
                  tip={track.available ? undefined : t("overlay.captures.needsWindows11")}
                  onClick={() => toggleTrack(track.id)}
                >
                  {track.label}
                </Chip>
              ))}
            </div>
            <p className="mt-[6px] text-[11px] leading-[1.45] text-white/55">
              {selectedTracks.includes("desktop")
                ? t("overlay.captures.audioDesktop")
                : selectedTracks.length === 0
                  ? t("overlay.captures.audioSilent")
                  : t("overlay.captures.audioMixed")}
            </p>

            {selectedTracks.includes("discord") && (
              <div className="mt-[10px] rounded-[9px] border border-white/[0.08] bg-white/[0.03] px-3 py-[10px]">
                <div className="text-[11.5px] text-white/60">
                  {discordClients.length === 0
                    ? t("overlay.captures.discordNone")
                    : discordClients.length === 1
                      ? t("overlay.captures.discordCapturing", { client: discordClients[0].label })
                      : t("overlay.captures.discordChoose")}
                </div>
                {discordClients.length > 1 && (
                  <div className="mt-[8px] flex flex-wrap gap-[7px]">
                    {discordClients.map((client) => (
                      <Chip
                        key={client.id}
                        compact
                        active={discordChosen(client.id)}
                        onClick={() => toggleDiscordClient(client.id)}
                      >
                        {client.label}
                      </Chip>
                    ))}
                  </div>
                )}
              </div>
            )}
          </div>

          <p className="text-[11px] leading-[1.5] text-white/55">
            {t("overlay.captures.clipsNote")}
          </p>
        </GroupBox>
      </div>

      <div className="flex items-center justify-between gap-[14px] rounded-[10px] border border-white/[0.08] bg-white/[0.04] px-4 py-[13px]">
        <div className="min-w-0">
          <div className="text-[12.5px] font-medium">{t("overlay.captures.folder")}</div>
          <div className="ov-mono mt-[3px] truncate text-[11px] text-white/50">{folder}</div>
        </div>
        <div className="flex shrink-0 gap-2">
          <button type="button" onClick={() => void openCaptureFolder()} className={FOLDER_BUTTON}>
            {t("overlay.captures.openFolder")}
          </button>
          <button
            type="button"
            onClick={() => void chooseCaptureFolder().then(reloadFolder)}
            className={FOLDER_BUTTON}
          >
            {t("overlay.captures.changeFolder")}
          </button>
          {!folderIsDefault && (
            <button
              type="button"
              onClick={() => void resetCaptureFolder().then(reloadFolder)}
              className={FOLDER_BUTTON}
            >
              {t("overlay.captures.useDefaultFolder")}
            </button>
          )}
        </div>
      </div>

      {error && (
        <div
          role={error.warning ? "status" : "alert"}
          className={`flex items-start gap-3 rounded-[10px] border px-4 py-3 text-[12.5px] leading-relaxed ${
            error.warning
              ? "border-[#f59e0b]/30 bg-[#f59e0b]/[0.08] text-warning"
              : "border-[#ef4444]/30 bg-[#ef4444]/[0.08] text-danger-soft"
          }`}
        >
          {error.warning && <AlertTriangle size={16} className="mt-[2px] shrink-0" />}
          <span className="min-w-0 flex-1">{error.text}</span>
          <Dismiss onClick={() => setError(null)} />
        </div>
      )}

      <div>
        <div className="mb-[10px] flex items-center gap-[7px]">
          {(["All", "Screenshots", "Clips"] as const).map((label) => (
            <button
              key={label}
              type="button"
              aria-pressed={filter === label}
              onClick={() => setFilter(label)}
              className={`rounded-[8px] border px-[11px] py-[6px] text-[12px] font-medium transition-colors duration-150 ${
                filter === label
                  ? "border-white/[0.14] bg-white/[0.09] text-white"
                  : "border-white/[0.08] text-white/55 hover:text-white"
              }`}
            >
              {t(FILTER_LABEL[label])}
            </button>
          ))}
          {stored > 0 && (
            <span className="ml-auto text-[11.5px] text-white/55">
              {t("overlay.captures.stored", { size: fmtBytes(stored) })}
            </span>
          )}
        </div>

        {shown.length === 0 ? (
          <p className="text-[13px] text-white/55">{t("overlay.captures.empty")}</p>
        ) : (
          groups.map((group) => (
            <div key={group.id} className="mt-[18px] first:mt-0">
              <div className="mb-[9px] flex items-center gap-[7px]">
                {group.known && (
                  <img
                    src={iconSrcFor(group.id, icons)}
                    onError={iconFallback(group.id)}
                    alt=""
                    className="h-[16px] w-[16px] rounded-[3px] object-cover"
                  />
                )}
                <span className="text-[12px] font-medium text-white/70">
                  {group.id === UNGROUPED ? t("overlay.captures.otherGame") : group.name}
                </span>
                <span className="text-[11px] text-white/55">
                  {totals.get(group.id) ?? group.items.length}
                </span>
              </div>
              <div className="grid gap-[14px] [grid-template-columns:repeat(auto-fill,minmax(190px,1fr))]">
                {group.items.map((capture) => (
                  <div
                    key={capture.path}
                    className="overflow-hidden rounded-[10px] border border-white/[0.08] bg-white/[0.04]"
                  >
                    <button
                      type="button"
                      onClick={() => setViewing(capture.path)}
                      className="relative block w-full cursor-zoom-in"
                    >
                      <CapturePreview capture={capture} />
                      {capture.kind === "clip" && (
                        <span className="absolute bottom-[6px] right-[6px] rounded-[5px] bg-black/70 px-[6px] py-[2px] text-[10px] font-medium text-white/85">
                          {t("overlay.captures.clip")}
                        </span>
                      )}
                    </button>
                    <div className="px-[10px] pb-[10px] pt-[9px]">
                      <div className="text-[11px] text-white/55">{fmtRelTime(capture.takenAt, now)}</div>
                      <div className="mt-[7px] flex gap-[5px]">
                        <button
                          type="button"
                          onClick={() => setViewing(capture.path)}
                          className="flex-1 rounded-[6px] bg-white/[0.07] py-[5px] text-[10.5px] text-white/75 hover:bg-white/[0.12]"
                        >
                          {t("overlay.captures.view")}
                        </button>
                        <button
                          type="button"
                          onClick={() => askRemove(capture.path)}
                          className="flex-1 rounded-[6px] bg-[#ef4444]/10 py-[5px] text-[10.5px] text-danger-soft hover:bg-[#ef4444]/20"
                        >
                          {armed === capture.path ? t("overlay.captures.deleteConfirm") : t("common.delete")}
                        </button>
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          ))
        )}
        {shown.length > paged.length && (
          <button
            type="button"
            onClick={() => setLimit((current) => current + PAGE)}
            className="mt-[16px] w-full rounded-[8px] border border-white/[0.08] py-[8px] text-[12px] font-medium text-white/55 transition-colors duration-150 hover:bg-white/[0.06] hover:text-white"
          >
            {t("overlay.captures.showMore", { count: shown.length - paged.length })}
          </button>
        )}
      </div>

      {viewing && (
        <Suspense fallback={null}>
          <MediaViewer
            overlay={{ onDelete: (path) => void remove(path) }}
            items={inViewerOrder}
            path={viewing}
            onPath={(path) => {
              setViewerError(null);
              setViewing(path);
            }}
            onClose={closeViewer}
          />
        </Suspense>
      )}

      {viewing &&
        viewerError &&
        createPortal(
          <div
            role="alert"
            className="fixed bottom-6 left-1/2 z-[130] flex max-w-[min(560px,90vw)] -translate-x-1/2 items-start gap-3 rounded-[10px] border border-[#ef4444]/40 bg-[#1a0d10]/95 px-4 py-3 text-[12.5px] leading-relaxed text-danger-soft shadow-[0_10px_30px_-10px_rgba(0,0,0,0.8)]"
          >
            <span className="min-w-0 flex-1">{viewerError}</span>
            <Dismiss onClick={() => setViewerError(null)} />
          </div>,
          document.body,
        )}
    </div>
  );
}

function CapturePreview({ capture }: { capture: MediaItem }) {
  const media = tileMedia(capture);
  if (media.video && media.src) {
    return (
      <span className="block aspect-[16/9] w-full bg-black">
        <LazyClip src={media.src} />
      </span>
    );
  }
  return (
    <img
      src={media.src ?? undefined}
      alt={capture.name}
      loading="lazy"
      decoding="async"
      className={`aspect-[16/9] w-full object-cover ${capture.thumbPath ? "bg-black" : ""}`}
    />
  );
}

function Dismiss({ onClick }: { onClick: () => void }) {
  const t = useT();
  return (
    <button
      type="button"
      aria-label={t("common.dismiss")}
      onClick={onClick}
      className="-my-1 -mr-2 grid h-[26px] w-[26px] shrink-0 place-items-center rounded-[6px] opacity-70 hover:bg-white/[0.08] hover:opacity-100"
    >
      <X size={14} />
    </button>
  );
}

function RecordingElapsed({ since }: { since: number }) {
  const [tick, setTick] = useState(0);

  useEffect(() => {
    if (!since) {
      setTick(0);
      return;
    }
    const update = () => setTick(Math.max(0, Math.round((Date.now() - since) / 1000)));
    update();
    const timer = window.setInterval(update, 1000);
    return () => window.clearInterval(timer);
  }, [since]);

  return <>{fmtDuration(tick * 1000) || "0:00"}</>;
}

function Setting({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-[7px]">
      <GroupLabel compact label={label} />
      {children}
    </div>
  );
}

function estimateMegabytesPerMinute(
  estimate: OverlayRecEstimate | null,
  res: string,
  fps: string,
  codec: string,
  quality: string,
  withAudio: boolean,
): number | null {
  if (!estimate || estimate.sourceWidth <= 0 || estimate.sourceHeight <= 0) return null;
  const wanted = res === "native" ? 0 : Number.parseInt(res, 10) || 0;
  const height =
    wanted === 0 || wanted >= estimate.sourceHeight ? estimate.sourceHeight : wanted;
  const width = Math.floor((height * estimate.sourceWidth) / estimate.sourceHeight);
  const frames = Math.min(240, Math.max(24, Number.parseInt(fps, 10) || 60));
  const perPixel =
    (estimate.bitsPerPixel[quality] ?? estimate.bitsPerPixel.balanced ?? 0) *
    (codec === "hevc" || codec === "av1" ? estimate.modernCodecFactor : 1);
  const video = Math.min(
    estimate.maxBps,
    Math.max(estimate.minBps, Math.floor((width & ~1) * (height & ~1) * frames * perPixel)),
  );
  const bitsPerSecond = video + (withAudio ? estimate.audioBps : 0);
  return Math.max(1, Math.round((bitsPerSecond * 60) / 8 / 1_000_000));
}
