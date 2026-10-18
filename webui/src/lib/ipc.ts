// ------------ Backend Bridge ------------
// Every call from the UI into the Rust side lives here, wrapped in typed functions and event listeners so
// components never deal with raw channel names (the calls themselves are in rpc.ts). It is split into sections:
// games, settings, mods, overlay, customization, playtime, the download queue, maintenance, installing, and captures.
import { log } from "./log";
import { t, type TKey } from "./i18n";
import { act, attempt, call, ok, onEvent, rpc, rpcAction, rpcRead, surfaceFailure, type Reply } from "./rpc";
import { jobFailureKey, useNotificationStore } from "../store/notifications";
import { useQueueStore } from "../store/queueStore";
import { GAMES, isGameColor, type GameId, type GraphicsApi, type ResourceQuality } from "../data/games";

const idOf = (p: { gameId?: unknown } | null | undefined): string => String(p?.gameId ?? "");

// ------------ App ------------
// The launcher's version and build channel.
export interface PendingInstall {
  path: string;
  versionType: string;
}

interface GameInfo {
  id: string;
  installed: boolean;
  pendingInstall: PendingInstall | null;
  qualitySwitch: ResourceQuality | null;
}

export type BuildType = "stable" | "beta";

export const CHANNEL_LABEL: Record<BuildType, string> = {
  get stable() {
    return t("ipc.channel.stable");
  },
  get beta() {
    return t("ipc.channel.beta");
  },
};

export async function appInfo(): Promise<{ version: string; buildType: BuildType }> {
  const [versionData, buildData] = await Promise.all([
    rpcRead<{ version?: string }>("get-app-version"),
    rpc<{ buildType?: unknown } | null>("get-build-info").catch(() => null),
  ]);
  return {
    version: String(versionData?.version || "0.0.0"),
    buildType: buildData?.buildType === "beta" ? "beta" : "stable",
  };
}

// ------------ Games and Launching ------------
// Listing games, launching them, and the events for a game starting, stopping, failing to launch,
// or having an update available.
export async function listGames(): Promise<GameInfo[]> {
  const ids = GAMES.map((g) => g.id);
  const cfg =
    (await getConfigValues(
      ids.flatMap((id) => [
        `games.${id}.gamePath`,
        `games.${id}.pendingInstall`,
        `games.${id}.qualitySwitch`,
        `games.${id}.resourceQuality`,
      ]),
    )) || {};
  return ids.map((id) => {
    const installPath = (cfg[`games.${id}.gamePath`] as string | null | undefined) || null;
    const pending = cfg[`games.${id}.pendingInstall`] as
      | { path?: string; versionType?: string }
      | null
      | undefined;
    const switchTo = cfg[`games.${id}.qualitySwitch`];
    const chosen = cfg[`games.${id}.resourceQuality`];
    return {
      id,
      installed: !!installPath,
      qualitySwitch:
        isQuality(switchTo) && (!isQuality(chosen) || chosen === switchTo) ? switchTo : null,
      pendingInstall: pending?.path
        ? { path: String(pending.path), versionType: String(pending.versionType || "default") }
        : null,
    };
  });
}

export async function launchGame(id: string): Promise<{ accepted: boolean; viaSteam: boolean }> {
  const r = await act<{ viaSteam?: boolean }>("actions.failed.launch", "launch-game", id);
  return { accepted: !!r, viaSteam: r?.viaSteam === true };
}

export async function getRunningGameIds(ids: readonly string[]): Promise<string[] | null> {
  const all = await rpcRead<{ runningIds?: unknown }>("get-running-game");
  if (!Array.isArray(all?.runningIds)) return null;
  const running = all.runningIds.map(String);
  return ids.filter((id) => running.includes(id));
}

export const onGameStarted = (cb: (id: string) => void) => onEvent("game-started", (p) => cb(idOf(p)));

export const onGameStopped = (cb: (id: string) => void) => onEvent("game-stopped", (p) => cb(idOf(p)));

export const onGameLaunchFailed = (cb: (id: string, reason: string) => void) =>
  onEvent("game-launch-failed", (p) => cb(idOf(p), String(p?.reason || "")));

export const onModsLoadFailed = (cb: (gameId: string, error: string, message: string) => void) =>
  onEvent("mods-load-failed", (p) =>
    cb(String(p?.gameId || ""), String(p?.error || ""), String(p?.message || "")),
  );

export const onModsNotice = (cb: (kind: "tidied" | "settings", message: string) => void) =>
  onEvent("mods-notice", (p) => {
    if (p?.message) cb(p.kind === "settings" ? "settings" : "tidied", p.message);
  });

export interface GameUpdateVersions {
  currentVersion: string | null;
  latestVersion: string | null;
}

export type RawVersions = { currentVersion?: unknown; latestVersion?: unknown };

type GameUpdateStatus =
  | ({ ok: true; available: boolean } & GameUpdateVersions)
  | { ok: false; error: string };

const updateVersions = (r: RawVersions): GameUpdateVersions => ({
  currentVersion: typeof r.currentVersion === "string" ? r.currentVersion : null,
  latestVersion: typeof r.latestVersion === "string" ? r.latestVersion : null,
});

export async function checkGameUpdate(gameId: string, force = false): Promise<GameUpdateStatus> {
  try {
    const r = await rpc<
      (Reply & { updateAvailable?: boolean } & RawVersions) | undefined
    >("check-for-updates", gameId, force);
    if (!r || r.success === false) {
      return { ok: false, error: r?.error || t("ipc.updateServerNoAnswer") };
    }
    return {
      ok: true,
      available: !!r.updateAvailable,
      ...updateVersions(r),
    };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export const onGameUpdateAvailable = (cb: (gameId: string, versions: GameUpdateVersions) => void) =>
  onEvent("update-available", (p) => {
    if (p?.gameId) cb(String(p.gameId), updateVersions(p));
  });

// ------------ Settings and Config ------------
// Reading and saving launcher settings and per-game config: update preferences, launch arguments,
// a custom launcher, the FPS unlock, and Steam installs.
interface SettingsReply {
  settings?: Record<string, string>;
  configNotice?: Partial<ConfigNotice> | null;
}

let settingsRead: Promise<SettingsReply | undefined> | null = null;

function readSettings(): Promise<SettingsReply | undefined> {
  return (settingsRead ??= rpcRead<SettingsReply>("get-settings").finally(() => {
    settingsRead = null;
  }));
}

export async function getSettings(): Promise<Record<string, string>> {
  return (await readSettings())?.settings || {};
}

interface ConfigNotice {
  type: "info" | "warning";
  title: string;
  text: string;
  saveBlocked?: boolean;
}

export async function getConfigNotice(): Promise<ConfigNotice | null> {
  const n = (await readSettings())?.configNotice;
  if (!n || typeof n.title !== "string" || typeof n.text !== "string") return null;
  return {
    type: n.type === "warning" ? "warning" : "info",
    title: n.title,
    text: n.text,
    saveBlocked: n.saveBlocked === true,
  };
}

export async function setSetting(key: string, value: string): Promise<boolean> {
  const res = await rpcAction<Reply & { warning?: string; warningTitle?: string }>(
    "actions.failed.saveSettings",
    "set-setting",
    key,
    value,
  );
  if (res?.warning) {
    useNotificationStore.getState().push({
      type: "warning",
      title: res.warningTitle ?? t("ipc.settingSaved"),
      text: res.warning,
    });
  }
  return res?.success === true;
}

export async function setConfigValue(key: string, value: unknown): Promise<void> {
  await rpcAction("actions.failed.saveSetting", "set-config", key, value);
}

async function getConfigValue(key: string): Promise<unknown> {
  return (await rpcRead<{ value?: unknown }>("get-config", key))?.value;
}

async function getConfigValues(keys: string[]): Promise<Record<string, unknown> | undefined> {
  return (await rpcRead<{ values?: Record<string, unknown> }>("get-config", keys))?.values;
}

export interface GameUpdatePrefs {
  autoUpdate: boolean;
  autoUpdateOnStartup: boolean;
  autoUpdateSchedule: "off" | "daily" | "weekly";
}

interface GameConfig {
  prefs: GameUpdatePrefs;
  launchArgs: string;
  customLauncher: string;
  launchViaSteam: boolean;
}

const GAME_CONFIG_KEYS = [
  "autoUpdate",
  "autoUpdateOnStartup",
  "autoUpdateSchedule",
  "launchArgs",
  "customLauncher",
  "launchViaSteam",
];

export async function getGameConfig(gameId: string): Promise<GameConfig> {
  const values = (await getConfigValues(GAME_CONFIG_KEYS.map((f) => `games.${gameId}.${f}`))) || {};
  const g = (f: string) => values[`games.${gameId}.${f}`];
  const schedule = g("autoUpdateSchedule");
  return {
    prefs: {
      autoUpdate: g("autoUpdate") !== false,
      autoUpdateOnStartup: g("autoUpdateOnStartup") !== false,
      autoUpdateSchedule: schedule === "off" || schedule === "weekly" ? schedule : "daily",
    },
    launchArgs: str(g("launchArgs")),
    customLauncher: str(g("customLauncher")),
    launchViaSteam: g("launchViaSteam") !== false,
  };
}

export const getCustomLauncher = async (gameId: string) =>
  str(await getConfigValue(`games.${gameId}.customLauncher`));

export async function setCustomLauncher(gameId: string, pick: boolean): Promise<string | undefined> {
  const label = "actions.failed.changeCustomLauncher";
  return (await act<PathReply>(label, "set-custom-launcher", gameId, pick ? "pick" : null))?.path;
}

export type FpsUnlockState = "idle" | "launching" | "attaching" | "locating" | "ready" | "failed";

export interface FpsUnlock {
  supported: boolean;
  enabled: boolean;
  targetFps: number;
  powerSave: boolean;
  backgroundFps: number;
  session: { state: FpsUnlockState; error: string | null };
  appliesNextLaunch: boolean;
}

const NO_FPS_UNLOCK: FpsUnlock = {
  supported: false,
  enabled: false,
  targetFps: 120,
  powerSave: false,
  backgroundFps: 30,
  session: { state: "idle", error: null },
  appliesNextLaunch: false,
};

export async function getFpsUnlock(gameId: string): Promise<FpsUnlock> {
  return { ...NO_FPS_UNLOCK, ...(await rpcRead<Partial<FpsUnlock>>("get-fps-unlock", gameId)) };
}

export type FpsUnlockPatch = Partial<
  Pick<FpsUnlock, "enabled" | "targetFps" | "powerSave" | "backgroundFps">
>;

export async function setFpsUnlock(gameId: string, patch: FpsUnlockPatch): Promise<boolean | undefined> {
  const r = await act<{ appliesNextLaunch?: boolean }>(
    "actions.failed.saveFpsUnlock",
    "set-fps-unlock",
    gameId,
    patch,
  );
  return r ? !!r.appliesNextLaunch : undefined;
}

export const onFpsUnlockStatus = (
  cb: (status: { gameId: string; state: FpsUnlockState; error: string | null }) => void,
) =>
  onEvent("fps-unlock-status", (p) =>
    cb({ gameId: idOf(p), state: p?.state ?? "idle", error: p?.error ?? null }),
  );

export interface SteamInstall {
  isSteamInstall: boolean;
  steamFound: boolean;
  launchViaSteam: boolean;
}

const NO_STEAM: SteamInstall = {
  isSteamInstall: false,
  steamFound: false,
  launchViaSteam: true,
};

export async function getSteamInstall(gameId: string, launchViaSteam?: boolean): Promise<SteamInstall> {
  const [detected, viaSteam] = await Promise.all([
    rpcRead<{ isSteamInstall?: boolean; steamFound?: boolean }>("get-steam-install", gameId),
    launchViaSteam ?? getConfigValue(`games.${gameId}.launchViaSteam`),
  ]);
  if (!detected) return NO_STEAM;
  return {
    isSteamInstall: !!detected.isSteamInstall,
    steamFound: !!detected.steamFound,
    launchViaSteam: viaSteam !== false,
  };
}

interface ModSupportedGame {
  id: string;
  variant: string;
  gameBananaId: number | null;
  installed: boolean;
  enabled: boolean;
}

// ------------ Mods ------------
// Mod support for a game: status, installing the mod loader, importing and toggling mods, and mod
// profiles (named sets of mods you can swap between).
export interface ModsStatus {
  masterEnabled: boolean;
  gameEnabled: boolean;
  gameId: string;
  supportsMods: boolean;
  experimental: boolean;
  forcesDx11: boolean;
  variant: string | null;
  toolchainInstalled: boolean;
  toolchainPath: string;
  isDefault: boolean;
  versions: Record<string, string>;
  toolsInstalling: boolean;
  supportedGames: ModSupportedGame[];
  gameSettings: ModGameSetting[];
}

export interface ModGameSetting {
  key: string;
  label: string;
  description: string;
  value: boolean;
}

export async function setModGameSetting(gameId: string, key: string, value: boolean): Promise<void> {
  await setConfigValue(`games.${gameId}.${key}`, value);
}

export interface LoaderSetting {
  id: string;
  label: string;
  description: string;
  kind: "bool" | "millis" | "key" | "choice";
  options?: { value: string; label: string }[] | null;
  value: string;
  accelerator?: string | null;
  defaultAccelerator?: string | null;
  min: number | null;
  max: number | null;
}

export async function getLoaderSettings(
  gameId?: string,
): Promise<{ gameId: string | null; rows: LoaderSetting[] }> {
  const r = await rpcRead<{ gameId?: string; settings?: LoaderSetting[] }>(
    "mod-ini-settings",
    ...(gameId ? [gameId] : []),
  );
  return { gameId: r?.gameId ?? null, rows: r?.settings ?? [] };
}

export async function setLoaderSetting(
  gameId: string | null,
  id: string,
  value: string,
  quiet = false,
): Promise<{ ok: boolean; error: string | null }> {
  const label = quiet ? null : "actions.failed.saveLoaderSetting";
  const r = await rpcAction<Reply>(label, "set-mod-ini-setting", gameId, id, value);
  return { ok: r?.success === true, error: r?.error ?? null };
}

interface ModSource {
  kind: "archive" | "gamebanana" | "manual";
  gbModId?: number;
  gbFileId?: number;
  version?: string | null;
  thumbnailUrl?: string | null;
}

export interface ModEntry {
  modId: string;
  folderName: string;
  name: string;
  enabled: boolean;
  sizeBytes: number | null;
  source: ModSource;
  version: string | null;
  installedAt: string | null;
  thumbnailUrl: string | null;
}

const NO_MODS: ModsStatus = {
  masterEnabled: false,
  gameEnabled: false,
  gameId: "",
  supportsMods: false,
  experimental: false,
  forcesDx11: false,
  variant: null,
  toolchainInstalled: false,
  toolchainPath: "",
  isDefault: true,
  versions: {},
  toolsInstalling: false,
  supportedGames: [],
  gameSettings: [],
};

export async function getModsStatus(gameId: string): Promise<ModsStatus> {
  return { ...NO_MODS, ...(await rpcRead<ModsStatus>("mods-status", gameId)) };
}

export const getModsMasterEnabled = async () => (await getConfigValue("behavior.modsEnabled")) === true;

export async function listMods(gameId: string): Promise<ModEntry[] | undefined> {
  const r = await rpcRead<{ mods?: ModEntry[] }>("list-mods", gameId);
  return r ? (r.mods ?? []) : undefined;
}

export async function setGameModsEnabled(gameId: string, enabled: boolean): Promise<void> {
  await rpcAction("actions.failed.saveSetting", "set-game-mods-enabled", gameId, enabled);
}

export async function importModArchive(
  gameId: string,
  paths?: string[],
): Promise<{ installed: string[]; failed: string[] } | "cancelled" | undefined> {
  const r = await rpcAction<Reply & { installed?: string[]; failed?: string[] }>(
    "actions.failed.importMod",
    "import-mod-archive",
    gameId,
    paths ?? null,
  );
  if (r?.cancelled) return "cancelled";
  if (!r?.success) return undefined;
  return { installed: r.installed ?? [], failed: r.failed ?? [] };
}

export async function setModEnabled(
  gameId: string,
  folderName: string,
  enabled: boolean,
): Promise<string | undefined> {
  const label = enabled ? "actions.failed.enableMod" : "actions.failed.disableMod";
  const r = await act<{ folderName?: string }>(label, "set-mod-enabled", gameId, folderName, enabled);
  return r && (r.folderName ?? folderName);
}

interface BulkToggleResult {
  changed: { folderName: string; previousFolderName: string; enabled: boolean }[];
  failed: { folderName: string; error: string }[];
}

export async function setModsEnabledBulk(
  gameId: string,
  folderNames: string[],
  enabled: boolean,
): Promise<BulkToggleResult | undefined> {
  const label = enabled ? "actions.failed.enableMods" : "actions.failed.disableMods";
  const args = [gameId, folderNames, enabled];
  const r = await act<Partial<BulkToggleResult>>(label, "set-mods-enabled-bulk", ...args);
  return r && { changed: r.changed ?? [], failed: r.failed ?? [] };
}

export interface ModProfile {
  id: string;
  name: string;
  modIds: string[];
  createdAt: string;
  modCount: number;
  resolvedCount: number;
}

interface ModProfileList {
  gameId: string;
  profiles: ModProfile[];
  activeId: string;
}

export async function listModProfiles(gameId: string): Promise<ModProfileList | undefined> {
  const r = await rpcRead<ModProfileList>("list-mod-profiles", gameId);
  return r ? { ...r, profiles: r.profiles ?? [] } : undefined;
}

type ProfileReply = { profile?: ModProfile };

export const createModProfile = async (gameId: string, name: string) =>
  (await act<ProfileReply>("actions.failed.createProfile", "create-mod-profile", gameId, name))?.profile;

export const renameModProfile = (gameId: string, profileId: string, name: string) =>
  ok("actions.failed.renameProfile", "rename-mod-profile", gameId, profileId, name);

export const deleteModProfile = (gameId: string, profileId: string) =>
  ok("actions.failed.deleteProfile", "delete-mod-profile", gameId, profileId);

export const duplicateModProfile = async (gameId: string, profileId: string) =>
  (await act<ProfileReply>("actions.failed.duplicateProfile", "duplicate-mod-profile", gameId, profileId))
    ?.profile;

export async function setModProfileMembers(gameId: string, profileId: string, add: string[], remove: string[]) {
  const args = [gameId, profileId, add, remove];
  return (await act<ProfileReply>("actions.failed.updateProfile", "set-mod-profile-members", ...args))?.profile;
}

export interface ApplyProfileResult {
  activeId: string;
  enabled: { folderName: string }[];
  disabled: { folderName: string }[];
  failed: { folderName: string; error: string }[];
  missing: string[];
}

export async function applyModProfile(
  gameId: string,
  profileId: string,
): Promise<ApplyProfileResult | undefined> {
  const r = await act<Partial<ApplyProfileResult>>(
    "actions.failed.applyProfile",
    "apply-mod-profile",
    gameId,
    profileId,
  );
  if (!r) return undefined;
  return {
    activeId: String(r.activeId || profileId),
    enabled: r.enabled ?? [],
    disabled: r.disabled ?? [],
    failed: r.failed ?? [],
    missing: r.missing ?? [],
  };
}

export const deleteMod = (gameId: string, folderName: string, modId?: string) =>
  ok("actions.failed.deleteMod", "delete-mod", gameId, folderName, ...(modId ? [modId] : []));

// ------------ Overlay and Mods Folder ------------
// Overlay shortcuts and capture folder settings, plus choosing the mods folder and mod install progress.
export interface OverlayHotkey {
  id: string;
  label: string;
  accelerator: string;
  default: string;
}

interface OverlayOption {
  id: string;
  label: string;
}

export interface OverlayAudioTrack extends OverlayOption {
  available: boolean;
}

const PER_APP_TRACKS = ["game", "discord"];

export function toggledAudioTracks(selected: string[], id: string): string[] {
  if (selected.includes(id)) return selected.filter((x) => x !== id);
  const next = [...selected, id];
  if (id === "desktop") return next.filter((x) => !PER_APP_TRACKS.includes(x));
  return PER_APP_TRACKS.includes(id) ? next.filter((x) => x !== "desktop") : next;
}

export interface OverlayConfig {
  hotkeys: OverlayHotkey[];
  audioTracks: OverlayAudioTrack[];
  selectedAudioTracks: string[];
  perAppAudio: boolean;
  captureFolder: string;
  captureFolderIsDefault: boolean;
  launchAction: string;
  recEstimate?: OverlayRecEstimate | null;
}

export interface OverlayRecEstimate {
  bitsPerPixel: Record<string, number>;
  modernCodecFactor: number;
  minBps: number;
  maxBps: number;
  audioBps: number;
  sourceWidth: number;
  sourceHeight: number;
}

export const getOverlayConfig = () => rpcRead<OverlayConfig>("get-overlay-config");

export async function setOverlayHotkey(
  id: string,
  accelerator: string,
  quiet = false,
): Promise<{ ok: boolean; warning: string | null; error: string | null }> {
  const r = await rpcAction<Reply & { warning?: string | null }>(
    quiet ? null : "actions.failed.changeShortcut",
    "set-overlay-hotkey",
    id,
    accelerator,
  );
  return { ok: r?.success === true, warning: r?.warning ?? null, error: r?.error ?? null };
}

export async function suspendOverlayHotkeys(on: boolean): Promise<void> {
  await rpc("suspend-overlay-hotkeys", on);
}

export interface OverlayStatus {
  state?: string;
  error?: string;
  unregistered?: string[];
  invalid?: string[];
  windowError?: string | null;
  gameId?: string | null;
  sessionStartedMs?: number | null;
  recording?: boolean;
  recordingSinceMs?: number;
  hudFeedback?: { event?: string; payload?: Record<string, unknown> | null } | null;
}

export const getOverlayStatus = () => rpcRead<OverlayStatus>("get-overlay-status");

export async function getCaptureSupport(): Promise<{ supported: boolean; recordingSupported: boolean }> {
  const s = await rpc<{ supported?: boolean; recordingSupported?: boolean }>("overlay-capture-status");
  return { supported: s?.supported === true, recordingSupported: s?.recordingSupported !== false };
}

export async function closeOverlay(): Promise<void> {
  await rpc("overlay-toggle", "close");
}

type PathReply = { path?: string };

export const chooseCaptureFolder = async () =>
  (await act<PathReply>("actions.failed.changeCapturesFolder", "set-overlay-capture-folder", "pick"))?.path;

export const resetCaptureFolder = async () =>
  (await act<PathReply>("actions.failed.resetCapturesFolder", "set-overlay-capture-folder", null))?.path;

export const openCaptureFolder = () => ok("actions.failed.openCapturesFolder", "open-capture-folder");

export const openModsFolder = (gameId: string) =>
  ok("actions.failed.openModsFolder", "open-mods-folder", gameId);

export const chooseModsPath = async () =>
  (await act<PathReply>("actions.failed.changeModsFolder", "set-mods-path", "pick"))?.path;

export const resetModsPath = async () =>
  (await act<PathReply>("actions.failed.resetModsFolder", "set-mods-path", null))?.path;

export interface ModProgress {
  gameId: string;
  message: string;
  percentage: number;
  done: boolean;
  failed: boolean;
}

export const onModProgress = (cb: (p: ModProgress) => void) =>
  onEvent("mods-progress", (p) =>
    cb({
      gameId: idOf(p),
      message: String(p?.message ?? ""),
      percentage: Number(p?.percentage ?? 0),
      done: p?.done === true,
      failed: p?.failed === true,
    }),
  );

// ------------ Customization and Library ------------
// Wallpapers, game icons, graphics API, quality and voice packs, the list of games shown in the sidebar,
// and the official wallpapers fetched from the server.
export interface MediaSlice {
  type: "default" | "custom";
  path: string | null;
}

export const defaultGraphicsApi = (gameId: string): GraphicsApi =>
  GAMES.find((g) => g.id === gameId)?.graphicsApiDefault ?? "dx11";

const RESOURCE_QUALITIES = ["sd", "hd", "uhd"] as const;
export const DEFAULT_RESOURCE_QUALITY: ResourceQuality = "hd";
const option = <V extends string>(value: V, label: TKey): { value: V; label: string } => ({
  value,
  get label() {
    return t(label);
  },
});

export const RESOURCE_QUALITY_OPTIONS: { value: ResourceQuality; label: string }[] = [
  option("sd", "ipc.quality.sd"),
  option("hd", "ipc.quality.hd"),
  option("uhd", "ipc.quality.uhd"),
];

export interface ResourceQualityInfo {
  installed: ResourceQuality[];
  sizes: Partial<Record<ResourceQuality, { downloadBytes: number; installBytes: number }>> | null;
}

const isQuality = (v: unknown): v is ResourceQuality =>
  typeof v === "string" && (RESOURCE_QUALITIES as readonly string[]).includes(v);

export async function getResourceQuality(gameId: string, withSizes = false): Promise<ResourceQualityInfo> {
  const r = await rpcRead<{ installed?: unknown; sizes?: unknown }>(
    "get-resource-quality",
    gameId,
    withSizes,
  );
  const installed = Array.isArray(r?.installed) ? r.installed.filter(isQuality) : [];
  const sizes: ResourceQualityInfo["sizes"] = {};
  if (r?.sizes && typeof r.sizes === "object") {
    for (const [k, v] of Object.entries(r.sizes as Record<string, unknown>)) {
      const s = v as { downloadBytes?: unknown; installBytes?: unknown };
      if (isQuality(k) && typeof s?.installBytes === "number" && typeof s.downloadBytes === "number") {
        sizes[k] = { downloadBytes: s.downloadBytes, installBytes: s.installBytes };
      }
    }
  }
  return { installed, sizes: Object.keys(sizes).length ? sizes : null };
}

export interface QualityCheck {
  selected: ResourceQuality;
  launchWith: ResourceQuality;
  onDisk: ResourceQuality[];
  files: Partial<Record<ResourceQuality, { present: number; total: number }>>;
}

export async function checkResourceQuality(
  gameId: string,
): Promise<{ ok: true; check: QualityCheck } | { ok: false; error: string }> {
  try {
    const r = await rpc<
      | (Reply & { selected?: unknown; launchWith?: unknown; onDisk?: unknown; files?: unknown })
      | undefined
    >("check-resource-quality", gameId);
    if (!r || r.success === false || !isQuality(r.selected) || !isQuality(r.launchWith)) {
      return { ok: false, error: r?.error || t("common.genericError") };
    }
    const files: QualityCheck["files"] = {};
    for (const [k, v] of Object.entries((r.files ?? {}) as Record<string, unknown>)) {
      const f = v as { present?: unknown; total?: unknown };
      if (isQuality(k) && typeof f?.present === "number" && typeof f.total === "number") {
        files[k] = { present: f.present, total: f.total };
      }
    }
    const onDisk = Array.isArray(r.onDisk) ? r.onDisk.filter(isQuality) : [];
    return { ok: true, check: { selected: r.selected, launchWith: r.launchWith, onDisk, files } };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

const VOICE_PACK_LANGUAGES = ["en-us", "ja-jp", "ko-kr", "zh-cn"] as const;
export type VoicePackLanguage = (typeof VOICE_PACK_LANGUAGES)[number];
export const DEFAULT_VOICE_PACK: VoicePackLanguage = "en-us";
export const VOICE_PACK_OPTIONS: { value: VoicePackLanguage; label: string }[] = [
  option("en-us", "ipc.voicePack.en"),
  option("ja-jp", "ipc.voicePack.ja"),
  option("ko-kr", "ipc.voicePack.ko"),
  option("zh-cn", "ipc.voicePack.zh"),
];

export async function getAppliedVoicePacks(gameId: string): Promise<string[] | null> {
  const r = await rpcRead<{ languages?: string[] | null }>("get-applied-voice-packs", gameId);
  return Array.isArray(r?.languages) ? r.languages : null;
}

interface Customization {
  wallpaper: Record<string, MediaSlice>;
  gameIcons: Record<string, MediaSlice>;
  graphicsApi: Record<string, GraphicsApi>;
  resourceQuality: Record<string, ResourceQuality>;
  voicePack: Record<string, VoicePackLanguage>;
  playtimeColor: Record<string, string>;
}

function normSlice(v: unknown): MediaSlice {
  const s = (v || {}) as { type?: string; path?: string | null };
  return { type: s.type === "custom" ? "custom" : "default", path: s.path ?? null };
}

export async function getCustomization(): Promise<Customization> {
  const out: Customization = {
    wallpaper: {},
    gameIcons: {},
    graphicsApi: {},
    resourceQuality: {},
    voicePack: {},
    playtimeColor: {},
  };
  const ids = GAMES.map((g) => g.id);
  const cfg = await getConfigValues([
    "wallpaper",
    "gameIcons",
    ...ids.flatMap((id) => [
      `games.${id}.graphicsApi`,
      `games.${id}.resourceQuality`,
      `games.${id}.voicePackLanguage`,
      `games.${id}.playtimeColor`,
    ]),
  ]);
  if (!cfg) return out;
  for (const [k, v] of Object.entries(rec(cfg.wallpaper) ?? {})) out.wallpaper[k] = normSlice(v);
  for (const [k, v] of Object.entries(rec(cfg.gameIcons) ?? {})) out.gameIcons[k] = normSlice(v);
  for (const k of ids) {
    const api = cfg[`games.${k}.graphicsApi`];
    if (api === "dx11" || api === "dx12") out.graphicsApi[k] = api;
    const quality = cfg[`games.${k}.resourceQuality`];
    if (isQuality(quality)) out.resourceQuality[k] = quality;
    const color = cfg[`games.${k}.playtimeColor`];
    if (typeof color === "string" && isGameColor(color)) out.playtimeColor[k] = color.trim().toLowerCase();
    const lang = cfg[`games.${k}.voicePackLanguage`];
    if (typeof lang === "string" && (VOICE_PACK_LANGUAGES as readonly string[]).includes(lang)) {
      out.voicePack[k] = lang as VoicePackLanguage;
    }
  }
  return out;
}

interface LibraryConfig {
  visible: GameId[];
  setupComplete: boolean;
}

let savedSetupComplete: boolean | undefined;

export async function getLibrary(): Promise<LibraryConfig> {
  const allIds = GAMES.map((g) => g.id);
  const library = (await getConfigValue("library")) as
    | { visible?: unknown; setupComplete?: unknown }
    | null
    | undefined;
  const raw = Array.isArray(library?.visible) ? library.visible : [];
  const visible = raw.filter((id): id is GameId => allIds.includes(id as GameId));
  const setupComplete = library?.setupComplete === true;
  savedSetupComplete = library ? setupComplete : undefined;
  return {
    visible: [...new Set(visible)],
    setupComplete,
  };
}

export async function saveLibrary(library: LibraryConfig): Promise<void> {
  await setConfigValue("library.visible", library.visible);
  if (library.setupComplete === savedSetupComplete) return;
  const { setupComplete } = library;
  const saved = await ok("actions.failed.saveSetting", "set-config", "library.setupComplete", setupComplete);
  savedSetupComplete = saved ? setupComplete : undefined;
}

export const selectWallpaperFile = async () =>
  (await rpcRead<PathReply>("select-wallpaper-file"))?.path || null;

export const saveWallpaper = (gameId: string, slice: MediaSlice) =>
  ok("actions.failed.saveWallpaper", "save-wallpaper", { gameId, config: slice });

export const selectGameIconFile = async () =>
  (await rpcRead<PathReply>("select-game-icon-file"))?.path || null;

export const saveGameIcon = (gameId: string, slice: MediaSlice) =>
  ok("actions.failed.saveGameIcon", "save-game-icon", { gameId, config: slice });

export interface RemoteMedia {
  wallpaper: string | null;
  static: string | null;
}

export async function getWallpaperMedia(): Promise<Record<string, RemoteMedia>> {
  const r = await rpcRead<{
    media?: Record<string, { wallpaper?: string | null; static?: string | null }>;
  }>("get-wallpaper-media");
  const out: Record<string, RemoteMedia> = {};
  for (const [k, v] of Object.entries(r?.media || {})) {
    out[k] = { wallpaper: v?.wallpaper || null, static: v?.static || null };
  }
  return out;
}

export const onWallpaperMediaUpdated = (cb: (media: Record<string, RemoteMedia>) => void) =>
  onEvent("wallpaper-media-updated", (p) => cb(p?.media || {}));

// ------------ Playtime ------------
// Playtime totals, sessions and daily minutes, and removing a session.
interface PillStats {
  allTimeMinutes: number;
  sessions: number;
  lastPlayed: string | null;
}

interface GameStat {
  id: string;
  minutes: number;
  sessions: number;
  longestSessionMinutes: number;
}

export interface SessionEntry {
  gameId: string;
  start: number;
  minutes: number;
  local?: boolean;
  end?: number;
  live?: boolean;
}

export type DailyMap = Record<string, Record<string, number>>;

interface PlaytimeDashboard {
  games: GameStat[];
  daily: DailyMap;
  recentSessions: SessionEntry[];
  sessionsTruncated?: boolean;
}

export interface PlaytimeData {
  pill: Record<string, PillStats>;
  dashboard: PlaytimeDashboard;
}

export const EMPTY_PLAYTIME: PlaytimeData = {
  pill: {},
  dashboard: {
    games: [],
    daily: {},
    recentSessions: [],
  },
};

export async function getPlaytime(): Promise<PlaytimeData> {
  const r = await rpcRead<PlaytimeData>("get-playtime-data");
  return r?.dashboard ? { pill: r.pill || {}, dashboard: r.dashboard } : EMPTY_PLAYTIME;
}

export async function getOlderSessions(
  before: number,
): Promise<{ sessions: SessionEntry[]; hasMore: boolean } | null> {
  const r = await act<{ sessions?: SessionEntry[]; hasMore?: boolean }>(
    "actions.failed.loadEarlierSessions",
    "get-playtime-sessions",
    before,
  );
  return r ? { sessions: r.sessions ?? [], hasMore: r.hasMore === true } : null;
}

export const deletePlaytimeSession = (gameId: string, start: number) =>
  ok("actions.failed.removeSession", "playtime-delete-session", gameId, start);

// ------------ Download Queue ------------
// The download queue: its state, the events it sends, and pausing, resuming, prioritising or cancelling a
// job. Also where a job failure is turned into a notification.
export interface QueueJob {
  id: string;
  gameId: string;
  kind: string;
  baseKind?: string;
  phase: string;
  total: number;
  downloaded: number;
  percent: number;
  speed: number;
  networkSpeed: number;
  diskSpeed: number;
  etaSecs: number;
  paused: boolean;
  error: string | null;
  message?: string | null;
  warning?: boolean;
  parked?: boolean;
  waitingNetwork?: boolean;
  heldForGame?: boolean;
}

interface QueueState {
  jobs: QueueJob[];
}

const queueState = (p: { jobs?: unknown } | null | undefined): QueueState => ({
  jobs: Array.isArray(p?.jobs) ? p.jobs : [],
});

export const downloadState = async () => queueState(await rpcRead<QueueState>("get-download-queue-state"));

export const onQueueState = (cb: (s: QueueState) => void) =>
  onEvent("download-queue-state", (p) => cb(queueState(p)));

const ENDED_PHASES = ["done", "cancelled", "deferred", "error"];

const STARTS_DOWNLOAD = ["install", "update"];

interface QueuedJobWatch {
  taken: () => boolean;
  stop: () => void;
}

export function watchQueuedJob(
  gameId: string,
  kinds: readonly string[],
  onQueued?: () => void,
): QueuedJobWatch {
  const ours = (jobs: QueueJob[]) =>
    jobs.some(
      (j) =>
        j.gameId === gameId &&
        kinds.includes(j.baseKind || j.kind) &&
        !ENDED_PHASES.includes(j.phase),
    );
  const already = ours(useQueueStore.getState().jobs);
  let taken = false;
  const stop = already
    ? () => {}
    : useQueueStore.subscribe((s) => {
        if (taken || !ours(s.jobs)) return;
        taken = true;
        onQueued?.();
      });
  return { taken: () => taken, stop };
}

export function reportJobFailure(label: TKey, gameId: string, detail: unknown, queued: boolean): void {
  if (queued) log.warn(`[action] ${label}:`, detail ?? "(no detail)");
  else surfaceFailure(label, label, detail, jobFailureKey(gameId));
}

async function queuedAction<T>(
  label: TKey,
  channel: string,
  job: { gameId: string; kinds: readonly string[]; onQueued?: () => void },
  ...args: unknown[]
): Promise<T | undefined> {
  const watch = watchQueuedJob(job.gameId, job.kinds, job.onQueued);
  return attempt<T>(channel, args, (detail) =>
    reportJobFailure(label, job.gameId, detail, watch.taken()),
  ).finally(watch.stop);
}

export async function downloadEnqueue(gameId: string, onQueued?: () => void): Promise<void> {
  await queuedAction(
    "actions.failed.download",
    "start-download",
    { gameId, kinds: STARTS_DOWNLOAD, onQueued },
    { gameId, versionType: "default" },
  );
}

export async function pauseJob(job: QueueJob): Promise<void> {
  if (job.kind === "repair") await rpcAction("actions.failed.pause", "pause-repair", job.gameId);
  else await rpcAction("actions.failed.pause", "pause-download", job.id);
}

export async function resumeJob(job: QueueJob): Promise<void> {
  if (job.kind === "repair") await rpcAction("actions.failed.resume", "resume-repair", job.gameId);
  else await rpcAction("actions.failed.resume", "resume-download", job.id);
}

export async function keepGoingWhilePlaying(job: QueueJob): Promise<void> {
  await rpcAction("actions.failed.keepGoing", "keep-going-while-playing", job.gameId);
}

export async function prioritizeDownload(id: string): Promise<void> {
  const label = "actions.failed.prioritizeDownload";
  const res = await rpcAction<{ deferred?: string }>(label, "prioritize-download", id);
  if (res?.deferred) {
    useNotificationStore.getState().push({ type: "info", title: t("ipc.upNext"), text: res.deferred });
  }
}

const CANCEL_CHANNEL: Record<string, string> = {
  repair: "cancel-repair",
  verify: "cancel-verify",
  move: "cancel-move",
};

export async function downloadCancel(id: string, kind = "install"): Promise<void> {
  await rpcAction("actions.failed.cancel", CANCEL_CHANNEL[kind] ?? "cancel-download", id);
}

// ------------ News and Maintenance ------------
// The news panel and game links, wiping launcher data, and game upkeep: repair, verify, move, uninstall.
export interface NewsEntry {
  content: string;
  time: string;
  jumpUrl: string;
}
export interface NewsSlide {
  url?: string;
  jumpUrl: string;
}
export interface NewsData {
  guidance?: {
    notice?: { contents?: NewsEntry[] };
    news?: { contents?: NewsEntry[] };
  };
  slideshow?: NewsSlide[];
}

export async function applyActiveGame(id: string): Promise<void> {
  await rpcAction("actions.failed.switchGame", "set-active-game", id);
}

export async function getNewsData(id: string): Promise<NewsData | undefined> {
  const outer = await rpcRead<{ data?: NewsData }>("get-news-data", id);
  return outer ? outer.data || {} : undefined;
}

export async function getSavedNewsData(id: string): Promise<NewsData | undefined> {
  const outer = await rpcRead<{ data?: NewsData | null }>("get-news-data", id, { saved: true });
  return outer?.data ?? undefined;
}

export interface GameLinks {
  socials: { platform: string; url: string }[];
  communityTools: { name: string; url: string }[];
}

export async function getGameLinks(): Promise<Record<string, GameLinks>> {
  return (await rpcRead<{ links?: Record<string, GameLinks> }>("get-game-links"))?.links || {};
}

export const openLogsFolder = () => ok("actions.failed.openLogsFolder", "open-logs-folder");

export async function wipeLauncherData(): Promise<void> {
  let saved: [string, string][] = [];
  try {
    saved = Object.entries(localStorage).filter(([k]) => k.startsWith("peebify."));
    saved.forEach(([k]) => localStorage.removeItem(k));
  } catch (e) {
    log.warn("[action] Could not clear the interface's saved state:", e);
  }
  if (await ok("actions.failed.wipeLauncherData", "wipe-launcher-data")) return;
  try {
    saved.forEach(([k, v]) => {
      if (localStorage.getItem(k) === null) localStorage.setItem(k, v);
    });
  } catch (e) {
    log.warn("[action] Could not restore the interface's saved state:", e);
  }
}

const repairModes: Record<string, "quick" | "full"> = {};

export const lastRepairMode = (gameId: string): "quick" | "full" | undefined => repairModes[gameId];

export async function repairGame(gameId: string, quick = false): Promise<boolean> {
  repairModes[gameId] = quick ? "quick" : "full";
  return ok("actions.failed.repair", quick ? "start-quick-repair" : "start-repair", gameId);
}

interface VerifyResult {
  brokenFiles: number;
  message?: string;
  updatePending?: boolean;
}

export async function verifyGameIntegrity(
  gameId: string,
  onQueued?: () => void,
): Promise<VerifyResult | undefined> {
  const watch = watchQueuedJob(gameId, ["verify"], onQueued);
  const r = await act<{ invalidFiles?: unknown[]; message?: string; updatePending?: boolean }>(
    "actions.failed.verifyGameFiles",
    "verify-game-integrity",
    gameId,
  ).finally(watch.stop);
  if (!r) return undefined;
  return {
    brokenFiles: Array.isArray(r.invalidFiles) ? r.invalidFiles.length : 0,
    message: r.message || undefined,
    updatePending: typeof r.updatePending === "boolean" ? r.updatePending : undefined,
  };
}

export async function moveGame(gameId: string): Promise<void> {
  await queuedAction("actions.failed.moveGame", "move-game-location", { gameId, kinds: ["move"] }, gameId);
}

export async function uninstallGame(gameId: string): Promise<boolean> {
  const r = await queuedAction<Reply>(
    "actions.failed.uninstall",
    "uninstall-game",
    { gameId, kinds: ["uninstall"] },
    gameId,
  );
  return r?.success === true;
}

export const openLeftoverFolder = (gameId: string) =>
  ok("actions.failed.openFolder", "open-leftover-folder", gameId);

interface DiskSpace {
  free: number;
  total: number;
  path?: string;
}
export async function getDiskSpace(path?: string): Promise<DiskSpace | null> {
  const r = await rpcRead<DiskSpace>("get-disk-space", path ?? null);
  return r ? { free: r.free || 0, total: r.total || 0, path: r.path } : null;
}

// ------------ Install Flow ------------
// Everything the install dialog needs: disk space, default paths, size preview, content packs, starting
// or resuming a download, and finding a game that is already installed.
export interface GameStorage {
  gamePath: string;
  gameBytes: number;
  mods: { bytes: number; installed: number; enabled: number } | null;
  media: { bytes: number; screenshots: number; clips: number };
  drive: string;
  free: number | null;
  total: number | null;
}
const NO_STORAGE: GameStorage = {
  gamePath: "",
  gameBytes: 0,
  mods: null,
  media: { bytes: 0, screenshots: 0, clips: 0 },
  drive: "",
  free: null,
  total: null,
};
export async function getGameStorage(gameId: string): Promise<GameStorage | null> {
  const r = await rpcRead<Partial<GameStorage>>("get-game-storage", gameId);
  return r ? { ...NO_STORAGE, ...r } : null;
}

interface InstallPathHealth {
  path: string;
  tooDeep: boolean;
  notWritable: boolean;
  missing: boolean;
  message: string;
}
export async function getInstallPathHealth(gameId: string): Promise<InstallPathHealth> {
  const r = await rpcRead<Partial<InstallPathHealth>>("get-install-path-health", gameId);
  return {
    path: r?.path ?? "",
    tooDeep: !!r?.tooDeep,
    notWritable: !!r?.notWritable,
    missing: !!r?.missing,
    message: r?.message ?? "",
  };
}

interface DefaultInstallPath {
  path: string;
  maxRootLength?: number;
  folderName?: string;
}
export async function getDefaultInstallPath(gameId: string): Promise<DefaultInstallPath | null> {
  const r = await rpcRead<DefaultInstallPath>("get-default-install-path", { gameId });
  return r?.path ? { path: r.path, maxRootLength: r.maxRootLength ?? 0, folderName: r.folderName } : null;
}

interface InstallPreviewPart {
  key: string;
  label: string;
  bytes: number;
  files: number;
}
export interface InstallPreview {
  gameId: string;
  version: string | null;
  downloadBytes: number | null;
  installBytes: number | null;
  requiredBytes: number | null;
  fileCount: number | null;
  approximate: boolean;
  parts: InstallPreviewPart[];
  notSupported: boolean;
}
interface InstallPreviewResult {
  preview: InstallPreview | null;
  error: string | null;
}
export async function getInstallPreview(gameId: string): Promise<InstallPreviewResult> {
  const r = await rpc<(Partial<InstallPreview> & Reply) | null>("get-install-preview", gameId).catch((e) => {
    log.warn("[read] get-install-preview failed:", e);
    return null;
  });
  if (!r || r.success === false) {
    if (r?.error) log.warn("[read] get-install-preview failed:", r.error);
    return { preview: null, error: r?.error ?? null };
  }
  return {
    preview: {
      gameId: r.gameId ?? gameId,
      version: r.version ?? null,
      downloadBytes: r.downloadBytes ?? null,
      installBytes: r.installBytes ?? null,
      requiredBytes: r.requiredBytes ?? null,
      fileCount: r.fileCount ?? null,
      approximate: !!r.approximate,
      parts: Array.isArray(r.parts) ? r.parts : [],
      notSupported: !!r.notSupported,
    },
    error: null,
  };
}

export interface ContentPack {
  tag: string;
  language: string | null;
  bytes: number;
  files: number;
  selected: boolean;
}
interface ContentPackList {
  supported: boolean;
  packs: ContentPack[];
  error: boolean;
}
export async function getContentPacks(gameId: string): Promise<ContentPackList> {
  const r = await rpcRead<{ packs?: ContentPack[]; supported?: boolean }>("get-content-packs", gameId);
  if (!r) return { supported: true, packs: [], error: true };
  if (!r.supported) return { supported: false, packs: [], error: false };
  return { supported: true, packs: Array.isArray(r.packs) ? r.packs : [], error: false };
}

export async function setContentPacks(gameId: string, tags: string[] | null): Promise<boolean> {
  const key = `games.${gameId}.contentTags`;
  const r = await rpcAction<Reply>("actions.failed.saveSetting", "set-config", key, tags);
  return !!r && r.success !== false;
}

export async function selectInstallDirectory(): Promise<string | null> {
  const r = await rpc<{ canceled?: boolean; path?: string }>("select-install-directory");
  return r && !r.canceled && r.path ? r.path : null;
}

export async function startDownload(
  installPath: string,
  gameId: string,
  onQueued?: () => void,
): Promise<void> {
  await queuedAction(
    "actions.failed.install",
    "start-download",
    { gameId, kinds: STARTS_DOWNLOAD, onQueued },
    { installPath, versionType: "default", gameId },
  );
}

export async function resumeInstall(gameId: string, pending: PendingInstall): Promise<void> {
  await queuedAction(
    "actions.failed.resumeInstall",
    "start-download",
    { gameId, kinds: STARTS_DOWNLOAD },
    { installPath: pending.path, versionType: pending.versionType, gameId },
  );
}

interface LocateResult {
  ok: boolean;
  path?: string;
  error?: string;
  cancelled?: boolean;
}

async function locate(channel: string, what: string, gameId: string): Promise<LocateResult> {
  const r = await rpc<(Reply & PathReply) | null>(channel, gameId).catch((e) => {
    log.warn(`[action] ${what} game install failed:`, e);
    return null;
  });
  if (r?.success && r.path) return { ok: true, path: r.path };
  return { ok: false, cancelled: !!r?.cancelled, error: r?.error };
}

export const locateGameInstall = (gameId: string) => locate("browse-game-path", "Locate", gameId);

export const detectDefaultGameInstall = (gameId: string) =>
  locate("detect-default-game-path", "Detect default", gameId);

export const openGameFolder = (gameId: string) =>
  ok("actions.failed.openGameFolder", "open-game-folder", gameId);

export const openScreenshotFolder = (gameId: string) =>
  ok("actions.failed.openScreenshotsFolder", "open-screenshot-folder", gameId);

// ------------ Network and Window Events ------------
// Online status, window maximize and restore, power saving, and the install complete and games
// detected events.
const isOnline = async (channel: string) => {
  const r = await rpcRead<{ isOnline?: boolean }>(channel);
  return typeof r?.isOnline === "boolean" ? r.isOnline : null;
};

export const getNetworkStatus = () => isOnline("get-network-status");

export const networkRecheck = () => isOnline("network-recheck");

export const onNetworkStatusChanged = (cb: (online: boolean) => void) =>
  onEvent("network-status-changed", (p) => cb(p?.isOnline !== false));

export const onInstallationComplete = (cb: (gameId: string, version: string | null) => void) =>
  onEvent("installation-complete", (p) => cb(String(p?.gameId || ""), p?.version ?? null));

export const onGamesDetected = (cb: (gameIds: string[]) => void) =>
  onEvent("games-detected", (p) => cb(Array.isArray(p?.gameIds) ? p.gameIds.map(String) : []));

export const onRendererPowerSave = (cb: (suspended: boolean) => void) =>
  onEvent("renderer-power-save", (p) => cb(p !== false));

export const onWindowRestored = (cb: () => void) => onEvent("window-restored", () => cb());

export const onWindowMaximizedChanged = (cb: (maximized: boolean) => void) =>
  onEvent("window:maximized-changed", (p) => cb(!!p?.maximized));

const windowState = (channel: string) => rpcRead<{ maximized?: boolean; powerSave?: boolean }>(channel);

export const toggleMaximizeWindow = async () => !!(await windowState("toggle-maximize-window"))?.maximized;

export const getWindowMaximized = async () => !!(await windowState("get-window-state"))?.maximized;

export const getRendererPowerSave = async () => (await windowState("get-window-state"))?.powerSave === true;

const num = (v: unknown, fallback = 0): number =>
  typeof v === "number" && Number.isFinite(v) ? v : fallback;
const strOrNull = (v: unknown): string | null => (typeof v === "string" && v ? v : null);
const str = (v: unknown): string => (typeof v === "string" ? v : "");
const rec =(v: unknown): Record<string, unknown> | null =>
  v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;

// ------------ Captures ------------
// Listing, opening, revealing and deleting screenshots and clips, and the event that says the list changed.
type MediaKind = "screenshot" | "clip";

export interface MediaItem {
  path: string;
  kind: MediaKind;
  gameId: string | null;
  name: string;
  sizeBytes: number;
  takenAt: number;
  thumbPath: string | null;
}

interface CaptureList {
  ok: boolean;
  items: MediaItem[];
  folder: string;
  error?: string;
}

function asMediaItem(raw: unknown): MediaItem[] {
  const m = rec(raw);
  if (!m || typeof m.path !== "string") return [];
  const takenAt = typeof m.takenAt === "string" ? Date.parse(m.takenAt) : NaN;
  return [
    {
      path: m.path,
      kind: m.kind === "clip" ? "clip" : "screenshot",
      gameId: strOrNull(m.gameId),
      name: str(m.name),
      sizeBytes: num(m.sizeBytes),
      takenAt: Number.isFinite(takenAt) ? takenAt : 0,
      thumbPath: strOrNull(m.thumbPath),
    },
  ];
}

export async function listCaptures(): Promise<CaptureList> {
  const r = await call<{ captures?: unknown[]; folder?: string }>(
    "overlay-list-captures",
    "ipc.captures.readFailed",
  );
  if (!r.ok) return { ok: false, items: [], folder: "", error: r.error };
  return {
    ok: true,
    items: Array.isArray(r.captures) ? r.captures.flatMap(asMediaItem) : [],
    folder: str(r.folder),
  };
}

const isCaptureFolderKey = (key: unknown) =>
  key === "overlayCaptureFolder" || key === "behavior.overlayCaptureFolder";

export async function onCapturesChanged(cb: () => void): Promise<() => void> {
  const offs = await Promise.all([
    onEvent("overlay-capture", () => cb()),
    onEvent("overlay-record-stopped", () => cb()),
    onEvent("overlay-capture-deleted", () => cb()),
    onEvent("settings-changed", (p) => {
      if (isCaptureFolderKey(p?.key)) cb();
    }),
  ]);
  return () => offs.forEach((off) => off());
}

export async function deleteCapture(path: string): Promise<{ ok: boolean; error?: string }> {
  const r = await call("overlay-delete-capture", "ipc.captures.deleteFailed", path);
  return r.ok ? { ok: true } : { ok: false, error: r.error };
}

async function captureAction(label: TKey, channel: string, path: string, quiet: boolean) {
  const r = await rpcAction<Reply>(quiet ? null : label, channel, path);
  return { ok: r?.success !== false, error: r?.error ?? null };
}

export const openCapture = (path: string, quiet = false) =>
  captureAction("actions.failed.open", "overlay-open-capture", path, quiet);

export const revealCapture = (path: string, quiet = false) =>
  captureAction("actions.failed.showInFolder", "overlay-reveal-capture", path, quiet);
