import { useRef } from "react";
import { formatClock, formatDuration } from "../lib/format";
import { formatSpeed } from "../lib/player";

type Props = {
  playing: boolean;
  current: number;
  duration: number;
  /** Trim range; the whole clip when there is no trim. */
  tin: number;
  tout: number;
  hasTrim: boolean;
  rate: number;
  onTogglePlay(): void;
  onSeek(t: number): void;
  onExit(): void;
};

/** Transport shown over the video while the player is fullscreen. */
export function FullscreenBar({ playing, current, duration, tin, tout, hasTrim, rate, onTogglePlay, onSeek, onExit }: Props) {
  const strip = useRef<HTMLDivElement>(null);
  const pct = (t: number) => (duration > 0 ? `${(Math.min(duration, Math.max(0, t)) / duration) * 100}%` : "0%");

  const seekAt = (clientX: number) => {
    const el = strip.current;
    if (!el || duration <= 0) return;
    const r = el.getBoundingClientRect();
    onSeek(((clientX - r.left) / r.width) * duration);
  };

  return (
    // Clicks here must not reach the player, which toggles play and fullscreen.
    <div className="fs-bar" onClick={(e) => e.stopPropagation()} onDoubleClick={(e) => e.stopPropagation()}>
      <div
        ref={strip}
        className="fs-seek"
        onPointerDown={(e) => {
          e.currentTarget.setPointerCapture(e.pointerId);
          seekAt(e.clientX);
        }}
        onPointerMove={(e) => {
          if (e.currentTarget.hasPointerCapture(e.pointerId)) seekAt(e.clientX);
        }}
      >
        <div className="fs-seek-track">
          {hasTrim && <div className="fs-seek-trim" style={{ left: pct(tin), width: `calc(${pct(tout)} - ${pct(tin)})` }} />}
          <div className="fs-seek-done" style={{ left: pct(tin), width: `calc(${pct(current)} - ${pct(tin)})` }} />
          <div className="fs-seek-head" style={{ left: pct(current) }} />
        </div>
      </div>
      <div className="fs-row">
        <button type="button" className="btn btn-icon btn-ghost" onClick={onTogglePlay} aria-label={playing ? "Pause (Space)" : "Play (Space)"}>
          {playing ? (
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden>
              <rect x="3.5" y="2.5" width="3" height="11" rx="0.6" fill="currentColor" />
              <rect x="9.5" y="2.5" width="3" height="11" rx="0.6" fill="currentColor" />
            </svg>
          ) : (
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden>
              <path d="M4 2.5v11l9.5-5.5Z" fill="currentColor" />
            </svg>
          )}
        </button>
        <span className="clock">
          {formatClock(current)} <span className="muted">/ {formatDuration(duration)}</span>
        </span>
        {rate !== 1 && <span className="muted">{formatSpeed(rate)}</span>}
        <button
          type="button"
          className="btn btn-icon btn-ghost fs-exit"
          onClick={onExit}
          title="Exit fullscreen (F)"
          aria-label="Exit fullscreen"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden>
            <path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4" stroke="currentColor" strokeWidth="1.6" fill="none" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}
