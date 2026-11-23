"""The Rust engine must answer exactly like the Python engine on the routes it has taken over.

Both engines are pointed at the same settings and the same manifest, and their JSON is compared.
Skipped when the Rust engine has not been built (`cargo build` in engine-rs).
"""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest
import requests
from fastapi.testclient import TestClient

from photovault.config import load_config, save_config
from photovault.server.app import create_app
from test_review import vault  # noqa: F401  (fixture)

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "engine-rs" / "target" / "debug" / ("photovault-engine.exe" if sys.platform == "win32" else "photovault-engine")
pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine-rs)")
TOKEN = "tk"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class RustEngine:
    def __init__(self, config_path):
        self.port = free_port()
        env = {**os.environ, "PHOTOVAULT_CONFIG": str(config_path)}
        self.proc = subprocess.Popen([str(EXE), "ui", "--port", str(self.port), "--token", TOKEN], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=str(ROOT))
        end = time.time() + 15
        while time.time() < end:
            try:
                if requests.get(self.url("/api/health"), headers={"x-photovault-token": TOKEN}, timeout=1).ok:
                    return
            except requests.RequestException:
                time.sleep(0.1)
        raise AssertionError("the Rust engine did not start")

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

    def get(self, route, **params):
        return requests.get(self.url(route), params=params, headers={"x-photovault-token": TOKEN}, timeout=10)

    def put(self, path, body):
        return requests.put(self.url(path), json=body, headers={"x-photovault-token": TOKEN}, timeout=10)

    def close(self):
        self.proc.terminate()
        self.proc.wait(timeout=10)


@pytest.fixture
def engines(vault):  # noqa: F811
    cfg, keys = vault
    path = os.environ["PHOTOVAULT_CONFIG"]
    with TestClient(create_app(TOKEN), headers={"x-photovault-token": TOKEN}) as py:
        py.post("/api/analysis/start")
        for _ in range(200):
            if py.get("/api/analysis").json()["state"] == "done":
                break
            time.sleep(0.05)
        rs = RustEngine(path)
        try:
            yield py, rs, cfg, keys
        finally:
            rs.close()


def both(py, rs, route, **params):
    a, b = py.get(route, params=params), rs.get(route, **params)
    assert a.status_code == b.status_code, (route, params, a.text, b.text)
    return a.json(), b.json()


def test_the_guards_match(engines):
    py, rs, *_ = engines
    assert requests.get(rs.url("/api/health"), timeout=5).status_code == 401
    assert requests.get(rs.url("/api/health?token=" + TOKEN), timeout=5).json() == py.get("/api/health").json()
    assert requests.get(rs.url("/api/health?token=" + TOKEN), headers={"Host": "evil.example.com"}, timeout=5).status_code == 403
    html = requests.get(rs.url("/"), timeout=5).text
    assert TOKEN in html and "__TOKEN__" not in html
    for asset in ("app.css", "app.js"):
        assert requests.get(rs.url("/static/" + asset), timeout=5).text == py.get("/static/" + asset).text


def test_settings_and_status_match(engines):
    py, rs, *_ = engines
    a, b = both(py, rs, "/api/config")
    assert a == b
    assert both(py, rs, "/api/status")[0] == both(py, rs, "/api/status")[1]


def test_review_summary_items_and_trips_match(engines):
    py, rs, cfg, keys = engines
    assert both(py, rs, "/api/review/summary")[0] == both(py, rs, "/api/review/summary")[1]

    def norm(r):   # the Rust engine only draws JPEG and PNG thumbnails for now
        for i in r["items"]:
            i.pop("has_thumb")
        return r

    tid = py.get("/api/trips").json()["trips"][0]["id"]
    for params in ({}, {"tag": "junk"}, {"tag": "trip"}, {"trip": tid, "order": "time"}, {"q": "IMG_00"}, {"limit": 3, "offset": 2},
                   {"trip": tid, "tag": "trip", "order": "time", "limit": 2, "spread": "true"}, {"q": "100%_"}):
        a, b = both(py, rs, "/api/review/items", **params)
        assert norm(a) == norm(b), params
    a, b = both(py, rs, "/api/trips")
    assert a["home"] == pytest.approx(b["home"]) and a["radius_km"] == b["radius_km"]
    for ta, tb in zip(a["trips"], b["trips"]):
        assert ta == tb
    assert len(a["trips"]) == len(b["trips"]) == 1


@pytest.mark.parametrize("params", [
    {"layout": "{year}/{year}-{month}/{name}"},
    {"layout": "{year}/{month}/{name}", "undated": "Unsorted/{stem}.{ext}", "trip_layout": "Trips/{trip}/{year}-{month}-{day}/{name}"},
    {"layout": "{year}/{oops}/{name}"}, {"layout": "../{name}"}, {"layout": "{year}/folder"},
    {"layout": "{year}/{name}", "undated": "/abs/{name}"}, {"layout": "{year}/{name}", "trip_layout": "Trips/{name}"},
])
def test_layout_previews_match(engines, params):
    py, rs, *_ = engines
    a, b = both(py, rs, "/api/layout-preview", **params)
    assert a == b


def test_folders_and_path_checks_match(engines, tmp_path):
    py, rs, *_ = engines
    (tmp_path / "Photos").mkdir()
    (tmp_path / "Alpha").mkdir()
    (tmp_path / "$hidden").mkdir()
    (tmp_path / "file.txt").write_text("x")
    a, b = both(py, rs, "/api/fs", path=str(tmp_path))
    assert a == b
    assert both(py, rs, "/api/fs", path=str(tmp_path / "missing"))[0] == {"detail": "That folder does not exist."}
    for p in (tmp_path, tmp_path / "NewBackup", tmp_path / "x" / "y"):
        a, b = both(py, rs, "/api/path-check", path=str(p))
        assert a == b, p
    assert not (tmp_path / "NewBackup").exists()
    pv, rv = py.get("/api/volumes").json(), rs.get("/api/volumes").json()
    assert {v["path"] for v in pv} == {v["path"] for v in rv}
    assert set(pv[0]) == set(rv[0])


GOOD = {
    "ssd_root": "E:/", "backup_dir": "PhotoBackup", "remote_roots": ["/DCIM"], "layout": "{year}/{year}-{month}/{name}",
    "undated_layout": "Undated/{name}", "chunk_mb": 4, "max_retries": 6, "ignore": ["._*"], "free_space_margin_gb": 5.0,
    "copy_originals_of_reencoded": True, "ai_enabled": False, "ai_host": "http://127.0.0.1:11434", "ai_model": "",
    "trip_radius_km": 60.0, "home_lat": None, "home_lon": None, "file_trips": False,
    "trip_layout": "Trips/{trip_year}/{trip_dated}/{name}",
}


@pytest.mark.parametrize("patch", [
    {"layout": "{year}/{oops}/{name}"}, {"layout": "../{name}"}, {"chunk_mb": 0}, {"max_retries": 99}, {"backup_dir": "../x"},
    {"remote_roots": ["DCIM"]}, {"ai_host": "http://example.com:11434"}, {"ssd_root": ""}, {"trip_layout": "Trips/{name}"},
    {"trip_radius_km": 1}, {"home_lat": 120},
])
def test_bad_settings_fail_on_the_same_field(engines, patch):
    py, rs, *_ = engines
    a, b = py.put("/api/config", json={**GOOD, **patch}), rs.put("/api/config", {**GOOD, **patch})
    assert a.status_code == b.status_code == 422
    assert {d["loc"][-1] for d in a.json()["detail"]} == {d["loc"][-1] for d in b.json()["detail"]}


def test_both_engines_write_the_same_settings_file(vault, tmp_path):  # noqa: F811
    body = {**GOOD, "ssd_root": "D:/Photos", "home_lat": 19.07, "home_lon": 72.88, "file_trips": True,
            "ignore": ["._*", "Thumbs.db"]}
    py_path, rs_path = tmp_path / "py.toml", tmp_path / "rs.toml"
    os.environ["PHOTOVAULT_CONFIG"] = str(py_path)
    with TestClient(create_app(TOKEN), headers={"x-photovault-token": TOKEN}) as py:
        assert py.put("/api/config", json=body).status_code == 200
    rs = RustEngine(rs_path)
    try:
        assert rs.put("/api/config", body).status_code == 200
        assert rs.get("/api/config").json()["config"] == body | {"ssd_root": "D:/Photos"}
    finally:
        rs.close()
    assert py_path.read_text(encoding="utf-8") == rs_path.read_text(encoding="utf-8")
    # and each can read what the other wrote
    assert load_config(rs_path) == load_config(py_path)
    save_config(load_config(py_path), tmp_path / "again.toml")
    assert (tmp_path / "again.toml").read_text(encoding="utf-8") == py_path.read_text(encoding="utf-8")


def test_parts_not_ported_say_so(engines):
    py, rs, *_ = engines
    r = requests.post(rs.url("/api/ai/start"), json={"kind": "trips"}, headers={"x-photovault-token": TOKEN}, timeout=5)
    assert r.status_code == 501 and "not been ported" in r.json()["detail"]
    assert rs.get("/api/ollama").json()["reachable"] is False
