//! Scan, and crash-safe copy.
//!
//! Per file: stream into `.photovault/incoming/<id>_<name>.part` (resumable), fsync, size-check, rename to the
//! incoming name, read the capture date and GPS from the file itself, then atomically rename into its dated
//! folder. A crash at any point leaves either no final file or a complete one. Bytes are copied untouched:
//! no conversion; EXIF and sidecars (.AAE, Live Photo .MOV) kept.

use crate::config::Config;
use crate::device::Device;
use crate::metadata::{self, epoch, from_epoch, Meta, FS_MIN_YEAR};
use rusqlite::{params, Connection};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const SIDECARS: [&str; 1] = [".aae"];

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Debug)]
pub enum CopyErr {
    Cancelled,
    Other(String),
}

impl From<std::io::Error> for CopyErr {
    fn from(e: std::io::Error) -> Self {
        CopyErr::Other(format!("OSError: {e}"))
    }
}
impl From<rusqlite::Error> for CopyErr {
    fn from(e: rusqlite::Error) -> Self {
        CopyErr::Other(format!("DatabaseError: {e}"))
    }
}

/// Pause, resume and cancel a running backup. Checked between chunks, so it reacts within a block.
#[derive(Default)]
pub struct Control {
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
}

impl Control {
    pub fn checkpoint(&self) -> Result<(), CopyErr> {
        while self.paused.load(Ordering::Relaxed) && !self.cancelled.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.cancelled.load(Ordering::Relaxed) { Err(CopyErr::Cancelled) } else { Ok(()) }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub src_path: String,
    pub name: String,
    pub size: i64,
    pub created: f64,
    pub modified: f64,
    pub status: String,
    pub dest: Option<String>,
}

/// Callbacks a UI uses to follow a run.
pub trait Hooks {
    fn on_plan(&self, _files: i64, _bytes: i64) {}
    fn on_file_start(&self, _row: &Row) {}
    fn on_progress(&self, _row: &Row, _done: i64) {}
    fn on_file_end(&self, _row: &Row, _ok: bool, _error: Option<&str>) {}
    fn on_retry(&self, _row: &Row, _attempt: i64, _error: &str) {}
}

pub const ROW_COLS: &str = "key,src_path,name,size,created,modified,status,dest";

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row { key: r.get(0)?, src_path: r.get(1)?, name: r.get(2)?, size: r.get(3)?, created: r.get(4)?, modified: r.get(5)?, status: r.get(6)?, dest: r.get(7)? })
}

pub fn make_key(name: &str, size: i64, created: f64) -> String {
    format!("{name}|{size}|{}", created as i64)
}

/// Open (and create or upgrade) the manifest, the way the Python engine does.
pub fn open_manifest(root: &Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(root.join(".photovault")).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let db = Connection::open(root.join(".photovault").join("manifest.db"))?;
    db.busy_timeout(Duration::from_secs(10))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    db.execute_batch(crate::schema::SCHEMA)?;
    let have: Vec<String> = db.prepare("PRAGMA table_info(items)")?.query_map([], |r| r.get(1))?.flatten().collect();
    for (col, typ) in [("taken", "REAL"), ("date_src", "TEXT"), ("lat", "REAL"), ("lon", "REAL")] {
        if !have.iter().any(|h| h == col) {
            db.execute_batch(&format!("ALTER TABLE items ADD COLUMN {col} {typ}"))?;
        }
    }
    db.execute("UPDATE items SET status='pending' WHERE status='copying'", [])?; // a run that died mid-copy
    Ok(db)
}

fn set_status(db: &Connection, key: &str, status: &str, error: Option<&str>) -> rusqlite::Result<()> {
    db.execute("UPDATE items SET status=?, error=?, updated=? WHERE key=?", params![status, error, now(), key])?;
    Ok(())
}

pub fn pending(db: &Connection) -> rusqlite::Result<Vec<Row>> {
    let sql = format!("SELECT {ROW_COLS} FROM items WHERE status IN ('pending','failed') ORDER BY created");
    Ok(db.prepare(&sql)?.query_map([], row_of)?.flatten().collect())
}

fn dest_in_use(db: &Connection, rel: &str) -> bool {
    db.query_row("SELECT 1 FROM items WHERE dest=? COLLATE NOCASE", [rel], |_| Ok(())).is_ok()
}

/// Record every phone file in the manifest. Returns (seen, new).
pub fn scan(device: &mut dyn Device, db: &Connection, cfg: &Config, on_progress: &dyn Fn(i64), should_stop: &dyn Fn() -> bool) -> Result<(i64, i64), String> {
    let (mut seen, mut new) = (0i64, 0i64);
    db.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let mut failure: Option<String> = None;
    let res = device.list_files(&cfg.remote_roots, &cfg.ignore, &mut |rf| {
        seen += 1;
        let key = make_key(rf.name(), rf.size, rf.created);
        let exists = db.query_row("SELECT 1 FROM items WHERE key=?", [&key], |_| Ok(())).is_ok();
        if !exists {
            match db.execute(
                "INSERT INTO items(key,src_path,name,size,created,modified,status,updated) VALUES(?,?,?,?,?,?, 'pending', ?)",
                params![key, rf.path, rf.name(), rf.size, rf.created, rf.modified, now()],
            ) {
                Ok(_) => new += 1,
                Err(e) => {
                    failure = Some(e.to_string());
                    return false;
                }
            }
        }
        if seen % 500 == 0 {
            let _ = db.execute_batch("COMMIT; BEGIN");
            on_progress(seen);
        }
        !should_stop()
    });
    let _ = db.execute_batch("COMMIT");
    res?;
    if let Some(f) = failure {
        return Err(f);
    }
    on_progress(seen);
    Ok((seen, new))
}

/// IMG_1234.AAE belongs to IMG_1234.* and the edited IMG_E1234.*.
pub fn sidecar_stems(name: &str) -> Vec<String> {
    let stem = splitext(name).0.to_string();
    let mut out = vec![stem.clone()];
    if stem.starts_with("IMG_") && !stem.starts_with("IMG_E") {
        out.push(format!("IMG_E{}", &stem[4..]));
    } else if stem.starts_with("IMG_E") {
        out.push(format!("IMG_{}", &stem[5..]));
    }
    out
}

/// os.path.splitext: a leading dot is not an extension.
pub fn splitext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !name[..i].chars().all(|c| c == '.') => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn dirname(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn like_escape(x: &str) -> String {
    x.replace('!', "!!").replace('_', "!_").replace('%', "!%")
}

/// Capture time of a finished file sharing a stem (used for .AAE sidecars).
fn sibling_taken(db: &Connection, src_dir: &str, stems: &[String]) -> Option<(f64, Option<f64>, Option<f64>)> {
    for stem in stems {
        let r = db.query_row(
            "SELECT taken, lat, lon FROM items WHERE status='done' AND taken IS NOT NULL \
             AND date_src IN ('exif','quicktime','mvhd') AND src_path LIKE ? ESCAPE '!' \
             AND name LIKE ? ESCAPE '!' AND name NOT LIKE '%.AAE' LIMIT 1",
            params![format!("{}/%", like_escape(src_dir)), format!("{}.%", like_escape(stem))],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, Option<f64>>(2)?)),
        );
        if let Ok(x) = r {
            return Some(x);
        }
    }
    None
}

/// Best capture time: file metadata, filename, sidecar sibling, then filesystem date.
pub fn resolve_meta(row: &Row, local: &Path, db: &Connection) -> Meta {
    let mut meta = metadata::read_metadata(local, &row.name);
    if meta.taken.is_none() && SIDECARS.contains(&crate::tagging::ext_of(&row.name).as_str()) {
        if let Some((t, lat, lon)) = sibling_taken(db, dirname(&row.src_path), &sidecar_stems(&row.name)) {
            meta = Meta { taken: Some(from_epoch(t)), source: "sibling", lat, lon };
        }
    }
    if meta.taken.is_none() {
        for ts in [row.created, row.modified] {
            let dt = from_epoch(ts);
            if metadata::sane(Some(dt), FS_MIN_YEAR) {
                return Meta { taken: Some(dt), source: "fs", lat: meta.lat, lon: meta.lon };
            }
        }
        return Meta { taken: None, source: "none", lat: meta.lat, lon: meta.lon };
    }
    meta
}

/// Relative destination path from the layout template; collisions get a key suffix.
pub fn plan_dest(row: &Row, meta: &Meta, cfg: &Config, db: &Connection) -> Result<String, String> {
    use chrono::{Datelike, Timelike};
    let (stem, ext) = splitext(&row.name);
    let mut vars = vec![("name", row.name.clone()), ("stem", stem.to_string()), ("ext", ext.trim_start_matches('.').to_string())];
    let tmpl = match meta.taken {
        None => &cfg.undated_layout,
        Some(dt) => {
            vars.push(("year", format!("{:04}", dt.year())));
            vars.push(("month", format!("{:02}", dt.month())));
            vars.push(("day", format!("{:02}", dt.day())));
            let _ = dt.hour();
            &cfg.layout
        }
    };
    let rel = crate::layout::format(tmpl, &vars).map_err(|e| format!("bad layout field {e}"))?;
    let norm = rel.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').filter(|p| !p.is_empty()).collect();
    if norm.starts_with('/') || parts.contains(&"..") || parts.first().is_some_and(|p| p.contains(':')) {
        return Err(format!("unsafe layout result: {rel}"));
    }
    if dest_in_use(db, &norm) || cfg.backup_root().join(&norm).exists() {
        let tag: String = Sha1::digest(row.key.as_bytes()).iter().take(3).map(|b| format!("{b:02x}")).collect();
        let dir = dirname(&norm);
        let name = format!("{stem}_{tag}{ext}");
        return Ok(if dir.is_empty() { name } else { format!("{dir}/{name}") });
    }
    Ok(norm)
}

pub fn incoming_path(cfg: &Config, row: &Row) -> PathBuf {
    let tag: String = Sha1::digest(row.key.as_bytes()).iter().take(5).map(|b| format!("{b:02x}")).collect();
    cfg.backup_root().join(".photovault").join("incoming").join(format!("{tag}_{}", row.name))
}

pub fn hash_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Move a verified incoming file into its dated folder and record it. Returns dest.
fn place(incoming: &Path, row: &Row, sha: &str, cfg: &Config, db: &Connection) -> Result<String, CopyErr> {
    let meta = resolve_meta(row, incoming, db);
    let rel = plan_dest(row, &meta, cfg, db).map_err(CopyErr::Other)?;
    let dest = cfg.backup_root().join(&rel);
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::rename(incoming, &dest)?;
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&dest) {
        let _ = f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs_f64(row.modified.max(0.0)));
    }
    let taken = meta.taken.map(epoch);
    db.execute(
        "UPDATE items SET status='done', dest=?, sha256=?, error=NULL, updated=?, taken=?, date_src=?, lat=?, lon=? WHERE key=?",
        params![rel, sha, now(), taken, if meta.taken.is_some() { meta.source } else { "none" }, meta.lat, meta.lon, row.key],
    )?;
    Ok(rel)
}

/// Fill `part` to row.size, resuming from its current length. Returns the sha256 hex.
fn copy_stream(device: &mut dyn Device, row: &Row, part: &Path, cfg: &Config, control: &Control, hooks: &dyn Hooks) -> Result<String, CopyErr> {
    let mut h = Sha256::new();
    let mut offset: u64 = 0;
    if part.exists() {
        let have = part.metadata()?.len();
        if have as i64 > row.size {
            std::fs::remove_file(part)?;
        } else {
            let mut f = std::fs::File::open(part)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            offset = have;
        }
    }
    if let Some(p) = part.parent() {
        std::fs::create_dir_all(p)?;
    }
    hooks.on_progress(row, offset as i64);
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
    let chunk = (cfg.chunk_mb.max(1) as usize) << 20;
    while (offset as i64) < row.size {
        control.checkpoint()?;
        let data = device.read_range(&row.src_path, offset, chunk).map_err(CopyErr::Other)?;
        if data.is_empty() {
            break;
        }
        out.write_all(&data)?;
        h.update(&data);
        offset += data.len() as u64;
        hooks.on_progress(row, offset as i64);
    }
    out.flush()?;
    out.sync_all()?;
    drop(out);
    let got = part.metadata()?.len() as i64;
    if got != row.size {
        return Err(CopyErr::Other(format!("IOError: size mismatch for {}: got {got}, expected {}", row.name, row.size)));
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn copy_one(device: &mut dyn Device, db: &Connection, cfg: &Config, row: &Row, control: &Control, hooks: &dyn Hooks) -> Result<(), CopyErr> {
    let inc = incoming_path(cfg, row);
    let part = PathBuf::from(format!("{}.part", inc.display()));
    set_status(db, &row.key, "copying", None)?;
    hooks.on_file_start(row);
    let mut delay = 1.0f64;
    for attempt in 1..=cfg.max_retries {
        let attempt_result = (|| -> Result<(), CopyErr> {
            control.checkpoint()?;
            let sha = if inc.exists() && inc.metadata()?.len() as i64 == row.size {
                let s = hash_file(&inc)?; // crashed after copy, before placing
                hooks.on_progress(row, row.size);
                s
            } else {
                let s = copy_stream(device, row, &part, cfg, control, hooks)?;
                std::fs::rename(&part, &inc)?;
                s
            };
            place(&inc, row, &sha, cfg, db)?;
            Ok(())
        })();
        match attempt_result {
            Ok(()) => {
                hooks.on_file_end(row, true, None);
                return Ok(());
            }
            Err(CopyErr::Cancelled) => return Err(CopyErr::Cancelled), // leave the .part file: the next run resumes it
            Err(CopyErr::Other(e)) => {
                if attempt == cfg.max_retries {
                    set_status(db, &row.key, "failed", Some(&e))?;
                    hooks.on_file_end(row, false, Some(&e));
                    return Ok(());
                }
                hooks.on_retry(row, attempt, &e);
                // wait, but stay responsive to cancel
                let until = std::time::Instant::now() + Duration::from_secs_f64(delay);
                while std::time::Instant::now() < until {
                    control.checkpoint()?;
                    std::thread::sleep(Duration::from_millis(50));
                }
                delay = (delay * 2.0).min(20.0);
                let _ = device.reconnect();
            }
        }
    }
    Ok(())
}

fn check_space(rows: &[Row], cfg: &Config) -> Result<(), String> {
    let need: i64 = rows.iter().map(|r| r.size).sum();
    let root = cfg.backup_root();
    let _ = std::fs::create_dir_all(&root);
    let free = crate::system::free_space(&root).unwrap_or(u64::MAX) as f64;
    let margin = cfg.free_space_margin_gb * 1024f64.powi(3);
    if free < need as f64 + margin.trunc() {
        return Err(format!(
            "Not enough space: need {:.1} GB + {:?} GB margin, only {:.1} GB free on {}",
            need as f64 / 1e9,
            cfg.free_space_margin_gb,
            free / 1e9,
            root.display()
        ));
    }
    Ok(())
}

pub struct RunOptions {
    pub limit: Option<usize>,
    pub only_reencoded: bool,
}

/// Copy everything pending. Returns (done, failed).
pub fn run_copy(device: &mut dyn Device, db: &Connection, cfg: &Config, opts: &RunOptions, control: &Control, hooks: &dyn Hooks) -> Result<(i64, i64), CopyErr> {
    let linked: std::collections::HashSet<String> =
        db.prepare("SELECT DISTINCT phone_key FROM legacy_link")?.query_map([], |r| r.get(0))?.flatten().collect();
    if cfg.copy_originals_of_reencoded {
        db.execute("UPDATE items SET status='pending' WHERE status='skipped'", [])?; // option switched back on
    } else {
        for k in &linked {
            db.execute("UPDATE items SET status='skipped' WHERE key=? AND status='pending'", [k])?;
        }
    }
    let mut rows = pending(db)?;
    if opts.only_reencoded {
        rows.retain(|r| linked.contains(&r.key));
    }
    // Sidecars last, so their photo/video siblings already have a capture date.
    rows.sort_by(|a, b| {
        let sa = SIDECARS.contains(&crate::tagging::ext_of(&a.name).as_str());
        let sb = SIDECARS.contains(&crate::tagging::ext_of(&b.name).as_str());
        (sa, a.created).partial_cmp(&(sb, b.created)).unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(l) = opts.limit {
        rows.truncate(l);
    }
    check_space(&rows, cfg).map_err(CopyErr::Other)?;
    hooks.on_plan(rows.len() as i64, rows.iter().map(|r| r.size).sum());
    let (mut done, mut failed) = (0, 0);
    for row in &rows {
        copy_one(device, db, cfg, row, control, hooks)?;
        let final_status: String = db.query_row("SELECT status FROM items WHERE key=?", [&row.key], |r| r.get(0))?;
        if final_status == "done" { done += 1 } else { failed += 1 }
    }
    Ok((done, failed))
}
