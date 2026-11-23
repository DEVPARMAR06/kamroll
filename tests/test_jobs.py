import asyncio

import pytest
from fastapi.testclient import TestClient

from photovault.config import Config, save_config
from photovault.server.app import create_app
from photovault.server.jobs import BackupJob
from test_copier import FakeDevice, blob


def make_job(tmp_path, dev, **kw):
    cfg = Config(ssd_root=tmp_path, chunk_mb=1, max_retries=3, free_space_margin_gb=0)
    for k, v in kw.items():
        setattr(cfg, k, v)
    return BackupJob(lambda: dev, cfg, kw.get("limit"), False), cfg


async def wait_for(pred, timeout=8.0):
    end = asyncio.get_event_loop().time() + timeout
    while not pred():
        if asyncio.get_event_loop().time() > end:
            raise AssertionError("timed out waiting")
        await asyncio.sleep(0.02)


FILES = {f"/DCIM/100APPLE/IMG_{i:04d}.JPG": blob(1_500_000 + i, i + 2) for i in range(4)}


async def test_job_runs_to_completion_with_consistent_progress(tmp_path):
    dev = FakeDevice(dict(FILES))
    job, cfg = make_job(tmp_path, dev)
    await job.run()
    snap = job.snapshot()
    total = sum(len(v) for v in FILES.values())
    assert snap["state"] == "finished" and snap["failed"] == 0
    assert snap["done"] == {"files": 4, "bytes": total} and snap["plan"] == {"files": 4, "bytes": total}
    assert snap["result"]["copied"] == 4 and snap["result"]["ssd_ok"] is True
    assert snap["current"] is None and snap["error"] is None


async def test_cable_drop_is_reported_and_recovered(tmp_path):
    dev = FakeDevice({"/DCIM/100APPLE/BIG.MOV": blob(5_000_000)})
    dev.fail_after_bytes = 2_000_000
    job, _ = make_job(tmp_path, dev)
    await job.run()
    snap = job.snapshot()
    assert snap["state"] == "finished" and snap["retries"] == 1
    assert any(e["level"] == "warn" and "cable unplugged" in e["text"] for e in snap["recent"])
    assert snap["done"]["bytes"] == 5_000_000  # resumed bytes are not counted twice


async def test_pause_stops_progress_and_resume_finishes(tmp_path):
    dev = FakeDevice({"/DCIM/100APPLE/BIG.MOV": blob(8_000_000)})
    dev.delay = 0.05
    job, _ = make_job(tmp_path, dev)
    task = asyncio.create_task(job.run())
    await wait_for(lambda: job.done_bytes > 1_000_000)
    job.pause()
    await asyncio.sleep(0.15)
    frozen = job.done_bytes
    await asyncio.sleep(0.4)
    assert job.snapshot()["state"] == "paused" and job.done_bytes == frozen
    job.resume()
    await asyncio.wait_for(task, 10)
    assert job.snapshot()["state"] == "finished" and job.done_bytes == 8_000_000


async def test_cancel_keeps_partial_and_next_run_resumes(tmp_path):
    dev = FakeDevice({"/DCIM/100APPLE/BIG.MOV": blob(8_000_000)})
    dev.delay = 0.05
    job, cfg = make_job(tmp_path, dev)
    task = asyncio.create_task(job.run())
    await wait_for(lambda: job.done_bytes > 2_000_000)
    job.cancel()
    await asyncio.wait_for(task, 10)
    assert job.snapshot()["state"] == "cancelled"
    assert list(cfg.backup_root.rglob("*.part"))                      # partial kept for resume
    dev.delay = 0
    job2 = BackupJob(lambda: dev, cfg, None, False)
    await job2.run()
    assert job2.snapshot()["state"] == "finished"
    assert next(cfg.backup_root.rglob("BIG.MOV")).read_bytes() == blob(8_000_000)
    assert not list(cfg.backup_root.rglob("*.part"))


async def test_not_enough_space_is_a_plain_error(tmp_path):
    job, _ = make_job(tmp_path, FakeDevice(dict(FILES)), free_space_margin_gb=10_000_000)
    await job.run()
    snap = job.snapshot()
    assert snap["state"] == "error" and "Not enough space" in snap["error"]


async def test_missing_iphone_gives_a_hint(tmp_path):
    class NoPhone(FakeDevice):
        async def reconnect(self):
            raise ConnectionRefusedError("usbmuxd socket")

    job, _ = make_job(tmp_path, NoPhone({}))
    await job.run()
    snap = job.snapshot()
    assert snap["state"] == "error" and "Apple" in snap["error"]


async def test_missing_drive_is_reported(tmp_path):
    job, _ = make_job(tmp_path / "not-there", FakeDevice({}))
    await job.run()
    assert job.snapshot()["state"] == "error" and "drive was not found" in job.snapshot()["error"]


def test_backup_endpoints(tmp_path, monkeypatch):
    cfg_path = tmp_path / "config.toml"
    cfg = Config(ssd_root=tmp_path / "ssd", chunk_mb=1, max_retries=2, free_space_margin_gb=0)
    (tmp_path / "ssd").mkdir()
    save_config(cfg, cfg_path)
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(cfg_path))
    dev = FakeDevice(dict(FILES))
    dev.delay = 0.02
    with TestClient(create_app("tk", device_factory=lambda: dev), headers={"x-photovault-token": "tk"}) as c:
        assert c.get("/api/backup").json()["state"] == "idle"
        assert c.post("/api/backup/pause").status_code == 409                 # nothing to pause
        assert c.post("/api/backup/start", json={"limit": 0}).status_code == 422
        assert c.post("/api/backup/start", json={}).json()["state"] in ("starting", "scanning", "copying")
        assert c.post("/api/backup/start", json={}).status_code == 409        # already running
        import time
        for _ in range(200):
            snap = c.get("/api/backup").json()
            if snap["state"] == "finished":
                break
            time.sleep(0.05)
        assert snap["state"] == "finished" and snap["done"]["files"] == 4


def test_start_without_saved_settings(tmp_path, monkeypatch):
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(tmp_path / "none.toml"))
    with TestClient(create_app("tk", device_factory=lambda: FakeDevice({})), headers={"x-photovault-token": "tk"}) as c:
        r = c.post("/api/backup/start", json={})
        assert r.status_code == 400 and "settings" in r.json()["detail"].lower()
