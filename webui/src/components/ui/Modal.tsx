// ------------ Modals ------------
// The dialog shell, the root that shows whichever dialog is open (a confirm or the install dialog), and the
// confirm dialog's button styles.
import { useId, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, m } from "framer-motion";
import { useEscapeKey } from "../../lib/hooks";
import { useFocusTrap } from "../../lib/useFocusTrap";
import { EASE_STANDARD, springFlow, springPour, useReducedMotionSafe } from "../../lib/motion";
import { AlertTriangle } from "lucide-react";
import { useModalStore } from "../../store/modalStore";
import { InstallModalBody } from "./InstallModalBody";
import { useT } from "../../lib/useT";

// ------------ Modal ------------
// The dialog box shell: dims the window, keeps keyboard focus inside, closes on Escape or a click outside, and
// labels itself for screen readers.
const ENTER = { duration: 0.18, ease: EASE_STANDARD } as const;
const EXIT = { duration: 0.14, ease: [0.4, 0, 1, 1] } as const;

const GLASS_ON = {
  backgroundColor: "rgba(18, 18, 22, 0.6)",
  backdropFilter: "blur(20px) saturate(180%)",
  borderColor: "rgba(255, 255, 255, 0.1)",
  boxShadow: "0 25px 50px -12px rgba(0, 0, 0, 0.25)",
};
const GLASS_OFF = {
  backgroundColor: "rgba(18, 18, 22, 0)",
  backdropFilter: "blur(0px) saturate(100%)",
  borderColor: "rgba(255, 255, 255, 0)",
  boxShadow: "0 25px 50px -12px rgba(0, 0, 0, 0)",
};

export function Modal({
  open,
  onClose,
  children,
  width = 430,
  labelledBy,
  describedBy,
  dismissOnBackdrop = true,
  grow = true,
}: {
  open: boolean;
  onClose: () => void;
  children: ReactNode;
  width?: number;
  labelledBy?: string;
  describedBy?: string;
  dismissOnBackdrop?: boolean;
  grow?: boolean;
}) {
  const panelRef = useRef<HTMLDivElement>(null);
  const autoTitleId = useId();

  useEscapeKey(onClose, open);
  useFocusTrap(panelRef, open);

  useLayoutEffect(() => {
    const panel = panelRef.current;
    if (!open || labelledBy || !panel) return;
    const label = () => {
      const heading = panel.querySelector<HTMLElement>("h1, h2, h3");
      if (!heading) {
        panel.removeAttribute("aria-labelledby");
        return;
      }
      if (!heading.id) heading.id = autoTitleId;
      panel.setAttribute("aria-labelledby", heading.id);
    };
    label();
    const observer = new MutationObserver(label);
    observer.observe(panel, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, [open, labelledBy, autoTitleId]);

  return createPortal(
    <AnimatePresence>
      {open && (
        <div className="fixed inset-0 z-(--z-modal) grid place-items-center px-[24px]">
          <m.div
            aria-hidden
            className="absolute inset-0 bg-black/55"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0, transition: EXIT }}
            transition={ENTER}
            onMouseDown={dismissOnBackdrop ? onClose : undefined}
          />
          <ModalPanel
            panelRef={panelRef}
            width={width}
            labelledBy={labelledBy}
            describedBy={describedBy}
            grow={grow}
          >
            {children}
          </ModalPanel>
        </div>
      )}
    </AnimatePresence>,
    document.body,
  );
}

// ------------ Modal Panel ------------
// The dialog box itself. It grows out of the button that opened it (the one with focus when it appears) and shrinks
// back into it on close. A dialog that opens on its own, with nothing focused, or that asks not to grow (the install
// dialog), rises in place as before.
type Offset = { x: number; y: number; scale: number } | null;

function triggerOffset(width: number): Offset {
  const trigger = document.activeElement;
  if (!(trigger instanceof HTMLElement) || trigger === document.body) return null;
  const r = trigger.getBoundingClientRect();
  if (r.width === 0) return null;
  return {
    x: r.left + r.width / 2 - window.innerWidth / 2,
    y: r.top + r.height / 2 - window.innerHeight / 2,
    scale: Math.min(0.6, Math.max(0.15, r.width / width)),
  };
}

function ModalPanel({
  panelRef,
  width,
  labelledBy,
  describedBy,
  grow,
  children,
}: {
  panelRef: RefObject<HTMLDivElement | null>;
  width: number;
  labelledBy?: string;
  describedBy?: string;
  grow: boolean;
  children: ReactNode;
}) {
  const reduce = useReducedMotionSafe();
  const [from] = useState(() => (reduce || !grow ? null : triggerOffset(width)));
  const enter = from ? { default: ENTER, x: springFlow, y: springFlow, scale: springFlow } : ENTER;
  return (
    <m.div
      ref={panelRef}
      role="dialog"
      aria-modal="true"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      tabIndex={-1}
      className="relative max-h-[88vh] max-w-full overflow-y-auto overscroll-contain rounded-ui border outline-none"
      style={{ width }}
      initial={{ ...GLASS_OFF, ...(from ?? { scale: 0.96, x: 0, y: 10 }) }}
      animate={{ ...GLASS_ON, scale: 1, x: 0, y: 0 }}
      exit={{
        ...GLASS_OFF,
        ...(from ?? { scale: 0.97, x: 0, y: 8 }),
        transition: from ? { default: EXIT, x: springPour, y: springPour, scale: springPour } : EXIT,
      }}
      transition={enter}
    >
      <m.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{ opacity: 0, transition: EXIT }}
        transition={from ? { ...ENTER, delay: 0.08 } : ENTER}
      >
        {children}
      </m.div>
    </m.div>
  );
}

// ------------ Modal Root ------------
// Where dialogs show up. Reads the modal store and draws either a confirm dialog (with an optional red danger
// style) or the install dialog.
export function ModalRoot() {
  const t = useT();
  const modal = useModalStore((s) => s.modal);
  const close = useModalStore((s) => s.close);
  const titleId = useId();
  const messageId = useId();
  const isConfirm = modal?.kind === "confirm";
  const infoOnly = modal?.kind === "confirm" && modal.cancelLabel === null;
  const confirm = () => {
    if (modal?.kind === "confirm") modal.onConfirm();
    close();
  };

  return (
    <Modal
      open={!!modal}
      onClose={infoOnly ? confirm : close}
      width={modal?.kind === "install" ? 780 : 420}
      grow={modal?.kind !== "install"}
      labelledBy={isConfirm ? titleId : undefined}
      describedBy={isConfirm ? messageId : undefined}
    >
      {modal?.kind === "confirm" && (
        <div className="p-[22px]">
          <div className="mb-[8px] flex items-center gap-[10px]">
            {modal.danger && <AlertTriangle size={18} className="shrink-0 text-red-400" />}
            <h2 id={titleId} className="text-[16px] font-semibold">
              {modal.title}
            </h2>
          </div>
          <p id={messageId} className="mb-[20px] text-[13px] leading-[1.55] text-white/65">
            {modal.message}
          </p>
          <div className="flex justify-end gap-[10px]">
            {!infoOnly && (
              <button onClick={close} className={MODAL_SECONDARY}>
                {modal.cancelLabel ?? t("common.cancel")}
              </button>
            )}
            <button
              autoFocus={!modal.danger || infoOnly}
              onClick={confirm}
              className={modal.danger ? MODAL_DANGER : MODAL_PRIMARY}
            >
              {modal.confirmLabel ?? t("common.confirm")}
            </button>
          </div>
        </div>
      )}

      {modal?.kind === "install" && <InstallModalBody gameId={modal.gameId} onClose={close} />}
    </Modal>
  );
}

// ------------ Modal Button Styles ------------
// Looks for the buttons at the bottom of the confirm dialog: secondary (Cancel), primary and danger.
const MODAL_SECONDARY =
  "rounded-ui px-[16px] py-[8px] text-[13px] font-medium text-white/70 transition duration-150 hover:bg-white/[0.08] hover:text-white active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-40";
const MODAL_PRIMARY =
  "flex items-center gap-[8px] rounded-ui bg-white px-[18px] py-[8px] text-[13px] font-semibold text-black transition duration-150 hover:bg-white/90 active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-40";
const MODAL_DANGER =
  "flex items-center gap-[8px] rounded-ui bg-red-500/90 px-[18px] py-[8px] text-[13px] font-semibold text-white transition duration-150 hover:bg-red-500 active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-40";
