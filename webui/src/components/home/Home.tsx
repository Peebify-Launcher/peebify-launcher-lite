// ------------ Home ------------
// The Home screen and its socials tray.
import { useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import { useSettingsStore } from "../../store/settingsStore";
import { fadeVariants, useReducedMotionSafe, pageVariantsSolid, EASE_STANDARD } from "../../lib/motion";
import { NewsPanel } from "./NewsPanel";
import { StartBar } from "./StartBar";
import { BottomRightActions } from "./BottomRightActions";
import { useUiStore } from "../../store/uiStore";
import { useGameLinksStore } from "../../store/remoteContentStore";
import { openExternal } from "../../lib/tauri";

// ------------ Game Home ------------
// The Home screen for the selected game. Lays out the socials tray, news panel, start bar and bottom right
// buttons on top of the wallpaper, and hides whichever ones the user turned off in settings.
export function GameHome() {
  const hideSocials = useSettingsStore((s) => s.values.hideSocials === "true");
  const hideNewsPanel = useSettingsStore((s) => s.values.hideNewsPanel === "true");
  const hidePlaytime = useSettingsStore((s) => s.values.hidePlaytime === "true");
  const hideBottomRight = useSettingsStore((s) => s.values.hideBottomRightButtons === "true");
  const reduce = useReducedMotionSafe();
  const fade = {
    variants: fadeVariants,
    custom: reduce ? 0 : 0.15,
    initial: "initial",
    animate: "animate",
    exit: "exit",
  };

  return (
    <m.div variants={pageVariantsSolid} className="absolute inset-0">
      <AnimatePresence initial={false}>
        {!hideSocials && (
          <m.div key="socials" {...fade}>
            <SocialsTray />
          </m.div>
        )}
        {!hideBottomRight && (
          <m.div key="actions" {...fade}>
            <BottomRightActions />
          </m.div>
        )}
      </AnimatePresence>

      <div className="absolute bottom-[22px] left-[18px] z-[6] flex flex-col items-start gap-[12px]">
        <AnimatePresence initial={false}>
          {!hideNewsPanel && (
            <m.div key="news" {...fade}>
              <m.div layout>
                <NewsPanel />
              </m.div>
            </m.div>
          )}
        </AnimatePresence>
        <m.div layout="position">
          <StartBar hidePlaytime={hidePlaytime} />
        </m.div>
      </div>
    </m.div>
  );
}

// ------------ Socials Tray ------------
// The slim strip of social icons (Discord, YouTube, X and so on) on the Home screen. Expands to show names on
// hover and opens the link in the browser. Hidden when the game has none.
export const BRAND_PATHS: Record<string, string> = {
  discord:
    "M20.317 4.3698a19.7913 19.7913 0 00-4.8851-1.5152.0741.0741 0 00-.0785.0371c-.211.3753-.4447.8648-.6083 1.2495-1.8447-.2762-3.68-.2762-5.4868 0-.1636-.3933-.4058-.8742-.6177-1.2495a.077.077 0 00-.0785-.037 19.7363 19.7363 0 00-4.8852 1.515.0699.0699 0 00-.0321.0277C.5334 9.0458-.319 13.5799.0992 18.0578a.0824.0824 0 00.0312.0561c2.0528 1.5076 4.0413 2.4228 5.9929 3.0294a.0777.0777 0 00.0842-.0276c.4616-.6304.8731-1.2952 1.226-1.9942a.076.076 0 00-.0416-.1057c-.6528-.2476-1.2743-.5495-1.8722-.8923a.077.077 0 01-.0076-.1277c.1258-.0943.2517-.1923.3718-.2914a.0743.0743 0 01.0776-.0105c3.9278 1.7933 8.18 1.7933 12.0614 0a.0739.0739 0 01.0785.0095c.1202.099.246.1981.3728.2924a.077.077 0 01-.0066.1276 12.2986 12.2986 0 01-1.873.8914.0766.0766 0 00-.0407.1067c.3604.698.7719 1.3628 1.225 1.9932a.076.076 0 00.0842.0286c1.961-.6067 3.9495-1.5219 6.0023-3.0294a.077.077 0 00.0313-.0552c.5004-5.177-.8382-9.6739-3.5485-13.6604a.061.061 0 00-.0312-.0286zM8.02 15.3312c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9555-2.4189 2.157-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.9555 2.4189-2.1569 2.4189zm7.9748 0c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9554-2.4189 2.1569-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.946 2.4189-2.1568 2.4189Z",
  youtube:
    "M23.498 6.186a3.016 3.016 0 0 0-2.122-2.136C19.505 3.545 12 3.545 12 3.545s-7.505 0-9.377.505A3.017 3.017 0 0 0 .502 6.186C0 8.07 0 12 0 12s0 3.93.502 5.814a3.016 3.016 0 0 0 2.122 2.136c1.871.505 9.376.505 9.376.505s7.505 0 9.377-.505a3.015 3.015 0 0 0 2.122-2.136C24 15.93 24 12 24 12s0-3.93-.502-5.814zM9.545 15.568V8.432L15.818 12l-6.273 3.568z",
  x: "M18.901 1.153h3.68l-8.04 9.19L24 22.846h-7.406l-5.8-7.584-6.638 7.584H.474l8.6-9.83L0 1.154h7.594l5.243 6.932ZM17.61 20.644h2.039L6.486 3.24H4.298Z",
};

const SOCIAL_META: Record<string, { label: string; icon?: string }> = {
  discord: { label: "Discord" }, // i18n-ignore
  youtube: { label: "YouTube" }, // i18n-ignore
  x: { label: "X" },
  lunite: { label: "Lunite", icon: "/icons/socials/lunite.webp" }, // i18n-ignore
  hoyolab: { label: "HoYoLAB", icon: "/icons/socials/accounts.webp" }, // i18n-ignore
};

const COLLAPSED = 56;
const EXPANDED = 186;

function SocialsTray() {
  const activeId = useUiStore((s) => s.activeGameId);
  const socials = useGameLinksStore((s) => s.links[activeId]?.socials) ?? [];
  const known = socials.filter((s) => SOCIAL_META[s.platform]);
  const [open, setOpen] = useState(false);
  const reduce = useReducedMotionSafe();

  if (known.length === 0) return null;

  return (
    <m.div
      onHoverStart={() => setOpen(true)}
      onHoverEnd={() => setOpen(false)}
      onFocusCapture={() => setOpen(true)}
      onBlurCapture={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOpen(false);
      }}
      animate={{ width: open ? EXPANDED : COLLAPSED }}
      initial={false}
      transition={{ duration: reduce ? 0 : 0.28, ease: EASE_STANDARD }}
      className="glass absolute right-0 top-[45%] z-[5] flex -translate-y-1/2 flex-col overflow-hidden py-[7px]"
      style={{ borderRadius: "11px 0 0 11px", borderRight: "none" }}
    >
      {known.map((s) => {
        const meta = SOCIAL_META[s.platform];
        const path = BRAND_PATHS[s.platform];
        return (
          <button
            key={s.platform}
            aria-label={meta.label}
            onClick={() => void openExternal(s.url)}
            className="mx-[6px] my-[2px] flex h-[44px] shrink-0 items-center gap-[12px] rounded-[8px] px-[10px] text-white/90 transition-colors hover:bg-white/10 hover:text-white"
          >
            <span className="grid h-[24px] w-[24px] shrink-0 place-items-center">
              {path ? (
                <svg viewBox="0 0 24 24" className="h-[22px] w-[22px]" fill="currentColor" aria-hidden>
                  <path d={path} />
                </svg>
              ) : (
                <img src={meta.icon} alt="" className="h-7 w-7 max-w-none object-contain" />
              )}
            </span>
            <span
              aria-hidden={!open}
              className="whitespace-nowrap text-[13px] font-medium transition-opacity duration-200"
              style={{ opacity: open ? 1 : 0 }}
            >
              {meta.label}
            </span>
          </button>
        );
      })}
    </m.div>
  );
}
