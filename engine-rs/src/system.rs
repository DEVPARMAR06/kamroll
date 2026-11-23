//! Looking at the computer: drives, folders and whether a backup location is usable. Read-only.

use crate::config::norm_path;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use sysinfo::Disks;

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

pub fn list_volumes() -> Vec<Value> {
    let system = if cfg!(windows) {
        format!("{}/", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_uppercase())
    } else {
        "/".into()
    };
    let mut out: Vec<(bool, String, Value)> = vec![];
    for d in Disks::new_with_refreshed_list().list() {
        let mount = slash(d.mount_point());
        let fs = d.file_system().to_string_lossy().to_string();
        if fs.is_empty() || d.total_space() == 0 {
            continue;
        }
        if cfg!(target_os = "macos") && !(mount.starts_with("/Volumes/") || mount == "/") {
            continue;
        }
        let is_system = mount.eq_ignore_ascii_case(&system);
        let label = {
            let n = d.name().to_string_lossy().to_string();
            if n.is_empty() {
                Path::new(&mount).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| mount.clone())
            } else {
                n
            }
        };
        out.push((
            is_system,
            mount.clone(),
            json!({
                "path": mount, "label": label, "fstype": fs, "total": d.total_space(), "free": d.available_space(),
                "removable": d.is_removable(), "is_system": is_system
            }),
        ));
    }
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    out.into_iter().map(|x| x.2).collect()
}

pub fn list_dirs(path: &str) -> Result<Value, (u16, &'static str)> {
    if path.is_empty() {
        let mut roots: Vec<Value> = list_volumes()
            .iter()
            .map(|v| {
                let p = v["path"].as_str().unwrap_or("");
                json!({"name": format!("{} ({})", v["label"].as_str().unwrap_or(""), p.trim_end_matches('/')), "path": p})
            })
            .collect();
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
        roots.push(json!({"name": "Home", "path": slash(&home)}));
        return Ok(json!({"path": "", "parent": null, "entries": roots}));
    }
    let p = Path::new(path);
    if !p.is_dir() {
        return Err((404, "That folder does not exist."));
    }
    let rd = std::fs::read_dir(p).map_err(|_| (403u16, "No permission to open that folder."))?;
    let mut names: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('$') || name.starts_with('.') || name == "System Volume Information" || name == "Recovery" {
                return None;
            }
            e.path().is_dir().then(|| (name, e.path()))
        })
        .collect();
    names.sort_by_key(|(n, _)| n.to_lowercase());
    names.truncate(500);
    let entries: Vec<Value> = names.iter().map(|(n, p)| json!({"name": n, "path": slash(p)})).collect();
    let parent = match p.parent() {
        Some(par) if par != p && !par.as_os_str().is_empty() => slash(par),
        _ => String::new(),
    };
    Ok(json!({"path": slash(p), "parent": parent, "entries": entries}))
}

fn free_space(existing: &Path) -> Option<u64> {
    let target = existing.canonicalize().ok()?;
    let target = target.to_string_lossy().replace('\\', "/").trim_start_matches("//?/").to_lowercase();
    let mut best: Option<(usize, u64)> = None;
    for d in Disks::new_with_refreshed_list().list() {
        let mount = slash(d.mount_point()).to_lowercase();
        if target.starts_with(&mount) && best.is_none_or(|(len, _)| mount.len() > len) {
            best = Some((mount.len(), d.available_space()));
        }
    }
    best.map(|(_, free)| free)
}

/// Is this a usable backup folder? Never creates anything.
pub fn path_check(path: &str) -> Value {
    let p = Path::new(path);
    let mut existing = p.to_path_buf();
    while !existing.exists() && existing.parent().is_some_and(|par| par != existing && !par.as_os_str().is_empty()) {
        existing = existing.parent().unwrap().to_path_buf();
    }
    let exists = p.is_dir();
    let volume_missing = !existing.exists();
    let writable = existing.metadata().map(|m| !m.permissions().readonly()).unwrap_or(false);
    let file_count = if exists { std::fs::read_dir(p).map(|r| r.count()).unwrap_or(0) } else { 0 };
    json!({
        "path": norm_path(path), "exists": exists, "will_create": !exists && existing.exists(), "writable": writable,
        "free": free_space(&existing), "volume_missing": volume_missing, "file_count": file_count
    })
}
