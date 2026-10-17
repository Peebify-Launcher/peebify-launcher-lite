// ------------ Window Controls ------------
// The notification bell plus minimize, maximize and close buttons in the top right corner. The launcher draws
// its own title bar, so these replace the normal Windows ones.
import { createRef, type ComponentProps } from "react";
import { Bell, Minus, Square, Copy, X } from "lucide-react";
import { minimizeWindow, closeWindow } from "../../lib/tauri";
import { toggleMaximizeWindow } from "../../lib/ipc";
import { useNotificationStore } from "../../store/notifications";
import { useUiStore } from "../../store/uiStore";
import { IconButton } from "../ui/Tooltip";
import { useT } from "../../lib/useT";

export const notificationBellRef = createRef<HTMLButtonElement>();

const WinButton = (props: ComponentProps<typeof IconButton>) => (
  <IconButton
    {...props}
    className="relative grid h-[30px] w-[30px] place-items-center rounded-[7px] transition-colors hover:bg-white/10"
  />
);

export function WindowControls({ hideBell = false }: { hideBell?: boolean }) {
  const t = useT();
  const togglePanel = useNotificationStore((s) => s.togglePanel);
  const panelOpen = useNotificationStore((s) => s.open);
  const unread = useNotificationStore((s) => s.items.filter((i) => !i.read).length);
  const maximized = useUiStore((s) => s.maximized);
  const setMaximized = useUiStore((s) => s.setMaximized);

  return (
    <div
      data-window-controls
      className="glass absolute right-[14px] top-[12px] z-(--z-controls) flex items-center rounded-ui px-[6px] py-[2px]"
    >
      {!hideBell && (
        <WinButton
          label={t("common.notifications")}
          aria-label={unread > 0 ? t("layout.window.notificationsUnread", { count: unread }) : undefined}
          aria-expanded={panelOpen}
          buttonRef={notificationBellRef}
          onClick={togglePanel}
        >
          <Bell size={15} />
          {unread > 0 && (
            <span
              key={unread}
              className="badge-pop accent-grad absolute right-[1px] top-[1px] grid h-[15px] min-w-[15px] place-items-center rounded-full border-[1.5px] border-[rgba(18,18,22,0.8)] px-[3px] text-[9px] font-medium leading-none text-(--on-accent)"
            >
              {unread}
            </span>
          )}
        </WinButton>
      )}
      <WinButton label={t("common.minimize")} onClick={() => void minimizeWindow()}>
        <Minus size={15} />
      </WinButton>
      <WinButton
        label={maximized ? t("layout.window.restore") : t("common.maximize")}
        onClick={() => void toggleMaximizeWindow().then(setMaximized)}
      >
        {maximized ? <Copy size={14} className="-scale-x-100" /> : <Square size={14} />}
      </WinButton>
      <WinButton label={t("common.close")} onClick={() => void closeWindow()}>
        <X size={15} />
      </WinButton>
    </div>
  );
}
