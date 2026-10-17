// ------------ Motion Presets ------------
// Shared animation timings and page, fade, list and collapse transitions, so everything moves the same way.
// Respects the reduced motion setting.
import type { ReactNode } from "react";
import { AnimatePresence, m, useReducedMotion, type Transition, type Variants } from "framer-motion";

export const EASE_OUT_SOFT = [0.22, 1, 0.36, 1] as const;

export const EASE_STANDARD = [0.4, 0, 0.2, 1] as const;

export const springThumb: Transition = { type: "spring", stiffness: 500, damping: 35 };

export const springFlow: Transition = { type: "spring", stiffness: 260, damping: 24 };

export const springPour: Transition = { type: "spring", stiffness: 240, damping: 31 };

export const edgeLead: Transition = { type: "spring", stiffness: 820, damping: 57 };
export const edgeTrail: Transition = { type: "spring", stiffness: 440, damping: 42 };

export const springJelly: Transition = { type: "spring", stiffness: 600, damping: 20 };

export const springDrip: Transition = { type: "spring", stiffness: 380, damping: 22 };

export const pageVariants: Variants = {
  initial: { opacity: 0, y: 8 },
  animate: { opacity: 1, y: 0, transition: { duration: 0.2, ease: EASE_OUT_SOFT } },
  exit: { opacity: 0, transition: { duration: 0.2 } },
};

export const pageVariantsSolid: Variants = {
  initial: { opacity: 0, y: 8 },
  animate: {
    opacity: 1,
    y: 0,
    transition: { opacity: { duration: 0.05 }, y: { duration: 0.2, ease: EASE_OUT_SOFT } },
  },
  exit: { opacity: 0, transition: { duration: 0.2 } },
};

export const fadeVariants: Variants = {
  initial: { opacity: 0 },
  animate: (duration = 0.12) => ({ opacity: 1, transition: { duration } }),
  exit: (duration = 0.12) => ({ opacity: 0, transition: { duration } }),
};

export const listItemVariants: Variants = {
  initial: { opacity: 0, y: 6 },
  animate: { opacity: 1, y: 0, transition: { duration: 0.15, ease: EASE_OUT_SOFT } },
  exit: { opacity: 0, scale: 0.98, transition: { duration: 0.12 } },
};

export function useReducedMotionSafe(): boolean {
  return Boolean(useReducedMotion());
}

export function replayAnimation(el: Element | null, className: string, ...alternatives: string[]): void {
  if (!el) return;
  el.classList.remove(className, ...alternatives);
  void (el as HTMLElement).offsetWidth;
  el.classList.add(className);
}

export function Collapse({
  open,
  duration = 0.26,
  id,
  initial = false,
  onExitComplete,
  children,
}: {
  open: boolean;
  duration?: number;
  id?: string;
  initial?: boolean;
  onExitComplete?: () => void;
  children: ReactNode;
}) {
  const reduce = useReducedMotionSafe();
  return (
    <AnimatePresence initial={initial} onExitComplete={onExitComplete}>
      {open && (
        <m.div
          key="body"
          id={id}
          initial={{ height: 0, opacity: 0 }}
          animate={{
            height: "auto",
            opacity: 1,
            transition: reduce ? { duration: 0 } : { height: springFlow, opacity: { duration, ease: EASE_STANDARD } },
          }}
          exit={{
            height: 0,
            opacity: 0,
            transition: reduce ? { duration: 0 } : { height: springPour, opacity: { duration: duration * 0.7 } },
          }}
          className="overflow-hidden"
        >
          {children}
        </m.div>
      )}
    </AnimatePresence>
  );
}
