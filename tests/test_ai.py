import json
import os
import threading
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
from fastapi.testclient import TestClient

from photovault import ai
from photovault.config import load_config, save_config
from photovault.manifest import Manifest
from photovault.server.app import create_app
from test_review import add, jpeg, vault  # noqa: F401  (fixture)


class FakeOllama:
    """A tiny local stand-in for Ollama's /api/chat."""

    def __init__(self):
        self.mode = "ok"
        self.requests: list[dict] = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                outer.requests.append(body)
                has_images = bool(body["messages"][0].get("images"))
                if outer.mode == "noimages" and has_images:
                    self._send(400, {"error": "this model does not support images"})
                    return
                if has_images:
                    content = json.dumps({"tags": ["Beach", "sunset", "beach", "friends", "sea", "sand", "extra"],
                                          "caption": "Friends on a beach at sunset.", "kind": "memory"})
                else:
                    content = json.dumps({"title": '"Lake days in Udaipur."', "summary": "A calm trip by the water."})
                if outer.mode == "wrapped":
                    content = "Sure! Here you go:\n" + content + "\nHope that helps."
                if outer.mode == "garbage":
                    content = "I cannot help with that."
                self._send(200, {"message": {"role": "assistant", "content": content}})

            def _send(self, code, obj):
                raw = json.dumps(obj).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


@pytest.fixture
def ollama():
    f = FakeOllama()
    yield f
    f.close()


def test_parse_json_copes_with_chatty_models():
    assert ai.parse_json('{"a": 1}') == {"a": 1}
    assert ai.parse_json('Here: {"a": 2} done') == {"a": 2}
    with pytest.raises(ai.AiError):
        ai.parse_json("no json at all")


def test_only_local_addresses_are_allowed():
    assert ai.is_loopback("http://127.0.0.1:11434") and ai.is_loopback("http://localhost:11434")
    assert not ai.is_loopback("http://example.com:11434")
    with pytest.raises(ai.AiError):
        ai.chat_json("http://example.com:11434", "m", "hi")


def test_trip_title_and_picture_description(ollama):
    title, summary = ai.suggest_trip(ollama.url, "m", places=["Udaipur"], start="2025-12-26", end="2025-12-31",
                                     days=6, files=40, mix={"trip": 38, "social": 2})
    assert title == "Lake days in Udaipur" and summary == "A calm trip by the water."   # quotes and the full stop are trimmed
    prompt = ollama.requests[-1]["messages"][0]["content"]
    assert "Udaipur" in prompt and "2025-12-26" in prompt and "38 trip" in prompt
    assert ollama.requests[-1]["format"] == "json" and "images" not in ollama.requests[-1]["messages"][0]
    d = ai.describe_picture(ollama.url, "m", b"\xff\xd8fake")
    assert d == {"tags": ["beach", "sunset", "friends", "sea", "sand"], "caption": "Friends on a beach at sunset.", "kind": "memory"}
    assert ollama.requests[-1]["messages"][0]["images"]                             # the picture was sent as base64


def test_wrapped_and_garbage_answers(ollama):
    ollama.mode = "wrapped"
    assert ai.suggest_trip(ollama.url, "m", places=[], start="a", end="b", days=1, files=1, mix={})[0] == "Lake days in Udaipur"
    ollama.mode = "garbage"
    with pytest.raises(ai.AiError):
        ai.suggest_trip(ollama.url, "m", places=[], start="a", end="b", days=1, files=1, mix={})


def test_ollama_not_running_is_a_readable_error():
    with pytest.raises(ai.AiError, match="did not answer"):
        ai.chat_json("http://127.0.0.1:9", "m", "hi", timeout=2)


def enable(cfg_url):
    path = os.environ["PHOTOVAULT_CONFIG"]
    cfg = load_config(path)
    cfg.ai_enabled, cfg.ai_host, cfg.ai_model = True, cfg_url, "fake-vision"
    save_config(cfg, path)
    return cfg


def client():
    return TestClient(create_app("tk"), headers={"x-photovault-token": "tk"})


def wait(c, route, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        s = c.get(route).json()
        job = s["job"] if "job" in s else s
        if job["state"] not in ("running", "idle"):
            return s
        time.sleep(0.05)
    raise AssertionError("timed out")


def analyse(c):
    c.post("/api/analysis/start")
    wait(c, "/api/analysis")


def test_nothing_runs_until_smart_tags_are_turned_on(vault):
    with client() as c:
        r = c.post("/api/ai/start", json={"kind": "trips"})
        assert r.status_code == 400 and "Turn on smart tags" in r.json()["detail"]
        assert c.get("/api/ai").json()["ready"] is False
        assert c.post("/api/ai/cancel").status_code == 409


def test_trip_titles_are_suggestions_not_renames(vault, ollama):
    cfg, keys = vault
    enable(ollama.url)
    with client() as c:
        analyse(c)
        before = c.get("/api/trips").json()["trips"][0]
        assert c.post("/api/ai/start", json={"kind": "trips"}).status_code == 200
        s = wait(c, "/api/ai")
        assert s["job"]["state"] == "done" and s["done"]["trips"] == 1
        after = c.get("/api/trips").json()["trips"][0]
        assert after["name"] == before["name"]                                       # the name is untouched
        assert after["ai"] == {"title": "Lake days in Udaipur", "summary": "A calm trip by the water."}


def test_picture_tags_are_searchable_and_only_uncertain_ones_are_sent(vault, ollama):
    cfg, keys = vault
    m = Manifest(cfg.backup_root)
    unsure = add(m, cfg.backup_root, "IMG_9999.JPG", jpeg("purple"), datetime(2025, 6, 6, 12))   # no camera data: 55% sure
    m.close()
    enable(ollama.url)
    with client() as c:
        analyse(c)
        c.post("/api/ai/start", json={"kind": "photos"})
        s = wait(c, "/api/ai")
        assert s["job"]["state"] == "done" and s["job"]["total"] == 1 and s["done"]["photos"] == 1
        hit = c.get("/api/review/items", params={"q": "sunset"}).json()
        assert hit["total"] == 1 and hit["items"][0]["key"] == unsure
        assert hit["items"][0]["ai"]["caption"] == "Friends on a beach at sunset." and "beach" in hit["items"][0]["ai"]["tags"]
        assert c.get("/api/review/items", params={"q": "IMG_9999"}).json()["total"] == 1           # names are searched too
        assert c.get("/api/review/items", params={"q": "volcano"}).json()["total"] == 0
        # resumable: nothing left to describe
        c.post("/api/ai/start", json={"kind": "photos"})
        assert wait(c, "/api/ai")["job"]["total"] == 0
        # scope "all" goes further
        c.post("/api/ai/start", json={"kind": "photos", "scope": "all", "limit": 5})
        assert wait(c, "/api/ai")["job"]["total"] == 5


def test_a_model_that_cannot_see_images_stops_with_a_clear_message(vault, ollama):
    cfg, keys = vault
    ollama.mode = "noimages"
    enable(ollama.url)
    with client() as c:
        analyse(c)
        c.post("/api/ai/start", json={"kind": "photos", "scope": "all", "limit": 10})
        s = wait(c, "/api/ai")["job"]
        assert s["state"] == "error" and "does not support images" in s["error"] and "3 failures" in s["error"] and "can see pictures" in s["error"]
        assert c.get("/api/ai").json()["done"]["photos"] == 0
