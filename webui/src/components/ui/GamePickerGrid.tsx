// ------------ Game Picker Grid ------------
// The row of game tiles at the top of the Mods tab. Click one to switch to that game, with a small status line
// under each. Its tile border style is also reused by the first run setup.
import { GAMES, gameById, type GameId } from "../../data/games";
import { useUiStore } from "../../store/uiStore";
import { useCustomizationStore } from "../../store/customizationStore";
import { iconFallback, iconSrcFor } from "../../lib/customMedia";
import { IconButton } from "./Tooltip";

const TILE_BORDER_ON = "border-(--accent-b)/55";

const TILE_BORDER_OFF =
  "border-white/[0.08] bg-white/[0.03] hover:border-white/20 hover:bg-white/[0.06]";

export const tileBorder = (active: boolean): string =>
  active ? TILE_BORDER_ON : TILE_BORDER_OFF;

const MAX_PER_ROW = 6;

function columnsFor(count: number): number {
  const rows = Math.max(1, Math.ceil(count / MAX_PER_ROW));
  return Math.max(1, Math.ceil(count / rows));
}

export function GamePickerGrid({
  ids,
  subtitleFor,
}: {
  ids?: GameId[];
  subtitleFor: (id: GameId) => { text: string; tone: "on" | "off" };
}) {
  const activeId = useUiStore((s) => s.activeGameId);
  const setActiveGame = useUiStore((s) => s.setActiveGame);
  const icons = useCustomizationStore((s) => s.gameIcons);

  const games = ids ? ids.map(gameById) : GAMES;

  const selectGame = (id: GameId) => {
    if (id === activeId) return;
    setActiveGame(id);
  };

  return (
    <div
      className="mb-4 grid gap-2"
      style={{ gridTemplateColumns: `repeat(${columnsFor(games.length)}, minmax(0, 1fr))` }}
    >
      {games.map((g) => {
        const on = g.id === activeId;
        const sub = subtitleFor(g.id);
        return (
          <IconButton
            key={g.id}
            onClick={() => selectGame(g.id)}
            aria-pressed={on}
            tip={g.name}
            className={`flex min-w-0 items-center gap-[10px] rounded-ui border p-[10px] text-left transition duration-150 active:scale-[0.97] ${tileBorder(on)}`}
            style={on ? { background: "rgba(255, 255, 255, 0.07)" } : undefined}
          >
            <img
              src={iconSrcFor(g.id, icons)}
              onError={iconFallback(g.id)}
              alt=""
              className={`h-[38px] w-[38px] shrink-0 rounded-[9px] object-cover transition-opacity ${
                on ? "" : "opacity-80"
              }`}
            />
            <div className="min-w-0">
              <div className={`truncate text-[13px] font-medium ${on ? "text-white" : "text-white/75"}`}>
                {g.short ?? g.name}
              </div>
              <div
                className={`truncate text-[11px] leading-[1.4] ${
                  sub.tone === "on" ? "text-emerald-300/80" : "text-white/55"
                }`}
              >
                {sub.text}
              </div>
            </div>
          </IconButton>
        );
      })}
    </div>
  );
}
