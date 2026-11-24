"""The old-folder tools and the command line: Rust must do exactly what Python does, on identical copies of a vault."""
import asyncio
import os
import re
import shutil
import sqlite3
import subprocess

import pytest

from photovault.config import Config, load_config, save_config
from photovault.copier import adopt, consolidate, run_copy, scan
from photovault.legacy import OLD_IMPORT_DIR, compare_old, import_old, relocate_old
from photovault.manifest import Manifest
from test_contract_rust import EXE
from test_contract_rust_backup import DirFake, exif_jpeg, mov, snapshot, stand_in_phone

pytestmark = pytest.mark.skipif(not EXE.exists(), reason="build the Rust engine first: cargo build (in engine)")


def make_old_folder(old, phone):
    big = phone / "DCIM" / "100APPLE"
    old.mkdir(parents=True)

    def put(rel, data, mtime):
        p = old / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        os.utime(p, (mtime, mtime))

    put("IMG_0001.JPG", (big / "IMG_0001.JPG").read_bytes(), 1_700_000_000)                  # same name and size
    put("renamed_photo.jpg", (phone / "DCIM" / "loose.JPG").read_bytes(), 1_690_000_100)             # same size, other name
    clip = mov(__import__("datetime").datetime(2025, 6, 1, 12, 0, 0)) + b"\0" * 777           # re-encoded video
    put("clip.MOV", clip, 1_748_779_200)
    put("unmatched.txt", b"only in the old folder", 1_650_000_000)
    put("IMG-20240101-WA0001.jpg", exif_jpeg("blue", make=None, size=(40, 40)), 1_704_100_000)
    put("sub/IMG_0003.PNG", b"\x89PNG other bytes entirely", 1_600_000_000)                    # same name, other bytes
    put("sub/deeper/notes.txt", b"hello", 1_640_000_000)
    for i in range(31):                                                                       # an import day, not a capture day
        put(f"bulk/b{i:02d}.txt", f"bulk {i}".encode(), 1_680_000_000)


def tables(cfg):
    rows, files = snapshot(cfg)
    con = sqlite3.connect(cfg.backup_root / ".photovault" / "manifest.db")
    extra = {"links": sorted(con.execute("SELECT old_key, phone_key, kind FROM legacy_link").fetchall()),
             "vinfo": sorted((k, round(c, 3), round(d, 3)) for k, c, d in con.execute("SELECT key, creation, duration FROM vinfo"))}
    con.close()
    return rows, files, extra


def park_an_import(cfg):
    """Pretend an earlier version put one imported file in FromOldBackup/."""
    m = Manifest(cfg.backup_root)
    row = m.db.execute("SELECT key, dest, name FROM items WHERE key LIKE 'legacy|%' AND status='done' ORDER BY key LIMIT 1").fetchone()
    new = f"{OLD_IMPORT_DIR}/{row['name']}"
    (cfg.backup_root / new).parent.mkdir(parents=True, exist_ok=True)
    os.replace(cfg.backup_root / row["dest"], cfg.backup_root / new)
    m.db.execute("UPDATE items SET dest=? WHERE key=?", (new, row["key"]))
    m.commit()
    m.close()


def rs(cfg_path, phone, *args):
    env = {**os.environ, "PHOTOVAULT_FAKE_DEVICE": str(phone)}
    r = subprocess.run([str(EXE), "-c", str(cfg_path), *args], env=env, capture_output=True, text=True, timeout=120)
    return r.returncode, r.stdout, r.stderr


@pytest.fixture
def world(tmp_path):
    ssd = tmp_path / "ssd"
    ssd.mkdir()
    cfg = Config(ssd_root=ssd, backup_dir="V", free_space_margin_gb=0)
    cfg_path = tmp_path / "config.toml"
    save_config(cfg, cfg_path)
    phone = tmp_path / "phone"
    stand_in_phone(phone)
    old = tmp_path / "old"
    make_old_folder(old, phone)
    return cfg, cfg_path, phone, old, tmp_path


def number(text, label):
    return int(re.search(re.escape(label) + r"\s*:?\s*(\d+)", text).group(1))


def test_old_folder_tools_match(world):
    cfg, cfg_path, phone, old, tmp = world

    async def backup():
        m = Manifest(cfg.backup_root)
        dev = DirFake(phone)
        await scan(dev, m, cfg)
        await run_copy(dev, m, cfg, progress=lambda *_: None)
        m.close()

    asyncio.run(backup())
    pristine = tmp / "pristine"
    shutil.copytree(cfg.backup_root, pristine)

    # ---- Python ----
    m = Manifest(cfg.backup_root)
    rep = asyncio.run(compare_old(DirFake(phone), m, old, cfg, progress=lambda *_: None))
    copied, skipped, failed = import_old(m, cfg, old, None, progress=lambda *_: None)
    again = import_old(m, cfg, old, None, progress=lambda *_: None)
    m.close()
    park_an_import(cfg)
    m = Manifest(cfg.backup_root)
    moved, rfailed = relocate_old(m, cfg, progress=lambda *_: None)
    m.close()
    py = tables(cfg)

    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)

    # ---- Rust ----
    code, out, err = rs(cfg_path, phone, "compare-old", str(old), "--out", str(tmp / "details.txt"))
    assert code == 0, err
    assert number(out, "old folder") == rep.examined
    assert number(out, "same name and size as a phone file") == len(rep.same_name_size)
    assert number(out, "same size, different name") == len(rep.same_size)
    assert number(out, "re-encoded copy of a phone original") == len(rep.converted)
    assert number(out, "slow-mo/speed-changed export") == len(rep.speed_changed)
    assert number(out, "not explained by anything on phone") == len(rep.unmatched)
    code, out, err = rs(cfg_path, phone, "import-old", str(old))
    assert code == 0, err
    assert f"imported {copied}, already in vault {skipped}, failed {failed}" in out
    code, out, err = rs(cfg_path, phone, "import-old", str(old))
    assert f"imported {again[0]}, already in vault {again[1]}, failed {again[2]}" in out
    park_an_import(cfg)
    code, out, err = rs(cfg_path, phone, "relocate-old")
    assert f"moved {moved} into the normal folders, failed {rfailed}" in out, out + err
    rust = tables(cfg)

    assert rep.converted and rep.same_size and rep.same_name_size and rep.unmatched and copied > 30
    assert rust[0] == py[0]
    assert rust[1] == py[1]
    assert rust[2] == py[2]
    code, out, err = rs(cfg_path, phone, "status")
    assert code == 0 and "done=" in out and "date sources:" in out
    code, out, err = rs(cfg_path, phone, "verify", "--deep")
    assert code == 0 and "missing=0 wrong_size=0 corrupt=0" in out, out


def test_adopt_and_consolidate_match(world):
    cfg, cfg_path, phone, old, tmp = world

    async def scan_only():
        m = Manifest(cfg.backup_root)
        await scan(DirFake(phone), m, cfg)
        m.close()

    asyncio.run(scan_only())
    pristine = tmp / "pristine"
    shutil.copytree(cfg.backup_root, pristine)
    folder = phone / "DCIM" / "100APPLE"          # an old backup that is a copy of part of the phone

    m = Manifest(cfg.backup_root)
    matched, examined = adopt(m, folder, cfg)
    done, failed = consolidate(m, cfg, None, progress=lambda *_: None)
    m.close()
    py = tables(cfg)

    shutil.rmtree(cfg.backup_root)
    shutil.copytree(pristine, cfg.backup_root)
    code, out, err = rs(cfg_path, phone, "adopt", str(folder))
    assert f"examined {examined} files, matched {matched} phone files already backed up" in out, out + err
    code, out, err = rs(cfg_path, phone, "consolidate")
    assert f"consolidated {done}, failed {failed}" in out, out + err
    rust = tables(cfg)
    assert done > 3
    assert rust == py


def test_run_scan_and_sync_from_the_command_line(world):
    cfg, cfg_path, phone, old, tmp = world
    code, out, err = rs(cfg_path, phone, "run", "--limit", "4")
    assert code == 0, err
    assert "copied 4, failed 0" in out and "5 files on phone" not in out
    code, out, err = rs(cfg_path, phone, "sync")
    assert code == 1 and "NOT IN SYNC" in out and "NOT on SSD yet: 5" in out
    code, out, err = rs(cfg_path, phone, "run")
    assert code == 0 and "copied 5, failed 0" in out
    code, out, err = rs(cfg_path, phone, "sync")
    assert code == 0 and "IN SYNC" in out
    code, out, err = rs(cfg_path, phone, "run", "--dry-run")
    assert code == 0 and "done=9" in out
