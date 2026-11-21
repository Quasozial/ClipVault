import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { Marker, MarkerKind } from "../api";
import { formatClock, formatDuration } from "../lib/format";
import { MARKER_NAMES, MarkerIcon } from "./MarkerIcon";

// ----- pure helpers (tested) -----

/** x position (px) of every marker inside [0, duration]; others are dropped. */
export function markerPositions(
  duration: number,
  markers: Marker[],
  widthPx: number,
): { x: number; kind: MarkerKind }[] {
  if (!(duration > 0)) return [];
  return markers
    .filter((m) => Number.isFinite(m.t_s) && m.t_s >= 0 && m.t_s <= duration)
    .map((m) => ({ x: (m.t_s / duration) * widthPx, kind: m.kind }));
}

/** Seconds at pixel `x` of a `widthPx` wide track, clamped to the clip. */
export function timeFromX(x: number, widthPx: number, duration: number): number {
  if (!(widthPx > 0) || !(duration > 0)) return 0;
  return Math.min(duration, Math.max(0, (x / widthPx) * duration));
}

const STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600];

/** Ruler step (s): the smallest round step giving at most 8 ticks. */
export function tickStep(duration: number): number {
  if (!(duration > 0)) return 1;
  return STEPS.find((s) => duration / s <= 8) ?? STEPS[STEPS.length - 1];
}

export const MIN_TRIM_GAP = 0.5;

/** Keeps the trim inside the clip and at least MIN_TRIM_GAP long; the handle being moved yields. */
export function clampTrim(tin: number, tout: number, duration: number, moving: "in" | "out" = "in"): [number, number] {
  let a = Math.min(duration, Math.max(0, tin));
  let b = Math.min(duration, Math.max(0, tout));
  if (b - a < MIN_TRIM_GAP) {
    if (moving === "out") {
      b = Math.min(duration, a + MIN_TRIM_GAP);
      a = Math.min(a, b - MIN_TRIM_GAP);
    } else {
      a = Math.max(0, b - MIN_TRIM_GAP);
      b = Math.max(b, a + MIN_TRIM_GAP);
    }
  }
  return [a, b];
}

// ----- component -----

export type TrackKind = "game" | "mic" | "voice" | "other";

export interface TimelineTrack {
  key: string;
  label: string;
  kind: TrackKind;
  /** Peak level 0..1 per 1/peaksPerSec s. */
  peaks: number[];
  peaksPerSec: number;
  volume: number;
}

/** Max of `peaks` over each of `columns` equal slices of [0, duration]. */
export function resamplePeaks(peaks: number[], peaksPerSec: number, duration: number, columns: number): number[] {
  const out = new Array<number>(Math.max(0, columns)).fill(0);
  if (!(duration > 0) || !(peaksPerSec > 0) || columns <= 0 || !peaks.length) return out;
  const per = (duration * peaksPerSec) / columns;
  for (let c = 0; c < columns; c++) {
    const a = Math.floor(c * per);
    const b = Math.max(a + 1, Math.floor((c + 1) * per));
    let m = 0;
    for (let i = a; i < b && i < peaks.length; i++) if (peaks[i] > m) m = peaks[i];
    out[c] = m;
  }
  return out;
}

function Waveform({ track, duration }: { track: TimelineTrack; duration: number }) {
  const ref = useRef<HTMLCanvasElement>(null);
  const [size, setSize] = useState<[number, number]>([0, 0]);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setSize([el.clientWidth, el.clientHeight]));
    ro.observe(el);
    setSize([el.clientWidth, el.clientHeight]);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const c = ref.current;
    const [w, h] = size;
    if (!c || !w || !h) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
    const g = c.getContext("2d");
    if (!g) return;
    g.clearRect(0, 0, c.width, c.height);
    const bar = Math.max(1, Math.round(dpr));
    const cols = Math.floor(c.width / bar);
    const vals = resamplePeaks(track.peaks, track.peaksPerSec, duration, cols);
    const mid = c.height / 2;
    // sqrt lifts quiet passages so speech stays visible next to loud game audio.
    const amp = (v: number) => (v > 0.003 ? Math.max(0.5 * dpr, Math.sqrt(v) * (mid - 2 * dpr)) : 0);
    g.fillStyle = getComputedStyle(c).color;
    for (let i = 0; i < vals.length; i++) {
      const a = amp(vals[i]);
      if (a > 0) g.fillRect(i * bar, mid - a, bar, a * 2);
    }
  }, [size, track.peaks, track.peaksPerSec, duration]);

  return <canvas ref={ref} className="tl-wave" aria-hidden />;
}

function SpeakerIcon({ muted }: { muted: boolean }) {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
      <path d="M2.5 6h2.2L8 3.2v9.6L4.7 10H2.5Z" fill="currentColor" />
      {muted ? (
        <path d="M10.5 6l4 4M14.5 6l-4 4" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      ) : (
        <path d="M10.5 5.5a3.5 3.5 0 0 1 0 5M12.4 3.8a6 6 0 0 1 0 8.4" stroke="currentColor" strokeWidth="1.4" fill="none" strokeLinecap="round" />
      )}
    </svg>
  );
}

function TrackLabel({ track, onVolume }: { track: TimelineTrack; onVolume?(key: string, v: number): void }) {
  const last = useRef(track.volume || 1);
  if (track.volume > 0) last.current = track.volume;
  const muted = track.volume === 0;
  return (
    <div className={`tl-label tl-label-audio tk-${track.kind}`}>
      <span className="tl-label-name">{track.label}</span>
      {onVolume && (
        <span className="tl-vol">
          <button
            type="button"
            className="tl-mute"
            aria-label={muted ? `Unmute ${track.label}` : `Mute ${track.label}`}
            title={muted ? "Unmute" : "Mute"}
            onClick={() => onVolume(track.key, muted ? last.current : 0)}
          >
            <SpeakerIcon muted={muted} />
          </button>
          <input
            type="range"
            min={0}
            max={1}
            step={0.01}
            value={track.volume}
            aria-label={`${track.label} volume`}
            style={{ ["--v" as string]: `${track.volume * 100}%` }}
            onChange={(e) => onVolume(track.key, Number(e.target.value))}
            onKeyDown={(e) => e.stopPropagation()}
          />
        </span>
      )}
    </div>
  );
}

export interface TimelineProps {
  duration: number;
  markers: Marker[];
  trimIn?: number;
  trimOut?: number;
  current: number;
  /** Label of the video row. */
  clipLabel?: string;
  /** Audio rows under the video row; `undefined` while loading. */
  tracks?: TimelineTrack[];
  tracksError?: string | null;
  onVolume?(key: string, volume: number): void;
  onSeek(t: number): void;
  onTrimChange(tin: number, tout: number): void;
}

type Drag = { kind: "seek" } | { kind: "in" } | { kind: "out" };

export function Timeline({
  duration,
  markers,
  trimIn,
  trimOut,
  current,
  clipLabel = "Clip",
  tracks,
  tracksError,
  onVolume,
  onSeek,
  onTrimChange,
}: TimelineProps) {
  const trackRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  // Handle positions while dragging; committed on release.
  const [pending, setPending] = useState<[number, number] | null>(null);

  const tin = pending?.[0] ?? trimIn ?? 0;
  const tout = pending?.[1] ?? trimOut ?? duration;
  const hasTrim = trimIn !== undefined || trimOut !== undefined || pending !== null;
  const pct = (t: number) => (duration > 0 ? `${(Math.min(duration, Math.max(0, t)) / duration) * 100}%` : "0%");

  const tAt = (clientX: number) => {
    const r = trackRef.current!.getBoundingClientRect();
    return timeFromX(clientX - r.left, r.width, duration);
  };

  const onPointerDown = (kind: Drag["kind"]) => (e: React.PointerEvent) => {
    if (e.button !== 0 || !(duration > 0)) return;
    e.preventDefault();
    e.stopPropagation();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    setDrag({ kind });
    if (kind === "seek") onSeek(tAt(e.clientX));
    else setPending([tin, tout]);
  };

  // Handles sit inside the lanes: stop their events here so the lanes' own
  // handlers do not run a second time (one set_trim per drag, not two).
  const onPointerMove = (e: React.PointerEvent) => {
    e.stopPropagation();
    if (!drag) return;
    const t = tAt(e.clientX);
    if (drag.kind === "seek") onSeek(t);
    else if (drag.kind === "in") setPending(clampTrim(t, tout, duration, "in"));
    else setPending(clampTrim(tin, t, duration, "out"));
  };

  const onPointerUp = (e?: React.PointerEvent) => {
    e?.stopPropagation();
    if (drag && drag.kind !== "seek" && pending) onTrimChange(pending[0], pending[1]);
    setDrag(null);
    setPending(null);
  };

  const handleKey = (which: "in" | "out") => (e: React.KeyboardEvent) => {
    const step = e.shiftKey ? 0.1 : 0.5;
    let d = 0;
    if (e.key === "ArrowLeft") d = -step;
    else if (e.key === "ArrowRight") d = step;
    else if (e.key === "Home") d = -duration;
    else if (e.key === "End") d = duration;
    else return;
    e.preventDefault();
    e.stopPropagation(); // keep the page's ←/→ seek out of it
    const next = which === "in" ? clampTrim(tin + d, tout, duration, "in") : clampTrim(tin, tout + d, duration, "out");
    onTrimChange(next[0], next[1]);
  };

  // A drag that ends outside the window still finishes.
  useEffect(() => {
    if (!drag) return;
    const up = () => onPointerUp();
    window.addEventListener("pointercancel", up);
    return () => window.removeEventListener("pointercancel", up);
  });

  const step = tickStep(duration);
  const ticks: number[] = [];
  for (let t = 0; t <= duration + 1e-6 && duration > 0; t += step) ticks.push(t);
  const minorTicks: number[] = [];
  for (let i = 1; duration > 0 && (i * step) / 5 < duration; i++) if (i % 5) minorTicks.push((i * step) / 5);

  const positioned = markers.filter((m) => Number.isFinite(m.t_s) && m.t_s >= 0 && m.t_s <= duration);
  const xs = markerPositions(duration, positioned, 100);

  return (
    <div className={`timeline${drag ? " is-dragging" : ""}`}>
      <div className="tl-labels">
        <div className="tl-label-spacer" />
        <div className="tl-label tl-label-clip">
          <span className="tl-label-name">{clipLabel}</span>
        </div>
        {tracks?.map((t) => <TrackLabel key={t.key} track={t} onVolume={onVolume} />)}
        {!tracks && !tracksError && <div className="tl-label tl-label-audio tl-label-loading">Loading audio…</div>}
      </div>

      <div className="tl-body">
        <div className="tl-ruler" aria-hidden>
          {minorTicks.map((t) => (
            <i key={`m${t}`} className="tl-minor" style={{ left: pct(t) }} />
          ))}
          {ticks.map((t) => (
            <span key={t} style={{ left: pct(t) }}>
              {formatDuration(t)}
            </span>
          ))}
        </div>

        <div
          ref={trackRef}
          className="tl-lanes"
          onPointerDown={onPointerDown("seek")}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          role="slider"
          tabIndex={-1}
          aria-label="Playback position"
          aria-valuemin={0}
          aria-valuemax={Math.round(duration)}
          aria-valuenow={Math.round(current)}
          aria-valuetext={formatClock(current)}
        >
          <div className="tl-track">
            <div className={`tl-range${hasTrim ? " is-trimmed" : ""}`} style={{ left: pct(tin), width: `calc(${pct(tout)} - ${pct(tin)})` }} />
            <div className="tl-played" style={{ left: pct(tin), width: `calc(${pct(Math.max(current, tin))} - ${pct(tin)})` }} />
            {positioned.map((m, i) => {
              const name = m.label?.trim() || MARKER_NAMES[m.kind];
              const tip = `${name} at ${formatDuration(m.t_s)}`;
              return (
                <button
                  key={`${m.kind}-${m.t_s}-${i}`}
                  type="button"
                  className={`tl-marker mk-${m.kind}`}
                  style={{ left: `${xs[i].x}%` }}
                  onPointerDown={(e) => e.stopPropagation()}
                  onClick={() => onSeek(m.t_s)}
                  aria-label={tip}
                  data-tip={tip}
                >
                  <MarkerIcon kind={m.kind} size={15} />
                </button>
              );
            })}
          </div>

          {tracks?.map((t) => (
            <div key={t.key} className={`tl-audio tk-${t.kind}${t.volume === 0 ? " is-muted" : ""}`}>
              <Waveform track={t} duration={duration} />
            </div>
          ))}
          {!tracks && !tracksError && <div className="tl-audio tl-audio-loading" />}

          {hasTrim && <div className="tl-dim" style={{ left: 0, width: pct(tin) }} />}
          {hasTrim && <div className="tl-dim" style={{ left: pct(tout), right: 0 }} />}

          {(["in", "out"] as const).map((which) => {
            const t = which === "in" ? tin : tout;
            return (
              <div
                key={which}
                className={`tl-handle tl-handle-${which}`}
                style={{ left: pct(t) }}
                role="slider"
                tabIndex={0}
                aria-label={which === "in" ? "Trim start" : "Trim end"}
                aria-valuemin={0}
                aria-valuemax={Math.round(duration)}
                aria-valuenow={Math.round(t * 10) / 10}
                aria-valuetext={formatClock(t)}
                onPointerDown={onPointerDown(which)}
                onPointerMove={onPointerMove}
                onPointerUp={onPointerUp}
                onKeyDown={handleKey(which)}
              >
                <span className="tl-grip" />
                {drag?.kind === which && <span className="tl-handle-time">{formatClock(t)}</span>}
              </div>
            );
          })}

          <div className="tl-playhead" style={{ left: pct(current) }} />
        </div>
        {tracksError && <p className="tl-note muted">Separate audio tracks unavailable: {tracksError}</p>}
      </div>
    </div>
  );
}
