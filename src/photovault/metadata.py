"""Capture date and GPS from the file itself (EXIF for photos, QuickTime atoms for video).

Read-only and local: it runs on the copy already on the SSD, never on the phone.
The result's `source` says where the date came from, so questionable dates can be reviewed:
exif | quicktime | mvhd | filename | none
"""
from __future__ import annotations

import re
import struct
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

IMAGE_EXTS = {".jpg", ".jpeg", ".heic", ".heif", ".png", ".dng", ".tif", ".tiff"}
VIDEO_EXTS = {".mov", ".mp4", ".m4v"}
MIN_YEAR = 1990
_MAC_EPOCH_OFFSET = 2082844800  # seconds between 1904-01-01 and 1970-01-01


@dataclass
class Meta:
    taken: datetime | None = None   # wall-clock time where it was taken (naive)
    source: str = "none"
    lat: float | None = None
    lon: float | None = None


FS_MIN_YEAR = 2007  # first iPhone; earlier filesystem dates are copy artefacts (e.g. year 2000)


def sane(dt: datetime | None, min_year: int = MIN_YEAR) -> bool:
    return dt is not None and min_year <= dt.year <= datetime.now().year + 1


def _parse_exif_dt(s) -> datetime | None:
    try:
        return datetime.strptime(str(s).strip()[:19], "%Y:%m:%d %H:%M:%S")
    except ValueError:
        return None


def _dms(vals, ref) -> float | None:
    try:
        d, m, s = (float(v) for v in vals)
    except (TypeError, ValueError):
        return None
    out = d + m / 60 + s / 3600
    return -out if str(ref).upper() in ("S", "W") else out


def _read_image(path: Path) -> Meta:
    from PIL import Image

    try:
        import pillow_heif

        pillow_heif.register_heif_opener()
    except ImportError:
        pass
    with Image.open(path) as im:
        ex = im.getexif()
        sub = ex.get_ifd(0x8769)
        raw = sub.get(36867) or sub.get(36868) or ex.get(306)
        gps = ex.get_ifd(0x8825)
    dt = _parse_exif_dt(raw) if raw else None
    lat = lon = None
    if gps and 2 in gps and 4 in gps:
        lat, lon = _dms(gps[2], gps.get(1)), _dms(gps[4], gps.get(3))
    return Meta(dt if sane(dt) else None, "exif" if sane(dt) else "none", lat, lon)


def _atoms(f, start: int, end: int):
    pos = start
    while pos + 8 <= end:
        f.seek(pos)
        size, typ = struct.unpack(">I4s", f.read(8))
        hdr = 8
        if size == 1:
            size = struct.unpack(">Q", f.read(8))[0]
            hdr = 16
        elif size == 0:
            size = end - pos
        if size < hdr:
            return
        yield typ, pos + hdr, min(pos + size, end)
        pos += size


_ISO6709 = re.compile(r"([+-]\d+(?:\.\d+)?)([+-]\d+(?:\.\d+)?)")


def _iso6709(s: str):
    m = _ISO6709.match(s.strip())
    return (float(m.group(1)), float(m.group(2))) if m else (None, None)


def _read_video(path: Path) -> Meta:
    keys: list[str] = []
    values: dict[str, str] = {}
    mvhd_dt = None
    xyz = None
    with open(path, "rb") as f:
        f.seek(0, 2)
        total = f.tell()
        for typ, s, e in _atoms(f, 0, total):
            if typ != b"moov":
                continue
            for t2, s2, e2 in _atoms(f, s, e):
                if t2 == b"mvhd":
                    f.seek(s2)
                    ver = f.read(1)[0]
                    f.seek(s2 + 4)
                    secs = struct.unpack(">Q", f.read(8))[0] if ver == 1 else struct.unpack(">I", f.read(4))[0]
                    if secs:
                        mvhd_dt = datetime.fromtimestamp(secs - _MAC_EPOCH_OFFSET, tz=timezone.utc).astimezone().replace(tzinfo=None)
                elif t2 == b"udta":
                    for t3, s3, e3 in _atoms(f, s2, e2):
                        if t3 == b"\xa9xyz":
                            f.seek(s3 + 4)
                            xyz = f.read(e3 - s3 - 4).decode("ascii", "ignore")
                elif t2 == b"meta":
                    f.seek(s2 + 4)
                    s2 += 0 if f.read(4) == b"hdlr" else 4  # QuickTime meta has no version/flags
                    for t3, s3, e3 in _atoms(f, s2, e2):
                        if t3 == b"keys":
                            f.seek(s3 + 4)
                            n = struct.unpack(">I", f.read(4))[0]
                            for _ in range(n):
                                ksz = struct.unpack(">I", f.read(4))[0]
                                f.read(4)
                                keys.append(f.read(ksz - 8).decode("ascii", "ignore"))
                        elif t3 == b"ilst":
                            for t4, s4, e4 in _atoms(f, s3, e3):
                                idx = struct.unpack(">I", t4)[0] - 1
                                for t5, s5, e5 in _atoms(f, s4, e4):
                                    if t5 == b"data" and 0 <= idx < len(keys):
                                        f.seek(s5 + 8)
                                        values[keys[idx]] = f.read(e5 - s5 - 8).decode("utf-8", "ignore")
    lat = lon = None
    loc = values.get("com.apple.quicktime.location.ISO6709") or xyz
    if loc:
        lat, lon = _iso6709(loc)
    cd = values.get("com.apple.quicktime.creationdate")
    if cd:
        try:
            dt = datetime.fromisoformat(cd.strip()).replace(tzinfo=None)  # keep wall time at capture
            if sane(dt):
                return Meta(dt, "quicktime", lat, lon)
        except ValueError:
            pass
    if sane(mvhd_dt):
        return Meta(mvhd_dt, "mvhd", lat, lon)
    return Meta(None, "none", lat, lon)


_NAME_DATE = re.compile(r"(?<!\d)((?:19|20)\d{2})[-_]?(0[1-9]|1[0-2])[-_]?(0[1-9]|[12]\d|3[01])(?!\d)")


def date_from_name(name: str) -> datetime | None:
    m = _NAME_DATE.search(name)
    if not m:
        return None
    try:
        dt = datetime(int(m.group(1)), int(m.group(2)), int(m.group(3)))
    except ValueError:
        return None
    return dt if sane(dt) else None


def read_metadata(path: Path, name: str | None = None) -> Meta:
    """Best-effort. Never raises: a file that can't be parsed just has no embedded date."""
    name = name or path.name
    ext = Path(name).suffix.lower()
    meta = Meta()
    try:
        if ext in IMAGE_EXTS:
            meta = _read_image(path)
        elif ext in VIDEO_EXTS:
            meta = _read_video(path)
    except Exception:  # noqa: BLE001 - corrupt/odd container
        meta = Meta()
    if meta.taken is None:
        dt = date_from_name(name)
        if dt:
            meta.taken, meta.source = dt, "filename"
    return meta
