from __future__ import annotations

import argparse
import asyncio
from pathlib import Path

from .config import load_config
from .copier import adopt, consolidate, run_copy, scan
from .device import AfcDevice
from .legacy import compare_old, import_old, relocate_old
from .manifest import Manifest
from .verify import sync_report, verify_ssd


def _fmt(counts: dict) -> str:
    return ", ".join(f"{k}={c} ({s/1e9:.1f} GB)" for k, (c, s) in sorted(counts.items())) or "empty"


async def _run(args) -> int:
    cfg = load_config(args.config)
    if not cfg.ssd_root.exists():
        raise SystemExit(f"SSD not found at {cfg.ssd_root}")
    manifest = Manifest(cfg.backup_root)
    try:
        if args.cmd == "status":
            print(_fmt(manifest.counts()))
            print("date sources:", manifest.date_sources())
            return 0
        if args.cmd == "verify":
            rep = verify_ssd(manifest, cfg, deep=args.deep)
            print(f"checked {rep.checked}: missing={len(rep.missing)} wrong_size={len(rep.wrong_size)} corrupt={len(rep.corrupt)}")
            for name, items in (("MISSING", rep.missing), ("WRONG SIZE", rep.wrong_size), ("CORRUPT", rep.corrupt)):
                for x in items[:20]:
                    print(f"  {name}: {x}")
            return 0 if rep.ok else 1
        if args.cmd == "adopt":
            m, n = adopt(manifest, Path(args.path), cfg)
            print(f"examined {n} files, matched {m} phone files already backed up")
            return 0
        if args.cmd == "relocate-old":
            moved, failed = relocate_old(manifest, cfg)
            print(f"moved {moved} into the normal folders, failed {failed}")
            return 1 if failed else 0
        if args.cmd == "import-old":
            copied, skipped, failed = import_old(manifest, cfg, Path(args.path), args.limit)
            print(f"imported {copied}, already in vault {skipped}, failed {failed}")
            print(_fmt(manifest.counts()))
            return 1 if failed else 0
        if args.cmd == "consolidate":
            done, failed = consolidate(manifest, cfg, args.limit)
            print(f"consolidated {done}, failed {failed}")
            print(_fmt(manifest.counts()))
            return 1 if failed else 0
        device = AfcDevice()
        await device.reconnect()
        seen, new = await scan(device, manifest, cfg)
        print(f"scan: {seen} files on phone, {new} new to manifest")
        if args.cmd == "sync":
            rep = await sync_report(device, manifest, cfg)
            print(f"phone files: {rep.on_phone}   backed up: {rep.backed_up}")
            print(f"NOT on SSD yet: {len(rep.missing_on_ssd)}   adopted but not consolidated: {len(rep.adopted_not_copied)}")
            print(f"only on SSD (deleted from phone since): {rep.only_on_ssd}   skipped by your choice: {rep.skipped_by_choice}")
            print(f"SSD check: missing={len(rep.ssd.missing)} wrong_size={len(rep.ssd.wrong_size)}")
            for x in rep.missing_on_ssd[:10]:
                print("  missing:", x)
            print("IN SYNC" if rep.in_sync else "NOT IN SYNC")
            return 0 if rep.in_sync else 1
        if args.cmd == "compare-old":
            rep = await compare_old(device, manifest, Path(args.path), cfg)
            print(f"old folder: {rep.examined} files")
            print(f"  same name and size as a phone file : {len(rep.same_name_size)}")
            print(f"  same size, different name          : {len(rep.same_size)}")
            print(f"  re-encoded copy of a phone original: {len(rep.converted)}")
            print(f"  slow-mo/speed-changed export       : {len(rep.speed_changed)}")
            print(f"  not explained by anything on phone : {len(rep.unmatched)}")
            if args.out:
                lines = ([f"unmatched\t{n}" for n in rep.unmatched]
                         + [f"converted\t{a}\t{b}" for a, b in rep.converted]
                         + [f"speed-changed\t{a}\t{b}" for a, b in rep.speed_changed])
                Path(args.out).write_text("\n".join(lines), encoding="utf-8")
                print(f"details written to {args.out}")
            return 0
        if args.cmd == "scan" or args.dry_run:
            print(_fmt(manifest.counts()))
            return 0
        done, failed = await run_copy(device, manifest, cfg, limit=args.limit,
                                       only_reencoded_originals=args.only_reencoded_originals)
        print(f"copied {done}, failed {failed}")
        print(_fmt(manifest.counts()))
        return 1 if failed else 0
    finally:
        manifest.close()


def main() -> None:
    p = argparse.ArgumentParser(prog="photovault", description="iPhone to SSD backup (read-only on the phone).")
    p.add_argument("-c", "--config", help="path to config.toml")
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run", help="scan the phone and copy everything new")
    r.add_argument("--dry-run", action="store_true", help="scan only, copy nothing")
    r.add_argument("--limit", type=int, help="copy at most N files (for testing)")
    r.add_argument("--only-reencoded-originals", action="store_true",
                   help="copy only the phone originals of videos that exist as re-encoded copies in an old backup")
    c = sub.add_parser("consolidate", help="copy adopted files from the old folder into the backup layout")
    c.add_argument("--limit", type=int, help="copy at most N files (for testing)")
    sub.add_parser("scan", help="record phone files in the manifest, copy nothing")
    a = sub.add_parser("adopt", help="mark phone files already present in an existing backup folder as done")
    a.add_argument("path")
    sub.add_parser("status", help="show manifest counts and where capture dates came from")
    v = sub.add_parser("verify", help="check backed-up files on the SSD (no phone needed)")
    v.add_argument("--deep", action="store_true", help="re-hash every file (slow)")
    sub.add_parser("relocate-old", help="move files imported into FromOldBackup/ into the normal dated folders")
    io = sub.add_parser("import-old", help="copy every file of an old backup folder into the vault (read-only on the old folder)")
    io.add_argument("path")
    io.add_argument("--limit", type=int, help="import at most N files (for testing)")
    co = sub.add_parser("compare-old", help="classify what an old backup folder really contains, vs the phone")
    co.add_argument("path")
    co.add_argument("--out", help="write the unmatched and converted file lists to this file")
    sub.add_parser("sync", help="compare the phone with the SSD and report whether they are in sync")
    u = sub.add_parser("ui", help="open the app in your browser (local engine on 127.0.0.1)")
    u.add_argument("--port", type=int, default=8765)
    u.add_argument("--no-open", action="store_true", help="do not open a browser window")
    u.add_argument("--token", help="fixed access token (for development; random by default)")
    args = p.parse_args()
    if args.cmd == "ui":
        raise SystemExit(_serve_ui(args))
    raise SystemExit(asyncio.run(_run(args)))


def _serve_ui(args) -> int:
    import secrets
    import threading
    import webbrowser

    import uvicorn

    from .server.app import create_app

    token = args.token or secrets.token_urlsafe(24)
    url = f"http://127.0.0.1:{args.port}/"
    print(f"photovault UI: {url}  (Ctrl+C to stop)")
    if not args.no_open:
        threading.Timer(1.0, lambda: webbrowser.open(url)).start()
    uvicorn.run(create_app(token, args.port), host="127.0.0.1", port=args.port, log_level="warning")
    return 0
