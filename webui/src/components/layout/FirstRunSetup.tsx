// ------------ First Run Setup ------------
// The "Which games do you play?" screen shown the first time the launcher opens. Games already installed are
// pre-ticked, and the ones picked become the sidebar. Can be changed later in Settings under Games.
import { useEffect, useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import { Check } from "lucide-react";
import { GAMES, type GameId } from "../../data/games";
import { useGamesStore } from "../../store/gamesStore";
import { useLibraryStore } from "../../store/libraryStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { iconFallback, iconSrcFor } from "../../lib/customMedia";
import { tileBorder } from "../ui/GamePickerGrid";
import { fadeVariants } from "../../lib/motion";
import { useT } from "../../lib/useT";
import { WindowControls } from "./WindowControls";

const ACCENT_TINT =
  "linear-gradient(135deg, rgba(var(--accent-a-rgb), 0.13), rgba(var(--accent-b-rgb), 0.13))";

export function FirstRunSetup() {
  const t = useT();
  const finishSetup = useLibraryStore((s) => s.finishSetup);
  const installedIds = useGamesStore((s) => s.installed);
  const icons = useCustomizationStore((s) => s.gameIcons);

  const [picked, setPicked] = useState<GameId[]>([]);
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    if (touched) return;
    setPicked(GAMES.map((g) => g.id).filter((id) => installedIds.includes(id)));
  }, [installedIds, touched]);

  const toggle = (id: GameId) => {
    setTouched(true);
    setPicked((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  };

  return (
    <m.div
      initial={false}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0, transition: { duration: 0.25 } }}
      role="dialog"
      aria-modal="true"
      aria-labelledby="setup-title"
      className="absolute inset-0 z-(--z-setup) overflow-hidden"
      style={{
        background: "rgba(10,10,13,0.86)",
        backdropFilter: "blur(28px) saturate(160%)",
        WebkitBackdropFilter: "blur(28px) saturate(160%)",
      }}
    >
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0"
        style={{
          background:
            "radial-gradient(760px 520px at 16% 10%, rgba(var(--accent-b-rgb), 0.10), transparent 68%)",
        }}
      />
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0"
        style={{
          background:
            "linear-gradient(118deg, rgba(0,0,0,0.45) 0%, rgba(0,0,0,0.05) 46%, rgba(0,0,0,0.42) 100%)",
        }}
      />

      <div className="relative flex h-full w-full">
        <aside className="flex w-[264px] shrink-0 flex-col border-r border-white/[0.07] bg-black/25 px-7 py-8 xl:w-[328px] xl:px-9 xl:py-10">
          <div className="flex shrink-0 items-center gap-[10px]">
            <img
              src="/icons/app.png"
              alt=""
              className="h-[34px] w-[34px] rounded-[9px] object-contain"
            />
            <span className="text-[15px] font-semibold tracking-[-0.01em]">
              {"Peebify" /* i18n-ignore */}
            </span>
            <span className="ml-auto rounded-full border border-white/[0.10] bg-white/[0.05] px-[9px] py-[3px] text-[10.5px] font-medium uppercase tracking-[0.08em] text-white/45">
              {t("setup.badge")}
            </span>
          </div>

          <div className="mt-auto pt-6">
            <AnimatePresence mode="wait" initial={false}>
              <m.p
                key={`games-${picked.length > 0}`}
                variants={fadeVariants}
                initial="initial"
                animate="animate"
                exit="exit"
                className="text-[12px] leading-[1.5] text-white/35"
              >
                {picked.length > 0
                  ? t("setup.status.picked", { count: picked.length })
                  : t("setup.status.nothingPickedFinish")}
              </m.p>
            </AnimatePresence>
          </div>
        </aside>

        <div className="flex min-w-0 flex-1 flex-col px-9 py-8 xl:px-[64px] xl:py-12">
          <div className="flex min-h-0 flex-1 flex-col">
            <h1
              id="setup-title"
              className="shrink-0 text-[28px] font-semibold tracking-[-0.02em] xl:text-[34px]"
            >
              {t("setup.title.games")}
            </h1>
            <p className="mt-2.5 max-w-[620px] shrink-0 text-[13px] leading-[1.55] text-white/50 xl:text-[14px]">
              {t("setup.blurb.games")}
            </p>

            <div className="my-7 grid min-h-0 flex-1 grid-cols-2 content-start gap-2 overflow-y-auto pr-1 lg:grid-cols-3 xl:grid-cols-4">
              {GAMES.map((game) => {
                const on = picked.includes(game.id);
                return (
                  <div key={game.id}>
                    <button
                      onClick={() => toggle(game.id)}
                      aria-pressed={on}
                      className={`relative flex h-full w-full items-center gap-[10px] rounded-ui border p-[11px] text-left transition duration-150 active:scale-[0.98] ${tileBorder(on)}`}
                      style={on ? { background: ACCENT_TINT } : undefined}
                    >
                      <img
                        src={iconSrcFor(game.id, icons)}
                        onError={iconFallback(game.id)}
                        alt=""
                        className={`h-[38px] w-[38px] shrink-0 rounded-[9px] object-cover transition-opacity ${
                          on ? "" : "opacity-70"
                        }`}
                      />
                      <div className="min-w-0">
                        <div
                          className={`truncate text-[12.5px] font-medium ${on ? "text-white" : "text-white/75"}`}
                        >
                          {game.name}
                        </div>
                        {installedIds.includes(game.id) && (
                          <div className="text-[11px] text-emerald-300/80">{t("setup.games.installed")}</div>
                        )}
                      </div>
                    </button>
                  </div>
                );
              })}
            </div>
          </div>

          <div className="flex shrink-0 items-center justify-end gap-4 border-t border-white/[0.06] pt-5">
            <button
              onClick={() => finishSetup(picked)}
              className="accent-grad flex items-center gap-[8px] rounded-ui px-[18px] py-[9px] text-[13px] font-semibold text-(--on-accent) transition duration-150 hover:opacity-90 active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("setup.action.finish")}
              <Check size={15} strokeWidth={2.6} />
            </button>
          </div>
        </div>
      </div>
      <div
        className="drag-region absolute left-0 top-0 z-(--z-chrome) h-[32px]"
        style={{ right: 124 }}
      />
      <WindowControls hideBell />
    </m.div>
  );
}
