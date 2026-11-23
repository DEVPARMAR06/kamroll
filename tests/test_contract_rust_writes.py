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

pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine-rs)")
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
