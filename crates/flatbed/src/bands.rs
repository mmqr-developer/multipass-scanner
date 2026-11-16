//! Putting a scan back together from the bands a driver delivers.
//!
//! Used by backends whose driver hands the image over a few rows at a time
//! (macOS).  It has no system dependency, so it is built and tested everywhere.

use crate::RawImage;

/// What the driver says about one band.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BandInfo {
    /// size of the whole image in pixels
    pub width: usize,
    pub height: usize,
    pub bits_per_component: usize,
    pub bits_per_pixel: usize,
    pub components: usize,
    /// bytes from one row to the next, as the driver reports it
    pub bytes_per_row: usize,
    /// first row of this band, as the driver reports it
    pub start_row: usize,
    /// rows in this band
    pub rows: usize,
}

/// Collects bands into one packed 8-bit image.
///
/// Bands are taken in the order they arrive.  Drivers are not careful with the
/// numbers they report, so the row stride is worked out from the size of the
/// band itself where that is possible, and `start_row` is only compared, not
/// trusted.
///
/// Grey data is read as one continuous stream of packed rows, whatever the
/// driver says about rows and strides.  Epson's driver for the V19 II on macOS
/// (seen at 75 dpi, 637 pixels wide) reports `bytesPerRow` 638 and 102 rows per
/// band, sends bands of exactly 102 × 638 bytes, but packs the rows at 637
/// bytes, so rows run on from one band into the next.  Read band by band at
/// 638, the image came out sheared by a pixel per row.
#[derive(Default)]
pub(crate) struct Assembler {
    width: usize,
    height: usize,
    /// bytes per pixel as delivered (1, 3 or 4) and as kept (1 or 3)
    src_px: usize,
    out_px: usize,
    data: Vec<u8>,
    rows_done: usize,
    /// grey only: the start of a row whose end is in the next band
    carry: Vec<u8>,
    /// some band's `start_row` was not where the bands before it ended
    pub start_rows_disagree: bool,
}

impl Assembler {
    pub fn rows_done(&self) -> usize {
        self.rows_done
    }

    pub fn add(&mut self, info: &BandInfo, bytes: &[u8]) -> Result<(), String> {
        if self.data.is_empty() {
            if info.width == 0 || info.height == 0 {
                return Err("the driver reported an empty image".into());
            }
            if info.bits_per_component != 8 {
                return Err(format!(
                    "the driver delivered {} bits per sample; 8 were asked for",
                    info.bits_per_component
                ));
            }
            let src_px = if info.bits_per_pixel >= 8 { info.bits_per_pixel / 8 } else { info.components };
            let out_px = match src_px {
                1 => 1,
                3 | 4 => 3, // 4: RGB with a padding or alpha byte, which is dropped
                n => return Err(format!("unexpected image from the driver: {n} bytes per pixel")),
            };
            self.width = info.width;
            self.height = info.height;
            self.src_px = src_px;
            self.out_px = out_px;
            self.data = vec![0; info.width * info.height * out_px];
        } else if info.width != self.width || info.height != self.height {
            return Err("the driver changed the image size in the middle of a scan".into());
        }
        let min_row = self.width * self.src_px;
        if self.src_px == 1 {
            return self.add_stream(bytes);
        }
        // The stride: the band's own size says it best.
        let (rows, stride) = if info.rows > 0 && bytes.len() % info.rows == 0 && bytes.len() / info.rows >= min_row {
            (info.rows, bytes.len() / info.rows)
        } else if info.bytes_per_row >= min_row {
            let rows = if info.rows > 0 { info.rows } else { bytes.len() / info.bytes_per_row };
            (rows, info.bytes_per_row)
        } else {
            return Err(format!(
                "cannot make sense of a band from the driver: {} bytes for {} rows of {} pixels",
                bytes.len(),
                info.rows,
                self.width
            ));
        };
        if rows == 0 {
            return Ok(());
        }
        if bytes.len() < (rows - 1) * stride + min_row {
            return Err(format!("a band from the driver is short: {} bytes for {rows} rows", bytes.len()));
        }
        if info.start_row != self.rows_done {
            self.start_rows_disagree = true;
        }
        let rows = rows.min(self.height - self.rows_done); // more rows than the image has: ignored
        let out_row = self.width * self.out_px;
        for r in 0..rows {
            let src = &bytes[r * stride..r * stride + min_row];
            let at = (self.rows_done + r) * out_row;
            let dst = &mut self.data[at..at + out_row];
            if self.src_px == self.out_px {
                dst.copy_from_slice(src);
            } else {
                for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&s[..3]);
                }
            }
        }
        self.rows_done += rows;
        Ok(())
    }

    /// Grey data: the band's bytes continue the rows where the last band
    /// stopped.  `bytes_per_row`, `rows` and `start_row` are not used.
    fn add_stream(&mut self, bytes: &[u8]) -> Result<(), String> {
        let w = self.width;
        let mut rest = bytes;
        if !self.carry.is_empty() {
            let take = (w - self.carry.len()).min(rest.len());
            self.carry.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.carry.len() < w {
                return Ok(());
            }
            let row = std::mem::take(&mut self.carry);
            self.put_grey_row(&row);
        }
        let mut rows = rest.chunks_exact(w);
        for row in rows.by_ref() {
            self.put_grey_row(row);
        }
        if self.rows_done < self.height {
            self.carry.extend_from_slice(rows.remainder());
        }
        Ok(())
    }

    fn put_grey_row(&mut self, row: &[u8]) {
        if self.rows_done < self.height {
            let at = self.rows_done * self.width;
            self.data[at..at + self.width].copy_from_slice(row);
            self.rows_done += 1;
        }
    }

    /// The finished image.  A grey scan that came back in colour is turned
    /// grey; a colour scan that came back grey is an error.
    pub fn finish(mut self, want_gray: bool) -> Result<RawImage, String> {
        if self.rows_done == 0 {
            return Err("the scan finished without an image".into());
        }
        // A row or two missing at the end is tolerated; more means the scan broke off.
        if self.rows_done < self.height {
            if (self.height - self.rows_done) * 200 > self.height {
                return Err(format!("the scan ended early: {} of {} rows arrived", self.rows_done, self.height));
            }
            self.height = self.rows_done;
            self.data.truncate(self.width * self.height * self.out_px);
        }
        if !want_gray && self.out_px == 1 {
            return Err("the colour scan came back in grey".into());
        }
        if want_gray && self.out_px == 3 {
            // Rec. 601 luma, rounded
            let gray: Vec<u8> = self
                .data
                .chunks_exact(3)
                .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114 + 500) / 1000) as u8)
                .collect();
            return Ok(RawImage { width: self.width, height: self.height, channels: 1, data: gray });
        }
        Ok(RawImage { width: self.width, height: self.height, channels: self.out_px, data: self.data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: usize, height: usize, px: usize, bytes_per_row: usize, start_row: usize, rows: usize) -> BandInfo {
        BandInfo {
            width,
            height,
            bits_per_component: 8,
            bits_per_pixel: 8 * px,
            components: px,
            bytes_per_row,
            start_row,
            rows,
        }
    }

    /// Pixel value that says where it is.
    fn value(x: usize, y: usize, c: usize) -> u8 {
        (x * 7 + y * 31 + c * 101) as u8
    }

    fn band(width: usize, px: usize, stride: usize, y0: usize, rows: usize) -> Vec<u8> {
        let mut b = vec![0xEE; rows * stride];
        for r in 0..rows {
            for x in 0..width {
                for c in 0..px {
                    b[r * stride + x * px + c] = if c < 3 { value(x, y0 + r, c) } else { 0xFF };
                }
            }
        }
        b
    }

    fn check(img: &RawImage, width: usize, height: usize, channels: usize) {
        assert_eq!((img.width, img.height, img.channels), (width, height, channels));
        assert_eq!(img.data.len(), width * height * channels);
        for y in 0..height {
            for x in 0..width {
                for c in 0..channels {
                    assert_eq!(img.data[(y * width + x) * channels + c], value(x, y, c), "x {x} y {y} c {c}");
                }
            }
        }
    }

    #[test]
    fn packed_rgb_in_uneven_bands() {
        let (w, h) = (5, 7);
        let mut a = Assembler::default();
        for (y0, rows) in [(0, 3), (3, 1), (4, 3)] {
            a.add(&info(w, h, 3, w * 3, y0, rows), &band(w, 3, w * 3, y0, rows)).unwrap();
        }
        assert!(!a.start_rows_disagree);
        check(&a.finish(false).unwrap(), w, h, 3);
    }

    #[test]
    fn row_padding_is_dropped() {
        let (w, h) = (5, 4);
        let mut a = Assembler::default();
        a.add(&info(w, h, 3, 16, 0, 2), &band(w, 3, 16, 0, 2)).unwrap();
        a.add(&info(w, h, 3, 16, 2, 2), &band(w, 3, 16, 2, 2)).unwrap();
        check(&a.finish(false).unwrap(), w, h, 3);
    }

    /// A grey scan whose driver reports the row stride of a colour scan: the
    /// band's own size gives the right one.
    #[test]
    fn a_wrong_reported_stride_is_corrected_from_the_band_size() {
        let (w, h) = (6, 4);
        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w * 3, 0, 4), &band(w, 1, w, 0, 4)).unwrap();
        check(&a.finish(true).unwrap(), w, h, 1);
    }

    /// What the V19 II's driver does on macOS: rows packed at the image width,
    /// sent in fixed-size bands that claim one byte of padding per row, so
    /// rows run across band boundaries.
    #[test]
    fn grey_rows_run_on_across_bands() {
        let (w, h) = (7, 20);
        let stream = band(w, 1, w, 0, h);
        let claimed_stride = w + 1;
        let claimed_rows = 3;
        let mut a = Assembler::default();
        for (i, chunk) in stream.chunks(claimed_rows * claimed_stride).enumerate() {
            a.add(&info(w, h, 1, claimed_stride, i * claimed_rows, claimed_rows), chunk).unwrap();
        }
        check(&a.finish(true).unwrap(), w, h, 1);
    }

    /// The driver's last band is padded out past the end of the image.
    #[test]
    fn grey_stream_with_trailing_padding() {
        let (w, h) = (5, 4);
        let mut bytes = band(w, 1, w, 0, h);
        bytes.extend_from_slice(&[0xEE; 9]);
        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w + 1, 0, h), &bytes[..13]).unwrap();
        a.add(&info(w, h, 1, w + 1, 2, h), &bytes[13..]).unwrap();
        assert_eq!(a.rows_done(), h);
        check(&a.finish(true).unwrap(), w, h, 1);
    }

    #[test]
    fn a_fourth_byte_per_pixel_is_dropped() {
        let (w, h) = (4, 3);
        let mut a = Assembler::default();
        a.add(&info(w, h, 4, w * 4, 0, 3), &band(w, 4, w * 4, 0, 3)).unwrap();
        check(&a.finish(false).unwrap(), w, h, 3);
    }

    #[test]
    fn start_rows_are_compared_but_arrival_order_decides() {
        let (w, h) = (3, 4);
        let mut a = Assembler::default();
        a.add(&info(w, h, 3, w * 3, 0, 2), &band(w, 3, w * 3, 0, 2)).unwrap();
        a.add(&info(w, h, 3, w * 3, 0, 2), &band(w, 3, w * 3, 2, 2)).unwrap(); // driver always says row 0
        assert!(a.start_rows_disagree);
        check(&a.finish(false).unwrap(), w, h, 3);
    }

    #[test]
    fn a_scan_that_breaks_off_is_an_error_but_one_missing_row_is_not() {
        let (w, h) = (2, 400);
        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w, 0, 399), &band(w, 1, w, 0, 399)).unwrap();
        check(&a.finish(true).unwrap(), w, 399, 1);

        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w, 0, 300), &band(w, 1, w, 0, 300)).unwrap();
        assert!(a.finish(true).err().unwrap().contains("300 of 400"));

        assert!(Assembler::default().finish(true).is_err());
    }

    #[test]
    fn grey_and_colour_mismatches() {
        let (w, h) = (2, 2);
        // grey asked, colour delivered: converted
        let mut a = Assembler::default();
        a.add(&info(w, h, 3, w * 3, 0, 2), &[10, 10, 10, 200, 200, 200, 0, 0, 0, 255, 255, 255]).unwrap();
        let g = a.finish(true).unwrap();
        assert_eq!((g.channels, g.data.as_slice()), (1, &[10u8, 200, 0, 255][..]));
        // colour asked, grey delivered: refused
        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w, 0, 2), &[1, 2, 3, 4]).unwrap();
        assert!(a.finish(false).is_err());
    }

    #[test]
    fn sixteen_bit_data_is_refused() {
        let mut i = info(2, 2, 6, 12, 0, 2);
        i.bits_per_component = 16;
        assert!(Assembler::default().add(&i, &[0; 24]).is_err());
    }

    #[test]
    fn extra_rows_past_the_image_are_ignored() {
        let (w, h) = (3, 2);
        let mut a = Assembler::default();
        a.add(&info(w, h, 1, w, 0, 3), &band(w, 1, w, 0, 3)).unwrap();
        assert_eq!(a.rows_done(), 2);
        check(&a.finish(true).unwrap(), w, h, 1);
    }
}
