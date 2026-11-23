"""Routes for the Health screen: compare phone and SSD, re-read files, queue repairs."""
from __future__ import annotations

import json
import sqlite3
from typing import Literal

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel

from ..config import Config
from ..manifest import Manifest
from ..verify import VerifyReport, repair_problems
from .jobs import HealthManager

SHOWN = 40   # file names shown per list in the UI; the full lists stay in the manifest


class CheckBody(BaseModel):
    kind: Literal["sync", "deep"]


def _last(cfg: Config) -> dict:
    db = cfg.backup_root / ".photovault" / "manifest.db"
    out: dict = {"sync": None, "deep": None}
    if not db.exists():
        return out
    con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
    try:
        for kind in out:
            row = con.execute("SELECT v FROM meta WHERE k=?", (f"health_{kind}",)).fetchone()
            if row:
                r = json.loads(row[0])
                for key in ("missing", "wrong_size", "corrupt"):
                    full = r.get(key, [])
                    r[key] = {"count": len(full), "shown": full[:SHOWN]}
                out[kind] = r
    except sqlite3.OperationalError:
        pass
    finally:
        con.close()
    return out


def register(app: FastAPI, current_cfg, health: HealthManager) -> None:
    def need_cfg() -> Config:
        cfg = current_cfg()
        if cfg is None:
            raise HTTPException(400, "Save your settings first.")
        return cfg

    @app.get("/api/health/state")
    async def state():
        cfg = need_cfg()
        return {"job": health.snapshot(), "last": _last(cfg)}

    @app.post("/api/health/start")
    async def start(body: CheckBody):
        cfg = need_cfg()
        try:
            health.start(cfg, body.kind)
        except FileNotFoundError as e:
            raise HTTPException(400, str(e))
        except RuntimeError as e:
            raise HTTPException(409, str(e))
        return health.snapshot()

    @app.post("/api/health/cancel")
    async def cancel():
        if not health.active:
            raise HTTPException(409, "Nothing is running.")
        health.cancel()
        return health.snapshot()

    @app.post("/api/health/repair")
    async def repair():
        cfg = need_cfg()
        if health.active:
            raise HTTPException(409, "Wait for the running check to finish.")
        m = Manifest(cfg.backup_root)
        try:
            rep = VerifyReport()
            for kind in ("sync", "deep"):
                row = m.db.execute("SELECT v FROM meta WHERE k=?", (f"health_{kind}",)).fetchone()
                if row:
                    r = json.loads(row[0])
                    rep.missing += r.get("missing", [])
                    rep.wrong_size += r.get("wrong_size", [])
                    rep.corrupt += r.get("corrupt", [])
            if not (rep.missing or rep.wrong_size or rep.corrupt):
                raise HTTPException(409, "No problems recorded.")
            res = repair_problems(m, cfg, rep)
            m.db.execute("DELETE FROM meta WHERE k IN ('health_sync', 'health_deep')")   # results are out of date now
            m.commit()
        finally:
            m.close()
        return res
