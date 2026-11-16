//! Saving the result: `imgNNNNN.png` in the user's Pictures folder, never
//! overwriting a file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use opencv::core::{Mat, Vector};
use opencv::prelude::*;
use opencv::{imgcodecs, imgproc};

use crate::cvx;

pub fn pictures_dir() -> PathBuf {
    crate::platform::pictures_dir()
}

/// Highest NNNNN among existing imgNNNNN.png files.
fn highest_number(dir: &Path) -> u32 {
    let mut best = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name.strip_prefix("img").and_then(|s| s.strip_suffix(".png")) {
                if num.len() >= 5 && num.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(v) = num.parse::<u32>() {
                        best = best.max(v);
                    }
                }
            }
        }
    }
    best
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Insert a pHYs chunk (resolution) right after IHDR.
fn with_dpi(png: &[u8], dpi: f64) -> Vec<u8> {
    // signature (8) + IHDR chunk (4 len + 4 type + 13 data + 4 crc)
    let split = 8 + 25;
    if png.len() < split || &png[12..16] != b"IHDR" {
        return png.to_vec();
    }
    let ppm = (dpi / 0.0254).round() as u32;
    let mut chunk = Vec::with_capacity(21);
    chunk.extend_from_slice(&9u32.to_be_bytes());
    let mut body = Vec::with_capacity(13);
    body.extend_from_slice(b"pHYs");
    body.extend_from_slice(&ppm.to_be_bytes());
    body.extend_from_slice(&ppm.to_be_bytes());
    body.push(1); // unit: metre
    chunk.extend_from_slice(&body);
    chunk.extend_from_slice(&crc32(&body).to_be_bytes());
    let mut out = Vec::with_capacity(png.len() + chunk.len());
    out.extend_from_slice(&png[..split]);
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&png[split..]);
    out
}

/// What a saved result takes per pixel: the eleven results saved on Linux so
/// far (1200 dpi, 21 to 143 megapixels) take 4.76 to 4.91 bytes a pixel.
const RESULT_BYTES_PER_PIXEL: f64 = 4.8;

/// Roughly the size in bytes of the PNG a result of `pixels` pixels is saved as.
pub fn estimated_file_size(pixels: u64) -> u64 {
    (pixels as f64 * RESULT_BYTES_PER_PIXEL).round() as u64
}

/// A file size for people: "107 MB", "3.4 GB".
pub fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb < 10.0 {
        format!("{mb:.1} MB")
    } else if mb < 999.5 {
        format!("{mb:.0} MB")
    } else {
        format!("{:.1} GB", mb / 1000.0)
    }
}

/// A pixel count for people: "22.0 megapixels", "708 megapixels".
pub fn megapixels(pixels: u64) -> String {
    let mp = pixels as f64 / 1e6;
    if mp < 99.95 {
        format!("{mp:.1} megapixels")
    } else {
        format!("{mp:.0} megapixels")
    }
}

/// Encode an RGB image (8 or 16 bits) as PNG bytes with the given resolution.
pub fn encode_png(rgb: &Mat, dpi: f64) -> Result<Vec<u8>, String> {
    let mut bgr = Mat::default();
    cvx::cvt_color(rgb, &mut bgr, imgproc::COLOR_RGB2BGR, 0).map_err(|e| e.to_string())?;
    let mut buf = Vector::<u8>::new();
    let params = Vector::<i32>::from_slice(&[imgcodecs::IMWRITE_PNG_COMPRESSION, 3]);
    imgcodecs::imencode(".png", &bgr, &mut buf, &params).map_err(|e| e.to_string())?;
    Ok(with_dpi(&buf.to_vec(), dpi))
}

/// Write `png` to the next free `dir/imgNNNNN.png`.  The file is created with
/// O_EXCL, so an existing file is never overwritten, even by a racing program.
pub fn save_next(dir: &Path, png: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut n = highest_number(dir) + 1;
    loop {
        if n > 99_999 {
            return Err(format!("no free imgNNNNN.png name left in {}", dir.display()));
        }
        let path = dir.join(format!("img{n:05}.png"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(png).and_then(|_| f.sync_all()) {
                    drop(f);
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("cannot write {}: {e}", path.display()));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("mps-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("img00007.png"), b"x").unwrap();
        std::fs::write(dir.join("img00003.png"), b"y").unwrap();
        let p = save_next(&dir, b"new").unwrap();
        assert_eq!(p.file_name().unwrap(), "img00008.png");
        let p2 = save_next(&dir, b"new2").unwrap();
        assert_eq!(p2.file_name().unwrap(), "img00009.png");
        assert_eq!(std::fs::read(dir.join("img00007.png")).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_are_written_for_people() {
        assert_eq!(megapixels(8_640_000), "8.6 megapixels");
        assert_eq!(megapixels(44_203_536), "44.2 megapixels");
        assert_eq!(megapixels(707_788_800), "708 megapixels");
        assert_eq!(human_size(4_300_000), "4.3 MB");
        assert_eq!(human_size(107_700_000), "108 MB");
        assert_eq!(human_size(3_397_386_240), "3.4 GB");
        // a 22.6-megapixel result was 107.7 MB
        assert_eq!(human_size(estimated_file_size(4996 * 4522)), "108 MB");
    }

    #[test]
    fn crc_known_value() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
    }
}
