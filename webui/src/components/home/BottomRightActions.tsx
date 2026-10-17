// ------------ Bottom Right Actions ------------
// The Home screen's bottom right buttons and the panel they open upwards, which the playtime pill uses too.
import { Fragment, useEffect, useState, useId, useLayoutEffect, useRef, type ReactNode } from "react";
import {
  Users,
  Settings,
  ExternalLink,
  Wrench,
  FolderOpen,
  FolderInput,
  FolderSearch,
  Trash2,
  Images,
  Cpu,
  Layers,
  Zap,
  RefreshCw,
  Globe,
  Newspaper,
  Map as MapIcon,
  Hammer,
  LineChart,
  BookOpen,
  Share2,
  ChevronUp,
  type LucideIcon,
} from "lucide-react";
import { Hint } from "../ui/Tooltip";
import { useUiStore } from "../../store/uiStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { useGameLinksStore } from "../../store/remoteContentStore";
import { type GameId, GRAPHICS_APIS, gameById, supportsQuickRepair } from "../../data/games";
import { openExternal } from "../../lib/tauri";
import {
  jobActiveBlocker,
  qualityLabel,
  useGameActions,
  useModsForceDx11,
  useResourceQuality,
} from "../../lib/gameActions";
import {
  openGameFolder,
  openScreenshotFolder,
  getCustomLauncher,
  defaultGraphicsApi,
  RESOURCE_QUALITY_OPTIONS,
} from "../../lib/ipc";
import { useT } from "../../lib/useT";
import { m } from "framer-motion";
import { useClickOutside, useEscapeKey } from "../../lib/hooks";
import { Collapse, springThumb, useReducedMotionSafe } from "../../lib/motion";

// ------------ Buttons ------------
// The pair of buttons at the bottom right of the Home screen. Community tools opens the game's list of links
// (wiki, maps and so on), and Quick settings holds repair, move, uninstall, folder shortcuts, DirectX and
// resource quality for the selected game.
const MENU_WIDTH = "w-[200px] min-[1000px]:w-[236px]";

export function BottomRightActions() {
  return (
    <div className="absolute bottom-[22px] right-4 z-[6] flex items-end gap-[10px]">
      <CommunityToolsMenu />
      <QuickSettingsMenu />
    </div>
  );
}

const TOOL_ICONS: { match: RegExp; icon: typeof Globe }[] = [
  { match: /official|homepage|site/i, icon: Globe },
  { match: /news|notice|blog/i, icon: Newspaper },
  { match: /track/i, icon: LineChart },
  { match: /map/i, icon: MapIcon },
  { match: /build|guide|tier/i, icon: Hammer },
  { match: /wiki|database/i, icon: BookOpen },
  { match: /network|hub|community/i, icon: Share2 },
];

function toolMeta(rawName: string, game: { name: string; short: string }, officialSite: string) {
  let label = rawName.trim();
  for (const prefix of [game.name, game.short]) {
    if (label.toLowerCase().startsWith(prefix.toLowerCase())) {
      const stripped = label.slice(prefix.length).replace(/^[\s:·–—-]+/, "").trim();
      if (stripped) {
        label = stripped;
        break;
      }
    }
  }
  if (/^official$/i.test(label)) label = officialSite;
  const icon = TOOL_ICONS.find((tool) => tool.match.test(rawName))?.icon ?? ExternalLink;
  return { label, icon };
}

function CommunityToolsMenu() {
  const activeId = useUiStore((s) => s.activeGameId);
  const game = gameById(activeId);
  const tools = useGameLinksStore((s) => s.links[activeId]?.communityTools) ?? [];
  const t = useT();
  return (
    <ExpandUp className={MENU_WIDTH} icon={Users} label={t("home.communityTools.title")}>
      {(close) => (
        <div className="py-[6px]">
          {tools.map((tool) => {
            const { label, icon: Icon } = toolMeta(tool.name, game, t("home.communityTools.officialSite"));
            return (
              <Hint key={tool.name} tip={tool.name} placement="left" className="block w-full">
                <button
                  onClick={() => {
                    close();
                    void openExternal(tool.url);
                  }}
                  className="group flex w-full items-center gap-[10px] px-[12px] py-[7px] text-left text-[13px] text-white/85 transition-colors hover:bg-white/[0.06]"
                >
                  <span className="grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] bg-white/[0.06] text-white/65 transition-colors group-hover:bg-white/[0.09] group-hover:text-white/90">
                    <Icon size={14} />
                  </span>
                  <span className="flex-1 truncate">{label}</span>
                  <ExternalLink
                    size={12}
                    className="shrink-0 opacity-0 transition-opacity group-hover:opacity-50"
                  />
                </button>
              </Hint>
            );
          })}
        </div>
      )}
    </ExpandUp>
  );
}

function PillSwitch<T extends string>({
  options,
  value,
  disabled,
  onChange,
}: {
  options: { value: T; label: string }[];
  value: T;
  disabled?: boolean;
  onChange: (v: T) => void;
}) {
  return (
    <span
      className={`flex shrink-0 items-center gap-[2px] rounded-[7px] border border-white/[0.09] bg-black/30 p-[2px] ${
        disabled ? "opacity-45" : ""
      }`}
    >
      {options.map((o) => {
        const active = o.value === value;
        return (
          <button
            key={o.value}
            disabled={disabled}
            onClick={() => onChange(o.value)}
            aria-pressed={active}
            className={`rounded-[5px] px-[6px] py-[3px] text-[10px] font-semibold leading-none tracking-[0.01em] transition-colors ${
              active
                ? "bg-white/[0.14] text-white"
                : disabled
                  ? "text-white/30"
                  : "text-white/45 hover:text-white/80"
            }`}
          >
            {o.label}
          </button>
        );
      })}
    </span>
  );
}

function useCustomLauncherSet(gameId: string): boolean {
  const [set, setSet] = useState(false);
  useEffect(() => {
    let alive = true;
    void getCustomLauncher(gameId).then((path) => {
      if (alive) setSet(!!path);
    });
    return () => {
      alive = false;
    };
  }, [gameId]);
  return set;
}

function DirectXRow({
  gameId,
  installed,
  forceDx11,
  customLauncher,
}: {
  gameId: GameId;
  installed: boolean;
  forceDx11: boolean;
  customLauncher: boolean;
}) {
  const api = useCustomizationStore((s) => s.graphicsApi[gameId]) ?? defaultGraphicsApi(gameId);
  const setGraphicsApi = useCustomizationStore((s) => s.setGraphicsApi);
  const t = useT();
  const locked = !installed || forceDx11 || customLauncher;

  return (
    <Hint
      tip={forceDx11 && !customLauncher ? t("home.quickSettings.modsNeedDx11") : undefined}
      placement="left"
      className="block w-full"
    >
      <div
        className={`flex w-full items-center gap-[10px] px-[12px] py-[7px] text-[13px] ${
          locked ? "text-white/30" : "text-white/85"
        }`}
      >
        <span
          className={`grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] ${
            locked ? "bg-white/[0.04] text-white/25" : "bg-white/[0.06] text-white/65"
          }`}
        >
          <Cpu size={14} />
        </span>
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate">DirectX</span>{/* i18n-ignore */}
          {customLauncher && (
            <span className="whitespace-normal text-[11px] leading-snug text-white/35">
              {t("home.quickSettings.notUsedWithCustomLauncher")}
            </span>
          )}
        </span>
        <PillSwitch
          options={GRAPHICS_APIS}
          value={forceDx11 ? "dx11" : api}
          disabled={locked}
          onChange={(v) => setGraphicsApi(gameId, v)}
        />
      </div>
    </Hint>
  );
}

function ResourceQualityRow({
  installed,
  jobActive,
  running,
  customLauncher,
  resourceQuality: { quality, setQuality, installedLabel, pending, apply },
}: {
  installed: boolean;
  jobActive: boolean;
  running: boolean;
  customLauncher: boolean;
  resourceQuality: ReturnType<typeof useResourceQuality>;
}) {
  const t = useT();
  const locked = customLauncher;
  const hint = customLauncher
    ? t("home.quickSettings.notUsedWithCustomLauncher")
    : t("home.quickSettings.qualityPending", { installed: installedLabel, quality: qualityLabel(quality) });

  return (
    <Hint
      tip={!customLauncher && !pending && !installed ? t("home.quickSettings.qualityHint") : undefined}
      placement="left"
      className="block w-full"
    >
      <div className={`w-full px-[12px] py-[7px] text-[13px] ${locked ? "text-white/30" : "text-white/85"}`}>
        <div className="flex items-center gap-[10px]">
          <span
            className={`grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] ${
              locked ? "bg-white/[0.04] text-white/25" : "bg-white/[0.06] text-white/65"
            }`}
          >
            <Layers size={14} />
          </span>
          <span className="min-w-0 flex-1 truncate">{t("home.quickSettings.quality")}</span>
          <PillSwitch
            options={RESOURCE_QUALITY_OPTIONS}
            value={quality}
            disabled={locked}
            onChange={setQuality}
          />
        </div>
        {(customLauncher || pending) && (
          <div className="mt-[6px] flex items-center gap-[8px] pl-[38px]">
            <span className="min-w-0 flex-1 text-[11px] leading-snug text-white/45">{hint}</span>
            {pending && !locked && (
              <button
                onClick={apply}
                disabled={jobActive || running}
                className="shrink-0 rounded-[6px] bg-white/[0.12] px-[9px] py-[5px] text-[11px] font-semibold leading-none text-white transition-colors hover:bg-white/[0.2] disabled:opacity-40"
              >
                {t("home.quickSettings.apply")}
              </button>
            )}
          </div>
        )}
      </div>
    </Hint>
  );
}

function QuickSettingsMenu() {
  const activeId = useUiStore((s) => s.activeGameId);
  const {
    game,
    managed,
    installed,
    partial,
    running,
    jobActive,
    steamCopy,
    uninstallBlocker,
    checkUpdates,
    quickRepair,
    confirmFullRepair,
    locate,
    confirmMove,
    confirmUninstall,
  } = useGameActions(activeId);
  const forceDx11 = useModsForceDx11(activeId);
  const customLauncher = useCustomLauncherSet(activeId);
  const resourceQuality = useResourceQuality(activeId, { installed, jobActive });
  const t = useT();

  type Item = {
    icon: typeof Wrench;
    label: string;
    hint?: string;
    danger?: boolean;
    disabled?: boolean;
    dividerBefore?: boolean;
    onClick: () => void;
  };

  const options: Item[] = [
    {
      icon: RefreshCw,
      label: t("home.quickSettings.checkUpdates"),
      disabled: !installed || !managed,
      onClick: () => void checkUpdates(),
    },
    {
      icon: Zap,
      label: t("home.quickSettings.quickRepair"),
      hint:
        installed && managed && !supportsQuickRepair(activeId)
          ? t("home.quickSettings.onlyFullRepair")
          : installed && managed && jobActive
            ? jobActiveBlocker()
            : undefined,
      disabled: !installed || !managed || !supportsQuickRepair(activeId) || jobActive,
      onClick: quickRepair,
    },
    {
      icon: Wrench,
      label: t("home.quickSettings.fullRepair"),
      hint: installed && managed && jobActive ? jobActiveBlocker() : undefined,
      disabled: !installed || !managed || jobActive,
      onClick: confirmFullRepair,
    },
    {
      icon: FolderOpen,
      label: t("home.quickSettings.gameFolder"),
      dividerBefore: true,
      disabled: !installed,
      onClick: () => void openGameFolder(activeId),
    },
    {
      icon: Images,
      label: t("common.screenshots"),
      onClick: () => void openScreenshotFolder(activeId),
    },
    {
      icon: FolderSearch,
      label: t("home.quickSettings.locateInstall"),
      dividerBefore: true,
      onClick: () => void locate(),
    },
    {
      icon: FolderInput,
      label: t("home.quickSettings.moveGame"),
      hint: installed && !steamCopy ? (uninstallBlocker ?? undefined) : undefined,
      disabled: !installed || steamCopy || running || jobActive,
      onClick: confirmMove,
    },
    {
      icon: Trash2,
      label: partial ? t("home.quickSettings.discardDownload") : t("home.quickSettings.uninstallGame"),
      hint: installed || partial ? (uninstallBlocker ?? undefined) : undefined,
      danger: true,
      disabled: (!installed && !partial) || !!uninstallBlocker,
      onClick: () => void confirmUninstall(),
    },
  ];

  return (
    <ExpandUp className={MENU_WIDTH} icon={Settings} label={t("home.quickSettings.title")}>
      {(close) => (
        <div className="max-h-[calc(100vh-124px)] overflow-y-auto overscroll-contain py-[6px]">
          {options.map(({ icon: Icon, label, hint, danger, disabled, dividerBefore, onClick }) => (
            <Fragment key={label}>
              {dividerBefore && <div className="mx-[12px] my-[5px] h-px bg-white/[0.07]" />}
              <button
                onClick={
                  disabled
                    ? undefined
                    : () => {
                        close();
                        onClick();
                      }
                }
                disabled={disabled}
                className={`group flex w-full items-center gap-[10px] px-[12px] py-[7px] text-left text-[13px] transition-colors ${
                  disabled
                    ? "cursor-not-allowed text-white/30"
                    : `${danger ? "text-red-300" : "text-white/85"} hover:bg-white/[0.06]`
                }`}
              >
                <span
                  className={`grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] transition-colors ${
                    disabled
                      ? "bg-white/[0.04] text-white/25"
                      : danger
                        ? "bg-red-500/[0.12] text-red-300/90"
                        : "bg-white/[0.06] text-white/65 group-hover:bg-white/[0.09] group-hover:text-white/90"
                  }`}
                >
                  <Icon size={14} />
                </span>
                <span className="flex min-w-0 flex-col">
                  <span className="truncate">{label}</span>
                  {hint && (
                    <span className="whitespace-normal text-[11px] leading-snug text-white/35">
                      {hint}
                    </span>
                  )}
                </span>
              </button>
            </Fragment>
          ))}
          {(game.graphicsApiChoice || game.resourceQualityChoice) && (
            <div className="mx-[12px] my-[5px] h-px bg-white/[0.07]" />
          )}
          {game.graphicsApiChoice && (
            <DirectXRow
              gameId={activeId}
              installed={installed}
              forceDx11={forceDx11}
              customLauncher={customLauncher}
            />
          )}
          {game.resourceQualityChoice && (
            <ResourceQualityRow
              installed={installed}
              jobActive={jobActive}
              running={running}
              customLauncher={customLauncher}
              resourceQuality={resourceQuality}
            />
          )}
        </div>
      )}
    </ExpandUp>
  );
}

// ------------ Expand Up Panel ------------
// A button on the Home screen that opens a panel upwards from itself, like the playtime pill. Closes on Escape
// or a click outside.
export function ExpandUp({
  width,
  className,
  icon: Icon,
  label,
  triggerClass,
  trigger,
  children,
}: {
  width?: number | string;
  className?: string;
  icon?: LucideIcon;
  label?: string;
  triggerClass?: string;
  trigger?: ReactNode;
  children: ReactNode | ((close: () => void) => ReactNode);
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const contentId = useId();
  const [triggerH, setTriggerH] = useState(42);
  const reduce = useReducedMotionSafe();
  useClickOutside(ref, () => setOpen(false), open);

  useLayoutEffect(() => {
    if (btnRef.current) setTriggerH(btnRef.current.offsetHeight);
  }, []);

  useEscapeKey(() => {
    setOpen(false);
    btnRef.current?.focus();
  }, open);

  return (
    <div
      ref={ref}
      className={className ? `relative ${className}` : "relative"}
      style={{ width, height: triggerH }}
    >
      <div className="glass absolute bottom-0 left-0 right-0 flex flex-col overflow-hidden rounded-ui shadow-2xl">
        <Collapse open={open} id={contentId}>
          {typeof children === "function" ? children(() => setOpen(false)) : children}
          <div className="mx-[12px] h-px bg-white/[0.08]" />
        </Collapse>
        <button
          ref={btnRef}
          aria-expanded={open}
          aria-controls={open ? contentId : undefined}
          onClick={() => setOpen((o) => !o)}
          className={
            triggerClass ??
            "expand-trigger flex h-[46px] w-full items-center gap-[10px] px-[13px] text-white"
          }
        >
          {Icon ? (
            <>
              <span className="grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] bg-white/[0.07] text-white/70">
                <Icon size={15} />
              </span>
              <span className="flex-1 truncate text-left text-[13px] font-medium">{label}</span>
              <m.span
                className="shrink-0 text-white/45"
                animate={{ rotate: open ? 180 : 0 }}
                transition={reduce ? { duration: 0 } : springThumb}
              >
                <ChevronUp size={16} />
              </m.span>
            </>
          ) : (
            trigger
          )}
        </button>
      </div>
    </div>
  );
}
