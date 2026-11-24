from __future__ import annotations

import json
import os
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

LAYOUT_FIELDS = {"year", "month", "day", "name", "stem", "ext"}
TRIP_LAYOUT_FIELDS = LAYOUT_FIELDS | {"trip", "trip_dated", "trip_year"}
DEFAULT_TRIP_LAYOUT = "Trips/{trip_year}/{trip_dated}/{name}"


@dataclass
class Config:
    ssd_root: Path
    backup_dir: str = "PhotoBackup"
    remote_roots: list[str] = field(default_factory=lambda: ["/DCIM"])
    layout: str = "{year}/{year}-{month}/{name}"
    undated_layout: str = "Undated/{name}"
    chunk_mb: int = 4
    max_retries: int = 6
    ignore: list[str] = field(default_factory=lambda: ["._*", ".DS_Store", ".MISC", "Thumbs.db"])
    free_space_margin_gb: float = 5.0
    copy_originals_of_reencoded: bool = True
    ai_enabled: bool = False
    ai_host: str = "http://127.0.0.1:11434"
    ai_model: str = ""
    trip_radius_km: float = 60.0
    home_lat: float | None = None
    home_lon: float | None = None
    file_trips: bool = False
    trip_layout: str = DEFAULT_TRIP_LAYOUT

    @property
    def backup_root(self) -> Path:
        p = Path(self.backup_dir)
        return p if p.is_absolute() else self.ssd_root / p

    @property
    def chunk_bytes(self) -> int:
        return self.chunk_mb * 1024 * 1024


def user_config_dir() -> Path:
    if sys.platform == "win32":
        base = Path(os.environ.get("APPDATA") or Path.home() / "AppData" / "Roaming")
    elif sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support"
    else:
        base = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    return base / "iphone-photo-vault"


def find_config() -> Path | None:
    """First existing config: $PHOTOVAULT_CONFIG, ./config.toml, ~/.photovault.toml, user config dir."""
    env = os.environ.get("PHOTOVAULT_CONFIG")
    if env:  # an explicit choice is never second-guessed by the search below
        return Path(env) if Path(env).exists() else None
    candidates = [Path("config.toml"), Path.home() / ".photovault.toml", user_config_dir() / "config.toml"]
    for cand in candidates:
        if cand.exists():
            return cand
    return None


def default_config_path() -> Path:
    """Where a new config is written when none exists yet."""
    env = os.environ.get("PHOTOVAULT_CONFIG")
    return Path(env) if env else user_config_dir() / "config.toml"


def check_layout(template: str) -> str | None:
    """Return a human-readable problem with a layout template, or None if it is fine."""
    try:
        out = template.format(year="2025", month="12", day="27", name="IMG_0001.HEIC", stem="IMG_0001", ext="HEIC")
    except (KeyError, IndexError, ValueError) as e:
        fields = ", ".join("{" + f + "}" for f in sorted(LAYOUT_FIELDS))
        return f"Unknown or malformed field {e}. Use {fields}."
    parts = Path(out.replace("\\", "/")).parts
    if os.path.isabs(out) or out.startswith(("/", "\\")) or ".." in parts or (parts and ":" in parts[0]):
        return "The layout must be a relative folder pattern (no leading slash, drive or '..')."
    if "{name}" not in template and "{stem}" not in template:
        return "The layout must include {name} (or {stem} and {ext}) so files keep their names."
    return None


def check_trip_layout(template: str) -> str | None:
    """A problem with the trip folder pattern, or None. Same rules as the dated layout, plus trip fields."""
    try:
        out = template.format(year="2025", month="12", day="27", name="IMG_0001.HEIC", stem="IMG_0001", ext="HEIC",
                              trip="Udaipur", trip_dated="Udaipur (Dec 2025)", trip_year="2025")
    except (KeyError, IndexError, ValueError) as e:
        fields = ", ".join("{" + f + "}" for f in sorted(TRIP_LAYOUT_FIELDS))
        return f"Unknown or malformed field {e}. Use {fields}."
    parts = Path(out.replace("\\", "/")).parts
    if os.path.isabs(out) or out.startswith(("/", "\\")) or ".." in parts or (parts and ":" in parts[0]):
        return "The layout must be a relative folder pattern (no leading slash, drive or '..')."
    if "{name}" not in template and "{stem}" not in template:
        return "The layout must include {name} (or {stem} and {ext}) so files keep their names."
    if "{trip}" not in template and "{trip_dated}" not in template:
        return "The layout must include {trip_dated} or {trip} so each trip gets its own folder."
    return None


def config_from_dict(data: dict) -> Config:
    paths, dev, cp, ai = (data.get(k, {}) for k in ("paths", "device", "copy", "ai"))
    if "ssd_root" not in paths:
        raise SystemExit("config: [paths] ssd_root is required")
    cfg = Config(ssd_root=Path(paths["ssd_root"]))
    if "backup_dir" in paths:
        cfg.backup_dir = paths["backup_dir"]
    if "remote_roots" in dev:
        cfg.remote_roots = list(dev["remote_roots"])
    for key in ("layout", "undated_layout", "chunk_mb", "max_retries", "ignore", "free_space_margin_gb",
                "copy_originals_of_reencoded"):
        if key in cp:
            setattr(cfg, key, cp[key])
    for key, attr in (("enabled", "ai_enabled"), ("host", "ai_host"), ("model", "ai_model")):
        if key in ai:
            setattr(cfg, attr, ai[key])
    trips = data.get("trips", {})
    for key in ("trip_radius_km", "home_lat", "home_lon", "file_trips", "trip_layout"):
        if key in trips:
            setattr(cfg, key, trips[key])
    return cfg


def load_config(path: str | Path | None) -> Config:
    if path is None:
        path = find_config()
    if path is None:
        raise SystemExit("No config found. Copy config.example.toml to config.toml and set ssd_root.")
    return config_from_dict(tomllib.loads(Path(path).read_text(encoding="utf-8")))


def _s(value) -> str:
    return json.dumps(value, ensure_ascii=True)  # a JSON string or array is also valid TOML


def save_config(cfg: Config, path: str | Path) -> None:
    """Write the config atomically, keeping the previous file as <name>.bak."""
    path = Path(path)
    root = str(cfg.ssd_root).replace("\\", "/")
    flag = lambda b: "true" if b else "false"  # noqa: E731
    home = (f"home_lat = {float(cfg.home_lat)}\nhome_lon = {float(cfg.home_lon)}\n"
            if cfg.home_lat is not None and cfg.home_lon is not None else "")
    text = f"""# Written by Kamroll. Edit here or in the app.

[paths]
ssd_root = {_s(root)}
backup_dir = {_s(cfg.backup_dir.replace(chr(92), "/"))}

[device]
remote_roots = {_s(cfg.remote_roots)}

[copy]
layout = {_s(cfg.layout)}
undated_layout = {_s(cfg.undated_layout)}
chunk_mb = {int(cfg.chunk_mb)}
max_retries = {int(cfg.max_retries)}
ignore = {_s(cfg.ignore)}
free_space_margin_gb = {float(cfg.free_space_margin_gb)}
copy_originals_of_reencoded = {flag(cfg.copy_originals_of_reencoded)}

[ai]
enabled = {flag(cfg.ai_enabled)}
host = {_s(cfg.ai_host)}
model = {_s(cfg.ai_model)}

[trips]
trip_radius_km = {float(cfg.trip_radius_km)}
file_trips = {flag(cfg.file_trips)}
trip_layout = {_s(cfg.trip_layout)}
{home}"""
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.with_suffix(path.suffix + ".bak").write_bytes(path.read_bytes())
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)
