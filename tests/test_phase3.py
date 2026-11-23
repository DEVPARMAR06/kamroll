import io
import os
import struct
from datetime import datetime

from PIL import Image

from photovault.copier import run_copy, scan
from photovault.legacy import import_old
from photovault.manifest import Manifest
from photovault.metadata import read_metadata
from photovault.verify import sync_report, verify_ssd
from test_copier import FakeDevice, blob, cfg, manifest  # noqa: F401  (fixtures)


def jpeg_with_exif(taken="2023:05:06 07:08:09", gps=True) -> bytes:
    ex = Image.Exif()
    ex[0x8769] = {36867: taken}
    if gps:
        ex[0x8825] = {1: "N", 2: (19.0, 4.0, 33.6), 3: "E", 4: (72.0, 52.0, 39.6)}
    buf = io.BytesIO()
    Image.new("RGB", (8, 8), "red").save(buf, "JPEG", exif=ex)
    return buf.getvalue()


def atom(typ: bytes, payload: bytes) -> bytes:
    return struct.pack(">I4s", 8 + len(payload), typ) + payload


def mov(creation_utc_mac=0, creationdate=None, iso6709=None, ts=600, dur=0) -> bytes:
    mvhd = atom(b"mvhd", b"\0\0\0\0" + struct.pack(">IIII", creation_utc_mac, 0, ts, dur) + b"\0" * 80)
    meta = b""
    if creationdate or iso6709:
        names, vals = [], []
        if creationdate:
            names.append(b"com.apple.quicktime.creationdate"); vals.append(creationdate.encode())
        if iso6709:
            names.append(b"com.apple.quicktime.location.ISO6709"); vals.append(iso6709.encode())
        keys = atom(b"keys", b"\0\0\0\0" + struct.pack(">I", len(names)) +
                    b"".join(struct.pack(">I4s", 8 + len(n), b"mdta") + n for n in names))
        ilst = atom(b"ilst", b"".join(
            atom(struct.pack(">I", i + 1), atom(b"data", struct.pack(">II", 1, 0) + v)) for i, v in enumerate(vals)))
        meta = atom(b"meta", atom(b"hdlr", b"\0" * 24) + keys + ilst)
    moov = atom(b"moov", mvhd + meta)
    return atom(b"ftyp", b"qt  \0\0\0\0qt  ") + atom(b"mdat", b"\0" * 64) + moov  # moov at the END, like iPhone


def test_exif_date_and_gps(tmp_path):
    f = tmp_path / "a.jpg"; f.write_bytes(jpeg_with_exif())
    m = read_metadata(f)
    assert (m.taken, m.source) == (datetime(2023, 5, 6, 7, 8, 9), "exif")
    assert abs(m.lat - 19.0760) < 1e-3 and abs(m.lon - 72.8777) < 1e-3


def test_quicktime_creationdate_and_location(tmp_path):
    f = tmp_path / "v.mov"
    f.write_bytes(mov(creationdate="2024-08-15T15:42:38+0530", iso6709="+19.0760+072.8777+010.000/"))
    m = read_metadata(f)
    assert (m.taken, m.source) == (datetime(2024, 8, 15, 15, 42, 38), "quicktime")
    assert abs(m.lat - 19.076) < 1e-6 and abs(m.lon - 72.8777) < 1e-6


def test_mvhd_fallback_and_garbage(tmp_path):
    f = tmp_path / "v.mp4"
    f.write_bytes(mov(creation_utc_mac=int(datetime(2022, 1, 2, 12).timestamp()) + 2082844800))
    assert read_metadata(f).source == "mvhd"
    g = tmp_path / "junk.mov"; g.write_bytes(b"not a movie")
    assert read_metadata(g).taken is None  # never raises


def test_filename_date_fallback(tmp_path):
    f = tmp_path / "VID-20240815-WA0001.mp4"; f.write_bytes(b"x")
    m = read_metadata(f)
    assert (m.taken.date().isoformat(), m.source) == ("2024-08-15", "filename")


async def test_exif_beats_filesystem_date(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0001.JPG": jpeg_with_exif()})
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (cfg.backup_root / "2023" / "2023-05" / "IMG_0001.JPG").exists()
    assert manifest.date_sources() == {"exif": 1}


async def test_bogus_filesystem_date_goes_to_undated(cfg, manifest):  # noqa: F811
    p = "/DCIM/100APPLE/IMG_0002.JPG"
    dev = FakeDevice({p: blob(500)})
    dev.times[p] = datetime(2000, 7, 1)  # other-phone copy with a bad date, no EXIF
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (cfg.backup_root / "Undated" / "IMG_0002.JPG").exists()


async def test_sidecar_follows_its_photo(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0003.JPG": jpeg_with_exif(), "/DCIM/100APPLE/IMG_0003.AAE": b"<plist/>"})
    dev.times["/DCIM/100APPLE/IMG_0003.AAE"] = datetime(2026, 1, 1)
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (cfg.backup_root / "2023" / "2023-05" / "IMG_0003.AAE").exists()


async def test_verify_catches_missing_and_corrupt(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/A.JPG": blob(1000, 3), "/DCIM/100APPLE/B.JPG": blob(1000, 5), "/DCIM/100APPLE/C.JPG": blob(1000, 7)})
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert verify_ssd(manifest, cfg, deep=True).ok
    files = sorted(cfg.backup_root.rglob("*.JPG"))
    files[0].unlink()
    data = bytearray(files[1].read_bytes()); data[10] ^= 0xFF; files[1].write_bytes(bytes(data))  # bit rot
    quick = verify_ssd(manifest, cfg, deep=False)
    assert len(quick.missing) == 1 and not quick.corrupt  # quick can't see bit rot
    deep = verify_ssd(manifest, cfg, deep=True)
    assert len(deep.missing) == 1 and len(deep.corrupt) == 1


async def test_sync_report(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/A.JPG": blob(1000)})
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert (await sync_report(dev, manifest, cfg)).in_sync
    dev.files["/DCIM/100APPLE/NEW.JPG"] = blob(50)  # new photo taken after the backup
    rep = await sync_report(dev, manifest, cfg)
    assert not rep.in_sync and len(rep.missing_on_ssd) == 1
    del dev.files["/DCIM/100APPLE/NEW.JPG"], dev.files["/DCIM/100APPLE/A.JPG"]  # deleted from phone
    rep = await sync_report(dev, manifest, cfg)
    assert rep.in_sync and rep.only_on_ssd == 1


async def test_compare_old_classifies_renamed_and_reencoded(cfg, manifest, tmp_path):  # noqa: F811
    import os
    from photovault.legacy import compare_old

    mac = 2082844800
    cap = datetime(2024, 8, 24, 16, 19, 0).timestamp()          # real capture time
    orig = mov(creation_utc_mac=int(cap) + mac, ts=600, dur=600 * 21)   # 21 s original on the phone
    reenc = mov(creation_utc_mac=int(datetime(2026, 10, 30).timestamp()) + mac, ts=600, dur=600 * 21) + b"\0" * 5  # new bytes
    other = mov(creation_utc_mac=int(cap) + mac + 999999, ts=600, dur=600 * 99)

    dev = FakeDevice({"/DCIM/100APPLE/IMG_0001.MOV": orig, "/DCIM/100APPLE/IMG_0002.MOV": blob(300)})
    await scan(dev, manifest, cfg)

    old = tmp_path / "old"
    old.mkdir()
    (old / "IMG_0002.MOV").write_bytes(blob(300))                # same name and size
    (old / "1000010407.MOV").write_bytes(orig)                   # same bytes, renamed on export
    f = old / "2104.MP4"; f.write_bytes(reenc)                   # re-encoded: size differs, mtime keeps capture time
    os.utime(f, (cap, cap))
    (old / "gone.MP4").write_bytes(other)                        # not on the phone
    (old / "._junk.MP4").write_bytes(b"x" * 4096)                # ignored

    rep = await compare_old(dev, manifest, old, cfg, progress=lambda *_: None)
    assert rep.examined == 4
    assert rep.same_name_size == ["IMG_0002.MOV"]
    assert rep.same_size == [("1000010407.MOV", "IMG_0001.MOV")]
    assert rep.converted == [("2104.MP4", "IMG_0001.MOV")]
    assert rep.unmatched == ["gone.MP4"]


async def test_import_old_is_separate_dated_and_idempotent(cfg, manifest, tmp_path):  # noqa: F811
    import os
    from photovault.legacy import import_old

    mac = 2082844800
    cap = datetime(2022, 7, 19, 10, 0, 0).timestamp()
    old = tmp_path / "old"
    old.mkdir()
    (old / "IMG_0002.MOV").write_bytes(blob(300))                       # already in the vault byte-for-byte
    conv = old / "781.MP4"                                              # re-encoded: header says 2026, mtime says 2022
    conv.write_bytes(mov(creation_utc_mac=int(datetime(2026, 10, 30).timestamp()) + mac, ts=600, dur=6000))
    os.utime(conv, (cap, cap))
    for i in range(31):                                                 # import-day batch: date lost
        f = old / f"1000{i:06d}.MP4"
        f.write_bytes(blob(50 + i))
        os.utime(f, (datetime(2024, 8, 15, 12).timestamp(),) * 2)
    before = sorted((p.name, p.stat().st_size) for p in old.iterdir())

    dev = FakeDevice({"/DCIM/100APPLE/IMG_0002.MOV": blob(300)})
    await scan(dev, manifest, cfg)
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)

    copied, skipped, failed = import_old(manifest, cfg, old, progress=lambda *_: None)
    assert (copied, skipped, failed) == (32, 1, 0)
    assert (cfg.backup_root / "2022" / "2022-07" / "781.MP4").exists()   # mtime beat the re-encode stamp
    assert len(list((cfg.backup_root / "Undated").glob("*.MP4"))) == 31
    assert sorted((p.name, p.stat().st_size) for p in old.iterdir()) == before             # old folder untouched
    assert import_old(manifest, cfg, old, progress=lambda *_: None) == (0, 1, 0)           # re-run does nothing
    assert manifest.pending() == []                                                        # never mistaken for phone work
    assert verify_ssd(manifest, cfg, deep=True).ok


async def test_compare_old_finds_slowmo_export(cfg, manifest, tmp_path):  # noqa: F811
    import os
    from photovault.legacy import compare_old

    mac = 2082844800
    cap = datetime(2026, 10, 2, 18, 24, 49).timestamp()
    phone = mov(creation_utc_mac=int(cap) + mac, ts=600, dur=600 * 31)                        # 31 s at capture
    slowmo = mov(creation_utc_mac=int(datetime(2026, 10, 30).timestamp()) + mac, ts=600, dur=600 * 179) + b"\0" * 9
    dev = FakeDevice({"/DCIM/121APPLE/IMG_1851.MOV": phone})
    await scan(dev, manifest, cfg)
    old = tmp_path / "old"; old.mkdir()
    f = old / "IMG_1851.MOV"; f.write_bytes(slowmo); os.utime(f, (cap, cap))                  # slowed export, same name
    rep = await compare_old(dev, manifest, old, cfg, progress=lambda *_: None)
    assert rep.speed_changed == [("IMG_1851.MOV", "IMG_1851.MOV")]
    assert rep.unmatched == [] and rep.converted == []


async def test_reencoded_originals_option(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0001.MOV": blob(100, 3), "/DCIM/100APPLE/IMG_0002.MOV": blob(200, 5)})
    await scan(dev, manifest, cfg)
    linked_key = next(r.key for r in manifest.pending() if r.name == "IMG_0001.MOV")
    manifest.set_link("legacy|x.MP4|1|1", linked_key, "converted")
    manifest.commit()

    cfg.copy_originals_of_reencoded = False                  # user does not want those originals
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert [p.name for p in cfg.backup_root.rglob("IMG_000*.MOV")] == ["IMG_0002.MOV"]
    rep = await sync_report(dev, manifest, cfg)
    assert rep.skipped_by_choice == 1 and rep.in_sync        # a choice is not a gap

    cfg.copy_originals_of_reencoded = True                   # changed their mind: it comes back
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)
    assert sorted(p.name for p in cfg.backup_root.rglob("IMG_000*.MOV")) == ["IMG_0001.MOV", "IMG_0002.MOV"]


async def test_only_reencoded_originals_flag(cfg, manifest):  # noqa: F811
    dev = FakeDevice({"/DCIM/100APPLE/IMG_0001.MOV": blob(100, 3), "/DCIM/100APPLE/IMG_0002.MOV": blob(200, 5)})
    await scan(dev, manifest, cfg)
    manifest.set_link("legacy|x.MP4|1|1", next(r.key for r in manifest.pending() if r.name == "IMG_0002.MOV"), "speed-changed")
    manifest.commit()
    done, _ = await run_copy(dev, manifest, cfg, progress=lambda *_: None, only_reencoded_originals=True)
    assert done == 1 and [r.name for r in manifest.pending()] == ["IMG_0001.MOV"]


async def test_old_export_never_takes_the_phone_originals_name(cfg, manifest, tmp_path):  # noqa: F811
    mac = 2082844800
    cap = datetime(2026, 10, 2, 18, 24, 49).timestamp()
    old = tmp_path / "old"
    old.mkdir()
    f = old / "IMG_1851.MOV"
    f.write_bytes(mov(creation_utc_mac=int(cap) + mac, ts=600, dur=600 * 179) + b"\0" * 9)
    os.utime(f, (cap, cap))
    dev = FakeDevice({"/DCIM/121APPLE/IMG_1851.MOV": blob(400)})
    dev.times["/DCIM/121APPLE/IMG_1851.MOV"] = datetime(2026, 10, 2, 18, 24, 49)
    await scan(dev, manifest, cfg)
    import_old(manifest, cfg, old, progress=lambda *_: None)          # old export arrives first
    await run_copy(dev, manifest, cfg, progress=lambda *_: None)      # then the phone original
    folder = cfg.backup_root / "2026" / "2026-10"
    assert sorted(p.name for p in folder.iterdir()) == ["IMG_1851 (old export).MOV", "IMG_1851.MOV"]
    assert (folder / "IMG_1851.MOV").read_bytes() == blob(400)


async def test_relocate_moves_out_of_fromoldbackup(cfg, manifest, tmp_path):  # noqa: F811
    from photovault.legacy import relocate_old
    old = tmp_path / "old"
    old.mkdir()
    (old / "a.MP4").write_bytes(b"x" * 10)
    import_old(manifest, cfg, old, progress=lambda *_: None)
    row = manifest.db.execute("SELECT key, dest FROM items WHERE key LIKE 'legacy|%'").fetchone()
    cur = cfg.backup_root / row["dest"]
    legacy = cfg.backup_root / "FromOldBackup" / "2026" / "2026-11" / "a.MP4"      # the earlier layout
    legacy.parent.mkdir(parents=True)
    os.replace(cur, legacy)
    manifest.db.execute("UPDATE items SET dest=? WHERE key=?", ("FromOldBackup/2026/2026-11/a.MP4", row["key"]))
    manifest.commit()
    assert relocate_old(manifest, cfg, progress=lambda *_: None) == (1, 0)
    assert cur.exists() and not (cfg.backup_root / "FromOldBackup").exists()
    assert verify_ssd(manifest, cfg, deep=True).ok
