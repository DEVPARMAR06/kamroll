"""File trip photos into trip folders on the SSD, and put them back.

Every move is a rename inside the backup folder, so no bytes are rewritten and the recorded
fingerprints stay valid. Each move is written to a journal first, so a crash halfway is finished
or forgotten on the next run. Nothing is ever deleted, and `filing_origin` remembers where each file
came from so it can go home again when you take it out of a trip or switch filing off.
"""
from __future__ import annotations

import hashlib
import os
import posixpath
import re
from dataclasses import dataclass
from datetime import date, datetime
from pathlib import Path

from .config import Config
from .copier import sidecar_stems
from .manifest import Manifest

_BAD = re.compile(r'[<>:"/\\|?*\x00-\x1f]')
_RESERVED = {"con", "prn", "aux", "nul", *(f"com{i}" for i in range(1, 10)), *(f"lpt{i}" for i in range(1, 10))}


def safe_name(text: str, limit: int = 80) -> str:
    """A folder name that is valid on Windows, macOS and exFAT."""
    t = re.sub(r"\s+", " ", _BAD.sub("-", text)).strip(" .")[:limit].rstrip(" .")
    if not t:
        t = "Trip"
    return t + "_" if t.lower() in _RESERVED else t


@dataclass
class Move:
    key: str
    name: str
    src: str        # relative to the backup folder
    dst: str
    reason: str     # into-trip | back


def trip_label(row) -> str:
    return row["name"] or row["auto_name"] or row["start"]


def trip_rel(cfg: Config, label: str, start: date, taken: float | None, name: str) -> str:
    d = datetime.fromtimestamp(taken) if taken else datetime.combine(start, datetime.min.time())
    stem, ext = os.path.splitext(name)
    rel = cfg.trip_layout.format(
        year=f"{d.year:04d}", month=f"{d.month:02d}", day=f"{d.day:02d}", name=name, stem=stem, ext=ext.lstrip("."),
        trip=safe_name(label), trip_dated=safe_name(f"{label} ({start:%b %Y})"), trip_year=f"{start.year:04d}")
    rel = rel.replace("\\", "/")
    if rel.startswith("/") or ".." in rel.split("/"):
        raise ValueError(f"unsafe trip layout result: {rel}")
    return rel


def plan_filing(manifest: Manifest, cfg: Config) -> list[Move]:
    """Work out every move needed to match the trips and the setting. Moves nothing."""
    db = manifest.db
    trips = {r["id"]: r for r in db.execute("SELECT id, name, auto_name, start FROM trips")}
    rows = db.execute(
        "SELECT i.key, i.name, i.dest, i.taken, t.tag, t.user_tag, t.trip_id FROM items i "
        "LEFT JOIN tags t ON t.key = i.key WHERE i.status = 'done' AND i.dest IS NOT NULL").fetchall()
    origins = dict(db.execute("SELECT key, origin FROM filing_origin").fetchall())
    occupied = {r["dest"].lower(): r["key"] for r in rows}
    media_at = {(posixpath.dirname(r["dest"]).lower(), os.path.splitext(r["name"])[0].lower()): r["key"]
                for r in rows if r["tag"] is not None}

    def unique(dst: str, key: str) -> str:
        if occupied.get(dst.lower(), key) != key:
            base, ext = posixpath.splitext(dst)
            dst = f"{base}_{hashlib.sha1(key.encode()).hexdigest()[:6]}{ext}"
        return dst

    moves: dict[str, Move] = {}
    for r in rows:
        if r["tag"] is None:
            continue                                         # sidecars follow their photo, below
        eff, tid, key = r["user_tag"] or r["tag"], r["trip_id"], r["key"]
        if cfg.file_trips and eff == "trip" and tid in trips:
            t = trips[tid]
            dst, reason = trip_rel(cfg, trip_label(t), date.fromisoformat(t["start"]), r["taken"], r["name"]), "into-trip"
        elif key in origins:
            dst, reason = origins[key], "back"
        else:
            continue
        dst = unique(dst, key)
        if dst != r["dest"]:
            moves[key] = Move(key, r["name"], r["dest"], dst, reason)
            occupied[dst.lower()] = key

    for r in rows:                                           # .AAE edit files travel with their photo
        if r["tag"] is not None:
            continue
        folder = posixpath.dirname(r["dest"]).lower()
        for stem in sidecar_stems(r["name"]):
            mk = media_at.get((folder, stem.lower()))
            if mk and mk in moves:
                dst = unique(posixpath.join(posixpath.dirname(moves[mk].dst), posixpath.basename(r["dest"])), r["key"])
                if dst != r["dest"]:
                    moves[r["key"]] = Move(r["key"], r["name"], r["dest"], dst, moves[mk].reason)
                    occupied[dst.lower()] = r["key"]
                break
    return sorted(moves.values(), key=lambda m: m.dst)


def _prune_empty(folder: Path, root: Path) -> None:
    while folder != root and root in folder.parents:
        try:
            folder.rmdir()                                   # only succeeds when empty
        except OSError:
            return
        folder = folder.parent


def _finish(db, mv_key: str, dst: str, reason: str, src: str) -> None:
    db.execute("UPDATE items SET dest=? WHERE key=?", (dst, mv_key))
    if reason == "into-trip":
        db.execute("INSERT OR IGNORE INTO filing_origin VALUES(?, ?)", (mv_key, src))
    else:
        db.execute("DELETE FROM filing_origin WHERE key=?", (mv_key,))
    db.execute("DELETE FROM moves WHERE key=?", (mv_key,))
    db.commit()


def recover_moves(manifest: Manifest, cfg: Config) -> int:
    """Settle moves a crash left half done. Returns how many were completed."""
    root, done = cfg.backup_root, 0
    for r in manifest.db.execute("SELECT key, src, dst, reason FROM moves").fetchall():
        src_ok, dst_ok = (root / r["src"]).is_file(), (root / r["dst"]).is_file()
        if dst_ok and not src_ok:
            _finish(manifest.db, r["key"], r["dst"], r["reason"], r["src"])
            done += 1
        else:
            manifest.db.execute("DELETE FROM moves WHERE key=?", (r["key"],))   # it never happened
            manifest.commit()
    return done


def apply_moves(manifest: Manifest, cfg: Config, moves: list[Move], on_progress=None, should_stop=None) -> dict:
    root, moved, failed = cfg.backup_root, 0, []
    recover_moves(manifest, cfg)
    for i, mv in enumerate(moves, 1):
        if should_stop and should_stop():
            break
        src, dst = root / mv.src, root / mv.dst
        try:
            manifest.db.execute("INSERT OR REPLACE INTO moves VALUES(?,?,?,?)", (mv.key, mv.src, mv.dst, mv.reason))
            manifest.commit()
            if src.is_file():
                if dst.exists():
                    raise FileExistsError(f"{mv.dst} already exists")
                dst.parent.mkdir(parents=True, exist_ok=True)
                os.replace(src, dst)
            elif not dst.is_file():
                raise FileNotFoundError(f"{mv.src} is missing")
            _finish(manifest.db, mv.key, mv.dst, mv.reason, mv.src)
            _prune_empty(src.parent, root)
            moved += 1
        except Exception as e:  # noqa: BLE001
            manifest.db.execute("DELETE FROM moves WHERE key=?", (mv.key,))
            manifest.commit()
            failed.append({"name": mv.name, "error": f"{type(e).__name__}: {e}"})
        if on_progress and (i % 10 == 0 or i == len(moves)):
            on_progress(i, len(moves))
    return {"moved": moved, "failed": failed[:20], "failed_count": len(failed)}
