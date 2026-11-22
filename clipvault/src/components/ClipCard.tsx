import { useState } from "react";
import { type Clip, type MarkerKind } from "../api";
import { formatBytes, formatDuration, formatTakenAt } from "../lib/format";
import { href } from "../lib/stores";
import { MarkerIcon, MARKER_NAMES } from "./MarkerIcon";
import { frameAt, STRIP_FRAMES, useScrubStrip } from "./Scrub";
import { StarIcon } from "./StarIcon";
import { Thumb } from "./Thumb";

const MARKER_ORDER: MarkerKind[] = ["goal", "laugh", "manual", "imported"];

function markerSummary(clip: Clip) {
  const counts = new Map<MarkerKind, number>();
  for (const m of clip.markers) if (m.kind !== "saved") counts.set(m.kind, (counts.get(m.kind) ?? 0) + 1);
  return MARKER_ORDER.filter((k) => counts.has(k)).map((k) => ({ kind: k, n: counts.get(k)! }));
}

/** Compression really replaced the file (the `compressed` flag alone also means "tried"). */
export function wasShrunk(c: Clip): boolean {
  return c.original_size !== null && c.original_size > c.size;
}

export function ClipCard({ clip, root, onStar }: { clip: Clip; root: Element | null; onStar(c: Clip): void }) {
  const trimmed = clip.trim_in !== null || clip.trim_out !== null;
  const markers = markerSummary(clip);
  const [hover, setHover] = useState(false);
  const [frame, setFrame] = useState<number | null>(null);
  const strip = useScrubStrip(clip, hover);
  const scrubbing = strip !== null && frame !== null;

  return (
    <a className="card" href={href({ page: "clip", id: clip.id })} title={clip.title}>
      <div
        className="card-media"
        onPointerEnter={() => setHover(true)}
        onPointerLeave={() => {
          setHover(false);
          setFrame(null);
        }}
        onPointerMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          setFrame(frameAt(e.clientX - r.left, r.width));
        }}
      >
        <Thumb clip={clip} root={root} />
        {scrubbing && (
          <>
            <div
              className="card-scrub"
              aria-hidden
              style={{
                backgroundImage: `url("${strip}")`,
                backgroundPosition: `${(frame / (STRIP_FRAMES - 1)) * 100}% 0`,
              }}
            />
            <div className="card-scrub-bar" aria-hidden>
              <span style={{ width: `${((frame + 1) / STRIP_FRAMES) * 100}%` }} />
            </div>
          </>
        )}
        <button
          type="button"
          className={`card-star${clip.starred ? " on" : ""}`}
          aria-label={clip.starred ? "Unstar" : "Star"}
          aria-pressed={clip.starred}
          title={clip.starred ? "Unstar" : "Star"}
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            onStar(clip);
          }}
        >
          <StarIcon on={clip.starred} />
        </button>
        {markers.length > 0 && (
          <div className="card-markers" aria-label={markers.map((m) => `${m.n} ${MARKER_NAMES[m.kind]}`).join(", ")}>
            {markers.map((m) => (
              <span key={m.kind} className={`card-mk mk-${m.kind}`}>
                <MarkerIcon kind={m.kind} size={14} />
                {m.n > 1 && <b>{m.n}</b>}
              </span>
            ))}
          </div>
        )}
        <span className="card-dur" title={trimmed ? "Trimmed" : undefined}>
          {trimmed && (
            <svg width="11" height="11" viewBox="0 0 16 16" aria-label="Trimmed" fill="none" stroke="currentColor" strokeWidth="1.5">
              <circle cx="4" cy="12" r="2.2" />
              <circle cx="12" cy="12" r="2.2" />
              <path d="M5.6 10.4 12 2M10.4 10.4 4 2" strokeLinecap="round" />
            </svg>
          )}
          {formatDuration(clip.duration_s)}
        </span>
      </div>
      <div className="card-info">
        <div className="card-title">{clip.title}</div>
        <div className="card-meta">
          <span className="card-game">
            {clip.game || "Unknown game"}
            <span className="card-size"> · {formatBytes(clip.size)}</span>
            {wasShrunk(clip) && (
              <span className="card-badge" title={`Compressed from ${formatBytes(clip.original_size!)}`}>
                Compressed
              </span>
            )}
          </span>
          <time dateTime={clip.recorded_at}>{formatTakenAt(clip.recorded_at)}</time>
        </div>
      </div>
    </a>
  );
}
