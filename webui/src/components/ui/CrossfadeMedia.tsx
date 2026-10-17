// ------------ Crossfade Media ------------
// Shows a wallpaper or news banner (image or looping video) and fades smoothly to the next one when the source
// changes, so nothing flashes or pops. Used by the wallpaper layer, the news slideshow and the wallpaper picker
// preview. During a game switch the next one spreads out as a wave from the rail icon you clicked instead, and a
// video starts playing before it is revealed, so it is already moving while the wave uncovers it.
import { useEffect, useRef, useState } from "react";
import { m } from "framer-motion";
import { log } from "../../lib/log";

const FADE_SECONDS = 0.6;
const WAVE_SECONDS = 0.75;
const WAVE_PLAY_WAIT_MS = 1200;
const WAVE_POLL_MS = 120;
const WAVE_GIVE_UP_MS = 4000;

export interface Wave {
  x: number;
  y: number;
  r: number;
  at: number;
}

interface Layer {
  id: number;
  src: string;
  video: boolean;
  ready: boolean;
  wave: Wave | null;
}

interface CrossfadeMediaProps {
  src: string;
  video: boolean;
  autoPlay?: boolean;
  playing?: boolean;
  onReady?: () => void;
  onError?: (src: string) => void;
  wave?: () => Wave | null;
}

function waveClip(w: Wave, open: boolean): string {
  return `circle(${open ? w.r : 0}px at ${w.x}px ${w.y}px)`;
}

export function CrossfadeMedia({
  src,
  video,
  autoPlay = true,
  playing = true,
  onReady,
  onError,
  wave,
}: CrossfadeMediaProps) {
  const [layers, setLayers] = useState<Layer[]>([]);
  const nextIdRef = useRef(0);
  const pruneTimersRef = useRef<Set<number>>(new Set());
  const videoRefs = useRef<Map<number, HTMLVideoElement>>(new Map());
  const videoRefCallbacks = useRef<
    Map<number, (el: HTMLVideoElement | null) => (() => void) | undefined>
  >(new Map());

  const videoRefFor = (id: number) => {
    let cb = videoRefCallbacks.current.get(id);
    if (!cb) {
      cb = (el: HTMLVideoElement | null) => {
        if (!el) {
          videoRefs.current.delete(id);
          videoRefCallbacks.current.delete(id);
          return undefined;
        }
        videoRefs.current.set(id, el);
        return () => {
          videoRefs.current.delete(id);
          videoRefCallbacks.current.delete(id);
          queueMicrotask(() => {
            if (el.isConnected) return;
            el.pause();
            el.removeAttribute("src");
            el.load();
          });
        };
      };
      videoRefCallbacks.current.set(id, cb);
    }
    return cb;
  };

  useEffect(() => {
    for (const el of videoRefs.current.values()) {
      if (playing && autoPlay) {
        void el.play().catch((e) => log.debug("[media] video resume was interrupted:", e));
      } else {
        el.pause();
      }
    }
  }, [playing, autoPlay]);

  const waveRef = useRef(wave);
  const playingRef = useRef(playing && autoPlay);
  const usedWaveRef = useRef(0);
  const readyIdsRef = useRef<Set<number>>(new Set());
  useEffect(() => {
    waveRef.current = wave;
    playingRef.current = playing && autoPlay;
  });

  const waveBusyRef = useRef<{ id: number; since: number; until: number } | null>(null);
  const [waveDone, setWaveDone] = useState(0);

  useEffect(() => {
    let next = waveRef.current?.() ?? null;
    if (next && next.at === usedWaveRef.current) next = null;
    const busy = waveBusyRef.current;
    if (busy && !next) {
      const now = Date.now();
      const stuck = !Number.isFinite(busy.until) && now - busy.since > WAVE_GIVE_UP_MS;
      if (busy.until > now && !stuck) {
        const wait = Number.isFinite(busy.until) ? busy.until - now : WAVE_POLL_MS;
        const timer = window.setTimeout(() => setWaveDone((n) => n + 1), wait);
        return () => window.clearTimeout(timer);
      }
      waveBusyRef.current = null;
    }
    if (next) usedWaveRef.current = next.at;
    setLayers((prev) => {
      const top = prev[prev.length - 1];
      if (top && top.src === src && top.video === video) return prev;
      nextIdRef.current += 1;
      const base = prev.filter((layer) => layer.ready).slice(next ? -2 : -1);
      const layerWave = base.length ? next : null;
      waveBusyRef.current = layerWave ? { id: nextIdRef.current, since: Date.now(), until: Infinity } : null;
      return [...base, { id: nextIdRef.current, src, video, ready: false, wave: layerWave }];
    });
  }, [src, video, waveDone]);

  useEffect(() => {
    const timers = pruneTimersRef.current;
    return () => {
      for (const t of timers) window.clearTimeout(t);
      timers.clear();
    };
  }, []);

  const videoLoaded = (layer: Layer, el: HTMLVideoElement) => {
    if (!layer.wave || !playingRef.current) {
      markReady(layer.id);
      return;
    }
    window.setTimeout(() => markReady(layer.id), WAVE_PLAY_WAIT_MS);
    void el.play().catch((e) => {
      log.debug("[media] video could not start before its wave:", e);
      markReady(layer.id);
    });
  };

  const markReady = (id: number) => {
    if (readyIdsRef.current.has(id)) return;
    readyIdsRef.current.add(id);
    if (waveBusyRef.current?.id === id) waveBusyRef.current.until = Date.now() + WAVE_SECONDS * 1000;
    onReady?.();
    setLayers((prev) =>
      prev.map((layer) => (layer.id === id ? { ...layer, ready: true } : layer))
    );
    const holdMs = (Math.max(FADE_SECONDS, WAVE_SECONDS) + 0.5) * 1000;
    const timer = window.setTimeout(() => {
      pruneTimersRef.current.delete(timer);
      setLayers((prev) => {
        const idx = prev.findIndex((layer) => layer.id === id);
        return idx > 0 ? prev.slice(idx) : prev;
      });
    }, holdMs);
    pruneTimersRef.current.add(timer);
  };

  const markFailed = (layer: Layer, code?: number) => {
    if (code === undefined) {
      log.warn("[media] failed to load", layer.src);
    } else {
      log.warn("[media] failed to load", layer.src, "code", code);
    }
    if (waveBusyRef.current?.id === layer.id) waveBusyRef.current = null;
    if (!onError) {
      markReady(layer.id);
      return;
    }
    onReady?.();
    setLayers((prev) => prev.filter((l) => l.id !== layer.id));
    onError(layer.src);
  };

  return (
    <>
      {layers.map((layer) => (
        <m.div
          key={layer.id}
          initial={layer.wave ? { clipPath: waveClip(layer.wave, false) } : { opacity: 0 }}
          animate={
            layer.wave ? { clipPath: waveClip(layer.wave, layer.ready) } : { opacity: layer.ready ? 1 : 0 }
          }
          transition={
            layer.wave
              ? { duration: WAVE_SECONDS, ease: [0.3, 0.6, 0.25, 1] }
              : { duration: layer.id === 1 && layers.length === 1 ? 0 : FADE_SECONDS, ease: "easeInOut" }
          }
          className="absolute inset-0"
        >
          {layer.video ? (
            <video
              src={layer.src}
              preload="auto"
              autoPlay={autoPlay && playing}
              muted
              loop
              playsInline
              ref={videoRefFor(layer.id)}
              onLoadedData={(e) => videoLoaded(layer, e.currentTarget)}
              onPlaying={() => {
                if (layer.wave) markReady(layer.id);
              }}
              onError={(e) => markFailed(layer, e.currentTarget.error?.code)}
              className="h-full w-full object-cover"
            />
          ) : (
            <img
              src={layer.src}
              alt=""
              referrerPolicy="no-referrer"
              onLoad={() => markReady(layer.id)}
              onError={() => markFailed(layer)}
              className="h-full w-full object-cover"
            />
          )}
        </m.div>
      ))}
    </>
  );
}
