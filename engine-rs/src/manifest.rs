//! Reads from the manifest the Python engine writes (`<backup>/.photovault/manifest.db`). Read-only for now.

use crate::config::Config;
use rusqlite::{params_from_iter, types::Value as Sql, Connection, OpenFlags};
use serde_json::{json, Map, Value};

const PREVIEWABLE: [&str; 3] = ["jpg", "jpeg", "png"];   // what this engine can draw a thumbnail for
const VIDEO: [&str; 3] = ["mov", "mp4", "m4v"];

pub fn open_ro(cfg: &Config) -> Option<Connection> {
    let p = cfg.manifest_path();
    if !p.exists() {
        return None;
    }
    Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

pub fn status(cfg: &Config) -> Value {
    let empty = json!({"has_backup": false, "counts": {}, "total_files": 0});
    let Some(con) = open_ro(cfg) else { return empty };
    let Ok(mut st) = con.prepare("SELECT status, COUNT(*), COALESCE(SUM(size),0) FROM items GROUP BY status") else { return empty };
    let rows: Vec<(String, i64, i64)> = match st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return empty,
    };
    let mut counts = Map::new();
    let mut total = 0;
    for (s, c, b) in rows {
        total += c;
        counts.insert(s, json!({"files": c, "bytes": b}));
    }
    json!({"has_backup": true, "counts": counts, "total_files": total})
}

pub fn review_summary(cfg: &Config) -> Value {
    let empty = json!({"analyzed": false, "total": 0, "counts": {}, "confirmed": 0, "trips": 0, "analysis_at": null});
    let Some(con) = open_ro(cfg) else { return empty };
    let Ok(mut st) = con.prepare("SELECT COALESCE(user_tag, tag) t, COUNT(*) c FROM tags GROUP BY t") else { return empty };
    let rows: Vec<(String, i64)> = match st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return empty,
    };
    if rows.is_empty() {
        return empty;
    }
    let confirmed: i64 = con.query_row("SELECT COUNT(*) FROM tags WHERE user_tag IS NOT NULL", [], |r| r.get(0)).unwrap_or(0);
    let trips: i64 = con.query_row("SELECT COUNT(*) FROM trips", [], |r| r.get(0)).unwrap_or(0);
    let at: Option<f64> = con.query_row("SELECT v FROM meta WHERE k='analysis_at'", [], |r| r.get::<_, String>(0)).ok().and_then(|s| s.parse().ok());
    let total: i64 = rows.iter().map(|r| r.1).sum();
    let counts: Map<String, Value> = rows.into_iter().map(|(k, v)| (k, json!(v))).collect();
    json!({"analyzed": true, "total": total, "counts": counts, "confirmed": confirmed, "trips": trips, "analysis_at": at})
}

pub struct ItemQuery {
    pub tag: Option<String>,
    pub trip: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub order: String,
    pub spread: bool,
    pub q: Option<String>,
}

pub fn review_items(cfg: &Config, q: &ItemQuery) -> Value {
    let empty = json!({"items": [], "total": 0});
    let Some(con) = open_ro(cfg) else { return empty };
    let mut wh: Vec<&str> = vec![];
    let mut args: Vec<Sql> = vec![];
    if let Some(t) = q.tag.as_ref().filter(|t| !t.is_empty()) {
        wh.push("COALESCE(t.user_tag, t.tag) = ?");
        args.push(Sql::Text(t.clone()));
    }
    if let Some(t) = q.trip.as_ref().filter(|t| !t.is_empty()) {
        wh.push("t.trip_id = ?");
        args.push(Sql::Text(t.clone()));
    }
    if let Some(text) = q.q.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let esc = text.replace('!', "!!").replace('%', "!%").replace('_', "!_");
        let like = format!("%{esc}%");
        wh.push("(i.name LIKE ? ESCAPE '!' OR a.tags LIKE ? ESCAPE '!' OR a.caption LIKE ? ESCAPE '!')");
        for _ in 0..3 {
            args.push(Sql::Text(like.clone()));
        }
    }
    let w = if wh.is_empty() { String::new() } else { format!("WHERE {}", wh.join(" AND ")) };
    let Ok(total) = con.query_row(
        &format!("SELECT COUNT(*) FROM tags t JOIN items i ON i.key = t.key LEFT JOIN ai_tags a ON a.key = t.key {w}"),
        params_from_iter(args.iter()),
        |r| r.get::<_, i64>(0),
    ) else {
        return empty;
    };
    let cols = "SELECT t.key, i.name, i.size, i.taken, i.lat, i.lon, t.tag, t.conf, t.reasons, t.user_tag, t.trip_id, \
                a.tags AS ai_tags, a.caption AS ai_caption, a.kind AS ai_kind \
                FROM tags t JOIN items i ON i.key = t.key LEFT JOIN ai_tags a ON a.key = t.key ";
    let oby = if q.order == "time" {
        "ORDER BY i.taken ASC, t.key"
    } else {
        "ORDER BY (t.user_tag IS NOT NULL), t.conf ASC, i.taken DESC"
    };
    let lim = q.limit.clamp(1, 200);
    let (sql, mut qargs) = if q.spread {
        (format!("{cols}{w} {oby} LIMIT 3000"), args.clone())
    } else {
        let mut a = args.clone();
        a.push(Sql::Integer(lim));
        a.push(Sql::Integer(q.offset.max(0)));
        (format!("{cols}{w} {oby} LIMIT ? OFFSET ?"), a)
    };
    let Ok(mut st) = con.prepare(&sql) else { return empty };
    type Row = (String, String, i64, Option<f64>, Option<f64>, String, i64, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let mut rows: Vec<Row> = match st.query_map(params_from_iter(qargs.drain(..)), |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?))
    }) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return empty,
    };
    if q.spread {
        let step = ((rows.len() as i64 + lim - 1) / lim).max(1) as usize;
        rows = rows.into_iter().step_by(step).take(lim as usize).collect();
    }
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(key, name, size, taken, lat, tag, conf, reasons, user_tag, trip_id, ai_tags, ai_caption, ai_kind)| {
            let e = ext(&name);
            let ai = ai_tags.map(|t| json!({"tags": serde_json::from_str::<Value>(&t).unwrap_or(json!([])), "caption": ai_caption, "kind": ai_kind}));
            json!({
                "key": key, "name": name, "size": size, "taken": taken,
                "kind": if VIDEO.contains(&e.as_str()) { "video" } else { "photo" },
                "has_thumb": PREVIEWABLE.contains(&e.as_str()),
                "tag": user_tag.clone().unwrap_or_else(|| tag.clone()), "suggested": tag, "conf": conf,
                "confirmed": user_tag.is_some(), "reasons": serde_json::from_str::<Value>(&reasons).unwrap_or(json!([])),
                "trip_id": trip_id, "gps": lat.is_some(), "ai": ai
            })
        })
        .collect();
    json!({"items": items, "total": total})
}

pub fn trips(cfg: &Config) -> Value {
    let home_cfg = match (cfg.home_lat, cfg.home_lon) {
        (Some(a), Some(b)) => Some(json!([a, b])),
        _ => None,
    };
    let empty = json!({"trips": [], "home": home_cfg, "radius_km": cfg.trip_radius_km});
    let Some(con) = open_ro(cfg) else { return empty };
    let Ok(mut st) = con.prepare("SELECT id, name, auto_name, start, end, days, items, max_km, places FROM trips WHERE items > 0 OR id LIKE 'c-%' ORDER BY start") else {
        return empty;
    };
    type T = (String, Option<String>, Option<String>, String, String, i64, i64, f64, Option<String>);
    let rows: Vec<T> = match st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return empty,
    };
    let mut out = vec![];
    for (id, name, auto_name, start, end, days, items, max_km, places) in rows {
        let keys: Vec<String> = con
            .prepare("SELECT t.key, i.name FROM tags t JOIN items i ON i.key = t.key WHERE t.trip_id = ? ORDER BY i.taken LIMIT 400")
            .ok()
            .map(|mut s| {
                s.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map(|it| it.flatten().filter(|(_, n)| PREVIEWABLE.contains(&ext(n).as_str())).map(|(k, _)| k).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let step = (keys.len() / 14).max(1);
        let sample: Vec<&String> = keys.iter().step_by(step).take(14).collect();
        let ai = con
            .query_row("SELECT title, summary FROM ai_trips WHERE id = ?", [&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .ok()
            .map(|(t, s)| json!({"title": t, "summary": s}));
        let name_s = name.clone().filter(|n| !n.is_empty());
        out.push(json!({
            "id": id, "name": name_s.clone().or(auto_name.clone().filter(|n| !n.is_empty())).unwrap_or_default(), "named": name_s.is_some(),
            "ai": ai, "auto_name": auto_name, "start": start, "end": end, "days": days, "items": items,
            "max_km": max_km.round() as i64, "places": serde_json::from_str::<Value>(&places.unwrap_or_else(|| "[]".into())).unwrap_or(json!([])),
            "sample": sample
        }));
    }
    let home = home_cfg.or_else(|| {
        con.query_row("SELECT v FROM meta WHERE k='home'", [], |r| r.get::<_, String>(0)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
    });
    json!({"trips": out, "home": home, "radius_km": cfg.trip_radius_km})
}
