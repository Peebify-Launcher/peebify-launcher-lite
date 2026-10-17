// ------------ Modal Store ------------
// Which dialog is open: a confirm box or the install dialog for a game.
import { create } from "zustand";
import { type GameId, isManaged } from "../data/games";
import { peekInstall, warmInstall, whenInstallReady } from "../lib/installPreload";

let installOpenToken = 0;

interface ConfirmOpts {
  title: string;
  message: string;
  confirmLabel?: string;
  cancelLabel?: string | null;
  danger?: boolean;
  onConfirm: () => void;
}

type ModalState =
  | ({ kind: "confirm" } & ConfirmOpts)
  | { kind: "install"; gameId: GameId }
  | null;

interface ModalStore {
  modal: ModalState;
  openConfirm: (o: ConfirmOpts) => void;
  openInstall: (gameId: GameId) => void;
  close: () => void;
}

export const useModalStore = create<ModalStore>((set, get) => ({
  modal: null,
  openConfirm: (o) => set({ modal: { kind: "confirm", ...o } }),
  openInstall: (gameId) => {
    const token = ++installOpenToken;
    if (!isManaged(gameId) || peekInstall(gameId)) {
      warmInstall(gameId);
      set({ modal: { kind: "install", gameId } });
      return;
    }
    void whenInstallReady(gameId).then(() => {
      if (token === installOpenToken && !get().modal) set({ modal: { kind: "install", gameId } });
    });
  },
  close: () => set({ modal: null }),
}));
