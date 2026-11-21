CREATE TABLE games (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    exe_names_json TEXT NOT NULL DEFAULT '[]',
    path           TEXT,
    enabled        INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE clips (
    id             TEXT PRIMARY KEY,
    title          TEXT NOT NULL,
    description    TEXT NOT NULL DEFAULT '',
    path           TEXT NOT NULL,
    game_id        INTEGER NOT NULL REFERENCES games(id),
    recorded_at    TEXT NOT NULL,
    duration       REAL NOT NULL,
    fps            INTEGER NOT NULL,
    width          INTEGER NOT NULL,
    height         INTEGER NOT NULL,
    size           INTEGER NOT NULL,
    trim_in        REAL,
    trim_out       REAL,
    thumbnail_path TEXT,
    source         TEXT NOT NULL CHECK (source IN ('native', 'moments')),
    source_id      TEXT,
    tags_json      TEXT NOT NULL DEFAULT '[]',
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    UNIQUE (source, source_id)
);

CREATE INDEX idx_clips_recorded_at ON clips(recorded_at);
CREATE INDEX idx_clips_game ON clips(game_id);

CREATE TABLE markers (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    clip_id   TEXT NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    t_seconds REAL NOT NULL,
    type      TEXT NOT NULL CHECK (type IN ('saved', 'goal', 'laugh', 'manual', 'imported')),
    label     TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_markers_clip ON markers(clip_id);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
