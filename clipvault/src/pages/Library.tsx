import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, on, type Clip } from "../api";
import { BulkBar } from "../components/BulkBar";
import { BulkExportDialog } from "../components/BulkExportDialog";
import { ClipCard } from "../components/ClipCard";
import { ContextMenu, type MenuItem } from "../components/ContextMenu";
import { FilterBar } from "../components/FilterBar";
import { ConfirmDialog } from "../components/ui";
import { insertClip } from "../lib/clips";
import { groupByDate } from "../lib/dateGroups";
import { formatBytes } from "../lib/format";
import { buildLayout, columnsFor, headingAt, moveFocus, visibleRange } from "../lib/grid";
import { applyView, EMPTY_FILTER, facetCounts, isFiltered, type LibraryFilter, type LibrarySort } from "../lib/libraryView";
import { prune, runBulk, selectRange, toggle } from "../lib/selection";
import { storageSummary } from "../lib/storage";
import { navigate, setLibraryOrder, startImport, useImport } from "../lib/stores";
import { reportError, toast } from "../lib/toast";

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
  // Clips moved to another folder: their paths changed.
  useEffect(() => on("clips-moved", () => void load()), [load]);

  // Background compression changed a clip's size, or Moments metadata found
  // in its file filled in name, tags, markers and trim.
  useEffect(
    () =>
      on("clip-updated", (clip) => {
        setAll((list) =>
          list
            ? list.map((x) =>
                x.id === clip.id
                  ? {
                      ...x,
                      title: clip.title,
                      description: clip.description,
                      tags: clip.tags,
                      markers: clip.markers,
                      trim_in: clip.trim_in,
                      trim_out: clip.trim_out,
                      size: clip.size,
                      compressed: clip.compressed,
                      original_size: clip.original_size,
                    }
                  : x,
              )
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
  useEffect(() => {
    if (all) setLibraryOrder(list.map((c) => c.id));
  }, [all, list]);
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

  // ----- selection, bulk actions, context menu, keyboard -----

  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const anchor = useRef<string | null>(null);
  const [focus, setFocus] = useState<number | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; ids: string[]; clip: Clip } | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string[] | null>(null);
  const [renaming, setRenaming] = useState<{ clip: Clip; title: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [exporting, setExporting] = useState<Clip[] | null>(null);
  const visibleIds = useMemo(() => list.map((c) => c.id), [list]);
  const byId = useMemo(() => new Map((all ?? []).map((c) => [c.id, c])), [all]);
  const clipsOf = (ids: string[]) => ids.map((id) => byId.get(id)).filter((c): c is Clip => Boolean(c));

  // Hidden clips drop out of the selection; focus stays in range.
  useEffect(() => {
    setSelected((s) => {
      const p = prune(s, visibleIds);
      return p.size === s.size ? s : p;
    });
    setFocus((f) => (f === null || f < visibleIds.length ? f : visibleIds.length ? visibleIds.length - 1 : null));
  }, [visibleIds]);

  const onSelect = useCallback(
    (c: Clip, e: { shift: boolean }) => {
      // Read the anchor now: the updater runs later, after it moved to `c`.
      const from = anchor.current;
      setSelected((s) => (e.shift ? selectRange(s, visibleIds, from, c.id) : toggle(s, c.id)));
      anchor.current = c.id;
      setFocus(visibleIds.indexOf(c.id));
    },
    [visibleIds],
  );

  const merge = (changed: Clip[]) => {
    const m = new Map(changed.map((c) => [c.id, c]));
    setAll((l) => (l ? l.map((c) => m.get(c.id) ?? c) : l));
  };

  const starMany = (ids: string[], starred: boolean) =>
    api.setStarredMany(ids, starred).then(merge, (e) => reportError(starred ? "Could not star the clips" : "Could not unstar the clips", e));

  const tagMany = (ids: string[], tags: string[]) =>
    api.addTagsMany(ids, tags).then(
      (changed) => {
        merge(changed);
        toast.info(`Tagged ${changed.length} ${changed.length === 1 ? "clip" : "clips"}`);
      },
      (e) => reportError("Could not add the tags", e),
    );

  const compressMany = async (ids: string[]) => {
    const todo = clipsOf(ids).filter((c) => !c.compressed);
    if (todo.length === 0) {
      toast.info("Nothing to compress: these clips are already compressed.");
      return;
    }
    const r = await runBulk(
      todo.map((c) => c.id),
      (id) => api.compressClip(id),
    );
    if (r.failed.length) reportError(`Could not queue ${r.failed.length} of ${todo.length} clips`, r.failed[0].error);
    if (r.ok.length) toast.info(`Queued ${r.ok.length} ${r.ok.length === 1 ? "clip" : "clips"} for compression`);
  };

  const deleteMany = async (ids: string[]) => {
    setBusy(true);
    const r = await runBulk(ids, (id) => api.deleteClip(id));
    setBusy(false);
    setConfirmDelete(null);
    const gone = new Set(r.ok);
    setAll((l) => (l ? l.filter((c) => !gone.has(c.id)) : l));
    setSelected((s) => new Set([...s].filter((id) => !gone.has(id))));
    if (r.failed.length) reportError(`Could not delete ${r.failed.length} of ${ids.length} clips`, r.failed[0].error);
  };

  const rename = (c: Clip, title: string) =>
    api.updateClipMeta(c.id, title, c.description, c.tags).then(
      (u) => {
        merge([u]);
        setRenaming(null);
      },
      (e) => reportError("Could not rename the clip", e),
    );

  const menuItems = (m: { ids: string[]; clip: Clip }): MenuItem[] => {
    const many = m.ids.length > 1;
    const anyUnstarred = clipsOf(m.ids).some((c) => !c.starred);
    return [
      ...(many
        ? []
        : [
            { label: "Open", onClick: () => navigate({ page: "clip", id: m.clip.id }) },
            { label: "Reveal in folder", onClick: () => void api.revealInFolder(m.clip.path).catch((e) => reportError("Could not open the folder", e)) },
          ]),
      { label: anyUnstarred ? "Star" : "Unstar", onClick: () => void starMany(m.ids, anyUnstarred) },
      ...(many ? [] : [{ label: "Rename…", onClick: () => setRenaming({ clip: m.clip, title: m.clip.title }) }]),
      { label: "Compress", onClick: () => void compressMany(m.ids) },
      { label: many ? `Delete ${m.ids.length} clips…` : "Delete…", danger: true, onClick: () => setConfirmDelete(m.ids) },
    ];
  };

  const onContext = useCallback(
    (c: Clip, x: number, y: number) => {
      // On a selected card the menu acts on the whole selection.
      const ids = selected.has(c.id) && selected.size > 1 ? [...selected] : [c.id];
      setFocus(visibleIds.indexOf(c.id));
      setMenu({ x, y, ids, clip: c });
    },
    [selected, visibleIds],
  );

  // Keeps the focused card in view and gives it DOM focus.
  const focusCard = (i: number) => {
    setFocus(i);
    const row = layout.rows.find((r) => r.kind === "cards" && i >= r.firstIndex && i < r.firstIndex + r.items.length);
    const el = scrollRef.current;
    if (row && el) {
      const top = PAD + row.top;
      if (top < el.scrollTop + 40) el.scrollTop = Math.max(0, top - 48);
      else if (top + row.height > el.scrollTop + el.clientHeight) el.scrollTop = top + row.height - el.clientHeight;
    }
    requestAnimationFrame(() => scrollRef.current?.querySelector<HTMLElement>(`[data-id="${CSS.escape(visibleIds[i] ?? "")}"]`)?.focus());
  };

  // Keys: "/" search, arrows move, Enter open, Space select, S star, Delete, Ctrl+A, Esc.
  const keyState = useRef({ focus, visibleIds, selected, layout, menu, dialog: false });
  keyState.current = { focus, visibleIds, selected, layout, menu, dialog: Boolean(confirmDelete || renaming || exporting) };
  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      const st = keyState.current;
      const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement;
      if (typing || st.menu || st.dialog || e.altKey) return;
      if (e.key === "/") {
        e.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a") {
        e.preventDefault();
        setSelected(new Set(st.visibleIds));
        return;
      }
      if (e.key === "Escape" && st.selected.size) {
        setSelected(new Set());
        return;
      }
      if (e.ctrlKey || e.metaKey || st.visibleIds.length === 0) return;
      const f = st.focus;
      if (e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "ArrowUp" || e.key === "ArrowDown") {
        e.preventDefault();
        actions.current.focusCard(f === null ? 0 : moveFocus(st.layout.rows, f, e.key));
        return;
      }
      if (f === null) return;
      const id = st.visibleIds[f];
      const targets = st.selected.size ? [...st.selected] : [id];
      if (e.key === "Enter") {
        e.preventDefault();
        navigate({ page: "clip", id });
      } else if (e.key === " ") {
        e.preventDefault();
        setSelected((s) => toggle(s, id));
        anchor.current = id;
      } else if (e.key.toLowerCase() === "s") {
        const anyUnstarred = targets.some((t) => !byIdRef.current.get(t)?.starred);
        void actions.current.starMany(targets, anyUnstarred);
      } else if (e.key === "Delete") {
        setConfirmDelete(targets);
      }
    };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
    // Handlers read live state through keyState/byIdRef/actions.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const byIdRef = useRef(byId);
  byIdRef.current = byId;
  const actions = useRef({ focusCard, starMany });
  actions.current = { focusCard, starMany };

  const storage = useMemo(() => storageSummary(all ?? []), [all]);
  const selectedSize = useMemo(() => clipsOf([...selected]).reduce((n, c) => n + c.size, 0), [selected, byId]);
  const deleteSize = confirmDelete ? clipsOf(confirmDelete).reduce((n, c) => n + c.size, 0) : 0;

  // Select (or, when all are selected already, deselect) every clip under one date heading.
  const groupIds = (g: { firstIndex: number; count: number }) => visibleIds.slice(g.firstIndex, g.firstIndex + g.count);
  const toggleGroup = (g: { firstIndex: number; count: number }) => {
    const ids = groupIds(g);
    setSelected((s) => {
      const next = new Set(s);
      if (ids.every((id) => s.has(id))) ids.forEach((id) => next.delete(id));
      else ids.forEach((id) => next.add(id));
      return next;
    });
    anchor.current = ids[ids.length - 1] ?? anchor.current;
  };
  const groupSelectButton = (g: { label: string; firstIndex: number; count: number }) => {
    const full = groupIds(g).every((id) => selected.has(id));
    return (
      <button
        type="button"
        className={`group-select${full ? " on" : ""}`}
        onClick={() => toggleGroup(g)}
        title={full ? `Deselect ${g.label}` : `Select all of ${g.label}`}
      >
        {full ? "Deselect" : "Select all"}
      </button>
    );
  };
  const stickyRow = sticky
    ? [...layout.rows].reverse().find((r): r is Extract<(typeof layout.rows)[number], { kind: "heading" }> => r.kind === "heading" && r.top <= scrollTop - PAD)
    : undefined;

  const filtered = isFiltered(filter);
  const rows = [];
  for (let r = first; r <= last; r++) {
    const row = layout.rows[r];
    if (row.kind === "heading") {
      rows.push(
        <h2 key={`h${r}`} className="group-heading" style={{ top: PAD + row.top, left: PAD, right: PAD, height: row.height }}>
          {row.label} <span>{row.count}</span>
          {groupSelectButton(row)}
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
            <ClipCard
              key={c.id}
              clip={c}
              root={scroller}
              onStar={toggleStar}
              selected={selected.has(c.id)}
              selecting={selected.size > 0}
              focused={focus !== null && visibleIds[focus] === c.id}
              onSelect={onSelect}
              onContext={onContext}
            />
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
          {all && all.length > 0 && (
            <span
              className="count-size"
              title={storage.saved > 0 ? `${storage.lowerBound ? "At least " : ""}${formatBytes(storage.saved)} saved by compression` : "Total size of the library"}
            >
              {" · "}
              {formatBytes(storage.total)}
            </span>
          )}
        </span>
      </div>
      {selected.size > 0 ? (
        <BulkBar
          count={selected.size}
          sizeText={formatBytes(selectedSize)}
          busy={busy}
          onStar={(s) => void starMany([...selected], s)}
          onAddTags={(t) => void tagMany([...selected], t)}
          onCompress={() => void compressMany([...selected])}
          onExport={() => setExporting(clipsOf([...selected]))}
          onDelete={() => setConfirmDelete([...selected])}
          onCancel={() => setSelected(new Set())}
        />
      ) : (
        all && all.length > 0 && <FilterBar filter={filter} facets={facets} onChange={changeFilter} />
      )}

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
          <div className="group-sticky">
            <div>
              <span className="group-sticky-label" aria-hidden>
                {sticky}
              </span>
              {stickyRow && groupSelectButton(stickyRow)}
            </div>
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

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          title={menu.ids.length > 1 ? `${menu.ids.length} clips` : undefined}
          items={menuItems(menu)}
          onClose={() => setMenu(null)}
        />
      )}
      {exporting && <BulkExportDialog clips={exporting} onClose={() => setExporting(null)} />}
      <ConfirmDialog
        open={confirmDelete !== null}
        title={confirmDelete && confirmDelete.length > 1 ? `Delete ${confirmDelete.length} clips?` : "Delete this clip?"}
        confirmLabel={busy ? "Deleting…" : "Move to Recycle Bin"}
        danger
        busy={busy}
        onConfirm={() => confirmDelete && void deleteMany(confirmDelete)}
        onCancel={() => setConfirmDelete(null)}
      >
        <p>
          {confirmDelete && confirmDelete.length > 1 ? `${confirmDelete.length} clips` : "The clip"} ({formatBytes(deleteSize)}) will be moved to the Recycle
          Bin. You can restore {confirmDelete && confirmDelete.length > 1 ? "them" : "it"} from there.
        </p>
      </ConfirmDialog>
      <ConfirmDialog
        open={renaming !== null}
        title="Rename clip"
        confirmLabel="Rename"
        onConfirm={() => renaming && renaming.title.trim() && void rename(renaming.clip, renaming.title)}
        onCancel={() => setRenaming(null)}
      >
        <input
          className="rename-input"
          aria-label="Title"
          value={renaming?.title ?? ""}
          onChange={(e) => setRenaming((r) => (r ? { ...r, title: e.target.value } : r))}
          onKeyDown={(e) => {
            if (e.key === "Enter" && renaming?.title.trim()) void rename(renaming.clip, renaming.title);
          }}
        />
      </ConfirmDialog>
    </div>
  );
}
