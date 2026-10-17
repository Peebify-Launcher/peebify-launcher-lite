// ------------ Mods Opt-In ------------
// What the Mods tab shows while mod support is off: what mods add, the games they work with, and the risk right
// beside the button that turns them on.
import { AlertTriangle, Layers, Search, type LucideIcon } from "lucide-react";
import { gameById, type GameId } from "../../data/games";
import { useCustomizationStore } from "../../store/customizationStore";
import { iconFallback, iconSrcFor } from "../../lib/customMedia";
import { MOD_RISK_WARNING } from "../../lib/mods";
import { useT } from "../../lib/useT";
import type { TKey } from "../../lib/i18n";

const PANEL = "rounded-ui border border-white/[0.08] bg-white/[0.05]";

const FEATURES: { icon: LucideIcon; title: TKey; text: TKey }[] = [
  { icon: Layers, title: "mods.page.off.library", text: "mods.page.off.libraryText" },
  { icon: Search, title: "mods.page.off.browse", text: "mods.page.off.browseText" },
];

export function ModsOptIn({
  games,
  onTurnOn,
}: {
  games: { id: GameId; variant: string }[];
  onTurnOn: () => void;
}) {
  const t = useT();
  const icons = useCustomizationStore((s) => s.gameIcons);

  return (
    <div className="flex flex-col gap-[18px]">
      <section aria-labelledby="mods-off-title" className={`${PANEL} flex flex-wrap gap-x-11 gap-y-7 px-8 py-[30px]`}>
        <div className="flex min-w-0 flex-[1.25_1_400px] flex-col gap-[22px]">
          <div className="flex flex-col gap-[10px]">
            <span className="self-start rounded-[6px] bg-white/10 px-2 py-1 text-[10px] font-semibold uppercase leading-none tracking-[0.08em] text-white/75">
              {t("mods.page.off.badge")}
            </span>
            <h2 id="mods-off-title" className="text-[22px] font-semibold leading-tight">
              {t("mods.page.off.title")}
            </h2>
            <p className="max-w-[50ch] text-[14px] leading-[21px] text-white/80">{t("mods.page.off.body")}</p>
          </div>
          <div className="flex flex-col gap-4">
            {FEATURES.map(({ icon: Icon, title, text }) => (
              <div key={title} className="flex items-start gap-[14px]">
                <span className="grid h-9 w-9 shrink-0 place-items-center rounded-[8px] bg-white/[0.05] text-white/70">
                  <Icon size={16} />
                </span>
                <div className="flex flex-col gap-[2px]">
                  <span className="text-[14px] font-medium leading-5">{t(title)}</span>
                  <span className="text-[12.5px] leading-[1.375] text-white/55">{t(text)}</span>
                </div>
              </div>
            ))}
          </div>
        </div>

        {games.length > 0 && (
          <div className="flex min-w-0 flex-[1_1_320px] flex-col gap-3">
            <span className="text-[10px] font-semibold uppercase leading-none tracking-[0.08em] text-white/55">
              {t("mods.page.off.worksWith")}
            </span>
            <div className="grid grid-cols-2 gap-2">
              {games.map(({ id, variant }) => (
                <div
                  key={id}
                  className="flex min-w-0 items-center gap-[10px] rounded-ui border border-white/[0.08] bg-white/[0.05] px-3 py-[10px]"
                >
                  <img
                    src={iconSrcFor(id, icons)}
                    onError={iconFallback(id)}
                    alt=""
                    className="h-[30px] w-[30px] shrink-0 rounded-[8px] object-cover"
                  />
                  <div className="flex min-w-0 flex-col">
                    <span className="truncate text-[13px] font-medium leading-[18px]">{gameById(id).name}</span>
                    <span className="text-[11.5px] font-medium uppercase leading-[1.3] text-white/55">{variant}</span>
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}
      </section>

      <section
        aria-labelledby="mods-risk-title"
        className={`${PANEL} flex flex-wrap items-center gap-x-8 gap-y-5 px-8 py-6`}
      >
        <div className="flex min-w-0 flex-[999_1_420px] items-start gap-4">
          <span className="grid h-10 w-10 shrink-0 place-items-center rounded-[9px] bg-yellow-400/[0.18] text-amber-300">
            <AlertTriangle size={18} />
          </span>
          <div className="flex flex-col gap-[5px]">
            <h2 id="mods-risk-title" className="text-[15px] font-semibold leading-5">
              {t("mods.page.off.riskTitle")}
            </h2>
            <p id="mods-risk-text" className="max-w-[62ch] text-[13px] leading-[1.5] text-white/80">
              {t(MOD_RISK_WARNING)}
            </p>
          </div>
        </div>

        <button
          type="button"
          aria-describedby="mods-risk-text"
          onClick={onTurnOn}
          className="start-button start-button-idle h-[46px] w-[220px] shrink-0 rounded-[12px] bg-white text-[15px] font-semibold tracking-[-0.005em] text-[#111] active:scale-[0.975]"
        >
          {t("mods.page.off.turnOn")}
        </button>
      </section>
    </div>
  );
}
