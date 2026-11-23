//! Small previews. JPEG and PNG are drawn here; HEIC and video are asked of the operating system's own
//! thumbnail provider (Windows Explorer's, macOS Quick Look), the same one Photos and Explorer use.

use image::imageops::FilterType;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

pub const OWN: [&str; 3] = ["jpg", "jpeg", "png"];
pub const OS_DRAWN: [&str; 6] = ["heic", "heif", "mov", "mp4", "m4v", "dng"];

/// Can this computer draw a preview for the file type?
pub fn can_preview(ext: &str) -> bool {
    OWN.contains(&ext) || (OS_DRAWN.contains(&ext) && cfg!(any(windows, target_os = "macos")))
}

pub fn cache_path(root: &Path, key: &str, size: u32) -> PathBuf {
    let h: String = Sha1::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    root.join(".photovault").join("thumbs").join(format!("{}_{size}.jpg", &h[..20]))
}

fn orientation(path: &Path) -> u32 {
    let Ok(f) = std::fs::File::open(path) else { return 1 };
    exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(f))
        .ok()
        .and_then(|e| e.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)))
        .unwrap_or(1)
}

fn encode(img: image::DynamicImage) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&rgb).ok()?;
    Some(out)
}

fn upright(img: image::DynamicImage, o: u32) -> image::DynamicImage {
    match o {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn own(path: &Path, size: u32, jpeg: bool) -> Option<Vec<u8>> {
    let img = image::open(path).ok()?;
    let img = if jpeg { upright(img, orientation(path)) } else { img };
    encode(img.resize(size, size, FilterType::Triangle))
}

/// A small upright JPEG of an image or video. None for anything this computer cannot draw.
pub fn make_thumbnail(path: &Path, size: u32) -> Option<Vec<u8>> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => own(path, size, true).or_else(|| os_thumb(path, size)),
        "png" => own(path, size, false).or_else(|| os_thumb(path, size)),
        e if OS_DRAWN.contains(&e) => os_thumb(path, size),
        _ => None,
    }
}

#[cfg(windows)]
fn os_thumb(path: &Path, size: u32) -> Option<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let hbmp = factory.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_BIGGERSIZEOK | SIIGBF_THUMBNAILONLY).ok()?;
        let mut bm = BITMAP::default();
        GetObjectW(hbmp, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut info = BITMAPINFO::default();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let got = GetDIBits(dc, hbmp, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(hbmp);
        if got == 0 {
            return None;
        }
        for p in px.chunks_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
            p[3] = 255;
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, px)?;
        encode(image::DynamicImage::ImageRgba8(img).resize(size, size, FilterType::Triangle))
    }
}

#[cfg(target_os = "macos")]
fn os_thumb(path: &Path, size: u32) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("photovault-ql-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let ok = std::process::Command::new("qlmanage")
        .args(["-t", "-s", &size.to_string(), "-o"])
        .arg(&dir)
        .arg(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let out = dir.join(format!("{}.png", path.file_name()?.to_string_lossy()));
    let data = if ok { image::open(&out).ok().and_then(|i| encode(i.resize(size, size, FilterType::Triangle))) } else { None };
    let _ = std::fs::remove_dir_all(&dir);
    data
}

#[cfg(not(any(windows, target_os = "macos")))]
fn os_thumb(_path: &Path, _size: u32) -> Option<Vec<u8>> {
    None
}
