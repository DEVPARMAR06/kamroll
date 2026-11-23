//! Rule-based tags: what kind of picture is this, and how sure are we? No AI, no network.
//! Each rule adds a reason a person can read; the best rule wins; a user's own choice always overrides it.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

pub const IMAGE_EXTS: [&str; 8] = [".jpg", ".jpeg", ".heic", ".heif", ".png", ".dng", ".tif", ".tiff"];
pub const VIDEO_EXTS: [&str; 3] = [".mov", ".mp4", ".m4v"];
pub const SIDECAR_EXTS: [&str; 2] = [".aae", ".plist"];

const SCREENS: [(u32, u32); 17] = [
    (1179, 2556), (1170, 2532), (1290, 2796), (1284, 2778), (1125, 2436), (1242, 2688), (828, 1792), (750, 1334), (1242, 2208),
    (1206, 2622), (1320, 2868), (640, 1136), (640, 960), (1668, 2388), (2048, 2732), (1640, 2360), (1488, 2266),
];

pub fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

#[derive(Default, Clone, Debug)]
pub struct Facts {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    pub luma: Option<f64>,
    pub sharp: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub tag: String,
    pub conf: i64,
    pub reasons: Vec<String>,
}

fn v(tag: &str, conf: i64, reasons: Vec<String>) -> Verdict {
    Verdict { tag: tag.into(), conf, reasons }
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

pub fn classify(name: &str, facts: Option<&Facts>, date_src: Option<&str>) -> Verdict {
    static WA: OnceLock<Regex> = OnceLock::new();
    static RID: OnceLock<Regex> = OnceLock::new();
    static LONG: OnceLock<Regex> = OnceLock::new();
    static COPY: OnceLock<Regex> = OnceLock::new();
    let wa = re(&WA, r"(?i)^(IMG|VID|PTT|AUD|STK|DOC)-\d{8}-WA\d+");
    let rid = re(&RID, r"(?i)^(filtered-)?([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{32})\.");
    let long = re(&LONG, r"^\d{9,}\.");
    let copy = re(&COPY, r"^.+ \d{1,2}\.[A-Za-z0-9]+$");

    let ext = ext_of(name);
    let default = Facts::default();
    let f = facts.unwrap_or(&default);
    let camera = f.make.is_some() || f.model.is_some();
    let is_video = VIDEO_EXTS.contains(&ext.as_str());

    if name.contains("(old export)") {
        return v("duplicate", 80, vec!["Named as an old export of a video that is also on the phone".into()]);
    }
    if copy.is_match(name) {
        return v("duplicate", 70, vec!["The name ends in a number, which is how a copied file is named".into()]);
    }
    if let Some(l) = f.luma {
        if l < 3.0 {
            return v("junk", 90, vec!["Almost entirely black".into(), format!("Average brightness is {l:.0} of 255")]);
        }
        if l < 8.0 {
            return v("junk", 45, vec!["Very dark: could be a night photo you want".into(), format!("Average brightness is {l:.0} of 255")]);
        }
    }
    if wa.is_match(name) {
        return v("social", 97, vec!["The name matches WhatsApp's pattern".into(), if camera { "Has camera data" } else { "No camera data" }.into()]);
    }
    if let (false, false, Some(w), Some(h)) = (is_video, camera, f.width, f.height) {
        let (w, h) = (w as u32, h as u32);
        if SCREENS.iter().any(|&(a, b)| (a, b) == (w, h) || (b, a) == (w, h)) {
            return v("screenshot", 98, vec![format!("{w} x {h} is a phone or tablet screen size"), "No camera make or model".into()]);
        }
    }
    if ext == ".png" && !camera {
        return v("screenshot", 80, vec!["PNG with no camera data".into()]);
    }
    if rid.is_match(name) || long.is_match(name) {
        let base = if (is_video && date_src != Some("quicktime")) || (!is_video && !camera) { 82 } else { 62 };
        return v(
            "social",
            base,
            vec![
                "The name is a random ID or a long number, as when a file is shared or saved from an app".into(),
                if base > 70 { "No camera recording of its own" } else { "But it has camera data" }.into(),
            ],
        );
    }
    if camera {
        let who = [f.make.as_deref(), f.model.as_deref()].iter().flatten().copied().collect::<Vec<_>>().join(" ");
        return v("memory", 88, vec![format!("Camera recorded it ({who})")]);
    }
    if is_video && date_src == Some("quicktime") {
        return v("memory", 82, vec!["The video has the camera's own creation date and location".into()]);
    }
    if is_video {
        return v("memory", 60, vec!["A video with no clear sign of where it came from".into()]);
    }
    v("memory", 55, vec!["No clear signal either way".into()])
}

// ---- reading the picture itself ----

fn ascii(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let field = ex.get_field(tag, exif::In::PRIMARY)?;
    if let exif::Value::Ascii(parts) = &field.value {
        let s = String::from_utf8_lossy(parts.first()?).trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Width and height from the `ispe` boxes of a HEIC/HEIF file (the largest one is the picture itself).
fn heif_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(4 << 20).read_to_end(&mut buf).ok()?;
    let mut best: Option<(u32, u32)> = None;
    let mut i = 0;
    while i + 16 <= buf.len() {
        if &buf[i..i + 4] == b"ispe" {
            let w = u32::from_be_bytes(buf[i + 8..i + 12].try_into().ok()?);
            let h = u32::from_be_bytes(buf[i + 12..i + 16].try_into().ok()?);
            if best.map(|(a, b)| (w as u64) * (h as u64) > (a as u64) * (b as u64)).unwrap_or(true) {
                best = Some((w, h));
            }
        }
        i += 1;
    }
    best
}

/// A greyscale picture, shrunk to fit inside `max` x `max` by averaging.
fn shrink(grey: &[u8], w: usize, h: usize, max: usize) -> (Vec<u8>, usize, usize) {
    if w <= max && h <= max {
        return (grey.to_vec(), w, h);
    }
    let scale = (max as f64 / w as f64).min(max as f64 / h as f64);
    let (nw, nh) = (((w as f64 * scale).round() as usize).max(1), ((h as f64 * scale).round() as usize).max(1));
    let mut out = vec![0u8; nw * nh];
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1).min(h));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1).min(w));
            let mut sum = 0u32;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    sum += grey[yy * w + xx] as u32;
                }
            }
            out[y * nw + x] = (sum / ((y1 - y0) * (x1 - x0)) as u32) as u8;
        }
    }
    (out, nw, nh)
}

fn grey_stats(grey: &[u8], w: usize, h: usize) -> (f64, f64) {
    let (g, w, h) = shrink(grey, w, h, 128);
    let n = (w * h) as f64;
    let mean = g.iter().map(|&p| p as f64).sum::<f64>() / n;
    let mut edges = g.clone();
    if w > 2 && h > 2 {
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let c = |dx: usize, dy: usize| g[(y + dy - 1) * w + x + dx - 1] as i32;
                let s = 8 * c(1, 1) - (c(0, 0) + c(1, 0) + c(2, 0) + c(0, 1) + c(2, 1) + c(0, 2) + c(1, 2) + c(2, 2));
                edges[y * w + x] = s.clamp(0, 255) as u8;
            }
        }
    }
    let em = edges.iter().map(|&p| p as f64).sum::<f64>() / n;
    let var = edges.iter().map(|&p| (p as f64 - em).powi(2)).sum::<f64>() / n;
    (mean, var)
}

fn to_grey(px: &[u8], channels: usize) -> Vec<u8> {
    px.chunks(channels)
        .map(|p| match channels {
            1 => p[0],
            _ => ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8,
        })
        .collect()
}

type Pixels = (Vec<u8>, usize, usize, usize, i64, i64);

fn jpeg_pixels(path: &Path) -> Option<Pixels> {
    let mut d = jpeg_decoder::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    d.read_info().ok()?;
    let info = d.info()?;
    let (ow, oh) = (info.width as i64, info.height as i64);
    let (sw, sh) = d.scale(256, 256).ok()?;
    let data = d.decode().ok()?;
    let ch = match d.info()?.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1,
        jpeg_decoder::PixelFormat::RGB24 => 3,
        _ => return None,
    };
    Some((data, sw as usize, sh as usize, ch, ow, oh))
}

fn png_pixels(path: &Path) -> Option<Pixels> {
    let im = image::open(path).ok()?;
    let (w, h) = (im.width() as i64, im.height() as i64);
    let g = im.to_luma8();
    Some((g.as_raw().clone(), g.width() as usize, g.height() as usize, 1, w, h))
}

/// Read the header and a small greyscale copy. Never fails: a damaged file just has no facts.
/// HEIC photos give their camera data and size from the file header; their pixels are not read here.
pub fn read_facts(path: &Path) -> Facts {
    let mut f = Facts::default();
    let ext = ext_of(&path.to_string_lossy());
    if let Ok(file) = std::fs::File::open(path) {
        if let Ok(ex) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) {
            f.make = ascii(&ex, exif::Tag::Make);
            f.model = ascii(&ex, exif::Tag::Model);
            f.software = ascii(&ex, exif::Tag::Software);
        }
    }
    let pixels = match ext.as_str() {
        ".jpg" | ".jpeg" => jpeg_pixels(path),
        ".png" => png_pixels(path),
        _ => None,
    };
    match pixels {
        Some((data, w, h, ch, ow, oh)) => {
            f.width = Some(ow);
            f.height = Some(oh);
            let (mean, var) = grey_stats(&to_grey(&data, ch), w, h);
            f.luma = Some(mean);
            f.sharp = Some(var);
        }
        None if ext == ".heic" || ext == ".heif" => {
            if let Some((w, h)) = heif_size(path) {
                f.width = Some(w as i64);
                f.height = Some(h as i64);
            }
        }
        None => {}
    }
    f
}

/// A JSON list of strings written exactly as Python's json.dumps does (", " between items, non-ASCII escaped),
/// so either engine writes the same bytes into the manifest.
pub fn json_list<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::from("[");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        for c in s.as_ref().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut b = [0u16; 2];
                    for u in c.encode_utf16(&mut b) {
                        out.push_str(&format!("\\u{:04x}", u));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}
