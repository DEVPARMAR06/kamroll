"""A whole backup, run by the Python engine and by the Rust engine from the same stand-in phone folder,
must produce the same manifest rows and the same files on the SSD, and a second run must find nothing new."""
import hashlib
import io
import json
import os
import posixpath
import shutil
import sqlite3
import struct
import time
from datetime import datetime, timezone
from pathlib import Path

import pytest
import requests
from fastapi.testclient import TestClient
from PIL import Image

from photovault.config import Config, save_config
from photovault.device import RemoteFile, ignored
from photovault.server.app import create_app
from test_contract_rust import EXE, TOKEN, RustEngine

pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine-rs)")
H = {"x-photovault-token": TOKEN}


class DirFake:
    """The Python engine's view of the same folder the Rust engine reads through PHOTOVAULT_FAKE_DEVICE."""

    def __init__(self, base: Path):
        self.base = base

    async def reconnect(self):
        pass

    def _local(self, remote):
        return self.base / remote.lstrip("/")

    async def list_files(self, roots, ignore):
        for root in roots:
            for folder in sorted(p.name for p in self._local(root).iterdir()):
                if ignored(folder, ignore):
                    continue
                fpath = posixpath.join(root, folder)
                lp = self._local(fpath)
                entries = [(n, posixpath.join(fpath, n)) for n in sorted(p.name for p in lp.iterdir())] if lp.is_dir() else [(folder, fpath)]
                for n, p in entries:
                    if ignored(n, ignore) or not self._local(p).is_file():
                        continue
                    st = self._local(p).stat()
                    t = datetime.fromtimestamp(st.st_mtime)
                    yield RemoteFile(p, st.st_size, t, t)

    async def read_chunks(self, path, offset, chunk):
        with open(self._local(path), "rb") as f:
            f.seek(offset)
            while data := f.read(chunk):
                yield data

    async def read_range(self, path, offset, n):
        with open(self._local(path), "rb") as f:
            f.seek(offset)
            return f.read(n)


def exif_jpeg(color, taken=None, gps=None, make="Apple", size=(64, 48)):
    ex = Image.Exif()
    if make:
        ex[271], ex[272] = make, "iPhone 15 Pro"
    if taken:
        ex.get_ifd(0x8769)[36867] = taken
    if gps:
        g = ex.get_ifd(0x8825)
        g[1], g[2], g[3], g[4] = "N", (gps[0], 30.0, 0.0), "E", (gps[1], 15.0, 0.0)
    buf = io.BytesIO()
    Image.new("RGB", size, color).save(buf, "JPEG", exif=ex)
    return buf.getvalue()


def mov(utc: datetime) -> bytes:
    secs = int(utc.replace(tzinfo=timezone.utc).timestamp()) + 2082844800
    mvhd = b"mvhd" + b"\x00\0\0\0" + struct.pack(">I", secs) + b"\0" * 20
    moov = struct.pack(">I", 8 + len(mvhd)) + b"moov" + struct.pack(">I", 8 + len(mvhd)) + mvhd
    return struct.pack(">I", 16) + b"ftypqt  " + b"\0" * 0 + moov + b"\0" * 40


def stand_in_phone(base: Path) -> None:
    files = {
        "DCIM/100APPLE/IMG_0001.JPG": (exif_jpeg("red", "2025:03:01 09:00:00", gps=(19, 73)), 1_700_000_000),
        "DCIM/100APPLE/IMG_0001.AAE": (b"<plist/>", 1_700_000_100),
        "DCIM/100APPLE/IMG_0002.JPG": (exif_jpeg("green", "2024:12:25 18:30:00", size=(70, 50)), 1_700_000_200),
        "DCIM/100APPLE/IMG_0003.PNG": (b"\x89PNG not really", 1_730_000_000),
        "DCIM/100APPLE/IMG-20240815-WA0017.jpg": (exif_jpeg("blue", make=None, size=(72, 40)), 1_723_660_200),
        "DCIM/100APPLE/IMG_0005.MOV": (mov(datetime(2025, 6, 1, 12, 0, 0)), 1_748_000_000),
        "DCIM/100APPLE/notes.txt": (b"just text", 1_740_000_000),
        "DCIM/100APPLE/._ignored": (b"x", 1_700_000_000),
        "DCIM/101APPLE/IMG_0001.JPG": (exif_jpeg("orange", "2025:03:01 09:00:00", size=(66, 50)), 1_700_000_300),  # same name, other folder
        "DCIM/loose.JPG": (exif_jpeg("pink", make=None, size=(50, 50)), 1_690_000_000),
    }
    for rel, (data, mtime) in files.items():
        p = base / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        os.utime(p, (mtime, mtime))


def snapshot(cfg: Config):
    root = cfg.backup_root
    con = sqlite3.connect(root / ".photovault" / "manifest.db")
    rows = [(k, n, s, st, d, sha, t, src, None if lat is None else round(lat, 5), None if lon is None else round(lon, 5), e)
            for k, n, s, st, d, sha, t, src, lat, lon, e in con.execute(
                "SELECT key, name, size, status, dest, sha256, taken, date_src, lat, lon, error FROM items ORDER BY key")]
    con.close()
    files = {}
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root).as_posix()
        if p.is_file() and not rel.startswith(".photovault/manifest.db"):
            files[rel] = hashlib.sha256(p.read_bytes()).hexdigest()
    return rows, files


def settle(get_state):
    for _ in range(600):
        s = get_state()
        if s["state"] in ("finished", "error", "cancelled"):
            return s
        time.sleep(0.05)
    raise AssertionError("the backup did not finish")


def brief(s):
    r = s.get("result") or {}
    return {"state": s["state"], "done": s["done"]["files"], "failed": s["failed"], "plan": s["plan"]["files"],
            "copied": r.get("copied"), "r_failed": r.get("failed"), "checked": r.get("checked"), "ssd_ok": r.get("ssd_ok"),
            "missing": r.get("missing"), "wrong_size": r.get("wrong_size")}


@pytest.fixture
def setup(tmp_path, monkeypatch):
    ssd = tmp_path / "ssd"
    ssd.mkdir()
    cfg = Config(ssd_root=ssd, backup_dir="V", free_space_margin_gb=0)
    path = tmp_path / "config.toml"
    save_config(cfg, path)
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(path))
    phone = tmp_path / "phone"
    stand_in_phone(phone)
    return cfg, phone, path, monkeypatch


def test_python_and_rust_back_up_the_same_way(setup):
    cfg, phone, cfg_path, monkeypatch = setup
    with TestClient(create_app(TOKEN, device_factory=lambda: DirFake(phone)), headers=H) as py:
        assert py.post("/api/backup/start", json={}).status_code == 200
        py_first = brief(settle(lambda: py.get("/api/backup").json()))
        py.post("/api/backup/start", json={})
        py_second = brief(settle(lambda: py.get("/api/backup").json()))
    py_rows, py_files = snapshot(cfg)

    shutil.rmtree(cfg.backup_root)
    monkeypatch.setenv("PHOTOVAULT_FAKE_DEVICE", str(phone))
    rs = RustEngine(cfg_path)
    try:
        def state():
            return requests.get(rs.url("/api/backup"), headers=H, timeout=10).json()

        assert requests.post(rs.url("/api/backup/start"), json={}, headers=H, timeout=10).status_code == 200
        rs_first = brief(settle(state))
        assert rs.get("/api/device").json()["connected"] is True
        requests.post(rs.url("/api/backup/start"), json={}, headers=H, timeout=10)
        rs_second = brief(settle(state))
        events = requests.get(rs.url("/api/backup/events"), headers=H, stream=True, timeout=10)
        line = next(events.iter_lines(decode_unicode=True))
        events.close()
        assert json.loads(line[len("data: "):])["state"] == "finished"
    finally:
        rs.close()
    rs_rows, rs_files = snapshot(cfg)

    assert py_first["state"] == "finished" and py_first["copied"] == 9      # the ._ file is ignored
    assert rs_first == py_first and rs_second == py_second
    assert py_second["plan"] == 0
    assert rs_rows == py_rows
    assert rs_files == py_files
    assert not any(p.is_file() for p in (cfg.backup_root / ".photovault" / "incoming").rglob("*"))


def test_a_stopped_backup_resumes_without_recopying(setup):
    cfg, phone, cfg_path, monkeypatch = setup
    big = phone / "DCIM" / "100APPLE" / "BIG.MOV"
    big.write_bytes(os.urandom(6 << 20))
    monkeypatch.setenv("PHOTOVAULT_FAKE_DEVICE", str(phone))
    rs = RustEngine(cfg_path)
    try:
        def post(path, body=None):
            return requests.post(rs.url(path), json=body or {}, headers=H, timeout=10)

        def state():
            return requests.get(rs.url("/api/backup"), headers=H, timeout=10).json()

        assert post("/api/backup/pause").status_code == 409              # nothing is running
        post("/api/backup/start")
        post("/api/backup/cancel")
        s = settle(state)
        assert s["state"] in ("cancelled", "finished")
        post("/api/backup/start")
        s = settle(state)
        assert s["state"] == "finished" and s["result"]["ssd_ok"] and s["result"]["failed"] == 0
    finally:
        rs.close()
    rows, files = snapshot(cfg)
    assert sum(1 for r in rows if r[3] == "done") == 10
    assert hashlib.sha256(big.read_bytes()).hexdigest() in files.values()


def test_phone_versus_ssd_comparison_matches(setup):
    cfg, phone, cfg_path, monkeypatch = setup
    with TestClient(create_app(TOKEN, device_factory=lambda: DirFake(phone)), headers=H) as py:
        py.post("/api/backup/start", json={})
        settle(lambda: py.get("/api/backup").json())
        (phone / "DCIM" / "100APPLE" / "NEW_0009.JPG").write_bytes(exif_jpeg("cyan", "2025:07:01 10:00:00", size=(60, 44)))
        (phone / "DCIM" / "100APPLE" / "IMG_0002.JPG").unlink()                       # deleted from the phone after backup
        (cfg.backup_root / "2025" / "2025-06").mkdir(parents=True, exist_ok=True)
        victim = next(p for p in cfg.backup_root.rglob("IMG_0005.MOV"))
        victim.write_bytes(b"short")                                                    # damaged on the SSD

        def run(get, post):
            assert post("/api/health/start", {"kind": "sync"}).status_code == 200
            for _ in range(400):
                if get("/api/health/state").json()["job"]["state"] != "running":
                    break
                time.sleep(0.05)
            st = get("/api/health/state").json()
            assert st["job"]["state"] == "done", st
            r = dict(st["last"]["sync"])
            r.pop("at")
            return r

        py_result = run(lambda p: py.get(p), lambda p, b: py.post(p, json=b))
    monkeypatch.setenv("PHOTOVAULT_FAKE_DEVICE", str(phone))
    rs = RustEngine(cfg_path)
    try:
        rs_result = run(lambda p: requests.get(rs.url(p), headers=H, timeout=10), lambda p, b: requests.post(rs.url(p), json=b, headers=H, timeout=10))
    finally:
        rs.close()
    assert py_result["waiting"] == 1 and py_result["only_on_ssd"] == 1 and py_result["wrong_size"]
    assert rs_result == py_result
