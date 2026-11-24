"""Smart tags: the Rust engine must talk to Ollama and store results exactly like the Python engine."""
import json
import os
import re
import shutil
import sqlite3
import threading
import time
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
import requests

from photovault.config import load_config, save_config
from test_contract_rust import EXE, TOKEN
from test_contract_rust_writes import H, PyApi, RsApi, scrub, wait
from test_review import vault  # noqa: F401  (fixture)

pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine)")


class FakeOllama(BaseHTTPRequestHandler):
    calls = 0

    def log_message(self, *a):
        pass

    def _send(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._send(200, {"models": [{"name": "llama3:8b", "size": 4.7e9}, {"name": "llava:7b", "size": 4.1e9},
                                    {"name": "nomic-embed-text", "size": 2.7e8}, {"name": "qwen2.5vl:3b", "size": 3.2e9}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        msg = body["messages"][0]
        if "images" in msg:
            FakeOllama.calls += 1
            n = FakeOllama.calls % 3
            if n == 0:
                reply = "Sure! " + json.dumps({"tags": ["Beach", "sunset", "beach", "Food, fun"], "caption": "A  sunset   on the beach.", "kind": "memory"})
            elif n == 1:
                reply = json.dumps({"tags": "dog, park", "caption": "A dog", "kind": "banana"})
            else:
                reply = json.dumps({"tags": [], "caption": "", "kind": "document"})
        else:
            reply = json.dumps({"title": '"Lakes of Mewar."', "summary": "Five warm days by the water."}) if "Udaipur" in msg["content"] else "not json at all"
        self._send(200, {"message": {"role": "assistant", "content": reply}})


@pytest.fixture
def ollama():
    srv = HTTPServer(("127.0.0.1", 0), FakeOllama)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{srv.server_address[1]}"
    srv.shutdown()


def ai_tables(cfg):
    con = sqlite3.connect(cfg.backup_root / ".photovault" / "manifest.db")
    out = {"tags": sorted(con.execute("SELECT key, model, tags, caption, kind FROM ai_tags").fetchall()),
           "trips": sorted(con.execute("SELECT id, model, title, summary FROM ai_trips").fetchall())}
    con.close()
    return out


def script(api, kinds):
    log = []
    for kind, extra in kinds:
        log.append(scrub(api.call("POST", "/api/ai/start", {"kind": kind, **extra})))
        wait(api, "/api/ai")
        log.append(scrub(api.call("GET", "/api/ai")))
    return log


def test_python_and_rust_smart_tags_match(vault, ollama):  # noqa: F811
    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    save_config(replace(load_config(cfg_path), ai_enabled=True, ai_model="llava:7b", ai_host=ollama), cfg_path)
    with PyApi() as py:
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
        assert py.call("GET", "/api/ollama?host=" + ollama)[0] == 200
        py_ollama = py.call("GET", "/api/ollama?host=" + ollama)[1]
        py_bad = py.call("GET", "/api/ollama?host=http://example.com")[1]
    pristine = cfg.backup_root.parent / "pristine"
    shutil.copytree(cfg.backup_root, pristine)
    FakeOllama.calls = 0
    plan = [("trips", {}), ("photos", {"limit": 4}), ("photos", {"limit": 50, "scope": "all"})]
    with PyApi() as py:
        py_log = script(py, plan)
        py_errs = [py.call("POST", "/api/ai/start", {"kind": "nope"})[0], py.call("POST", "/api/ai/cancel")[0]]
    py_state = ai_tables(cfg)
    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    FakeOllama.calls = 0
    with RsApi(cfg_path) as rs:
        rs_ollama = rs.call("GET", "/api/ollama?host=" + ollama)[1]
        rs_bad = rs.call("GET", "/api/ollama?host=http://example.com")[1]
        rs_log = script(rs, plan)
        rs_errs = [rs.call("POST", "/api/ai/start", {"kind": "nope"})[0], rs.call("POST", "/api/ai/cancel")[0]]
    rs_state = ai_tables(cfg)
    assert py_ollama["reachable"] and [m["name"] for m in py_ollama["models"]][0] == "llava:7b"
    assert rs_ollama == py_ollama and rs_bad == py_bad
    assert rs_log == py_log
    assert rs_errs == py_errs == [422, 409]
    assert py_state["trips"] and py_state["tags"]
    assert rs_state == py_state


def test_failing_model_stops_after_three_the_same_way(vault, ollama):  # noqa: F811
    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    save_config(replace(load_config(cfg_path), ai_enabled=True, ai_model="llava:7b", ai_host="http://127.0.0.1:9"), cfg_path)
    with PyApi() as py:
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
        py_log = script(py, [("photos", {"scope": "all"})])
    with RsApi(cfg_path) as rs:
        rs_log = script(rs, [("photos", {"scope": "all"})])
    assert py_log[-1]["job"]["state"] == "error" if isinstance(py_log[-1], dict) else True
    assert rs_log == py_log


def test_an_accepted_ai_name_survives_more_photos_and_reanalysis(vault, ollama):  # noqa: F811
    """Accept the model's title for a trip, back up more photos (one batch before, one a few days after), analyse
    again: still one trip, still that name, still its suggestion, now with the new photos, the same in both engines."""
    from datetime import datetime

    from photovault.manifest import Manifest
    from test_review import UDAIPUR, add, jpeg

    cfg, keys = vault
    cfg_path = os.environ["PHOTOVAULT_CONFIG"]
    save_config(replace(load_config(cfg_path), ai_enabled=True, ai_model="llava:7b", ai_host=ollama), cfg_path)
    with PyApi() as py:
        py.call("POST", "/api/analysis/start")
        wait(py, "/api/analysis")
    pristine = cfg.backup_root.parent / "pristine"
    shutil.copytree(cfg.backup_root, pristine)

    def scenario(api):
        api.call("POST", "/api/ai/start", {"kind": "trips"})
        wait(api, "/api/ai")
        before = api.call("GET", "/api/trips")[1]["trips"][0]
        title = before["ai"]["title"]
        api.call("PUT", f"/api/trips/{before['id']}", {"name": title})                    # "use this name", as the app does
        m = Manifest(cfg.backup_root)
        n = 0
        for day in (datetime(2025, 12, 24, 9), datetime(2026, 1, 5, 9)):                  # an earlier day, and a few days after
            for i in range(3):
                n += 1
                add(m, cfg.backup_root, f"IMG_9{n:03d}.JPG", jpeg("olive", size=(40 + n, 41), make="Apple"), day.replace(hour=9 + i), *UDAIPUR)
        m.close()
        api.call("POST", "/api/analysis/start")
        wait(api, "/api/analysis")
        after = api.call("GET", "/api/trips")[1]["trips"]
        return before, after

    FakeOllama.calls = 0
    with PyApi() as py:
        py_before, py_after = scenario(py)
    py_state = ai_tables(cfg)
    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    FakeOllama.calls = 0
    with RsApi(cfg_path) as rs:
        rs_before, rs_after = scenario(rs)
    rs_state = ai_tables(cfg)

    title = py_before["ai"]["title"]
    assert title == "Lakes of Mewar"
    assert len(py_after) == 1                                                             # not a second trip
    t = py_after[0]
    assert (t["start"], t["end"]) == ("2025-12-24", "2026-01-05")                         # grew to take the new photos
    assert t["name"] == title and t["named"] is True                                      # the accepted name is kept
    assert t["ai"] and t["ai"]["title"] == title                                          # and its suggestion followed it
    assert t["items"] >= py_before["items"] + 6
    assert len(py_state["trips"]) == 1 and py_state["trips"][0][0] == "2025-12-24"        # the suggestion row moved to the new id
    norm = lambda s: json.loads(re.sub(r"c-[0-9a-f]{8,}", "c-NEW", json.dumps(s)))        # noqa: E731
    assert norm(rs_after) == norm(py_after)
    assert rs_state == py_state
