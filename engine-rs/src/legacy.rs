//! Old backup folders: compare one with the phone, import it into the vault, and tidy earlier imports.
//! Nothing here modifies the old folder or the phone.

use crate::config::Config;
use crate::copier::{self, splitext, Row, ROW_COLS};
use crate::device::{ignored, Device};
use crate::metadata::{self, from_epoch, FS_MIN_YEAR};
use chrono::{Datelike, NaiveDate};
use rusqlite::{params, Connection};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAC_EPOCH: u64 = 2_082_844_800;
const TIME_TOL: f64 = 3.0;
const LOOSE_TIME_TOL: f64 = 30.0;
const DUR_TOL: f64 = 1.0;
pub const OLD_IMPORT_DIR: &str = "FromOldBackup";
const BULK_DAY_MIN: usize = 30;

fn is_video(name: &str) -> bool {
    crate::tagging::VIDEO_EXTS.contains(&crate::tagging::ext_of(name).as_str())
}

/// (creation_utc_epoch or 0, duration_seconds) from the moov/mvhd atom, or None.
pub fn video_info(read_at: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>, size: u64) -> Option<(f64, f64)> {
    let mut pos = 0u64;
    while pos + 8 <= size {
        let h = read_at(pos, 16)?;
        if h.len() < 8 {
            return None;
        }
        let mut sz = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let typ = &h[4..8];
        let mut hdr = 8u64;
        if sz == 1 && h.len() >= 16 {
            sz = u64::from_be_bytes(h[8..16].try_into().unwrap());
            hdr = 16;
        } else if sz == 0 {
            sz = size - pos;
        }
        if sz < hdr {
            return None;
        }
        if typ == b"moov" {
            let d = read_at(pos + hdr, (sz - hdr).min(4_000_000) as usize)?;
            let mut p = 0usize;
            while p + 8 <= d.len() {
                let s2 = u32::from_be_bytes(d[p..p + 4].try_into().unwrap()) as usize;
                if s2 < 8 {
                    break;
                }
                if &d[p + 4..p + 8] == b"mvhd" {
                    let ver = *d.get(p + 8)?;
                    let (c, ts, du) = if ver == 1 {
                        let b = d.get(p + 12..p + 40)?;
                        (u64::from_be_bytes(b[..8].try_into().unwrap()), u32::from_be_bytes(b[16..20].try_into().unwrap()), u64::from_be_bytes(b[20..28].try_into().unwrap()))
                    } else {
                        let b = d.get(p + 12..p + 28)?;
                        (u32::from_be_bytes(b[..4].try_into().unwrap()) as u64, u32::from_be_bytes(b[8..12].try_into().unwrap()), u32::from_be_bytes(b[12..16].try_into().unwrap()) as u64)
                    };
                    let created = if c != 0 { c as i64 - MAC_EPOCH as i64 } else { 0 };
                    return Some((created as f64, if ts != 0 { du as f64 / ts as f64 } else { 0.0 }));
                }
                p += s2;
            }
            return None;
        }
        pos += sz;
    }
    None
}

#[derive(Default)]
pub struct Report {
    pub examined: i64,
    pub same_name_size: Vec<String>,
    pub same_size: Vec<(String, String)>,
    pub converted: Vec<(String, String)>,
    pub speed_changed: Vec<(String, String)>,
    pub unmatched: Vec<String>,
}

fn file_info(path: &Path, size: u64) -> Option<(f64, f64)> {
    let mut f = std::fs::File::open(path).ok()?;
    video_info(
        &mut |o, n| {
            f.seek(SeekFrom::Start(o)).ok()?;
            let mut b = vec![0u8; n];
            let mut got = 0;
            while got < n {
                let k = f.read(&mut b[got..]).ok()?;
                if k == 0 {
                    break;
                }
                got += k;
            }
            b.truncate(got);
            Some(b)
        },
        size,
    )
}

fn mtime(m: &std::fs::Metadata) -> f64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn legacy_key(f: &Path, old_dir: &Path, size: u64, mt: f64) -> String {
    let rel = f.strip_prefix(old_dir).unwrap_or(f).to_string_lossy().replace('\\', "/");
    format!("legacy|{rel}|{size}|{}", mt as i64)
}

/// All files under a folder, ordered like Python's sorted(Path.rglob("*")).
pub fn walk(dir: &Path) -> Vec<PathBuf> {
    fn rec(d: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                out.push(p.clone());
                if p.is_dir() {
                    rec(&p, out);
                }
            }
        }
    }
    let mut v = vec![];
    rec(dir, &mut v);
    let key = |p: &PathBuf| -> Vec<String> { p.components().map(|c| { let s = c.as_os_str().to_string_lossy().to_string(); if cfg!(windows) { s.to_lowercase() } else { s } }).collect() };
    v.sort_by_key(key);
    v
}

fn all_rows(db: &Connection) -> rusqlite::Result<Vec<Row>> {
    let sql = format!("SELECT {ROW_COLS} FROM items WHERE key NOT LIKE 'legacy|%'");
    Ok(db.prepare(&sql)?.query_map([], |r| Ok(Row { key: r.get(0)?, src_path: r.get(1)?, name: r.get(2)?, size: r.get(3)?, created: r.get(4)?, modified: r.get(5)?, status: r.get(6)?, dest: r.get(7)? }))?.flatten().collect())
}

fn get_vinfo(db: &Connection, key: &str) -> Option<(f64, f64)> {
    db.query_row("SELECT creation, duration FROM vinfo WHERE key=?", [key], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?))).ok()
}

pub fn compare_old(device: &mut dyn Device, db: &Connection, old_dir: &Path, cfg: &Config, progress: &dyn Fn(&str)) -> Result<Report, String> {
    let mut rep = Report::default();
    let rows = all_rows(db).map_err(|e| e.to_string())?;
    let by_name_size: HashSet<(String, i64)> = rows.iter().map(|r| (r.name.to_lowercase(), r.size)).collect();
    let mut by_size: HashMap<i64, Vec<&Row>> = HashMap::new();
    for r in &rows {
        by_size.entry(r.size).or_default().push(r);
    }
    let videos: Vec<&Row> = rows.iter().filter(|r| is_video(&r.name)).collect();
    let mut pinfo: Vec<(f64, f64, String, String)> = vec![];
    for (i, r) in videos.iter().enumerate() {
        let info = match get_vinfo(db, &r.key) {
            Some(x) => Some(x),
            None => {
                let path = r.src_path.clone();
                let got = video_info(&mut |o, n| device.read_range(&path, o, n).ok(), r.size as u64);
                if let Some((c, d)) = got {
                    let _ = db.execute("INSERT OR REPLACE INTO vinfo VALUES(?,?,?)", params![r.key, c, d]);
                }
                got
            }
        };
        if let Some((c, d)) = info {
            pinfo.push((c, d, r.name.clone(), r.key.clone()));
        }
        if (i + 1) % 200 == 0 {
            progress(&format!("  read {}/{} phone video headers", i + 1, videos.len()));
        }
    }

    for f in walk(old_dir) {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(md) = f.metadata() else { continue };
        if !md.is_file() || ignored(&name, &cfg.ignore) {
            continue;
        }
        rep.examined += 1;
        let size = md.len() as i64;
        let mt = mtime(&md);
        if by_name_size.contains(&(name.to_lowercase(), size)) {
            rep.same_name_size.push(name);
            continue;
        }
        let video = is_video(&name);
        let inf = if video { file_info(&f, md.len()) } else { None };
        if let Some(same) = by_size.get(&size) {
            if same.len() == 1 {
                let pv = if inf.is_some() { get_vinfo(db, &same[0].key) } else { None };
                let ok = match (pv, inf) {
                    (None, _) => true,
                    (Some(p), Some(i)) => (p.0 - i.0).abs() <= 1.0 && (p.1 - i.1).abs() <= DUR_TOL,
                    _ => true,
                };
                if ok {
                    rep.same_size.push((name, same[0].name.clone()));
                    continue;
                }
            }
        }
        if video {
            let cands: Vec<&(f64, f64, String, String)> = pinfo
                .iter()
                .filter(|(c, d, _, _)| (c - mt).abs() <= TIME_TOL || inf.is_some_and(|i| (c - mt).abs() <= LOOSE_TIME_TOL && (d - i.1).abs() <= DUR_TOL))
                .collect();
            if !cands.is_empty() {
                let stem = splitext(&name).0.to_lowercase();
                let mut best: Vec<&&(f64, f64, String, String)> = cands.iter().filter(|x| splitext(&x.2).0.to_lowercase() == stem).collect();
                if best.is_empty() {
                    best = cands.iter().filter(|x| inf.is_some_and(|i| (x.1 - i.1).abs() <= DUR_TOL)).collect();
                }
                if best.is_empty() {
                    best = cands.iter().collect();
                }
                let label = if best.len() == 1 { best[0].2.clone() } else { format!("{} candidates", best.len()) };
                let speed = inf.is_some_and(|i| (best[0].1 - i.1).abs() > DUR_TOL * 2.0);
                if speed { rep.speed_changed.push((name.clone(), label)) } else { rep.converted.push((name.clone(), label)) }
                if best.len() == 1 {
                    let _ = db.execute(
                        "INSERT OR REPLACE INTO legacy_link VALUES(?,?,?)",
                        params![legacy_key(&f, old_dir, md.len(), mt), best[0].3, if speed { "speed-changed" } else { "converted" }],
                    );
                }
                continue;
            }
        }
        rep.unmatched.push(name);
    }
    Ok(rep)
}

// ---- importing ----

/// Capture time for a file from an old backup: (time, source).
pub fn legacy_taken(path: &Path, name: &str, mt: f64, bulk_days: &HashSet<NaiveDate>) -> (Option<chrono::NaiveDateTime>, &'static str) {
    let meta = metadata::read_metadata(path, name);
    let m = from_epoch(mt);
    let m_ok = metadata::sane(Some(m), FS_MIN_YEAR) && !bulk_days.contains(&m.date());
    if let Some(t) = meta.taken {
        let reencoded = meta.source == "mvhd" && m_ok && (t - m).num_seconds() >= 86_400;
        if !reencoded {
            return (Some(t), meta.source);
        }
    }
    if m_ok { (Some(m), "mtime") } else { (None, "none") }
}

/// Destination in the normal layout. An old export that shares a name with a phone file keeps its own name
/// plus " (old export)", so the phone original always owns the clean name.
pub fn legacy_rel(cfg: &Config, db: &Connection, name: &str, taken: Option<chrono::NaiveDateTime>, key: &str, phone_names: &HashSet<String>) -> Result<String, String> {
    let (stem0, ext) = splitext(name);
    let (mut stem, mut name) = (stem0.to_string(), name.to_string());
    if phone_names.contains(&name.to_lowercase()) {
        stem = format!("{stem} (old export)");
        name = format!("{stem}{ext}");
    }
    let mut vars = vec![("name", name.clone()), ("stem", stem.clone()), ("ext", ext.trim_start_matches('.').to_string())];
    let tmpl = match taken {
        Some(t) => {
            vars.push(("year", format!("{:04}", t.year())));
            vars.push(("month", format!("{:02}", t.month())));
            vars.push(("day", format!("{:02}", t.day())));
            &cfg.layout
        }
        None => &cfg.undated_layout,
    };
    let rel = crate::layout::format(tmpl, &vars).map_err(|e| format!("bad layout field {e}"))?;
    let norm = rel.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').filter(|p| !p.is_empty()).collect();
    if norm.starts_with('/') || parts.contains(&"..") || parts.first().is_some_and(|p| p.contains(':')) {
        return Err(format!("unsafe layout result: {rel}"));
    }
    let in_use = db.query_row("SELECT 1 FROM items WHERE dest=? COLLATE NOCASE", [&norm], |_| Ok(())).is_ok();
    if in_use || cfg.backup_root().join(&norm).exists() {
        let tag: String = Sha1::digest(key.as_bytes()).iter().take(3).map(|b| format!("{b:02x}")).collect();
        let leaf = format!("{stem}_{tag}{ext}");
        return Ok(match norm.rsplit_once('/') { Some((d, _)) => format!("{d}/{leaf}"), None => leaf });
    }
    Ok(norm)
}

fn local_date(mt: f64) -> NaiveDate {
    from_epoch(mt).date()
}

/// Copy every file of an old backup folder into the normal dated layout, next to the phone files.
/// Returns (copied, skipped_identical, failed).
pub fn import_old(db: &Connection, cfg: &Config, old_dir: &Path, limit: Option<usize>, progress: &dyn Fn(&str)) -> Result<(i64, i64, i64), String> {
    let done_ns: HashSet<(String, i64)> = db
        .prepare("SELECT name, size FROM items WHERE status='done' AND key NOT LIKE 'legacy|%'")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get::<_, String>(0)?.to_lowercase(), r.get::<_, i64>(1)?))).map(|it| it.flatten().collect()))
        .map_err(|e| e.to_string())?;
    let files: Vec<PathBuf> = walk(old_dir)
        .into_iter()
        .filter(|f| f.is_file() && !ignored(&f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), &cfg.ignore))
        .collect();
    let mut day_counts: HashMap<NaiveDate, usize> = HashMap::new();
    for f in &files {
        if let Ok(m) = f.metadata() {
            *day_counts.entry(local_date(mtime(&m))).or_default() += 1;
        }
    }
    let bulk_days: HashSet<NaiveDate> = day_counts.into_iter().filter(|(_, c)| *c >= BULK_DAY_MIN).map(|(d, _)| d).collect();
    let phone_names: HashSet<String> = db
        .prepare("SELECT name FROM items WHERE key NOT LIKE 'legacy|%'")
        .and_then(|mut s| s.query_map([], |r| Ok(r.get::<_, String>(0)?.to_lowercase())).map(|it| it.flatten().collect()))
        .map_err(|e| e.to_string())?;
    let mut todo: Vec<(PathBuf, u64, f64, String)> = vec![];
    let mut skipped = 0i64;
    for f in files {
        let Ok(md) = f.metadata() else { continue };
        let (size, mt) = (md.len(), mtime(&md));
        let key = legacy_key(&f, old_dir, size, mt);
        let status: Option<String> = db.query_row("SELECT status FROM items WHERE key=?", [&key], |r| r.get(0)).ok();
        if status.as_deref() == Some("done") {
            continue;
        }
        let name = f.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if done_ns.contains(&(name, size as i64)) {
            skipped += 1;
            continue;
        }
        todo.push((f, size, mt, key));
    }
    if let Some(l) = limit {
        todo.truncate(l);
    }
    let need: u64 = todo.iter().map(|t| t.1).sum();
    let root = cfg.backup_root();
    let _ = std::fs::create_dir_all(&root);
    let free = crate::system::free_space(&root).unwrap_or(u64::MAX) as f64;
    if free < need as f64 + (cfg.free_space_margin_gb * 1024f64.powi(3)).trunc() {
        return Err(format!("Not enough space to import {:.1} GB", need as f64 / 1e9));
    }
    let (mut copied, mut failed) = (0i64, 0i64);
    let total = todo.len();
    for (i, (f, size, mt, key)) in todo.iter().enumerate() {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let attempt = (|| -> Result<(), String> {
            let now = copier::now();
            let _ = db.execute(
                "INSERT OR IGNORE INTO items(key,src_path,name,size,created,modified,status,updated) VALUES(?,?,?,?,?,?, 'pending', ?)",
                params![key, f.to_string_lossy(), name, *size as i64, mt, mt, now],
            );
            db.execute("UPDATE items SET status='importing', error=NULL, updated=? WHERE key=?", params![now, key]).map_err(|e| e.to_string())?;
            let tag: String = Sha1::digest(key.as_bytes()).iter().take(5).map(|b| format!("{b:02x}")).collect();
            let inc = root.join(".photovault").join("incoming").join(format!("{tag}_{name}"));
            std::fs::create_dir_all(inc.parent().unwrap()).map_err(|e| e.to_string())?;
            let part = PathBuf::from(format!("{}.part", inc.display()));
            let mut h = Sha256::new();
            {
                let mut fin = std::fs::File::open(f).map_err(|e| e.to_string())?;
                let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 4 << 20];
                loop {
                    let n = fin.read(&mut buf).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                    h.update(&buf[..n]);
                }
                out.flush().map_err(|e| e.to_string())?;
                out.sync_all().map_err(|e| e.to_string())?;
            }
            if part.metadata().map_err(|e| e.to_string())?.len() != *size {
                return Err("IOError: size mismatch".into());
            }
            std::fs::rename(&part, &inc).map_err(|e| e.to_string())?;
            let (taken, src) = legacy_taken(&inc, &name, *mt, &bulk_days);
            let rel = legacy_rel(cfg, db, &name, taken, key, &phone_names)?;
            let dest = root.join(&rel);
            std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::rename(&inc, &dest).map_err(|e| e.to_string())?;
            if let Ok(df) = std::fs::OpenOptions::new().write(true).open(&dest) {
                let _ = df.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(mt.max(0.0)));
            }
            let sha: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            db.execute(
                "UPDATE items SET status='done', dest=?, sha256=?, error=NULL, updated=?, taken=?, date_src=?, lat=NULL, lon=NULL WHERE key=?",
                params![rel, sha, copier::now(), taken.map(metadata::epoch), src, key],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })();
        match attempt {
            Ok(()) => copied += 1,
            Err(e) => {
                let _ = db.execute("UPDATE items SET status='importing', error=?, updated=? WHERE key=?", params![e, copier::now(), key]);
                failed += 1;
            }
        }
        if (i + 1) % 50 == 0 || i + 1 == total {
            progress(&format!("  {}/{}  ok={copied} failed={failed}", i + 1, total));
        }
    }
    Ok((copied, skipped, failed))
}

/// Move files an earlier version put in FromOldBackup/ into the normal dated layout (same-volume renames).
pub fn relocate_old(db: &Connection, cfg: &Config, progress: &dyn Fn(&str)) -> Result<(i64, i64), String> {
    let phone_names: HashSet<String> = db
        .prepare("SELECT name FROM items WHERE key NOT LIKE 'legacy|%'")
        .and_then(|mut s| s.query_map([], |r| Ok(r.get::<_, String>(0)?.to_lowercase())).map(|it| it.flatten().collect()))
        .map_err(|e| e.to_string())?;
    type R = (String, String, String, Option<f64>);
    let rows: Vec<R> = db
        .prepare("SELECT key, name, dest, taken FROM items WHERE status='done' AND key LIKE 'legacy|%' AND dest LIKE ?")
        .and_then(|mut s| s.query_map([format!("{OLD_IMPORT_DIR}/%")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.flatten().collect()))
        .map_err(|e| e.to_string())?;
    let root = cfg.backup_root();
    let (mut moved, mut failed) = (0i64, 0i64);
    let total = rows.len();
    for (i, (key, name, dest, taken)) in rows.iter().enumerate() {
        let attempt = (|| -> Result<String, String> {
            let t = taken.filter(|t| *t != 0.0).map(from_epoch);
            let old = root.join(dest);
            if !old.is_file() {
                return Err(format!("IOError: missing: {}", old.display()));
            }
            db.execute("UPDATE items SET dest=NULL WHERE key=?", [key]).map_err(|e| e.to_string())?;
            let rel = legacy_rel(cfg, db, name, t, key, &phone_names)?;
            let new = root.join(&rel);
            std::fs::create_dir_all(new.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::rename(&old, &new).map_err(|e| e.to_string())?;
            db.execute("UPDATE items SET dest=? WHERE key=?", params![rel, key]).map_err(|e| e.to_string())?;
            Ok(rel)
        })();
        match attempt {
            Ok(_) => moved += 1,
            Err(e) => {
                let _ = db.execute("UPDATE items SET dest=?, error=? WHERE key=?", params![dest, e, key]);
                failed += 1;
            }
        }
        if (i + 1) % 100 == 0 || i + 1 == total {
            progress(&format!("  {}/{}  moved={moved} failed={failed}", i + 1, total));
        }
    }
    let dir = root.join(OLD_IMPORT_DIR);
    if dir.exists() {
        let mut dirs: Vec<PathBuf> = walk(&dir).into_iter().filter(|p| p.is_dir()).collect();
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for d in dirs {
            let _ = std::fs::remove_dir(&d);
        }
        let _ = std::fs::remove_dir(&dir);
    }
    Ok((moved, failed))
}
