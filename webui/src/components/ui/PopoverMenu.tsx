// ------------ Popover Menu ------------
// The look every pop-out in the launcher shares, the panel they open in, a pop-out menu and its keyboard handling.
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { createPortal } from "react-dom";
import { Collapse } from "../../lib/motion";
import { useClickOutside, useDismissOnViewportChange, useEscapeKey } from "../../lib/hooks";

// ------------ Menu Look ------------
// Every dropdown and pop-out looks like the Home page's Quick settings and Community tools (BottomRightActions): the
// same glass panel, rows with a soft highlight and an icon tile, and a hairline under the button they open from.
export const MENU_PANEL = "glass overflow-hidden rounded-ui shadow-2xl";
export const MENU_DIVIDER = "mx-[12px] h-px bg-white/[0.08]";

export const menuRow = (danger = false) =>
  `group flex w-full items-center gap-[10px] px-[12px] py-[7px] text-left text-[13px] transition-colors hover:bg-white/[0.06] ${
    danger ? "text-red-300" : "text-white/85"
  }`;

export const menuIcon = (danger = false) =>
  `grid h-[28px] w-[28px] shrink-0 place-items-center rounded-[8px] transition-colors ${
    danger
      ? "bg-red-500/[0.12] text-red-300/90"
      : "bg-white/[0.06] text-white/65 group-hover:bg-white/[0.09] group-hover:text-white/90"
  }`;

export function popoutRoot(): HTMLElement {
  return document.querySelector<HTMLElement>("[data-window-root]") ?? document.body;
}

// ------------ Drop Panel ------------
// The panel a pop-out opens in, on top of everything. It grows out of the edge facing its button on the same springs
// as Quick settings (Collapse) and pours back in before it goes. Its height moves, never its opacity: fading a blurred
// pane makes the blur snap into place when the fade ends.
export function DropPanel({
  open,
  panelRef,
  style,
  role,
  ariaLabel,
  onKeyDown,
  onClickCapture,
  children,
}: {
  open: boolean;
  panelRef: RefObject<HTMLDivElement | null>;
  style: CSSProperties;
  role: "menu" | "dialog";
  ariaLabel?: string;
  onKeyDown?: (e: KeyboardEvent<HTMLDivElement>) => void;
  onClickCapture?: (e: MouseEvent<HTMLDivElement>) => void;
  children: ReactNode;
}) {
  const [shown, setShown] = useState(open);
  if (open && !shown) setShown(true);
  if (!shown) return null;
  return createPortal(
    <div
      ref={panelRef}
      role={role}
      aria-label={ariaLabel}
      onKeyDown={onKeyDown}
      onClickCapture={onClickCapture}
      style={style}
      className={`z-(--z-tooltip) ${MENU_PANEL}`}
    >
      <Collapse open={open} initial onExitComplete={() => setShown(false)}>
        {children}
      </Collapse>
    </div>,
    popoutRoot(),
  );
}

// ------------ Menu ------------
// A small menu that pops open next to a button and flips above it if there is no room below. Supports arrow keys
// and closes on Escape or a click outside.
function focusAnchor(anchor: HTMLElement | null) {
  const target = anchor?.matches("button") ? anchor : anchor?.querySelector<HTMLElement>("button");
  target?.focus({ preventScroll: true });
}

export function PopoverMenu({
  open,
  onClose,
  anchorRef,
  width,
  estimatedHeight,
  children,
}: {
  open: boolean;
  onClose: () => void;
  anchorRef: RefObject<HTMLElement | null>;
  width: number;
  estimatedHeight: number;
  children: ReactNode;
}) {
  const menuRef = useRef<HTMLDivElement>(null);
  const [style, setStyle] = useState<CSSProperties>({});

  useClickOutside([anchorRef, menuRef], onClose, open);
  useEscapeKey(() => {
    onClose();
    focusAnchor(anchorRef.current);
  }, open);

  useEffect(() => {
    if (open) focusFirstMenuItem(menuRef.current);
  }, [open]);

  useLayoutEffect(() => {
    if (!open) return;
    const rect = anchorRef.current?.getBoundingClientRect();
    if (!rect) return;
    const flipUp = rect.bottom + estimatedHeight + 8 > window.innerHeight;
    setStyle({
      position: "fixed",
      width,
      right: window.innerWidth - rect.right,
      ...(flipUp ? { bottom: window.innerHeight - rect.top + 6 } : { top: rect.bottom + 6 }),
    });
  }, [open, anchorRef, estimatedHeight, width]);

  useDismissOnViewportChange(menuRef, onClose, open);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Tab") {
      e.preventDefault();
      onClose();
      focusAnchor(anchorRef.current);
      return;
    }
    handleMenuArrowKeys(e);
  };

  const onClickCapture = (e: MouseEvent<HTMLDivElement>) => {
    if (!(e.target as HTMLElement).closest('[role="menuitem"]')) return;
    onClose();
    focusAnchor(anchorRef.current);
  };

  return (
    <DropPanel
      open={open}
      panelRef={menuRef}
      style={style}
      role="menu"
      onKeyDown={onKeyDown}
      onClickCapture={onClickCapture}
    >
      <div className="py-[6px]">{children}</div>
    </DropPanel>
  );
}

// ------------ Menu Keyboard Navigation ------------
// Arrow key, Home and End handling so pop-out menus can be used without a mouse.
const ITEM_SELECTOR = '[role="menuitem"]:not(:disabled)';

function focusFirstMenuItem(menu: HTMLElement | null): void {
  menu?.querySelector<HTMLElement>(ITEM_SELECTOR)?.focus({ preventScroll: true });
}

function handleMenuArrowKeys(e: KeyboardEvent<HTMLElement>): void {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "Home" && e.key !== "End") return;
  const entries = Array.from(e.currentTarget.querySelectorAll<HTMLElement>(ITEM_SELECTOR));
  if (entries.length === 0) return;
  e.preventDefault();
  const current = entries.indexOf(document.activeElement as HTMLElement);
  let next: number;
  if (e.key === "Home") next = 0;
  else if (e.key === "End") next = entries.length - 1;
  else if (e.key === "ArrowDown") next = current < 0 ? 0 : (current + 1) % entries.length;
  else next = current < 0 ? entries.length - 1 : (current - 1 + entries.length) % entries.length;
  entries[next].focus({ preventScroll: true });
}
