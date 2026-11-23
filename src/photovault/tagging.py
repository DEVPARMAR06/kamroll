"""Rule-based tags: what kind of picture is this, and how sure are we? No AI, no network.

Each rule adds a reason a person can read. The best rule wins; a user's own choice always overrides it.
Tags: memory, trip (set later from GPS), screenshot, social, document, duplicate, junk.
"""
from __future__ import annotations

import io
import re
from dataclasses import dataclass, field
from pathlib import Path

IMAGE_EXTS = {".jpg", ".jpeg", ".heic", ".heif", ".png", ".dng", ".tif", ".tiff"}
VIDEO_EXTS = {".mov", ".mp4", ".m4v"}
SIDECAR_EXTS = {".aae", ".plist"}

# Pixel sizes of iPhone and iPad screens (portrait). Screenshots match one of these exactly.
_SCREENS = {(1179, 2556), (1170, 2532), (1290, 2796), (1284, 2778), (1125, 2436), (1242, 2688), (828, 1792),
            (750, 1334), (1242, 2208), (1206, 2622), (1320, 2868), (640, 1136), (640, 960), (1668, 2388),
            (2048, 2732), (1640, 2360), (1488, 2266)}
SCREEN_SIZES = _SCREENS | {(h, w) for w, h in _SCREENS}

WHATSAPP = re.compile(r"^(IMG|VID|PTT|AUD|STK|DOC)-\d{8}-WA\d+", re.I)
RANDOM_ID = re.compile(r"^(filtered-)?([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{32})\.", re.I)
LONG_NUMBER = re.compile(r"^\d{9,}\.")
FINDER_COPY = re.compile(r"^.+ \d{1,2}\.[A-Za-z0-9]+$")


@dataclass
class Facts:
    """Things read from the image itself. All optional: a damaged file just has none."""
    width: int | None = None
    height: int | None = None
    make: str | None = None
    model: str | None = None
    software: str | None = None
    luma: float | None = None     # average brightness, 0 (black) to 255
    sharp: float | None = None    # edge strength; near 0 means flat or very blurry


@dataclass
class Verdict:
    tag: str
    conf: int
    reasons: list[str] = field(default_factory=list)


def _register_heif() -> None:
    try:
        import pillow_heif

        pillow_heif.register_heif_opener()
    except ImportError:
        pass


def read_facts(path: Path) -> Facts:
    """Open the image header and a small greyscale copy. Never raises."""
    try:
        from PIL import Image, ImageFilter, ImageStat

        _register_heif()
        with Image.open(path) as im:
            f = Facts(width=im.width, height=im.height)
            ex = im.getexif()
            f.make, f.model, f.software = (str(ex.get(k)).strip() if ex.get(k) else None for k in (271, 272, 305))
            if path.suffix.lower() in (".jpg", ".jpeg"):
                im.draft("L", (256, 256))
            g = im.convert("L")
            g.thumbnail((128, 128))
            f.luma = ImageStat.Stat(g).mean[0]
            f.sharp = ImageStat.Stat(g.filter(ImageFilter.FIND_EDGES)).var[0]
            return f
    except Exception:  # noqa: BLE001
        return Facts()


def video_previews_available() -> bool:
    """Video frames need PyAV (it bundles ffmpeg). Without it videos just have no preview."""
    try:
        import av  # noqa: F401

        return True
    except ImportError:
        return False


def _video_thumbnail(path: Path, size: int) -> bytes | None:
    try:
        import av
        from PIL import Image  # noqa: F401

        with av.open(str(path)) as c:
            duration = (c.duration or 0) / 1e6
            try:
                c.seek(int(min(1.0, duration / 2) * 1e6))  # about a second in, past any black first frame
            except Exception:  # noqa: BLE001
                pass
            frame = next(c.decode(video=0))
            img = frame.to_image().convert("RGB")
            rotation = getattr(frame, "rotation", 0) or 0   # iPhone portrait clips are stored sideways
            if rotation:
                img = img.rotate(rotation, expand=True)
            img.thumbnail((size, size))
            buf = io.BytesIO()
            img.save(buf, "JPEG", quality=80)
            return buf.getvalue()
    except Exception:  # noqa: BLE001
        return None


def make_thumbnail(path: Path, size: int = 320) -> bytes | None:
    """A small JPEG of an image or video, upright. None for anything we cannot draw."""
    if path.suffix.lower() in VIDEO_EXTS:
        return _video_thumbnail(path, size)
    try:
        from PIL import Image, ImageOps

        _register_heif()
        with Image.open(path) as im:
            if path.suffix.lower() in (".jpg", ".jpeg"):
                im.draft("RGB", (size * 2, size * 2))
            im = ImageOps.exif_transpose(im).convert("RGB")
            im.thumbnail((size, size))
            buf = io.BytesIO()
            im.save(buf, "JPEG", quality=80)
            return buf.getvalue()
    except Exception:  # noqa: BLE001
        return None


def classify(name: str, facts: Facts | None, date_src: str | None) -> Verdict:
    ext = Path(name).suffix.lower()
    f = facts or Facts()
    camera = bool(f.make or f.model)
    is_video = ext in VIDEO_EXTS

    if "(old export)" in name:
        return Verdict("duplicate", 80, ["Named as an old export of a video that is also on the phone"])
    if FINDER_COPY.match(name):
        return Verdict("duplicate", 70, ["The name ends in a number, which is how a copied file is named"])
    if f.luma is not None and f.luma < 3:
        return Verdict("junk", 90, ["Almost entirely black", f"Average brightness is {f.luma:.0f} of 255"])
    if f.luma is not None and f.luma < 8:
        return Verdict("junk", 45, ["Very dark: could be a night photo you want", f"Average brightness is {f.luma:.0f} of 255"])
    if WHATSAPP.match(name):
        return Verdict("social", 97, ["The name matches WhatsApp's pattern", "No camera data" if not camera else "Has camera data"])
    if not is_video and not camera and f.width and (f.width, f.height) in SCREEN_SIZES:
        return Verdict("screenshot", 98, [f"{f.width} x {f.height} is a phone or tablet screen size", "No camera make or model"])
    if ext == ".png" and not camera:
        return Verdict("screenshot", 80, ["PNG with no camera data"])
    if RANDOM_ID.match(name) or LONG_NUMBER.match(name):
        base = 82 if (is_video and date_src != "quicktime") or (not is_video and not camera) else 62
        return Verdict("social", base, ["The name is a random ID or a long number, as when a file is shared or saved from an app",
                                        "No camera recording of its own" if base > 70 else "But it has camera data"])
    if camera:
        return Verdict("memory", 88, [f"Camera recorded it ({' '.join(x for x in (f.make, f.model) if x)})"])
    if is_video and date_src == "quicktime":
        return Verdict("memory", 82, ["The video has the camera's own creation date and location"])
    if is_video:
        return Verdict("memory", 60, ["A video with no clear sign of where it came from"])
    return Verdict("memory", 55, ["No clear signal either way"])
