# Library: browse better, then manage better

Date: 2026-11-22. Status: awaiting owner approval (builder check done).

## Intent

The library works but is a flat, date-sorted wall of cards. Nothing is broken; the goal is an overall
upgrade so that finding, judging and tidying clips is fast. Two phases, in this order (owner's pick):

1. **Browse** — see clips in time, judge a clip without opening it, narrow the list in one click.
2. **Manage** — act on many clips at once, from mouse or keyboard, and see what the library costs on disk.

## Goals

- Date-grouped grid with sticky headings.
- Hover-scrub preview on cards.
- One-click filter chips (game, marker kind, tag, starred) with live counts; more sort orders.
- Clearer card: readable marker badges, file size, compressed state.
- Phase 2: multi-select, bulk actions, context menu, keyboard grid navigation, storage summary.

## Non-goals

- No new pages, no sidebar redesign, no change to the clip view / editor.
- No saved searches / smart collections (possible later).
- No changes to capture, save or compression logic.
- No paging: the full list still loads at once (hundreds to low thousands of rows).

## Anti-goals (a failure even if it "works")

- Scrolling or typing in search gets slower than today (the grid is row-virtualised; keep it that way).
- Hover preview spawns FFmpeg for every card the mouse brushes past.
- Any delete that bypasses the Recycle Bin, or bulk delete without a confirm that names the count.
- Generic "AI dashboard" look: keep the current dark, quiet, orange-accent style and existing tokens.

## Current state (what exists)

- `src/pages/Library.tsx`: toolbar (search with `/` shortcut, game `<select>`, Starred toggle, sort
  newest/oldest, count), virtualised grid (`lib/grid.ts`: `columnsFor`, `visibleRows`; fixed row height),
  remembered filter + scroll across visits, live insert on `clip-saved`, size update on `clip-updated`.
- Filtering happens in the backend (`list_clips` with `ListFilter {game, search, starred, sort}`), and the
  same rules are mirrored client-side in `lib/clips.ts` `matchesFilter` for live inserts.
- Thumbnails: `Thumb.tsx` asks `ensure_thumbnail` when a card scrolls into view; backend runs one FFmpeg
  at a time (`library.rs` queue), caches `<thumbs>/<id>.jpg`.
- Delete: `delete_clip` moves the file to the Recycle Bin (`trash` crate) then removes the row.
- Dev mock: `?mock` in the URL serves fixtures from `src/mock/backend.ts`.

## Approaches considered

- **A. Browse only** — biggest daily win, small backend change; leaves tidying painful.
- **B. Manage only** — fixes bulk chores, but selecting is clumsy without good filters first.
- **C. Both, browse first** — **picked.** Filters make bulk selection useful ("filter, then select all"),
  so B lands on top of A.

Filtering location, decided as the builder's call below: client-side.

## Phase 1 — Browse

### Data flow (change)

Load the **whole** library once (`list_clips` with an empty filter) and do search, chips, sort and
grouping in the client with pure functions in `lib/` (extend `lib/clips.ts` or add `lib/library.ts`).
Reason: chip counts need the unfiltered set, filtering becomes instant, and `matchesFilter` already
implements the search rules. The backend filter stays as is (unused params are fine). Search stays
debounced (200 ms).

Counts on chips reflect the **other** active filters (standard faceted behaviour): e.g. game chip counts
honour the current search and marker chips, but not the game selection itself.

### Date groups

- Headings: Today, Yesterday, This week (rest of the current Mon–Sun week), Last week, then one per
  month ("November 2026"), older years as "2025" etc. Local time.
- Heading shows the group's clip count.
- Headings are full-width rows in the virtualised list. The virtualiser moves from "fixed row height"
  to a precomputed list of row offsets (heading rows + card rows); `visibleRows` becomes a binary search
  over offsets. Pure + unit-tested like today.
- The current group's heading sticks to the top of the scroll area while scrolling.
- Groups follow the sort: date sorts group by date; non-date sorts (longest, largest, most markers)
  show **no** groups.

### Sort orders

Newest, Oldest, Longest, Largest, Most markers (excluding `saved` markers, same as the card summary).
Ties break by newest.

### Filter chips

A second toolbar row under the search:

- **Games**: one chip per game with count, single-select (replaces the game `<select>`). If more than
  ~6 games, the rest collapse into a "+N more" menu.
- **Markers**: Goal, Laugh, Manual, plus Imported only when any clip has `imported` markers
  (builder's call: Moments imports would otherwise be unreachable by chip) — multi-select, a clip matches if it has **any** selected kind.
- **Tags**: chips for the most-used tags (top ~8 by count), multi-select, clip must have **all**
  selected tags.
- **Starred** moves into this row as a chip.
- "Clear" link appears when any chip or search is active.
- Chips with count 0 under current filters are shown disabled, not hidden (avoids layout jumps).
- Filter state is remembered across visits like today (module-level `remembered`).

### Card

- Hover scrub: moving the pointer horizontally across the thumbnail shows one of 8 frames evenly spaced
  over the clip (or its trim range). A thin progress line at the bottom shows the position.
- Card footer meta: game · date · size (e.g. "1.2 GB"); a small "Compressed" badge only when the file was
  really replaced (see Storage summary: `original_size` set and larger than `size`). The existing
  `compressed` flag is also set when compression failed or did not shrink the file, so it must not drive
  the badge. Rows compressed before this change show no badge (builder's call).
- Marker badges: same icons, slightly larger with a coloured chip background per kind (existing
  `mk-*` colours), counts always shown when > 1.

### Scrub strips (backend)

- New command `ensure_scrub_strip(id) -> Option<String>`: one JPEG with 8 frames side by side
  (`fps`/`select` + `scale=320:-2` + `tile=8x1`), cached at `<thumbs>/<id>.strip.jpg`, generated through
  the same one-at-a-time FFmpeg queue as thumbnails (queued behind thumbnails). Invalidated when the trim
  changes or the clip is deleted: new hooks in `set_trim` and `delete_clip` in `commands.rs` (today
  nothing forgets the thumbnail there; only the waveform is forgotten on delete).
- Front end requests a strip only after the pointer has rested on a card for ~250 ms, caches the result
  per session (like `Thumb.tsx`), and falls back silently to the static thumbnail on failure.
- Mock backend returns a generated placeholder strip so the preview works with `?mock`.

## Phase 2 — Manage

### Selection

- A checkbox appears on card hover (top-left); once anything is selected, all cards show it.
- Click checkbox toggles; Shift-click selects the range in **current visible order**; Ctrl+A selects all
  clips matching the current filters; Esc clears.
- Selection survives filter changes only for clips still visible (hidden ones drop out).

### Bulk action bar

Replaces the chip row while a selection exists: "N selected" · Star · Unstar · Add tag… · Compress ·
Export for sharing… · Delete · Cancel.

- **Delete**: confirm dialog naming the count and total size, "moved to the Recycle Bin".
  Uses existing `delete_clip` per clip; failures are collected and reported once.
- **Add tag**: small popover with the existing tag input (`lib/tags.ts` rules).
- **Compress**: queues `compress_clip` for each; already-compressed are skipped.
- **Export**: there is no export dialog today (export is the inline "Share" block in `ClipView.tsx`), so
  this is a new small Library dialog: pick a preset (same presets as the Share block), then each clip is
  exported in sequence via `export_clip` with **all audio tracks mixed at 100%** (owner's decision; tracks
  probed per clip with `audio_tracks`). Progress ("3 of 12"), Cancel (stops after the current clip), and
  at the end one "Show in folder" plus a list of any clips that failed.

New backend commands only where a loop of existing ones would be wrong; a `set_starred_many` /
`add_tags_many` in one transaction is preferred over N IPC calls.

### Context menu

Right-click a card: Open, Reveal in folder, Star/Unstar, Rename… (the **title**, via `update_clip_meta`;
the file on disk is never renamed — builder's call, avoids races with compression and touching Moments
folders), Compress, Delete. If the card is
part of a selection, the menu acts on the whole selection (and says "N clips").

### Keyboard

Arrow keys move focus between cards (grid-aware, skipping headings), Enter opens, Space toggles
selection, S stars, Delete asks to delete. Focus ring visible. Focus scrolls into view inside the
virtualised list.

### Storage summary

Toolbar right side: "40 of 312 clips · 412 GB" where the size is the **whole library** total (builder's
call; the filtered count stays as today). The bulk bar shows the selection's size ("12 selected · 18 GB").
Tooltip on the size: "61 GB saved by compression". "Saved" needs the original size: add nullable
`original_size` to clips (schema v-next), written by `compress.rs` **only when it actually replaces the
file**. Rows compressed before the migration are unknown; the tooltip then says "at least 61 GB saved".

## Constraints

- Tauri 2 + React 19 + Vite, no new UI libraries; plain CSS in `src/styles.css` with existing tokens.
- Must stay smooth at ~2000 clips.
- Every new pure function gets vitest tests next to the existing ones (`*.test.ts`).
- Rust changes get unit tests in the module, like the rest of `src-tauri`.
- Must work in `?mock` so the UI can be checked in a browser.

## Left to the builder

- Exact chip/heading styling within the existing visual language.
- File/module split on the front end (e.g. extracting `ClipCard`, `FilterBar`, `BulkBar` components).
- Exact FFmpeg filter for the strip, as long as it is one FFmpeg run per clip and stays under ~1 s
  for a 60 s 1080p clip.
- Whether Phase 2 ships as one commit or several.
