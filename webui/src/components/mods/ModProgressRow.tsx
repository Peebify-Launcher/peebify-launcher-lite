// ------------ Mod Progress Row ------------
// A thin progress bar with a message that shows while a mod is being downloaded or installed for this game.
import { useModsStore } from "../../store/mods";
import { ProgressBar } from "../ui/panels";

export function ModProgressRow({ gameId }: { gameId: string }) {
  const progress = useModsStore((s) => s.progress);
  if (!progress || progress.gameId !== gameId) return null;

  const percent = Math.max(0, Math.min(100, progress.percentage));
  return (
    <div className="mb-4 rounded-ui border border-white/[0.08] bg-white/[0.04] px-4 py-3">
      <div className="mb-2 flex items-center justify-between text-[12.5px] text-white/60">
        <span className="truncate">{progress.message}</span>
        <span className="ml-3 shrink-0 tabular-nums">{Math.round(percent)}%</span>
      </div>
      <ProgressBar value={percent} size="sm" ariaLabel={progress.message} />
    </div>
  );
}
