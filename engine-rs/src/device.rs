//! The phone, as the copier sees it: list files, read a range, reconnect. The real iPhone lives in `afc.rs`;
//! a folder on disk can stand in for it (set PHOTOVAULT_FAKE_DEVICE) for tests and demos.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub path: String,
    pub size: i64,
    pub created: f64,
    pub modified: f64,
}

impl RemoteFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

pub struct DeviceInfo {
    pub name: String,
    pub model: String,
    pub ios: String,
}

pub trait Device: Send {
    /// Call `each` for every photo/video file under the roots. Returning false from `each` stops the listing.
    fn list_files(&mut self, roots: &[String], ignore: &[String], each: &mut dyn FnMut(RemoteFile) -> bool) -> Result<(), String>;
    fn read_range(&mut self, path: &str, offset: u64, n: usize) -> Result<Vec<u8>, String>;
    fn reconnect(&mut self) -> Result<(), String>;
    fn info(&mut self) -> Result<DeviceInfo, String>;
}

/// fnmatch-style match: `*`, `?` and `[abc]`. Case-insensitive on Windows, like Python's fnmatch there.
pub fn ignored(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob(p, name))
}

pub fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = if cfg!(windows) {
        (pattern.to_lowercase().chars().collect(), name.to_lowercase().chars().collect())
    } else {
        (pattern.chars().collect(), name.chars().collect())
    };
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => match p.iter().position(|c| *c == ']') {
                Some(end) if end > 1 && !n.is_empty() => {
                    let (neg, set) = if p[1] == '!' { (true, &p[2..end]) } else { (false, &p[1..end]) };
                    let mut hit = false;
                    let mut i = 0;
                    while i < set.len() {
                        if i + 2 < set.len() && set[i + 1] == '-' {
                            hit |= set[i] <= n[0] && n[0] <= set[i + 2];
                            i += 3;
                        } else {
                            hit |= set[i] == n[0];
                            i += 1;
                        }
                    }
                    hit != neg && go(&p[end + 1..], &n[1..])
                }
                _ => !n.is_empty() && n[0] == '[' && go(&p[1..], &n[1..]),
            },
            Some(c) => !n.is_empty() && n[0] == *c && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

/// A folder pretending to be an iPhone: `<dir>/DCIM/100APPLE/IMG_0001.JPG` is `/DCIM/100APPLE/IMG_0001.JPG`.
pub struct DirDevice {
    pub dir: PathBuf,
}

fn times(m: &std::fs::Metadata) -> (f64, f64) {
    let secs = |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let modified = secs(m.modified());
    (modified, modified)
}

impl DirDevice {
    fn local(&self, remote: &str) -> PathBuf {
        self.dir.join(remote.trim_start_matches('/'))
    }
}

fn sorted_names(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    v.sort();
    v
}

impl Device for DirDevice {
    fn list_files(&mut self, roots: &[String], ignore: &[String], each: &mut dyn FnMut(RemoteFile) -> bool) -> Result<(), String> {
        for root in roots {
            let rdir = self.local(root);
            for folder in sorted_names(&rdir) {
                if ignored(&folder, ignore) {
                    continue;
                }
                let fpath = format!("{}/{}", root.trim_end_matches('/'), folder);
                let lp = self.local(&fpath);
                let entries: Vec<(String, String)> = if lp.is_dir() {
                    sorted_names(&lp).into_iter().map(|n| (n.clone(), format!("{fpath}/{n}"))).collect()
                } else {
                    vec![(folder.clone(), fpath.clone())]
                };
                for (n, p) in entries {
                    if ignored(&n, ignore) {
                        continue;
                    }
                    let Ok(m) = self.local(&p).metadata() else { continue };
                    if !m.is_file() {
                        continue;
                    }
                    let (created, modified) = times(&m);
                    if !each(RemoteFile { path: p, size: m.len() as i64, created, modified }) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn read_range(&mut self, path: &str, offset: u64, n: usize) -> Result<Vec<u8>, String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(self.local(path)).map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; n];
        let mut got = 0;
        while got < n {
            let k = f.read(&mut buf[got..]).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            got += k;
        }
        buf.truncate(got);
        Ok(buf)
    }

    fn reconnect(&mut self) -> Result<(), String> {
        if self.dir.is_dir() { Ok(()) } else { Err("NoDeviceConnectedError: the stand-in phone folder is missing".into()) }
    }

    fn info(&mut self) -> Result<DeviceInfo, String> {
        self.reconnect()?;
        Ok(DeviceInfo { name: "Test iPhone".into(), model: "iPhone 15 Pro".into(), ios: "17.0".into() })
    }
}

/// The phone this engine should use: the stand-in folder if PHOTOVAULT_FAKE_DEVICE is set, else a real iPhone over USB.
pub fn make_device() -> Box<dyn Device> {
    match std::env::var_os("PHOTOVAULT_FAKE_DEVICE") {
        Some(dir) => Box::new(DirDevice { dir: PathBuf::from(dir) }),
        None => Box::new(crate::afc::AfcDevice::new()),
    }
}

/// (short code, plain-language hint) for a failure to reach the iPhone. Same words as the Python engine.
pub fn explain_device_error(e: &str) -> (String, String) {
    let lower = e.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        return ("timeout".into(), "The iPhone did not answer. Unlock it and tap Trust if asked.".into());
    }
    if lower.contains("nodeviceconnected") {
        return ("NoDeviceConnectedError".into(), "No iPhone found. Plug it in with a cable, unlock it and tap Trust.".into());
    }
    if lower.contains("usbmuxd") || lower.contains("socket") || lower.contains("connectionrefused") || lower.contains("refused") {
        return ("ConnectionRefusedError".into(), "Apple's USB service is not running. On Windows, install Apple Devices from the Microsoft Store.".into());
    }
    ("Error".into(), "Could not talk to the iPhone. Unlock it, reconnect the cable and try again.".into())
}
