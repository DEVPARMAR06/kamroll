//! The HTTP API. Same routes, shapes and guards as the Python engine; parts not ported yet answer plainly.

use crate::{config, layout, manifest, system};
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
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

// ---- pieces that are not ported: honest idle answers so the UI still loads, and 501 for anything that would act ----

async fn device() -> Json<Value> {
    Json(json!({"connected": false, "name": null, "model": null, "ios": null, "error": "not_ported",
                "hint": "The Rust engine does not talk to the iPhone yet. Use the Python engine for backups."}))
}
async fn ollama() -> Json<Value> {
    Json(json!({"reachable": false, "models": [], "error": "Smart tags are not available in the Rust engine yet."}))
}
async fn idle_job() -> Json<Value> {
    Json(json!({"state": "idle", "message": "", "version": 0}))
}
async fn idle_analysis() -> Json<Value> {
    Json(json!({"state": "idle", "done": 0, "total": 0, "message": "", "summary": null, "error": null}))
}
async fn idle_ai() -> Json<Value> {
    Json(json!({"job": {"state": "idle", "kind": null, "done": 0, "total": 0, "message": "", "error": null, "skipped": 0},
                "enabled": false, "model": "", "ready": false, "done": {"photos": 0, "trips": 0}}))
}
async fn idle_health() -> Json<Value> {
    Json(json!({"job": {"state": "idle", "kind": null, "done": 0, "total": 0, "message": "", "error": null, "started_at": null},
                "last": {"sync": null, "deep": null}}))
}
async fn idle_organize() -> Json<Value> {
    Json(json!({"state": "idle", "done": 0, "total": 0, "message": "", "error": null, "result": null}))
}
async fn idle_organize_preview() -> Json<Value> {
    Json(json!({"enabled": false, "into": 0, "back": 0, "total": 0, "samples": []}))
}
async fn no_content() -> StatusCode {
    StatusCode::NO_CONTENT   // an event stream that ends: the page stops reconnecting
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
    let state = Arc::new(AppState { token });
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
        .route("/api/backup", get(idle_job))
        .route("/api/backup/events", get(no_content))
        .route("/api/analysis", get(idle_analysis))
        .route("/api/ai", get(idle_ai))
        .route("/api/health/state", get(idle_health))
        .route("/api/organize", get(idle_organize))
        .route("/api/organize/preview", get(idle_organize_preview))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
