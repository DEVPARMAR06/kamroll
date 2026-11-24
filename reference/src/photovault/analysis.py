"""Tag every backed-up file and detect trips. Reads only the SSD copy; writes only derived tables."""
from __future__ import annotations

import json
import time
from collections import defaultdict
from datetime import date, datetime
from pathlib import Path

from .config import Config
from .manifest import Manifest
from .tagging import IMAGE_EXTS, SIDECAR_EXTS, Facts, Verdict, classify, read_facts
from .trips import Gazetteer, Trip, detect_home, detect_trips, haversine, merge_into_named


def _cached_facts(db, key: str) -> Facts | None:
    r = db.execute("SELECT width,height,make,model,software,luma,sharp FROM facts WHERE key=?", (key,)).fetchone()
    return Facts(*r) if r else None


def run_analysis(manifest: Manifest, cfg: Config, progress=lambda done, total, msg: None) -> dict:
    db = manifest.db
    rows = db.execute("SELECT key,name,dest,size,taken,date_src,lat,lon,sha256 FROM items WHERE status='done'").fetchall()
    media = [r for r in rows if Path(r["name"]).suffix.lower() not in SIDECAR_EXTS]
    total = len(media)

    # 1. What the pictures themselves say (cached, so a re-run only reads new files)
    facts: dict[str, Facts] = {}
    for i, r in enumerate(media):
        f = _cached_facts(db, r["key"])
        if f is None and Path(r["name"]).suffix.lower() in IMAGE_EXTS:
            f = read_facts(cfg.backup_root / r["dest"])
            db.execute("INSERT OR REPLACE INTO facts VALUES(?,?,?,?,?,?,?,?)",
                       (r["key"], f.width, f.height, f.make, f.model, f.software, f.luma, f.sharp))
        if f is not None:
            facts[r["key"]] = f
        if i % 50 == 0:
            progress(i, total, "Looking at your pictures")
            db.commit()
    db.commit()

    # 2. Files with identical bytes: keep the phone original, mark the rest
    by_hash: dict[str, list] = defaultdict(list)
    for r in media:
        if r["sha256"]:
            by_hash[r["sha256"]].append(r)
    dupe_of: dict[str, str] = {}
    for group in by_hash.values():
        if len(group) > 1:
            keep = min(group, key=lambda r: (r["key"].startswith("legacy|"), r["dest"]))
            for r in group:
                if r is not keep:
                    dupe_of[r["key"]] = keep["name"]

    # 3. Rules
    progress(total, total, "Tagging")
    verdicts: dict[str, Verdict] = {}
    for r in media:
        if r["key"] in dupe_of:
            verdicts[r["key"]] = Verdict("duplicate", 99, [f"Identical to {dupe_of[r['key']]}: the same bytes"])
        else:
            verdicts[r["key"]] = classify(r["name"], facts.get(r["key"]), r["date_src"])

    # 4. Home and trips
    progress(total, total, "Finding trips")
    items = [{"key": r["key"], "taken": r["taken"], "lat": r["lat"], "lon": r["lon"]} for r in media]
    points = [(datetime.fromtimestamp(r["taken"]).date(), r["lat"], r["lon"]) for r in media
              if r["taken"] is not None and r["lat"] is not None]
    home = (cfg.home_lat, cfg.home_lon) if cfg.home_lat is not None and cfg.home_lon is not None else detect_home(points)
    trips: list[Trip] = []
    trip_of: dict[str, str] = {}
    if home:
        deleted = {r[0] for r in db.execute("SELECT id FROM trip_deleted")}
        trips = [t for t in detect_trips(items, home, cfg.trip_radius_km, Gazetteer()) if t.id not in deleted]
        named = [(date.fromisoformat(r["start"]), date.fromisoformat(r["end"])) for r in
                 db.execute("SELECT start, end FROM trips WHERE name IS NOT NULL AND name != '' AND id NOT LIKE 'c-%'")]
        for t in trips:      # the id is the first day
            t.id = t.start.isoformat()
        trips = merge_into_named(trips, named, items)
        for t in trips:
            t.id = t.start.isoformat()
        for t in trips:
            for key in t.keys:
                trip_of[key] = t.id
                v = verdicts.get(key)
                if v and v.tag == "memory":
                    verdicts[key] = Verdict("trip", max(v.conf, 85), [
                        (f"Taken on a day trip, {t.start:%d %b %Y}" if t.start == t.end
                         else f"Taken during a trip, {t.start:%d %b} to {t.end:%d %b %Y}"),
                        f"Up to {t.max_km:.0f} km from home"] + v.reasons)

    # 4a. A trip that grew (photos from an earlier day were backed up later) gets a new id. It is still the same
    #     trip to the person, so it keeps its name, its titles and the edits they made.
    renamed: dict[str, str] = {}
    new_ids = {t.id for t in trips}
    old_trips = [r for r in db.execute("SELECT id, name, start, end FROM trips WHERE id NOT LIKE 'c-%'") if r["id"] not in new_ids]
    for t in trips:
        if db.execute("SELECT 1 FROM trips WHERE id=?", (t.id,)).fetchone():
            continue
        best = None
        for o in old_trips:
            ov = (min(date.fromisoformat(o["end"]), t.end) - max(date.fromisoformat(o["start"]), t.start)).days + 1
            if ov > 0 and (best is None or ov > best[0]):
                best = (ov, o)
        if best:
            o = best[1]
            old_trips = [x for x in old_trips if x["id"] != o["id"]]
            if o["name"]:
                renamed[t.id] = o["name"]
            db.execute("UPDATE trip_edits SET trip_id=? WHERE trip_id=?", (t.id, o["id"]))
            db.execute("UPDATE ai_trips SET id=? WHERE id=?", (t.id, o["id"]))

    # 4b. The person's own changes: photos added to or taken out of trips, and trips they made
    custom = db.execute("SELECT id, name, start, end FROM trip_custom").fetchall()
    # Photos backed up later that were taken on the days of a trip the person made join it, unless they decided otherwise.
    decided = {r[0] for r in db.execute("SELECT key FROM trip_edits")}
    for c in custom:
        c0, c1 = date.fromisoformat(c["start"]), date.fromisoformat(c["end"])
        for r in media:
            if r["taken"] is None or r["key"] in decided:
                continue
            if c0 <= datetime.fromtimestamp(r["taken"]).date() <= c1:
                trip_of[r["key"]] = c["id"]
                v = verdicts.get(r["key"])
                if v and v.tag == "memory":
                    verdicts[r["key"]] = Verdict("trip", 90, [f"Taken on the days of your trip, {c0:%d %b} to {c1:%d %b %Y}"] + v.reasons)
    valid = {t.id for t in trips} | {c["id"] for c in custom}
    for key, tid in db.execute("SELECT key, trip_id FROM trip_edits").fetchall():
        v = verdicts.get(key)
        if v is None:
            continue
        if tid == "":
            trip_of.pop(key, None)
            if v.tag == "trip":
                verdicts[key] = Verdict("memory", 80, ["Taken out of the trip by you"])
        elif tid in valid:
            trip_of[key] = tid
            if v.tag == "memory":
                verdicts[key] = Verdict("trip", 95, ["Added to the trip by you"])

    # 5. Save. A user's own tag choices and trip names survive a re-run.
    now = time.time()
    for key, v in verdicts.items():
        db.execute(
            "INSERT INTO tags(key,tag,conf,reasons,trip_id,updated) VALUES(?,?,?,?,?,?) "
            "ON CONFLICT(key) DO UPDATE SET tag=excluded.tag, conf=excluded.conf, reasons=excluded.reasons, "
            "trip_id=excluded.trip_id, updated=excluded.updated",
            (key, v.tag, v.conf, json.dumps(v.reasons), trip_of.get(key), now))
    old_names = {r[0]: r[1] for r in db.execute("SELECT id, name FROM trips WHERE name IS NOT NULL AND name != ''")}
    old_names.update(renamed)
    db.execute("DELETE FROM trips")
    members = defaultdict(int)
    for tid in trip_of.values():
        members[tid] += 1
    for t in trips:
        if members[t.id] == 0:
            continue                      # every photo was taken out of it
        db.execute("INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                   (t.id, old_names.get(t.id), t.auto_name, t.start.isoformat(), t.end.isoformat(), t.days,
                    members[t.id], t.max_km, t.lat, t.lon, json.dumps(t.places)))
    for c in custom:
        days = (datetime.fromisoformat(c["end"]).date() - datetime.fromisoformat(c["start"]).date()).days + 1
        db.execute("INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                   (c["id"], c["name"], "", c["start"], c["end"], max(days, 1), members[c["id"]], 0, 0, 0, "[]"))
    db.execute("INSERT OR REPLACE INTO meta VALUES('analysis_at', ?)", (str(now),))
    if home:
        db.execute("INSERT OR REPLACE INTO meta VALUES('home', ?)", (json.dumps(list(home)),))
    manifest.commit()
    counts: dict[str, int] = defaultdict(int)
    for v in verdicts.values():
        counts[v.tag] += 1
    return {"total": total, "counts": dict(counts), "trips": len(trips), "home": list(home) if home else None}


def recount_trips(db) -> None:
    """Refresh the file count of every trip after photos were added or taken out."""
    counts = dict(db.execute("SELECT trip_id, COUNT(*) FROM tags WHERE trip_id IS NOT NULL GROUP BY trip_id").fetchall())
    for (tid,) in db.execute("SELECT id FROM trips").fetchall():
        db.execute("UPDATE trips SET items=? WHERE id=?", (counts.get(tid, 0), tid))


def assign_to_trip(db, keys: list[str], trip_id: str) -> int:
    """Put photos into a trip. Remembered, so a later re-analysis keeps it."""
    n = 0
    for k in keys:
        if db.execute("SELECT 1 FROM tags WHERE key=?", (k,)).fetchone() is None:
            continue
        db.execute("INSERT OR REPLACE INTO trip_edits VALUES(?,?)", (k, trip_id))
        db.execute("UPDATE tags SET trip_id=? WHERE key=?", (trip_id, k))
        db.execute("UPDATE tags SET tag='trip', conf=95, reasons=? WHERE key=? AND tag='memory' AND user_tag IS NULL",
                   (json.dumps(["Added to the trip by you"]), k))
        db.execute("UPDATE tags SET user_tag='trip' WHERE key=? AND user_tag='memory'", (k,))
        n += 1
    recount_trips(db)
    return n


def remove_from_trip(db, keys: list[str], trip_id: str | None = None) -> int:
    """Take photos out of their trip. They go back to Memory unless you tagged them something else."""
    n = 0
    for k in keys:
        row = db.execute("SELECT trip_id FROM tags WHERE key=?", (k,)).fetchone()
        if row is None or row[0] is None or (trip_id and row[0] != trip_id):
            continue
        db.execute("INSERT OR REPLACE INTO trip_edits VALUES(?,?)", (k, ""))
        db.execute("UPDATE tags SET trip_id=NULL WHERE key=?", (k,))
        db.execute("UPDATE tags SET tag='memory', conf=80, reasons=? WHERE key=? AND tag='trip' AND user_tag IS NULL",
                   (json.dumps(["Taken out of the trip by you"]), k))
        db.execute("UPDATE tags SET user_tag='memory' WHERE key=? AND user_tag='trip'", (k,))
        n += 1
    recount_trips(db)
    return n
