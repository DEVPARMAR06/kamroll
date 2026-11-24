//! Optional smart tags through a local Ollama. Nothing here runs unless the person turns it on.
//!
//! Two jobs: suggest a title for a trip (any chat model), and describe a picture in a few words (needs a model
//! that can see images). Everything goes to this computer only; other addresses are refused.

use crate::config::{is_loopback, Config};
use base64::Engine;
use regex::Regex;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KINDS: [&str; 5] = ["memory", "screenshot", "social", "document", "junk"];
const VISION_HINTS: [&str; 9] = ["llava", "moondream", "-vl", "vl:", "vision", "minicpm-v", "gemma3", "bakllava", "granite3.2-vision"];

/// A readable reason the model could not help. The app shows it as is.
#[derive(Debug)]
pub struct AiError(pub String);

fn post(host: &str, path: &str, payload: &Value, timeout: f64) -> Result<Value, AiError> {
    if !is_loopback(host) {
        return Err(AiError("Only a local Ollama (127.0.0.1 or localhost) is allowed.".into()));
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs_f64(timeout)).build();
    let url = format!("{}{}", host.trim_end_matches('/'), path);
    match agent.post(&url).set("Content-Type", "application/json").send_string(&payload.to_string()) {
        Ok(r) => r.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(|| AiError("Ollama did not answer. Check that it is running.".into())),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from))).unwrap_or_default();
            Err(AiError(if detail.is_empty() { format!("Ollama answered with an error ({code}).") } else { detail }))
        }
        Err(_) => Err(AiError("Ollama did not answer. Check that it is running.".into())),
    }
}

/// Models sometimes wrap the JSON in words. Take the first object that parses.
pub fn parse_json(text: &str) -> Result<Value, AiError> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.is_object() {
            return Ok(v);
        }
    }
    let re = Regex::new(r"(?s)\{.*?\}").unwrap();
    for m in re.find_iter(text) {
        if let Ok(v) = serde_json::from_str::<Value>(m.as_str()) {
            if v.is_object() {
                return Ok(v);
            }
        }
    }
    Err(AiError("The model did not return a usable answer.".into()))
}

pub fn chat_json(host: &str, model: &str, prompt: &str, images: &[Vec<u8>], timeout: f64) -> Result<Value, AiError> {
    let mut msg = json!({"role": "user", "content": prompt});
    if !images.is_empty() {
        msg["images"] = json!(images.iter().map(|b| base64::engine::general_purpose::STANDARD.encode(b)).collect::<Vec<_>>());
    }
    let out = post(host, "/api/chat", &json!({"model": model, "messages": [msg], "stream": false, "format": "json", "options": {"temperature": 0.2}}), timeout)?;
    parse_json(out.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or(""))
}

fn clean(v: Option<&Value>, limit: usize, trailing: &str) -> String {
    let text = v.and_then(|x| x.as_str()).unwrap_or("");
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = collapsed.trim_matches(|c| c == ' ' || c == '"' || c == '\'');
    let t = if trailing.is_empty() { t } else { t.trim_end_matches(|c| trailing.contains(c)) };
    t.chars().take(limit).collect()
}

pub struct TripFacts<'a> {
    pub places: Vec<String>,
    pub start: &'a str,
    pub end: &'a str,
    pub days: i64,
    pub files: i64,
    pub mix: Vec<(String, i64)>,
}

/// (title, summary) for a trip, from facts the app already knows.
pub fn suggest_trip(host: &str, model: &str, f: &TripFacts) -> Result<(String, String), AiError> {
    let kinds = f.mix.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ");
    let prompt = format!(
        "You name photo albums for a person's trips. Reply with JSON only: {{\"title\": \"a warm title of two to five words\", \"summary\": \"one friendly sentence\"}}. Do not invent facts. Use only what is given.\nPlaces visited: {}.\nDates: {} to {} ({} day{}).\nFiles: {} ({}).",
        if f.places.is_empty() { "unknown".to_string() } else { f.places.join(", ") },
        f.start,
        f.end,
        f.days,
        if f.days != 1 { "s" } else { "" },
        f.files,
        if kinds.is_empty() { "photos and videos".to_string() } else { kinds }
    );
    let out = chat_json(host, model, &prompt, &[], 180.0)?;
    let title = clean(out.get("title"), 60, ".");
    let summary = clean(out.get("summary"), 220, "");
    if title.is_empty() {
        return Err(AiError("The model did not suggest a title.".into()));
    }
    Ok((title, summary))
}

const VISION_PROMPT: &str = "Look at this picture. Reply with JSON only: {\"tags\": [\"up to five short lowercase words for what it shows, such as beach, sunset, food, friends\"], \"caption\": \"one short sentence\", \"kind\": \"one of memory, screenshot, social, document, junk\"}. Use document for receipts, forms, whiteboards and printed pages. Use junk for blurry, dark or accidental shots.";

pub struct Picture {
    pub tags: Vec<String>,
    pub caption: String,
    pub kind: Option<String>,
}

pub fn describe_picture(host: &str, model: &str, jpeg: &[u8]) -> Result<Picture, AiError> {
    let out = chat_json(host, model, VISION_PROMPT, &[jpeg.to_vec()], 180.0)?;
    let raw: Vec<Value> = match out.get("tags") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => s.split(',').map(|x| json!(x)).collect(),
        _ => vec![],
    };
    let mut tags: Vec<String> = vec![];
    for t in raw {
        let t = clean(Some(&t), 24, ".").to_lowercase();
        if !t.is_empty() && !tags.contains(&t) {
            tags.push(t);
        }
    }
    tags.truncate(5);
    let kind = clean(out.get("kind"), 20, ".").to_lowercase();
    Ok(Picture { tags, caption: clean(out.get("caption"), 160, ""), kind: if KINDS.contains(&kind.as_str()) { Some(kind) } else { None } })
}

/// Is a local Ollama running, and which of its models could look at pictures (a name-based guess)?
pub fn ollama_info(host: &str) -> Value {
    if !is_loopback(host) {
        return json!({"reachable": false, "models": [], "error": "Only a local Ollama (127.0.0.1 or localhost) is allowed."});
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_millis(2500)).build();
    let tags = agent.get(&format!("{}/api/tags", host.trim_end_matches('/'))).call().ok().and_then(|r| r.into_string().ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let Some(tags) = tags else { return json!({"reachable": false, "models": [], "error": "Ollama is not running at this address."}) };
    let mut models: Vec<(bool, String, f64)> = vec![];
    for m in tags.get("models").and_then(|v| v.as_array()).into_iter().flatten() {
        let name = m.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        if name.contains("embed") || name.contains("minilm") {
            continue;
        }
        let gb = ((m.get("size").and_then(|s| s.as_f64()).unwrap_or(0.0) / 1e9) * 10.0).round_ties_even() / 10.0;
        let vision = VISION_HINTS.iter().any(|h| name.to_lowercase().contains(h));
        models.push((vision, name, gb));
    }
    models.sort_by(|a, b| (!a.0, &a.1).cmp(&(!b.0, &b.1)));
    json!({"reachable": true, "models": models.into_iter().map(|(v, n, g)| json!({"name": n, "gb": g, "vision": v})).collect::<Vec<_>>(), "error": null})
}

// ---- the background job ----

pub struct AiJob {
    pub state: String,
    pub kind: Option<String>,
    pub done: i64,
    pub total: i64,
    pub message: String,
    pub error: Option<String>,
    pub skipped: i64,
    pub stop: Arc<AtomicBool>,
}

impl Default for AiJob {
    fn default() -> Self {
        AiJob { state: "idle".into(), kind: None, done: 0, total: 0, message: String::new(), error: None, skipped: 0, stop: Arc::new(AtomicBool::new(false)) }
    }
}

impl AiJob {
    pub fn active(&self) -> bool {
        self.state == "running"
    }
    pub fn snapshot(&self) -> Value {
        json!({"state": self.state, "kind": self.kind, "done": self.done, "total": self.total, "message": self.message, "error": self.error, "skipped": self.skipped})
    }
}

pub type Shared = Arc<Mutex<AiJob>>;

fn now() -> f64 {
    crate::copier::now()
}

fn run_trips(job: &Shared, db: &Connection, cfg: &Config, stop: &AtomicBool) -> Result<(), AiError> {
    type R = (String, String, String, i64, i64, Option<String>);
    let rows: Vec<R> = db
        .prepare("SELECT id, start, end, days, items, places FROM trips WHERE items > 0 ORDER BY start DESC")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).map(|it| it.flatten().collect()))
        .map_err(|e| AiError(e.to_string()))?;
    {
        let mut j = job.lock().unwrap();
        j.total = rows.len() as i64;
        j.message = "Asking the model for trip titles…".into();
    }
    for (id, start, end, days, items, places) in rows {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mix: Vec<(String, i64)> = db
            .prepare("SELECT COALESCE(user_tag, tag), COUNT(*) FROM tags WHERE trip_id=? GROUP BY 1")
            .and_then(|mut s| s.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()))
            .unwrap_or_default();
        let places: Vec<String> = places.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default();
        let (title, summary) = suggest_trip(&cfg.ai_host, &cfg.ai_model, &TripFacts { places, start: &start, end: &end, days, files: items, mix })?;
        db.execute("INSERT OR REPLACE INTO ai_trips VALUES(?,?,?,?,?)", params![id, cfg.ai_model, title, summary, now()]).map_err(|e| AiError(e.to_string()))?;
        job.lock().unwrap().done += 1;
    }
    Ok(())
}

fn run_photos(job: &Shared, db: &Connection, cfg: &Config, limit: i64, scope: &str, stop: &AtomicBool) -> Result<(), AiError> {
    let where_ = if scope == "all" { "" } else { "AND t.user_tag IS NULL AND t.conf < 70" };
    let sql = format!(
        "SELECT t.key, i.name, i.dest FROM tags t JOIN items i ON i.key = t.key LEFT JOIN ai_tags a ON a.key = t.key AND a.model = ? WHERE a.key IS NULL {where_} ORDER BY t.conf ASC, i.taken DESC LIMIT ?"
    );
    let all: Vec<(String, String, String)> = db
        .prepare(&sql)
        .and_then(|mut s| s.query_map(params![cfg.ai_model, limit * 3], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default()))).map(|it| it.flatten().collect()))
        .map_err(|e| AiError(e.to_string()))?;
    let rows: Vec<_> = all.into_iter().filter(|(_, n, _)| crate::thumbs::can_preview(crate::tagging::ext_of(n).trim_start_matches('.'))).take(limit as usize).collect();
    {
        let mut j = job.lock().unwrap();
        j.total = rows.len() as i64;
        j.message = "Describing pictures…".into();
    }
    let mut failures = 0;
    let root = cfg.backup_root();
    for (key, _name, dest) in rows {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(jpeg) = crate::thumbs::make_thumbnail(&root.join(&dest), 512) else {
            let mut j = job.lock().unwrap();
            j.skipped += 1;
            j.done += 1;
            continue;
        };
        match describe_picture(&cfg.ai_host, &cfg.ai_model, &jpeg) {
            Ok(d) => {
                failures = 0;
                db.execute(
                    "INSERT OR REPLACE INTO ai_tags VALUES(?,?,?,?,?,?)",
                    params![key, cfg.ai_model, crate::tagging::json_list(&d.tags), d.caption, d.kind, now()],
                )
                .map_err(|e| AiError(e.to_string()))?;
                job.lock().unwrap().done += 1;
            }
            Err(e) => {
                failures += 1;
                if failures >= 3 {
                    let low = e.0.to_lowercase();
                    let hint = if ["multimodal", "image", "vision"].iter().any(|w| low.contains(w)) {
                        " Choose a model that can see pictures in Setup, such as one named llava, moondream or qwen2.5vl."
                    } else {
                        ""
                    };
                    return Err(AiError(format!("{} (stopped after 3 failures in a row).{hint}", e.0)));
                }
                let mut j = job.lock().unwrap();
                j.skipped += 1;
                j.done += 1;
            }
        }
    }
    Ok(())
}

pub fn spawn(job: Shared, cfg: Config, kind: String, limit: i64, scope: String) {
    let stop = {
        let mut j = job.lock().unwrap();
        *j = AiJob { state: "running".into(), kind: Some(kind.clone()), message: "Starting…".into(), ..Default::default() };
        j.stop.clone()
    };
    std::thread::spawn(move || {
        let result = (|| -> Result<(), AiError> {
            let db = crate::edits::open_rw(&cfg).ok_or_else(|| AiError("There is nothing backed up yet.".into()))?;
            if kind == "trips" { run_trips(&job, &db, &cfg, &stop) } else { run_photos(&job, &db, &cfg, limit, &scope, &stop) }
        })();
        let mut j = job.lock().unwrap();
        match result {
            Ok(()) => {
                let stopped = stop.load(Ordering::Relaxed);
                j.state = if stopped { "cancelled" } else { "done" }.into();
                j.message = if stopped { "Stopped" } else { "Finished" }.into();
            }
            Err(e) => {
                j.state = "error".into();
                j.error = Some(e.0);
                j.message = "Stopped by an error".into();
            }
        }
    });
}
