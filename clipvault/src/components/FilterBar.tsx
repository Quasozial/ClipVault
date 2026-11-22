import type { ReactNode } from "react";
import { isFiltered, type Facets, type LibraryFilter, type MarkerFacet } from "../lib/libraryView";
import { MarkerIcon, MARKER_NAMES } from "./MarkerIcon";
import { StarIcon } from "./StarIcon";

/** Game chips shown before the rest collapse into "+N more". */
const MAX_GAME_CHIPS = 6;

function Chip(props: { on: boolean; count: number; onClick(): void; children: ReactNode; className?: string; title?: string }) {
  return (
    <button
      type="button"
      className={`chip${props.on ? " on" : ""}${props.className ? ` ${props.className}` : ""}`}
      aria-pressed={props.on}
      disabled={!props.on && props.count === 0}
      onClick={props.onClick}
      title={props.title}
    >
      {props.children}
      <span className="chip-n">{props.count}</span>
    </button>
  );
}

const toggle = <T,>(list: T[], v: T) => (list.includes(v) ? list.filter((x) => x !== v) : [...list, v]);

/** One-click filters with live counts: games (one), marker kinds and tags (several), starred. */
export function FilterBar({ filter, facets, onChange }: { filter: LibraryFilter; facets: Facets; onChange(f: LibraryFilter): void }) {
  const set = (p: Partial<LibraryFilter>) => onChange({ ...filter, ...p });
  // Most clips first; the selected game always keeps its chip.
  const byCount = facets.games.slice().sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  let chips = byCount.slice(0, MAX_GAME_CHIPS);
  if (filter.game && !chips.some(([g]) => g === filter.game)) {
    const sel = byCount.find(([g]) => g === filter.game) ?? [filter.game, 0];
    chips = [...chips.slice(0, MAX_GAME_CHIPS - 1), sel];
  }
  const more = byCount.filter(([g]) => !chips.some(([c]) => c === g));

  return (
    <div className="filterbar" role="toolbar" aria-label="Filters">
      <div className="chip-group" role="group" aria-label="Game">
        {chips.map(([g, n]) => (
          <Chip key={g} on={filter.game === g} count={n} onClick={() => set({ game: filter.game === g ? null : g })}>
            {g || "Unknown game"}
          </Chip>
        ))}
        {more.length > 0 && (
          <select
            className="chip-more"
            value=""
            onChange={(e) => e.target.value && set({ game: e.target.value })}
            aria-label="More games"
          >
            <option value="">+{more.length} more</option>
            {more.map(([g, n]) => (
              <option key={g} value={g} disabled={n === 0}>
                {g} ({n})
              </option>
            ))}
          </select>
        )}
      </div>
      <span className="chip-sep" aria-hidden />
      <div className="chip-group" role="group" aria-label="Markers">
        {facets.markers.map(([k, n]) => (
          <Chip key={k} className={`mk-${k}`} on={filter.markers.includes(k)} count={n} onClick={() => set({ markers: toggle<MarkerFacet>(filter.markers, k) })}>
            <MarkerIcon kind={k} size={13} />
            {MARKER_NAMES[k]}
          </Chip>
        ))}
        <Chip on={filter.starred} count={facets.starred} onClick={() => set({ starred: !filter.starred })}>
          <StarIcon on={filter.starred} size={13} />
          Starred
        </Chip>
      </div>
      {facets.tags.length > 0 && (
        <>
          <span className="chip-sep" aria-hidden />
          <div className="chip-group" role="group" aria-label="Tags">
            {facets.tags.map(([t, n]) => (
              <Chip key={t} on={filter.tags.includes(t)} count={n} onClick={() => set({ tags: toggle(filter.tags, t) })} title={`Tag: ${t}`}>
                #{t}
              </Chip>
            ))}
          </div>
        </>
      )}
      {isFiltered(filter) && (
        <button type="button" className="chip-clear" onClick={() => onChange({ search: "", game: null, markers: [], tags: [], starred: false })}>
          Clear
        </button>
      )}
    </div>
  );
}
