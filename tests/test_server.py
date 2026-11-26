import pytest
from fastapi.testclient import TestClient

from photovault.server.app import create_app

TOKEN = "t0ken"
GOOD = {
    "ssd_root": "E:/", "backup_dir": "PhotoBackup", "remote_roots": ["/DCIM"],
    "layout": "{year}/{year}-{month}/{name}", "undated_layout": "Undated/{name}", "chunk_mb": 4,
    "max_retries": 6, "ignore": ["._*"], "free_space_margin_gb": 5.0, "copy_originals_of_reencoded": True,
    "ai_enabled": False, "ai_host": "http://127.0.0.1:11434", "ai_model": "",
}


@pytest.fixture
def client(tmp_path, monkeypatch):
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(tmp_path / "cfg" / "config.toml"))
    return TestClient(create_app(TOKEN), headers={"x-photovault-token": TOKEN})


def test_api_needs_the_token(tmp_path, monkeypatch):
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(tmp_path / "c.toml"))
    bare = TestClient(create_app(TOKEN))
    assert bare.get("/api/health").status_code == 401
    assert TestClient(create_app(TOKEN), headers={"x-photovault-token": "nope"}).get("/api/health").status_code == 401
    assert bare.get("/api/health?token=" + TOKEN).status_code == 200


def test_foreign_host_is_rejected(client):
    assert client.get("/api/health", headers={"host": "evil.example.com"}).status_code == 403


def test_index_carries_token_and_static_works(client):
    html = client.get("/").text
    assert TOKEN in html and "__TOKEN__" not in html
    assert client.get("/static/app.css").status_code == 200


def test_config_roundtrip_and_bak(client, tmp_path):
    first = client.get("/api/config").json()
    assert first["exists"] is False
    saved = client.put("/api/config", json=GOOD).json()
    assert saved["exists"] is True and saved["backup_root"].replace("\\", "/") == "E:/PhotoBackup"
    again = client.get("/api/config").json()
    assert again["config"]["layout"] == GOOD["layout"] and again["config"]["ssd_root"] in ("E:/", "E:")   # a drive letter is only a path on Windows
    client.put("/api/config", json={**GOOD, "backup_dir": "Photos2"})
    assert (tmp_path / "cfg" / "config.toml.bak").exists()
    assert client.get("/api/config").json()["config"]["backup_dir"] == "Photos2"


@pytest.mark.parametrize("patch", [
    {"layout": "{year}/{oops}/{name}"}, {"layout": "../{name}"}, {"layout": "/abs/{name}"},
    {"layout": "{year}/folder"}, {"chunk_mb": 0}, {"max_retries": 99}, {"backup_dir": "../x"},
    {"remote_roots": ["DCIM"]}, {"ai_host": "http://example.com:11434"}, {"ssd_root": ""},
])
def test_bad_settings_are_rejected(client, patch):
    r = client.put("/api/config", json={**GOOD, **patch})
    assert r.status_code == 422


def test_layout_preview(client):
    ok = client.get("/api/layout-preview", params={"layout": "{year}/{month}/{name}"}).json()
    assert ok["samples"][0]["path"] == "2025/12/IMG_0442.HEIC"
    assert ok["samples"][2]["path"] == "Undated/IMG_2147.PNG"
    assert client.get("/api/layout-preview", params={"layout": "{nope}/{name}"}).json()["error"]


def test_fs_browser_and_path_check(client, tmp_path):
    (tmp_path / "Photos").mkdir()
    (tmp_path / "$hidden").mkdir()
    r = client.get("/api/fs", params={"path": str(tmp_path)}).json()
    assert [e["name"] for e in r["entries"]] == ["Photos"]
    assert client.get("/api/fs", params={"path": str(tmp_path / "missing")}).status_code == 404
    assert client.get("/api/fs").json()["entries"]  # roots
    chk = client.get("/api/path-check", params={"path": str(tmp_path / "NewBackup")}).json()
    assert chk["will_create"] and chk["writable"] and not chk["exists"]
    assert not (tmp_path / "NewBackup").exists()  # checking never creates


def test_volumes_and_status_without_backup(client):
    vols = client.get("/api/volumes").json()
    assert vols and {"path", "label", "fstype", "total", "free", "is_system"} <= set(vols[0])
    assert client.get("/api/status").json()["has_backup"] is False


def test_ollama_refuses_non_local_hosts(client):
    r = client.get("/api/ollama", params={"host": "http://example.com:11434"}).json()
    assert r["reachable"] is False and "local" in r["error"]
