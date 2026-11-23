//! Capture date and GPS from the file itself (EXIF for photos, QuickTime atoms for video).
//! Read-only and local: it runs on the copy already on the SSD, never on the phone.
//! `source` says where the date came from: exif | quicktime | mvhd | filename | none.

use crate::tagging::{ext_of, IMAGE_EXTS, VIDEO_EXTS};
use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use regex::Regex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const MIN_YEAR: i32 = 1990;
pub const FS_MIN_YEAR: i32 = 2007; // first iPhone; earlier filesystem dates are copy artefacts
const MAC_EPOCH_OFFSET: i64 = 2_082_844_800;

#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub taken: Option<NaiveDateTime>, // wall-clock time where it was taken
    pub source: &'static str,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl Default for Meta {
    fn default() -> Self {
        Meta { taken: None, source: "none", lat: None, lon: None }
    }
}

pub fn sane(dt: Option<NaiveDateTime>, min_year: i32) -> bool {
    dt.map(|d| d.year() >= min_year && d.year() <= Local::now().year() + 1).unwrap_or(false)
}

/// Seconds since 1970 for a wall-clock time in this computer's time zone (what Python's `.timestamp()` gives).
pub fn epoch(dt: NaiveDateTime) -> f64 {
    Local.from_local_datetime(&dt).earliest().map(|d| d.timestamp() as f64).unwrap_or_else(|| dt.and_utc().timestamp() as f64)
}

pub fn from_epoch(ts: f64) -> NaiveDateTime {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    Local.timestamp_opt(secs, nanos).earliest().map(|d| d.naive_local()).unwrap_or_default()
}

fn parse_exif_dt(s: &str) -> Option<NaiveDateTime> {
    let t = s.trim();
    let t = t.get(..19).unwrap_or(t);
    NaiveDateTime::parse_from_str(t, "%Y:%m:%d %H:%M:%S").ok()
}

fn dms(vals: &exif::Field, reference: Option<String>) -> Option<f64> {
    let exif::Value::Rational(r) = &vals.value else { return None };
    if r.len() < 3 {
        return None;
    }
    let out = r[0].to_f64() + r[1].to_f64() / 60.0 + r[2].to_f64() / 3600.0;
    Some(if matches!(reference.as_deref().map(|s| s.to_uppercase()).as_deref(), Some("S") | Some("W")) { -out } else { out })
}

fn ascii_field(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let f = ex.get_field(tag, exif::In::PRIMARY)?;
    if let exif::Value::Ascii(p) = &f.value {
        let s = String::from_utf8_lossy(p.first()?).trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
        return if s.is_empty() { None } else { Some(s) };
    }
    None
}

fn read_image(path: &Path) -> Meta {
    let Ok(file) = File::open(path) else { return Meta::default() };
    let Ok(ex) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) else { return Meta::default() };
    let raw = ascii_field(&ex, exif::Tag::DateTimeOriginal)
        .or_else(|| ascii_field(&ex, exif::Tag::DateTimeDigitized))
        .or_else(|| ascii_field(&ex, exif::Tag::DateTime));
    let dt = raw.as_deref().and_then(parse_exif_dt);
    let lat = ex.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).and_then(|f| dms(f, ascii_field(&ex, exif::Tag::GPSLatitudeRef)));
    let lon = ex.get_field(exif::Tag::GPSLongitude, exif::In::PRIMARY).and_then(|f| dms(f, ascii_field(&ex, exif::Tag::GPSLongitudeRef)));
    let (lat, lon) = if ex.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).is_some() && ex.get_field(exif::Tag::GPSLongitude, exif::In::PRIMARY).is_some() { (lat, lon) } else { (None, None) };
    if sane(dt, MIN_YEAR) {
        Meta { taken: dt, source: "exif", lat, lon }
    } else {
        Meta { taken: None, source: "none", lat, lon }
    }
}

// ---- QuickTime / MP4 ----

type Atom = ([u8; 4], u64, u64); // type, start of payload, end

fn u32_at(f: &mut File, pos: u64) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    f.seek(SeekFrom::Start(pos))?;
    f.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn atoms(f: &mut File, start: u64, end: u64) -> std::io::Result<Vec<Atom>> {
    let mut out = vec![];
    let mut pos = start;
    while pos + 8 <= end {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h)?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = h[4..8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            let mut b = [0u8; 8];
            f.read_exact(&mut b)?;
            size = u64::from_be_bytes(b);
            hdr = 16;
        } else if size == 0 {
            size = end - pos;
        }
        if size < hdr {
            break;
        }
        out.push((typ, pos + hdr, (pos + size).min(end)));
        pos += size;
    }
    Ok(out)
}

fn read_bytes(f: &mut File, pos: u64, n: u64) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(pos))?;
    let mut b = vec![0u8; n as usize];
    f.read_exact(&mut b)?;
    Ok(b)
}

fn ascii_ignore(b: &[u8]) -> String {
    b.iter().filter(|c| c.is_ascii()).map(|&c| c as char).collect()
}

fn utf8_ignore(b: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = b;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                out.push_str(std::str::from_utf8(&rest[..e.valid_up_to()]).unwrap());
                rest = &rest[e.valid_up_to() + e.error_len().unwrap_or(1)..];
            }
        }
    }
}

fn iso6709(s: &str) -> (Option<f64>, Option<f64>) {
    let re = Regex::new(r"^([+-]\d+(?:\.\d+)?)([+-]\d+(?:\.\d+)?)").unwrap();
    match re.captures(s.trim()) {
        Some(m) => (m[1].parse().ok(), m[2].parse().ok()),
        None => (None, None),
    }
}

fn read_video(path: &Path) -> std::io::Result<Meta> {
    let mut f = File::open(path)?;
    let total = f.metadata()?.len();
    let mut keys: Vec<String> = vec![];
    let mut values: HashMap<String, String> = HashMap::new();
    let mut mvhd_dt: Option<NaiveDateTime> = None;
    let mut xyz: Option<String> = None;
    for (typ, s, e) in atoms(&mut f, 0, total)? {
        if &typ != b"moov" {
            continue;
        }
        for (t2, mut s2, e2) in atoms(&mut f, s, e)? {
            match &t2 {
                b"mvhd" => {
                    let ver = read_bytes(&mut f, s2, 1)?[0];
                    let secs = if ver == 1 {
                        u64::from_be_bytes(read_bytes(&mut f, s2 + 4, 8)?.try_into().unwrap())
                    } else {
                        u32_at(&mut f, s2 + 4)? as u64
                    };
                    if secs != 0 {
                        let ts = secs as i64 - MAC_EPOCH_OFFSET;
                        let dt = Local.timestamp_opt(ts, 0).earliest().ok_or_else(|| std::io::Error::other("date out of range"))?;
                        mvhd_dt = Some(dt.naive_local());
                    }
                }
                b"udta" => {
                    for (t3, s3, e3) in atoms(&mut f, s2, e2)? {
                        if t3 == [0xA9, b'x', b'y', b'z'] {
                            xyz = Some(ascii_ignore(&read_bytes(&mut f, s3 + 4, e3 - s3 - 4)?));
                        }
                    }
                }
                b"meta" => {
                    if &read_bytes(&mut f, s2 + 4, 4)? != b"hdlr" {
                        s2 += 4; // QuickTime meta has no version/flags
                    }
                    for (t3, s3, e3) in atoms(&mut f, s2, e2)? {
                        if &t3 == b"keys" {
                            let n = u32_at(&mut f, s3 + 4)?;
                            let mut pos = s3 + 8;
                            for _ in 0..n {
                                let ksz = u32_at(&mut f, pos)? as u64;
                                if ksz < 8 {
                                    break;
                                }
                                keys.push(ascii_ignore(&read_bytes(&mut f, pos + 8, ksz - 8)?));
                                pos += ksz;
                            }
                        } else if &t3 == b"ilst" {
                            for (t4, s4, e4) in atoms(&mut f, s3, e3)? {
                                let idx = u32::from_be_bytes(t4) as i64 - 1;
                                for (t5, s5, e5) in atoms(&mut f, s4, e4)? {
                                    if &t5 == b"data" && idx >= 0 && (idx as usize) < keys.len() {
                                        values.insert(keys[idx as usize].clone(), utf8_ignore(&read_bytes(&mut f, s5 + 8, e5 - s5 - 8)?));
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let loc = values.get("com.apple.quicktime.location.ISO6709").filter(|s| !s.is_empty()).cloned().or(xyz.filter(|s| !s.is_empty()));
    let (lat, lon) = loc.map(|l| iso6709(&l)).unwrap_or((None, None));
    if let Some(cd) = values.get("com.apple.quicktime.creationdate") {
        let re = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})").unwrap();
        if let Some(m) = re.captures(cd.trim()) {
            let n = |i: usize| m[i].parse::<u32>().unwrap_or(0);
            let dt = NaiveDate::from_ymd_opt(m[1].parse().unwrap_or(0), n(2), n(3)).and_then(|d| d.and_hms_opt(n(4), n(5), n(6)));
            if sane(dt, MIN_YEAR) {
                return Ok(Meta { taken: dt, source: "quicktime", lat, lon });
            }
        }
    }
    if sane(mvhd_dt, MIN_YEAR) {
        return Ok(Meta { taken: mvhd_dt, source: "mvhd", lat, lon });
    }
    Ok(Meta { taken: None, source: "none", lat, lon })
}

/// A date written in the file name, such as IMG_20240815 or 2024-08-15.
pub fn date_from_name(name: &str) -> Option<NaiveDateTime> {
    let b = name.as_bytes();
    let digit = |i: usize| b.get(i).map(|c| c.is_ascii_digit()).unwrap_or(false);
    let num = |i: usize, n: usize| -> Option<u32> {
        if (i..i + n).all(digit) { name.get(i..i + n)?.parse().ok() } else { None }
    };
    for i in 0..b.len() {
        if i > 0 && digit(i - 1) {
            continue;
        }
        let Some(year) = num(i, 4).filter(|y| (1900..=2099).contains(y)) else { continue };
        let mut j = i + 4;
        if matches!(b.get(j), Some(b'-') | Some(b'_')) {
            j += 1;
        }
        let Some(month) = num(j, 2).filter(|m| (1..=12).contains(m)) else { continue };
        j += 2;
        if matches!(b.get(j), Some(b'-') | Some(b'_')) {
            j += 1;
        }
        let Some(day) = num(j, 2).filter(|d| (1..=31).contains(d)) else { continue };
        j += 2;
        if digit(j) {
            continue;
        }
        let dt = NaiveDate::from_ymd_opt(year as i32, month, day).and_then(|d| d.and_hms_opt(0, 0, 0));
        return if sane(dt, MIN_YEAR) { dt } else { None };
    }
    None
}

/// Best effort. Never fails: a file that cannot be parsed just has no embedded date.
pub fn read_metadata(path: &Path, name: &str) -> Meta {
    let ext = ext_of(name);
    let mut meta = if IMAGE_EXTS.contains(&ext.as_str()) {
        read_image(path)
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        read_video(path).unwrap_or_default()
    } else {
        Meta::default()
    };
    if meta.taken.is_none() {
        if let Some(dt) = date_from_name(name) {
            meta.taken = Some(dt);
            meta.source = "filename";
        }
    }
    meta
}
