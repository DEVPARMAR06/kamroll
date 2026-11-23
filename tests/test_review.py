import hashlib
import io
import time
from datetime import date, datetime, timedelta

import pytest
from fastapi.testclient import TestClient
from PIL import Image

from photovault.analysis import run_analysis
from photovault.config import Config, save_config
from photovault.manifest import Manifest
from photovault.server.app import create_app
from photovault.tagging import Facts, classify, read_facts
from photovault.trips import Gazetteer, detect_home, detect_trips

HOME = (19.06, 73.07)
UDAIPUR = (24.58, 73.68)


def jpeg(color="red", size=(64, 48), make=None) -> bytes:
    ex = Image.Exif()
    if make:
        ex[271], ex[272] = make, "iPhone 15 Pro"
    buf = io.BytesIO()
    Image.new("RGB", size, color).save(buf, "JPEG", exif=ex)
    return buf.getvalue()


# ---- tagging rules ----------------------------------------------------------------------

@pytest.mark.parametrize("name,facts,src,tag", [
    ("IMG-20240815-WA0017.jpg", Facts(), None, "social"),
    ("IMG_2147.PNG", Facts(width=1179, height=2556), None, "screenshot"),
    ("IMG_9000.PNG", Facts(width=900, height=700), None, "screenshot"),
    ("EFCCC206-47F7-47DC-919E-A80033ECFFC5.MP4", Facts(), "mvhd", "social"),
    ("1000019549.MP4", Facts(), "mvhd", "social"),
    ("IMG_4746 2.MOV", Facts(), "quicktime", "duplicate"),
    ("IMG_9820 (old export).mov", Facts(), "quicktime", "duplicate"),
    ("IMG_0001.JPG", Facts(width=4032, height=3024, make="Apple", model="iPhone 15 Pro", luma=90), "exif", "memory"),
    ("IMG_0002.JPG", Facts(make="Apple", luma=2.0), "exif", "junk"),
    ("IMG_9820.MOV", Facts(), "quicktime", "memory"),
])
def test_rules(name, facts, src, tag):
    v = classify(name, facts, src)
    assert v.tag == tag and v.reasons and 0 < v.conf <= 100


def test_screenshot_size_with_camera_data_is_not_a_screenshot():
    v = classify("IMG_5.JPG", Facts(width=1179, height=2556, make="Apple"), "exif")
    assert v.tag == "memory"


def test_read_facts_from_real_files(tmp_path):
    (tmp_path / "a.jpg").write_bytes(jpeg("black", make="Apple"))
    (tmp_path / "b.jpg").write_bytes(jpeg("white", size=(1179, 2556)))
    a, b = read_facts(tmp_path / "a.jpg"), read_facts(tmp_path / "b.jpg")
    assert a.make == "Apple" and a.luma < 8
    assert (b.width, b.height) == (1179, 2556) and b.make is None and b.luma > 200
    assert read_facts(tmp_path / "missing.jpg").width is None            # never raises


# ---- trips ------------------------------------------------------------------------------

def _item(key, day, lat=None, lon=None, hour=12):
    return {"key": key, "taken": datetime.combine(day, datetime.min.time()).replace(hour=hour).timestamp(),
            "lat": lat, "lon": lon}


def make_year():
    start = date(2025, 11, 1)
    items = [_item(f"h{i}", start + timedelta(days=i), *HOME) for i in range(50)]          # 50 days at home
    trip_days = [date(2025, 12, 26) + timedelta(days=i) for i in range(5)]
    items += [_item(f"u{i}", d, UDAIPUR[0] + i * 0.001, UDAIPUR[1], hour=10 + j)
              for i, d in enumerate(trip_days) for j in range(2)]                            # 10 geotagged
    items += [_item("nogps", date(2025, 12, 28), hour=20)]                                   # same days, no GPS
    items += [_item("stray", date(2025, 11, 20), 28.6, 77.2)]                                # one far geotag
    return items


def test_home_is_where_most_days_are():
    pts = [(datetime.fromtimestamp(i["taken"]).date(), i["lat"], i["lon"]) for i in make_year() if i["lat"]]
    lat, lon = detect_home(pts)
    assert abs(lat - HOME[0]) < 0.05 and abs(lon - HOME[1]) < 0.05


def test_trip_detection_dates_members_and_name():
    trips = detect_trips(make_year(), HOME, 60, Gazetteer())
    assert len(trips) == 1                                                  # the single stray geotag is ignored
    t = trips[0]
    assert (t.start, t.end, t.days) == (date(2025, 12, 26), date(2025, 12, 30), 5)
    assert "nogps" in t.keys and len(t.keys) == 11                          # photos without GPS join by date
    assert t.auto_name.startswith("Udaipur") and t.max_km > 500


def test_two_trips_far_apart_stay_separate():
    items = make_year() + [_item(f"g{i}", date(2026, 2, 10) + timedelta(days=i), 15.5, 73.8) for i in range(4)]
    names = [t.auto_name for t in detect_trips(items, HOME, 60, Gazetteer())]
    assert len(names) == 2


# ---- analysis and the API ----------------------------------------------------------------

def add(m: Manifest, root, name, data, taken, lat=None, lon=None, src="exif", legacy=False):
    key = ("legacy|" if legacy else "") + f"{name}|{len(data)}|{int(taken.timestamp())}"
    rel = f"{taken:%Y}/{taken:%Y-%m}/{name}"
    (root / rel).parent.mkdir(parents=True, exist_ok=True)
    (root / rel).write_bytes(data)
    m.upsert_seen(key, "/DCIM/x/" + name, name, len(data), taken.timestamp(), taken.timestamp())
    m.set_done(key, rel, hashlib.sha256(data).hexdigest(), taken.timestamp(), src, lat, lon)
    return key


@pytest.fixture
def vault(tmp_path, monkeypatch):
    cfg = Config(ssd_root=tmp_path, backup_dir="V", free_space_margin_gb=0)
    cfg_path = tmp_path / "config.toml"
    save_config(cfg, cfg_path)
    monkeypatch.setenv("PHOTOVAULT_CONFIG", str(cfg_path))
    m = Manifest(cfg.backup_root)
    t = datetime(2025, 12, 27, 10)
    keys = {
        "cam": add(m, cfg.backup_root, "IMG_0001.JPG", jpeg("red", make="Apple"), datetime(2025, 3, 1, 9)),
        "wa": add(m, cfg.backup_root, "IMG-20240815-WA0017.jpg", jpeg("blue"), datetime(2024, 8, 15)),
        "black": add(m, cfg.backup_root, "IMG_0003.JPG", jpeg("black", make="Apple"), datetime(2025, 4, 1)),
        "orig": add(m, cfg.backup_root, "IMG_0004.JPG", jpeg("green", make="Apple"), datetime(2025, 5, 1)),
        "copy": add(m, cfg.backup_root, "IMG_0004 copy.JPG", jpeg("green", make="Apple"), datetime(2025, 5, 1), legacy=True),
        "mov": add(m, cfg.backup_root, "IMG_0005.MOV", b"not really a video", datetime(2025, 6, 1), src="quicktime"),
        "aae": add(m, cfg.backup_root, "IMG_0001.AAE", b"<plist/>", datetime(2025, 3, 1, 9)),
    }
    for d in range(4):                                                      # a trip, with GPS
        keys[f"t{d}"] = add(m, cfg.backup_root, f"IMG_10{d}.JPG", jpeg("orange", size=(64 + d, 48), make="Apple"), t + timedelta(days=d), *UDAIPUR)
    for d in range(12):                                                     # home
        add(m, cfg.backup_root, f"IMG_20{d:02d}.JPG", jpeg("gray", size=(80 + d, 48), make="Apple"), datetime(2025, 11, 1) + timedelta(days=d), *HOME)
    m.close()
    return cfg, keys


def test_analysis_tags_everything_and_keeps_user_choices(vault):
    cfg, keys = vault
    m = Manifest(cfg.backup_root)
    res = run_analysis(m, cfg)
    tag = lambda k: m.db.execute("SELECT tag FROM tags WHERE key=?", (keys[k],)).fetchone()[0]  # noqa: E731
    assert tag("wa") == "social" and tag("black") == "junk" and tag("copy") == "duplicate"
    assert tag("orig") == "memory" and tag("cam") == "memory" and tag("t1") == "trip"
    assert m.db.execute("SELECT COUNT(*) FROM tags WHERE key=?", (keys["aae"],)).fetchone()[0] == 0   # sidecars are not tagged
    assert res["trips"] == 1 and abs(res["home"][0] - HOME[0]) < 0.1
    m.db.execute("UPDATE tags SET user_tag='junk' WHERE key=?", (keys["cam"],))
    m.db.execute("UPDATE trips SET name='Mewar holiday'")
    m.commit()
    run_analysis(m, cfg)                                                    # a re-run keeps what the person decided
    assert m.db.execute("SELECT user_tag FROM tags WHERE key=?", (keys["cam"],)).fetchone()[0] == "junk"
    assert m.db.execute("SELECT name FROM trips").fetchone()[0] == "Mewar holiday"
    m.close()


def test_review_and_trips_api(vault):
    cfg, keys = vault
    with TestClient(create_app("tk"), headers={"x-photovault-token": "tk"}) as c:
        assert c.get("/api/review/summary").json()["analyzed"] is False
        assert c.post("/api/analysis/start").json()["state"] in ("running", "done")
        for _ in range(200):
            if c.get("/api/analysis").json()["state"] == "done":
                break
            time.sleep(0.05)
        assert c.get("/api/analysis").json()["state"] == "done"
        s = c.get("/api/review/summary").json()
        assert s["analyzed"] and s["counts"]["junk"] == 1 and s["counts"]["trip"] == 4
        junk = c.get("/api/review/items", params={"tag": "junk"}).json()
        assert junk["total"] == 1 and junk["items"][0]["reasons"] and junk["items"][0]["has_thumb"]
        # thumbnails: an image works, a video has no preview, a forged key is refused
        img = c.get("/api/thumb", params={"key": keys["cam"]})
        assert img.status_code == 200 and img.headers["content-type"] == "image/jpeg" and img.content[:2] == b"\xff\xd8"
        assert c.get("/api/thumb", params={"key": keys["mov"]}).status_code == 404
        assert c.get("/api/thumb", params={"key": "../../etc/passwd"}).status_code == 404
        # the person's choices
        assert c.post("/api/review/tag", json={"keys": [keys["cam"]], "tag": "junk"}).json()["changed"] == 1
        assert c.post("/api/review/tag", json={"keys": [keys["cam"]], "tag": "nonsense"}).status_code == 422
        assert c.get("/api/review/summary").json()["counts"]["junk"] == 2
        assert c.post("/api/review/tag", json={"keys": [keys["wa"]]}).status_code == 200       # confirm as suggested
        assert c.get("/api/review/summary").json()["confirmed"] == 2
        # trips
        trips = c.get("/api/trips").json()
        assert len(trips["trips"]) == 1 and trips["trips"][0]["days"] == 4 and trips["trips"][0]["name"].startswith("Udaipur")
        tid = trips["trips"][0]["id"]
        assert c.put(f"/api/trips/{tid}", json={"name": "Udaipur with family"}).status_code == 200
        assert c.get("/api/trips").json()["trips"][0]["name"] == "Udaipur with family"
        assert c.put("/api/trips/1999-01-01", json={"name": "x"}).status_code == 404
        assert c.get("/api/review/items", params={"trip": tid}).json()["total"] == 4


# ---- video previews ----------------------------------------------------------------------

def make_video(path, color="red", w=96, h=64, frames=12):
    import av

    with av.open(str(path), "w") as out:
        s = out.add_stream("mpeg4", rate=10)
        s.width, s.height, s.pix_fmt = w, h, "yuv420p"
        for _ in range(frames):
            for pkt in s.encode(av.VideoFrame.from_image(Image.new("RGB", (w, h), color))):
                out.mux(pkt)
        for pkt in s.encode():
            out.mux(pkt)


def test_video_thumbnail_is_a_jpeg_and_junk_is_refused(tmp_path):
    from photovault.tagging import make_thumbnail, video_previews_available

    assert video_previews_available()
    make_video(tmp_path / "v.mp4")
    data = make_thumbnail(tmp_path / "v.mp4", 64)
    assert data and data[:2] == b"\xff\xd8"
    im = Image.open(io.BytesIO(data))
    assert max(im.size) == 64 and im.size[0] > im.size[1]                  # landscape stays landscape
    (tmp_path / "bad.mov").write_bytes(b"not a video")
    assert make_thumbnail(tmp_path / "bad.mov") is None                    # never raises


def test_api_serves_video_previews(vault):
    cfg, keys = vault
    m = Manifest(cfg.backup_root)
    key = add(m, cfg.backup_root, "IMG_7777.MOV", b"", datetime(2025, 7, 1), src="quicktime")
    make_video(cfg.backup_root / "2025" / "2025-07" / "IMG_7777.MOV")
    m.close()
    with TestClient(create_app("tk"), headers={"x-photovault-token": "tk"}) as c:
        r = c.get("/api/thumb", params={"key": key, "size": 128})
        assert r.status_code == 200 and r.content[:2] == b"\xff\xd8"


def test_trip_items_come_in_time_order_and_can_be_spread(vault):
    cfg, keys = vault
    with TestClient(create_app("tk"), headers={"x-photovault-token": "tk"}) as c:
        c.post("/api/analysis/start")
        for _ in range(200):
            if c.get("/api/analysis").json()["state"] == "done":
                break
            time.sleep(0.05)
        tid = c.get("/api/trips").json()["trips"][0]["id"]
        ordered = c.get("/api/review/items", params={"trip": tid, "order": "time"}).json()["items"]
        times = [i["taken"] for i in ordered]
        assert len(times) == 4 and times == sorted(times)
        picked = c.get("/api/review/items", params={"trip": tid, "tag": "trip", "order": "time", "limit": 2, "spread": True}).json()
        assert picked["total"] == 4 and [i["key"] for i in picked["items"]] == [ordered[0]["key"], ordered[2]["key"]]
