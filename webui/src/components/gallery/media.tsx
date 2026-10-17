// ------------ Gallery Helpers ------------
// Small helpers for the gallery and the overlay's Captures tab: picks the image or video to show on a tile, loads
// a clip's preview only once it scrolls into view, and clears a message after a few seconds.
import { useEffect, useRef, useState } from "react";
import { Film } from "lucide-react";
import type { MediaItem } from "../../lib/ipc";
import { localFileSrc } from "../../lib/customMedia";

export function tileMedia(item: MediaItem): { src: string | null; video: boolean } {
  if (item.thumbPath) return { src: localFileSrc(item.thumbPath), video: false };
  const src = localFileSrc(item.path);
  if (src && item.kind === "clip") return { src: `${src}#t=0.5`, video: true };
  return { src, video: false };
}

export function useAutoClear<T>(value: T | null, set: (value: null) => void, ms: number): void {
  useEffect(() => {
    if (!value) return;
    const timer = window.setTimeout(() => set(null), ms);
    return () => window.clearTimeout(timer);
  }, [value, set, ms]);
}

export function LazyClip({ src }: { src: string }) {
  const box = useRef<HTMLSpanElement>(null);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const node = box.current;
    if (!node || visible) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: "200px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [visible]);

  if (visible) {
    return (
      <video src={src} muted playsInline preload="metadata" className="h-full w-full object-cover" />
    );
  }
  return (
    <span ref={box} className="grid h-full w-full place-items-center text-white/25">
      <Film size={26} />
    </span>
  );
}
