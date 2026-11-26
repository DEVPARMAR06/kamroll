"""Looking at the computer: drives, folders, the iPhone, Ollama. Read-only helpers for the UI."""
from __future__ import annotations

import asyncio
import json
import os
import shutil
import string
import sys
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

import psutil

IPHONE_NAMES = {
    "iPhone14,2": "iPhone 13 Pro", "iPhone14,3": "iPhone 13 Pro Max", "iPhone14,4": "iPhone 13 mini",
    "iPhone14,5": "iPhone 13", "iPhone14,7": "iPhone 14", "iPhone14,8": "iPhone 14 Plus",
    "iPhone15,2": "iPhone 14 Pro", "iPhone15,3": "iPhone 14 Pro Max", "iPhone15,4": "iPhone 15",
    "iPhone15,5": "iPhone 15 Plus", "iPhone16,1": "iPhone 15 Pro", "iPhone16,2": "iPhone 15 Pro Max",
    "iPhone17,1": "iPhone 16 Pro", "iPhone17,2": "iPhone 16 Pro Max", "iPhone17,3": "iPhone 16",
    "iPhone17,4": "iPhone 16 Plus",
}
VISION_HINTS = ("llava", "moondream", "-vl", "vl:", "vision", "minicpm-v", "gemma3", "bakllava", "granite3.2-vision")


def _win_label(root: str) -> str:
    import ctypes

    buf = ctypes.create_unicode_buffer(261)
    ok = ctypes.windll.kernel32.GetVolumeInformationW(root, buf, 261, None, None, None, None, 0)
    return buf.value if ok else ""


def list_volumes() -> list[dict]:
    """Mounted drives, external-looking ones first. The system drive is flagged, not hidden."""
    out = []
    system = os.environ.get("SystemDrive", "C:").upper() + "\\" if sys.platform == "win32" else "/"
    for p in psutil.disk_partitions(all=False):
        if "cdrom" in p.opts or not p.fstype or p.fstype == "squashfs":   # squashfs: Linux app packages, not drives
            continue
        mount = p.mountpoint
        try:
            u = psutil.disk_usage(mount)
        except OSError:
            continue
        if u.total == 0:
            continue
        is_system = mount.upper() == system.upper() if sys.platform == "win32" else mount == "/"
        if sys.platform == "win32":
            label = _win_label(mount)
        else:
            label = Path(mount).name
        if sys.platform == "darwin" and not (mount.startswith("/Volumes/") or mount == "/"):
            continue  # hide macOS system internals
        out.append({
            "path": mount.replace("\\", "/"), "label": label or mount, "fstype": p.fstype,
            "total": u.total, "free": u.free,
            "removable": "removable" in p.opts, "is_system": is_system,
        })
    out.sort(key=lambda v: (v["is_system"], v["path"]))
    return out


def list_dirs(path: str | None) -> dict:
    """Folder browser: sub-folders of `path`, or the drive/home roots when path is empty."""
    if not path:
        roots = [{"name": f"{v['label']} ({v['path'].rstrip('/')})", "path": v["path"]} for v in list_volumes()]
        home = str(Path.home()).replace("\\", "/")
        roots.append({"name": "Home", "path": home})
        return {"path": "", "parent": None, "entries": roots}
    p = Path(path)
    if not p.is_dir():
        raise FileNotFoundError(path)
    entries = []
    try:
        for child in sorted(p.iterdir(), key=lambda c: c.name.lower()):
            if child.name.startswith(("$", ".")) or child.name in ("System Volume Information", "Recovery"):
                continue
            try:
                if child.is_dir():
                    entries.append({"name": child.name, "path": str(child).replace("\\", "/")})
            except OSError:
                continue
            if len(entries) >= 500:
                break
    except PermissionError:
        raise
    parent = str(p.parent).replace("\\", "/") if p.parent != p else ""
    return {"path": str(p).replace("\\", "/"), "parent": parent, "entries": entries}


def path_check(path: str) -> dict:
    """Is this a usable backup folder? Never creates anything."""
    p = Path(path)
    existing = p
    while not existing.exists() and existing.parent != existing:
        existing = existing.parent
    exists = p.is_dir()
    try:
        usage = shutil.disk_usage(existing)
        free = usage.free
    except OSError:
        free = None
    writable = os.access(existing, os.W_OK)
    return {
        "path": str(p).replace("\\", "/"), "exists": exists, "will_create": not exists and existing.exists(),
        "writable": writable, "free": free, "volume_missing": not existing.exists(),
        "file_count": sum(1 for _ in p.glob("*")) if exists else 0,
    }


async def device_info() -> dict:
    """The connected iPhone, or a plain-language reason there isn't one."""
    try:
        from pymobiledevice3.lockdown import create_using_usbmux

        ld = await asyncio.wait_for(create_using_usbmux(), 8)
        v = ld.all_values
        pt = v.get("ProductType", "")
        return {"connected": True, "name": v.get("DeviceName", "iPhone"), "model": IPHONE_NAMES.get(pt, pt),
                "ios": v.get("ProductVersion", ""), "error": None, "hint": None}
    except Exception as e:  # noqa: BLE001
        code, hint = explain_device_error(e)
        return _no_device(code, hint)


def explain_device_error(e: BaseException) -> tuple[str, str]:
    """(short code, plain-language hint) for a failure to reach the iPhone."""
    if isinstance(e, asyncio.TimeoutError):
        return "timeout", "The iPhone did not answer. Unlock it and tap Trust if asked."
    name, text = type(e).__name__, str(e)
    if "usbmuxd" in text.lower() or "socket" in text.lower() or name in ("ConnectionRefusedError", "FileNotFoundError"):
        return name, "Apple's USB service is not running. On Windows, install Apple Devices from the Microsoft Store."
    if name == "NoDeviceConnectedError":
        return name, "No iPhone found. Plug it in with a cable, unlock it and tap Trust."
    return name, "Could not talk to the iPhone. Unlock it, reconnect the cable and try again."


def _no_device(code: str, hint: str) -> dict:
    return {"connected": False, "name": None, "model": None, "ios": None, "error": code, "hint": hint}


def _is_loopback(host_url: str) -> bool:
    u = urlparse(host_url)
    return u.scheme in ("http", "https") and u.hostname in ("127.0.0.1", "localhost", "::1")


def _fetch_tags(host: str) -> list[dict]:
    with urllib.request.urlopen(host.rstrip("/") + "/api/tags", timeout=2.5) as r:  # noqa: S310 - loopback only
        return json.loads(r.read().decode("utf-8")).get("models", [])


async def ollama_info(host: str) -> dict:
    """Is a local Ollama running, and which of its models could look at pictures (a name-based guess)."""
    if not _is_loopback(host):
        return {"reachable": False, "models": [], "error": "Only a local Ollama (127.0.0.1 or localhost) is allowed."}
    try:
        tags = await asyncio.to_thread(_fetch_tags, host)
    except Exception:  # noqa: BLE001
        return {"reachable": False, "models": [], "error": "Ollama is not running at this address."}
    models = []
    for m in tags:
        name = m.get("name", "")
        if "embed" in name or "minilm" in name:
            continue
        models.append({"name": name, "gb": round(m.get("size", 0) / 1e9, 1),
                       "vision": any(h in name.lower() for h in VISION_HINTS)})
    models.sort(key=lambda m: (not m["vision"], m["name"]))
    return {"reachable": True, "models": models, "error": None}
