//! The command line: the same commands and the same words as `photovault` in the Python engine.

use crate::config::{self, Config};
use crate::copier::{self, Hooks, Row, RunOptions};
use crate::{adopt, device, legacy, verify};
use std::path::PathBuf;

fn fmt(counts: &[(String, i64, i64)]) -> String {
    if counts.is_empty() {
        return "empty".into();
    }
    counts.iter().map(|(k, c, s)| format!("{k}={c} ({:.1} GB)", *s as f64 / 1e9)).collect::<Vec<_>>().join(", ")
}

struct Printer {
    total: std::cell::Cell<i64>,
    i: std::cell::Cell<i64>,
    ok: std::cell::Cell<i64>,
    failed: std::cell::Cell<i64>,
}

impl Hooks for Printer {
    fn on_plan(&self, files: i64, _bytes: i64) {
        self.total.set(files);
    }
    fn on_file_end(&self, _row: &Row, ok: bool, _error: Option<&str>) {
        self.i.set(self.i.get() + 1);
        if ok {
            self.ok.set(self.ok.get() + 1)
        } else {
            self.failed.set(self.failed.get() + 1)
        }
        if self.i.get() % 25 == 0 || self.i.get() == self.total.get() {
            println!("  {}/{}  ok={} failed={}", self.i.get(), self.total.get(), self.ok.get(), self.failed.get());
        }
    }
}

pub struct Args {
    pub cmd: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
    pub deep: bool,
    pub dry_run: bool,
    pub only_reencoded: bool,
    pub out: Option<String>,
}

fn load() -> Result<Config, String> {
    let p = config::find_config().ok_or("No settings found. Open the app and save your settings first.")?;
    let cfg = config::load(&p)?;
    if !PathBuf::from(&cfg.ssd_root).exists() {
        return Err(format!("SSD not found at {}", cfg.ssd_root));
    }
    Ok(cfg)
}

pub fn run(a: Args) -> i32 {
    match run_inner(a) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn run_inner(a: Args) -> Result<i32, String> {
    let cfg = load()?;
    let db = copier::open_manifest(&cfg.backup_root()).map_err(|e| e.to_string())?;
    let path = || PathBuf::from(a.path.clone().unwrap_or_default());
    match a.cmd.as_str() {
        "status" => {
            println!("{}", fmt(&adopt::counts(&db)));
            let ds = adopt::date_sources(&db).iter().map(|(k, v)| format!("'{k}': {v}")).collect::<Vec<_>>().join(", ");
            println!("date sources: {{{ds}}}");
            return Ok(0);
        }
        "verify" => {
            let rep = verify::verify_ssd(&db, &cfg.backup_root(), a.deep, &|_, _| {}, &|| false).unwrap_or_default();
            println!("checked {}: missing={} wrong_size={} corrupt={}", rep.checked, rep.missing.len(), rep.wrong_size.len(), rep.corrupt.len());
            for (n, items) in [("MISSING", &rep.missing), ("WRONG SIZE", &rep.wrong_size), ("CORRUPT", &rep.corrupt)] {
                for x in items.iter().take(20) {
                    println!("  {n}: {x}");
                }
            }
            return Ok(if rep.ok() { 0 } else { 1 });
        }
        "adopt" => {
            let (m, n) = adopt::adopt(&db, &path(), &cfg);
            println!("examined {n} files, matched {m} phone files already backed up");
            return Ok(0);
        }
        "relocate-old" => {
            let (moved, failed) = legacy::relocate_old(&db, &cfg, &|s| println!("{s}"))?;
            println!("moved {moved} into the normal folders, failed {failed}");
            return Ok(if failed > 0 { 1 } else { 0 });
        }
        "import-old" => {
            let (copied, skipped, failed) = legacy::import_old(&db, &cfg, &path(), a.limit, &|s| println!("{s}"))?;
            println!("imported {copied}, already in vault {skipped}, failed {failed}");
            println!("{}", fmt(&adopt::counts(&db)));
            return Ok(if failed > 0 { 1 } else { 0 });
        }
        "consolidate" => {
            let (done, failed) = adopt::consolidate(&db, &cfg, a.limit, &|s| println!("{s}"))?;
            println!("consolidated {done}, failed {failed}");
            println!("{}", fmt(&adopt::counts(&db)));
            return Ok(if failed > 0 { 1 } else { 0 });
        }
        _ => {}
    }
    let mut dev = device::make_device();
    dev.reconnect()?;
    let (seen, new) = copier::scan(dev.as_mut(), &db, &cfg, &|_| {}, &|| false)?;
    println!("scan: {seen} files on phone, {new} new to manifest");
    match a.cmd.as_str() {
        "sync" => {
            let r = verify::sync_report(dev.as_mut(), &db, &cfg, &|_| {})?;
            let n = |k: &str| r[k].as_i64().unwrap_or(0);
            let len = |k: &str| r[k].as_array().map(|v| v.len()).unwrap_or(0);
            println!("phone files: {}   backed up: {}", n("on_phone"), n("backed_up"));
            println!("NOT on SSD yet: {}   adopted but not consolidated: {}", n("waiting"), n("adopted_not_copied"));
            println!("only on SSD (deleted from phone since): {}   skipped by your choice: {}", n("only_on_ssd"), n("skipped"));
            println!("SSD check: missing={} wrong_size={}", len("missing"), len("wrong_size"));
            for x in r["waiting_sample"].as_array().into_iter().flatten().take(10) {
                println!("  missing: {}", x.as_str().unwrap_or(""));
            }
            let ok = r["in_sync"].as_bool().unwrap_or(false);
            println!("{}", if ok { "IN SYNC" } else { "NOT IN SYNC" });
            Ok(if ok { 0 } else { 1 })
        }
        "compare-old" => {
            let rep = legacy::compare_old(dev.as_mut(), &db, &path(), &cfg, &|s| println!("{s}"))?;
            println!("old folder: {} files", rep.examined);
            println!("  same name and size as a phone file : {}", rep.same_name_size.len());
            println!("  same size, different name          : {}", rep.same_size.len());
            println!("  re-encoded copy of a phone original: {}", rep.converted.len());
            println!("  slow-mo/speed-changed export       : {}", rep.speed_changed.len());
            println!("  not explained by anything on phone : {}", rep.unmatched.len());
            if let Some(out) = &a.out {
                let mut lines: Vec<String> = rep.unmatched.iter().map(|n| format!("unmatched\t{n}")).collect();
                lines.extend(rep.converted.iter().map(|(x, y)| format!("converted\t{x}\t{y}")));
                lines.extend(rep.speed_changed.iter().map(|(x, y)| format!("speed-changed\t{x}\t{y}")));
                std::fs::write(out, lines.join("\n")).map_err(|e| e.to_string())?;
                println!("details written to {out}");
            }
            Ok(0)
        }
        _ => {
            if a.cmd == "scan" || a.dry_run {
                println!("{}", fmt(&adopt::counts(&db)));
                return Ok(0);
            }
            let p = Printer { total: 0.into(), i: 0.into(), ok: 0.into(), failed: 0.into() };
            let control = copier::Control::default();
            let r = copier::run_copy(dev.as_mut(), &db, &cfg, &RunOptions { limit: a.limit, only_reencoded: a.only_reencoded }, &control, &p);
            let (done, failed) = match r {
                Ok(x) => x,
                Err(copier::CopyErr::Other(e)) => return Err(e),
                Err(copier::CopyErr::Cancelled) => return Err("cancelled".into()),
            };
            println!("copied {done}, failed {failed}");
            println!("{}", fmt(&adopt::counts(&db)));
            Ok(if failed > 0 { 1 } else { 0 })
        }
    }
}
