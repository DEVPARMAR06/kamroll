//! File trip photos into trip folders on the SSD, and put them back.
//!
//! Every move is a rename inside the backup folder, so no bytes are rewritten and the saved fingerprints stay
//! valid. Each move is journaled first, so a crash halfway is finished or forgotten on the next run. Nothing is
//! deleted, and `filing_origin` remembers where each file came from so it can go home again.

use crate::config::Config;
use chrono::{Local, NaiveDate, TimeZone};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};

const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5",
    "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A folder name that is valid on Windows, macOS and exFAT.
pub fn safe_name(text: &str) -> String {
    let replaced: String = text.chars().map(|c| if "<>:\"/\\|?*".contains(c) || (c as u32) < 0x20 { '-' } else { c }).collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = collapsed.trim_matches(|c| c == ' ' || c == '.').chars().take(80).collect();
    let t = trimmed.trim_end_matches([' ', '.']).to_string();
    if t.is_empty() {
        return "Trip".into();
    }
    if RESERVED.contains(&t.to_lowercase().as_str()) {
        format!("{t}_")
    } else {
        t
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mv {
    pub key: String,
    pub name: String,
    pub src: String,
    pub dst: String,
    pub reason: String,
}

fn splitext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn dirname(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// IMG_1234.AAE belongs to IMG_1234.* and the edited IMG_E1234.*.
fn sidecar_stems(name: &str) -> Vec<String> {
    let stem = splitext(name).0.to_string();
    let mut out = vec![stem.clone()];
    if stem.starts_with("IMG_") && !stem.starts_with("IMG_E") {
        out.push(format!("IMG_E{}", &stem[4..]));
    } else if stem.starts_with("IMG_E") {
        out.push(format!("IMG_{}", &stem[5..]));
    }
    out
}

fn short_hash(key: &str) -> String {
    Sha1::digest(key.as_bytes()).iter().take(3).map(|b| format!("{b:02x}")).collect()
}

fn local_from_ts(ts: f64) -> chrono::NaiveDateTime {
    Local.timestamp_opt(ts as i64, 0).earliest().map(|d| d.naive_local()).unwrap_or_default()
}

fn trip_rel(cfg: &Config, label: &str, start: NaiveDate, taken: Option<f64>, name: &str) -> String {
    use chrono::Datelike;
    let d = taken.map(local_from_ts).unwrap_or_else(|| start.and_hms_opt(0, 0, 0).unwrap());
    let (stem, ext) = splitext(name);
    let vars = vec![
        ("year", format!("{:04}", d.year())),
        ("month", format!("{:02}", d.month())),
        ("day", format!("{:02}", d.day())),
        ("name", name.to_string()),
        ("stem", stem.to_string()),
        ("ext", ext.trim_start_matches('.').to_string()),
        ("trip", safe_name(label)),
        ("trip_dated", safe_name(&format!("{label} ({})", start.format("%b %Y")))),
        ("trip_year", format!("{:04}", start.year())),
    ];
    crate::layout::format(&cfg.trip_layout, &vars).unwrap_or_default().replace('\\', "/")
}

/// Work out every move needed to match the trips and the setting. Moves nothing.
pub fn plan_filing(db: &Connection, cfg: &Config) -> Vec<Mv> {
    type Trip = (Option<String>, Option<String>, String);
    let mut trips: HashMap<String, Trip> = HashMap::new();
    if let Ok(mut st) = db.prepare("SELECT id, name, auto_name, start FROM trips") {
        if let Ok(it) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)))) {
            trips.extend(it.flatten());
        }
    }
    type Row = (String, String, String, Option<f64>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = db
        .prepare(
            "SELECT i.key, i.name, i.dest, i.taken, t.tag, t.user_tag, t.trip_id FROM items i \
             LEFT JOIN tags t ON t.key = i.key WHERE i.status = 'done' AND i.dest IS NOT NULL",
        )
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    let origins: HashMap<String, String> = db
        .prepare("SELECT key, origin FROM filing_origin")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default();

    let mut occupied: HashMap<String, String> = rows.iter().map(|r| (r.2.to_lowercase(), r.0.clone())).collect();
    let media_at: HashMap<(String, String), String> = rows
        .iter()
        .filter(|r| r.4.is_some())
        .map(|r| ((dirname(&r.2).to_lowercase(), splitext(&r.1).0.to_lowercase()), r.0.clone()))
        .collect();

    fn unique(occupied: &HashMap<String, String>, dst: String, key: &str) -> String {
        if occupied.get(&dst.to_lowercase()).map(|k| k != key).unwrap_or(false) {
            let (base, ext) = match dst.rfind('.') {
                Some(i) if dst.rfind('/').map(|s| s < i).unwrap_or(true) => (dst[..i].to_string(), dst[i..].to_string()),
                _ => (dst.clone(), String::new()),
            };
            return format!("{base}_{}{ext}", short_hash(key));
        }
        dst
    }

    let mut moves: HashMap<String, Mv> = HashMap::new();
    for (key, name, dest, taken, tag, user_tag, trip_id) in &rows {
        let Some(tag) = tag else { continue };                      // sidecars follow their photo, below
        let eff = user_tag.clone().unwrap_or_else(|| tag.clone());
        let trip = trip_id.as_ref().and_then(|t| trips.get(t));
        let (dst, reason) = if cfg.file_trips && eff == "trip" && trip.is_some() {
            let (n, a, start) = trip.unwrap();
            let label = n.clone().filter(|x| !x.is_empty()).or(a.clone().filter(|x| !x.is_empty())).unwrap_or_else(|| start.clone());
            match NaiveDate::parse_from_str(start, "%Y-%m-%d") {
                Ok(d) => (trip_rel(cfg, &label, d, *taken, name), "into-trip"),
                Err(_) => continue,
            }
        } else if let Some(o) = origins.get(key) {
            (o.clone(), "back")
        } else {
            continue;
        };
        let dst = unique(&occupied, dst, key);
        if &dst != dest {
            occupied.insert(dst.to_lowercase(), key.clone());
            moves.insert(key.clone(), Mv { key: key.clone(), name: name.clone(), src: dest.clone(), dst, reason: reason.into() });
        }
    }
    for (key, name, dest, _, tag, _, _) in &rows {
        if tag.is_some() {
            continue;
        }
        let folder = dirname(dest).to_lowercase();
        for stem in sidecar_stems(name) {
            if let Some(mk) = media_at.get(&(folder.clone(), stem.to_lowercase())) {
                if let Some(m) = moves.get(mk).cloned() {
                    let base = dest.rsplit('/').next().unwrap_or(dest);
                    let dst = unique(&occupied, format!("{}/{}", dirname(&m.dst), base), key);
                    if &dst != dest {
                        occupied.insert(dst.to_lowercase(), key.clone());
                        moves.insert(key.clone(), Mv { key: key.clone(), name: name.clone(), src: dest.clone(), dst, reason: m.reason });
                    }
                    break;
                }
            }
        }
    }
    let mut out: Vec<Mv> = moves.into_values().collect();
    out.sort_by(|a, b| a.dst.cmp(&b.dst));
    out
}

fn prune_empty(mut folder: PathBuf, root: &Path) {
    while folder != root && folder.starts_with(root) {
        if std::fs::remove_dir(&folder).is_err() {
            return;                                           // only succeeds when empty
        }
        match folder.parent() {
            Some(p) => folder = p.to_path_buf(),
            None => return,
        }
    }
}

fn finish(db: &Connection, key: &str, dst: &str, reason: &str, src: &str) -> rusqlite::Result<()> {
    db.execute("UPDATE items SET dest=? WHERE key=?", params![dst, key])?;
    if reason == "into-trip" {
        db.execute("INSERT OR IGNORE INTO filing_origin VALUES(?, ?)", params![key, src])?;
    } else {
        db.execute("DELETE FROM filing_origin WHERE key=?", [key])?;
    }
    db.execute("DELETE FROM moves WHERE key=?", [key])?;
    Ok(())
}

/// Settle moves a crash left half done. Returns how many were completed.
pub fn recover_moves(db: &Connection, root: &Path) -> i64 {
    let rows: Vec<(String, String, String, String)> = db
        .prepare("SELECT key, src, dst, reason FROM moves")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    let mut done = 0;
    for (key, src, dst, reason) in rows {
        if root.join(&dst).is_file() && !root.join(&src).is_file() {
            let _ = finish(db, &key, &dst, &reason, &src);
            done += 1;
        } else {
            let _ = db.execute("DELETE FROM moves WHERE key=?", [&key]);              // it never happened
        }
    }
    done
}

pub fn apply_moves(db: &Connection, root: &Path, moves: &[Mv], on_progress: &dyn Fn(i64, i64), should_stop: &dyn Fn() -> bool) -> Value {
    recover_moves(db, root);
    let (mut moved, mut failed): (i64, Vec<Value>) = (0, vec![]);
    let total = moves.len() as i64;
    for (i, mv) in moves.iter().enumerate() {
        if should_stop() {
            break;
        }
        let (src, dst) = (root.join(&mv.src), root.join(&mv.dst));
        let attempt = || -> Result<(), String> {
            db.execute("INSERT OR REPLACE INTO moves VALUES(?,?,?,?)", params![mv.key, mv.src, mv.dst, mv.reason]).map_err(|e| e.to_string())?;
            if src.is_file() {
                if dst.exists() {
                    return Err(format!("FileExistsError: {} already exists", mv.dst));
                }
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&src, &dst).map_err(|e| e.to_string())?;
            } else if !dst.is_file() {
                return Err(format!("FileNotFoundError: {} is missing", mv.src));
            }
            finish(db, &mv.key, &mv.dst, &mv.reason, &mv.src).map_err(|e| e.to_string())?;
            if let Some(p) = src.parent() {
                prune_empty(p.to_path_buf(), root);
            }
            Ok(())
        };
        match attempt() {
            Ok(()) => moved += 1,
            Err(e) => {
                let _ = db.execute("DELETE FROM moves WHERE key=?", [&mv.key]);
                failed.push(json!({"name": mv.name, "error": e}));
            }
        }
        let n = i as i64 + 1;
        if n % 10 == 0 || n == total {
            on_progress(n, total);
        }
    }
    let count = failed.len();
    json!({"moved": moved, "failed": failed.into_iter().take(20).collect::<Vec<_>>(), "failed_count": count})
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub struct OrgJob {
    pub state: String,
    pub done: i64,
    pub total: i64,
    pub message: String,
    pub error: Option<String>,
    pub result: Option<Value>,
    pub stop: Arc<AtomicBool>,
}

impl Default for OrgJob {
    fn default() -> Self {
        OrgJob { state: "idle".into(), done: 0, total: 0, message: String::new(), error: None, result: None, stop: Arc::new(AtomicBool::new(false)) }
    }
}

impl OrgJob {
    pub fn active(&self) -> bool {
        self.state == "running"
    }
    pub fn snapshot(&self) -> Value {
        json!({"state": self.state, "done": self.done, "total": self.total, "message": self.message, "error": self.error, "result": self.result})
    }
}

pub type SharedOrg = Arc<Mutex<OrgJob>>;

pub fn spawn_apply(job: SharedOrg, cfg: Config) {
    let stop = {
        let mut j = job.lock().unwrap();
        *j = OrgJob { state: "running".into(), message: "Working out what to move…".into(), ..Default::default() };
        j.stop.clone()
    };
    std::thread::spawn(move || {
        let outcome = (|| -> Result<Value, String> {
            let db = crate::edits::open_rw(&cfg).ok_or("There is nothing backed up yet.")?;
            let root = cfg.backup_root();
            let moves = plan_filing(&db, &cfg);
            {
                let mut j = job.lock().unwrap();
                j.total = moves.len() as i64;
                j.message = if moves.is_empty() { "Nothing to move.".into() } else { format!("Moving {} files…", thousands(moves.len())) };
            }
            let (job2, stop2) = (job.clone(), stop.clone());
            let mut res = apply_moves(
                &db,
                &root,
                &moves,
                &move |d, _| {
                    if let Ok(mut j) = job2.lock() {
                        j.done = d;
                    }
                },
                &move || stop2.load(Ordering::Relaxed),
            );
            let ok = crate::verify::verify_ssd(&db, &root, false, &|_, _| {}, &|| false).map(|r| r.ok()).unwrap_or(false);
            res["planned"] = json!(moves.len());
            res["ssd_ok"] = json!(ok);
            res["stopped"] = json!(stop.load(Ordering::Relaxed));
            Ok(res)
        })();
        let mut j = job.lock().unwrap();
        match outcome {
            Ok(res) => {
                let stopped = stop.load(Ordering::Relaxed);
                j.result = Some(res);
                j.state = if stopped { "cancelled" } else { "done" }.into();
                j.message = if stopped { "Stopped" } else { "Finished" }.into();
            }
            Err(e) => {
                j.state = "error".into();
                j.error = Some(e);
                j.message = "Stopped by an error".into();
            }
        }
    });
}
