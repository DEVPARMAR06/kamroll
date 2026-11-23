//! The manifest's tables. The same schema the Python engine creates, so either engine can open the other's file.

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
    key       TEXT PRIMARY KEY,
    src_path  TEXT NOT NULL,
    name      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    created   REAL NOT NULL,
    modified  REAL NOT NULL,
    status    TEXT NOT NULL,
    dest      TEXT,
    sha256    TEXT,
    error     TEXT,
    updated   REAL NOT NULL,
    taken     REAL,
    date_src  TEXT,
    lat       REAL,
    lon       REAL
);
CREATE TABLE IF NOT EXISTS vinfo (key TEXT PRIMARY KEY, creation REAL, duration REAL);
CREATE TABLE IF NOT EXISTS facts (
    key TEXT PRIMARY KEY, width INTEGER, height INTEGER, make TEXT, model TEXT, software TEXT,
    luma REAL, sharp REAL
);
CREATE TABLE IF NOT EXISTS tags (
    key      TEXT PRIMARY KEY,
    tag      TEXT NOT NULL,
    conf     INTEGER NOT NULL,
    reasons  TEXT NOT NULL,
    user_tag TEXT,
    trip_id  TEXT,
    updated  REAL
);
CREATE INDEX IF NOT EXISTS idx_tags_trip ON tags(trip_id);
CREATE TABLE IF NOT EXISTS trips (
    id TEXT PRIMARY KEY, name TEXT, auto_name TEXT, start TEXT, end TEXT, days INTEGER, items INTEGER,
    max_km REAL, lat REAL, lon REAL, places TEXT
);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS trip_edits (key TEXT PRIMARY KEY, trip_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS trip_custom (id TEXT PRIMARY KEY, name TEXT, start TEXT, end TEXT);
CREATE TABLE IF NOT EXISTS trip_deleted (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS ai_tags (key TEXT PRIMARY KEY, model TEXT, tags TEXT, caption TEXT, kind TEXT, updated REAL);
CREATE TABLE IF NOT EXISTS moves (key TEXT PRIMARY KEY, src TEXT, dst TEXT, reason TEXT);
CREATE TABLE IF NOT EXISTS filing_origin (key TEXT PRIMARY KEY, origin TEXT);
CREATE TABLE IF NOT EXISTS ai_trips (id TEXT PRIMARY KEY, model TEXT, title TEXT, summary TEXT, updated REAL);
CREATE TABLE IF NOT EXISTS legacy_link (
    old_key   TEXT PRIMARY KEY,
    phone_key TEXT NOT NULL,
    kind      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_status ON items(status);
CREATE INDEX IF NOT EXISTS idx_name_size ON items(name COLLATE NOCASE, size);
";
