"""Routes for tags, thumbnails and trips. Reads the manifest read-only; user choices go through Manifest."""
from __future__ import annotations

import asyncio
import hashlib
import json
import re
import sqlite3
import time
from datetime import datetime, timedelta
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from pydantic import BaseModel, Field

from ..config import Config
from ..analysis import assign_to_trip, recount_trips, remove_from_trip
from ..manifest import Manifest
from ..tagging import IMAGE_EXTS, VIDEO_EXTS, make_thumbnail, video_previews_available
from .jobs import AnalysisManager

TAGS = ["memory", "trip", "screenshot", "social", "document", "duplicate", "junk"]


class TagBody(BaseModel):
    keys: list[str] = Field(min_length=1, max_length=500)
    tag: str | None = None       # None confirms the suggested tag as it is


class TripName(BaseModel):
    name: str = Field(max_length=80)


class TripKeys(BaseModel):
    keys: list[str] = Field(min_length=1, max_length=2000)


class NewTrip(BaseModel):
    name: str = Field(min_length=1, max_length=80)
    start: str = Field(pattern=r"^\d{4}-\d{2}-\d{2}$")
    end: str = Field(pattern=r"^\d{4}-\d{2}-\d{2}$")
    keys: list[str] = Field(default_factory=list, max_length=2000)   # photos to put in it besides those taken on its days


_migrated: set[str] = set()


def _ro(cfg: Config):
    db = cfg.backup_root / ".photovault" / "manifest.db"
    if not db.exists():
        return None
    if str(db) not in _migrated:
        Manifest(cfg.backup_root).close()      # creates any table added since the manifest was made
        _migrated.add(str(db))
    con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    return con


def register(app: FastAPI, current_cfg, analysis: AnalysisManager) -> None:
    def need_cfg() -> Config:
        cfg = current_cfg()
        if cfg is None:
            raise HTTPException(400, "Save your settings first.")
        return cfg

    @app.get("/api/analysis")
    async def analysis_state():
        return analysis.snapshot()

    @app.post("/api/analysis/start")
    async def analysis_start():
        cfg = need_cfg()
        try:
            analysis.start(cfg)
        except FileNotFoundError as e:
            raise HTTPException(400, str(e))
        except RuntimeError as e:
            raise HTTPException(409, str(e))
        return analysis.snapshot()

    @app.get("/api/review/summary")
    async def summary():
        cfg = need_cfg()
        con = _ro(cfg)
        empty = {"analyzed": False, "total": 0, "counts": {}, "confirmed": 0, "trips": 0, "analysis_at": None}
        if con is None:
            return empty
        try:
            rows = con.execute("SELECT COALESCE(user_tag, tag) t, COUNT(*) c FROM tags GROUP BY t").fetchall()
            if not rows:
                return empty
            confirmed = con.execute("SELECT COUNT(*) FROM tags WHERE user_tag IS NOT NULL").fetchone()[0]
            trips = con.execute("SELECT COUNT(*) FROM trips").fetchone()[0]
            at = con.execute("SELECT v FROM meta WHERE k='analysis_at'").fetchone()
        except sqlite3.OperationalError:
            return empty
        finally:
            con.close()
        counts = {r["t"]: r["c"] for r in rows}
        return {"analyzed": True, "total": sum(counts.values()), "counts": counts, "confirmed": confirmed,
                "trips": trips, "analysis_at": float(at[0]) if at else None}

    @app.get("/api/review/items")
    async def items(tag: str | None = None, trip: str | None = None, limit: int = 60, offset: int = 0,
                    order: str = "conf", spread: bool = False, q: str | None = None):
        cfg = need_cfg()
        con = _ro(cfg)
        if con is None:
            return {"items": [], "total": 0}
        where, args = [], []
        if tag:
            where.append("COALESCE(t.user_tag, t.tag) = ?")
            args.append(tag)
        if trip:
            where.append("t.trip_id = ?")
            args.append(trip)
        if q and q.strip():
            esc = q.strip().replace("!", "!!").replace("%", "!%").replace("_", "!_")
            like = "%" + esc + "%"
            where.append("(i.name LIKE ? ESCAPE '!' OR a.tags LIKE ? ESCAPE '!' OR a.caption LIKE ? ESCAPE '!')")
            args += [like, like, like]
        w = ("WHERE " + " AND ".join(where)) if where else ""
        try:
            total = con.execute("SELECT COUNT(*) FROM tags t JOIN items i ON i.key = t.key "
                                f"LEFT JOIN ai_tags a ON a.key = t.key {w}", args).fetchone()[0]
            cols = ("SELECT t.key, i.name, i.size, i.taken, i.lat, i.lon, t.tag, t.conf, t.reasons, t.user_tag, "
                    "t.trip_id, a.tags AS ai_tags, a.caption AS ai_caption, a.kind AS ai_kind "
                    "FROM tags t JOIN items i ON i.key = t.key LEFT JOIN ai_tags a ON a.key = t.key ")
            if order == "time":
                oby = "ORDER BY i.taken ASC, t.key"      # trips read like a story, in the order they happened
            else:
                oby = "ORDER BY (t.user_tag IS NOT NULL), t.conf ASC, i.taken DESC"
            lim = max(1, min(limit, 200))
            if spread:                                   # a few files from across the whole set, not just the first
                every = con.execute(cols + w + " " + oby + " LIMIT 3000", args).fetchall()
                step = max(1, -(-len(every) // lim))
                rows = every[::step][:lim]
            else:
                rows = con.execute(cols + w + " " + oby + " LIMIT ? OFFSET ?", args + [lim, max(0, offset)]).fetchall()
        except sqlite3.OperationalError:
            return {"items": [], "total": 0}
        finally:
            con.close()
        out = []
        previewable = IMAGE_EXTS | (VIDEO_EXTS if video_previews_available() else set())
        for r in rows:
            ext = Path(r["name"]).suffix.lower()
            out.append({
                "key": r["key"], "name": r["name"], "size": r["size"], "taken": r["taken"],
                "kind": "video" if ext in VIDEO_EXTS else "photo", "has_thumb": ext in previewable,
                "tag": r["user_tag"] or r["tag"], "suggested": r["tag"], "conf": r["conf"],
                "confirmed": r["user_tag"] is not None, "reasons": json.loads(r["reasons"]),
                "trip_id": r["trip_id"], "gps": r["lat"] is not None,
                "ai": ({"tags": json.loads(r["ai_tags"] or "[]"), "caption": r["ai_caption"], "kind": r["ai_kind"]}
                       if r["ai_tags"] is not None else None),
            })
        return {"items": out, "total": total}

    @app.get("/api/thumb")
    async def thumb(key: str, size: int = 320):
        cfg = need_cfg()
        size = max(64, min(size, 1280))
        con = _ro(cfg)
        row = con.execute("SELECT dest FROM items WHERE key=?", (key,)).fetchone() if con else None
        if con:
            con.close()
        if not row or not row["dest"]:
            raise HTTPException(404, "Unknown file")
        src = (cfg.backup_root / row["dest"]).resolve()
        if cfg.backup_root.resolve() not in src.parents or not src.is_file():
            raise HTTPException(404, "File not found")
        cache = cfg.backup_root / ".photovault" / "thumbs" / f"{hashlib.sha1(key.encode()).hexdigest()[:20]}_{size}.jpg"
        if not cache.exists():
            data = await asyncio.to_thread(make_thumbnail, src, size)
            if data is None:
                raise HTTPException(404, "No preview")
            cache.parent.mkdir(parents=True, exist_ok=True)
            cache.write_bytes(data)
        return FileResponse(cache, media_type="image/jpeg", headers={"Cache-Control": "private, max-age=86400"})

    @app.post("/api/review/tag")
    async def set_tag(body: TagBody):
        cfg = need_cfg()
        if body.tag is not None and body.tag not in TAGS:
            raise HTTPException(422, "Unknown tag")
        m = Manifest(cfg.backup_root)
        try:
            if body.tag is None:  # confirm: freeze the suggestion as the person's choice
                m.db.executemany("UPDATE tags SET user_tag = tag WHERE key = ?", [(k,) for k in body.keys])
            else:
                m.db.executemany("UPDATE tags SET user_tag = ? WHERE key = ?", [(body.tag, k) for k in body.keys])
            m.commit()
        finally:
            m.close()
        return {"ok": True, "changed": len(body.keys)}

    @app.get("/api/trips")
    async def trips():
        cfg = need_cfg()
        con = _ro(cfg)
        if con is None:
            return {"trips": [], "home": None, "radius_km": cfg.trip_radius_km}
        try:
            rows = con.execute("SELECT * FROM trips WHERE items > 0 OR id LIKE 'c-%' ORDER BY start").fetchall()
            home = con.execute("SELECT v FROM meta WHERE k='home'").fetchone()
            out = []
            previewable = IMAGE_EXTS | (VIDEO_EXTS if video_previews_available() else set())
            for r in rows:
                keys = [x["key"] for x in con.execute(
                    "SELECT t.key, i.name FROM tags t JOIN items i ON i.key=t.key WHERE t.trip_id=? "
                    "ORDER BY i.taken LIMIT 400", (r["id"],)) if Path(x["name"]).suffix.lower() in previewable]
                step = max(1, len(keys) // 14)
                ai = con.execute("SELECT title, summary FROM ai_trips WHERE id=?", (r["id"],)).fetchone()
                out.append({"id": r["id"], "name": r["name"] or r["auto_name"] or "", "named": bool(r["name"]),
                            "ai": {"title": ai["title"], "summary": ai["summary"]} if ai else None,
                            "auto_name": r["auto_name"], "start": r["start"], "end": r["end"], "days": r["days"],
                            "items": r["items"], "max_km": round(r["max_km"]), "places": json.loads(r["places"] or "[]"),
                            "sample": keys[::step][:14]})
        except sqlite3.OperationalError:
            return {"trips": [], "home": None, "radius_km": cfg.trip_radius_km}
        finally:
            con.close()
        h = json.loads(home[0]) if home else None
        if cfg.home_lat is not None and cfg.home_lon is not None:
            h = [cfg.home_lat, cfg.home_lon]
        return {"trips": out, "home": h, "radius_km": cfg.trip_radius_km}

    @app.put("/api/trips/{trip_id}")
    async def rename_trip(trip_id: str, body: TripName):
        cfg = need_cfg()
        m = Manifest(cfg.backup_root)
        try:
            cur = m.db.execute("UPDATE trips SET name=? WHERE id=?", (body.name.strip() or None, trip_id))
            if trip_id.startswith("c-"):
                m.db.execute("UPDATE trip_custom SET name=? WHERE id=?", (body.name.strip() or "Trip", trip_id))
            m.commit()
        finally:
            m.close()
        if cur.rowcount == 0:
            raise HTTPException(404, "Unknown trip")
        return {"ok": True}

    @app.post("/api/trips")
    async def create_trip(body: NewTrip):
        """A trip of your own, for example a train journey with no GPS. Photos taken on its days join it."""
        cfg = need_cfg()
        try:
            d0, d1 = datetime.fromisoformat(body.start), datetime.fromisoformat(body.end)
        except ValueError:
            raise HTTPException(422, "Those dates are not valid.")
        if d1 < d0:
            raise HTTPException(422, "The end date is before the start date.")
        m = Manifest(cfg.backup_root)
        try:
            tid = "c-" + format(int(time.time() * 1000), "x")
            m.db.execute("INSERT INTO trip_custom VALUES(?,?,?,?)", (tid, body.name.strip(), body.start, body.end))
            m.db.execute("INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                         (tid, body.name.strip(), "", body.start, body.end, (d1 - d0).days + 1, 0, 0, 0, 0, "[]"))
            lo, hi = d0.timestamp(), (d1 + timedelta(days=1)).timestamp()
            keys = [r[0] for r in m.db.execute(
                "SELECT t.key FROM tags t JOIN items i ON i.key = t.key WHERE i.taken >= ? AND i.taken < ?", (lo, hi))]
            n = assign_to_trip(m.db, list(dict.fromkeys(keys + body.keys)), tid)
            m.commit()
        finally:
            m.close()
        return {"id": tid, "files": n}

    @app.post("/api/trips/{trip_id}/items")
    async def add_to_trip(trip_id: str, body: TripKeys):
        cfg = need_cfg()
        m = Manifest(cfg.backup_root)
        try:
            if m.db.execute("SELECT 1 FROM trips WHERE id=?", (trip_id,)).fetchone() is None:
                raise HTTPException(404, "Unknown trip")
            n = assign_to_trip(m.db, body.keys, trip_id)
            m.commit()
        finally:
            m.close()
        return {"changed": n}

    @app.post("/api/trips/{trip_id}/remove")
    async def take_out_of_trip(trip_id: str, body: TripKeys):
        cfg = need_cfg()
        m = Manifest(cfg.backup_root)
        try:
            n = remove_from_trip(m.db, body.keys, trip_id)
            m.commit()
        finally:
            m.close()
        return {"changed": n}

    @app.delete("/api/trips/{trip_id}")
    async def delete_trip(trip_id: str):
        """Dissolve a trip. Its photos stay on the SSD and go back to Memory; nothing is deleted."""
        cfg = need_cfg()
        m = Manifest(cfg.backup_root)
        try:
            if m.db.execute("SELECT 1 FROM trips WHERE id=?", (trip_id,)).fetchone() is None:
                raise HTTPException(404, "Unknown trip")
            keys = [r[0] for r in m.db.execute("SELECT key FROM tags WHERE trip_id=?", (trip_id,))]
            remove_from_trip(m.db, keys, trip_id)
            m.db.execute("DELETE FROM trip_edits WHERE trip_id=?", (trip_id,))   # no stale pointers to a trip that is gone
            for k in keys:                                                      # they are free of it, not "removed"
                m.db.execute("DELETE FROM trip_edits WHERE key=? AND trip_id=''", (k,))
            if trip_id.startswith("c-"):
                m.db.execute("DELETE FROM trip_custom WHERE id=?", (trip_id,))
            else:
                m.db.execute("INSERT OR IGNORE INTO trip_deleted VALUES(?)", (trip_id,))   # do not find it again next time
            m.db.execute("DELETE FROM trips WHERE id=?", (trip_id,))
            recount_trips(m.db)
            m.commit()
        finally:
            m.close()
        return {"ok": True, "released": len(keys)}
