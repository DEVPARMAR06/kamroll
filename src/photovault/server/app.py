"""Engine API. The UI only ever talks to this; a Rust engine can implement the same routes later.

Local-only: bound to 127.0.0.1, every /api call needs the per-launch token, and the Host header
must be loopback (blocks other web pages and DNS rebinding from reaching it).
"""
from __future__ import annotations

import asyncio
import hmac
import json
import sqlite3
from dataclasses import asdict
from datetime import datetime
from pathlib import Path

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field, field_validator

from .. import __version__
from ..config import (Config, check_layout, check_trip_layout, config_from_dict, default_config_path, find_config, load_config,
                      save_config)
from . import system
from . import health as health_routes
from . import organize, review, smart
from .jobs import AiManager, AnalysisManager, HealthManager, JobManager, OrganizeManager

UI_DIR = Path(__file__).resolve().parent.parent / "ui"
SAMPLES = [("IMG_0442.HEIC", datetime(2025, 12, 27)), ("IMG_9820.MOV", datetime(2024, 8, 24)),
           ("IMG_2147.PNG", None)]


class StartBody(BaseModel):
    limit: int | None = Field(default=None, ge=1, le=1_000_000)
    only_reencoded_originals: bool = False


class ConfigModel(BaseModel):
    ssd_root: str = Field(min_length=1)
    backup_dir: str = Field(min_length=1)
    remote_roots: list[str] = Field(min_length=1)
    layout: str
    undated_layout: str
    chunk_mb: int = Field(ge=1, le=64)
    max_retries: int = Field(ge=1, le=20)
    ignore: list[str]
    free_space_margin_gb: float = Field(ge=0, le=500)
    copy_originals_of_reencoded: bool
    ai_enabled: bool
    ai_host: str
    ai_model: str
    trip_radius_km: float = Field(default=60.0, ge=5, le=2000)
    home_lat: float | None = Field(default=None, ge=-90, le=90)
    home_lon: float | None = Field(default=None, ge=-180, le=180)
    file_trips: bool = False
    trip_layout: str = "Trips/{trip_year}/{trip_dated}/{name}"

    @field_validator("layout", "undated_layout")
    @classmethod
    def _layout_ok(cls, v: str) -> str:
        problem = check_layout(v)
        if problem:
            raise ValueError(problem)
        return v

    @field_validator("trip_layout")
    @classmethod
    def _trip_layout_ok(cls, v: str) -> str:
        problem = check_trip_layout(v)
        if problem:
            raise ValueError(problem)
        return v

    @field_validator("backup_dir")
    @classmethod
    def _backup_dir_ok(cls, v: str) -> str:
        if ".." in Path(v.replace("\\", "/")).parts:
            raise ValueError("The folder name cannot contain '..'.")
        return v

    @field_validator("remote_roots")
    @classmethod
    def _roots_ok(cls, v: list[str]) -> list[str]:
        if any(not r.startswith("/") for r in v):
            raise ValueError("Phone folders start with a slash, for example /DCIM.")
        return v

    @field_validator("ai_host")
    @classmethod
    def _host_ok(cls, v: str) -> str:
        if not system._is_loopback(v):
            raise ValueError("Only a local Ollama (127.0.0.1 or localhost) is allowed.")
        return v

    def to_config(self) -> Config:
        d = self.model_dump()
        cfg = Config(ssd_root=Path(d.pop("ssd_root")))
        for k, v in d.items():
            setattr(cfg, k, v)
        return cfg


def _model_from_config(cfg: Config) -> dict:
    d = asdict(cfg)
    d["ssd_root"] = str(cfg.ssd_root).replace("\\", "/")
    return d


def _current() -> tuple[Config | None, Path]:
    found = find_config()
    path = found or default_config_path()
    return (load_config(found) if found else None), path


def _status(cfg: Config) -> dict:
    db = cfg.backup_root / ".photovault" / "manifest.db"
    if not db.exists():
        return {"has_backup": False, "counts": {}, "total_files": 0}
    con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
    try:
        rows = con.execute("SELECT status, COUNT(*), COALESCE(SUM(size),0) FROM items GROUP BY status").fetchall()
    finally:
        con.close()
    counts = {s: {"files": c, "bytes": b} for s, c, b in rows}
    return {"has_backup": True, "counts": counts, "total_files": sum(v["files"] for v in counts.values())}


def create_app(token: str, port: int | None = None, device_factory=None) -> FastAPI:
    app = FastAPI(title="iphone-photo-vault engine", version=__version__, docs_url=None, redoc_url=None,
                  openapi_url="/api/openapi.json")

    if device_factory is None:
        from ..device import AfcDevice
        device_factory = AfcDevice
    jobs = JobManager(device_factory)
    analysis = AnalysisManager()
    review.register(app, lambda: _current()[0], analysis)
    smart.register(app, lambda: _current()[0], AiManager())
    health_mgr = HealthManager(device_factory, lambda: jobs.active or org.active)
    org = OrganizeManager(lambda: jobs.active or health_mgr.active)
    jobs.extra_busy = lambda: org.active
    organize.register(app, lambda: _current()[0], org)
    health_routes.register(app, lambda: _current()[0], health_mgr)

    @app.middleware("http")
    async def guard(request: Request, call_next):
        host = (request.headers.get("host") or "").split(":")[0]
        if host not in ("127.0.0.1", "localhost", "[::1]", "testserver"):
            return JSONResponse({"detail": "Forbidden host"}, status_code=403)
        if request.url.path.startswith("/api"):
            sent = request.headers.get("x-photovault-token") or request.query_params.get("token") or ""
            if not hmac.compare_digest(sent, token):
                return JSONResponse({"detail": "Missing or wrong token"}, status_code=401)
        return await call_next(request)

    @app.get("/api/health")
    async def health():
        return {"ok": True, "version": __version__}

    @app.get("/api/config")
    async def get_config():
        cfg, path = _current()
        if cfg is None:
            cfg = Config(ssd_root=Path(""))
        return {"config": _model_from_config(cfg), "path": str(path).replace("\\", "/"),
                "exists": find_config() is not None, "backup_root": str(cfg.backup_root).replace("\\", "/")}

    @app.put("/api/config")
    async def put_config(body: ConfigModel):
        cfg = body.to_config()
        found = find_config()
        path = found or default_config_path()
        save_config(cfg, path)
        return {"config": _model_from_config(cfg), "path": str(path).replace("\\", "/"), "exists": True,
                "backup_root": str(cfg.backup_root).replace("\\", "/")}

    @app.get("/api/device")
    async def device():
        return await system.device_info()

    @app.get("/api/volumes")
    async def volumes():
        return system.list_volumes()

    @app.get("/api/fs")
    async def fs(path: str = ""):
        try:
            return system.list_dirs(path or None)
        except FileNotFoundError:
            raise HTTPException(404, "That folder does not exist.")
        except PermissionError:
            raise HTTPException(403, "No permission to open that folder.")

    @app.get("/api/path-check")
    async def path_check(path: str):
        return system.path_check(path)

    @app.get("/api/layout-preview")
    async def layout_preview(layout: str, undated: str = "Undated/{name}", trip_layout: str | None = None):
        for field, tmpl in (("layout", layout), ("undated_layout", undated)):
            problem = check_layout(tmpl)
            if problem:
                return {"error": problem, "field": field, "samples": []}
        trip_sample = None
        if trip_layout:
            problem = check_trip_layout(trip_layout)
            if problem:
                return {"error": problem, "field": "trip_layout", "samples": []}
            trip_sample = trip_layout.format(year="2025", month="12", day="27", name="IMG_0442.HEIC", stem="IMG_0442",
                                             ext="HEIC", trip="Udaipur", trip_dated="Udaipur (Dec 2025)", trip_year="2025")
        out = []
        for name, dt in SAMPLES:
            stem, _, ext = name.rpartition(".")
            tmpl = layout if dt else undated
            d = dt or datetime(2000, 1, 1)
            rel = tmpl.format(year=f"{d.year:04d}", month=f"{d.month:02d}", day=f"{d.day:02d}",
                              name=name, stem=stem, ext=ext)
            out.append({"name": name, "date": dt.strftime("%d %b %Y") if dt else None, "path": rel.replace("\\", "/")})
        return {"error": None, "field": None, "samples": out, "trip_sample": trip_sample}

    @app.get("/api/ollama")
    async def ollama(host: str = "http://127.0.0.1:11434"):
        return await system.ollama_info(host)

    @app.get("/api/status")
    async def status():
        cfg, _ = _current()
        if cfg is None:
            return {"has_backup": False, "counts": {}, "total_files": 0}
        return _status(cfg)


    @app.get("/api/backup")
    async def backup_state():
        return jobs.snapshot()

    @app.post("/api/backup/start")
    async def backup_start(body: StartBody):
        try:
            jobs.start(body.limit, body.only_reencoded_originals)
        except FileNotFoundError as e:
            raise HTTPException(400, str(e))
        except RuntimeError as e:
            raise HTTPException(409, str(e))
        return jobs.snapshot()

    def _need_job(*states: str):
        if jobs.job is None or jobs.job.state not in states:
            raise HTTPException(409, "That is not possible in the current state.")
        return jobs.job

    @app.post("/api/backup/pause")
    async def backup_pause():
        _need_job("scanning", "copying").pause()
        return jobs.snapshot()

    @app.post("/api/backup/resume")
    async def backup_resume():
        _need_job("paused").resume()
        return jobs.snapshot()

    @app.post("/api/backup/cancel")
    async def backup_cancel():
        _need_job("starting", "scanning", "copying", "paused").cancel()
        return jobs.snapshot()

    @app.get("/api/backup/events")
    async def backup_events(request: Request):
        """Server-sent events: a full snapshot whenever something changes, and at least once a second."""
        async def gen():
            last = -1
            idle = 0.0
            while True:
                snap = jobs.snapshot()
                if snap["version"] != last or idle >= 1.0:
                    last, idle = snap["version"], 0.0
                    yield "data: " + json.dumps(snap) + "\n\n"
                await asyncio.sleep(0.2)
                idle += 0.2
                if await request.is_disconnected():
                    return
        return StreamingResponse(gen(), media_type="text/event-stream",
                                 headers={"Cache-Control": "no-store", "X-Accel-Buffering": "no"})

    app.mount("/static", StaticFiles(directory=UI_DIR), name="static")

    @app.get("/", response_class=HTMLResponse)
    async def index():
        html = (UI_DIR / "index.html").read_text(encoding="utf-8")
        return HTMLResponse(html.replace("__TOKEN__", token), headers={"Cache-Control": "no-store"})

    return app
