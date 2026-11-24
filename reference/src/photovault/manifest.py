"""SQLite manifest stored ON the SSD so Mac and Windows share one source of truth."""
from __future__ import annotations

import sqlite3
import time
from dataclasses import dataclass
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS items (
    key       TEXT PRIMARY KEY,      -- name|size|created_epoch
    src_path  TEXT NOT NULL,
    name      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    created   REAL NOT NULL,
    modified  REAL NOT NULL,
    status    TEXT NOT NULL,         -- pending | copying | done | adopted | failed
    dest      TEXT,                  -- relative to backup root, or absolute (adopted)
    sha256    TEXT,
    error     TEXT,
    updated   REAL NOT NULL,
    taken     REAL,                  -- capture time (wall clock), from the file itself
    date_src  TEXT,                  -- exif | quicktime | mvhd | filename | sibling | fs | none
    lat       REAL,
    lon       REAL
);
CREATE TABLE IF NOT EXISTS vinfo (
    key      TEXT PRIMARY KEY,
    creation REAL,                   -- mvhd creation time (UTC epoch), 0 if unset
    duration REAL                    -- seconds
);
CREATE TABLE IF NOT EXISTS facts (
    key TEXT PRIMARY KEY, width INTEGER, height INTEGER, make TEXT, model TEXT, software TEXT,
    luma REAL, sharp REAL
);
CREATE TABLE IF NOT EXISTS tags (
    key      TEXT PRIMARY KEY,
    tag      TEXT NOT NULL,          -- suggested by the rules
    conf     INTEGER NOT NULL,       -- 0-100
    reasons  TEXT NOT NULL,          -- JSON list of plain-language reasons
    user_tag TEXT,                   -- the person's own choice, wins over tag
    trip_id  TEXT,
    updated  REAL
);
CREATE INDEX IF NOT EXISTS idx_tags_trip ON tags(trip_id);
CREATE TABLE IF NOT EXISTS trips (
    id TEXT PRIMARY KEY, name TEXT, auto_name TEXT, start TEXT, end TEXT, days INTEGER, items INTEGER,
    max_km REAL, lat REAL, lon REAL, places TEXT
);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS trip_edits (key TEXT PRIMARY KEY, trip_id TEXT NOT NULL);   -- '' = taken out of every trip
CREATE TABLE IF NOT EXISTS trip_custom (id TEXT PRIMARY KEY, name TEXT, start TEXT, end TEXT);
CREATE TABLE IF NOT EXISTS trip_deleted (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS ai_tags (key TEXT PRIMARY KEY, model TEXT, tags TEXT, caption TEXT, kind TEXT, updated REAL);
CREATE TABLE IF NOT EXISTS moves (key TEXT PRIMARY KEY, src TEXT, dst TEXT, reason TEXT);       -- journal of a move in progress
CREATE TABLE IF NOT EXISTS filing_origin (key TEXT PRIMARY KEY, origin TEXT);                  -- where a filed photo came from
CREATE TABLE IF NOT EXISTS ai_trips (id TEXT PRIMARY KEY, model TEXT, title TEXT, summary TEXT, updated REAL);
CREATE TABLE IF NOT EXISTS legacy_link (
    old_key   TEXT PRIMARY KEY,      -- legacy|... key of the file in the old folder
    phone_key TEXT NOT NULL,         -- the phone original it is a copy of
    kind      TEXT NOT NULL          -- converted | speed-changed
);
CREATE INDEX IF NOT EXISTS idx_status ON items(status);
CREATE INDEX IF NOT EXISTS idx_name_size ON items(name COLLATE NOCASE, size);
"""


@dataclass
class Row:
    key: str
    src_path: str
    name: str
    size: int
    created: float
    modified: float
    status: str
    dest: str | None


class Manifest:
    def __init__(self, backup_root: Path):
        meta = backup_root / ".photovault"
        meta.mkdir(parents=True, exist_ok=True)
        # Used from one task at a time (the engine awaits any thread work), so sharing is safe.
        self.db = sqlite3.connect(meta / "manifest.db", check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.executescript(SCHEMA)
        have = {r[1] for r in self.db.execute("PRAGMA table_info(items)")}
        for col, typ in (("taken", "REAL"), ("date_src", "TEXT"), ("lat", "REAL"), ("lon", "REAL")):
            if col not in have:  # upgrade a manifest created by an older version
                self.db.execute(f"ALTER TABLE items ADD COLUMN {col} {typ}")
        # A previous run died mid-copy: those rows are simply pending again.
        self.db.execute("UPDATE items SET status='pending' WHERE status='copying'")
        self.db.commit()

    def close(self) -> None:
        self.db.close()

    @staticmethod
    def make_key(name: str, size: int, created: float) -> str:
        return f"{name}|{size}|{int(created)}"

    def upsert_seen(self, key, src_path, name, size, created, modified) -> bool:
        """Record a phone file. Returns True if it is new to the manifest."""
        cur = self.db.execute("SELECT 1 FROM items WHERE key=?", (key,))
        if cur.fetchone():
            return False
        self.db.execute(
            "INSERT INTO items(key,src_path,name,size,created,modified,status,updated)"
            " VALUES(?,?,?,?,?,?, 'pending', ?)",
            (key, src_path, name, size, created, modified, time.time()),
        )
        return True

    def commit(self) -> None:
        self.db.commit()

    def pending(self) -> list[Row]:
        cur = self.db.execute(
            "SELECT key,src_path,name,size,created,modified,status,dest FROM items"
            " WHERE status IN ('pending','failed') ORDER BY created"
        )
        return [Row(**dict(r)) for r in cur]

    def set_link(self, old_key, phone_key, kind) -> None:
        self.db.execute("INSERT OR REPLACE INTO legacy_link VALUES(?,?,?)", (old_key, phone_key, kind))

    def linked_phone_keys(self) -> set[str]:
        return {r[0] for r in self.db.execute("SELECT DISTINCT phone_key FROM legacy_link")}

    def get_vinfo(self, key):
        return self.db.execute("SELECT creation, duration FROM vinfo WHERE key=?", (key,)).fetchone()

    def set_vinfo(self, key, creation, duration) -> None:
        self.db.execute("INSERT OR REPLACE INTO vinfo VALUES(?,?,?)", (key, creation, duration))

    def all_rows(self) -> list[Row]:
        """Every phone file (rows imported from an old folder are not phone files)."""
        cur = self.db.execute(
            "SELECT key,src_path,name,size,created,modified,status,dest FROM items WHERE key NOT LIKE 'legacy|%'")
        return [Row(**dict(r)) for r in cur]

    def adopted(self) -> list[Row]:
        cur = self.db.execute(
            "SELECT key,src_path,name,size,created,modified,status,dest FROM items"
            " WHERE status='adopted' ORDER BY created"
        )
        return [Row(**dict(r)) for r in cur]

    def set_done(self, key, dest, sha256, taken, date_src, lat, lon) -> None:
        self.db.execute(
            "UPDATE items SET status='done', dest=?, sha256=?, error=NULL, updated=?,"
            " taken=?, date_src=?, lat=?, lon=? WHERE key=?",
            (dest, sha256, time.time(), taken, date_src, lat, lon, key),
        )
        self.db.commit()

    def sibling_taken(self, src_dir: str, stems: list[str]):
        """Capture time of a finished file sharing a stem (used for .AAE sidecars)."""
        def esc(x: str) -> str:
            return x.replace("!", "!!").replace("_", "!_").replace("%", "!%")

        for stem in stems:
            row = self.db.execute(
                "SELECT taken, lat, lon FROM items WHERE status='done' AND taken IS NOT NULL"
                " AND date_src IN ('exif','quicktime','mvhd') AND src_path LIKE ? ESCAPE '!'"
                " AND name LIKE ? ESCAPE '!' AND name NOT LIKE '%.AAE' LIMIT 1",
                (esc(src_dir) + "/%", esc(stem) + ".%"),
            ).fetchone()
            if row:
                return row[0], row[1], row[2]
        return None

    def set_status(self, key, status, dest=None, sha256=None, error=None) -> None:
        self.db.execute(
            "UPDATE items SET status=?, dest=COALESCE(?,dest), sha256=COALESCE(?,sha256),"
            " error=?, updated=? WHERE key=?",
            (status, dest, sha256, error, time.time(), key),
        )
        self.db.commit()

    def dest_in_use(self, rel: str) -> bool:
        cur = self.db.execute("SELECT 1 FROM items WHERE dest=? COLLATE NOCASE", (rel,))
        return cur.fetchone() is not None

    def find_by_name_size(self, name: str, size: int) -> list[Row]:
        cur = self.db.execute(
            "SELECT key,src_path,name,size,created,modified,status,dest FROM items"
            " WHERE name=? COLLATE NOCASE AND size=?",
            (name, size),
        )
        return [Row(**dict(r)) for r in cur]

    def date_sources(self) -> dict[str, int]:
        cur = self.db.execute("SELECT COALESCE(date_src,'-') s, COUNT(*) c FROM items WHERE status='done' GROUP BY s")
        return {r["s"]: r["c"] for r in cur}

    def done_rows(self):
        return self.db.execute("SELECT key,name,size,dest,sha256 FROM items WHERE status='done'").fetchall()

    def counts(self) -> dict[str, int]:
        cur = self.db.execute("SELECT status, COUNT(*) c, COALESCE(SUM(size),0) s FROM items GROUP BY status")
        return {r["status"]: (r["c"], r["s"]) for r in cur}
