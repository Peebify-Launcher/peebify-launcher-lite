// ------------ Image Lightbox ------------
// A full-screen image viewer for a mod's screenshots on GameBanana, with previous and next arrows. Closes on
// Escape and preloads the neighbouring images.
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { m } from "framer-motion";
import { ChevronLeft, ChevronRight, X } from "lucide-react";
import type { GameBananaImage } from "../../lib/gamebanana";
import { useEscapeKey } from "../../lib/hooks";
import { useFocusTrap } from "../../lib/useFocusTrap";
import { useT } from "../../lib/useT";

export function Lightbox({
  title,
  images,
  index,
  onClose,
  onIndexChange,
}: {
  title: string;
  images: GameBananaImage[];
  index: number;
  onClose: () => void;
  onIndexChange: (index: number) => void;
}) {
  const t = useT();
  const current = images[index];
  const root = useRef<HTMLDivElement>(null);
  const isTop = useEscapeKey(onClose);
  useFocusTrap(root, true, true);

  useEffect(() => {
    const preload = (i: number) => {
      const src = images[i]?.url;
      if (src) new Image().src = src;
    };
    preload(index + 1);
    preload(index - 1);
  }, [images, index]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!isTop()) return;
      if (e.key === "Escape") e.preventDefault();
      else if (e.key === "ArrowLeft" && index > 0) onIndexChange(index - 1);
      else if (e.key === "ArrowRight" && index < images.length - 1) onIndexChange(index + 1);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [index, images.length, isTop, onIndexChange]);

  if (!current) return null;

  return createPortal(
    <m.div
      ref={root}
      role="dialog"
      aria-modal="true"
      aria-label={t("mods.lightbox.dialogLabel", { index: index + 1, total: images.length })}
      tabIndex={-1}
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.15 }}
      className="fixed inset-0 z-(--z-modal) flex items-center justify-center outline-none"
      style={{ background: "rgba(0,0,0,0.88)" }}
      onClick={onClose}
    >
      <button
        type="button"
        onClick={onClose}
        aria-label={t("mods.lightbox.close")}
        className="absolute right-5 top-5 z-10 rounded-full bg-white/[0.08] p-2.5 text-white/70 transition hover:bg-white/[0.15] hover:text-white"
      >
        <X size={18} />
      </button>

      {index > 0 && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onIndexChange(index - 1);
          }}
          aria-label={t("mods.lightbox.previous")}
          className="absolute left-5 z-10 rounded-full bg-white/[0.08] p-3 text-white/70 transition hover:bg-white/[0.15] hover:text-white"
        >
          <ChevronLeft size={20} />
        </button>
      )}
      {index < images.length - 1 && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onIndexChange(index + 1);
          }}
          aria-label={t("mods.lightbox.next")}
          className="absolute right-5 top-1/2 z-10 -translate-y-1/2 rounded-full bg-white/[0.08] p-3 text-white/70 transition hover:bg-white/[0.15] hover:text-white"
        >
          <ChevronRight size={20} />
        </button>
      )}

      <LightboxImage
        key={current.url}
        src={current.url}
        alt={t("mods.lightbox.alt", { title, index: index + 1, total: images.length })}
      />

      {images.length > 1 && (
        <span className="absolute bottom-6 left-1/2 -translate-x-1/2 rounded-full bg-black/60 px-3 py-1.5 text-[12.5px] text-white/70">
          {t("mods.imageCounter", { index: index + 1, total: images.length })}
        </span>
      )}
    </m.div>,
    document.body,
  );
}

function LightboxImage({ src, alt }: { src: string; alt: string }) {
  const t = useT();
  const [loaded, setLoaded] = useState(false);
  const [failed, setFailed] = useState(false);

  if (failed) {
    return (
      <div
        onClick={(e) => e.stopPropagation()}
        className="flex h-[60vh] w-[70vw] items-center justify-center rounded-ui bg-white/[0.04] text-[13px] text-white/45"
      >
        {t("mods.previewUnavailable")}
      </div>
    );
  }
  return (
    <>
      {!loaded && (
        <div className="absolute h-[60vh] w-[70vw] animate-pulse rounded-ui bg-white/[0.04]" />
      )}
      <img
        src={src}
        alt={alt}
        onLoad={() => setLoaded(true)}
        onError={() => setFailed(true)}
        onClick={(e) => e.stopPropagation()}
        className={`max-h-[85vh] max-w-[88vw] rounded-ui object-contain transition-opacity duration-150 ${
          loaded ? "opacity-100" : "opacity-0"
        }`}
      />
    </>
  );
}
