"""Routes for the optional smart tags (a local Ollama). Off unless the person turns it on in Setup."""
from __future__ import annotations

import sqlite3
from typing import Literal

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, Field

from ..config import Config
from .jobs import AiManager


class AiStart(BaseModel):
    kind: Literal["trips", "photos"]
    limit: int = Field(default=300, ge=1, le=5000)
    scope: Literal["uncertain", "all"] = "uncertain"


def register(app: FastAPI, current_cfg, ai: AiManager) -> None:
    def need_cfg() -> Config:
        cfg = current_cfg()
        if cfg is None:
            raise HTTPException(400, "Save your settings first.")
        return cfg

    @app.get("/api/ai")
    async def state():
        cfg = need_cfg()
        done = {"photos": 0, "trips": 0}
        db = cfg.backup_root / ".photovault" / "manifest.db"
        if db.exists() and cfg.ai_model:
            con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
            try:
                done["photos"] = con.execute("SELECT COUNT(*) FROM ai_tags WHERE model=?", (cfg.ai_model,)).fetchone()[0]
                done["trips"] = con.execute("SELECT COUNT(*) FROM ai_trips WHERE model=?", (cfg.ai_model,)).fetchone()[0]
            except sqlite3.OperationalError:
                pass
            finally:
                con.close()
        return {"job": ai.snapshot(), "enabled": cfg.ai_enabled, "model": cfg.ai_model, "ready": bool(cfg.ai_enabled and cfg.ai_model),
                "done": done}

    @app.post("/api/ai/start")
    async def start(body: AiStart):
        cfg = need_cfg()
        try:
            ai.start(cfg, body.kind, body.limit, body.scope)
        except FileNotFoundError as e:
            raise HTTPException(400, str(e))
        except RuntimeError as e:
            raise HTTPException(409, str(e))
        return ai.snapshot()

    @app.post("/api/ai/cancel")
    async def cancel():
        if not ai.active:
            raise HTTPException(409, "Nothing is running.")
        ai.cancel()
        return ai.snapshot()
