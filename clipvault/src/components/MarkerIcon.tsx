import type { MarkerKind } from "../api";

export const MARKER_NAMES: Record<MarkerKind, string> = {
  saved: "Saved",
  goal: "Goal",
  laugh: "Laugh",
  manual: "Manual",
  imported: "Imported",
};

/** Small line glyph per marker kind; colour comes from `.mk-<kind>` in CSS. */
export function MarkerIcon({ kind, size = 14 }: { kind: MarkerKind; size?: number }) {
  const common = {
    width: size,
    height: size,
    viewBox: "0 0 16 16",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.6,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
    className: `mk mk-${kind}`,
  };
  switch (kind) {
    case "goal": // goal frame with net
      return (
        <svg {...common}>
          <path d="M2 13V4h12v9" />
          <path d="M5.5 4v9M10.5 4v9M2 8.5h12" strokeWidth={1} />
        </svg>
      );
    case "laugh": // open smile
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="6" />
          <path d="M5 9.2c.8 1.5 1.8 2.1 3 2.1s2.2-.6 3-2.1Z" fill="currentColor" strokeWidth={1} />
          <path d="M5.8 6.2h.01M10.2 6.2h.01" strokeWidth={2} />
        </svg>
      );
    case "manual": // flag
      return (
        <svg {...common}>
          <path d="M4 14V2.5" />
          <path d="M4 3h8l-1.8 3L12 9H4" fill="currentColor" fillOpacity={0.35} />
        </svg>
      );
    case "imported": // arrow into tray
      return (
        <svg {...common}>
          <path d="M8 2v7.5M5 6.8 8 9.8l3-3" />
          <path d="M2.5 10.5V13h11v-2.5" />
        </svg>
      );
    case "saved": // bookmark
    default:
      return (
        <svg {...common}>
          <path d="M4 2h8v12l-4-3-4 3Z" fill="currentColor" fillOpacity={0.3} />
        </svg>
      );
  }
}
