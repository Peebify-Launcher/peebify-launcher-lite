// ------------ Notifications ------------
// The notification list and toasts, and the icon and colors each notification type is drawn with.
import { create } from "zustand";
import { Info, CheckCircle2, AlertTriangle, XCircle, Activity } from "lucide-react";

// ------------ Notification Store ------------
// The notification list and the short lived toasts. Repeated errors in a row are collapsed so they
// don't flood the screen.
type NotifType = "info" | "progress" | "success" | "warning" | "error";

interface NotifAction {
  label: string;
  run: () => void;
}

export interface Notification {
  id: string;
  type: NotifType;
  title: string;
  text: string;
  time: number;
  read: boolean;
  action?: NotifAction;
  key?: string;
}

export const jobFailureKey = (gameId: string): string => `job-failure:${gameId}`;

const ERROR_DEDUPE_MS = 5000;

let counter = 0;
const genId = () => `n${Date.now()}_${counter++}`;

interface NotifState {
  items: Notification[];
  toasts: Notification[];
  open: boolean;
  push: (n: {
    type?: NotifType;
    title: string;
    text: string;
    action?: NotifAction;
    key?: string;
  }) => void;
  toast: (n: { type?: NotifType; title: string; text: string; action?: NotifAction }) => void;
  dismiss: (id: string) => void;
  dismissToast: (id: string) => void;
  clearAll: () => void;
  togglePanel: () => void;
  setOpen: (open: boolean) => void;
}

export const useNotificationStore = create<NotifState>((set) => ({
  items: [],
  toasts: [],
  open: false,
  push: (n) =>
    set((s) => {
      const type = n.type ?? "info";
      const now = Date.now();
      if (
        type === "error" &&
        s.items.some(
          (i) =>
            i.type === "error" &&
            (i.text === n.text || (!!n.key && i.key === n.key)) &&
            now - i.time < ERROR_DEDUPE_MS,
        )
      ) {
        return s;
      }
      const item: Notification = {
        id: genId(),
        type,
        title: n.title,
        text: n.text,
        time: now,
        read: s.open,
        action: n.action,
        key: n.key,
      };
      return {
        items: [item, ...s.items].slice(0, 50),
        toasts: s.open ? s.toasts : [...s.toasts, item].slice(-4),
      };
    }),
  toast: (n) =>
    set((s) => {
      if (s.open) return s;
      const item: Notification = {
        id: genId(),
        type: n.type ?? "info",
        title: n.title,
        text: n.text,
        time: Date.now(),
        read: true,
        action: n.action,
      };
      return { toasts: [...s.toasts, item].slice(-4) };
    }),
  dismiss: (id) =>
    set((s) => ({
      items: s.items.filter((i) => i.id !== id),
      toasts: s.toasts.filter((i) => i.id !== id),
    })),
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((i) => i.id !== id) })),
  clearAll: () => set({ items: [], toasts: [] }),
  togglePanel: () =>
    set((s) => ({
      open: !s.open,
      items: s.open ? s.items : s.items.map((i) => ({ ...i, read: true })),
      toasts: s.open ? s.toasts : [],
    })),
  setOpen: (open) =>
    set((s) => ({
      open,
      items: open ? s.items.map((i) => ({ ...i, read: true })) : s.items,
      toasts: open ? [] : s.toasts,
    })),
}));

// ------------ Notification Icons ------------
// The icon and colors for each notification type: blue for information, violet for work under way, green when it
// finished, amber when it needs a look and red when it failed. Each type has its own icon too, so the color is
// never the only difference. `tint` and `edge` color an unread row in the notification panel.
export const NOTIF_ICONS: Record<NotifType, typeof Info> = {
  info: Info,
  progress: Activity,
  success: CheckCircle2,
  warning: AlertTriangle,
  error: XCircle,
};

export const NOTIF_ICON_COLORS: Record<NotifType, { bg: string; fg: string; tint: string; edge: string }> = {
  info: { bg: "rgba(96,165,250,.18)", fg: "#93c5fd", tint: "rgba(96,165,250,.07)", edge: "rgba(96,165,250,.2)" },
  progress: { bg: "rgba(167,139,250,.18)", fg: "#c4b5fd", tint: "rgba(167,139,250,.07)", edge: "rgba(167,139,250,.2)" },
  success: { bg: "rgba(52,211,153,.18)", fg: "#6ee7b7", tint: "rgba(52,211,153,.07)", edge: "rgba(52,211,153,.2)" },
  warning: { bg: "rgba(250,204,21,.18)", fg: "#fcd34d", tint: "rgba(250,204,21,.07)", edge: "rgba(250,204,21,.2)" },
  error: { bg: "rgba(239,68,68,.18)", fg: "#fca5a5", tint: "rgba(239,68,68,.07)", edge: "rgba(239,68,68,.22)" },
};
