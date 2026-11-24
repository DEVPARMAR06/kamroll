"""Verification and the phone-vs-SSD sync report."""
from __future__ import annotations

import asyncio
import hashlib
import os
import time
from dataclasses import dataclass, field

from .config import Config
from .copier import Cancelled, _epoch
from .device import Device
from .manifest import Manifest


@dataclass
class VerifyReport:
    checked: int = 0
    missing: list[str] = field(default_factory=list)
    wrong_size: list[str] = field(default_factory=list)
    corrupt: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not (self.missing or self.wrong_size or self.corrupt)


def verify_ssd(manifest: Manifest, cfg: Config, deep: bool = False, progress=print,
               on_progress=None, should_stop=None) -> VerifyReport:
    """Quick: every 'done' file exists with the recorded size. Deep: re-hash and compare SHA-256.

    on_progress(done, total) is called now and then; should_stop() lets a UI cancel a long deep check.
    """
    rep = VerifyReport()
    rows = manifest.done_rows()
    total = len(rows)
    for i, r in enumerate(rows, 1):
        if should_stop and should_stop():
            raise Cancelled()
        p = cfg.backup_root / r["dest"]
        rep.checked += 1
        if not p.is_file():
            rep.missing.append(r["dest"])
        elif p.stat().st_size != r["size"]:
            rep.wrong_size.append(r["dest"])
        elif deep:
            h = hashlib.sha256()
            with open(p, "rb") as f:
                while block := f.read(4 << 20):
                    h.update(block)
                    if should_stop and should_stop():
                        raise Cancelled()
            if h.hexdigest() != r["sha256"]:
                rep.corrupt.append(r["dest"])
        if deep and i % 200 == 0:
            progress(f"  verified {i}/{total}")
        if on_progress and (i % 20 == 0 or i == total):
            on_progress(i, total)
    return rep


def repair_problems(manifest: Manifest, cfg: Config, rep: VerifyReport) -> dict:
    """Queue every missing, wrong-size or corrupt file for copying again on the next backup.

    A damaged file is moved aside into .photovault/quarantine (never deleted). Files that came from
    an old folder rather than the phone cannot be fetched again, so they are only reported.
    """
    queued = skipped_legacy = 0
    quarantine = cfg.backup_root / ".photovault" / "quarantine" / time.strftime("%Y%m%d-%H%M%S")
    for rel in rep.missing + rep.wrong_size + rep.corrupt:
        row = manifest.db.execute("SELECT key FROM items WHERE dest=? AND status='done'", (rel,)).fetchone()
        if row is None:
            continue
        if row[0].startswith("legacy|"):
            skipped_legacy += 1
            continue
        p = cfg.backup_root / rel
        if p.is_file():
            quarantine.mkdir(parents=True, exist_ok=True)
            os.replace(p, quarantine / p.name)
        manifest.db.execute(
            "UPDATE items SET status='pending', dest=NULL, sha256=NULL, error=NULL, updated=? WHERE key=?",
            (time.time(), row[0]))
        queued += 1
    manifest.commit()
    return {"queued": queued, "legacy_skipped": skipped_legacy}


@dataclass
class SyncReport:
    on_phone: int = 0
    backed_up: int = 0
    adopted_not_copied: list[str] = field(default_factory=list)
    missing_on_ssd: list[str] = field(default_factory=list)
    only_on_ssd: int = 0
    skipped_by_choice: int = 0
    ssd: VerifyReport = field(default_factory=VerifyReport)

    @property
    def in_sync(self) -> bool:
        return not self.missing_on_ssd and not self.adopted_not_copied and self.ssd.ok


async def sync_report(device: Device, manifest: Manifest, cfg: Config, on_progress=None) -> SyncReport:
    """Fresh phone listing vs manifest vs the files actually on the SSD."""
    rep = SyncReport()
    phone_keys = set()
    async for rf in device.list_files(cfg.remote_roots, cfg.ignore):
        rep.on_phone += 1
        if on_progress and rep.on_phone % 500 == 0:
            on_progress(rep.on_phone)
        key = manifest.make_key(rf.name, rf.size, _epoch(rf.created))
        phone_keys.add(key)
        row = manifest.db.execute("SELECT status FROM items WHERE key=?", (key,)).fetchone()
        if row is None or row[0] in ("pending", "copying", "failed"):
            rep.missing_on_ssd.append(rf.path)
        elif row[0] == "skipped":
            rep.skipped_by_choice += 1
        elif row[0] == "adopted":
            rep.adopted_not_copied.append(rf.path)
        else:
            rep.backed_up += 1
    done_keys = {r[0] for r in manifest.db.execute("SELECT key FROM items WHERE status='done' AND key NOT LIKE 'legacy|%'")}
    rep.only_on_ssd = len(done_keys - phone_keys)  # deleted from the phone after backup: fine
    rep.ssd = await asyncio.to_thread(verify_ssd, manifest, cfg, False, lambda *_: None)
    return rep
