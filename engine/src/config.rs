//! Settings, read and written in the same `config.toml` format as the Python engine.

use crate::layout;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const DEFAULT_TRIP_LAYOUT: &str = "Trips/{trip_year}/{trip_dated}/{name}";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Config {
    pub ssd_root: String,
    pub backup_dir: String,
    pub remote_roots: Vec<String>,
    pub layout: String,
    pub undated_layout: String,
    pub chunk_mb: i64,
    pub max_retries: i64,
    pub ignore: Vec<String>,
    pub free_space_margin_gb: f64,
    pub copy_originals_of_reencoded: bool,
    pub ai_enabled: bool,
    pub ai_host: String,
    pub ai_model: String,
    pub trip_radius_km: f64,
    pub home_lat: Option<f64>,
    pub home_lon: Option<f64>,
    pub file_trips: bool,
    pub trip_layout: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ssd_root: ".".into(),
            backup_dir: "PhotoBackup".into(),
            remote_roots: vec!["/DCIM".into()],
            layout: "{year}/{year}-{month}/{name}".into(),
            undated_layout: "Undated/{name}".into(),
            chunk_mb: 4,
            max_retries: 6,
            ignore: vec!["._*".into(), ".DS_Store".into(), ".MISC".into(), "Thumbs.db".into()],
            free_space_margin_gb: 5.0,
            copy_originals_of_reencoded: true,
            ai_enabled: false,
            ai_host: "http://127.0.0.1:11434".into(),
            ai_model: String::new(),
            trip_radius_km: 60.0,
            home_lat: None,
            home_lon: None,
            file_trips: false,
            trip_layout: DEFAULT_TRIP_LAYOUT.into(),
        }
    }
}

/// Like Python's `Path`: forward slashes, no repeated or trailing separators (except a root such as `E:/`).
pub fn norm_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    let mut out = String::new();
    let mut prev_slash = false;
    for (i, c) in s.chars().enumerate() {
        if c == '/' {
            if prev_slash && i > 1 {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    let is_root = out == "/" || (out.len() == 3 && out.ends_with(":/"));
    while out.len() > 1 && out.ends_with('/') && !is_root && !(out.len() == 3 && out.ends_with(":/")) {
        out.pop();
    }
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

fn is_absolute(p: &str) -> bool {
    let s = p.replace('\\', "/");
    s.starts_with('/') || (s.len() >= 2 && s.as_bytes()[1] == b':')
}

impl Config {
    pub fn backup_root(&self) -> PathBuf {
        if is_absolute(&self.backup_dir) {
            PathBuf::from(&self.backup_dir)
        } else {
            Path::new(&self.ssd_root).join(&self.backup_dir)
        }
    }
    pub fn backup_root_string(&self) -> String {
        self.backup_root().to_string_lossy().replace('\\', "/")
    }
    pub fn manifest_path(&self) -> PathBuf {
        self.backup_root().join(".photovault").join("manifest.db")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default()
}

pub fn user_config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
    } else if cfg!(target_os = "macos") {
        home_dir().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".config"))
    };
    base.join("kamroll")
}

/// The settings file, if there is one. There is one place for it (see `user_config_dir`); the
/// PHOTOVAULT_CONFIG variable and the command line's `-c` point somewhere else for tests and scripts.
pub fn find_config() -> Option<PathBuf> {
    let p = default_config_path();
    p.exists().then_some(p)
}

/// Copy settings saved by an earlier version into the one place settings now live. The old file is left alone.
pub fn migrate_settings() {
    if std::env::var_os("PHOTOVAULT_CONFIG").is_some() {
        return;
    }
    let target = user_config_dir().join("config.toml");
    migrate_into(
        &target,
        &[
            home_dir().join(".kamroll.toml"),
            home_dir().join(".photovault.toml"),
            user_config_dir().with_file_name("iphone-photo-vault").join("config.toml"),
        ],
    );
}

fn migrate_into(target: &Path, older: &[PathBuf]) {
    if target.exists() {
        return;
    }
    if let Some(found) = older.iter().find(|p| p.is_file()) {
        if let Some(dir) = target.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::copy(found, target);
    }
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("PHOTOVAULT_CONFIG").map(PathBuf::from).unwrap_or_else(|| user_config_dir().join("config.toml"))
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: toml::Value = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut c = Config::default();
    let table = |name: &str| v.get(name).and_then(|t| t.as_table());
    let get_s = |t: Option<&toml::map::Map<String, toml::Value>>, k: &str| t.and_then(|t| t.get(k)).and_then(|x| x.as_str()).map(String::from);
    let get_list = |t: Option<&toml::map::Map<String, toml::Value>>, k: &str| {
        t.and_then(|t| t.get(k)).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|y| y.as_str().map(String::from)).collect::<Vec<_>>())
    };
    let get_f = |t: Option<&toml::map::Map<String, toml::Value>>, k: &str| {
        t.and_then(|t| t.get(k)).and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
    };
    let get_i = |t: Option<&toml::map::Map<String, toml::Value>>, k: &str| t.and_then(|t| t.get(k)).and_then(|x| x.as_integer());
    let get_b = |t: Option<&toml::map::Map<String, toml::Value>>, k: &str| t.and_then(|t| t.get(k)).and_then(|x| x.as_bool());

    let (paths, dev, cp, ai, trips) = (table("paths"), table("device"), table("copy"), table("ai"), table("trips"));
    c.ssd_root = norm_path(&get_s(paths, "ssd_root").ok_or("config: [paths] ssd_root is required")?);
    if let Some(x) = get_s(paths, "backup_dir") {
        c.backup_dir = x;
    }
    if let Some(x) = get_list(dev, "remote_roots") {
        c.remote_roots = x;
    }
    if let Some(x) = get_s(cp, "layout") {
        c.layout = x;
    }
    if let Some(x) = get_s(cp, "undated_layout") {
        c.undated_layout = x;
    }
    if let Some(x) = get_i(cp, "chunk_mb") {
        c.chunk_mb = x;
    }
    if let Some(x) = get_i(cp, "max_retries") {
        c.max_retries = x;
    }
    if let Some(x) = get_list(cp, "ignore") {
        c.ignore = x;
    }
    if let Some(x) = get_f(cp, "free_space_margin_gb") {
        c.free_space_margin_gb = x;
    }
    if let Some(x) = get_b(cp, "copy_originals_of_reencoded") {
        c.copy_originals_of_reencoded = x;
    }
    if let Some(x) = get_b(ai, "enabled") {
        c.ai_enabled = x;
    }
    if let Some(x) = get_s(ai, "host") {
        c.ai_host = x;
    }
    if let Some(x) = get_s(ai, "model") {
        c.ai_model = x;
    }
    if let Some(x) = get_f(trips, "trip_radius_km") {
        c.trip_radius_km = x;
    }
    c.home_lat = get_f(trips, "home_lat");
    c.home_lon = get_f(trips, "home_lon");
    if let Some(x) = get_b(trips, "file_trips") {
        c.file_trips = x;
    }
    if let Some(x) = get_s(trips, "trip_layout") {
        c.trip_layout = x;
    }
    Ok(c)
}

fn s(v: &str) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into())
}
fn list(v: &[String]) -> String {
    // Same spacing as Python's json.dumps, so both engines write byte-identical settings files.
    format!("[{}]", v.iter().map(|x| s(x)).collect::<Vec<_>>().join(", "))
}
fn flag(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// Write the config atomically, keeping the previous file as `<name>.bak`. Same text as the Python engine.
pub fn save(c: &Config, path: &Path) -> std::io::Result<()> {
    let home = match (c.home_lat, c.home_lon) {
        (Some(a), Some(b)) => format!("home_lat = {a:?}\nhome_lon = {b:?}\n"),
        _ => String::new(),
    };
    let text = format!(
        "# Written by Kamroll. Edit here or in the app.\n\n[paths]\nssd_root = {}\nbackup_dir = {}\n\n[device]\nremote_roots = {}\n\n[copy]\nlayout = {}\nundated_layout = {}\nchunk_mb = {}\nmax_retries = {}\nignore = {}\nfree_space_margin_gb = {:?}\ncopy_originals_of_reencoded = {}\n\n[ai]\nenabled = {}\nhost = {}\nmodel = {}\n\n[trips]\ntrip_radius_km = {:?}\nfile_trips = {}\ntrip_layout = {}\n{}",
        s(&c.ssd_root.replace('\\', "/")),
        s(&c.backup_dir.replace('\\', "/")),
        list(&c.remote_roots),
        s(&c.layout),
        s(&c.undated_layout),
        c.chunk_mb,
        c.max_retries,
        list(&c.ignore),
        c.free_space_margin_gb,
        flag(c.copy_originals_of_reencoded),
        flag(c.ai_enabled),
        s(&c.ai_host),
        s(&c.ai_model),
        c.trip_radius_km,
        flag(c.file_trips),
        s(&c.trip_layout),
        home
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

pub fn is_loopback(url: &str) -> bool {
    let rest = match url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        Some(r) => r,
        None => return false,
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or("").trim_start_matches('[') } else { host.split(':').next().unwrap_or("") };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// A validation problem in the shape FastAPI uses, so the UI shows it next to the right field.
pub struct FieldError {
    pub field: &'static str,
    pub msg: String,
}

pub fn errors_json(errs: &[FieldError]) -> Value {
    json!({"detail": errs.iter().map(|e| json!({"loc": ["body", e.field], "msg": e.msg, "type": "value_error"})).collect::<Vec<_>>()})
}

/// Check a settings object from the UI. Returns the config or every problem found.
pub fn validate(body: &Value) -> Result<Config, Vec<FieldError>> {
    let mut errs: Vec<FieldError> = vec![];
    let mut c = Config::default();
    let obj = match body.as_object() {
        Some(o) => o,
        None => return Err(vec![FieldError { field: "ssd_root", msg: "Input should be a valid dictionary".into() }]),
    };
    macro_rules! text {
        ($f:literal, $dst:expr, $min1:expr) => {
            match obj.get($f).and_then(|v| v.as_str()) {
                Some(x) if !($min1 && x.is_empty()) => $dst = x.to_string(),
                Some(_) => errs.push(FieldError { field: $f, msg: "String should have at least 1 character".into() }),
                None => errs.push(FieldError { field: $f, msg: "Field required".into() }),
            }
        };
    }
    macro_rules! boolean {
        ($f:literal, $dst:expr) => {
            match obj.get($f).and_then(|v| v.as_bool()) {
                Some(x) => $dst = x,
                None => errs.push(FieldError { field: $f, msg: "Input should be a valid boolean".into() }),
            }
        };
    }
    text!("ssd_root", c.ssd_root, true);
    text!("backup_dir", c.backup_dir, true);
    text!("layout", c.layout, false);
    text!("undated_layout", c.undated_layout, false);
    text!("ai_host", c.ai_host, false);
    text!("ai_model", c.ai_model, false);
    if let Some(v) = obj.get("trip_layout") {
        match v.as_str() {
            Some(x) => c.trip_layout = x.to_string(),
            None => errs.push(FieldError { field: "trip_layout", msg: "Input should be a valid string".into() }),
        }
    }
    boolean!("copy_originals_of_reencoded", c.copy_originals_of_reencoded);
    boolean!("ai_enabled", c.ai_enabled);
    if let Some(v) = obj.get("file_trips") {
        match v.as_bool() {
            Some(x) => c.file_trips = x,
            None => errs.push(FieldError { field: "file_trips", msg: "Input should be a valid boolean".into() }),
        }
    }
    for (f, dst) in [("remote_roots", &mut c.remote_roots), ("ignore", &mut c.ignore)] {
        match obj.get(f).and_then(|v| v.as_array()) {
            Some(a) if a.iter().all(|x| x.is_string()) => *dst = a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => errs.push(FieldError { field: if f == "ignore" { "ignore" } else { "remote_roots" }, msg: "Input should be a valid list of strings".into() }),
        }
    }
    let int_in = |f: &'static str, lo: i64, hi: i64, errs: &mut Vec<FieldError>| -> Option<i64> {
        match obj.get(f).and_then(|v| v.as_i64()) {
            Some(x) if x >= lo && x <= hi => Some(x),
            Some(x) if x < lo => {
                errs.push(FieldError { field: f, msg: format!("Input should be greater than or equal to {lo}") });
                None
            }
            Some(_) => {
                errs.push(FieldError { field: f, msg: format!("Input should be less than or equal to {hi}") });
                None
            }
            None => {
                errs.push(FieldError { field: f, msg: "Input should be a valid integer".into() });
                None
            }
        }
    };
    let num_in = |f: &'static str, lo: f64, hi: f64, required: bool, errs: &mut Vec<FieldError>| -> Option<f64> {
        match obj.get(f) {
            None | Some(Value::Null) if !required => None,
            Some(v) => match v.as_f64() {
                Some(x) if x >= lo && x <= hi => Some(x),
                Some(x) if x < lo => {
                    errs.push(FieldError { field: f, msg: format!("Input should be greater than or equal to {lo}") });
                    None
                }
                Some(_) => {
                    errs.push(FieldError { field: f, msg: format!("Input should be less than or equal to {hi}") });
                    None
                }
                None => {
                    errs.push(FieldError { field: f, msg: "Input should be a valid number".into() });
                    None
                }
            },
            None => {
                errs.push(FieldError { field: f, msg: "Field required".into() });
                None
            }
        }
    };
    if let Some(x) = int_in("chunk_mb", 1, 64, &mut errs) {
        c.chunk_mb = x;
    }
    if let Some(x) = int_in("max_retries", 1, 20, &mut errs) {
        c.max_retries = x;
    }
    if let Some(x) = num_in("free_space_margin_gb", 0.0, 500.0, true, &mut errs) {
        c.free_space_margin_gb = x;
    }
    if let Some(x) = num_in("trip_radius_km", 5.0, 2000.0, false, &mut errs) {
        c.trip_radius_km = x;
    }
    c.home_lat = num_in("home_lat", -90.0, 90.0, false, &mut errs);
    c.home_lon = num_in("home_lon", -180.0, 180.0, false, &mut errs);

    let value_err = |f: &'static str, m: String| FieldError { field: f, msg: format!("Value error, {m}") };
    if let Some(p) = layout::check_layout(&c.layout) {
        errs.push(value_err("layout", p));
    }
    if let Some(p) = layout::check_layout(&c.undated_layout) {
        errs.push(value_err("undated_layout", p));
    }
    if let Some(p) = layout::check_trip_layout(&c.trip_layout) {
        errs.push(value_err("trip_layout", p));
    }
    if c.backup_dir.replace('\\', "/").split('/').any(|p| p == "..") {
        errs.push(value_err("backup_dir", "The folder name cannot contain '..'.".into()));
    }
    if c.remote_roots.is_empty() {
        errs.push(FieldError { field: "remote_roots", msg: "List should have at least 1 item after validation, not 0".into() });
    } else if c.remote_roots.iter().any(|r| !r.starts_with('/')) {
        errs.push(value_err("remote_roots", "Phone folders start with a slash, for example /DCIM.".into()));
    }
    if !is_loopback(&c.ai_host) {
        errs.push(value_err("ai_host", "Only a local Ollama (127.0.0.1 or localhost) is allowed.".into()));
    }
    if errs.is_empty() {
        c.ssd_root = norm_path(&c.ssd_root);
        Ok(c)
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_normalise_like_python() {
        assert_eq!(norm_path("E:/"), "E:/");
        assert_eq!(norm_path("E:\\"), "E:/");
        assert_eq!(norm_path("C:/a/b/"), "C:/a/b");
        assert_eq!(norm_path("C:\\a\\\\b"), "C:/a/b");
        assert_eq!(norm_path(""), ".");
        assert_eq!(norm_path("/Volumes/SSD/"), "/Volumes/SSD");
    }

    #[test]
    fn loopback_only() {
        assert!(is_loopback("http://127.0.0.1:11434"));
        assert!(is_loopback("http://localhost:11434/x"));
        assert!(!is_loopback("http://example.com:11434"));
        assert!(!is_loopback("ftp://127.0.0.1"));
    }

    #[test]
    fn roundtrip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut c = Config { ssd_root: "E:/".into(), ..Default::default() };
        c.home_lat = Some(19.07);
        c.home_lon = Some(72.88);
        c.file_trips = true;
        save(&c, &p).unwrap();
        assert_eq!(load(&p).unwrap(), c);
        c.backup_dir = "Photos2".into();
        save(&c, &p).unwrap();
        assert!(dir.path().join("config.toml.bak").exists());
    }

    #[test]
    fn rejects_bad_settings_with_the_same_reasons() {
        let good = serde_json::to_value(Config { ssd_root: "E:/".into(), ..Default::default() }).unwrap();
        assert!(validate(&good).is_ok());
        let mut bad = good.clone();
        bad["layout"] = json!("{year}/{oops}/{name}");
        bad["chunk_mb"] = json!(0);
        bad["ai_host"] = json!("http://example.com:11434");
        bad["remote_roots"] = json!(["DCIM"]);
        let errs = validate(&bad).err().unwrap();
        let fields: Vec<_> = errs.iter().map(|e| e.field).collect();
        for f in ["layout", "chunk_mb", "ai_host", "remote_roots"] {
            assert!(fields.contains(&f), "missing {f}");
        }
    }

    #[test]
    fn earlier_settings_move_to_the_one_place_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let (old, target) = (dir.path().join(".photovault.toml"), dir.path().join("kamroll").join("config.toml"));
        std::fs::write(&old, "old").unwrap();
        migrate_into(&target, &[dir.path().join("missing"), old.clone()]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        std::fs::write(&old, "changed").unwrap();
        migrate_into(&target, &[old.clone()]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(old.exists());
    }
}
