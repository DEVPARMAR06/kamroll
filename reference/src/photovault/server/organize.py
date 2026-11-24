"""Routes for filing trip photos into trip folders (and putting them back)."""
from __future__ import annotations

from fastapi import FastAPI, HTTPException

from ..config import Config
from ..filing import plan_filing, recover_moves
from ..manifest import Manifest
from .jobs import OrganizeManager


def register(app: FastAPI, current_cfg, org: OrganizeManager) -> None:
    def need_cfg() -> Config:
        cfg = current_cfg()
        if cfg is None:
            raise HTTPException(400, "Save your settings first.")
        return cfg

    @app.get("/api/organize")
    async def state():
        return org.snapshot()

    @app.get("/api/organize/preview")
    async def preview():
        """What would move if you applied the setting now. Nothing is changed."""
        cfg = need_cfg()
        if not (cfg.backup_root / ".photovault" / "manifest.db").exists():
            return {"enabled": cfg.file_trips, "into": 0, "back": 0, "total": 0, "samples": []}
        m = Manifest(cfg.backup_root)
        try:
            recover_moves(m, cfg)
            moves = plan_filing(m, cfg)
        finally:
            m.close()
        into = sum(1 for x in moves if x.reason == "into-trip")
        return {"enabled": cfg.file_trips, "into": into, "back": len(moves) - into, "total": len(moves),
                "samples": [{"name": x.name, "from": x.src, "to": x.dst, "reason": x.reason} for x in moves[:12]]}

    @app.post("/api/organize/start")
    async def start():
        cfg = need_cfg()
        try:
            org.start(cfg)
        except FileNotFoundError as e:
            raise HTTPException(400, str(e))
        except RuntimeError as e:
            raise HTTPException(409, str(e))
        return org.snapshot()

    @app.post("/api/organize/cancel")
    async def cancel():
        if not org.active:
            raise HTTPException(409, "Nothing is running.")
        org.cancel()
        return org.snapshot()
