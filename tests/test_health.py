import asyncio
import time

import pytest
from fastapi.testclient import TestClient

from photovault.config import Config, save_config
from photovault.copier import Cancelled
from photovault.manifest import Manifest
from photovault.server.app import create_app
from photovault.server.jobs import BackupJob
from photovault.verify import VerifyReport, repair_problems, verify_ssd
from test_copier import FakeDevice, blob

FILES = {f"/DCIM/100APPLE/IMG_{i:04d}.JPG": blob(200_000 + i, i + 3) for i in range(6)}


async def backed_up(tmp_path, monkeypatch, files=None):
    cfg = Config(ssd_root=tmp_path, backup_dir="V", free_space_margin_gb=0, chunk_mb=1)
    cfg_path = tmp_path / "config.toml"
    save_config(cfg, cfg_path)
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(cfg_path))
    dev = FakeDevice(dict(files or FILES))
    job = BackupJob(lambda: dev, cfg, None, False)
    await job.run()
    assert job.snapshot()["state"] == "finished"
    return cfg, dev


def wait_done(c, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        s = c.get("/api/health/state").json()
        if s["job"]["state"] != "running":
            return s
        time.sleep(0.05)
    raise AssertionError("timed out")


async def test_deep_check_finds_damage_and_repair_restores_from_phone(tmp_path, monkeypatch):
    cfg, dev = await backed_up(tmp_path, monkeypatch)
    files = sorted(cfg.backup_root.rglob("IMG_*.JPG"))
    files[0].unlink()                                                     # lost
    data = bytearray(files[1].read_bytes()); data[100] ^= 0xFF; files[1].write_bytes(bytes(data))   # bit rot
    with TestClient(create_app("tk", device_factory=lambda: dev), headers={"x-photovault-token": "tk"}) as c:
        assert c.post("/api/health/start", json={"kind": "deep"}).json()["state"] == "running"
        s = wait_done(c)
        deep = s["last"]["deep"]
        assert s["job"]["state"] == "done" and deep["ok"] is False and deep["checked"] == 6
        assert deep["missing"]["count"] == 1 and deep["corrupt"]["count"] == 1 and not deep["wrong_size"]["count"]
        res = c.post("/api/health/repair").json()
        assert res == {"queued": 2, "legacy_skipped": 0}
        assert c.get("/api/health/state").json()["last"]["deep"] is None           # old result cleared
        assert list((cfg.backup_root / ".photovault" / "quarantine").rglob("*.JPG"))  # damaged file kept aside
    job = BackupJob(lambda: dev, cfg, None, False)                        # the next backup restores both
    await job.run()
    assert job.snapshot()["done"]["files"] == 2
    m = Manifest(cfg.backup_root)
    assert verify_ssd(m, cfg, deep=True, progress=lambda *_: None).ok
    m.close()


async def test_sync_check_reports_new_phone_photos(tmp_path, monkeypatch):
    cfg, dev = await backed_up(tmp_path, monkeypatch)
    with TestClient(create_app("tk", device_factory=lambda: dev), headers={"x-photovault-token": "tk"}) as c:
        c.post("/api/health/start", json={"kind": "sync"})
        sync = wait_done(c)["last"]["sync"]
        assert sync["in_sync"] is True and sync["on_phone"] == 6 and sync["backed_up"] == 6
        dev.files["/DCIM/100APPLE/IMG_NEW.JPG"] = blob(5000)               # taken after the backup
        c.post("/api/health/start", json={"kind": "sync"})
        sync = wait_done(c)["last"]["sync"]
        assert sync["in_sync"] is False and sync["waiting"] == 1 and sync["waiting_sample"] == ["/DCIM/100APPLE/IMG_NEW.JPG"]


async def test_sync_check_explains_a_missing_phone(tmp_path, monkeypatch):
    cfg, dev = await backed_up(tmp_path, monkeypatch)

    class NoPhone(FakeDevice):
        async def reconnect(self):
            raise ConnectionRefusedError("usbmuxd socket")

    with TestClient(create_app("tk", device_factory=lambda: NoPhone({})), headers={"x-photovault-token": "tk"}) as c:
        c.post("/api/health/start", json={"kind": "sync"})
        s = wait_done(c)["job"]
        assert s["state"] == "error" and "Apple" in s["error"]


async def test_checks_refuse_while_backing_up_or_without_a_backup(tmp_path, monkeypatch):
    cfg = Config(ssd_root=tmp_path, backup_dir="V")
    save_config(cfg, tmp_path / "c.toml")
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(tmp_path / "c.toml"))
    with TestClient(create_app("tk", device_factory=lambda: FakeDevice({})), headers={"x-photovault-token": "tk"}) as c:
        assert c.post("/api/health/start", json={"kind": "deep"}).status_code == 400    # nothing backed up
        assert c.post("/api/health/start", json={"kind": "bogus"}).status_code == 422
        assert c.post("/api/health/cancel").status_code == 409
        assert c.post("/api/health/repair").status_code == 409                          # no problems recorded


async def test_verify_can_be_stopped(tmp_path, monkeypatch):
    cfg, dev = await backed_up(tmp_path, monkeypatch)
    m = Manifest(cfg.backup_root)
    with pytest.raises(Cancelled):
        verify_ssd(m, cfg, deep=True, progress=lambda *_: None, should_stop=lambda: True)
    m.close()


async def test_old_folder_files_are_reported_not_requeued(tmp_path, monkeypatch):
    cfg, dev = await backed_up(tmp_path, monkeypatch)
    old = tmp_path / "old"; old.mkdir()
    (old / "x.MP4").write_bytes(b"x" * 20)
    from photovault.legacy import import_old
    m = Manifest(cfg.backup_root)
    import_old(m, cfg, old, progress=lambda *_: None)
    dest = m.db.execute("SELECT dest FROM items WHERE key LIKE 'legacy|%'").fetchone()[0]
    (cfg.backup_root / dest).unlink()
    rep = VerifyReport(missing=[dest])
    assert repair_problems(m, cfg, rep) == {"queued": 0, "legacy_skipped": 1}
    assert m.db.execute("SELECT status FROM items WHERE key LIKE 'legacy|%'").fetchone()[0] == "done"
    m.close()
