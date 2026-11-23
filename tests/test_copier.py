import asyncio
import hashlib
from datetime import datetime, timedelta

import pytest

from photovault.config import Config
from photovault.copier import adopt, copy_one, run_copy, scan
from photovault.device import RemoteFile
from photovault.manifest import Manifest


class FakeDevice:
    """In-memory phone. fail_after_bytes simulates the cable dropping mid-file."""

    def __init__(self, files: dict[str, bytes]):
        self.files = files
        self.fail_after_bytes: int | None = None
        self.reconnects = 0
        self.delay = 0.0
        self.t0 = datetime(2026, 3, 14, 10, 0, 0)
        self.times: dict[str, datetime] = {}

    async def list_files(self, roots, ignore):
        for i, (path, data) in enumerate(sorted(self.files.items())):
            if path.rsplit("/", 1)[-1].startswith("._"):
                continue
            t = self.times.get(path, self.t0 + timedelta(minutes=i))
            yield RemoteFile(path, len(data), t, t)

    async def read_chunks(self, path, offset, chunk):
        data = self.files[path]
        sent = 0
        for pos in range(offset, len(data), chunk):
            if self.fail_after_bytes is not None and sent >= self.fail_after_bytes:
                self.fail_after_bytes = None  # fail once
                raise ConnectionError("cable unplugged")
            if self.delay:
                await asyncio.sleep(self.delay)
            piece = data[pos:pos + chunk]
            sent += len(piece)
            yield piece

    async def read_range(self, path, offset, n):
        return self.files[path][offset:offset + n]

    async def reconnect(self):
        self.reconnects += 1


@pytest.fixture
def cfg(tmp_path):
    return Config(ssd_root=tmp_path, chunk_mb=1, max_retries=3, free_space_margin_gb=0)


@pytest.fixture
def manifest(cfg):
    m = Manifest(cfg.backup_root)
    yield m
    m.close()


def blob(n, seed=1):
    return bytes((i * seed) % 251 for i in range(n))


async def test_copies_original_bytes_untouched(cfg, manifest):
    files = {"/DCIM/100APPLE/IMG_0001.JPG": blob(3_000_000), "/DCIM/100APPLE/IMG_0001.AAE": b"<plist/>"}
    dev = FakeDevice(files)
    await scan(dev, manifest, cfg)
    done, failed = await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (done, failed) == (2, 0)
    out = {p.name: p.read_bytes() for p in cfg.backup_root.rglob("IMG_0001.*")}
    assert out["IMG_0001.JPG"] == files["/DCIM/100APPLE/IMG_0001.JPG"]
    assert out["IMG_0001.AAE"] == b"<plist/>"  # sidecar preserved next to the image
    row = manifest.db.execute("SELECT sha256 FROM items WHERE name='IMG_0001.JPG'").fetchone()
    assert row[0] == hashlib.sha256(files["/DCIM/100APPLE/IMG_0001.JPG"]).hexdigest()


async def test_cable_drop_resumes_and_leaves_no_partial(cfg, manifest):
    data = blob(5_000_000)
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0002.MOV": data})
    dev.fail_after_bytes = 2_000_000
    await scan(dev, manifest, cfg)
    done, failed = await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (done, failed) == (1, 0)
    assert dev.reconnects == 1
    final = next(cfg.backup_root.rglob("IMG_0002.MOV"))
    assert final.read_bytes() == data
    assert not list(cfg.backup_root.rglob("*.part"))


async def test_rerun_copies_nothing_new(cfg, manifest):
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0003.JPG": blob(1000)})
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    seen, new = await scan(dev, manifest, cfg)
    assert (seen, new) == (1, 0)
    assert manifest.pending() == []


async def test_persistent_failure_marks_failed_and_continues(cfg, manifest):
    dev = FakeDevice({"/DCIM/100APPLE/A.JPG": blob(10), "/DCIM/100APPLE/B.JPG": blob(20)})
    orig = dev.read_chunks

    async def flaky(path, offset, chunk):
        if path.endswith("A.JPG"):
            raise ConnectionError("always fails")
        async for c in orig(path, offset, chunk):
            yield c

    dev.read_chunks = flaky
    cfg.max_retries = 2
    await scan(dev, manifest, cfg)
    done, failed = await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (done, failed) == (1, 1)


async def test_same_name_different_files_do_not_overwrite(cfg, manifest):
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0001.JPG": blob(100, 3), "/DCIM/101APPLE/IMG_0001.JPG": blob(200, 5)})
    # force the same layout slot: both created the same minute
    async def same_time(roots, ignore):
        for path, data in dev.files.items():
            yield RemoteFile(path, len(data), dev.t0, dev.t0)
    dev.list_files = same_time
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    sizes = sorted(p.stat().st_size for p in cfg.backup_root.rglob("IMG_0001*.JPG"))
    assert sizes == [100, 200]


async def test_adopt_skips_files_already_on_ssd(cfg, manifest, tmp_path):
    existing = tmp_path / "old_backup"
    existing.mkdir()
    (existing / "IMG_0009.MOV").write_bytes(blob(500))
    (existing / "._IMG_0009.MOV").write_bytes(b"x" * 4096)  # macOS junk, ignored
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0009.MOV": blob(500), "/DCIM/100APPLE/IMG_0010.MOV": blob(700)})
    await scan(dev, manifest, cfg)
    matched, examined = adopt(manifest, existing, cfg)
    assert (matched, examined) == (1, 1)
    done, _ = await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert done == 1  # only IMG_0010 copied
    assert not list(cfg.backup_root.rglob("IMG_0009.MOV"))


async def test_stale_copying_rows_restart_as_pending(cfg):
    m = Manifest(cfg.backup_root)
    m.upsert_seen("k|1|1", "/DCIM/x", "x", 1, 1.0, 1.0)
    m.set_status("k|1|1", "copying")
    m.close()
    m2 = Manifest(cfg.backup_root)  # simulates restart after a crash
    assert [r.key for r in m2.pending()] == ["k|1|1"]
    m2.close()


async def test_consolidate_copies_adopted_and_leaves_old_folder(cfg, manifest, tmp_path):
    from photovault.copier import consolidate
    existing = tmp_path / "old_backup"
    existing.mkdir()
    data = blob(2_500_000)
    (existing / "IMG_0009.MOV").write_bytes(data)
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0009.MOV": data})
    await scan(dev, manifest, cfg)
    adopt(manifest, existing, cfg)
    done, failed = consolidate(manifest, cfg, progress=lambda *_: None)
    assert (done, failed) == (1, 0)
    copied = next(cfg.backup_root.rglob("IMG_0009.MOV"))
    assert copied.read_bytes() == data
    assert (existing / "IMG_0009.MOV").exists()  # old folder untouched
    assert manifest.counts()["done"][0] == 1


async def test_consolidate_missing_source_stays_adopted(cfg, manifest, tmp_path):
    from photovault.copier import consolidate
    existing = tmp_path / "old_backup"
    existing.mkdir()
    (existing / "A.MOV").write_bytes(blob(10))
    dev = FakeDevice({"/DCIM/100APPLE/A.MOV": blob(10)})
    await scan(dev, manifest, cfg)
    adopt(manifest, existing, cfg)
    (existing / "A.MOV").unlink()
    done, failed = consolidate(manifest, cfg, progress=lambda *_: None)
    assert (done, failed) == (0, 1)
    assert manifest.counts()["adopted"][0] == 1


async def test_limit_copies_only_n(cfg, manifest):
    dev = FakeDevice({f"/DCIM/100APPLE/IMG_{i}.JPG": blob(100 + i) for i in range(5)})
    await scan(dev, manifest, cfg)
    done, _ = await run_copy(dev, manifest, cfg, progress=lambda *_: None, limit=2)
    assert done == 2 and len(manifest.pending()) == 3
