# Library: Browse + Manage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the ClipVault library: date groups, chips with counts, more sorts, hover scrub, richer cards (Phase 1); multi-select, bulk actions, context menu, keyboard nav, storage summary (Phase 2).

**Architecture:** The library loads every clip once and does search/filter/sort/grouping in pure TS functions under `src/lib/`. The virtualised grid moves from fixed row height to a precomputed row layout (headings + card rows) with binary-search windowing. Backend gains scrub-strip generation, an `original_size` column, and two bulk commands.

**Tech Stack:** Tauri 2, React 19, Vite, vitest; Rust (rusqlite, FFmpeg via `ffmpeg_proc`).

**Spec:** `docs/superpowers/specs/2026-11-22-library-browse-manage-design.md`

## Global Constraints

- No new UI libraries; plain CSS in `clipvault/src/styles.css` using existing tokens; keep dark, quiet, orange-accent look.
- Must stay smooth at ~2000 clips (grid stays virtualised).
- Every new pure TS function has vitest tests next to it (`*.test.ts`); Rust changes get unit tests in-module.
- Everything must work under `?mock` (`src/mock/backend.ts`).
- Deletes only via existing `delete_clip` (Recycle Bin). Bulk delete confirm names count and total size.
- Hover preview: one FFmpeg run per clip, only after ~250 ms rest on a card; never for every card brushed.
- Clip file on disk is never renamed; "Rename" = title.
- Commands run from `clipvault/`: TS tests `npx vitest run <file>`; Rust tests `cargo test --manifest-path src-tauri/Cargo.toml <name>` (set `$env:CARGO_TARGET_DIR='C:/cvt'` first as in docs/VERIFY.md).

## Review Focus

- Clip recorded "in the future" (clock skew) or at exactly midnight / Monday 00:00 — must land in Today / correct week, not crash or create an empty group. Test in Task 2.
- Live `clip-saved` insert while filters/sort/groups are active — the new clip appears in the right group and position only if it matches. Test in Task 1 (`applyView` on list + new clip).
- Clip with `duration_s` 0 or a trim range shorter than 1 s — scrub strip args must not divide by zero. Test in Task 4.
- Shift-click range after the filter changed so the anchor is no longer visible — falls back to single toggle. Test in Task 8.
- Bulk delete where some clips fail (file locked) — the rest are removed, failures reported once, failed cards stay. Covered in Task 9 step "bulk runner" test.

---

## Phase 1 — Browse

### Task 1: Library view functions (filter, facets, sort)

**Files:**
- Create: `clipvault/src/lib/libraryView.ts`, `clipvault/src/lib/libraryView.test.ts`
- Modify: `clipvault/src/lib/clips.ts` (extract search word match so both use it)

**Interfaces:**
- Produces:
  ```ts
  export type LibrarySort = "newest" | "oldest" | "longest" | "largest" | "markers";
  export type MarkerFacet = "goal" | "laugh" | "manual" | "imported";
  export interface LibraryFilter { search: string; game: string | null; markers: MarkerFacet[]; tags: string[]; starred: boolean }
  export const EMPTY_FILTER: LibraryFilter;
  export function isFiltered(f: LibraryFilter): boolean;
  export function matchesLibraryFilter(c: Clip, f: LibraryFilter, ignore?: "game" | "markers" | "tags" | "starred"): boolean;
  export function sortClips(clips: Clip[], sort: LibrarySort): Clip[];          // new array; ties → newest
  export function applyView(all: Clip[], f: LibraryFilter, sort: LibrarySort): Clip[];
  export interface Facets { games: [string, number][]; markers: [MarkerFacet, number][]; tags: [string, number][]; starred: number }
  export function facetCounts(all: Clip[], f: LibraryFilter, topTagCount?: number /* 8 */): Facets;
  export function markerCount(c: Clip): number;                                 // excludes "saved"
  ```
  `clips.ts`: `export function matchesSearch(clip: Clip, search: string): boolean` (existing word rules); `matchesFilter` keeps working via it.

- [ ] **Step 1: Write failing tests** in `libraryView.test.ts` with a small fixture builder `clip(partial)`:
  - `matchesLibraryFilter`: game exact; markers ANY-of (`["goal","laugh"]` matches a clip with only laugh); tags ALL-of; starred; search delegates to `matchesSearch` (marker kind "goal" matches).
  - `ignore: "game"` skips only the game test.
  - `sortClips`: longest by `duration_s`, largest by `size`, markers by `markerCount` (a clip with 3 `saved` markers counts 0); ties ordered newest first.
  - `facetCounts`: game counts ignore the game selection but honour search; marker list includes `imported` only when some clip has one; tags are the top 8 by count, ties alphabetical; `starred` count honours other filters.
  - `applyView(list.concat(newClip), f, "newest")` puts a matching new clip first and omits a non-matching one.
- [ ] **Step 2: Run** `npx vitest run src/lib/libraryView.test.ts` — Expected: FAIL (module missing).
- [ ] **Step 3: Implement** the interfaces above; move word matching from `matchesFilter` into `matchesSearch`.
- [ ] **Step 4: Run** `npx vitest run src/lib` — Expected: all PASS (old `clips.test.ts` included).
- [ ] **Step 5: Commit** `feat(library): pure filter, facet and sort functions`

### Task 2: Date groups

**Files:**
- Create: `clipvault/src/lib/dateGroups.ts`, `clipvault/src/lib/dateGroups.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export function groupLabel(at: Date, now: Date): string;
  export interface Group<T> { label: string; items: T[] }
  export function groupByDate<T>(items: T[], dateOf: (t: T) => Date, now: Date): Group<T>[]; // keeps input order, merges consecutive equal labels
  ```
- Rules (local time, weeks start Monday): `at` on/after today 00:00 (incl. future) → "Today"; yesterday → "Yesterday"; else same week → "This week"; previous week → "Last week"; same year → "November 2026" (`toLocaleString("en", {month:"long"})` + year); earlier years → "2025".

- [ ] **Step 1: Write failing tests** with fixed `now = new Date(2026, 10, 23, 10)` (Monday) and `new Date(2026, 10, 25, 10)` (Wednesday): future → Today; Monday-now: Sunday → "Yesterday" (not "Last week"); Wednesday-now: Monday → "This week", previous Sunday → "Last week"; two weeks ago → "November 2026"; Oct → "October 2026"; 2025-12-31 → "2025". `groupByDate` with oldest-first input yields groups in that order, no empty groups.
- [ ] **Step 2: Run** `npx vitest run src/lib/dateGroups.test.ts` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(library): date group labels`

### Task 3: Grid layout with headings

**Files:**
- Modify: `clipvault/src/lib/grid.ts`, `clipvault/src/lib/grid.test.ts`

**Interfaces:**
- Consumes: `Group<T>` from Task 2.
- Produces (keep `columnsFor`; `visibleRows` may be removed once unused):
  ```ts
  export type LayoutRow<T> =
    | { kind: "heading"; label: string; count: number; top: number; height: number }
    | { kind: "cards"; items: T[]; firstIndex: number; top: number; height: number };
  export interface Layout<T> { rows: LayoutRow<T>[]; height: number }
  export function buildLayout<T>(groups: Group<T>[], o: { cols: number; cardRowH: number; headingH: number; headings: boolean }): Layout<T>;
  export function visibleRange(rows: { top: number; height: number }[], scrollTop: number, viewport: number, overscan: number): { first: number; last: number };
  export function headingAt<T>(rows: LayoutRow<T>[], scrollTop: number): string | null; // label of the group the top edge is in
  export function moveFocus<T>(rows: LayoutRow<T>[], index: number, key: "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown"): number;
  ```
  `firstIndex` = index of the row's first item in the flat visible order. `cardRowH` includes the gap. `moveFocus`: left/right ±1 clamped; up/down go to the same column in the previous/next cards row, skipping headings, clamped to that row's length; returns `index` unchanged at the edges.

- [ ] **Step 1: Write failing tests:** 2 groups (5 and 2 items), cols 4 → rows heading, cards(4), cards(1), heading, cards(2) with cumulative `top`; `headings:false` → no heading rows; `visibleRange` matches the old fixed-height results when all rows are equal height, and finds the right rows with mixed heights; `headingAt` returns the second label once scrolled past its heading top; `moveFocus` down from index 3 (col 3) into a 1-item row → 4, down from 4 into next group → 5, up from 0 → 0.
- [ ] **Step 2: Run** `npx vitest run src/lib/grid.test.ts` — FAIL.
- [ ] **Step 3: Implement**; `visibleRange` binary-searches the first row whose bottom > scrollTop.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(library): row layout with group headings`

### Task 4: Scrub strips (backend)

**Files:**
- Modify: `clipvault/src-tauri/src/thumbs.rs`, `clipvault/src-tauri/src/library.rs`, `clipvault/src-tauri/src/commands.rs`, `clipvault/src-tauri/src/lib.rs` (register), `clipvault/src/api.ts`, `clipvault/src/mock/backend.ts`

**Interfaces:**
- Produces:
  ```rust
  pub const STRIP_FRAMES: u32 = 8;
  pub fn strip_window(duration_s: f64, trim_in: Option<f64>, trim_out: Option<f64>) -> (f64, f64); // (start, span), span >= 0.1
  pub fn strip_args(video: &Path, out: &Path, start_s: f64, span_s: f64) -> Vec<OsString>;        // thumbs.rs
  pub fn generate_strip(ffmpeg: &Path, video: &Path, out: &Path, start_s: f64, span_s: f64) -> Result<()>;
  pub fn strip_path(thumbs_dir: &Path, id: &str) -> PathBuf;                                       // <thumbs>/<id>.strip.jpg
  pub fn ensure_scrub_strip(db: &Mutex<Connection>, ffmpeg: &Path, thumbs_dir: &Path, id: &str) -> Result<Option<String>>; // library.rs
  pub fn forget_strip(thumbs_dir: &Path, id: &str);                                               // library.rs, ignores errors
  #[tauri::command] pub async fn ensure_scrub_strip(core, ffmpeg, id: String) -> Result<Option<String>, String>;
  ```
  TS: `api.ensureScrubStrip(id: string) => call<string | null>("ensure_scrub_strip", { id })`.
- Filter: `-ss {start} -t {span} -i {video} -vf fps={8/span},scale=320:-2,tile=8x1 -frames:v 1 -q:v 4 -c:v mjpeg -f image2 {out}`; runs under `THUMB_LOCK` via `library::THUMBS.run` keyed `"strip:{id}"`, `THUMB_TIMEOUT`.

- [ ] **Step 1: Write failing tests** in `thumbs.rs`: `strip_window(60.0, None, None) == (0.0, 60.0)`; `strip_window(60.0, Some(10.0), Some(20.0)) == (10.0, 10.0)`; `strip_window(0.0, None, None).1 >= 0.1`; `strip_window(60.0, Some(5.0), Some(5.0)).1 >= 0.1`; `strip_args` contains `"tile=8x1"`, `"-frames:v" "1"`, and `fps=` with no `inf`/`NaN`. `generate_strip` errs when FFmpeg is missing (like `thumbnail_errors_when_ffmpeg_missing`).
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml strip` — FAIL.
- [ ] **Step 3: Implement** the functions; command mirrors `ensure_thumbnail`; call `forget_strip` in the `set_trim` and `delete_clip` commands (after the DB change succeeds); register in `lib.rs`.
- [ ] **Step 4: Run** `cargo test --manifest-path src-tauri/Cargo.toml` — all PASS.
- [ ] **Step 5: Mock:** `ensureScrubStrip` returns a data-URL SVG strip with 8 differently tinted panels (so hovering visibly changes frames); `fileSrc` must pass data URLs through.
- [ ] **Step 6: Commit** `feat(library): scrub strip generation`

### Task 5: `original_size` (backend + type)

**Files:**
- Modify: `clipvault/src-tauri/src/db/mod.rs`, `clipvault/src-tauri/src/db/clips.rs`, `clipvault/src-tauri/src/compress.rs`, `clipvault/src/api.ts`, `clipvault/src/mock/backend.ts`

**Interfaces:**
- Produces: schema v6 `ALTER TABLE clips ADD COLUMN original_size INTEGER;` (`LATEST_VERSION = 6`); `Clip.original_size: Option<u64>` (`#[serde(default)]`); `pub fn set_compressed(conn, id: &str, size: u64, original_size: Option<u64>) -> Result<()>`. TS `Clip.original_size: number | null`.
- `compress_clip` passes `Some(original)` only on the branch where `rename` succeeded (`break (conn, new)`); every other call passes `None`.

- [ ] **Step 1: Write failing tests** in `db/clips.rs`: insert clip, `set_compressed(id, 400, Some(1000))` → `get` returns `size 400, original_size Some(1000), compressed true`; `set_compressed(id, 1000, None)` → `original_size None`. In `db/mod.rs`: migrating a v5 DB reaches v6 and old rows read `original_size None` (follow the existing migration tests).
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml db::` — FAIL.
- [ ] **Step 3: Implement** (SELECT gains `c.original_size` at the end; `row_to_clip` reads it).
- [ ] **Step 4: Run** full `cargo test` — PASS.
- [ ] **Step 5: Mock:** a few compressed fixtures with `original_size`, one compressed with `null`, one clip with `imported` markers.
- [ ] **Step 6: Commit** `feat(db): record original size of compressed clips`

### Task 6: Library UI — Phase 1

**Files:**
- Create: `clipvault/src/components/ClipCard.tsx`, `clipvault/src/components/FilterBar.tsx`, `clipvault/src/components/Scrub.tsx`
- Modify: `clipvault/src/pages/Library.tsx`, `clipvault/src/styles.css`

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces:
  ```ts
  // FilterBar.tsx
  export function FilterBar(p: { filter: LibraryFilter; facets: Facets; onChange(f: LibraryFilter): void }): JSX.Element;
  // ClipCard.tsx (moved out of Library.tsx; Phase 2 extends props)
  export function ClipCard(p: { clip: Clip; root: Element | null; onStar(c: Clip): void }): JSX.Element;
  // Scrub.tsx
  export function useScrubStrip(clip: Clip, hovering: boolean): string | null; // requests after 250 ms hover; session cache keyed `${id}:${trim_in}:${trim_out}`
  ```
- Behaviour (from spec):
  - Load: `api.listClips({})` once (and on `imp.generation`); `clip-saved` appends, `clip-updated` replaces size/compressed/original_size; visible list = `applyView`.
  - Toolbar row 1: search (unchanged, `/`, Esc), sort `<select>` (Newest, Oldest, Longest, Largest, Most markers), count "N of M clips".
  - Row 2 `FilterBar`: game chips single-select with counts, >6 games → first 6 + "+N more" `<select>`; marker chips (Goal, Laugh, Manual, Imported-if-present) multi; top tag chips multi; Starred chip; "Clear" when `isFiltered`. Zero-count chips disabled, not hidden. Chips are `<button aria-pressed>`.
  - Groups only for newest/oldest; sticky heading = absolutely positioned element at scroll top showing `headingAt(...)`; heading rows show label + count.
  - Card: scrub on pointer move (frame = `floor(x / width * 8)`, `background-position` on the strip; progress line), fallback to `Thumb`; meta "game · date · size"; "Compressed" badge when `original_size != null && original_size > size`; marker badges use `mk-*` colour backgrounds, counts when > 1.
  - Remembered state now holds `filter: LibraryFilter`, `sort`, `scrollTop`.
  - Empty states: keep both messages; filtered empty offers "Clear filters".
- [ ] **Step 1: Implement** the components and Library changes.
- [ ] **Step 2: Run** `npx tsc --noEmit` and `npx vitest run` — no errors, all PASS.
- [ ] **Step 3: Verify in browser** (`preview_start vite`, `http://localhost:1420/?mock`, 1400×900): groups and sticky heading while scrolling; each chip changes the list and counts; Longest sort removes headings; hover scrub changes frames after a short rest; no console errors; scroll stays smooth with the mock enlarged to 2000 clips (temporary fixture multiplier, not committed).
- [ ] **Step 4: Commit** `feat(library): date groups, filter chips, sorts and hover scrub`

---

## Phase 2 — Manage

### Task 7: Bulk backend commands

**Files:**
- Modify: `clipvault/src-tauri/src/db/clips.rs`, `clipvault/src-tauri/src/library.rs`, `clipvault/src-tauri/src/commands.rs`, `clipvault/src-tauri/src/lib.rs`, `clipvault/src/api.ts`, `clipvault/src/mock/backend.ts`

**Interfaces:**
- Produces:
  ```rust
  pub fn set_starred_many(conn: &Connection, ids: &[String], starred: bool) -> Result<Vec<Clip>>; // one transaction; unknown ids skipped
  pub fn add_tags_many(conn: &Connection, ids: &[String], tags: &[String]) -> Result<Vec<Clip>>;  // merge, case-insensitive dedupe, keep existing order then new
  ```
  Commands `set_starred_many(ids, starred)`, `add_tags_many(ids, tags)`; TS `api.setStarredMany(ids, starred): Promise<Clip[]>`, `api.addTagsMany(ids, tags): Promise<Clip[]>`.
- [ ] **Step 1: Failing tests:** star 3 of 4 → those 3 starred, 4th untouched; unknown id ignored; add `["Clutch","ranked"]` to a clip with `["clutch"]` → `["clutch","ranked"]`; empty tag strings ignored.
- [ ] **Step 2: Run** `cargo test ... many` — FAIL.
- [ ] **Step 3: Implement**, register commands, mock equivalents.
- [ ] **Step 4: Run** full `cargo test` — PASS.
- [ ] **Step 5: Commit** `feat(library): bulk star and tag commands`

### Task 8: Selection logic and storage summary

**Files:**
- Create: `clipvault/src/lib/selection.ts`, `clipvault/src/lib/selection.test.ts`, `clipvault/src/lib/storage.ts`, `clipvault/src/lib/storage.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export function toggle(sel: ReadonlySet<string>, id: string): Set<string>;
  export function selectRange(sel: ReadonlySet<string>, order: string[], anchor: string | null, id: string): Set<string>; // anchor missing from order → toggle(id)
  export function prune(sel: ReadonlySet<string>, visible: string[]): Set<string>;
  export function storageSummary(clips: Clip[]): { total: number; saved: number; lowerBound: boolean }; // lowerBound: some compressed clip has original_size null
  export async function runBulk<T>(ids: string[], op: (id: string) => Promise<T>, opts?: { signal?: AbortSignal; onProgress?(done: number): void }): Promise<{ ok: string[]; failed: { id: string; error: string }[] }>; // sequential, stops before next id when aborted
  ```
- [ ] **Step 1: Failing tests:** range forwards and backwards adds the inclusive span to the existing selection; stale anchor → single toggle; prune drops hidden ids; `storageSummary` sums `size`, `saved = Σ(original_size - size)` where original_size > size, `lowerBound` true with one compressed null; `runBulk` with one rejecting op returns 2 ok + 1 failed, abort after first leaves rest unprocessed.
- [ ] **Step 2: Run** `npx vitest run src/lib/selection.test.ts src/lib/storage.test.ts` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(library): selection, bulk runner and storage summary`

### Task 9: Selection UI, bulk bar, context menu, keyboard, storage line

**Files:**
- Create: `clipvault/src/components/BulkBar.tsx`, `clipvault/src/components/ContextMenu.tsx`
- Modify: `clipvault/src/components/ClipCard.tsx`, `clipvault/src/pages/Library.tsx`, `clipvault/src/styles.css`

**Interfaces:**
- Consumes: Tasks 3 (`moveFocus`), 7, 8; `ConfirmDialog` from `components/ui.tsx`; `addTags` rules from `lib/tags.ts`; `formatBytes`.
- `ClipCard` gains `selected: boolean; selecting: boolean; focused: boolean; onSelect(c: Clip, e: { shift: boolean }): void; onContext(c: Clip, x: number, y: number): void`.
- Behaviour (spec): checkbox top-left on hover, always visible while `selecting`; Ctrl+A selects visible, Esc clears; selection pruned on filter change. Bulk bar replaces FilterBar: "N selected · size" · Star · Unstar · Add tag… (popover, Enter commits) · Compress (skips `compressed`) · Export… (Task 10) · Delete (ConfirmDialog: "Move N clips (X GB) to the Recycle Bin?") · Cancel. Delete/compress use `runBulk`; failures → one toast listing count + first error; succeeded ids removed from list/selection. Context menu: Open, Reveal in folder, Star/Unstar, Rename… (inline title prompt → `updateClipMeta` keeping description/tags), Compress, Delete; acts on the selection when the card is selected and says "N clips". Keyboard (when focus is not in an input): arrows via `moveFocus`, Enter opens, Space toggles, S stars, Delete asks. Focused card scrolls into view (`rows[i].top`). Toolbar right: "N of M clips · {formatBytes(total)}", tooltip "{saved} saved by compression" prefixed "at least " when `lowerBound`.
- [ ] **Step 1: Implement.**
- [ ] **Step 2: Run** `npx tsc --noEmit` and `npx vitest run` — PASS.
- [ ] **Step 3: Verify in browser** (`?mock`): shift-range, Ctrl+A under a chip filter, bulk star/tag reflect immediately, delete confirm text shows count and size, context menu on selected vs unselected card, arrow navigation across a group heading, storage tooltip; no console errors.
- [ ] **Step 4: Commit** `feat(library): multi-select, bulk actions, context menu and keyboard`

### Task 10: Bulk export dialog

**Files:**
- Create: `clipvault/src/components/BulkExportDialog.tsx`
- Modify: `clipvault/src/lib/player.ts`, `clipvault/src/lib/player.test.ts`, `clipvault/src/components/BulkBar.tsx`

**Interfaces:**
- Produces: `export function allTracksMixed(tracks: { index: number }[]): ExportTrack[]` (every track, volume 1) in `player.ts`; `BulkExportDialog(p: { clips: Clip[]; onClose(): void })`.
- Behaviour: preset `<select>` from `EXPORT_PRESETS`; Start → `runBulk` over clips: `audioTracks(id)` then `exportClip(id, exportOptions(preset, allTracksMixed(tracks)))`; shows "Exporting 3 of 12", Cancel aborts after current clip; done view: "Show in folder" (`revealInFolder` of the last output path) + list of failed titles with errors.
- [ ] **Step 1: Failing test:** `allTracksMixed([{index:0},{index:2}])` → `[{index:0,volume:1},{index:2,volume:1}]`.
- [ ] **Step 2: Run** `npx vitest run src/lib/player.test.ts` — FAIL; implement; PASS.
- [ ] **Step 3: Implement dialog**, wire to BulkBar.
- [ ] **Step 4: Verify in browser** (`?mock`): export 3 clips, progress counts up, cancel mid-run stops, done view lists a forced mock failure.
- [ ] **Step 5: Commit** `feat(library): bulk export with all tracks mixed`

### Task 11: Final check

- [ ] `npx tsc --noEmit`, `npx vitest run`, `cargo test --manifest-path src-tauri/Cargo.toml`, `cargo clippy --manifest-path src-tauri/Cargo.toml` — all clean.
- [ ] Browser pass over every spec bullet (Phase 1 + 2) under `?mock`, screenshot for the owner.
- [ ] Note for the owner what needs the real app (real FFmpeg strips, real Recycle Bin delete, migration on their `library.db` — back it up first; see MSIX AppData note).
