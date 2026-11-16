//! Reading the BMP a driver hands over.  Windows' scanning interface (WIA)
//! delivers a scan as a BMP file in memory; this turns it into a [`RawImage`].
//!
//! Only what a scanner driver produces is accepted: uncompressed, 8 bits with
//! a palette, or 24 or 32 bits.  The file may or may not start with the
//! 14-byte file header (WIA has both a "BMP" and a "memory BMP" format).

use crate::RawImage;

fn u16_at(d: &[u8], p: usize) -> Option<u32> {
    Some(u16::from_le_bytes(d.get(p..p + 2)?.try_into().ok()?) as u32)
}

fn u32_at(d: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(p..p + 4)?.try_into().ok()?))
}

fn i32_at(d: &[u8], p: usize) -> Option<i32> {
    Some(i32::from_le_bytes(d.get(p..p + 4)?.try_into().ok()?))
}

/// What the header says, before any pixel is touched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub bits: u32,
    /// rows are stored first-row-first (a negative height in the header)
    pub top_down: bool,
    /// bytes per stored row, padded to a multiple of four
    pub stride: usize,
    /// where the pixels start
    pub pixels_at: usize,
    /// where the palette starts (8-bit only) and how many entries it has
    pub palette_at: usize,
    pub palette_len: usize,
}

pub fn layout(d: &[u8]) -> Result<Layout, String> {
    let short = || "the scanner driver sent an incomplete picture (BMP header cut off)".to_string();
    // with a file header ("BM", size, reserved, offset to the pixels) or without
    let (dib, offset_from_file) = if d.len() >= 2 && &d[..2] == b"BM" {
        (14, u32_at(d, 10).ok_or_else(short)? as usize)
    } else {
        (0, 0)
    };
    let header = u32_at(d, dib).ok_or_else(short)? as usize;
    if header < 40 {
        return Err(format!("the scanner driver sent a picture in an unknown format (BMP header of {header} bytes)"));
    }
    let width = i32_at(d, dib + 4).ok_or_else(short)?;
    let height = i32_at(d, dib + 8).ok_or_else(short)?;
    let bits = u16_at(d, dib + 14).ok_or_else(short)?;
    let compression = u32_at(d, dib + 16).ok_or_else(short)?;
    let colours_used = u32_at(d, dib + 32).ok_or_else(short)? as usize;
    if width <= 0 || height == 0 {
        return Err(format!("the scanner driver sent a picture of {width} x {height} pixels"));
    }
    // 0 = uncompressed; 3 = "bit fields", which for 32 bits is in practice plain BGRX
    if !(compression == 0 || (compression == 3 && bits == 32)) {
        return Err(format!("the scanner driver sent a compressed picture (BMP compression {compression})"));
    }
    if !matches!(bits, 8 | 24 | 32) {
        return Err(format!("the scanner driver sent {bits} bits per pixel; 8, 24 or 32 are understood"));
    }
    let width = width as usize;
    let top_down = height < 0;
    let height = height.unsigned_abs() as usize;
    let stride = (width * bits as usize).div_ceil(32) * 4;
    let palette_at = dib + header;
    let palette_len = if bits == 8 { if colours_used == 0 { 256 } else { colours_used.min(256) } } else { 0 };
    let after_palette = palette_at + palette_len * 4 + if compression == 3 && header == 40 { 12 } else { 0 };
    // Trust the file header's offset when it is plausible; some drivers leave it zero.
    let pixels_at = if offset_from_file >= after_palette && offset_from_file < d.len() { offset_from_file } else { after_palette };
    Ok(Layout { width, height, bits, top_down, stride, pixels_at, palette_at, palette_len })
}

/// Decode `d` into grey (`want_gray`) or RGB.  A picture that is a few rows
/// short (a transfer that ended early) is an error, not a shorter image.
pub fn decode(d: &[u8], want_gray: bool) -> Result<RawImage, String> {
    let l = layout(d)?;
    let need = l.pixels_at + l.stride * (l.height - 1) + (l.width * l.bits as usize).div_ceil(8);
    if d.len() < need {
        let rows = d.len().saturating_sub(l.pixels_at) / l.stride;
        return Err(format!(
            "the scan arrived incomplete: {rows} of {} rows ({} of {need} bytes)",
            l.height,
            d.len()
        ));
    }
    // 8-bit: the palette says what each value means (normally a grey ramp, but
    // a driver may store it the other way round)
    let mut palette = [[0u8; 3]; 256];
    for (i, p) in palette.iter_mut().enumerate() {
        *p = [i as u8; 3];
    }
    if l.bits == 8 {
        for (i, p) in palette.iter_mut().enumerate().take(l.palette_len) {
            let at = l.palette_at + i * 4;
            if let Some(e) = d.get(at..at + 3) {
                *p = [e[2], e[1], e[0]]; // stored as B, G, R
            }
        }
    }
    let grey = |r: u8, g: u8, b: u8| ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114 + 500) / 1000) as u8;
    let channels = if want_gray { 1 } else { 3 };
    let mut data = Vec::with_capacity(l.width * l.height * channels);
    for row in 0..l.height {
        let stored = if l.top_down { row } else { l.height - 1 - row };
        let line = &d[l.pixels_at + stored * l.stride..];
        match (l.bits, want_gray) {
            (8, true) => data.extend(line[..l.width].iter().map(|&v| {
                let [r, g, b] = palette[v as usize];
                if r == g && g == b { r } else { grey(r, g, b) }
            })),
            (8, false) => {
                for &v in &line[..l.width] {
                    data.extend_from_slice(&palette[v as usize]);
                }
            }
            (bits, _) => {
                let step = bits as usize / 8;
                for px in line[..l.width * step].chunks_exact(step) {
                    let (b, g, r) = (px[0], px[1], px[2]);
                    if want_gray {
                        data.push(grey(r, g, b));
                    } else {
                        data.extend_from_slice(&[r, g, b]);
                    }
                }
            }
        }
    }
    Ok(RawImage { width: l.width, height: l.height, channels, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BMP as a driver would write it.
    fn bmp(width: i32, height: i32, bits: u16, file_header: bool, palette: Option<&[[u8; 4]]>, rows: &[Vec<u8>]) -> Vec<u8> {
        let stride = (width as usize * bits as usize).div_ceil(32) * 4;
        let pal_bytes = palette.map_or(0, |p| p.len() * 4);
        let mut d = Vec::new();
        if file_header {
            d.extend_from_slice(b"BM");
            d.extend_from_slice(&0u32.to_le_bytes()); // size: drivers get this wrong, so don't rely on it
            d.extend_from_slice(&[0; 4]);
            d.extend_from_slice(&((14 + 40 + pal_bytes) as u32).to_le_bytes());
        }
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&width.to_le_bytes());
        d.extend_from_slice(&height.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&bits.to_le_bytes());
        d.extend_from_slice(&[0; 24]);
        if let Some(p) = palette {
            for e in p {
                d.extend_from_slice(e);
            }
        }
        for r in rows {
            let mut r = r.clone();
            r.resize(stride, 0xEE); // padding must be ignored
            d.extend_from_slice(&r);
        }
        d
    }

    fn ramp() -> Vec<[u8; 4]> {
        (0..256).map(|i| [i as u8, i as u8, i as u8, 0]).collect()
    }

    #[test]
    fn colour_rows_are_bottom_up_and_bgr() {
        // 3 x 2: stored bottom row first
        let bottom = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        let top = vec![11, 12, 13, 14, 15, 16, 17, 18, 19];
        let d = bmp(3, 2, 24, true, None, &[bottom, top]);
        let img = decode(&d, false).unwrap();
        assert_eq!((img.width, img.height, img.channels), (3, 2, 3));
        assert_eq!(img.data, [13, 12, 11, 16, 15, 14, 19, 18, 17, 3, 2, 1, 6, 5, 4, 9, 8, 7]);
    }

    #[test]
    fn a_negative_height_means_top_down() {
        let first = vec![1, 2, 3];
        let second = vec![4, 5, 6];
        let d = bmp(1, -2, 24, true, None, &[first, second]);
        assert_eq!(decode(&d, false).unwrap().data, [3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn grey_with_a_palette_and_row_padding() {
        // 5 px wide: rows are padded to 8 bytes
        let d = bmp(5, 2, 8, true, Some(&ramp()), &[vec![10, 20, 30, 40, 50], vec![60, 70, 80, 90, 100]]);
        let l = layout(&d).unwrap();
        assert_eq!((l.stride, l.pixels_at), (8, 14 + 40 + 1024));
        let img = decode(&d, true).unwrap();
        assert_eq!((img.width, img.height, img.channels), (5, 2, 1));
        assert_eq!(img.data, [60, 70, 80, 90, 100, 10, 20, 30, 40, 50]);
    }

    #[test]
    fn an_inverted_palette_is_followed() {
        let inverted: Vec<[u8; 4]> = (0..256).map(|i| [255 - i as u8, 255 - i as u8, 255 - i as u8, 0]).collect();
        let d = bmp(2, 1, 8, true, Some(&inverted), &[vec![0, 250]]);
        assert_eq!(decode(&d, true).unwrap().data, [255, 5]);
    }

    #[test]
    fn without_the_file_header() {
        let d = bmp(2, 1, 8, false, Some(&ramp()), &[vec![7, 9]]);
        assert_eq!(layout(&d).unwrap().pixels_at, 40 + 1024);
        assert_eq!(decode(&d, true).unwrap().data, [7, 9]);
    }

    #[test]
    fn asked_for_the_other_kind() {
        // grey wanted, colour delivered: weighted mean
        let d = bmp(1, 1, 24, true, None, &[vec![0, 0, 255]]); // pure red
        assert_eq!(decode(&d, true).unwrap().data, [76]);
        // colour wanted, grey delivered: the value three times
        let d = bmp(1, 1, 8, true, Some(&ramp()), &[vec![42]]);
        assert_eq!(decode(&d, false).unwrap().data, [42, 42, 42]);
    }

    #[test]
    fn thirty_two_bits_drop_the_fourth_byte() {
        let d = bmp(2, 1, 32, true, None, &[vec![1, 2, 3, 99, 4, 5, 6, 99]]);
        assert_eq!(decode(&d, false).unwrap().data, [3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn a_cut_off_scan_is_an_error() {
        let mut d = bmp(4, 3, 24, true, None, &[vec![0; 12], vec![0; 12], vec![0; 12]]);
        d.truncate(d.len() - 13);
        let e = decode(&d, false).err().unwrap();
        assert!(e.contains("1 of 3 rows"), "{e}");
        assert!(layout(&d[..20]).is_err());
        assert!(decode(b"BMxx", false).is_err());
    }

    #[test]
    fn the_last_row_needs_no_padding() {
        // a driver may stop after the last pixel of the last stored row
        let mut d = bmp(1, 2, 24, true, None, &[vec![1, 2, 3], vec![4, 5, 6]]);
        d.truncate(d.len() - 1);
        assert_eq!(decode(&d, false).unwrap().data, [6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn other_formats_are_refused() {
        let mut d = bmp(8, 1, 24, true, None, &[vec![0; 24]]);
        d[14 + 14] = 1; // 1 bit per pixel
        assert!(decode(&d, true).err().unwrap().contains("1 bits"));
        let mut d = bmp(8, 1, 24, true, None, &[vec![0; 24]]);
        d[14 + 16] = 1; // run-length compression
        assert!(decode(&d, true).err().unwrap().contains("compressed"));
    }
}
