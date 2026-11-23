//! Health: re-read every backed-up file and compare it with the fingerprint saved when it was copied,
//! and queue anything missing or damaged to be copied again. (Comparing with the phone needs the iPhone,
//! which this engine cannot reach yet.)

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};

pub const SHOWN: usize = 40;

#[derive(Default)]
pub struct Report {
    pub checked: i64,
    pub missing: Vec<String>,
    pub wrong_size: Vec<String>,
    pub corrupt: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.wrong_size.is_empty() && self.corrupt.is_empty()
    }
}

pub fn sha256_file(p: &Path, should_stop: &dyn Fn() -> bool) -> Option<String> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        if should_stop() {
            return None;
        }
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Quick: every file exists with the recorded size. Deep: re-hash and compare. `Err` means it was stopped.
pub fn verify_ssd(db: &Connection, root: &Path, deep: bool, on_progress: &dyn Fn(i64, i64), should_stop: &dyn Fn() -> bool) -> Result<Report, ()> {
    let rows: Vec<(String, i64, Option<String>)> = db
        .prepare("SELECT dest, size, sha256 FROM items WHERE status='done'")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    let total = rows.len() as i64;
    let mut rep = Report::default();
    for (i, (dest, size, sha)) in rows.into_iter().enumerate() {
        if should_stop() {
            return Err(());
        }
        let p = root.join(&dest);
        rep.checked += 1;
        match p.metadata() {
            Ok(m) if m.is_file() => {
                if m.len() as i64 != size {
                    rep.wrong_size.push(dest);
                } else if deep {
                    match sha256_file(&p, should_stop) {
                        None if should_stop() => return Err(()),
                        got if got != sha => rep.corrupt.push(dest),
                        _ => {}
                    }
                }
            }
            _ => rep.missing.push(dest),
        }
        let n = i as i64 + 1;
        if n % 20 == 0 || n == total {
            on_progress(n, total);
        }
    }
    Ok(rep)
}

/// Queue every missing, wrong-size or corrupt file for copying again on the next backup. A damaged file is
/// moved into `.photovault/quarantine`, never deleted. Files that came from an old folder cannot be re-fetched.
pub fn repair_problems(db: &Connection, root: &Path, problems: &[String]) -> Value {
    let (mut queued, mut legacy) = (0, 0);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let quarantine = root.join(".photovault").join("quarantine").join(stamp);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    for rel in problems {
        let Ok(key) = db.query_row("SELECT key FROM items WHERE dest=? AND status='done'", [rel], |r| r.get::<_, String>(0)) else { continue };
        if key.starts_with("legacy|") {
            legacy += 1;
            continue;
        }
        let p = root.join(rel);
        if p.is_file() {
            let _ = std::fs::create_dir_all(&quarantine);
            let name = p.file_name().map(|n| n.to_owned()).unwrap_or_default();
            let _ = std::fs::rename(&p, quarantine.join(name));
        }
        let _ = db.execute("UPDATE items SET status='pending', dest=NULL, sha256=NULL, error=NULL, updated=? WHERE key=?", params![now, key]);
        queued += 1;
    }
    json!({"queued": queued, "legacy_skipped": legacy})
}

pub struct HealthJob {
    pub state: String,
    pub kind: Option<String>,
    pub done: i64,
    pub total: i64,
    pub message: String,
    pub error: Option<String>,
    pub started_at: Option<f64>,
    pub stop: Arc<AtomicBool>,
}

impl Default for HealthJob {
    fn default() -> Self {
        HealthJob { state: "idle".into(), kind: None, done: 0, total: 0, message: String::new(), error: None, started_at: None, stop: Arc::new(AtomicBool::new(false)) }
    }
}

impl HealthJob {
    pub fn active(&self) -> bool {
        self.state == "running"
    }
    pub fn snapshot(&self) -> Value {
        json!({"state": self.state, "kind": self.kind, "done": self.done, "total": self.total, "message": self.message, "error": self.error, "started_at": self.started_at})
    }
}

pub type Shared = Arc<Mutex<HealthJob>>;

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Run a deep check on a worker thread. The caller has already refused if one is running.
pub fn spawn_deep(job: Shared, db_path: std::path::PathBuf, root: std::path::PathBuf) {
    let stop = {
        let mut j = job.lock().unwrap();
        *j = HealthJob { state: "running".into(), kind: Some("deep".into()), message: "Starting…".into(), started_at: Some(now()), ..Default::default() };
        j.stop.clone()
    };
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            job.lock().unwrap().message = "Re-reading every file on the SSD…".into();
            let db = Connection::open(&db_path).map_err(|e| e.to_string())?;
            let stop2 = stop.clone();
            let job2 = job.clone();
            let rep = verify_ssd(
                &db,
                &root,
                true,
                &move |d, t| {
                    if let Ok(mut j) = job2.lock() {
                        j.done = d;
                        j.total = t;
                    }
                },
                &move || stop2.load(Ordering::Relaxed),
            )
            .map_err(|_| "cancelled".to_string())?;
            let payload = json!({
                "checked": rep.checked, "missing": rep.missing, "wrong_size": rep.wrong_size, "corrupt": rep.corrupt,
                "ok": rep.ok(), "kind": "deep", "at": now()
            });
            db.execute("INSERT OR REPLACE INTO meta VALUES(?, ?)", params!["health_deep", payload.to_string()]).map_err(|e| e.to_string())?;
            Ok(())
        })();
        let mut j = job.lock().unwrap();
        match result {
            Ok(()) => {
                j.state = "done".into();
                j.message = "Finished".into();
            }
            Err(e) if e == "cancelled" => {
                j.state = "cancelled".into();
                j.message = "Stopped".into();
            }
            Err(e) => {
                j.state = "error".into();
                j.error = Some(e);
                j.message = "Stopped by an error".into();
            }
        }
    });
}

/// The saved results of the last checks, with long lists cut down to what the screen shows.
pub fn last(db: Option<&Connection>) -> Value {
    let mut out = json!({"sync": null, "deep": null});
    let Some(db) = db else { return out };
    for kind in ["sync", "deep"] {
        let Ok(text) = db.query_row("SELECT v FROM meta WHERE k=?", [format!("health_{kind}")], |r| r.get::<_, String>(0)) else { continue };
        let Ok(mut r) = serde_json::from_str::<Value>(&text) else { continue };
        for key in ["missing", "wrong_size", "corrupt"] {
            let full = r.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default();
            r[key] = json!({"count": full.len(), "shown": full.iter().take(SHOWN).cloned().collect::<Vec<_>>()});
        }
        out[kind] = r;
    }
    out
}

/// Compare a fresh phone listing with the manifest and with what is actually on the SSD.
pub fn sync_report(device: &mut dyn crate::device::Device, db: &Connection, cfg: &crate::config::Config, on_progress: &dyn Fn(i64)) -> Result<Value, String> {
    let (mut on_phone, mut backed_up, mut skipped) = (0i64, 0i64, 0i64);
    let (mut waiting, mut adopted): (Vec<String>, Vec<String>) = (vec![], vec![]);
    let mut phone_keys = std::collections::HashSet::new();
    device.list_files(&cfg.remote_roots, &cfg.ignore, &mut |rf| {
        on_phone += 1;
        if on_phone % 500 == 0 {
            on_progress(on_phone);
        }
        let key = crate::copier::make_key(rf.name(), rf.size, rf.created);
        let status: Option<String> = db.query_row("SELECT status FROM items WHERE key=?", [&key], |r| r.get(0)).ok();
        match status.as_deref() {
            None | Some("pending") | Some("copying") | Some("failed") => waiting.push(rf.path.clone()),
            Some("skipped") => skipped += 1,
            Some("adopted") => adopted.push(rf.path.clone()),
            _ => backed_up += 1,
        }
        phone_keys.insert(key);
        true
    })?;
    let done_keys: Vec<String> = db
        .prepare("SELECT key FROM items WHERE status='done' AND key NOT LIKE 'legacy|%'")
        .and_then(|mut s| s.query_map([], |r| r.get(0)).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    let only_on_ssd = done_keys.iter().filter(|k| !phone_keys.contains(*k)).count();
    let rep = verify_ssd(db, &cfg.backup_root(), false, &|_, _| {}, &|| false).unwrap_or_default();
    let in_sync = waiting.is_empty() && adopted.is_empty() && rep.ok();
    Ok(json!({
        "on_phone": on_phone, "backed_up": backed_up, "waiting": waiting.len(),
        "waiting_sample": waiting.iter().take(25).collect::<Vec<_>>(), "adopted_not_copied": adopted.len(),
        "only_on_ssd": only_on_ssd, "skipped": skipped, "checked": rep.checked, "missing": rep.missing,
        "wrong_size": rep.wrong_size, "corrupt": rep.corrupt, "ok": rep.ok(), "in_sync": in_sync,
    }))
}

/// Run the phone-vs-SSD comparison on a worker thread.
pub fn spawn_sync(job: Shared, cfg: crate::config::Config) {
    {
        let mut j = job.lock().unwrap();
        *j = HealthJob { state: "running".into(), kind: Some("sync".into()), message: "Starting…".into(), started_at: Some(now()), ..Default::default() };
    }
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            job.lock().unwrap().message = "Connecting to the iPhone…".into();
            let mut dev = crate::device::make_device();
            dev.reconnect().map_err(|e| crate::device::explain_device_error(&e).1)?;
            job.lock().unwrap().message = "Comparing the iPhone with the SSD…".into();
            let db = Connection::open(cfg.manifest_path()).map_err(|e| e.to_string())?;
            let j2 = job.clone();
            let mut payload = sync_report(dev.as_mut(), &db, &cfg, &move |n| {
                if let Ok(mut j) = j2.lock() {
                    j.done = n;
                }
            })?;
            payload["kind"] = json!("sync");
            payload["at"] = json!(now());
            db.execute("INSERT OR REPLACE INTO meta VALUES(?, ?)", params!["health_sync", payload.to_string()]).map_err(|e| e.to_string())?;
            Ok(())
        })();
        let mut j = job.lock().unwrap();
        match result {
            Ok(()) => {
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
