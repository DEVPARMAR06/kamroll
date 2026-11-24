"""The Rust engine must change the vault exactly like the Python engine does.

The same scripted session (tags, trips, a deep check and repair, filing into trip folders and back) is run
against the Python engine and then, from an identical copy of the vault, against the Rust engine. Every
answer and the final state of the manifest and of the files on disk must match.
"""
import json
import os
import re
import shutil
import sqlite3
import time

import pytest
import requests
from fastapi.testclient import TestClient

from photovault.config import load_config, save_config
from photovault.server.app import create_app
from test_contract_rust import EXE, TOKEN, RustEngine
from test_review import vault  # noqa: F401  (fixture)

pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine)")
H = {"x-photovault-token": TOKEN}
VOLATILE = {"at", "started_at", "analysis_at"}


class PyApi:
    def __init__(self):
        self.c = TestClient(create_app(TOKEN), headers=H)

    def __enter__(self):
        self.c.__enter__()
        return self

    def __exit__(self, *a):
        self.c.__exit__(*a)

    def call(self, method, path, body=None):
        r = self.c.request(method, path, json=body)
        return r.status_code, r.json()


class RsApi:
    def __init__(self, config_path):
        self.e = RustEngine(config_path)

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.e.close()

    def call(self, method, path, body=None):
        r = requests.request(method, self.e.url(path), json=body, headers=H, timeout=20)
        return r.status_code, r.json()


def scrub(x):
    if isinstance(x, dict):
        if isinstance(x.get("detail"), list):      # validation errors: same field and kind; wording extras are not compared
            return {"detail": [{"loc": d["loc"], "type": d["type"]} for d in x["detail"]]}
        return {k: scrub(v) for k, v in x.items() if k not in VOLATILE}
    if isinstance(x, (list, tuple)):
        return [scrub(v) for v in x]
    if isinstance(x, str):
        return re.sub(r"c-[0-9a-f]{8,}", "c-NEW", x)
    return x


def wait(api, path, field="state"):
    for _ in range(400):
        s = api.call("GET", path)[1]
        s = s.get("job", s)
        if s[field] not in ("running", "idle"):
            return
        time.sleep(0.05)
    raise AssertionError(f"{path} did not finish")


def snapshot(cfg):
    con = sqlite3.connect(cfg.backup_root / ".photovault" / "manifest.db")
    out = {}
    for name, sql in {
        "tags": "SELECT key, tag, conf, reasons, user_tag, trip_id FROM tags",
        "trips": "SELECT * FROM trips",
        "trip_edits": "SELECT * FROM trip_edits",
        "trip_custom": "SELECT * FROM trip_custom",
        "trip_deleted": "SELECT * FROM trip_deleted",
        "items": "SELECT key, status, dest, sha256, size FROM items",
        "filing_origin": "SELECT * FROM filing_origin",
        "moves": "SELECT * FROM moves",
        "health": "SELECT k FROM meta WHERE k LIKE 'health%'",
    }.items():
        rows = [json.loads(re.sub(r"c-[0-9a-f]{8,}", "c-NEW", json.dumps(r))) for r in con.execute(sql)]
        out[name] = sorted(rows, key=lambda r: json.dumps(r))
    con.close()
    files = []
    for p in cfg.backup_root.rglob("*"):
        rel = p.relative_to(cfg.backup_root).as_posix()
        if p.is_file() and not rel.startswith(".photovault/") or rel.startswith(".photovault/quarantine/") and p.is_file():
            files.append(re.sub(r"quarantine/\d{8}-\d{6}/", "quarantine/STAMP/", rel))
    out["files"] = sorted(files)
    return out


def script(api, cfg, keys):
    log = []

    def step(method, path, body=None):
        log.append((method, scrub(path), scrub(api.call(method, path, body))))
        return api.call  # unused

    def last():
        return log[-1][2][1]

    trip = api.call("GET", "/api/trips")[1]["trips"][0]["id"]
    step("POST", "/api/review/tag", {"keys": [keys["cam"], keys["wa"]], "tag": "junk"})
    step("POST", "/api/review/tag", {"keys": [keys["orig"]], "tag": None})
    step("POST", "/api/review/tag", {"keys": [keys["orig"]], "tag": "nonsense"})
    step("POST", "/api/review/tag", {"keys": [], "tag": "junk"})
    step("PUT", f"/api/trips/{trip}", {"name": "  Mewar holiday "})
    step("PUT", "/api/trips/nope", {"name": "x"})
    step("POST", "/api/trips", {"name": "Train", "start": "2025-03-01", "end": "2025-03-02", "keys": [keys["black"]]})
    step("POST", "/api/trips", {"name": "Bad", "start": "2025-03-01", "end": "2025-02-01"})
    step("POST", "/api/trips", {"name": "Bad", "start": "2025-13-01", "end": "2025-14-01"})
    step("POST", "/api/trips", {"name": "", "start": "2025-03-01", "end": "2025-03-02"})
    custom = next(t["id"] for t in api.call("GET", "/api/trips")[1]["trips"] if t["id"].startswith("c-"))
    step("POST", f"/api/trips/{custom}/items", {"keys": [keys["orig"], "no-such-key"]})
    step("POST", "/api/trips/nope/items", {"keys": [keys["orig"]]})
    step("POST", f"/api/trips/{custom}/remove", {"keys": [keys["orig"]]})
    step("POST", f"/api/trips/{trip}/remove", {"keys": [keys["t0"]]})
    step("POST", f"/api/trips/{trip}/items", {"keys": [keys["t0"]]})
    step("GET", "/api/trips")
    step("GET", "/api/review/summary")

    # filing into trip folders and back
    save_config(load_config(os.environ["PHOTOVAULT_CONFIG"]).__class__(**{**load_config(os.environ["PHOTOVAULT_CONFIG"]).__dict__, "file_trips": True}),
                os.environ["PHOTOVAULT_CONFIG"])
    step("GET", "/api/organize/preview")
    step("POST", "/api/organize/start")
    wait(api, "/api/organize")
    step("GET", "/api/organize")
    step("GET", "/api/organize/preview")
    step("POST", "/api/organize/cancel")
    step("PUT", f"/api/trips/{trip}", {"name": "Mewar: Udaipur?"})
    step("GET", "/api/organize/preview")
    step("POST", "/api/organize/start")
    wait(api, "/api/organize")
    step("GET", "/api/organize")
    step("POST", f"/api/trips/{trip}/remove", {"keys": [keys["t1"]]})
    step("POST", "/api/organize/start")
    wait(api, "/api/organize")
    step("GET", "/api/organize")
    step("DELETE", f"/api/trips/{trip}")
    step("DELETE", f"/api/trips/{custom}")
    step("DELETE", "/api/trips/nope")
    step("POST", "/api/organize/start")
    wait(api, "/api/organize")
    step("GET", "/api/organize")

    # health: damage a file and lose another, check deeply, repair
    root = cfg.backup_root
    con = sqlite3.connect(root / ".photovault" / "manifest.db")
    d_cam = con.execute("SELECT dest FROM items WHERE key=?", (keys["cam"],)).fetchone()[0]
    d_wa = con.execute("SELECT dest FROM items WHERE key=?", (keys["wa"],)).fetchone()[0]
    con.close()
    p = root / d_cam
    p.write_bytes(bytes(b ^ 0xFF for b in p.read_bytes()))
    (root / d_wa).unlink()
    step("GET", "/api/health/state")
    step("POST", "/api/health/cancel")
    step("POST", "/api/health/repair")
    step("POST", "/api/health/start", {"kind": "bogus"})
    step("POST", "/api/health/start", {"kind": "deep"})
    wait(api, "/api/health/state")
    step("GET", "/api/health/state")
    step("POST", "/api/health/repair")
    step("GET", "/api/health/state")
    step("GET", "/api/status")
    return log


def test_python_and_rust_make_the_same_changes(vault):  # noqa: F811
    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    with PyApi() as py:                                   # analyse once so both start from the same tags
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
    time.sleep(0.2)
    pristine = cfg.backup_root.parent / "pristine"
    shutil.copytree(cfg.backup_root, pristine)
    pristine_cfg = open(cfg_path, "rb").read()

    with PyApi() as py:
        py_log = script(py, cfg, keys)
    py_state = snapshot(cfg)

    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    open(cfg_path, "wb").write(pristine_cfg)
    with RsApi(cfg_path) as rs:
        rs_log = script(rs, cfg, keys)
    rs_state = snapshot(cfg)

    assert len(py_log) == len(rs_log)
    for a, b in zip(py_log, rs_log):
        assert a == b, (a[:2], a[2], b[2])
    for table in py_state:
        assert py_state[table] == rs_state[table], table


def analysis_state(cfg):
    con = sqlite3.connect(cfg.backup_root / ".photovault" / "manifest.db")
    out = {
        "tags": sorted(con.execute("SELECT key, tag, conf, reasons, user_tag, trip_id FROM tags").fetchall()),
        "trips": sorted(con.execute("SELECT id, name, auto_name, start, end, days, items, ROUND(max_km, 6), ROUND(lat, 6), ROUND(lon, 6), places FROM trips").fetchall()),
        "facts": sorted(con.execute("SELECT key, width, height, make, model, software, luma < 8 FROM facts").fetchall()),
        "home": [round(x, 6) for x in json.loads(con.execute("SELECT v FROM meta WHERE k='home'").fetchone()[0])],
    }
    con.close()
    return out


def test_python_and_rust_analyse_the_same_way(vault):  # noqa: F811
    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    pristine = cfg.backup_root.parent / "pristine"
    shutil.copytree(cfg.backup_root, pristine)
    with PyApi() as py:
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
        py_summary = py.call("GET", "/api/analysis")[1]
    py_state = analysis_state(cfg)
    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    with RsApi(cfg_path) as rs:
        assert rs.call("POST", "/api/analysis/start")[0] == 200
        wait(rs, "/api/analysis")
        rs_summary = rs.call("GET", "/api/analysis")[1]
        assert rs.call("POST", "/api/analysis/start")[0] in (200, 409)
    assert rs_summary["state"] == "done", rs_summary
    assert rs_summary["summary"] == py_summary["summary"]
    rs_state = analysis_state(cfg)
    for table in py_state:
        assert py_state[table] == rs_state[table], table


def test_thumbnails_match_in_size_and_errors(vault):  # noqa: F811
    import io

    from PIL import Image

    cfg, keys = vault
    with PyApi() as py, RsApi(os.environ["PHOTOVAULT_CONFIG"]) as rs:
        def get(api, path):
            if isinstance(api, PyApi):
                r = api.c.get(path)
            else:
                r = requests.get(api.e.url(path), headers=H, timeout=20)
            return r

        for k in ("cam", "wa", "t1"):
            a, b = get(py, f"/api/thumb?key={keys[k]}&size=96"), get(rs, f"/api/thumb?key={keys[k]}&size=96")
            assert a.status_code == b.status_code == 200 and b.headers["content-type"] == "image/jpeg"
            assert Image.open(io.BytesIO(a.content)).size == Image.open(io.BytesIO(b.content)).size
        for path in ("/api/thumb?key=nope", f"/api/thumb?key={keys['mov']}", f"/api/thumb?key={keys['aae']}"):
            a, b = get(py, path), get(rs, path)
            assert (a.status_code, a.json()) == (b.status_code, b.json()), path


def test_reanalysis_keeps_names_and_pulls_new_photos_into_existing_trips(vault):  # noqa: F811
    from datetime import datetime

    from photovault.manifest import Manifest
    from test_review import UDAIPUR, add, jpeg

    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    with PyApi() as py:
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
    pristine = cfg.backup_root.parent / "pristine"
    shutil.copytree(cfg.backup_root, pristine)

    def scenario(api):
        trip = api.call("GET", "/api/trips")[1]["trips"][0]["id"]
        api.call("PUT", f"/api/trips/{trip}", {"name": "Mewar holiday"})
        made = api.call("POST", "/api/trips", {"name": "Pune train", "start": "2025-03-01", "end": "2025-03-02"})[1]["id"]
        api.call("POST", f"/api/trips/{made}/remove", {"keys": [keys["cam"]]})            # the person took this one out
        m = Manifest(cfg.backup_root)                                                      # more photos get backed up later
        new = {
            "early": add(m, cfg.backup_root, "IMG_7001.JPG", jpeg("cyan", size=(61, 40), make="Apple"), datetime(2025, 12, 24, 10), *UDAIPUR),
            "train": add(m, cfg.backup_root, "IMG_7002.JPG", jpeg("pink", size=(62, 40), make="Apple"), datetime(2025, 3, 2, 12)),
            "elsewhere": add(m, cfg.backup_root, "IMG_7003.JPG", jpeg("gold", size=(63, 40), make="Apple"), datetime(2025, 3, 20, 12)),
        }
        m.close()
        api.call("POST", "/api/analysis/start")
        wait(api, "/api/analysis")
        return new, made, api.call("GET", "/api/trips")[1]["trips"]

    with PyApi() as py:
        py_new, py_made, py_trips = scenario(py)
    py_state = analysis_state(cfg)
    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    with RsApi(cfg_path) as rs:
        rs_new, rs_made, rs_trips = scenario(rs)
    rs_state = analysis_state(cfg)

    def trip_of(state, key):
        return next((t[5] for t in state["tags"] if t[0] == key), "missing")

    named = [t for t in py_trips if t["name"] == "Mewar holiday"]
    assert len(named) == 1 and named[0]["start"] == "2025-12-24"                    # same trip, new first day, name kept
    assert trip_of(py_state, py_new["early"]) == named[0]["id"]
    assert trip_of(py_state, py_new["train"]) is not None and trip_of(py_state, py_new["train"]).startswith("c-")
    assert trip_of(py_state, py_new["elsewhere"]) is None
    assert trip_of(py_state, keys["cam"]) is None                                     # taken out by the person: stays out
    norm = lambda s: json.loads(re.sub(r"c-[0-9a-f]{8,}", "c-NEW", json.dumps(s)))  # noqa: E731
    for table in py_state:
        a, b = norm(py_state[table]), norm(rs_state[table])
        assert a == b, (table, [x for x in a if x not in b][:2], [x for x in b if x not in a][:2])
    assert norm(rs_trips) == norm(py_trips)
