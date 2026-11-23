"""Scan, adopt and crash-safe copy.

Per file: stream into `.photovault/incoming/<id>_<name>.part` (resumable), fsync, size-check,
rename to the incoming name, read the capture date/GPS from the file itself, then atomically
rename into its dated folder. A crash at any point leaves either no final file or a complete
one. Bytes are copied untouched: no conversion; EXIF and sidecars (.AAE, Live Photo .MOV) kept.
"""
from __future__ import annotations

import asyncio
import hashlib
import os
import posixpath
import shutil
from datetime import datetime
from pathlib import Path

from .config import Config
from .device import Device, ignored
from .manifest import Manifest, Row
from .metadata import FS_MIN_YEAR, Meta, read_metadata, sane

SIDECAR_EXTS = {".aae"}


class Cancelled(Exception):
    """Raised inside a copy when the user cancels. Never retried."""


class Control:
    """Pause, resume and cancel a running backup. Checked between chunks, so it reacts within a block."""

    def __init__(self) -> None:
        self._go = asyncio.Event()
        self._go.set()
        self.paused = False
        self.cancelled = False

    def pause(self) -> None:
        self.paused = True
        self._go.clear()

    def resume(self) -> None:
        self.paused = False
        self._go.set()

    def cancel(self) -> None:
        self.cancelled = True
        self._go.set()

    async def checkpoint(self) -> None:
        await self._go.wait()
        if self.cancelled:
            raise Cancelled()


class Hooks:
    """Optional callbacks a UI can use to follow a run. Every method is a no-op by default."""

    def on_plan(self, files: int, total_bytes: int) -> None: ...
    def on_file_start(self, row: Row) -> None: ...
    def on_progress(self, row: Row, done_bytes: int) -> None: ...   # absolute bytes of this file so far
    def on_file_end(self, row: Row, ok: bool, error: str | None) -> None: ...
    def on_retry(self, row: Row, attempt: int, error: str) -> None: ...


def _epoch(dt: datetime) -> float:
    return dt.timestamp()


async def scan(device: Device, manifest: Manifest, cfg: Config, on_progress=None) -> tuple[int, int]:
    """Record every phone file in the manifest. Returns (seen, new)."""
    seen = new = 0
    async for rf in device.list_files(cfg.remote_roots, cfg.ignore):
        seen += 1
        key = Manifest.make_key(rf.name, rf.size, _epoch(rf.created))
        if manifest.upsert_seen(key, rf.path, rf.name, rf.size, _epoch(rf.created), _epoch(rf.modified)):
            new += 1
        if seen % 500 == 0:
            manifest.commit()
            if on_progress:
                on_progress(seen)
    manifest.commit()
    if on_progress:
        on_progress(seen)
    return seen, new


def adopt(manifest: Manifest, existing_dir: Path, cfg: Config) -> tuple[int, int]:
    """Mark phone files that already exist in `existing_dir` (matched by name+size) as adopted.

    Returns (matched, files_examined). Never modifies `existing_dir`.
    """
    matched = examined = 0
    for p in existing_dir.rglob("*"):
        if not p.is_file() or ignored(p.name, cfg.ignore):
            continue
        examined += 1
        for row in manifest.find_by_name_size(p.name, p.stat().st_size):
            if row.status in ("pending", "failed"):
                manifest.set_status(row.key, "adopted", dest=str(p.resolve()))
                matched += 1
                break
    return matched, examined


def sidecar_stems(name: str) -> list[str]:
    """IMG_1234.AAE belongs to IMG_1234.* and the edited IMG_E1234.*."""
    stem = Path(name).stem
    out = [stem]
    if stem.startswith("IMG_") and not stem.startswith("IMG_E"):
        out.append("IMG_E" + stem[4:])
    elif stem.startswith("IMG_E"):
        out.append("IMG_" + stem[5:])
    return out


def resolve_meta(row: Row, local: Path, manifest: Manifest) -> Meta:
    """Best capture time: file metadata, filename, sidecar sibling, then filesystem date."""
    meta = read_metadata(local, row.name)
    if meta.taken is None and Path(row.name).suffix.lower() in SIDECAR_EXTS:
        sib = manifest.sibling_taken(posixpath.dirname(row.src_path), sidecar_stems(row.name))
        if sib:
            meta = Meta(datetime.fromtimestamp(sib[0]), "sibling", sib[1], sib[2])
    if meta.taken is None:
        for ts in (row.created, row.modified):
            dt = datetime.fromtimestamp(ts)
            if sane(dt, FS_MIN_YEAR):
                return Meta(dt, "fs", meta.lat, meta.lon)
        return Meta(None, "none", meta.lat, meta.lon)
    return meta


def plan_dest(row: Row, meta: Meta, cfg: Config, manifest: Manifest) -> str:
    """Relative destination path from the layout template; collisions get a key suffix."""
    stem, ext = os.path.splitext(row.name)
    if meta.taken is None:
        rel = cfg.undated_layout.format(name=row.name, stem=stem, ext=ext.lstrip("."))
    else:
        dt = meta.taken
        rel = cfg.layout.format(
            year=f"{dt.year:04d}", month=f"{dt.month:02d}", day=f"{dt.day:02d}",
            name=row.name, stem=stem, ext=ext.lstrip("."),
        )
    if os.path.isabs(rel) or ".." in Path(rel).parts:  # reject escapes from a bad template
        raise ValueError(f"unsafe layout result: {rel}")
    rel = rel.replace("\\", "/")
    if manifest.dest_in_use(rel) or (cfg.backup_root / rel).exists():
        tag = hashlib.sha1(row.key.encode()).hexdigest()[:6]
        rel = str(Path(rel).with_name(f"{stem}_{tag}{ext}")).replace("\\", "/")
    return rel


def incoming_path(cfg: Config, row: Row) -> Path:
    tag = hashlib.sha1(row.key.encode()).hexdigest()[:10]
    return cfg.backup_root / ".photovault" / "incoming" / f"{tag}_{row.name}"


def _hash_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(4 << 20):
            h.update(block)
    return h.hexdigest()


def _place(incoming: Path, row: Row, sha: str, cfg: Config, manifest: Manifest) -> str:
    """Move a verified incoming file into its dated folder and record it. Returns dest."""
    meta = resolve_meta(row, incoming, manifest)
    rel = plan_dest(row, meta, cfg, manifest)
    dest = cfg.backup_root / rel
    dest.parent.mkdir(parents=True, exist_ok=True)
    os.replace(incoming, dest)
    os.utime(dest, (row.modified, row.modified))
    taken = _epoch(meta.taken) if meta.taken else None
    manifest.set_done(row.key, rel, sha, taken, meta.source if meta.taken else "none", meta.lat, meta.lon)
    return rel


def _write_chunk(out, h, data: bytes) -> None:
    out.write(data)
    h.update(data)


def _rehash_part(part: Path, h) -> None:
    with open(part, "rb") as f:
        while block := f.read(1 << 20):
            h.update(block)


async def _copy_stream(device: Device, row: Row, part: Path, cfg: Config,
                       control: Control | None = None, hooks: Hooks | None = None) -> str:
    """Fill `part` to row.size, resuming from its current length. Returns sha256 hex.

    Disk work runs in a thread so a UI served by the same process stays responsive.
    """
    h = hashlib.sha256()
    offset = 0
    if part.exists():
        have = part.stat().st_size
        if have > row.size:
            part.unlink()
        else:
            await asyncio.to_thread(_rehash_part, part, h)  # re-hash the bytes we already have
            offset = have
    part.parent.mkdir(parents=True, exist_ok=True)
    if hooks:
        hooks.on_progress(row, offset)
    with open(part, "ab") as out:
        if offset < row.size:
            async for data in device.read_chunks(row.src_path, offset, cfg.chunk_bytes):
                if control:
                    await control.checkpoint()
                await asyncio.to_thread(_write_chunk, out, h, data)
                offset += len(data)
                if hooks:
                    hooks.on_progress(row, offset)
        out.flush()
        await asyncio.to_thread(os.fsync, out.fileno())
    if part.stat().st_size != row.size:
        raise IOError(f"size mismatch for {row.name}: got {part.stat().st_size}, expected {row.size}")
    return h.hexdigest()


async def copy_one(device: Device, manifest: Manifest, cfg: Config, row: Row,
                   control: Control | None = None, hooks: Hooks | None = None) -> None:
    inc = incoming_path(cfg, row)
    part = inc.with_name(inc.name + ".part")
    manifest.set_status(row.key, "copying")
    if hooks:
        hooks.on_file_start(row)
    delay = 1.0
    for attempt in range(1, cfg.max_retries + 1):
        try:
            if control:
                await control.checkpoint()
            if inc.exists() and inc.stat().st_size == row.size:  # crashed after copy, before placing
                sha = await asyncio.to_thread(_hash_file, inc)
                if hooks:
                    hooks.on_progress(row, row.size)
            else:
                sha = await _copy_stream(device, row, part, cfg, control, hooks)
                os.replace(part, inc)
            _place(inc, row, sha, cfg, manifest)
            if hooks:
                hooks.on_file_end(row, True, None)
            return
        except Cancelled:
            raise  # leave the .part file: the next run resumes it
        except Exception as e:  # noqa: BLE001 - any device/IO fault: reconnect and resume
            if attempt == cfg.max_retries:
                err = f"{type(e).__name__}: {e}"
                manifest.set_status(row.key, "failed", error=err)
                if hooks:
                    hooks.on_file_end(row, False, err)
                return
            if hooks:
                hooks.on_retry(row, attempt, f"{type(e).__name__}: {e}")
            await asyncio.sleep(delay)
            delay = min(delay * 2, 20)
            try:
                await device.reconnect()
            except Exception:  # noqa: BLE001
                pass  # next attempt will try again


def _check_space(rows: list[Row], cfg: Config) -> None:
    need = sum(r.size for r in rows)
    cfg.backup_root.mkdir(parents=True, exist_ok=True)
    free = shutil.disk_usage(cfg.backup_root).free
    if free < need + int(cfg.free_space_margin_gb * 1024**3):
        raise SystemExit(
            f"Not enough space: need {need/1e9:.1f} GB + {cfg.free_space_margin_gb} GB margin, "
            f"only {free/1e9:.1f} GB free on {cfg.backup_root}"
        )


def consolidate(manifest: Manifest, cfg: Config, limit: int | None = None, progress=print) -> tuple[int, int]:
    """Copy adopted files from their old folder into the dated layout (old folder is only read).

    Returns (copied, failed). After this the old folder can be deleted by the user.
    """
    rows = manifest.adopted()[:limit]
    _check_space(rows, cfg)
    done = failed = 0
    for i, row in enumerate(rows, 1):
        src = Path(row.dest)
        try:
            if not src.is_file() or src.stat().st_size != row.size:
                raise IOError(f"source missing or size changed: {src}")
            inc = incoming_path(cfg, row)
            inc.parent.mkdir(parents=True, exist_ok=True)
            part = inc.with_name(inc.name + ".part")
            h = hashlib.sha256()
            with open(src, "rb") as fin, open(part, "wb") as out:
                while block := fin.read(4 << 20):
                    out.write(block)
                    h.update(block)
                out.flush()
                os.fsync(out.fileno())
            if part.stat().st_size != row.size:
                raise IOError(f"size mismatch copying {row.name}")
            os.replace(part, inc)
            _place(inc, row, h.hexdigest(), cfg, manifest)
            done += 1
        except Exception as e:  # noqa: BLE001
            manifest.set_status(row.key, "adopted", error=f"{type(e).__name__}: {e}")
            failed += 1
        if i % 25 == 0 or i == len(rows):
            progress(f"  {i}/{len(rows)}  ok={done} failed={failed}")
    return done, failed


async def run_copy(device: Device, manifest: Manifest, cfg: Config, progress=print,
                   limit: int | None = None, only_reencoded_originals: bool = False,
                   control: Control | None = None, hooks: Hooks | None = None) -> tuple[int, int]:
    linked = manifest.linked_phone_keys()
    if cfg.copy_originals_of_reencoded:
        manifest.db.execute("UPDATE items SET status='pending' WHERE status='skipped'")  # option switched back on
        manifest.commit()
    else:
        for k in linked:  # the user chose not to keep phone originals of re-encoded old copies
            manifest.db.execute("UPDATE items SET status='skipped' WHERE key=? AND status='pending'", (k,))
        manifest.commit()
    rows = manifest.pending()
    if only_reencoded_originals:
        rows = [r for r in rows if r.key in linked]
    # Sidecars last, so their photo/video siblings already have a capture date.
    todo = sorted(rows, key=lambda r: (Path(r.name).suffix.lower() in SIDECAR_EXTS, r.created))[:limit]
    _check_space(todo, cfg)
    if hooks:
        hooks.on_plan(len(todo), sum(r.size for r in todo))
    done = failed = 0
    total = len(todo)
    for i, row in enumerate(todo, 1):
        await copy_one(device, manifest, cfg, row, control, hooks)
        final = manifest.db.execute("SELECT status FROM items WHERE key=?", (row.key,)).fetchone()[0]
        if final == "done":
            done += 1
        else:
            failed += 1
        if i % 25 == 0 or i == total:
            progress(f"  {i}/{total}  ok={done} failed={failed}")
    return done, failed
