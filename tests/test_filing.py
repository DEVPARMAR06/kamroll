import os
import time
from datetime import datetime

from fastapi.testclient import TestClient

from photovault.analysis import run_analysis
from photovault.config import load_config, save_config
from photovault.filing import Move, apply_moves, plan_filing, recover_moves, safe_name
from photovault.manifest import Manifest
from photovault.server.app import create_app
from photovault.verify import verify_ssd
from test_review import add, jpeg, vault  # noqa: F401  (fixture)


def with_filing(on: bool, layout: str | None = None):
    path = os.environ["PHOTOVAULT_CONFIG"]
    cfg = load_config(path)
    cfg.file_trips = on
    if layout:
        cfg.trip_layout = layout
    save_config(cfg, path)
    return cfg


def analysed(cfg):
    m = Manifest(cfg.backup_root)
    run_analysis(m, cfg)
    return m


def dest(m, key):
    return m.db.execute("SELECT dest FROM items WHERE key=?", (key,)).fetchone()[0]


def test_safe_names_work_on_every_filesystem():
    assert safe_name('Mumbai: 30/5 "trip"?') == "Mumbai- 30-5 -trip--"
    assert safe_name("...  ") == "Trip" and safe_name("CON") == "CON_" and len(safe_name("x" * 300)) == 80


def test_nothing_moves_unless_filing_is_on(vault):
    cfg, keys = vault
    cfg = with_filing(False)
    m = analysed(cfg)
    assert plan_filing(m, cfg) == []
    m.close()


def test_trip_photos_move_into_a_trip_folder_and_back(vault):
    cfg, keys = vault
    add_m = Manifest(cfg.backup_root)
    aae = add(add_m, cfg.backup_root, "IMG_100.AAE", b"<plist/>", datetime(2025, 12, 27, 10))    # edit file of trip photo 0
    add_m.close()
    cfg = with_filing(True)
    m = analysed(cfg)
    before = {k: dest(m, k) for k in (keys["t0"], keys["t1"], keys["cam"], aae)}
    moves = plan_filing(m, cfg)
    assert {x.key for x in moves} == {keys["t0"], keys["t1"], keys["t2"], keys["t3"], aae}       # trip photos and the edit file
    assert dest(m, keys["t0"]) == before[keys["t0"]]                                            # planning moves nothing
    res = apply_moves(m, cfg, moves)
    assert res == {"moved": 5, "failed": [], "failed_count": 0}
    folder = "Trips/2025/Udaipur (Dec 2025)"
    assert dest(m, keys["t0"]) == f"{folder}/IMG_100.JPG" and dest(m, aae) == f"{folder}/IMG_100.AAE"
    assert (cfg.backup_root / folder / "IMG_100.JPG").is_file() and not (cfg.backup_root / before[keys["t0"]]).exists()
    assert dest(m, keys["cam"]) == before[keys["cam"]]                                          # other photos stay put
    assert verify_ssd(m, cfg, deep=True, progress=lambda *_: None).ok                           # same bytes, same fingerprints
    assert plan_filing(m, cfg) == []                                                            # settled
    # switching it off puts everything back exactly where it was
    cfg = with_filing(False)
    back = plan_filing(m, cfg)
    assert {x.reason for x in back} == {"back"} and len(back) == 5
    apply_moves(m, cfg, back)
    assert dest(m, keys["t0"]) == before[keys["t0"]] and dest(m, aae) == before[aae]
    assert not (cfg.backup_root / "Trips").exists()                                              # empty folders are tidied away
    assert verify_ssd(m, cfg, deep=True, progress=lambda *_: None).ok
    m.close()


def test_taking_a_photo_out_of_a_trip_sends_it_home(vault):
    cfg, keys = vault
    cfg = with_filing(True)
    m = analysed(cfg)
    original = dest(m, keys["t1"])
    apply_moves(m, cfg, plan_filing(m, cfg))
    from photovault.analysis import remove_from_trip
    remove_from_trip(m.db, [keys["t1"]])
    m.commit()
    moves = plan_filing(m, cfg)
    assert [(x.key, x.reason) for x in moves] == [(keys["t1"], "back")]
    apply_moves(m, cfg, moves)
    assert dest(m, keys["t1"]) == original and dest(m, keys["t0"]).startswith("Trips/")
    m.close()


def test_renaming_a_trip_renames_its_folder_and_origin_survives(vault):
    cfg, keys = vault
    cfg = with_filing(True)
    m = analysed(cfg)
    original = dest(m, keys["t0"])
    apply_moves(m, cfg, plan_filing(m, cfg))
    m.db.execute("UPDATE trips SET name='Mewar with family'")
    m.commit()
    moves = plan_filing(m, cfg)
    assert len(moves) == 4 and all(x.reason == "into-trip" for x in moves)
    apply_moves(m, cfg, moves)
    assert dest(m, keys["t0"]) == "Trips/2025/Mewar with family (Dec 2025)/IMG_100.JPG"
    assert not (cfg.backup_root / "Trips" / "2025" / "Udaipur (Dec 2025)").exists()
    cfg = with_filing(False)
    apply_moves(m, cfg, plan_filing(m, cfg))
    assert dest(m, keys["t0"]) == original                                                       # still goes all the way home
    m.close()


def test_two_files_with_the_same_name_do_not_overwrite_each_other(vault):
    cfg, keys = vault
    extra = Manifest(cfg.backup_root)
    # same file name as trip photo 0 but a different picture, kept in a different folder
    data = jpeg("pink", size=(70, 40), make="Apple")
    (cfg.backup_root / "Elsewhere").mkdir()
    (cfg.backup_root / "Elsewhere" / "IMG_100.JPG").write_bytes(data)
    twin = "IMG_100.JPG|9|1"
    extra.upsert_seen(twin, "/DCIM/x/IMG_100.JPG", "IMG_100.JPG", len(data), 1.0, 1.0)
    import hashlib
    extra.set_done(twin, "Elsewhere/IMG_100.JPG", hashlib.sha256(data).hexdigest(), datetime(2025, 12, 28, 9).timestamp(), "exif", 24.58, 73.68)
    extra.close()
    cfg = with_filing(True)
    m = analysed(cfg)
    apply_moves(m, cfg, plan_filing(m, cfg))
    a, b = dest(m, keys["t0"]), dest(m, twin)
    assert a != b and a.startswith("Trips/") and b.startswith("Trips/")
    assert (cfg.backup_root / a).is_file() and (cfg.backup_root / b).is_file()
    m.close()


def test_a_crash_half_way_is_settled_on_the_next_run(vault):
    cfg, keys = vault
    cfg = with_filing(True)
    m = analysed(cfg)
    mv = next(x for x in plan_filing(m, cfg) if x.key == keys["t2"])
    # the file was renamed and the journal written, then the power went
    m.db.execute("INSERT INTO moves VALUES(?,?,?,?)", (mv.key, mv.src, mv.dst, mv.reason))
    m.commit()
    (cfg.backup_root / mv.dst).parent.mkdir(parents=True, exist_ok=True)
    os.replace(cfg.backup_root / mv.src, cfg.backup_root / mv.dst)
    assert verify_ssd(m, cfg, progress=lambda *_: None).missing == [mv.src]                      # the manifest is stale
    assert recover_moves(m, cfg) == 1
    assert dest(m, mv.key) == mv.dst and verify_ssd(m, cfg, progress=lambda *_: None).ok
    # a journal for a move that never started is simply forgotten
    m.db.execute("INSERT INTO moves VALUES(?,?,?,?)", (keys["cam"], dest(m, keys["cam"]), "Trips/x/nope.jpg", "into-trip"))
    m.commit()
    assert recover_moves(m, cfg) == 0 and m.db.execute("SELECT COUNT(*) FROM moves").fetchone()[0] == 0
    m.close()


def test_a_missing_file_is_reported_and_the_rest_still_move(vault):
    cfg, keys = vault
    cfg = with_filing(True)
    m = analysed(cfg)
    os.remove(cfg.backup_root / dest(m, keys["t0"]))
    res = apply_moves(m, cfg, plan_filing(m, cfg))
    assert res["moved"] == 3 and res["failed_count"] == 1 and "missing" in res["failed"][0]["error"]
    m.close()


def test_filing_through_the_api(vault):
    cfg, keys = vault
    with_filing(True)
    with TestClient(create_app("tk"), headers={"x-photovault-token": "tk"}) as c:
        c.post("/api/analysis/start")
        for _ in range(200):
            if c.get("/api/analysis").json()["state"] == "done":
                break
            time.sleep(0.05)
        p = c.get("/api/organize/preview").json()
        assert p["into"] == 4 and p["back"] == 0 and p["samples"][0]["to"].startswith("Trips/2025/Udaipur")
        assert not (cfg.backup_root / "Trips").exists()                                          # a preview moves nothing
        assert c.post("/api/organize/start").json()["state"] == "running"
        for _ in range(200):
            s = c.get("/api/organize").json()
            if s["state"] != "running":
                break
            time.sleep(0.05)
        assert s["state"] == "done" and s["result"]["moved"] == 4 and s["result"]["ssd_ok"] is True
        assert c.get("/api/organize/preview").json()["total"] == 0
        assert c.get("/api/thumb", params={"key": keys["t0"]}).status_code == 200               # previews follow the file
        assert c.post("/api/organize/cancel").status_code == 409


def test_a_bad_trip_layout_is_refused(vault):
    cfg, keys = vault
    with TestClient(create_app("tk"), headers={"x-photovault-token": "tk"}) as c:
        body = c.get("/api/config").json()["config"]
        for bad in ("Trips/{oops}/{name}", "../{trip_dated}/{name}", "Trips/{name}", "Trips/{trip_dated}"):
            assert c.put("/api/config", json={**body, "trip_layout": bad}).status_code == 422
        assert c.put("/api/config", json={**body, "trip_layout": "Trips/{trip}/{year}-{month}-{day}/{name}", "file_trips": True}).status_code == 200
