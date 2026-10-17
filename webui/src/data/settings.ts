// ------------ Settings Table ------------
// A table of every toggle, dropdown and slider on the Settings page, grouped into sections and tabs.
// The settings UI is drawn from this. The default values live in settingsDefaults.ts.
import type { LucideIcon } from "lucide-react";
import { LANGUAGES, SYSTEM_LANGUAGE, type TKey } from "../lib/i18n";
import {
  AppWindow,
  Bell,
  Clock,
  Download,
  EyeOff,
  FastForward,
  FileImage,
  FileVideo,
  Film,
  Frame,
  Gamepad2,
  Gauge,
  Images,
  Info,
  Languages,
  LineChart,
  MessageSquare,
  Minus,
  Monitor,
  MonitorPlay,
  MousePointerClick,
  Network,
  Newspaper,
  Palette,
  Power,
  Puzzle,
  Ratio,
  ScrollText,
  Share2,
  SlidersHorizontal,
  Sparkles,
  Tag,
  Timer,
  Trash2,
  Undo2,
  X,
  ZoomIn,
} from "lucide-react";
export type SettingsCategory =
  | "general"
  | "appearance"
  | "games"
  | "mods"
  | "overlay"
  | "launcher";

type SettingText = TKey | { name: string };

export const settingText = (text: SettingText, tr: (key: TKey) => string): string =>
  typeof text === "string" ? tr(text) : text.name;

export interface SettingOption {
  value: string;
  label: SettingText;
}

interface BaseSetting {
  id: string;
  icon: LucideIcon;
  title: SettingText;
  description: TKey;
  group?: TKey;
  parent?: string;
  indent?: boolean;
  disableWhen?: (values: Record<string, string>) => boolean;
}

interface SegmentedSetting extends BaseSetting {
  kind: "segmented";
  width?: number;
  options: SettingOption[];
}

interface ToggleSetting extends BaseSetting {
  kind: "toggle";
  invert?: boolean;
}

interface SelectSetting extends BaseSetting {
  kind: "select";
  options: SettingOption[];
  unit?: string;
}

interface NumberSetting extends BaseSetting {
  kind: "number";
  min: number;
  max: number;
  unit: string;
}

interface ActionSetting extends BaseSetting {
  kind: "action";
  button: SettingText;
  danger?: boolean;
}

export type Setting =
  | SegmentedSetting
  | ToggleSetting
  | SelectSetting
  | NumberSetting
  | ActionSetting;

interface SettingsSection {
  category: SettingsCategory;
  title: TKey;
  subtitle: TKey;
  tab?: { label: TKey; icon: LucideIcon };
  settings: Setting[];
}

const triState = (): SettingOption[] => [
  { value: "minimize", label: "common.minimize" },
  { value: "tray", label: "settings.options.toTray" },
  { value: "close", label: "common.close" },
];

export const SETTINGS_SECTIONS: SettingsSection[] = [
  {
    category: "general",
    title: "settings.sections.general.title",
    subtitle: "settings.sections.general.subtitle",
    tab: { label: "settings.sections.general.title", icon: SlidersHorizontal },
    settings: [
      {
        kind: "select",
        id: "language",
        icon: Languages,
        group: "settings.groups.languageAndStartup",
        title: "language.title",
        description: "language.description",
        options: [
          { value: SYSTEM_LANGUAGE, label: "language.systemDefault" },
          ...LANGUAGES.map((l) => ({ value: l.code, label: { name: l.name } })),
        ],
      },
      {
        kind: "toggle",
        id: "startOnBoot",
        icon: Power,
        group: "settings.groups.languageAndStartup",
        title: "settings.items.startOnBoot.title",
        description: "settings.items.startOnBoot.description",
      },
      {
        kind: "segmented",
        id: "startOnBootAction",
        icon: AppWindow,
        parent: "startOnBoot",
        indent: true,
        group: "settings.groups.languageAndStartup",
        title: "settings.items.startOnBootAction.title",
        description: "settings.items.startOnBootAction.description",
        options: [
          { value: "open", label: "settings.items.startOnBootAction.options.open" },
          { value: "minimized", label: "settings.items.startOnBootAction.options.minimized" },
          { value: "tray", label: "settings.options.toTray" },
        ],
      },
      {
        kind: "toggle",
        id: "rememberWindowState",
        icon: Frame,
        group: "settings.groups.languageAndStartup",
        title: "settings.items.rememberWindowState.title",
        description: "settings.items.rememberWindowState.description",
      },
      {
        kind: "segmented",
        id: "launchAction",
        icon: Gamepad2,
        group: "settings.groups.windowBehavior",
        title: "settings.items.launchAction.title",
        description: "settings.items.launchAction.description",
        options: triState(),
      },
      {
        kind: "segmented",
        id: "closeAction",
        icon: X,
        group: "settings.groups.windowBehavior",
        title: "settings.items.closeAction.title",
        description: "settings.items.closeAction.description",
        options: triState(),
      },
      {
        kind: "segmented",
        id: "minimizeAction",
        icon: Minus,
        group: "settings.groups.windowBehavior",
        title: "settings.items.minimizeAction.title",
        description: "settings.items.minimizeAction.description",
        options: triState(),
      },
      {
        kind: "toggle",
        id: "reopenAfterGameClose",
        icon: Undo2,
        group: "settings.groups.windowBehavior",
        title: "settings.items.reopenAfterGameClose.title",
        description: "settings.items.reopenAfterGameClose.description",
        disableWhen: (v) => v.launchAction === "close",
      },
      {
        kind: "toggle",
        id: "osNotifications",
        icon: Bell,
        group: "settings.groups.notifications",
        title: "settings.items.osNotifications.title",
        description: "settings.items.osNotifications.description",
      },
      {
        kind: "select",
        id: "downloadSpeedLimit",
        icon: Gauge,
        group: "settings.groups.gameDownloads",
        title: "settings.items.downloadSpeedLimit.title",
        description: "settings.items.downloadSpeedLimit.description",
        options: [
          { value: "0", label: "settings.items.downloadSpeedLimit.unlimited" },
          ...["5", "10", "25", "50", "100", "250", "500"].map((v) => ({ value: v, label: { name: `${v} MB/s` } })),
        ],
        unit: "MB/s",
      },
      {
        kind: "segmented",
        id: "downloadWhileGaming",
        icon: Gamepad2,
        group: "settings.groups.gameDownloads",
        title: "settings.items.downloadWhileGaming.title",
        description: "settings.items.downloadWhileGaming.description",
        width: 300,
        options: [
          { value: "off", label: "settings.items.downloadWhileGaming.options.off" },
          { value: "slow", label: "settings.items.downloadWhileGaming.options.slow" },
          { value: "pause", label: "settings.items.downloadWhileGaming.options.pause" },
        ],
      },
      {
        kind: "segmented",
        id: "downloadConnections",
        icon: Network,
        group: "settings.groups.gameDownloads",
        title: "settings.items.downloadConnections.title",
        description: "settings.items.downloadConnections.description",
        width: 300,
        options: [
          { value: "0", label: "settings.items.downloadConnections.auto" },
          ...["4", "8", "16", "32"].map((v) => ({ value: v, label: { name: v } })),
        ],
      },
    ],
  },
  {
    category: "appearance",
    title: "settings.sections.appearance.title",
    subtitle: "settings.sections.appearance.subtitle",
    tab: { label: "settings.sections.appearance.title", icon: Palette },
    settings: [
      {
        kind: "toggle",
        id: "hideNewsPanel",
        icon: Newspaper,
        invert: true,
        group: "settings.groups.homeScreen",
        title: "settings.items.hideNewsPanel.title",
        description: "settings.items.hideNewsPanel.description",
      },
      {
        kind: "toggle",
        id: "hideSocials",
        icon: Share2,
        invert: true,
        group: "settings.groups.homeScreen",
        title: "settings.items.hideSocials.title",
        description: "settings.items.hideSocials.description",
      },
      {
        kind: "toggle",
        id: "hidePlaytime",
        icon: Timer,
        invert: true,
        group: "settings.groups.homeScreen",
        title: "settings.items.hidePlaytime.title",
        description: "settings.items.hidePlaytime.description",
      },
      {
        kind: "toggle",
        id: "hideBottomRightButtons",
        icon: MousePointerClick,
        invert: true,
        group: "settings.groups.homeScreen",
        title: "settings.items.hideBottomRightButtons.title",
        description: "settings.items.hideBottomRightButtons.description",
      },
      {
        kind: "toggle",
        id: "hideSidebarPlaytime",
        icon: LineChart,
        invert: true,
        group: "settings.groups.sidebar",
        title: "settings.items.hideSidebarPlaytime.title",
        description: "settings.items.hideSidebarPlaytime.description",
      },
      {
        kind: "toggle",
        id: "hideSidebarGallery",
        icon: Images,
        invert: true,
        group: "settings.groups.sidebar",
        title: "settings.items.hideSidebarGallery.title",
        description: "settings.items.hideSidebarGallery.description",
      },
      {
        kind: "toggle",
        id: "hideSidebarDownloads",
        icon: Download,
        invert: true,
        group: "settings.groups.sidebar",
        title: "settings.items.hideSidebarDownloads.title",
        description: "settings.items.hideSidebarDownloads.description",
      },
      {
        kind: "toggle",
        id: "hideSidebarMods",
        icon: Puzzle,
        invert: true,
        group: "settings.groups.sidebar",
        title: "settings.items.hideSidebarMods.title",
        description: "settings.items.hideSidebarMods.description",
      },
      {
        kind: "toggle",
        id: "hideSidebarVersion",
        icon: Tag,
        invert: true,
        group: "settings.groups.sidebar",
        title: "settings.items.hideSidebarVersion.title",
        description: "settings.items.hideSidebarVersion.description",
      },
      {
        kind: "select",
        id: "uiScale",
        icon: ZoomIn,
        group: "settings.groups.display",
        title: "settings.items.uiScale.title",
        description: "settings.items.uiScale.description",
        options: [
          { value: "80", label: { name: "80%" } },
          { value: "90", label: { name: "90%" } },
          { value: "100", label: "settings.items.uiScale.options.default" },
          { value: "110", label: { name: "110%" } },
          { value: "125", label: { name: "125%" } },
          { value: "150", label: { name: "150%" } },
          { value: "175", label: { name: "175%" } },
          { value: "200", label: { name: "200%" } },
        ],
      },
      {
        kind: "toggle",
        id: "animatedWallpaper",
        icon: Film,
        group: "settings.groups.display",
        title: "settings.items.animatedWallpaper.title",
        description: "settings.items.animatedWallpaper.description",
      },
      {
        kind: "segmented",
        id: "glassEffects",
        icon: Sparkles,
        group: "settings.groups.display",
        title: "settings.items.glassEffects.title",
        description: "settings.items.glassEffects.description",
        options: [
          { value: "full", label: "settings.items.glassEffects.options.full" },
          { value: "balanced", label: "settings.items.glassEffects.options.balanced" },
          { value: "lite", label: "settings.items.glassEffects.options.lite" },
        ],
      },
      {
        kind: "segmented",
        id: "timeFormat",
        icon: Clock,
        group: "settings.groups.display",
        title: "settings.items.timeFormat.title",
        description: "settings.items.timeFormat.description",
        options: [
          { value: "system", label: "settings.items.timeFormat.options.system" },
          { value: "12", label: "settings.items.timeFormat.options.h12" },
          { value: "24", label: "settings.items.timeFormat.options.h24" },
        ],
      },
    ],
  },
  {
    category: "games",
    title: "settings.sections.games.title",
    subtitle: "settings.sections.games.subtitle",
    tab: { label: "settings.sections.games.title", icon: Gamepad2 },
    settings: [],
  },
  {
    category: "mods",
    title: "common.mods",
    subtitle: "settings.sections.mods.subtitle",
    settings: [
      {
        kind: "toggle",
        id: "showNsfwMods",
        icon: EyeOff,
        group: "settings.groups.gameBanana",
        title: "settings.items.showNsfwMods.title",
        description: "settings.items.showNsfwMods.description",
      },
    ],
  },
  {
    category: "overlay",
    title: "common.overlay",
    subtitle: "settings.sections.overlay.subtitle",
    tab: { label: "common.overlay", icon: MonitorPlay },
    settings: [
      {
        kind: "toggle",
        id: "overlayEnabled",
        icon: Monitor,
        group: "common.overlay",
        title: "settings.items.overlayEnabled.title",
        description: "settings.items.overlayEnabled.description",
      },

      {
        kind: "segmented",
        id: "overlayShotFormat",
        icon: FileImage,
        parent: "overlayEnabled",
        group: "common.screenshots",
        title: "settings.items.overlayShotFormat.title",
        description: "settings.items.overlayShotFormat.description",
        options: [
          { value: "png", label: { name: "PNG" } },
          { value: "jpeg", label: { name: "JPEG" } },
        ],
      },
      {
        kind: "number",
        id: "overlayShotQuality",
        icon: SlidersHorizontal,
        parent: "overlayEnabled",
        indent: true,
        group: "common.screenshots",
        title: "settings.items.overlayShotQuality.title",
        description: "settings.items.overlayShotQuality.description",
        min: 40,
        max: 100,
        unit: "%",
      },
      {
        kind: "toggle",
        id: "overlayShotToast",
        icon: MessageSquare,
        parent: "overlayEnabled",
        group: "common.screenshots",
        title: "settings.items.overlayShotToast.title",
        description: "settings.items.overlayShotToast.description",
      },

      {
        kind: "segmented",
        id: "overlayRecRes",
        icon: Ratio,
        parent: "overlayEnabled",
        group: "settings.groups.recording",
        title: "settings.items.overlayRecRes.title",
        description: "settings.items.overlayRecRes.description",
        options: [
          { value: "native", label: "settings.items.overlayRecRes.options.native" },
          { value: "1440", label: { name: "1440p" } },
          { value: "1080", label: { name: "1080p" } },
          { value: "720", label: { name: "720p" } },
        ],
      },
      {
        kind: "segmented",
        id: "overlayRecFps",
        icon: FastForward,
        parent: "overlayEnabled",
        group: "settings.groups.recording",
        title: "settings.items.overlayRecFps.title",
        description: "settings.items.overlayRecFps.description",
        options: [
          { value: "30", label: { name: "30" } },
          { value: "60", label: { name: "60" } },
          { value: "120", label: { name: "120" } },
        ],
      },
      {
        kind: "segmented",
        id: "overlayRecCodec",
        icon: FileVideo,
        parent: "overlayEnabled",
        group: "settings.groups.recording",
        title: "settings.items.overlayRecCodec.title",
        description: "settings.items.overlayRecCodec.description",
        options: [
          { value: "h264", label: { name: "H.264" } },
          { value: "hevc", label: { name: "HEVC (H.265)" } },
          { value: "av1", label: { name: "AV1" } },
        ],
      },
      {
        kind: "segmented",
        id: "overlayRecQuality",
        icon: Sparkles,
        parent: "overlayEnabled",
        group: "settings.groups.recording",
        title: "settings.items.overlayRecQuality.title",
        description: "settings.items.overlayRecQuality.description",
        options: [
          { value: "efficient", label: "settings.items.overlayRecQuality.options.efficient" },
          { value: "balanced", label: "settings.items.overlayRecQuality.options.balanced" },
          { value: "high", label: "settings.items.overlayRecQuality.options.high" },
        ],
      },
    ],
  },
  {
    category: "launcher",
    title: "settings.sections.about.title",
    subtitle: "settings.sections.about.subtitle",
    tab: { label: "settings.sections.about.title", icon: Info },
    settings: [
      {
        kind: "toggle",
        id: "peebifyContent",
        icon: Newspaper,
        group: "settings.groups.privacy",
        title: "settings.items.peebifyContent.title",
        description: "settings.items.peebifyContent.description",
      },
      {
        kind: "action",
        id: "openLogs",
        icon: ScrollText,
        group: "settings.groups.troubleshooting",
        title: "settings.items.openLogs.title",
        description: "settings.items.openLogs.description",
        button: "settings.items.openLogs.button",
      },
      {
        kind: "action",
        id: "clearData",
        icon: Trash2,
        group: "settings.groups.dangerZone",
        title: "settings.items.clearData.title",
        description: "settings.items.clearData.description",
        button: "settings.items.clearData.button",
        danger: true,
      },
    ],
  },
];

export const ALL_SETTINGS: Setting[] = SETTINGS_SECTIONS.flatMap((s) => s.settings);

export const MODS_SETTINGS: Setting[] = SETTINGS_SECTIONS.find((s) => s.category === "mods")!.settings;

export const PEEBIFY_DISCORD_URL = "https://discord.gg/5kfpJTv2Xc";
export const PEEBIFY_SITE_URL = "https://peebify.net";
export const XXMI_URL = "https://github.com/SpectrumQT/XXMI-Launcher";
