"""Find trips from where and when photos were taken. Offline: place names come from a bundled city list."""
from __future__ import annotations

import math
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta

GAP_DAYS = 3          # away days this close together belong to one trip
MIN_AWAY_ITEMS = 3    # ignore a single stray geotag
GRID = 0.1            # degrees (about 11 km) for home detection


def haversine(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    r = 6371.0
    p1, p2 = math.radians(lat1), math.radians(lat2)
    a = math.sin((p2 - p1) / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(math.radians(lon2 - lon1) / 2) ** 2
    return 2 * r * math.asin(math.sqrt(a))


def detect_home(points: list[tuple[date, float, float]]) -> tuple[float, float] | None:
    """Home is where photos were taken on the most different days."""
    cells: dict[tuple[int, int], list] = defaultdict(list)
    for d, lat, lon in points:
        cells[(round(lat / GRID), round(lon / GRID))].append((d, lat, lon))
    if not cells:
        return None
    best = max(cells.values(), key=lambda pts: len({p[0] for p in pts}))
    return (sum(p[1] for p in best) / len(best), sum(p[2] for p in best) / len(best))


class Gazetteer:
    """Nearest city for a coordinate, from the geonamescache data (cities over 15,000 people)."""

    def __init__(self) -> None:
        import geonamescache

        g = geonamescache.GeonamesCache()
        self.countries = {k: v["name"] for k, v in g.get_countries().items()}
        self.grid: dict[tuple[int, int], list] = defaultdict(list)
        for c in g.get_cities().values():
            self.grid[(int(c["latitude"]), int(c["longitude"]))].append(
                (c["latitude"], c["longitude"], c["name"], c["countrycode"], c["population"]))

    def nearest(self, lat: float, lon: float, max_km: float = 60.0):
        """(name, country_code, distance_km) or None."""
        best = None
        cy, cx = int(lat), int(lon)
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                for clat, clon, name, cc, _pop in self.grid.get((cy + dy, cx + dx), ()):
                    d = haversine(lat, lon, clat, clon)
                    if best is None or d < best[2]:
                        best = (name, cc, d)
        return best if best and best[2] <= max_km else None


@dataclass
class Trip:
    id: str                      # the first day, ISO
    start: date
    end: date
    keys: list[str] = field(default_factory=list)
    max_km: float = 0.0
    lat: float = 0.0
    lon: float = 0.0
    places: list[str] = field(default_factory=list)
    auto_name: str = ""

    @property
    def days(self) -> int:
        return (self.end - self.start).days + 1


def detect_trips(items: list[dict], home: tuple[float, float], radius_km: float,
                 gazetteer: Gazetteer | None = None) -> list[Trip]:
    """items: dicts with key, taken (epoch), lat, lon (lat/lon may be None). Returns trips, oldest first."""
    away: dict[date, list[tuple[float, float, float]]] = defaultdict(list)
    for it in items:
        if it.get("taken") is None or it.get("lat") is None:
            continue
        d = datetime.fromtimestamp(it["taken"]).date()
        km = haversine(home[0], home[1], it["lat"], it["lon"])
        if km > radius_km:
            away[d].append((it["lat"], it["lon"], km))
    groups: list[list[date]] = []
    for d in sorted(away):
        if groups and (d - groups[-1][-1]).days <= GAP_DAYS:
            groups[-1].append(d)
        else:
            groups.append([d])
    home_country = None
    if gazetteer:
        h = gazetteer.nearest(home[0], home[1], 100)
        home_country = h[1] if h else None
    trips = []
    for days in groups:
        pts = [p for d in days for p in away[d]]
        if len(pts) < MIN_AWAY_ITEMS:
            continue
        t = Trip(id=days[0].isoformat(), start=days[0], end=days[-1],
                 max_km=max(p[2] for p in pts), lat=sum(p[0] for p in pts) / len(pts), lon=sum(p[1] for p in pts) / len(pts))
        t.keys = [it["key"] for it in items
                  if it.get("taken") is not None and t.start <= datetime.fromtimestamp(it["taken"]).date() <= t.end]
        if gazetteer:
            names: Counter = Counter()
            first_country = None
            for lat, lon, _ in pts:
                hit = gazetteer.nearest(round(lat, 1), round(lon, 1))
                if hit:
                    names[hit[0]] += 1
                    first_country = first_country or hit[1]
            t.places = [n for n, _ in names.most_common(4)]
            if t.places:
                label = t.places[0]
                if first_country and home_country and first_country != home_country:
                    label += ", " + gazetteer.countries.get(first_country, first_country)
                t.auto_name = label
        trips.append(t)
    return trips
