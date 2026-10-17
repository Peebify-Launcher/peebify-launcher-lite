// ------------ Banners ------------
// The status card that drops in at the top of the window, and the offline banner built from it.
import { useEffect, useRef, useState, type ReactNode } from "react";
import { m, AnimatePresence } from "framer-motion";
import { X, Loader2, Wifi, WifiOff } from "lucide-react";
import { useUiStore } from "../../store/uiStore";
import { networkRecheck } from "../../lib/ipc";
import { useT } from "../../lib/useT";
import { springDrip } from "../../lib/motion";

// ------------ Status Card ------------
// The pill-shaped card and its parts (icon, text, pill button, close button).
const EASE: [number, number, number, number] = [0.2, 0.7, 0.2, 1];
const EASE_IN: [number, number, number, number] = [0.4, 0, 1, 1];

const STATUS_CARD_MOTION = {
  initial: { opacity: 0, y: -44, scaleX: 0.55, scaleY: 0.7 },
  animate: {
    opacity: 1,
    y: 0,
    scaleX: 1,
    scaleY: 1,
    transition: { default: springDrip, opacity: { duration: 0.16, ease: EASE } },
  },
  exit: { opacity: 0, y: -44, scaleX: 0.7, scaleY: 0.8, transition: { duration: 0.24, ease: EASE_IN } },
};

const glass = {
  background: "rgba(14,14,18,0.7)",
  backdropFilter: "blur(24px) saturate(180%)",
  WebkitBackdropFilter: "blur(24px) saturate(180%)",
  boxShadow: "0 18px 40px -16px rgba(0,0,0,0.6), inset 0 1px 0 rgba(255,255,255,0.08)",
};

function StatusCard({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <m.div
      role="status"
      {...STATUS_CARD_MOTION}
      className={`pointer-events-auto relative flex max-w-full items-center gap-[12px] overflow-hidden rounded-ui border border-white/[0.07] py-[9px] pl-[9px] pr-[10px] ${className}`}
      style={{ ...glass, transformOrigin: "50% 0%" }}
    >
      {children}
    </m.div>
  );
}

type Tone = "amber" | "green";

const TONES: Record<Tone, { background: string; color: string }> = {
  amber: { background: "rgba(252,211,77,0.14)", color: "#fcd34d" },
  green: { background: "rgba(110,231,183,0.14)", color: "#6ee7b7" },
};

function StatusIcon({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <div
      className="grid h-[32px] w-[32px] shrink-0 place-items-center rounded-[7px]"
      style={TONES[tone]}
    >
      {children}
    </div>
  );
}

function StatusText({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="flex min-w-0 flex-1 flex-col">
      <span className="text-[13px] font-semibold leading-[1.3] text-white">{title}</span>
      <span className="text-[12px] tabular-nums text-white/60">{detail}</span>
    </div>
  );
}

function StatusPill({
  children,
  onClick,
  disabled = false,
}: {
  children: ReactNode;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className="flex h-[26px] shrink-0 items-center rounded-full border border-white/15 px-[12px] text-[12px] font-semibold text-white/85 transition-colors duration-150 hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-45"
    >
      {children}
    </button>
  );
}

function StatusClose({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      className="grid h-[22px] w-[22px] shrink-0 place-items-center rounded-[6px] text-white/45 transition-colors hover:bg-white/10 hover:text-white"
    >
      <X size={12} />
    </button>
  );
}

// ------------ Offline Banner ------------
// The amber "You're offline" pill at the top of the window. Appears when the launcher loses internet, can be
// dismissed, and has a button to check the connection again.
const RETRY_SECONDS = 15;
const RECOVERED_MS = 2400;

export function OfflineBanner() {
  const isOnline = useUiStore((s) => s.isOnline);
  const t = useT();
  const [dismissed, setDismissed] = useState(false);
  const [checking, setChecking] = useState(false);
  const [recovered, setRecovered] = useState(false);
  const [next, setNext] = useState(RETRY_SECONDS);
  const wasOffline = useRef(false);

  useEffect(() => {
    if (!isOnline) {
      wasOffline.current = true;
      setDismissed(false);
      setRecovered(false);
      setNext(RETRY_SECONDS);
      return;
    }
    if (!wasOffline.current) return;
    wasOffline.current = false;
    setRecovered(true);
    const timer = window.setTimeout(() => setRecovered(false), RECOVERED_MS);
    return () => window.clearTimeout(timer);
  }, [isOnline]);

  const counting = !isOnline && !dismissed && !checking;
  useEffect(() => {
    if (!counting) return;
    const timer = window.setInterval(() => setNext((n) => (n <= 1 ? RETRY_SECONDS : n - 1)), 1000);
    return () => window.clearInterval(timer);
  }, [counting]);

  const shown = !dismissed && (!isOnline || recovered);

  const recheck = async () => {
    if (checking) return;
    setChecking(true);
    try {
      const online = await networkRecheck();
      if (online !== null) useUiStore.getState().setOnline(online);
    } finally {
      setNext(RETRY_SECONDS);
      setChecking(false);
    }
  };

  return (
    <AnimatePresence>
      {shown && (
        <StatusCard key="offline" className="max-w-[560px]">
          {isOnline ? (
            <>
              <StatusIcon tone="green">
                <Wifi size={16} />
              </StatusIcon>
              <StatusText title={t("layout.offline.backOnline")} detail={t("layout.offline.restored")} />
            </>
          ) : (
            <>
              <StatusIcon tone="amber">
                {checking ? <Loader2 size={16} className="animate-spin" /> : <WifiOff size={16} />}
              </StatusIcon>
              <StatusText
                title={t("layout.offline.title")}
                detail={
                  checking
                    ? t("layout.offline.checking")
                    : t("layout.offline.retrying", { seconds: next })
                }
              />
              <StatusPill disabled={checking} onClick={() => void recheck()}>
                {t("common.retry")}
              </StatusPill>
            </>
          )}
          <StatusClose label={t("layout.offline.dismiss")} onClick={() => setDismissed(true)} />
        </StatusCard>
      )}
    </AnimatePresence>
  );
}
