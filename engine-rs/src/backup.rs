//! Runs one backup at a time inside the engine and reports live progress to the UI.

use crate::config::Config;
use crate::copier::{self, now, Control, CopyErr, Hooks, Row, RunOptions};
use crate::device;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const SPEED_WINDOW: f64 = 12.0;
const ACTIVE: [&str; 5] = ["starting", "scanning", "copying", "paused", "cancelling"];

struct State {
    state: String,
    message: String,
    scan_seen: i64,
    plan_files: i64,
    plan_bytes: i64,
    done_files: i64,
    done_bytes: i64,
    failed: i64,
    retries: i64,
    current: Option<(String, i64, i64)>,
    file_abs: i64,
    samples: VecDeque<(f64, i64)>,
    recent: VecDeque<Value>,
    started: f64,
    finished: Option<f64>,
    result: Option<Value>,
    error: Option<String>,
    version: u64,
}

pub struct BackupJob {
    st: Mutex<State>,
    pub control: Control,
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

impl State {
    fn touch(&mut self) {
        self.version += 1;
    }
    fn log(&mut self, level: &str, text: &str) {
        if self.recent.len() >= 40 {
            self.recent.pop_front();
        }
        self.recent.push_back(json!({"t": now(), "level": level, "text": text}));
        self.touch();
    }
    fn set(&mut self, state: &str, message: &str) {
        self.state = state.into();
        self.message = message.into();
        self.touch();
    }
    fn speed(&self) -> f64 {
        if self.state != "copying" || self.samples.len() < 2 {
            return 0.0;
        }
        let (t0, b0) = self.samples.front().copied().unwrap();
        let (t1, b1) = self.samples.back().copied().unwrap();
        if t1 - t0 >= 1.0 { ((b1 - b0) as f64 / (t1 - t0)).max(0.0) } else { 0.0 }
    }
}

impl BackupJob {
    pub fn new() -> Arc<BackupJob> {
        Arc::new(BackupJob {
            st: Mutex::new(State {
                state: "starting".into(),
                message: "Starting…".into(),
                scan_seen: 0,
                plan_files: 0,
                plan_bytes: 0,
                done_files: 0,
                done_bytes: 0,
                failed: 0,
                retries: 0,
                current: None,
                file_abs: 0,
                samples: VecDeque::new(),
                recent: VecDeque::new(),
                started: now(),
                finished: None,
                result: None,
                error: None,
                version: 0,
            }),
            control: Control::default(),
        })
    }

    pub fn state(&self) -> String {
        self.st.lock().unwrap().state.clone()
    }

    pub fn active(&self) -> bool {
        ACTIVE.contains(&self.state().as_str())
    }

    pub fn snapshot(&self) -> Value {
        let s = self.st.lock().unwrap();
        let end = s.finished.unwrap_or_else(now);
        let speed = s.speed();
        let remaining = (s.plan_bytes - s.done_bytes).max(0);
        let eta = if speed > 0.0 && s.state == "copying" { json!((remaining as f64 / speed) as i64) } else { Value::Null };
        json!({
            "state": s.state, "message": s.message,
            "scan": {"seen": s.scan_seen},
            "plan": {"files": s.plan_files, "bytes": s.plan_bytes},
            "done": {"files": s.done_files, "bytes": s.done_bytes},
            "failed": s.failed, "retries": s.retries,
            "current": s.current.as_ref().map(|(n, size, done)| json!({"name": n, "size": size, "done": done})),
            "speed_bps": speed, "eta_seconds": eta,
            "started_at": s.started, "elapsed": ((end - s.started) * 10.0).round() / 10.0,
            "recent": s.recent.iter().cloned().collect::<Vec<_>>(), "result": s.result, "error": s.error, "version": s.version,
        })
    }

    pub fn pause(&self) {
        self.control.paused.store(true, Ordering::Relaxed);
        let mut s = self.st.lock().unwrap();
        s.samples.clear();
        s.set("paused", "Paused. Nothing is lost; resume when you are ready.");
        s.log("info", "Paused");
    }

    pub fn resume(&self) {
        self.control.paused.store(false, Ordering::Relaxed);
        let mut s = self.st.lock().unwrap();
        let m = format!("Copying {} files", thousands(s.plan_files));
        s.set("copying", &m);
        s.log("info", "Resumed");
    }

    pub fn cancel(&self) {
        self.control.cancelled.store(true, Ordering::Relaxed);
        self.st.lock().unwrap().set("cancelling", "Stopping after the current block…");
    }

    fn fail(&self, text: &str) {
        let mut s = self.st.lock().unwrap();
        s.finished = Some(now());
        s.error = Some(text.to_string());
        s.set("error", text);
        s.log("error", text);
    }

    /// The whole run, on its own thread.
    pub fn run(self: &Arc<Self>, cfg: Config, opts: RunOptions) {
        let me = self.clone();
        std::thread::spawn(move || me.run_inner(cfg, opts));
    }

    fn run_inner(&self, cfg: Config, opts: RunOptions) {
        let ssd = std::path::PathBuf::from(&cfg.ssd_root);
        if !ssd.exists() {
            return self.fail(&format!("The backup drive was not found at {}. Connect it and try again.", cfg.ssd_root));
        }
        let mut dev = device::make_device();
        self.st.lock().unwrap().set("starting", "Connecting to the iPhone…");
        if let Err(e) = dev.reconnect() {
            return self.fail(&device::explain_device_error(&e).1);
        }
        let db = match copier::open_manifest(&cfg.backup_root()) {
            Ok(d) => d,
            Err(e) => return self.fail(&e.to_string()),
        };
        self.st.lock().unwrap().set("scanning", "Reading the iPhone's photo list…");
        self.st.lock().unwrap().log("info", "Reading the photo list from the iPhone");
        let stopped = || self.control.cancelled.load(Ordering::Relaxed);
        let seen = |n: i64| {
            let mut s = self.st.lock().unwrap();
            s.scan_seen = n;
            s.touch();
        };
        let (total_seen, new) = match copier::scan(dev.as_mut(), &db, &cfg, &seen, &stopped) {
            Ok(x) => x,
            Err(e) => return self.fail(&e),
        };
        self.st.lock().unwrap().log("info", &format!("{} files on the iPhone, {} new since the last scan", thousands(total_seen), thousands(new)));
        let cancelled = |this: &BackupJob| {
            let mut s = this.st.lock().unwrap();
            s.finished = Some(now());
            s.result = Some(json!({"copied": s.done_files, "failed": s.failed, "bytes": s.done_bytes, "cancelled": true}));
            s.set("cancelled", "Stopped. Files already copied are safe.");
            s.log("info", "Cancelled");
        };
        if self.control.checkpoint().is_err() {
            return cancelled(self);
        }
        match copier::run_copy(dev.as_mut(), &db, &cfg, &opts, &self.control, self) {
            Err(CopyErr::Cancelled) => cancelled(self),
            Err(CopyErr::Other(e)) => self.fail(&e),
            Ok((done, failed)) => {
                self.st.lock().unwrap().set("copying", "Checking the files on the SSD…");
                let rep = crate::verify::verify_ssd(&db, &cfg.backup_root(), false, &|_, _| {}, &|| false).unwrap_or_default();
                let failures: Vec<Value> = db
                    .prepare("SELECT name, error FROM items WHERE status='failed' AND error IS NOT NULL LIMIT 20")
                    .and_then(|mut s| s.query_map([], |r| Ok(json!({"name": r.get::<_, String>(0)?, "error": r.get::<_, String>(1)?}))).map(|it| it.flatten().collect()))
                    .unwrap_or_default();
                let mut s = self.st.lock().unwrap();
                s.result = Some(json!({
                    "copied": done, "failed": failed, "bytes": s.done_bytes, "checked": rep.checked, "ssd_ok": rep.ok(),
                    "missing": rep.missing.len(), "wrong_size": rep.wrong_size.len(), "failures": failures,
                }));
                s.finished = Some(now());
                let msg = format!("Backup finished: {} files copied{}", thousands(done), if failed > 0 { format!(", {} failed", thousands(failed)) } else { String::new() });
                s.set("finished", &msg);
                s.log(if failed > 0 { "warn" } else { "info" }, &msg);
            }
        }
    }
}

impl Hooks for BackupJob {
    fn on_plan(&self, files: i64, bytes: i64) {
        let mut s = self.st.lock().unwrap();
        s.plan_files = files;
        s.plan_bytes = bytes;
        s.set("copying", &format!("Copying {} files", thousands(files)));
        s.log("info", &format!("{} files to copy ({:.1} GB)", thousands(files), bytes as f64 / 1e9));
    }
    fn on_file_start(&self, row: &Row) {
        let mut s = self.st.lock().unwrap();
        s.current = Some((row.name.clone(), row.size, 0));
        s.file_abs = 0;
        s.touch();
    }
    fn on_progress(&self, _row: &Row, done: i64) {
        let mut s = self.st.lock().unwrap();
        s.done_bytes += done - s.file_abs;
        s.file_abs = done;
        if let Some(c) = s.current.as_mut() {
            c.2 = done;
        }
        let t = now();
        let b = s.done_bytes;
        s.samples.push_back((t, b));
        while s.samples.front().is_some_and(|(t0, _)| t - t0 > SPEED_WINDOW) {
            s.samples.pop_front();
        }
        s.touch();
    }
    fn on_file_end(&self, row: &Row, ok: bool, error: Option<&str>) {
        let mut s = self.st.lock().unwrap();
        if ok {
            s.done_files += 1;
        } else {
            s.failed += 1;
            s.log("error", &format!("{} could not be copied: {}", row.name, error.unwrap_or("")));
        }
        s.current = None;
        s.touch();
    }
    fn on_retry(&self, row: &Row, attempt: i64, error: &str) {
        let mut s = self.st.lock().unwrap();
        s.retries += 1;
        s.message = "Connection to the iPhone dropped. Reconnecting…".into();
        s.log("warn", &format!("{}: {error}. Retrying (attempt {}).", row.name, attempt + 1));
    }
}

#[derive(Default)]
pub struct Manager {
    pub job: Mutex<Option<Arc<BackupJob>>>,
}

impl Manager {
    pub fn current(&self) -> Option<Arc<BackupJob>> {
        self.job.lock().unwrap().clone()
    }
    pub fn active(&self) -> bool {
        self.current().map(|j| j.active()).unwrap_or(false)
    }
    pub fn snapshot(&self) -> Value {
        match self.current() {
            Some(j) => j.snapshot(),
            None => json!({"state": "idle", "message": "", "version": 0}),
        }
    }
}
