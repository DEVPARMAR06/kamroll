//! Adopting and consolidating an existing backup folder, plus the manifest counts the command line prints.

use crate::config::Config;
use crate::copier::{self, CopyErr, Row, ROW_COLS};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Mark phone files that already exist in `existing_dir` (matched by name+size) as adopted. Returns (matched, examined).
pub fn adopt(db: &Connection, existing_dir: &Path, cfg: &Config) -> (i64, i64) {
    let (mut matched, mut examined) = (0i64, 0i64);
    for p in crate::legacy::walk(existing_dir) {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(md) = p.metadata() else { continue };
        if !md.is_file() || crate::device::ignored(&name, &cfg.ignore) {
            continue;
        }
        examined += 1;
        let sql = format!("SELECT {ROW_COLS} FROM items WHERE name=? COLLATE NOCASE AND size=?");
        let rows: Vec<Row> = db
            .prepare(&sql)
            .and_then(|mut s| s.query_map(params![name, md.len() as i64], copier::row_of).map(|it| it.flatten().collect()))
            .unwrap_or_default();
        for row in rows {
            if row.status == "pending" || row.status == "failed" {
                let abs = p.canonicalize().map(|c| c.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or_else(|_| p.to_string_lossy().to_string());
                let _ = db.execute("UPDATE items SET status='adopted', dest=?, error=NULL, updated=? WHERE key=?", params![abs, copier::now(), row.key]);
                matched += 1;
                break;
            }
        }
    }
    (matched, examined)
}

/// Copy adopted files from their old folder into the dated layout (the old folder is only read). Returns (copied, failed).
pub fn consolidate(db: &Connection, cfg: &Config, limit: Option<usize>, progress: &dyn Fn(&str)) -> Result<(i64, i64), String> {
    let sql = format!("SELECT {ROW_COLS} FROM items WHERE status='adopted' ORDER BY created");
    let mut rows: Vec<Row> = db.prepare(&sql).and_then(|mut s| s.query_map([], copier::row_of).map(|it| it.flatten().collect())).map_err(|e| e.to_string())?;
    if let Some(l) = limit {
        rows.truncate(l);
    }
    copier::check_space(&rows, cfg)?;
    let (mut done, mut failed) = (0i64, 0i64);
    let total = rows.len();
    for (i, row) in rows.iter().enumerate() {
        let attempt = (|| -> Result<(), CopyErr> {
            let src = PathBuf::from(row.dest.clone().unwrap_or_default());
            if !src.is_file() || src.metadata()?.len() as i64 != row.size {
                return Err(CopyErr::Other(format!("IOError: source missing or size changed: {}", src.display())));
            }
            let inc = copier::incoming_path(cfg, row);
            std::fs::create_dir_all(inc.parent().unwrap())?;
            let part = PathBuf::from(format!("{}.part", inc.display()));
            let mut h = Sha256::new();
            {
                let mut fin = std::fs::File::open(&src)?;
                let mut out = std::fs::File::create(&part)?;
                let mut buf = vec![0u8; 4 << 20];
                loop {
                    let n = fin.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n])?;
                    h.update(&buf[..n]);
                }
                out.flush()?;
                out.sync_all()?;
            }
            if part.metadata()?.len() as i64 != row.size {
                return Err(CopyErr::Other(format!("IOError: size mismatch copying {}", row.name)));
            }
            std::fs::rename(&part, &inc)?;
            let sha: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            copier::place(&inc, row, &sha, cfg, db)?;
            Ok(())
        })();
        match attempt {
            Ok(()) => done += 1,
            Err(e) => {
                let msg = match e {
                    CopyErr::Other(m) => m,
                    CopyErr::Cancelled => "cancelled".into(),
                };
                let _ = db.execute("UPDATE items SET status='adopted', error=?, updated=? WHERE key=?", params![msg, copier::now(), row.key]);
                failed += 1;
            }
        }
        if (i + 1) % 25 == 0 || i + 1 == total {
            progress(&format!("  {}/{}  ok={done} failed={failed}", i + 1, total));
        }
    }
    Ok((done, failed))
}

/// status -> (files, bytes), sorted by status.
pub fn counts(db: &Connection) -> Vec<(String, i64, i64)> {
    db.prepare("SELECT status, COUNT(*), COALESCE(SUM(size),0) FROM items GROUP BY status ORDER BY status")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default()
}

pub fn date_sources(db: &Connection) -> Vec<(String, i64)> {
    db.prepare("SELECT COALESCE(date_src,'-') s, COUNT(*) c FROM items WHERE status='done' GROUP BY s")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default()
}
