//! Tag every backed-up file and detect trips. Reads only the SSD copy; writes only derived tables.

use crate::config::Config;
use crate::tagging::{self, Facts, Verdict};
use crate::trips::{self, Gazetteer, Item};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};

struct Row {
    key: String,
    name: String,
    dest: String,
    taken: Option<f64>,
    date_src: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    sha256: Option<String>,
}

fn cached_facts(db: &Connection, key: &str) -> Option<Facts> {
    db.query_row("SELECT width,height,make,model,software,luma,sharp FROM facts WHERE key=?", [key], |r| {
        Ok(Facts { width: r.get(0)?, height: r.get(1)?, make: r.get(2)?, model: r.get(3)?, software: r.get(4)?, luma: r.get(5)?, sharp: r.get(6)? })
    })
    .ok()
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn run_analysis(db: &Connection, cfg: &Config, progress: &dyn Fn(i64, i64, &str)) -> rusqlite::Result<Value> {
    let rows: Vec<Row> = db
        .prepare("SELECT key,name,dest,taken,date_src,lat,lon,sha256 FROM items WHERE status='done'")?
        .query_map([], |r| {
            Ok(Row { key: r.get(0)?, name: r.get(1)?, dest: r.get::<_, Option<String>>(2)?.unwrap_or_default(), taken: r.get(3)?, date_src: r.get(4)?, lat: r.get(5)?, lon: r.get(6)?, sha256: r.get(7)? })
        })?
        .flatten()
        .collect();
    let media: Vec<&Row> = rows.iter().filter(|r| !tagging::SIDECAR_EXTS.contains(&tagging::ext_of(&r.name).as_str())).collect();
    let total = media.len() as i64;
    let root = cfg.backup_root();

    // 1. What the pictures themselves say (cached, so a re-run only reads new files)
    let mut facts: HashMap<&str, Facts> = HashMap::new();
    db.execute_batch("BEGIN")?;
    for (i, r) in media.iter().enumerate() {
        let mut f = cached_facts(db, &r.key);
        if f.is_none() && tagging::IMAGE_EXTS.contains(&tagging::ext_of(&r.name).as_str()) {
            let read = tagging::read_facts(&root.join(&r.dest));
            db.execute(
                "INSERT OR REPLACE INTO facts VALUES(?,?,?,?,?,?,?,?)",
                params![r.key, read.width, read.height, read.make, read.model, read.software, read.luma, read.sharp],
            )?;
            f = Some(read);
        }
        if let Some(f) = f {
            facts.insert(&r.key, f);
        }
        if i % 50 == 0 {
            progress(i as i64, total, "Looking at your pictures");
            db.execute_batch("COMMIT; BEGIN")?;
        }
    }
    db.execute_batch("COMMIT")?;

    // 2. Files with identical bytes: keep the phone original, mark the rest
    let mut by_hash: HashMap<&str, Vec<&Row>> = HashMap::new();
    let mut hash_order: Vec<&str> = vec![];
    for r in &media {
        if let Some(h) = r.sha256.as_deref().filter(|h| !h.is_empty()) {
            if !by_hash.contains_key(h) {
                hash_order.push(h);
            }
            by_hash.entry(h).or_default().push(r);
        }
    }
    let mut dupe_of: HashMap<&str, &str> = HashMap::new();
    for h in &hash_order {
        let group = &by_hash[h];
        if group.len() > 1 {
            let keep = group.iter().min_by(|a, b| (a.key.starts_with("legacy|"), &a.dest).cmp(&(b.key.starts_with("legacy|"), &b.dest))).unwrap();
            for r in group {
                if !std::ptr::eq(*r, *keep) {
                    dupe_of.insert(&r.key, &keep.name);
                }
            }
        }
    }

    // 3. Rules
    progress(total, total, "Tagging");
    let mut verdicts: Vec<(&str, Verdict)> = vec![];
    for r in &media {
        let v = match dupe_of.get(r.key.as_str()) {
            Some(of) => Verdict { tag: "duplicate".into(), conf: 99, reasons: vec![format!("Identical to {of}: the same bytes")] },
            None => tagging::classify(&r.name, facts.get(r.key.as_str()), r.date_src.as_deref()),
        };
        verdicts.push((&r.key, v));
    }
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, (k, _)) in verdicts.iter().enumerate() {
        index.insert(k, i);
    }

    // 4. Home and trips
    progress(total, total, "Finding trips");
    let items: Vec<Item> = media.iter().map(|r| Item { key: r.key.clone(), taken: r.taken, lat: r.lat, lon: r.lon }).collect();
    let points: Vec<_> = media.iter().filter_map(|r| Some((trips::local_date(r.taken?), r.lat?, r.lon?))).collect();
    let home = match (cfg.home_lat, cfg.home_lon) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => trips::detect_home(&points),
    };
    let mut found: Vec<trips::Trip> = vec![];
    let mut trip_of: HashMap<String, String> = HashMap::new();
    if let Some(h) = home {
        let deleted: HashSet<String> = db.prepare("SELECT id FROM trip_deleted")?.query_map([], |r| r.get(0))?.flatten().collect();
        let gaz = Gazetteer::new();
        found = trips::detect_trips(&items, h, cfg.trip_radius_km, Some(&gaz)).into_iter().filter(|t| !deleted.contains(&t.id)).collect();
        for t in &found {
            for key in &t.keys {
                trip_of.insert(key.clone(), t.id.clone());
                if let Some(&i) = index.get(key.as_str()) {
                    let v = &verdicts[i].1;
                    if v.tag == "memory" {
                        let when = if t.start == t.end {
                            format!("Taken on a day trip, {}", t.start.format("%d %b %Y"))
                        } else {
                            format!("Taken during a trip, {} to {}", t.start.format("%d %b"), t.end.format("%d %b %Y"))
                        };
                        let mut reasons = vec![when, format!("Up to {:.0} km from home", t.max_km)];
                        reasons.extend(v.reasons.clone());
                        verdicts[i].1 = Verdict { tag: "trip".into(), conf: v.conf.max(85), reasons };
                    }
                }
            }
        }
    }

    // 4b. The person's own changes: photos added to or taken out of trips, and trips they made
    let custom: Vec<(String, String, String, String)> =
        db.prepare("SELECT id, name, start, end FROM trip_custom")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.flatten().collect();
    let valid: HashSet<String> = found.iter().map(|t| t.id.clone()).chain(custom.iter().map(|c| c.0.clone())).collect();
    let edits: Vec<(String, String)> = db.prepare("SELECT key, trip_id FROM trip_edits")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    for (key, tid) in edits {
        let Some(&i) = index.get(key.as_str()) else { continue };
        if tid.is_empty() {
            trip_of.remove(&key);
            if verdicts[i].1.tag == "trip" {
                verdicts[i].1 = Verdict { tag: "memory".into(), conf: 80, reasons: vec!["Taken out of the trip by you".into()] };
            }
        } else if valid.contains(&tid) {
            trip_of.insert(key, tid);
            if verdicts[i].1.tag == "memory" {
                verdicts[i].1 = Verdict { tag: "trip".into(), conf: 95, reasons: vec!["Added to the trip by you".into()] };
            }
        }
    }

    // 5. Save. A user's own tag choices and trip names survive a re-run.
    let stamp = now();
    db.execute_batch("BEGIN")?;
    for (key, v) in &verdicts {
        db.execute(
            "INSERT INTO tags(key,tag,conf,reasons,trip_id,updated) VALUES(?,?,?,?,?,?) \
             ON CONFLICT(key) DO UPDATE SET tag=excluded.tag, conf=excluded.conf, reasons=excluded.reasons, \
             trip_id=excluded.trip_id, updated=excluded.updated",
            params![key, v.tag, v.conf, tagging::json_list(&v.reasons), trip_of.get(*key), stamp],
        )?;
    }
    let old_names: HashMap<String, String> = db
        .prepare("SELECT id, name FROM trips WHERE name IS NOT NULL AND name != ''")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    db.execute("DELETE FROM trips", [])?;
    let mut members: HashMap<&str, i64> = HashMap::new();
    for tid in trip_of.values() {
        *members.entry(tid).or_default() += 1;
    }
    for t in &found {
        let n = members.get(t.id.as_str()).copied().unwrap_or(0);
        if n == 0 {
            continue; // every photo was taken out of it
        }
        db.execute(
            "INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![t.id, old_names.get(&t.id), t.auto_name, t.start.format("%Y-%m-%d").to_string(), t.end.format("%Y-%m-%d").to_string(), t.days(), n, t.max_km, t.lat, t.lon, tagging::json_list(&t.places)],
        )?;
    }
    for (id, name, start, end) in &custom {
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        let days = match (d(start), d(end)) {
            (Some(a), Some(b)) => (b - a).num_days() + 1,
            _ => 1,
        };
        let n = members.get(id.as_str()).copied().unwrap_or(0);
        db.execute("INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)", params![id, name, "", start, end, days.max(1), n, 0, 0, 0, "[]"])?;
    }
    db.execute("INSERT OR REPLACE INTO meta VALUES('analysis_at', ?)", [stamp.to_string()])?;
    if let Some(h) = home {
        db.execute("INSERT OR REPLACE INTO meta VALUES('home', ?)", [serde_json::to_string(&[h.0, h.1]).unwrap()])?;
    }
    db.execute_batch("COMMIT")?;

    let mut counts = serde_json::Map::new();
    for (_, v) in &verdicts {
        let c = counts.entry(v.tag.clone()).or_insert(json!(0));
        *c = json!(c.as_i64().unwrap_or(0) + 1);
    }
    Ok(json!({"total": total, "counts": counts, "trips": found.len(), "home": home.map(|h| vec![h.0, h.1])}))
}

pub struct AnalysisJob {
    pub state: String,
    pub done: i64,
    pub total: i64,
    pub message: String,
    pub summary: Option<Value>,
    pub error: Option<String>,
}

impl Default for AnalysisJob {
    fn default() -> Self {
        AnalysisJob { state: "idle".into(), done: 0, total: 0, message: String::new(), summary: None, error: None }
    }
}

impl AnalysisJob {
    pub fn active(&self) -> bool {
        self.state == "running"
    }
    pub fn snapshot(&self) -> Value {
        json!({"state": self.state, "done": self.done, "total": self.total, "message": self.message, "summary": self.summary, "error": self.error})
    }
}

pub type Shared = Arc<Mutex<AnalysisJob>>;

pub fn spawn(job: Shared, cfg: Config) {
    {
        let mut j = job.lock().unwrap();
        *j = AnalysisJob { state: "running".into(), message: "Starting…".into(), ..Default::default() };
    }
    let _unused = Arc::new(AtomicBool::new(false)).load(Ordering::Relaxed);
    std::thread::spawn(move || {
        let result = (|| -> Result<Value, String> {
            let db = crate::edits::open_rw(&cfg).ok_or("There is nothing backed up yet.")?;
            let j2 = job.clone();
            run_analysis(&db, &cfg, &move |d, t, m| {
                if let Ok(mut j) = j2.lock() {
                    j.done = d;
                    j.total = t;
                    j.message = m.to_string();
                }
            })
            .map_err(|e| e.to_string())
        })();
        let mut j = job.lock().unwrap();
        match result {
            Ok(s) => {
                j.summary = Some(s);
                j.state = "done".into();
                j.message = "Finished".into();
            }
            Err(e) => {
                j.state = "error".into();
                j.error = Some(e);
                j.message = "Stopped by an error".into();
            }
        }
    });
}
