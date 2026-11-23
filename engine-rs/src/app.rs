//! The HTTP API. Same routes, shapes and guards as the Python engine; parts not ported yet answer plainly.

use crate::{analysis, backup, config, copier, device, edits, filing, layout, manifest, system, verify};
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use include_dir::{include_dir, Dir};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

static UI: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../src/photovault/ui");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const NOT_PORTED: &str = "This part of the engine has not been ported to Rust yet. Use the Python engine for backups, checks and filing.";

pub struct AppState {
    pub token: String,
    pub health: verify::Shared,
    pub org: filing::SharedOrg,
    pub analysis: analysis::Shared,
    pub backup: backup::Manager,
}
type St = State<Arc<AppState>>;
type Q = Query<HashMap<String, String>>;

fn detail(code: u16, msg: &str) -> Response {
    (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(json!({"detail": msg}))).into_response()
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Only this computer, and only with the per-launch token: blocks other web pages and DNS rebinding.
async fn guard(State(st): St, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_string();
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return detail(403, "Forbidden host");
    }
    if req.uri().path().starts_with("/api") {
        let from_header = req.headers().get("x-photovault-token").and_then(|h| h.to_str().ok()).map(String::from);
        let from_query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token="))).map(String::from);
        let sent = from_header.or(from_query).unwrap_or_default();
        if !same(&sent, &st.token) {
            return detail(401, "Missing or wrong token");
        }
    }
    next.run(req).await
}

fn current() -> (Option<config::Config>, std::path::PathBuf) {
    match config::find_config() {
        Some(p) => (config::load(&p).ok(), p),
        None => (None, config::default_config_path()),
    }
}

fn config_json(cfg: &config::Config, path: &std::path::Path, exists: bool) -> Value {
    json!({"config": cfg, "path": path.to_string_lossy().replace('\\', "/"), "exists": exists, "backup_root": cfg.backup_root_string()})
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": VERSION}))
}

async fn get_config() -> Json<Value> {
    let (cfg, path) = current();
    let exists = config::find_config().is_some();
    Json(config_json(&cfg.unwrap_or_default(), &path, exists))
}

async fn put_config(Json(body): Json<Value>) -> Response {
    match config::validate(&body) {
        Err(errs) => (StatusCode::UNPROCESSABLE_ENTITY, Json(config::errors_json(&errs))).into_response(),
        Ok(cfg) => {
            let path = config::find_config().unwrap_or_else(config::default_config_path);
            if let Err(e) = config::save(&cfg, &path) {
                return detail(500, &format!("Could not save the settings: {e}"));
            }
            Json(config_json(&cfg, &path, true)).into_response()
        }
    }
}

async fn volumes() -> Json<Value> {
    Json(Value::Array(system::list_volumes()))
}

async fn fs(Query(q): Q) -> Response {
    match system::list_dirs(q.get("path").map(String::as_str).unwrap_or("")) {
        Ok(v) => Json(v).into_response(),
        Err((code, msg)) => detail(code, msg),
    }
}

fn missing(field: &str) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": [{"loc": ["query", field], "msg": "Field required", "type": "missing"}]}))).into_response()
}

async fn path_check(Query(q): Q) -> Response {
    match q.get("path") {
        Some(p) => Json(system::path_check(p)).into_response(),
        None => missing("path"),
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

async fn layout_preview(Query(q): Q) -> Response {
    let Some(lay) = q.get("layout") else { return missing("layout") };
    let undated = q.get("undated").cloned().unwrap_or_else(|| "Undated/{name}".into());
    for (field, t) in [("layout", lay.as_str()), ("undated_layout", undated.as_str())] {
        if let Some(p) = layout::check_layout(t) {
            return Json(json!({"error": p, "field": field, "samples": []})).into_response();
        }
    }
    let trip_layout = q.get("trip_layout").filter(|s| !s.is_empty());
    let mut trip_sample = Value::Null;
    if let Some(t) = trip_layout {
        if let Some(p) = layout::check_trip_layout(t) {
            return Json(json!({"error": p, "field": "trip_layout", "samples": []})).into_response();
        }
        let vars = vec![
            ("year", "2025".to_string()), ("month", "12".into()), ("day", "27".into()), ("name", "IMG_0442.HEIC".into()),
            ("stem", "IMG_0442".into()), ("ext", "HEIC".into()), ("trip", "Udaipur".into()),
            ("trip_dated", "Udaipur (Dec 2025)".into()), ("trip_year", "2025".into()),
        ];
        trip_sample = layout::format(t, &vars).map(Value::String).unwrap_or(Value::Null);
    }
    let samples: Vec<Value> = [("IMG_0442.HEIC", Some((2025, 12, 27))), ("IMG_9820.MOV", Some((2024, 8, 24))), ("IMG_2147.PNG", None)]
        .iter()
        .map(|(name, date)| {
            let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
            let (y, m, d) = date.unwrap_or((2000, 1, 1));
            let vars = vec![
                ("year", format!("{y:04}")), ("month", format!("{m:02}")), ("day", format!("{d:02}")),
                ("name", name.to_string()), ("stem", stem.to_string()), ("ext", ext.to_string()),
            ];
            let tmpl = if date.is_some() { lay.as_str() } else { undated.as_str() };
            let path = layout::format(tmpl, &vars).unwrap_or_default().replace('\\', "/");
            json!({"name": name, "date": date.map(|(y, m, d)| format!("{d:02} {} {y}", MONTHS[(m - 1) as usize])), "path": path})
        })
        .collect();
    Json(json!({"error": null, "field": null, "samples": samples, "trip_sample": trip_sample})).into_response()
}

fn need_cfg() -> Result<config::Config, Response> {
    current().0.ok_or_else(|| detail(400, "Save your settings first."))
}

async fn status() -> Response {
    match current().0 {
        Some(cfg) => Json(manifest::status(&cfg)).into_response(),
        None => Json(json!({"has_backup": false, "counts": {}, "total_files": 0})).into_response(),
    }
}

async fn review_summary() -> Response {
    match need_cfg() {
        Ok(cfg) => Json(manifest::review_summary(&cfg)).into_response(),
        Err(r) => r,
    }
}

async fn review_items(Query(q): Q) -> Response {
    let cfg = match need_cfg() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let num = |k: &str, d: i64| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let query = manifest::ItemQuery {
        tag: q.get("tag").cloned(),
        trip: q.get("trip").cloned(),
        limit: num("limit", 60),
        offset: num("offset", 0),
        order: q.get("order").cloned().unwrap_or_else(|| "conf".into()),
        spread: matches!(q.get("spread").map(String::as_str), Some("true" | "1")),
        q: q.get("q").cloned(),
    };
    Json(manifest::review_items(&cfg, &query)).into_response()
}

async fn trips() -> Response {
    match need_cfg() {
        Ok(cfg) => Json(manifest::trips(&cfg)).into_response(),
        Err(r) => r,
    }
}

async fn thumb(Query(q): Q) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let Some(key) = q.get("key") else { return missing("key") };
    let size = q.get("size").and_then(|s| s.parse::<i64>().ok()).unwrap_or(320).clamp(64, 1280) as u32;
    let dest: Option<String> = manifest::open_ro(&cfg).and_then(|c| c.query_row("SELECT dest FROM items WHERE key=?", [key], |r| r.get::<_, Option<String>>(0)).ok().flatten());
    let Some(dest) = dest.filter(|d| !d.is_empty()) else { return detail(404, "Unknown file") };
    let root = cfg.backup_root();
    let src = root.join(&dest);
    let inside = match (src.canonicalize(), root.canonicalize()) { (Ok(s), Ok(r)) => s.starts_with(&r) && s.is_file(), _ => false };
    if !inside {
        return detail(404, "File not found");
    }
    let cache = crate::thumbs::cache_path(&root, key, size);
    if !cache.exists() {
        let s2 = src.clone();
        let data = tokio::task::spawn_blocking(move || crate::thumbs::make_thumbnail(&s2, size)).await.ok().flatten();
        let Some(data) = data else { return detail(404, "No preview") };
        if let Some(p) = cache.parent() { let _ = std::fs::create_dir_all(p); }
        let _ = std::fs::write(&cache, &data);
    }
    match std::fs::read(&cache) {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response(),
        Err(_) => detail(404, "No preview"),
    }
}

// ---- edits: tags and trips ----

fn bad(loc: &[&str], msg: &str, typ: &str) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": [{"loc": loc, "msg": msg, "type": typ}]}))).into_response()
}

/// `keys` must be a list of 1..=max strings, as the Python models require.
fn keys_of(body: &Value, min: usize, max: usize) -> Result<Vec<String>, Response> {
    let Some(arr) = body.get("keys").and_then(|v| v.as_array()) else {
        return Err(bad(&["body", "keys"], "Field required", "missing"));
    };
    if arr.len() < min {
        return Err(bad(&["body", "keys"], &format!("List should have at least {min} item after validation, not 0"), "too_short"));
    }
    if arr.len() > max {
        return Err(bad(&["body", "keys"], &format!("List should have at most {max} items after validation, not {}", arr.len()), "too_long"));
    }
    arr.iter().map(|v| v.as_str().map(String::from).ok_or_else(|| bad(&["body", "keys"], "Input should be a valid string", "string_type"))).collect()
}

fn open_db(cfg: &config::Config) -> Result<rusqlite::Connection, Response> {
    edits::open_rw(cfg).ok_or_else(|| detail(500, "The backup has no manifest yet."))
}

fn db_err(e: rusqlite::Error) -> Response {
    detail(500, &format!("Could not save: {e}"))
}

async fn set_tag(Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let keys = match keys_of(&body, 1, 500) { Ok(k) => k, Err(r) => return r };
    let tag = match body.get("tag") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(t.clone()),
        Some(_) => return bad(&["body", "tag"], "Input should be a valid string", "string_type"),
    };
    if tag.as_deref().is_some_and(|t| !edits::TAGS.contains(&t)) {
        return detail(422, "Unknown tag");
    }
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    match edits::set_tags(&db, &keys, tag.as_deref()) {
        Ok(()) => Json(json!({"ok": true, "changed": keys.len()})).into_response(),
        Err(e) => db_err(e),
    }
}

async fn rename_trip(axum::extract::Path(id): axum::extract::Path<String>, Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let Some(name) = body.get("name").and_then(|v| v.as_str()) else { return bad(&["body", "name"], "Field required", "missing") };
    if name.chars().count() > 80 {
        return bad(&["body", "name"], "String should have at most 80 characters", "string_too_long");
    }
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    match edits::rename_trip(&db, &id, name) {
        Ok(0) => detail(404, "Unknown trip"),
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => db_err(e),
    }
}

async fn create_trip(Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let name = body.get("name").and_then(|v| v.as_str());
    let name = match name {
        None => return bad(&["body", "name"], "Field required", "missing"),
        Some(n) if n.is_empty() => return bad(&["body", "name"], "String should have at least 1 character", "string_too_short"),
        Some(n) if n.chars().count() > 80 => return bad(&["body", "name"], "String should have at most 80 characters", "string_too_long"),
        Some(n) => n,
    };
    let date = |f: &str| -> Result<String, Response> {
        let v = body.get(f).and_then(|v| v.as_str()).ok_or_else(|| bad(&["body", f], "Field required", "missing"))?;
        let b = v.as_bytes();
        let ok = b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
        if ok { Ok(v.to_string()) } else { Err(bad(&["body", f], "String should match pattern '^\\d{4}-\\d{2}-\\d{2}$'", "string_pattern_mismatch")) }
    };
    let (start, end) = match (date("start"), date("end")) {
        (Ok(s), Ok(e)) => (s, e),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let extra = if body.get("keys").is_some() { match keys_of(&body, 0, 2000) { Ok(k) => k, Err(r) => return r } } else { vec![] };
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    match edits::create_trip(&db, name, &start, &end, &extra) {
        Ok((id, n)) => Json(json!({"id": id, "files": n})).into_response(),
        Err(edits::CreateError::Dates) => detail(422, "Those dates are not valid."),
        Err(edits::CreateError::Order) => detail(422, "The end date is before the start date."),
        Err(edits::CreateError::Db(e)) => db_err(e),
    }
}

async fn add_to_trip(axum::extract::Path(id): axum::extract::Path<String>, Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let keys = match keys_of(&body, 1, 2000) { Ok(k) => k, Err(r) => return r };
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    if db.query_row("SELECT 1 FROM trips WHERE id=?", [&id], |_| Ok(())).is_err() {
        return detail(404, "Unknown trip");
    }
    match edits::assign_to_trip(&db, &keys, &id) {
        Ok(n) => Json(edits::ok_changed(n)).into_response(),
        Err(e) => db_err(e),
    }
}

async fn take_out_of_trip(axum::extract::Path(id): axum::extract::Path<String>, Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let keys = match keys_of(&body, 1, 2000) { Ok(k) => k, Err(r) => return r };
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    match edits::remove_from_trip(&db, &keys, Some(&id)) {
        Ok(n) => Json(edits::ok_changed(n)).into_response(),
        Err(e) => db_err(e),
    }
}

async fn delete_trip(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    match edits::delete_trip(&db, &id) {
        Ok(None) => detail(404, "Unknown trip"),
        Ok(Some(n)) => Json(json!({"ok": true, "released": n})).into_response(),
        Err(e) => db_err(e),
    }
}

// ---- Health: the SSD half. Comparing with the phone still needs the Python engine. ----

async fn health_state(State(st): St) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    let db = rusqlite::Connection::open_with_flags(cfg.manifest_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    let job = st.health.lock().unwrap().snapshot();
    Json(json!({"job": job, "last": verify::last(db.as_ref())})).into_response()
}

async fn health_start(State(st): St, Json(body): Json<Value>) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    match body.get("kind").and_then(|v| v.as_str()) {
        Some("deep") | Some("sync") => {}
        _ => return bad(&["body", "kind"], "Input should be 'sync' or 'deep'", "literal_error"),
    }
    if st.health.lock().unwrap().active() {
        return detail(409, "A check is already running.");
    }
    if st.backup.active() || st.org.lock().unwrap().active() {
        return detail(409, "A backup is running. Wait for it to finish first.");
    }
    if !cfg.manifest_path().exists() {
        return detail(400, "There is nothing backed up yet.");
    }
    if body.get("kind").and_then(|v| v.as_str()) == Some("sync") {
        verify::spawn_sync(st.health.clone(), cfg);
    } else {
        verify::spawn_deep(st.health.clone(), cfg.manifest_path(), cfg.backup_root());
    }
    Json(st.health.lock().unwrap().snapshot()).into_response()
}

async fn health_cancel(State(st): St) -> Response {
    let j = st.health.lock().unwrap();
    if !j.active() {
        return detail(409, "Nothing is running.");
    }
    j.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    Json(j.snapshot()).into_response()
}

async fn health_repair(State(st): St) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    if st.health.lock().unwrap().active() {
        return detail(409, "Wait for the running check to finish.");
    }
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    let mut problems: Vec<String> = vec![];
    for kind in ["sync", "deep"] {
        if let Ok(text) = db.query_row("SELECT v FROM meta WHERE k=?", [format!("health_{kind}")], |r| r.get::<_, String>(0)) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                for f in ["missing", "wrong_size", "corrupt"] {
                    problems.extend(v.get(f).and_then(|x| x.as_array()).into_iter().flatten().filter_map(|s| s.as_str().map(String::from)));
                }
            }
        }
    }
    if problems.is_empty() {
        return detail(409, "No problems recorded.");
    }
    let res = verify::repair_problems(&db, &cfg.backup_root(), &problems);
    let _ = db.execute("DELETE FROM meta WHERE k IN ('health_sync', 'health_deep')", []);
    Json(res).into_response()
}

// ---- filing ----

async fn org_state(State(st): St) -> Json<Value> {
    Json(st.org.lock().unwrap().snapshot())
}

async fn org_preview() -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    if !cfg.manifest_path().exists() {
        return Json(json!({"enabled": cfg.file_trips, "into": 0, "back": 0, "total": 0, "samples": []})).into_response();
    }
    let db = match open_db(&cfg) { Ok(d) => d, Err(r) => return r };
    filing::recover_moves(&db, &cfg.backup_root());
    let moves = filing::plan_filing(&db, &cfg);
    let into = moves.iter().filter(|m| m.reason == "into-trip").count();
    let samples: Vec<Value> = moves.iter().take(12).map(|m| json!({"name": m.name, "from": m.src, "to": m.dst, "reason": m.reason})).collect();
    Json(json!({"enabled": cfg.file_trips, "into": into, "back": moves.len() - into, "total": moves.len(), "samples": samples})).into_response()
}

async fn org_start(State(st): St) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    if st.org.lock().unwrap().active() {
        return detail(409, "Filing is already running.");
    }
    if st.health.lock().unwrap().active() || st.backup.active() {
        return detail(409, "A backup or check is running. Wait for it to finish first.");
    }
    if !cfg.manifest_path().exists() {
        return detail(400, "There is nothing backed up yet.");
    }
    filing::spawn_apply(st.org.clone(), cfg);
    Json(st.org.lock().unwrap().snapshot()).into_response()
}

async fn org_cancel(State(st): St) -> Response {
    let j = st.org.lock().unwrap();
    if !j.active() {
        return detail(409, "Nothing is running.");
    }
    j.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    Json(j.snapshot()).into_response()
}

// ---- pieces that are not ported: honest idle answers so the UI still loads, and 501 for anything that would act ----

async fn device() -> Json<Value> {
    let r = tokio::task::spawn_blocking(|| device::make_device().info()).await.unwrap_or_else(|_| Err("timeout".into()));
    Json(match r {
        Ok(i) => json!({"connected": true, "name": i.name, "model": i.model, "ios": i.ios, "error": null, "hint": null}),
        Err(e) => {
            let (code, hint) = device::explain_device_error(&e);
            json!({"connected": false, "name": null, "model": null, "ios": null, "error": code, "hint": hint})
        }
    })
}
async fn ollama() -> Json<Value> {
    Json(json!({"reachable": false, "models": [], "error": "Smart tags are not available in the Rust engine yet."}))
}
async fn backup_state(State(st): St) -> Json<Value> {
    Json(st.backup.snapshot())
}

async fn backup_start(State(st): St, body: Option<Json<Value>>) -> Response {
    let body = body.map(|b| b.0).unwrap_or(json!({}));
    let limit = match body.get("limit") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(n) if (1..=1_000_000).contains(&n) => Some(n as usize),
            _ => return bad(&["body", "limit"], "Input should be a valid integer", "int_parsing"),
        },
    };
    let only = body.get("only_reencoded_originals").and_then(|v| v.as_bool()).unwrap_or(false);
    if st.backup.active() {
        return detail(409, "A backup is already running.");
    }
    if st.org.lock().unwrap().active() {
        return detail(409, "Files are being filed into trip folders. Wait for that to finish first.");
    }
    let Some(path) = config::find_config() else { return detail(400, "Save your settings first.") };
    let Ok(cfg) = config::load(&path) else { return detail(400, "Save your settings first.") };
    let job = backup::BackupJob::new();
    *st.backup.job.lock().unwrap() = Some(job.clone());
    job.run(cfg, copier::RunOptions { limit, only_reencoded: only });
    Json(st.backup.snapshot()).into_response()
}

fn need_job(st: &AppState, states: &[&str]) -> Result<std::sync::Arc<backup::BackupJob>, Response> {
    match st.backup.current() {
        Some(j) if states.contains(&j.state().as_str()) => Ok(j),
        _ => Err(detail(409, "That is not possible in the current state.")),
    }
}

async fn backup_pause(State(st): St) -> Response {
    match need_job(&st, &["scanning", "copying"]) { Ok(j) => { j.pause(); Json(st.backup.snapshot()).into_response() } Err(r) => r }
}
async fn backup_resume(State(st): St) -> Response {
    match need_job(&st, &["paused"]) { Ok(j) => { j.resume(); Json(st.backup.snapshot()).into_response() } Err(r) => r }
}
async fn backup_cancel(State(st): St) -> Response {
    match need_job(&st, &["starting", "scanning", "copying", "paused"]) { Ok(j) => { j.cancel(); Json(st.backup.snapshot()).into_response() } Err(r) => r }
}

/// Server-sent events: a full snapshot whenever something changes, and at least once a second.
async fn backup_events(State(st): St) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(8);
    let st2 = st.clone();
    tokio::spawn(async move {
        let (mut last, mut idle) = (u64::MAX, 0.0f64);
        loop {
            let snap = st2.backup.snapshot();
            let ver = snap.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
            if ver != last || idle >= 1.0 {
                last = ver;
                idle = 0.0;
                if tx.send(Ok(axum::body::Bytes::from(format!("data: {}

", snap)))).await.is_err() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            idle += 0.2;
        }
    });
    let mut r = Response::new(axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
async fn analysis_state(State(st): St) -> Json<Value> {
    Json(st.analysis.lock().unwrap().snapshot())
}
async fn analysis_start(State(st): St) -> Response {
    let cfg = match need_cfg() { Ok(c) => c, Err(r) => return r };
    if st.analysis.lock().unwrap().active() {
        return detail(409, "Already analysing.");
    }
    if !cfg.manifest_path().exists() {
        return detail(400, "There is nothing backed up yet.");
    }
    analysis::spawn(st.analysis.clone(), cfg);
    Json(st.analysis.lock().unwrap().snapshot()).into_response()
}
async fn idle_ai() -> Json<Value> {
    Json(json!({"job": {"state": "idle", "kind": null, "done": 0, "total": 0, "message": "", "error": null, "skipped": 0},
                "enabled": false, "model": "", "ready": false, "done": {"photos": 0, "trips": 0}}))
}
async fn fallback(req: Request) -> Response {
    if req.uri().path().starts_with("/api") {
        detail(501, NOT_PORTED)
    } else {
        detail(404, "Not found")
    }
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

async fn static_file(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match UI.get_file(&path) {
        Some(f) => ([(header::CONTENT_TYPE, mime(&path))], f.contents()).into_response(),
        None => detail(404, "Not found"),
    }
}

async fn index(State(st): St) -> Response {
    let html = UI.get_file("index.html").and_then(|f| f.contents_utf8()).unwrap_or("").replace("__TOKEN__", &st.token);
    let mut r = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub fn router(token: String) -> Router {
    let state = Arc::new(AppState { token, health: Default::default(), org: Default::default(), analysis: Default::default(), backup: Default::default() });
    Router::new()
        .route("/", get(index))
        .route("/static/{*path}", get(static_file))
        .route("/api/health", get(health))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/volumes", get(volumes))
        .route("/api/fs", get(fs))
        .route("/api/path-check", get(path_check))
        .route("/api/layout-preview", get(layout_preview))
        .route("/api/status", get(status))
        .route("/api/review/summary", get(review_summary))
        .route("/api/review/items", get(review_items))
        .route("/api/trips", get(trips))
        .route("/api/device", get(device))
        .route("/api/ollama", get(ollama))
        .route("/api/backup", get(backup_state))
        .route("/api/backup/start", post(backup_start))
        .route("/api/backup/pause", post(backup_pause))
        .route("/api/backup/resume", post(backup_resume))
        .route("/api/backup/cancel", post(backup_cancel))
        .route("/api/backup/events", get(backup_events))
        .route("/api/analysis", get(analysis_state))
        .route("/api/analysis/start", post(analysis_start))
        .route("/api/ai", get(idle_ai))
        .route("/api/thumb", get(thumb))
        .route("/api/review/tag", post(set_tag))
        .route("/api/trips", post(create_trip))
        .route("/api/trips/{id}", axum::routing::put(rename_trip).delete(delete_trip))
        .route("/api/trips/{id}/items", post(add_to_trip))
        .route("/api/trips/{id}/remove", post(take_out_of_trip))
        .route("/api/health/state", get(health_state))
        .route("/api/health/start", post(health_start))
        .route("/api/health/cancel", post(health_cancel))
        .route("/api/health/repair", post(health_repair))
        .route("/api/organize", get(org_state))
        .route("/api/organize/preview", get(org_preview))
        .route("/api/organize/start", post(org_start))
        .route("/api/organize/cancel", post(org_cancel))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
