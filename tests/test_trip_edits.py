import time
from datetime import datetime

from fastapi.testclient import TestClient

from photovault.server.app import create_app
from test_review import add, jpeg, vault  # noqa: F401  (fixture)


def analyse(c):
    c.post("/api/analysis/start")
    for _ in range(300):
        if c.get("/api/analysis").json()["state"] == "done":
            return
        time.sleep(0.05)
    raise AssertionError("analysis did not finish")


def client():
    return TestClient(create_app("tk"), headers={"x-photovault-token": "tk"})


def trip(c):
    return c.get("/api/trips").json()["trips"][0]


def items(c, **params):
    return c.get("/api/review/items", params={"limit": 200, **params}).json()["items"]


def test_add_and_remove_photos_and_they_survive_reanalysis(vault):
    cfg, keys = vault
    with client() as c:
        analyse(c)
        tid = trip(c)["id"]
        assert trip(c)["items"] == 4
        # add a photo that was not on the trip
        assert c.post(f"/api/trips/{tid}/items", json={"keys": [keys["cam"]]}).json() == {"changed": 1}
        added = next(i for i in items(c, trip=tid) if i["key"] == keys["cam"])
        assert added["tag"] == "trip" and "Added to the trip by you" in added["reasons"]
        assert trip(c)["items"] == 5
        # take a trip photo out
        assert c.post(f"/api/trips/{tid}/remove", json={"keys": [keys["t0"]]}).json() == {"changed": 1}
        gone = next(i for i in items(c) if i["key"] == keys["t0"])
        assert gone["tag"] == "memory" and gone["trip_id"] is None
        assert c.post(f"/api/trips/{tid}/remove", json={"keys": [keys["cam"]]}).json()["changed"] == 1
        c.post(f"/api/trips/{tid}/items", json={"keys": [keys["cam"]]})              # put it back
        # a full re-analysis keeps both decisions
        analyse(c)
        members = {i["key"] for i in items(c, trip=tid)}
        assert keys["cam"] in members and keys["t0"] not in members
        assert next(i for i in items(c) if i["key"] == keys["t0"])["tag"] == "memory"
        assert trip(c)["items"] == 4


def test_a_trip_of_your_own_with_dates_and_a_name(vault):
    cfg, keys = vault
    with client() as c:
        analyse(c)
        made = c.post("/api/trips", json={"name": "Train to Pune", "start": "2025-03-01", "end": "2025-03-01"}).json()
        assert made["id"].startswith("c-") and made["files"] == 1          # the one photo taken that day
        names = {t["name"] for t in c.get("/api/trips").json()["trips"]}
        assert "Train to Pune" in names
        assert c.put(f"/api/trips/{made['id']}", json={"name": "Pune by train"}).status_code == 200
        analyse(c)                                                           # still there, still named, still full
        mine = next(t for t in c.get("/api/trips").json()["trips"] if t["id"] == made["id"])
        assert mine["name"] == "Pune by train" and mine["items"] == 1
        assert c.post("/api/trips", json={"name": "x", "start": "2025-03-05", "end": "2025-03-01"}).status_code == 422
        assert c.post("/api/trips", json={"name": "x", "start": "nope", "end": "2025-03-01"}).status_code == 422


def test_delete_a_trip_releases_its_photos_for_good(vault):
    cfg, keys = vault
    with client() as c:
        analyse(c)
        tid = trip(c)["id"]
        assert c.delete(f"/api/trips/{tid}").json() == {"ok": True, "released": 4}
        assert c.get("/api/trips").json()["trips"] == []
        assert all(i["tag"] == "memory" for i in items(c) if i["key"] in (keys["t0"], keys["t1"]))
        analyse(c)                                                           # it is not found again
        assert c.get("/api/trips").json()["trips"] == []
        assert c.delete(f"/api/trips/{tid}").status_code == 404


def test_a_trip_emptied_by_you_disappears_and_own_trip_can_be_deleted(vault):
    cfg, keys = vault
    with client() as c:
        analyse(c)
        tid = trip(c)["id"]
        c.post(f"/api/trips/{tid}/remove", json={"keys": [keys[f"t{d}"] for d in range(4)]})
        assert c.get("/api/trips").json()["trips"] == []
        analyse(c)
        assert c.get("/api/trips").json()["trips"] == []
        made = c.post("/api/trips", json={"name": "Weekend", "start": "2025-05-01", "end": "2025-05-02"}).json()["id"]
        assert c.delete(f"/api/trips/{made}").json()["ok"] is True
        analyse(c)
        assert all(t["id"] != made for t in c.get("/api/trips").json()["trips"])


def test_unknown_trip_and_empty_key_lists_are_refused(vault):
    cfg, keys = vault
    with client() as c:
        analyse(c)
        assert c.post("/api/trips/1999-01-01/items", json={"keys": [keys["cam"]]}).status_code == 404
        assert c.post(f"/api/trips/{trip(c)['id']}/items", json={"keys": []}).status_code == 422
