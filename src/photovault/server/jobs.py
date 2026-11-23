"""Runs one backup at a time inside the engine and reports live progress to the UI."""
from __future__ import annotations

import asyncio
import json
import time
from collections import deque
from pathlib import Path

from ..config import Config, find_config, load_config
from ..copier import Cancelled, Control, Hooks, run_copy, scan
from ..manifest import Manifest
from ..verify import verify_ssd
from . import system

ACTIVE = {"starting", "scanning", "copying", "paused", "cancelling"}
SPEED_WINDOW = 12.0  # seconds of history used for speed and time left


class BackupJob(Hooks):
    def __init__(self, device_factory, cfg: Config, limit: int | None, only_reencoded: bool):
        self.device_factory, self.cfg, self.limit, self.only_reencoded = device_factory, cfg, limit, only_reencoded
        self.control = Control()
        self.state = "starting"
        self.message = "Starting…"
        self.scan_seen = 0
        self.plan_files = self.plan_bytes = 0
        self.done_files = self.done_bytes = self.failed = self.retries = 0
        self.current: dict | None = None
        self._file_abs = 0
        self.samples: deque[tuple[float, int]] = deque()
        self.recent: deque[dict] = deque(maxlen=40)
        self.started = time.time()
        self.finished: float | None = None
        self.result: dict | None = None
        self.error: str | None = None
        self.version = 0
        self.task: asyncio.Task | None = None

    # -- bookkeeping ---------------------------------------------------------------------
    def _touch(self) -> None:
        self.version += 1

    def log(self, level: str, text: str) -> None:
        self.recent.append({"t": time.time(), "level": level, "text": text})
        self._touch()

    def _set(self, state: str, message: str) -> None:
        self.state, self.message = state, message
        self._touch()

    def _speed(self) -> float:
        if self.state != "copying" or len(self.samples) < 2:
            return 0.0
        (t0, b0), (t1, b1) = self.samples[0], self.samples[-1]
        return max(0.0, (b1 - b0) / (t1 - t0)) if t1 - t0 >= 1.0 else 0.0

    def snapshot(self) -> dict:
        now = self.finished or time.time()
        speed = self._speed()
        remaining = max(0, self.plan_bytes - self.done_bytes)
        return {
            "state": self.state, "message": self.message,
            "scan": {"seen": self.scan_seen},
            "plan": {"files": self.plan_files, "bytes": self.plan_bytes},
            "done": {"files": self.done_files, "bytes": self.done_bytes},
            "failed": self.failed, "retries": self.retries, "current": self.current,
            "speed_bps": speed, "eta_seconds": int(remaining / speed) if speed > 0 and self.state == "copying" else None,
            "started_at": self.started, "elapsed": round(now - self.started, 1),
            "recent": list(self.recent), "result": self.result, "error": self.error, "version": self.version,
        }

    # -- Hooks from the copier -----------------------------------------------------------------
    def on_plan(self, files: int, total_bytes: int) -> None:
        self.plan_files, self.plan_bytes = files, total_bytes
        self._set("copying", f"Copying {files:,} files")
        self.log("info", f"{files:,} files to copy ({total_bytes / 1e9:.1f} GB)")

    def on_file_start(self, row) -> None:
        self.current = {"name": row.name, "size": row.size, "done": 0}
        self._file_abs = 0
        self._touch()

    def on_progress(self, row, done_bytes: int) -> None:
        self.done_bytes += done_bytes - self._file_abs
        self._file_abs = done_bytes
        if self.current:
            self.current["done"] = done_bytes
        now = time.time()
        self.samples.append((now, self.done_bytes))
        while self.samples and now - self.samples[0][0] > SPEED_WINDOW:
            self.samples.popleft()
        self._touch()

    def on_file_end(self, row, ok: bool, error: str | None) -> None:
        if ok:
            self.done_files += 1
        else:
            self.failed += 1
            self.log("error", f"{row.name} could not be copied: {error}")
        self.current = None
        self._touch()

    def on_retry(self, row, attempt: int, error: str) -> None:
        self.retries += 1
        self.message = "Connection to the iPhone dropped. Reconnecting…"
        self.log("warn", f"{row.name}: {error}. Retrying (attempt {attempt + 1}).")

    # -- control -----------------------------------------------------------------------------
    def pause(self) -> None:
        self.control.pause()
        self.samples.clear()
        self._set("paused", "Paused. Nothing is lost; resume when you are ready.")
        self.log("info", "Paused")

    def resume(self) -> None:
        self.control.resume()
        self._set("copying", f"Copying {self.plan_files:,} files")
        self.log("info", "Resumed")

    def cancel(self) -> None:
        self.control.cancel()
        self._set("cancelling", "Stopping after the current block…")

    # -- the run -----------------------------------------------------------------------------
    async def run(self) -> None:
        manifest = None
        try:
            if not self.cfg.ssd_root.exists():
                raise RuntimeError(f"The backup drive was not found at {self.cfg.ssd_root}. Connect it and try again.")
            device = self.device_factory()
            self._set("starting", "Connecting to the iPhone…")
            try:
                await device.reconnect()
            except Exception as e:  # noqa: BLE001
                _, hint = system.explain_device_error(e)
                raise RuntimeError(hint) from e
            manifest = Manifest(self.cfg.backup_root)
            self._set("scanning", "Reading the iPhone's photo list…")
            self.log("info", "Reading the photo list from the iPhone")

            def seen(n: int) -> None:
                self.scan_seen = n
                self._touch()

            total_seen, new = await scan(device, manifest, self.cfg, on_progress=seen)
            self.log("info", f"{total_seen:,} files on the iPhone, {new:,} new since the last scan")
            await self.control.checkpoint()
            done, failed = await run_copy(device, manifest, self.cfg, progress=lambda *_: None, limit=self.limit,
                                          only_reencoded_originals=self.only_reencoded,
                                          control=self.control, hooks=self)
            self._set("copying", "Checking the files on the SSD…")
            rep = await asyncio.to_thread(verify_ssd, manifest, self.cfg, False, lambda *_: None)
            self.result = {
                "copied": done, "failed": failed, "bytes": self.done_bytes, "checked": rep.checked,
                "ssd_ok": rep.ok, "missing": len(rep.missing), "wrong_size": len(rep.wrong_size),
                "failures": [
                    {"name": r["name"], "error": r["error"]} for r in manifest.db.execute(
                        "SELECT name, error FROM items WHERE status='failed' AND error IS NOT NULL LIMIT 20")],
            }
            self.finished = time.time()
            msg = f"Backup finished: {done:,} files copied" + (f", {failed:,} failed" if failed else "")
            self._set("finished", msg)
            self.log("info" if not failed else "warn", msg)
        except Cancelled:
            self.finished = time.time()
            self.result = {"copied": self.done_files, "failed": self.failed, "bytes": self.done_bytes, "cancelled": True}
            self._set("cancelled", "Stopped. Files already copied are safe.")
            self.log("info", "Cancelled")
        except SystemExit as e:  # run_copy reports "not enough space" this way
            self._fail(str(e))
        except Exception as e:  # noqa: BLE001
            self._fail(str(e) or type(e).__name__)
        finally:
            if manifest is not None:
                manifest.close()

    def _fail(self, text: str) -> None:
        self.finished = time.time()
        self.error = text
        self._set("error", text)
        self.log("error", text)


class JobManager:
    def __init__(self, device_factory):
        self.device_factory = device_factory
        self.job: BackupJob | None = None
        self.extra_busy = lambda: False

    @property
    def active(self) -> bool:
        return self.job is not None and self.job.state in ACTIVE

    def snapshot(self) -> dict:
        if self.job is None:
            return {"state": "idle", "message": "", "version": 0}
        return self.job.snapshot()

    def start(self, limit: int | None, only_reencoded: bool) -> BackupJob:
        if self.active:
            raise RuntimeError("A backup is already running.")
        if self.extra_busy():
            raise RuntimeError("Files are being filed into trip folders. Wait for that to finish first.")
        found = find_config()
        if found is None:
            raise FileNotFoundError("Save your settings first.")
        cfg = load_config(found)
        self.job = BackupJob(self.device_factory, cfg, limit, only_reencoded)
        self.job.task = asyncio.create_task(self.job.run())
        return self.job


class AnalysisManager:
    """Tags files and finds trips in the background (reads the SSD copy only)."""

    def __init__(self) -> None:
        self.state = "idle"
        self.done = self.total = 0
        self.message = ""
        self.summary: dict | None = None
        self.error: str | None = None
        self.task: asyncio.Task | None = None

    @property
    def active(self) -> bool:
        return self.state == "running"

    def snapshot(self) -> dict:
        return {"state": self.state, "done": self.done, "total": self.total, "message": self.message,
                "summary": self.summary, "error": self.error}

    def _work(self, cfg: Config) -> None:
        from ..analysis import run_analysis

        manifest = Manifest(cfg.backup_root)
        try:
            def progress(done: int, total: int, msg: str) -> None:
                self.done, self.total, self.message = done, total, msg

            self.summary = run_analysis(manifest, cfg, progress)
        finally:
            manifest.close()

    async def _run(self, cfg: Config) -> None:
        try:
            await asyncio.to_thread(self._work, cfg)
            self.state, self.message = "done", "Finished"
        except Exception as e:  # noqa: BLE001
            self.state, self.error, self.message = "error", str(e) or type(e).__name__, "Stopped by an error"

    def start(self, cfg: Config) -> None:
        if self.active:
            raise RuntimeError("Already analysing.")
        if not (cfg.backup_root / ".photovault" / "manifest.db").exists():
            raise FileNotFoundError("There is nothing backed up yet.")
        self.state, self.done, self.total, self.error, self.summary = "running", 0, 0, None, None
        self.message = "Starting…"
        self.task = asyncio.create_task(self._run(cfg))


class HealthManager:
    """Compare the phone with the SSD, or re-read every backed-up file, in the background."""

    def __init__(self, device_factory, busy) -> None:
        self.device_factory, self.busy = device_factory, busy   # busy(): is a backup running?
        self.state = "idle"
        self.kind: str | None = None
        self.done = self.total = 0
        self.message = ""
        self.error: str | None = None
        self.started: float | None = None
        self.stop = False
        self.task: asyncio.Task | None = None

    @property
    def active(self) -> bool:
        return self.state == "running"

    def snapshot(self) -> dict:
        return {"state": self.state, "kind": self.kind, "done": self.done, "total": self.total,
                "message": self.message, "error": self.error, "started_at": self.started}

    def start(self, cfg: Config, kind: str) -> None:
        if self.active:
            raise RuntimeError("A check is already running.")
        if self.busy():
            raise RuntimeError("A backup is running. Wait for it to finish first.")
        if not (cfg.backup_root / ".photovault" / "manifest.db").exists():
            raise FileNotFoundError("There is nothing backed up yet.")
        self.kind, self.state, self.done, self.total, self.error, self.stop = kind, "running", 0, 0, None, False
        self.message, self.started = "Starting…", time.time()
        self.task = asyncio.create_task(self._run(cfg, kind))

    async def _run(self, cfg: Config, kind: str) -> None:
        manifest = Manifest(cfg.backup_root)
        try:
            result = await (self._sync(cfg, manifest) if kind == "sync" else self._deep(cfg, manifest))
            result.update(kind=kind, at=time.time())
            manifest.db.execute("INSERT OR REPLACE INTO meta VALUES(?, ?)", (f"health_{kind}", json.dumps(result)))
            manifest.commit()
            self.state, self.message = "done", "Finished"
        except Cancelled:
            self.state, self.message = "cancelled", "Stopped"
        except SystemExit as e:
            self.state, self.error, self.message = "error", str(e), "Stopped by an error"
        except Exception as e:  # noqa: BLE001
            self.state, self.error, self.message = "error", str(e) or type(e).__name__, "Stopped by an error"
        finally:
            manifest.close()

    async def _deep(self, cfg: Config, manifest: Manifest) -> dict:
        self.message = "Re-reading every file on the SSD…"

        def progress(done: int, total: int) -> None:
            self.done, self.total = done, total

        rep = await asyncio.to_thread(verify_ssd, manifest, cfg, True, lambda *_: None, progress, lambda: self.stop)
        return {"checked": rep.checked, "missing": rep.missing, "wrong_size": rep.wrong_size,
                "corrupt": rep.corrupt, "ok": rep.ok}

    async def _sync(self, cfg: Config, manifest: Manifest) -> dict:
        from ..verify import sync_report

        self.message = "Connecting to the iPhone…"
        device = self.device_factory()
        try:
            await device.reconnect()
        except Exception as e:  # noqa: BLE001
            _, hint = system.explain_device_error(e)
            raise RuntimeError(hint) from e
        self.message = "Comparing the iPhone with the SSD…"

        def progress(n: int) -> None:
            self.done = n

        rep = await sync_report(device, manifest, cfg, progress)
        return {"on_phone": rep.on_phone, "backed_up": rep.backed_up, "waiting": len(rep.missing_on_ssd),
                "waiting_sample": rep.missing_on_ssd[:25], "adopted_not_copied": len(rep.adopted_not_copied),
                "only_on_ssd": rep.only_on_ssd, "skipped": rep.skipped_by_choice,
                "checked": rep.ssd.checked, "missing": rep.ssd.missing, "wrong_size": rep.ssd.wrong_size,
                "corrupt": rep.ssd.corrupt, "ok": rep.ssd.ok, "in_sync": rep.in_sync}

    def cancel(self) -> None:
        self.stop = True


class AiManager:
    """Asks a local Ollama for trip titles or picture descriptions, in the background and resumably."""

    def __init__(self) -> None:
        self.state = "idle"
        self.kind: str | None = None
        self.done = self.total = 0
        self.message = ""
        self.error: str | None = None
        self.stop = False
        self.task: asyncio.Task | None = None
        self.skipped = 0

    @property
    def active(self) -> bool:
        return self.state == "running"

    def snapshot(self) -> dict:
        return {"state": self.state, "kind": self.kind, "done": self.done, "total": self.total,
                "message": self.message, "error": self.error, "skipped": self.skipped}

    def start(self, cfg: Config, kind: str, limit: int, scope: str) -> None:
        if self.active:
            raise RuntimeError("Smart tags are already running.")
        if not cfg.ai_enabled or not cfg.ai_model:
            raise FileNotFoundError("Turn on smart tags and choose a model in Setup first.")
        if not (cfg.backup_root / ".photovault" / "manifest.db").exists():
            raise FileNotFoundError("There is nothing backed up yet.")
        self.kind, self.state, self.done, self.total, self.error, self.stop, self.skipped = kind, "running", 0, 0, None, False, 0
        self.message = "Starting…"
        self.task = asyncio.create_task(self._run(cfg, kind, limit, scope))

    def cancel(self) -> None:
        self.stop = True

    async def _run(self, cfg: Config, kind: str, limit: int, scope: str) -> None:
        from ..ai import AiError

        manifest = Manifest(cfg.backup_root)
        try:
            await (self._trips(cfg, manifest) if kind == "trips" else self._photos(cfg, manifest, limit, scope))
            self.state = "cancelled" if self.stop else "done"
            self.message = "Stopped" if self.stop else "Finished"
        except AiError as e:
            self.state, self.error, self.message = "error", str(e), "Stopped by an error"
        except Exception as e:  # noqa: BLE001
            self.state, self.error, self.message = "error", str(e) or type(e).__name__, "Stopped by an error"
        finally:
            manifest.close()

    async def _trips(self, cfg: Config, m: Manifest) -> None:
        from ..ai import suggest_trip

        rows = m.db.execute("SELECT id, start, end, days, items, places FROM trips WHERE items > 0 ORDER BY start DESC").fetchall()
        self.total = len(rows)
        self.message = "Asking the model for trip titles…"
        for r in rows:
            if self.stop:
                return
            mix = dict(m.db.execute("SELECT COALESCE(user_tag, tag), COUNT(*) FROM tags WHERE trip_id=? GROUP BY 1", (r["id"],)).fetchall())
            title, summary = await asyncio.to_thread(
                suggest_trip, cfg.ai_host, cfg.ai_model, places=json.loads(r["places"] or "[]"), start=r["start"], end=r["end"],
                days=r["days"], files=r["items"], mix=mix)
            m.db.execute("INSERT OR REPLACE INTO ai_trips VALUES(?,?,?,?,?)", (r["id"], cfg.ai_model, title, summary, time.time()))
            m.commit()
            self.done += 1

    async def _photos(self, cfg: Config, m: Manifest, limit: int, scope: str) -> None:
        from ..ai import AiError, describe_picture
        from ..tagging import IMAGE_EXTS, VIDEO_EXTS, make_thumbnail, video_previews_available

        previewable = IMAGE_EXTS | (VIDEO_EXTS if video_previews_available() else set())
        where = "" if scope == "all" else "AND t.user_tag IS NULL AND t.conf < 70"
        rows = [r for r in m.db.execute(
            "SELECT t.key, i.name, i.dest FROM tags t JOIN items i ON i.key = t.key "
            "LEFT JOIN ai_tags a ON a.key = t.key AND a.model = ? "
            f"WHERE a.key IS NULL {where} ORDER BY t.conf ASC, i.taken DESC LIMIT ?", (cfg.ai_model, limit * 3))
            if Path(r["name"]).suffix.lower() in previewable][:limit]
        self.total = len(rows)
        self.message = "Describing pictures…"
        failures = 0
        for r in rows:
            if self.stop:
                return
            jpeg = await asyncio.to_thread(make_thumbnail, cfg.backup_root / r["dest"], 512)
            if jpeg is None:
                self.skipped += 1
                self.done += 1
                continue
            try:
                d = await asyncio.to_thread(describe_picture, cfg.ai_host, cfg.ai_model, jpeg)
                failures = 0
            except AiError as e:
                failures += 1
                if failures >= 3:                      # the same complaint three times: do not keep hammering it
                    hint = (" Choose a model that can see pictures in Setup, such as one named llava, moondream or qwen2.5vl."
                            if any(w in str(e).lower() for w in ("multimodal", "image", "vision")) else "")
                    raise AiError(f"{e} (stopped after 3 failures in a row).{hint}") from e
                self.skipped += 1
                self.done += 1
                continue
            m.db.execute("INSERT OR REPLACE INTO ai_tags VALUES(?,?,?,?,?,?)",
                         (r["key"], cfg.ai_model, json.dumps(d["tags"]), d["caption"], d["kind"], time.time()))
            m.commit()
            self.done += 1


class OrganizeManager:
    """Applies the trip filing plan to the SSD in the background, with progress and a stop button."""

    def __init__(self, busy) -> None:
        self.busy = busy                                    # busy(): is a backup or check running?
        self.state = "idle"
        self.done = self.total = 0
        self.message = ""
        self.error: str | None = None
        self.result: dict | None = None
        self.stop = False
        self.task: asyncio.Task | None = None

    @property
    def active(self) -> bool:
        return self.state == "running"

    def snapshot(self) -> dict:
        return {"state": self.state, "done": self.done, "total": self.total, "message": self.message,
                "error": self.error, "result": self.result}

    def start(self, cfg: Config) -> None:
        if self.active:
            raise RuntimeError("Filing is already running.")
        if self.busy():
            raise RuntimeError("A backup or check is running. Wait for it to finish first.")
        if not (cfg.backup_root / ".photovault" / "manifest.db").exists():
            raise FileNotFoundError("There is nothing backed up yet.")
        self.state, self.done, self.total, self.error, self.result, self.stop = "running", 0, 0, None, None, False
        self.message = "Working out what to move…"
        self.task = asyncio.create_task(self._run(cfg))

    def cancel(self) -> None:
        self.stop = True

    def _work(self, cfg: Config) -> dict:
        from ..filing import apply_moves, plan_filing
        from ..verify import verify_ssd

        manifest = Manifest(cfg.backup_root)
        try:
            moves = plan_filing(manifest, cfg)
            self.total = len(moves)
            self.message = f"Moving {len(moves):,} files…" if moves else "Nothing to move."

            def progress(done: int, total: int) -> None:
                self.done = done

            res = apply_moves(manifest, cfg, moves, progress, lambda: self.stop)
            rep = verify_ssd(manifest, cfg, deep=False, progress=lambda *_: None)
            res.update(planned=len(moves), ssd_ok=rep.ok, stopped=self.stop)
            return res
        finally:
            manifest.close()

    async def _run(self, cfg: Config) -> None:
        try:
            self.result = await asyncio.to_thread(self._work, cfg)
            self.state, self.message = ("cancelled", "Stopped") if self.stop else ("done", "Finished")
        except Exception as e:  # noqa: BLE001
            self.state, self.error, self.message = "error", str(e) or type(e).__name__, "Stopped by an error"
