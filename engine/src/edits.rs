//! Changes you make from the Review and Trips screens: confirm or change tags, and add, take out, create,
//! rename and delete trips. Same rules and the same rows as the Python engine, so either can read the other's work.

use crate::config::Config;
use chrono::{Duration, Local, NaiveDate, TimeZone};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TAGS: [&str; 7] = ["memory", "trip", "screenshot", "social", "document", "duplicate", "junk"];

const EXTRA_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trip_edits (key TEXT PRIMARY KEY, trip_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS trip_custom (id TEXT PRIMARY KEY, name TEXT, start TEXT, end TEXT);
CREATE TABLE IF NOT EXISTS trip_deleted (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS moves (key TEXT PRIMARY KEY, src TEXT, dst TEXT, reason TEXT);
CREATE TABLE IF NOT EXISTS filing_origin (key TEXT PRIMARY KEY, origin TEXT);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
";

/// Open the manifest for writing. Creates the small tables edits need if an older manifest lacks them.
pub fn open_rw(cfg: &Config) -> Option<Connection> {
    let p = cfg.manifest_path();
    if !p.exists() {
        return None;
    }
    let con = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    let _ = con.busy_timeout(std::time::Duration::from_secs(10));
    let _ = con.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;");
    con.execute_batch(EXTRA_SCHEMA).ok()?;
    Some(con)
}

fn json_list(items: &[&str]) -> String {
    crate::tagging::json_list(items)
}

pub fn recount_trips(db: &Connection) -> rusqlite::Result<()> {
    let mut counts = std::collections::HashMap::<String, i64>::new();
    {
        let mut st = db.prepare("SELECT trip_id, COUNT(*) FROM tags WHERE trip_id IS NOT NULL GROUP BY trip_id")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
            counts.insert(r.0, r.1);
        }
    }
    let ids: Vec<String> = db.prepare("SELECT id FROM trips")?.query_map([], |r| r.get(0))?.flatten().collect();
    for id in ids {
        db.execute("UPDATE trips SET items=? WHERE id=?", params![counts.get(&id).copied().unwrap_or(0), id])?;
    }
    Ok(())
}

/// Put photos into a trip. Remembered, so a later re-analysis keeps it.
pub fn assign_to_trip(db: &Connection, keys: &[String], trip_id: &str) -> rusqlite::Result<i64> {
    let mut n = 0;
    for k in keys {
        if db.query_row("SELECT 1 FROM tags WHERE key=?", [k], |_| Ok(())).is_err() {
            continue;
        }
        db.execute("INSERT OR REPLACE INTO trip_edits VALUES(?,?)", params![k, trip_id])?;
        db.execute("UPDATE tags SET trip_id=? WHERE key=?", params![trip_id, k])?;
        db.execute(
            "UPDATE tags SET tag='trip', conf=95, reasons=? WHERE key=? AND tag='memory' AND user_tag IS NULL",
            params![json_list(&["Added to the trip by you"]), k],
        )?;
        db.execute("UPDATE tags SET user_tag='trip' WHERE key=? AND user_tag='memory'", [k])?;
        n += 1;
    }
    recount_trips(db)?;
    Ok(n)
}

/// Take photos out of their trip. They go back to Memory unless you tagged them something else.
pub fn remove_from_trip(db: &Connection, keys: &[String], trip_id: Option<&str>) -> rusqlite::Result<i64> {
    let mut n = 0;
    for k in keys {
        let current: Option<Option<String>> = db.query_row("SELECT trip_id FROM tags WHERE key=?", [k], |r| r.get(0)).ok();
        let Some(Some(cur)) = current else { continue };
        if trip_id.is_some_and(|t| t != cur) {
            continue;
        }
        db.execute("INSERT OR REPLACE INTO trip_edits VALUES(?, '')", [k])?;
        db.execute("UPDATE tags SET trip_id=NULL WHERE key=?", [k])?;
        db.execute(
            "UPDATE tags SET tag='memory', conf=80, reasons=? WHERE key=? AND tag='trip' AND user_tag IS NULL",
            params![json_list(&["Taken out of the trip by you"]), k],
        )?;
        db.execute("UPDATE tags SET user_tag='memory' WHERE key=? AND user_tag='trip'", [k])?;
        n += 1;
    }
    recount_trips(db)?;
    Ok(n)
}

pub fn set_tags(db: &Connection, keys: &[String], tag: Option<&str>) -> rusqlite::Result<()> {
    for k in keys {
        match tag {
            None => db.execute("UPDATE tags SET user_tag = tag WHERE key = ?", [k])?,
            Some(t) => db.execute("UPDATE tags SET user_tag = ? WHERE key = ?", params![t, k])?,
        };
    }
    Ok(())
}

pub fn rename_trip(db: &Connection, id: &str, name: &str) -> rusqlite::Result<usize> {
    let name = name.trim();
    let changed = db.execute("UPDATE trips SET name=? WHERE id=?", params![if name.is_empty() { None } else { Some(name) }, id])?;
    if id.starts_with("c-") {
        db.execute("UPDATE trip_custom SET name=? WHERE id=?", params![if name.is_empty() { "Trip" } else { name }, id])?;
    }
    Ok(changed)
}

fn local_ts(d: NaiveDate) -> f64 {
    let naive = d.and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp() as f64).unwrap_or(0.0)
}

pub enum CreateError {
    Dates,
    Order,
    Db(rusqlite::Error),
}

/// A trip of your own, for example a train journey with no GPS. Photos taken on its days join it.
pub fn create_trip(db: &Connection, name: &str, start: &str, end: &str, extra: &[String]) -> Result<(String, i64), CreateError> {
    let d0 = NaiveDate::parse_from_str(start, "%Y-%m-%d").map_err(|_| CreateError::Dates)?;
    let d1 = NaiveDate::parse_from_str(end, "%Y-%m-%d").map_err(|_| CreateError::Dates)?;
    if d1 < d0 {
        return Err(CreateError::Order);
    }
    let name = name.trim();
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let tid = format!("c-{ms:x}");
    let run = || -> rusqlite::Result<i64> {
        db.execute("INSERT INTO trip_custom VALUES(?,?,?,?)", params![tid, name, start, end])?;
        db.execute(
            "INSERT INTO trips VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![tid, name, "", start, end, (d1 - d0).num_days() + 1, 0, 0, 0, 0, "[]"],
        )?;
        let (lo, hi) = (local_ts(d0), local_ts(d1 + Duration::days(1)));
        let mut keys: Vec<String> = db
            .prepare("SELECT t.key FROM tags t JOIN items i ON i.key = t.key WHERE i.taken >= ? AND i.taken < ?")?
            .query_map(params![lo, hi], |r| r.get(0))?
            .flatten()
            .collect();
        keys.extend(extra.iter().cloned());
        let mut seen = std::collections::HashSet::new();
        keys.retain(|k| seen.insert(k.clone()));
        assign_to_trip(db, &keys, &tid)
    };
    run().map(|n| (tid.clone(), n)).map_err(CreateError::Db)
}

/// Dissolve a trip. Its photos stay on the SSD and go back to Memory; nothing is deleted.
pub fn delete_trip(db: &Connection, id: &str) -> rusqlite::Result<Option<i64>> {
    if db.query_row("SELECT 1 FROM trips WHERE id=?", [id], |_| Ok(())).is_err() {
        return Ok(None);
    }
    let keys: Vec<String> = db.prepare("SELECT key FROM tags WHERE trip_id=?")?.query_map([id], |r| r.get(0))?.flatten().collect();
    remove_from_trip(db, &keys, Some(id))?;
    db.execute("DELETE FROM trip_edits WHERE trip_id=?", [id])?;
    for k in &keys {
        db.execute("DELETE FROM trip_edits WHERE key=? AND trip_id=''", [k])?;
    }
    if id.starts_with("c-") {
        db.execute("DELETE FROM trip_custom WHERE id=?", [id])?;
    } else {
        db.execute("INSERT OR IGNORE INTO trip_deleted VALUES(?)", [id])?;
    }
    db.execute("DELETE FROM trips WHERE id=?", [id])?;
    recount_trips(db)?;
    Ok(Some(keys.len() as i64))
}

pub fn ok_changed(n: i64) -> Value {
    json!({"changed": n})
}
