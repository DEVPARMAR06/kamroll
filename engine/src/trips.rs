//! Find trips from where and when photos were taken. Offline: place names come from a bundled city list.

use chrono::{Local, NaiveDate, TimeZone};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const GAP_DAYS: i64 = 3;
pub const MIN_AWAY_ITEMS: usize = 3;
const GRID: f64 = 0.1;

pub fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let a = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * ((lon2 - lon1).to_radians() / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub fn local_date(ts: f64) -> NaiveDate {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    Local.timestamp_opt(secs, nanos).earliest().map(|d| d.date_naive()).unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
}

/// Python's round(x, 1).
fn round1(x: f64) -> f64 {
    (x * 10.0).round_ties_even() / 10.0
}

/// Home is where photos were taken on the most different days.
pub fn detect_home(points: &[(NaiveDate, f64, f64)]) -> Option<(f64, f64)> {
    let mut order: Vec<(i64, i64)> = vec![];
    let mut cells: HashMap<(i64, i64), Vec<(NaiveDate, f64, f64)>> = HashMap::new();
    for &(d, lat, lon) in points {
        let k = ((lat / GRID).round_ties_even() as i64, (lon / GRID).round_ties_even() as i64);
        if !cells.contains_key(&k) {
            order.push(k);
        }
        cells.entry(k).or_default().push((d, lat, lon));
    }
    let mut best: Option<&Vec<(NaiveDate, f64, f64)>> = None;
    let mut best_days = 0;
    for k in &order {
        let pts = &cells[k];
        let days = pts.iter().map(|p| p.0).collect::<HashSet<_>>().len();
        if best.is_none() || days > best_days {
            best = Some(pts);
            best_days = days;
        }
    }
    let b = best?;
    let n = b.len() as f64;
    Some((b.iter().map(|p| p.1).sum::<f64>() / n, b.iter().map(|p| p.2).sum::<f64>() / n))
}

type City = (f64, f64, String, String);

pub struct Gazetteer {
    countries: HashMap<String, String>,
    grid: HashMap<(i32, i32), Vec<City>>,
}

impl Gazetteer {
    pub fn new() -> Gazetteer {
        let mut countries = HashMap::new();
        for l in include_str!("../data/countries.tsv").lines() {
            if let Some((k, v)) = l.split_once('\t') {
                countries.insert(k.to_string(), v.to_string());
            }
        }
        let mut grid: HashMap<(i32, i32), Vec<City>> = HashMap::new();
        for l in include_str!("../data/cities.tsv").lines() {
            let p: Vec<&str> = l.split('\t').collect();
            if p.len() < 4 {
                continue;
            }
            let (Ok(lat), Ok(lon)) = (p[0].parse::<f64>(), p[1].parse::<f64>()) else { continue };
            grid.entry((lat as i32, lon as i32)).or_default().push((lat, lon, p[2].to_string(), p[3].to_string()));
        }
        Gazetteer { countries, grid }
    }

    /// (name, country_code, distance_km) of the nearest city within `max_km`.
    pub fn nearest(&self, lat: f64, lon: f64, max_km: f64) -> Option<(String, String, f64)> {
        let mut best: Option<(String, String, f64)> = None;
        let (cy, cx) = (lat as i32, lon as i32);
        for dy in -1..=1 {
            for dx in -1..=1 {
                for (clat, clon, name, cc) in self.grid.get(&(cy + dy, cx + dx)).into_iter().flatten() {
                    let d = haversine(lat, lon, *clat, *clon);
                    if best.as_ref().map(|b| d < b.2).unwrap_or(true) {
                        best = Some((name.clone(), cc.clone(), d));
                    }
                }
            }
        }
        best.filter(|b| b.2 <= max_km)
    }

    pub fn country(&self, code: &str) -> String {
        self.countries.get(code).cloned().unwrap_or_else(|| code.to_string())
    }
}

pub struct Trip {
    pub id: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub keys: Vec<String>,
    pub max_km: f64,
    pub lat: f64,
    pub lon: f64,
    pub places: Vec<String>,
    pub auto_name: String,
}

impl Trip {
    pub fn days(&self) -> i64 {
        (self.end - self.start).num_days() + 1
    }
}

pub struct Item {
    pub key: String,
    pub taken: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub fn detect_trips(items: &[Item], home: (f64, f64), radius_km: f64, gaz: Option<&Gazetteer>) -> Vec<Trip> {
    let mut away: BTreeMap<NaiveDate, Vec<(f64, f64, f64)>> = BTreeMap::new();
    for it in items {
        let (Some(t), Some(lat), Some(lon)) = (it.taken, it.lat, it.lon) else { continue };
        let km = haversine(home.0, home.1, lat, lon);
        if km > radius_km {
            away.entry(local_date(t)).or_default().push((lat, lon, km));
        }
    }
    let mut groups: Vec<Vec<NaiveDate>> = vec![];
    for &d in away.keys() {
        match groups.last_mut() {
            Some(g) if (d - *g.last().unwrap()).num_days() <= GAP_DAYS => g.push(d),
            _ => groups.push(vec![d]),
        }
    }
    let home_country = gaz.and_then(|g| g.nearest(home.0, home.1, 100.0)).map(|h| h.1);
    let mut trips = vec![];
    for days in groups {
        let pts: Vec<(f64, f64, f64)> = days.iter().flat_map(|d| away[d].iter().copied()).collect();
        if pts.len() < MIN_AWAY_ITEMS {
            continue;
        }
        let n = pts.len() as f64;
        let (start, end) = (days[0], *days.last().unwrap());
        let keys = items
            .iter()
            .filter(|it| it.taken.map(|x| (start..=end).contains(&local_date(x))).unwrap_or(false))
            .map(|it| it.key.clone())
            .collect();
        let mut t = Trip {
            id: start.format("%Y-%m-%d").to_string(),
            start,
            end,
            keys,
            max_km: pts.iter().map(|p| p.2).fold(f64::MIN, f64::max),
            lat: pts.iter().map(|p| p.0).sum::<f64>() / n,
            lon: pts.iter().map(|p| p.1).sum::<f64>() / n,
            places: vec![],
            auto_name: String::new(),
        };
        if let Some(g) = gaz {
            let mut names: Vec<(String, usize)> = vec![];
            let mut first_country: Option<String> = None;
            for &(lat, lon, _) in &pts {
                if let Some((name, cc, _)) = g.nearest(round1(lat), round1(lon), 60.0) {
                    match names.iter_mut().find(|(n, _)| *n == name) {
                        Some(e) => e.1 += 1,
                        None => names.push((name, 1)),
                    }
                    first_country.get_or_insert(cc);
                }
            }
            names.sort_by(|a, b| b.1.cmp(&a.1)); // stable: ties keep first-seen order, like Counter.most_common
            t.places = names.iter().take(4).map(|(n, _)| n.clone()).collect();
            if let Some(first) = t.places.first() {
                let mut label = first.clone();
                if let (Some(fc), Some(hc)) = (&first_country, &home_country) {
                    if fc != hc {
                        label = format!("{label}, {}", g.country(fc));
                    }
                }
                t.auto_name = label;
            }
        }
        trips.push(t);
    }
    trips
}

pub const MERGE_GAP_DAYS: i64 = 14; // a trip the person named absorbs photos from the same place this soon after (or before) it
pub const MERGE_KM: f64 = 50.0;

/// Photos from the same place, a few days apart from a trip the person has named, belong to that trip.
/// Trips nobody has named are left alone, and a visit weeks later stays its own trip.
pub fn merge_into_named(trips: Vec<Trip>, named: &[(NaiveDate, NaiveDate)], items: &[Item]) -> Vec<Trip> {
    let claimed = |t: &Trip| named.iter().any(|(a, b)| *a <= t.end && *b >= t.start);
    let mut sorted = trips;
    sorted.sort_by_key(|t| t.start);
    let mut out: Vec<Trip> = vec![];
    for t in sorted {
        let merge = match out.last() {
            Some(prev) => (claimed(prev) || claimed(&t)) && (t.start - prev.end).num_days() <= MERGE_GAP_DAYS && haversine(prev.lat, prev.lon, t.lat, t.lon) <= MERGE_KM,
            None => false,
        };
        if merge {
            let prev = out.last_mut().unwrap();
            prev.end = prev.end.max(t.end);
            let (s, e) = (prev.start, prev.end);
            prev.keys = items.iter().filter(|it| it.taken.map(|x| (s..=e).contains(&local_date(x))).unwrap_or(false)).map(|it| it.key.clone()).collect();
            prev.max_km = prev.max_km.max(t.max_km);
            prev.lat = (prev.lat + t.lat) / 2.0;
            prev.lon = (prev.lon + t.lon) / 2.0;
            for p in t.places {
                if !prev.places.contains(&p) {
                    prev.places.push(p);
                }
            }
            prev.places.truncate(4);
            if prev.auto_name.is_empty() {
                prev.auto_name = t.auto_name;
            }
        } else {
            out.push(t);
        }
    }
    out
}
