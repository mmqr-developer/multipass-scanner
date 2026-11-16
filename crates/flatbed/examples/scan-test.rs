//! A first test of a scanner backend, without OpenCV or the GUI:
//!
//!     cargo run --release -p flatbed --example scan-test -- OUT_DIR
//!
//! Put a photo face down against the top edge of the glass, off to one side,
//! and close the lid.  The program opens the scanner, takes five small scans
//! (about a minute) and checks what it can by itself.  It writes BMP files to
//! OUT_DIR for the checks that need eyes.
//!
//! On Windows, with more than one scanner (a network printer counts), name
//! the one to use: set MULTIPASS_SCANNER to part of its name.

use std::path::Path;
use std::time::Instant;

use flatbed::{AreaMm, RawImage, Scanner};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "scan-test".into());
    let result = flatbed::run_with_main_loop(move || run(&dir));
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
    let t = Instant::now();
    let mut sc = flatbed::open_first()?;
    let (gw, gh) = sc.glass_mm();
    println!("scanner: {}", sc.name());
    println!("glass:   {gw:.1} x {gh:.1} mm (opened in {:.1} s)", t.elapsed().as_secs_f64());
    for note in sc.notes() {
        println!("  {note}");
    }
    let mut failed = 0;
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };

    // 1. what the app's first step does: the top inch in grey at 75 dpi
    let strip = scan(sc.as_mut(), "1. top inch, grey, 75 dpi", 75, true, AreaMm { x: 0.0, y: 0.0, w: gw, h: 25.4 })?;
    check(strip.channels == 1, format!("grey: {} channel(s)", strip.channels));
    check(
        near(strip.width, gw / 25.4 * 75.0, 2) && near(strip.height, 75.0, 2),
        format!("size {} x {} px, expected about {:.0} x 75", strip.width, strip.height, gw / 25.4 * 75.0),
    );
    check_shear(&mut check, &strip);
    save(dir, "1_top_strip.bmp", &strip)?;

    // 2. the whole glass in grey at 75 dpi
    let whole = scan(sc.as_mut(), "2. whole glass, grey, 75 dpi", 75, true, AreaMm { x: 0.0, y: 0.0, w: gw, h: gh })?;
    check(
        near(whole.width, gw / 25.4 * 75.0, 2) && near(whole.height, gh / 25.4 * 75.0, 2),
        format!(
            "size {} x {} px, expected about {:.0} x {:.0}",
            whole.width,
            whole.height,
            gw / 25.4 * 75.0,
            gh / 25.4 * 75.0
        ),
    );
    check_shear(&mut check, &whole);
    save(dir, "2_whole_glass.bmp", &whole)?;

    // Where is the origin of the scan area?  The strip from step 1 must be the
    // top of the whole-glass scan, not its bottom and not mirrored.
    if strip.width == whole.width && strip.height <= whole.height {
        let rows = strip.height;
        let top = difference(&strip, &whole, 0, false);
        let bottom = difference(&strip, &whole, whole.height - rows, false);
        let mirrored = difference(&strip, &whole, 0, true);
        println!("  strip against the whole glass: top {top:.1}, bottom {bottom:.1}, top mirrored {mirrored:.1} (mean difference in levels)");
        if contrast(&strip) < 8.0 {
            println!("  ??   the top of the glass looks empty: put a photo against the top edge, off to one side, to test the origin");
        } else {
            check(top < 6.0 && top * 2.0 < bottom.min(mirrored), "the scan area is measured from the corner where the scan starts".into());
            if bottom < top {
                println!("       y seems to be measured from the far end of the glass");
                #[cfg(target_os = "macos")]
                println!("       try MULTIPASS_ICA_Y_FROM_FAR_END=1");
            }
        }
    }

    // 3. and 4. what the app's last step does, on a small patch: colour, twice
    let patch = AreaMm { x: 20.0, y: 5.0, w: 50.0, h: 40.0 };
    let a = scan(sc.as_mut(), "3. 50 x 40 mm patch, colour, 300 dpi", 300, false, patch)?;
    check(a.channels == 3, format!("colour: {} channel(s)", a.channels));
    check(
        near(a.width, 50.0 / 25.4 * 300.0, 2) && near(a.height, 40.0 / 25.4 * 300.0, 2),
        format!("size {} x {} px, expected about 591 x 472", a.width, a.height),
    );
    check_shear(&mut check, &a);
    save(dir, "3_patch.bmp", &a)?;
    let b = scan(sc.as_mut(), "4. the same patch again", 300, false, patch)?;
    if (a.width, a.height, a.channels) == (b.width, b.height, b.channels) {
        let d = difference(&a, &b, 0, false);
        check(d < 4.0, format!("two scans of the same area differ by {d:.2} levels on average (noise only)"));
    } else {
        check(false, format!("the second scan has another size: {} x {}", b.width, b.height));
    }

    // 5. a resolution the app needs
    println!("5. 10 x 10 mm, colour, 1200 dpi");
    let t5 = Instant::now();
    match sc.scan(1200, false, AreaMm { x: 20.0, y: 5.0, w: 10.0, h: 10.0 }) {
        Ok(img) => {
            println!("  {} x {} px in {:.1} s", img.width, img.height, t5.elapsed().as_secs_f64());
            check(near(img.width, 472.0, 2) && near(img.height, 472.0, 2), "1200 dpi works".into());
            check_shear(&mut check, &img);
            save(dir, "5_1200dpi.bmp", &img)?;
        }
        Err(e) => check(false, format!("1200 dpi: {e}")),
    }

    println!();
    println!("Now look at the pictures in {dir}:");
    println!("  2_whole_glass.bmp  the photo is at the TOP of the picture and not mirrored;");
    println!("                     the lid around it is plain white, without a frame or crop");
    println!("  3_patch.bmp        colours look right; no sharpening halos along edges");
    if failed > 0 {
        return Err(format!("{failed} check(s) failed"));
    }
    println!("All automatic checks passed.");
    Ok(())
}

fn scan(sc: &mut dyn Scanner, what: &str, dpi: i32, gray: bool, area: AreaMm) -> Result<RawImage, String> {
    println!("{what}");
    let t = Instant::now();
    let img = sc.scan(dpi, gray, area).map_err(|e| format!("{what}: {e}"))?;
    let mean = img.data.iter().map(|&v| v as f64).sum::<f64>() / img.data.len().max(1) as f64;
    println!("  {} x {} px in {:.1} s, mean level {mean:.0}", img.width, img.height, t.elapsed().as_secs_f64());
    Ok(img)
}

fn near(got: usize, expected: f64, slack: usize) -> bool {
    (got as f64 - expected).abs() <= slack as f64
}

/// Rows put together with the wrong length shift a little against each other,
/// so the image is sheared.  Each row must match the next best unshifted.
fn check_shear(check: &mut impl FnMut(bool, String), img: &RawImage) {
    if contrast(img) < 8.0 {
        return; // nothing to see on a blank lid
    }
    let d: Vec<f64> = (-2..=2).map(|s| row_shift_difference(img, s)).collect();
    let best = (0..5).min_by(|&i, &j| d[i].total_cmp(&d[j])).unwrap() as i32 - 2;
    check(
        best == 0,
        if best == 0 {
            "rows line up (not sheared)".into()
        } else {
            format!("rows are sheared: each row matches the next shifted by {best} px (row length wrong)")
        },
    );
}

/// Mean difference between each row and the next, the next shifted by `s` pixels.
fn row_shift_difference(img: &RawImage, s: i32) -> f64 {
    let (w, ch) = (img.width, img.channels);
    let m = 4usize;
    if w <= 2 * m || img.height < 2 {
        return 0.0;
    }
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in 0..img.height - 1 {
        for x in m..w - m {
            let xb = (x as i32 + s) as usize;
            for c in 0..ch {
                let a = img.data[(y * w + x) * ch + c] as i32;
                let b = img.data[((y + 1) * w + xb) * ch + c] as i32;
                sum += (a - b).unsigned_abs() as u64;
                n += 1;
            }
        }
    }
    sum as f64 / n as f64
}

/// Spread of the levels in an image: small when it shows nothing but the lid.
fn contrast(img: &RawImage) -> f64 {
    let n = img.data.len().max(1) as f64;
    let mean = img.data.iter().map(|&v| v as f64).sum::<f64>() / n;
    (img.data.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n).sqrt()
}

/// Mean absolute difference between `a` and the rows of `b` from `b_row` on
/// (same width and channels), with `b` mirrored left to right if asked.
fn difference(a: &RawImage, b: &RawImage, b_row: usize, mirror: bool) -> f64 {
    let (w, ch) = (a.width, a.channels);
    let mut sum = 0u64;
    for y in 0..a.height {
        for x in 0..w {
            let bx = if mirror { w - 1 - x } else { x };
            for c in 0..ch {
                let va = a.data[(y * w + x) * ch + c] as i32;
                let vb = b.data[((y + b_row) * w + bx) * ch + c] as i32;
                sum += (va - vb).unsigned_abs() as u64;
            }
        }
    }
    sum as f64 / (a.height * w * ch).max(1) as f64
}

/// 24-bit BMP: no dependency, and every system can show it.
fn save(dir: &str, name: &str, img: &RawImage) -> Result<(), String> {
    let (w, h) = (img.width, img.height);
    let row = (w * 3 + 3) / 4 * 4;
    let mut out = Vec::with_capacity(54 + row * h);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: rows bottom to top
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..h).rev() {
        for x in 0..w {
            let p = (y * w + x) * img.channels;
            let (r, g, b) = if img.channels == 3 {
                (img.data[p], img.data[p + 1], img.data[p + 2])
            } else {
                (img.data[p], img.data[p], img.data[p])
            };
            out.extend_from_slice(&[b, g, r]);
        }
        out.resize(out.len() + row - w * 3, 0);
    }
    let path = Path::new(dir).join(name);
    std::fs::write(&path, out).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("  saved {}", path.display());
    Ok(())
}
