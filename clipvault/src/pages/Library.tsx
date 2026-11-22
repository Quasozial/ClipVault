import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, on, type Clip, type ClipFilter, type MarkerKind, type SortOrder } from "../api";
import { MarkerIcon, MARKER_NAMES } from "../components/MarkerIcon";
import { StarIcon } from "../components/StarIcon";
import { Thumb } from "../components/Thumb";
import { insertClip, matchesFilter } from "../lib/clips";
import { formatDuration, formatTakenAt } from "../lib/format";
import { columnsFor, visibleRows } from "../lib/grid";
import { href, startImport, useImport } from "../lib/stores";
import { reportError } from "../lib/toast";

/**
 * The library loads the full clip list in one `list_clips` call (the command has
 * no paging; 630 rows of metadata is small) and renders only the rows in view
 * (row virtualisation below). Thumbnails are fetched per card once visible.
 */

// Big cards, 3-4 per row on typical windows (fewer only when narrow).
const MIN_CARD = 300;
const MAX_COLS = 4;
const GAP = 20; // keep in sync with .grid-row gap
const INFO_H = 60; // title + meta under the 16:9 thumbnail (keep in sync with .card-info)
const BANNER_KEY = "clipvault.importBannerDismissed";

// Kept across visits so "back" from a clip lands where you were.
const remembered: { search: string; game: string; starred: boolean; sort: SortOrder; scrollTop: number } = {
  search: "",
  game: "",
  starred: false,
  sort: "newest",
  scrollTop: 0,
};

const MARKER_ORDER: MarkerKind[] = ["goal", "laugh", "manual", "saved", "imported"];

function markerSummary(clip: Clip) {
  const counts = new Map<MarkerKind, number>();
  for (const m of clip.markers) if (m.kind !== "saved") counts.set(m.kind, (counts.get(m.kind) ?? 0) + 1);
  return MARKER_ORDER.filter((k) => counts.has(k)).map((k) => ({ kind: k, n: counts.get(k)! }));
}

function ClipCard({ clip, root, onStar }: { clip: Clip; root: Element | null; onStar(c: Clip): void }) {
  const trimmed = clip.trim_in !== null || clip.trim_out !== null;
  const markers = markerSummary(clip);
  return (
    <a className="card" href={href({ page: "clip", id: clip.id })} title={clip.title}>
      <div className="card-media">
        <Thumb clip={clip} root={root} />
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
                <MarkerIcon kind={m.kind} size={13} />
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
          <span className="card-game">{clip.game || "Unknown game"}</span>
          <time dateTime={clip.recorded_at}>{formatTakenAt(clip.recorded_at)}</time>
        </div>
      </div>
    </a>
  );
}

function ImportBanner({ onDismiss }: { onDismiss(): void }) {
  const imp = useImport();
  return (
    <div className="banner" role="region" aria-label="Import from SteelSeries Moments">
      <div className="banner-text">
        <strong>Bring over your SteelSeries Moments clips?</strong>
        <span>
          ClipVault found a Moments library on this PC. Clips stay where they are; their titles, tags and markers come
          along.
        </span>
        {imp.running && (
          <div className="progress" aria-live="polite">
            <div className="progress-bar">
              <span style={{ width: imp.progress && imp.progress.total ? `${(imp.progress.done / imp.progress.total) * 100}%` : "4%" }} />
            </div>
            <small>{imp.progress ? `Importing ${imp.progress.done} of ${imp.progress.total}` : "Reading the Moments library…"}</small>
          </div>
        )}
      </div>
      <div className="banner-actions">
        <button type="button" className="btn btn-primary" disabled={imp.running} onClick={() => void startImport()}>
          {imp.running ? "Importing…" : "Import clips"}
        </button>
        <button type="button" className="btn btn-ghost" disabled={imp.running} onClick={onDismiss}>
          Not now
        </button>
      </div>
    </div>
  );
}

export function Library() {
  const [search, setSearch] = useState(remembered.search);
  const [query, setQuery] = useState(remembered.search); // debounced
  const [game, setGame] = useState(remembered.game);
  const [starredOnly, setStarredOnly] = useState(remembered.starred);
  const [sort, setSort] = useState<SortOrder>(remembered.sort);
  const [clips, setClips] = useState<Clip[] | null>(null);
  const [games, setGames] = useState<string[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showBanner, setShowBanner] = useState(false);
  const imp = useImport();

  const scrollRef = useRef<HTMLDivElement>(null);
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [scrollTop, setScrollTop] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);

  const filter: ClipFilter = useMemo(
    () => ({ game: game || undefined, search: query || undefined, starred: starredOnly || undefined, sort }),
    [game, query, starredOnly, sort],
  );
  const filterRef = useRef(filter);
  filterRef.current = filter;

  useEffect(() => {
    const t = setTimeout(() => setQuery(search.trim()), 200);
    return () => clearTimeout(t);
  }, [search]);

  useEffect(() => {
    Object.assign(remembered, { search, game, starred: starredOnly, sort });
  }, [search, game, starredOnly, sort]);

  const toggleStar = useCallback((c: Clip) => {
    const next = !c.starred;
    const apply = (starred: boolean) =>
      setClips((list) => {
        if (!list) return list;
        // Unstarring under the "Starred" filter drops the card.
        if (!starred && filterRef.current.starred) return list.filter((x) => x.id !== c.id);
        return list.map((x) => (x.id === c.id ? { ...x, starred } : x));
      });
    apply(next);
    api.setStarred(c.id, next).catch((e) => {
      reportError("Could not change the star", e);
      if (filterRef.current.starred && !next) api.listClips(filterRef.current).then(setClips, () => {});
      else apply(!next);
    });
  }, []);

  const load = useCallback(() => {
    let stale = false;
    api.listClips(filter).then(
      (v) => {
        if (stale) return;
        setClips(v);
        setLoadError(null);
      },
      (e) => {
        if (stale) return;
        setLoadError(e.message);
        reportError("Could not load clips", e);
      },
    );
    return () => {
      stale = true;
    };
  }, [filter]);

  useEffect(load, [load, imp.generation]);

  useEffect(() => {
    api.listGames().then(
      (g) => setGames(g.map((x) => x.name)),
      (e) => reportError("Could not load games", e),
    );
  }, [imp.generation]);

  // Background compression changed a clip's size.
  useEffect(
    () =>
      on("clip-updated", (clip) => {
        setClips((list) => (list ? list.map((x) => (x.id === clip.id ? { ...x, size: clip.size, compressed: clip.compressed } : x)) : list));
      }),
    [],
  );

  // Live insert of freshly saved clips.
  useEffect(
    () =>
      on("clip-saved", (clip) => {
        if (!matchesFilter(clip, filterRef.current)) return;
        setClips((list) => (list ? insertClip(list, clip, filterRef.current.sort ?? "newest") : list));
      }),
    [],
  );

  // First-run offer: empty library + Moments DB on this PC + not dismissed before.
  useEffect(() => {
    if (!clips || clips.length > 0 || filter.game || filter.search || filter.starred) return;
    let dismissed = false;
    try {
      dismissed = localStorage.getItem(BANNER_KEY) === "1";
    } catch {
      /* storage unavailable: treat as not dismissed */
    }
    if (dismissed) return;
    api.momentsDbAvailable().then(
      (ok) => setShowBanner(ok),
      (e) => reportError("Could not check for a Moments library", e),
    );
  }, [clips, filter.game, filter.search, filter.starred]);

  const dismissBanner = () => {
    try {
      localStorage.setItem(BANNER_KEY, "1");
    } catch {
      /* ignore */
    }
    setShowBanner(false);
  };

  useEffect(() => {
    if (imp.report && showBanner) dismissBanner();
  }, [imp.report, showBanner]);

  // Grid geometry.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    setScroller(el);
    const ro = new ResizeObserver(() => setSize({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el);
    setSize({ w: el.clientWidth, h: el.clientHeight });
    return () => ro.disconnect();
  }, []);

  // Restore the scroll position once the list is there.
  const restored = useRef(false);
  useLayoutEffect(() => {
    if (restored.current || !clips || !scrollRef.current) return;
    restored.current = true;
    scrollRef.current.scrollTop = remembered.scrollTop;
    setScrollTop(remembered.scrollTop);
  }, [clips]);

  const pad = 24;
  const innerW = Math.max(0, size.w - pad * 2);
  const cols = columnsFor(innerW, MIN_CARD, GAP, MAX_COLS);
  const cardW = cols > 0 ? (innerW - GAP * (cols - 1)) / cols : 0;
  const rowH = Math.round((cardW * 9) / 16 + INFO_H + GAP);
  const list = clips ?? [];
  const rowCount = Math.ceil(list.length / cols);
  const { first, last } = visibleRows({ scrollTop, viewport: size.h, rowHeight: rowH, rowCount, overscan: 2 });

  // "/" focuses search, like most tools.
  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      if (e.key === "/" && !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)) {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, []);

  const filtered = Boolean(filter.game || filter.search || filter.starred);
  const rows = [];
  for (let r = first; r <= last; r++) {
    rows.push(
      <div key={r} className="grid-row" style={{ top: pad + r * rowH, left: pad, right: pad, gridTemplateColumns: `repeat(${cols}, 1fr)` }}>
        {list.slice(r * cols, r * cols + cols).map((c) => (
          <ClipCard key={c.id} clip={c} root={scroller} onStar={toggleStar} />
        ))}
      </div>,
    );
  }

  return (
    <div className="library">
      <div className="toolbar">
        <div className="search">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
            <circle cx="7" cy="7" r="4.8" fill="none" stroke="currentColor" strokeWidth="1.6" />
            <path d="M10.6 10.6 14 14" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
          </svg>
          <input
            ref={searchRef}
            type="search"
            placeholder="Search clips"
            title="Every word must match. Searches title, description, tags, game, markers (goal, laugh…) and date (2026-11-20)."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                setSearch("");
                e.currentTarget.blur();
              }
            }}
            aria-label="Search clips"
          />
          <kbd className="search-key">/</kbd>
        </div>
        <select value={game} onChange={(e) => setGame(e.target.value)} aria-label="Filter by game">
          <option value="">All games</option>
          {games.map((g) => (
            <option key={g} value={g}>
              {g}
            </option>
          ))}
          {game && !games.includes(game) && <option value={game}>{game}</option>}
        </select>
        <button
          type="button"
          className={`btn btn-toggle${starredOnly ? " on" : ""}`}
          aria-pressed={starredOnly}
          onClick={() => setStarredOnly((v) => !v)}
          title="Show only starred clips"
        >
          <StarIcon on={starredOnly} size={14} /> Starred
        </button>
        <select value={sort} onChange={(e) => setSort(e.target.value as SortOrder)} aria-label="Sort">
          <option value="newest">Newest first</option>
          <option value="oldest">Oldest first</option>
        </select>
        <span className="count">{clips ? `${clips.length} ${clips.length === 1 ? "clip" : "clips"}` : ""}</span>
      </div>

      {showBanner && <ImportBanner onDismiss={dismissBanner} />}

      <div
        ref={scrollRef}
        className="grid-scroll"
        onScroll={(e) => {
          const t = e.currentTarget.scrollTop;
          remembered.scrollTop = t;
          setScrollTop(t);
        }}
      >
        {clips && clips.length > 0 && (
          <div className="grid-sizer" style={{ height: pad * 2 + rowCount * rowH - GAP }}>
            {rows}
          </div>
        )}
        {clips && clips.length === 0 && (
          <div className="empty">
            {filtered ? (
              <>
                <p>
                  No {filter.starred ? "starred " : ""}clips match {filter.search ? `“${filter.search}”` : "this filter"}
                  {filter.game ? ` in ${filter.game}` : ""}.
                </p>
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setSearch("");
                    setQuery("");
                    setGame("");
                    setStarredOnly(false);
                  }}
                >
                  Clear filters
                </button>
              </>
            ) : (
              <>
                <p>No clips yet.</p>
                <p className="muted">Clips you save in a game show up here. The save hotkey is set in Settings.</p>
              </>
            )}
          </div>
        )}
        {!clips && loadError && (
          <div className="empty">
            <p>Could not load the library.</p>
            <p className="muted">{loadError}</p>
            <button type="button" className="btn" onClick={load}>
              Try again
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
