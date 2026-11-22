import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, on, type Clip } from "../api";
import { ClipCard } from "../components/ClipCard";
import { FilterBar } from "../components/FilterBar";
import { insertClip } from "../lib/clips";
import { groupByDate } from "../lib/dateGroups";
import { buildLayout, columnsFor, headingAt, visibleRange } from "../lib/grid";
import { applyView, EMPTY_FILTER, facetCounts, isFiltered, type LibraryFilter, type LibrarySort } from "../lib/libraryView";
import { startImport, useImport } from "../lib/stores";
import { reportError } from "../lib/toast";

/**
 * The library loads the whole clip list in one `list_clips` call and searches,
 * filters, counts and sorts it client-side (`lib/libraryView`), so chips show
 * live counts and filtering is instant. Only rows in view are rendered (row
 * virtualisation over `lib/grid` layout rows). Thumbnails load per card once visible.
 */

// Big cards, 3-4 per row on typical windows (fewer only when narrow).
const MIN_CARD = 300;
const MAX_COLS = 4;
const GAP = 20; // keep in sync with .grid-row gap
const INFO_H = 60; // title + meta under the 16:9 thumbnail (keep in sync with .card-info)
const HEADING_H = 44; // keep in sync with .group-heading
const PAD = 24;
const BANNER_KEY = "clipvault.importBannerDismissed";

const SORTS: [LibrarySort, string][] = [
  ["newest", "Newest first"],
  ["oldest", "Oldest first"],
  ["longest", "Longest"],
  ["largest", "Largest"],
  ["markers", "Most markers"],
];

// Kept across visits so "back" from a clip lands where you were.
const remembered: { filter: LibraryFilter; sort: LibrarySort; scrollTop: number } = {
  filter: EMPTY_FILTER,
  sort: "newest",
  scrollTop: 0,
};

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
  const [search, setSearch] = useState(remembered.filter.search);
  const [filter, setFilter] = useState<LibraryFilter>(remembered.filter);
  const [sort, setSort] = useState<LibrarySort>(remembered.sort);
  const [all, setAll] = useState<Clip[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showBanner, setShowBanner] = useState(false);
  const imp = useImport();

  const scrollRef = useRef<HTMLDivElement>(null);
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [scrollTop, setScrollTop] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);

  // Debounced search into the filter.
  useEffect(() => {
    const t = setTimeout(() => setFilter((f) => (f.search === search.trim() ? f : { ...f, search: search.trim() })), 200);
    return () => clearTimeout(t);
  }, [search]);

  useEffect(() => {
    Object.assign(remembered, { filter, sort });
  }, [filter, sort]);

  const changeFilter = useCallback((f: LibraryFilter) => {
    setFilter(f);
    setSearch((s) => (s.trim() === f.search ? s : f.search));
  }, []);

  const toggleStar = useCallback((c: Clip) => {
    const next = !c.starred;
    const apply = (starred: boolean) => setAll((list) => (list ? list.map((x) => (x.id === c.id ? { ...x, starred } : x)) : list));
    apply(next);
    api.setStarred(c.id, next).catch((e) => {
      reportError("Could not change the star", e);
      apply(!next);
    });
  }, []);

  const load = useCallback(() => {
    let stale = false;
    api.listClips({}).then(
      (v) => {
        if (stale) return;
        setAll(v);
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
  }, []);

  useEffect(load, [load, imp.generation]);

  // Background compression changed a clip's size.
  useEffect(
    () =>
      on("clip-updated", (clip) => {
        setAll((list) =>
          list
            ? list.map((x) => (x.id === clip.id ? { ...x, size: clip.size, compressed: clip.compressed, original_size: clip.original_size } : x))
            : list,
        );
      }),
    [],
  );

  // Live insert of freshly saved clips; the view sorts and filters it in.
  useEffect(() => on("clip-saved", (clip) => setAll((list) => (list ? insertClip(list, clip, "newest") : list))), []);

  // First-run offer: empty library + Moments DB on this PC + not dismissed before.
  useEffect(() => {
    if (!all || all.length > 0) return;
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
  }, [all]);

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
    if (restored.current || !all || !scrollRef.current) return;
    restored.current = true;
    scrollRef.current.scrollTop = remembered.scrollTop;
    setScrollTop(remembered.scrollTop);
  }, [all]);

  // A new filter or sort starts at the top (a short result could otherwise sit above the view).
  const viewKey = useRef({ filter, sort });
  useLayoutEffect(() => {
    if (viewKey.current.filter === filter && viewKey.current.sort === sort) return;
    viewKey.current = { filter, sort };
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
    remembered.scrollTop = 0;
    setScrollTop(0);
  }, [filter, sort]);

  const list = useMemo(() => (all ? applyView(all, filter, sort) : []), [all, filter, sort]);
  const facets = useMemo(() => facetCounts(all ?? [], filter), [all, filter]);
  const dated = sort === "newest" || sort === "oldest";

  const innerW = Math.max(0, size.w - PAD * 2);
  const cols = columnsFor(innerW, MIN_CARD, GAP, MAX_COLS);
  const cardW = cols > 0 ? (innerW - GAP * (cols - 1)) / cols : 0;
  const cardRowH = Math.round((cardW * 9) / 16 + INFO_H + GAP);
  const layout = useMemo(() => {
    const groups = dated ? groupByDate(list, (c) => new Date(c.recorded_at), new Date()) : [{ label: "", items: list }];
    return buildLayout(groups, { cols, cardRowH, headingH: HEADING_H, headings: dated });
  }, [list, dated, cols, cardRowH]);
  const { first, last } = visibleRange(layout.rows, scrollTop - PAD, size.h, 2);
  const sticky = dated && scrollTop > PAD ? headingAt(layout.rows, scrollTop - PAD) : null;

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

  const filtered = isFiltered(filter);
  const rows = [];
  for (let r = first; r <= last; r++) {
    const row = layout.rows[r];
    if (row.kind === "heading") {
      rows.push(
        <h2 key={`h${r}`} className="group-heading" style={{ top: PAD + row.top, left: PAD, right: PAD, height: row.height }}>
          {row.label} <span>{row.count}</span>
        </h2>,
      );
    } else {
      rows.push(
        <div
          key={`r${row.firstIndex}`}
          className="grid-row"
          style={{ top: PAD + row.top, left: PAD, right: PAD, gridTemplateColumns: `repeat(${cols}, 1fr)` }}
        >
          {row.items.map((c) => (
            <ClipCard key={c.id} clip={c} root={scroller} onStar={toggleStar} />
          ))}
        </div>,
      );
    }
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
        <select value={sort} onChange={(e) => setSort(e.target.value as LibrarySort)} aria-label="Sort">
          {SORTS.map(([v, label]) => (
            <option key={v} value={v}>
              {label}
            </option>
          ))}
        </select>
        <span className="count">
          {all ? (filtered ? `${list.length} of ${all.length} clips` : `${all.length} ${all.length === 1 ? "clip" : "clips"}`) : ""}
        </span>
      </div>
      {all && all.length > 0 && <FilterBar filter={filter} facets={facets} onChange={changeFilter} />}

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
        {sticky && (
          <div className="group-sticky" aria-hidden>
            <div>{sticky}</div>
          </div>
        )}
        {list.length > 0 && (
          <div className="grid-sizer" style={{ height: PAD * 2 + layout.height - GAP }}>
            {rows}
          </div>
        )}
        {all && list.length === 0 && (
          <div className="empty">
            {filtered ? (
              <>
                <p>No clips match {filter.search ? `“${filter.search}”` : "these filters"}.</p>
                <button type="button" className="btn" onClick={() => changeFilter(EMPTY_FILTER)}>
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
        {!all && loadError && (
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
