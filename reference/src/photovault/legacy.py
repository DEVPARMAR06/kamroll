"""Compare an old backup folder with the phone, to learn what it really holds.

Name+size matching (used by `adopt`) only finds byte-identical files with the same name.
Old backups made by exporting from the Photos app have different names, and often
re-encoded bytes with a new creation time. This module classifies every old file:

  same_name_size   same name and size as a phone file (treated as identical)
  same_size        same size, different name: almost certainly the same bytes
  converted        a phone video has the same capture time and duration, but the old
                   file is a re-encoded copy: the phone original is still worth backing up
  unmatched        nothing on the phone explains it (deleted from the phone, or its date was lost)

Nothing here modifies the old folder or the phone.
"""
from __future__ import annotations

import struct
from dataclasses import dataclass, field
from pathlib import Path

from .config import Config
from .device import Device, ignored
from .manifest import Manifest

MAC_EPOCH = 2082844800
VIDEO_EXTS = {".mov", ".mp4", ".m4v"}
TIME_TOL = 3.0   # seconds between the old file's modified time and the phone video's capture time
LOOSE_TIME_TOL = 30.0  # wider window, only when the duration matches too
DUR_TOL = 1.0    # seconds of duration difference


async def video_info(read_at, size: int):
    """(creation_utc_epoch or 0, duration_seconds) from the moov/mvhd atom, or None."""
    pos = 0
    while pos + 8 <= size:
        h = await read_at(pos, 16)
        if len(h) < 8:
            return None
        sz, typ = struct.unpack(">I4s", h[:8])
        hdr = 8
        if sz == 1 and len(h) >= 16:
            sz, hdr = struct.unpack(">Q", h[8:16])[0], 16
        elif sz == 0:
            sz = size - pos
        if sz < hdr:
            return None
        if typ == b"moov":
            d = await read_at(pos + hdr, min(sz - hdr, 4_000_000))
            p = 0
            while p + 8 <= len(d):
                s2, t2 = struct.unpack(">I4s", d[p:p + 8])
                if s2 < 8:
                    break
                if t2 == b"mvhd":
                    ver = d[p + 8]
                    if ver == 1:
                        c, _, ts, du = struct.unpack(">QQIQ", d[p + 12:p + 40])
                    else:
                        c, _, ts, du = struct.unpack(">IIII", d[p + 12:p + 28])
                    return (c - MAC_EPOCH if c else 0), (du / ts if ts else 0.0)
                p += s2
            return None
        pos += sz
    return None


@dataclass
class LegacyReport:
    examined: int = 0
    same_name_size: list[str] = field(default_factory=list)
    same_size: list[tuple[str, str]] = field(default_factory=list)
    converted: list[tuple[str, str]] = field(default_factory=list)
    speed_changed: list[tuple[str, str]] = field(default_factory=list)   # slow-mo / time-lapse exports
    unmatched: list[str] = field(default_factory=list)


async def _phone_video_info(device: Device, manifest: Manifest, row):
    cached = manifest.get_vinfo(row.key)
    if cached:
        return cached[0], cached[1]

    async def read_at(o, n):
        return await device.read_range(row.src_path, o, n)

    try:
        info = await video_info(read_at, row.size)
    except Exception:  # noqa: BLE001
        return None
    if info:
        manifest.set_vinfo(row.key, *info)
    return info


def legacy_key(f: Path, old_dir: Path, st) -> str:
    return f"legacy|{f.relative_to(old_dir).as_posix()}|{st.st_size}|{int(st.st_mtime)}"


async def compare_old(device: Device, manifest: Manifest, old_dir: Path, cfg: Config, progress=print) -> LegacyReport:
    rep = LegacyReport()
    rows = manifest.all_rows()
    by_name_size = {(r.name.lower(), r.size): r for r in rows}
    by_size: dict[int, list] = {}
    for r in rows:
        by_size.setdefault(r.size, []).append(r)
    videos = [r for r in rows if Path(r.name).suffix.lower() in VIDEO_EXTS]
    pinfo: list[tuple[float, float, str, str]] = []
    for i, r in enumerate(videos, 1):
        inf = await _phone_video_info(device, manifest, r)
        if inf:
            pinfo.append((inf[0], inf[1], r.name, r.key))
        if i % 200 == 0:
            progress(f"  read {i}/{len(videos)} phone video headers")
    manifest.commit()

    for f in sorted(old_dir.rglob("*")):
        if not f.is_file() or ignored(f.name, cfg.ignore):
            continue
        rep.examined += 1
        st = f.stat()
        if (f.name.lower(), st.st_size) in by_name_size:
            rep.same_name_size.append(f.name)
            continue
        inf = None
        if f.suffix.lower() in VIDEO_EXTS:
            with open(f, "rb") as fh:
                async def read_at(o, n, fh=fh):
                    fh.seek(o)
                    return fh.read(n)

                try:
                    inf = await video_info(read_at, st.st_size)
                except Exception:  # noqa: BLE001
                    inf = None
        same = by_size.get(st.st_size, [])
        if len(same) == 1:
            pv = manifest.get_vinfo(same[0].key) if inf else None
            # Same size alone can be a coincidence: for videos also require the same header times.
            if pv is None or (abs(pv[0] - inf[0]) <= 1 and abs(pv[1] - inf[1]) <= DUR_TOL):
                rep.same_size.append((f.name, same[0].name))
                continue
        if f.suffix.lower() in VIDEO_EXTS:
            # Capture time alone identifies the phone original. A slow-mo export keeps the capture
            # time but is slowed down, so its duration no longer matches.
            # An export's modified time can drift a few seconds from the capture time, so a
            # looser time window is allowed when the duration also agrees.
            cands = [(n, d, k) for c, d, n, k in pinfo
                     if abs(c - st.st_mtime) <= TIME_TOL
                     or (inf and abs(c - st.st_mtime) <= LOOSE_TIME_TOL and abs(d - inf[1]) <= DUR_TOL)]
            if cands:
                stem = f.stem.lower()
                best = [x for x in cands if Path(x[0]).stem.lower() == stem] or \
                       ([x for x in cands if inf and abs(x[1] - inf[1]) <= DUR_TOL]) or cands
                label = best[0][0] if len(best) == 1 else f"{len(best)} candidates"
                speed = bool(inf and abs(best[0][1] - inf[1]) > DUR_TOL * 2)
                (rep.speed_changed if speed else rep.converted).append((f.name, label))
                if len(best) == 1:  # remember the link so the phone original can be kept or skipped by option
                    manifest.set_link(legacy_key(f, old_dir, st), best[0][2], "speed-changed" if speed else "converted")
                continue
        rep.unmatched.append(f.name)
    manifest.commit()
    return rep


# ---------------------------------------------------------------------------
# Importing an old backup folder
# ---------------------------------------------------------------------------
import hashlib  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
from collections import Counter  # noqa: E402
from datetime import datetime  # noqa: E402

from .metadata import FS_MIN_YEAR, read_metadata, sane  # noqa: E402

OLD_IMPORT_DIR = "FromOldBackup"  # where an earlier version put imports; `relocate-old` moves them out
BULK_DAY_MIN = 30  # this many files sharing one modified day means the real date was lost


def legacy_taken(path: Path, name: str, mtime: float, bulk_days: set):
    """Capture time for a file from an old backup: (datetime|None, source).

    Embedded dates win, except a video header date that is later than the file's modified
    time (a re-encode stamps its own creation time; the modified time keeps the original).
    A modified day shared by many files is an import day, not a capture day.
    """
    meta = read_metadata(path, name)
    m = datetime.fromtimestamp(mtime)
    m_ok = sane(m, FS_MIN_YEAR) and m.date() not in bulk_days
    if meta.taken is not None:
        reencoded = meta.source == "mvhd" and m_ok and (meta.taken - m).days >= 1
        if not reencoded:
            return meta.taken, meta.source
    if m_ok:
        return m, "mtime"
    return None, "none"


def legacy_rel(cfg: Config, manifest: Manifest, name: str, taken, key: str, phone_names: set) -> str:
    """Destination in the normal layout. An old export that shares a name with a phone file keeps
    its own name plus ' (old export)', so the phone original always owns the clean name."""
    stem, ext = os.path.splitext(name)
    if name.lower() in phone_names:
        stem = f"{stem} (old export)"
        name = f"{stem}{ext}"
    if taken:
        rel = cfg.layout.format(year=f"{taken.year:04d}", month=f"{taken.month:02d}", day=f"{taken.day:02d}",
                                name=name, stem=stem, ext=ext.lstrip("."))
    else:
        rel = cfg.undated_layout.format(name=name, stem=stem, ext=ext.lstrip("."))
    if os.path.isabs(rel) or ".." in Path(rel).parts:
        raise ValueError(f"unsafe layout result: {rel}")
    rel = rel.replace("\\", "/")
    if manifest.dest_in_use(rel) or (cfg.backup_root / rel).exists():
        tag = hashlib.sha1(key.encode()).hexdigest()[:6]
        rel = str(Path(rel).with_name(f"{stem}_{tag}{ext}")).replace("\\", "/")
    return rel


def relocate_old(manifest: Manifest, cfg: Config, progress=print) -> tuple[int, int]:
    """Move files an earlier version put in FromOldBackup/ into the normal dated layout.

    Same-volume renames, so no data is rewritten; the recorded hashes still apply.
    """
    phone_names = {n.lower() for (n,) in manifest.db.execute("SELECT name FROM items WHERE key NOT LIKE 'legacy|%'")}
    rows = manifest.db.execute(
        "SELECT key, name, dest, taken FROM items WHERE status='done' AND key LIKE 'legacy|%' AND dest LIKE ?",
        (OLD_IMPORT_DIR + "/%",)).fetchall()
    moved = failed = 0
    for i, r in enumerate(rows, 1):
        try:
            taken = datetime.fromtimestamp(r["taken"]) if r["taken"] else None
            old = cfg.backup_root / r["dest"]
            if not old.is_file():
                raise IOError(f"missing: {old}")
            manifest.db.execute("UPDATE items SET dest=NULL WHERE key=?", (r["key"],))
            rel = legacy_rel(cfg, manifest, r["name"], taken, r["key"], phone_names)
            new = cfg.backup_root / rel
            new.parent.mkdir(parents=True, exist_ok=True)
            os.replace(old, new)
            manifest.db.execute("UPDATE items SET dest=? WHERE key=?", (rel, r["key"]))
            manifest.commit()
            moved += 1
        except Exception as e:  # noqa: BLE001
            manifest.db.execute("UPDATE items SET dest=?, error=? WHERE key=?",
                                (r["dest"], f"{type(e).__name__}: {e}", r["key"]))
            manifest.commit()
            failed += 1
        if i % 100 == 0 or i == len(rows):
            progress(f"  {i}/{len(rows)}  moved={moved} failed={failed}")
    root = cfg.backup_root / OLD_IMPORT_DIR
    if root.exists():  # remove now-empty folders, deepest first
        for d in sorted((q for q in root.rglob("*") if q.is_dir()), key=lambda q: len(q.parts), reverse=True):
            try:
                d.rmdir()
            except OSError:
                pass
        try:
            root.rmdir()
        except OSError:
            pass
    return moved, failed


def import_old(manifest: Manifest, cfg: Config, old_dir: Path, limit: int | None = None, progress=print):
    """Copy every file of an old backup folder into the normal dated layout, next to the phone files.

    Skips files already in the vault byte-for-byte (same name and size as a done file).
    The old folder is only read. Returns (copied, skipped_identical, failed).
    """
    done_ns = {(n.lower(), s) for n, s in manifest.db.execute(
        "SELECT name, size FROM items WHERE status='done' AND key NOT LIKE 'legacy|%'")}
    files = [f for f in sorted(old_dir.rglob("*")) if f.is_file() and not ignored(f.name, cfg.ignore)]
    bulk_days = {d for d, c in Counter(datetime.fromtimestamp(f.stat().st_mtime).date() for f in files).items()
                 if c >= BULK_DAY_MIN}
    phone_names = {n.lower() for (n,) in manifest.db.execute("SELECT name FROM items WHERE key NOT LIKE 'legacy|%'")}
    todo, skipped = [], 0
    for f in files:
        st = f.stat()
        key = legacy_key(f, old_dir, st)
        row = manifest.db.execute("SELECT status FROM items WHERE key=?", (key,)).fetchone()
        if row and row[0] == "done":
            continue
        if (f.name.lower(), st.st_size) in done_ns:
            skipped += 1
            continue
        todo.append((f, st, key))
    todo = todo[:limit]
    need = sum(st.st_size for _, st, _ in todo)
    cfg.backup_root.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(cfg.backup_root).free < need + int(cfg.free_space_margin_gb * 1024**3):
        raise SystemExit(f"Not enough space to import {need/1e9:.1f} GB")
    copied = failed = 0
    for i, (f, st, key) in enumerate(todo, 1):
        try:
            manifest.upsert_seen(key, str(f), f.name, st.st_size, st.st_mtime, st.st_mtime)
            manifest.set_status(key, "importing")
            inc = cfg.backup_root / ".photovault" / "incoming" / (hashlib.sha1(key.encode()).hexdigest()[:10] + "_" + f.name)
            inc.parent.mkdir(parents=True, exist_ok=True)
            part = inc.with_name(inc.name + ".part")
            h = hashlib.sha256()
            with open(f, "rb") as fin, open(part, "wb") as out:
                while block := fin.read(4 << 20):
                    out.write(block)
                    h.update(block)
                out.flush()
                os.fsync(out.fileno())
            if part.stat().st_size != st.st_size:
                raise IOError("size mismatch")
            os.replace(part, inc)
            taken, src = legacy_taken(inc, f.name, st.st_mtime, bulk_days)
            rel = legacy_rel(cfg, manifest, f.name, taken, key, phone_names)
            dest = cfg.backup_root / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            os.replace(inc, dest)
            os.utime(dest, (st.st_mtime, st.st_mtime))
            manifest.set_done(key, rel, h.hexdigest(), taken.timestamp() if taken else None, src, None, None)
            copied += 1
        except Exception as e:  # noqa: BLE001
            manifest.set_status(key, "importing", error=f"{type(e).__name__}: {e}")
            failed += 1
        if i % 50 == 0 or i == len(todo):
            progress(f"  {i}/{len(todo)}  ok={copied} failed={failed}")
    return copied, skipped, failed
