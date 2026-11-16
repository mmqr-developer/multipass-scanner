//! How fine the detail in a scan is, to say whether the chosen resolution
//! suits the picture.
//!
//! A scan is picture plus noise.  The noise is the same at every frequency (each
//! pixel has its own); the picture is strong at low frequencies and fades out
//! towards fine detail.  Where the spectrum of the scan has come down to the
//! level it keeps up to the finest frequency, there is only noise left: the
//! picture holds no detail finer than that, and a scan at a higher resolution
//! would only add pixels.  If the spectrum is still falling at the finest
//! frequency, the picture may hold more than this scan shows.
//!
//! The spectrum is taken in tiles, each compared with its own noise level
//! (noise is stronger in dark areas), and the tiles with the most detail decide.
//!
//! What a scan shows also depends on the scanner.  On the Epson Perfection
//! V19 II the noise per pixel is the same at every resolution (measured on
//! 15 November 2026: 1.9 levels of 255 in mid-tones at 1200, 2400 and 4800 dpi),
//! so a finer scan has less noise at every size of detail and shows finer
//! detail of the same print: one photo measured 750 dpi in its 1200 dpi scan,
//! 1150 dpi in its 2400 dpi scan and 1450 to 1650 dpi in its 4800 dpi scans.
//! The two limits below are set so that all of those scans give the same
//! advice, 2400 dpi.

use opencv::core::Mat;
use opencv::prelude::*;
use rayon::prelude::*;

use crate::post::Fft;
use crate::worker::RESOLUTIONS;

/// Tile size (a power of two) and half of it: the number of frequency steps
/// from zero to the finest a scan can hold.
const T: usize = 256;
const HALF: usize = T / 2;
/// About this many tiles are looked at, spread evenly over the scan.
const TILES: f64 = 1500.0;
/// The share of the tiles, those with the most detail, that decide.
const DETAIL_SHARE: f64 = 0.2;
/// The noise level is read from the finest quarter of the frequencies.
const FLOOR_FROM: usize = HALF * 3 / 4;
/// Detail counts while its power is at least this share of the noise's.
/// A quarter: combining four scans lowers the noise to that level.
const MIN_SIGNAL: f32 = 0.25;
/// The spectrum has levelled off if, within that finest quarter, the lower
/// half is no more than this much stronger than the upper half.
const LEVEL_RATIO: f32 = 1.08;

/// Detail that reaches this share of the scan's resolution is close to what
/// the scan can show above its noise: a finer scan would show more.
const NEAR_LIMIT: f64 = 0.55;
/// A resolution shows most of the detail found if the detail ends below this
/// share of it.
const COMFORTABLE: f64 = 0.7;

/// What the analysis found.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// the resolution that holds all the detail found, in dpi
    pub useful_dpi: f64,
    /// the detail goes on to the finest this scan can show
    pub reaches_limit: bool,
    /// how many tiles decided
    pub tiles: usize,
    /// power over the noise level at each frequency step (1 = only noise)
    pub spectrum: Vec<f32>,
}

/// Analyse `scan` (RGB, 8 bits per sample) taken at `dpi`.  `None`: too little
/// of the scan could be used (too small, or almost all of it clipped).
pub fn analyse(scan: &Mat, dpi: f64) -> Option<Detail> {
    let (h, w) = (scan.rows() as usize, scan.cols() as usize);
    if scan.channels() != 3 || h < T || w < T {
        return None;
    }
    let data = scan.data_bytes().ok()?;
    let step = T.max(((h * w) as f64 / TILES).sqrt() as usize);
    let origins: Vec<(usize, usize)> =
        (0..=h - T).step_by(step).flat_map(|y| (0..=w - T).step_by(step).map(move |x| (y, x))).collect();
    let fft = Fft::new(T);
    let win: Vec<f32> = (0..T).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / T as f64).cos()) as f32).collect();
    // which frequency step each bin of a tile's spectrum belongs to.  The two
    // axes are left out: a scanner's streaks and the picture's straight edges
    // lie there.
    let ring: Vec<usize> = (0..T * T)
        .map(|k| {
            let signed = |i: usize| if i < HALF { i as f64 } else { i as f64 - T as f64 };
            let (fy, fx) = (signed(k / T), signed(k % T));
            let r = fy.hypot(fx);
            if fy.abs() < 2.0 || fx.abs() < 2.0 || r >= HALF as f64 {
                usize::MAX
            } else {
                r as usize
            }
        })
        .collect();
    let mut count = vec![0f64; HALF];
    ring.iter().filter(|&&r| r != usize::MAX).for_each(|&r| count[r] += 1.0);

    // each usable tile: its spectrum over its own noise level
    let spectra: Vec<Vec<f32>> = origins
        .par_iter()
        .filter_map(|&(y0, x0)| {
            let (mut re, mut im) = (vec![0f32; T * T], vec![0f32; T * T]);
            let (mut sum, mut clipped) = (0f64, 0usize);
            for y in 0..T {
                let row = &data[((y0 + y) * w + x0) * 3..];
                for x in 0..T {
                    let v = row[x * 3 + 1]; // green
                    clipped += (v >= 250 || v <= 2) as usize;
                    re[y * T + x] = v as f32;
                    sum += v as f64;
                }
            }
            // a clipped area has lost its noise, so it has no noise level to compare with
            if clipped * 50 > T * T {
                return None;
            }
            let mean = (sum / (T * T) as f64) as f32;
            for y in 0..T {
                for x in 0..T {
                    re[y * T + x] = (re[y * T + x] - mean) * win[y] * win[x];
                }
            }
            fft.tile(&mut re, &mut im, false);
            let mut power = vec![0f64; HALF];
            for (k, &r) in ring.iter().enumerate() {
                if r != usize::MAX {
                    power[r] += (re[k] * re[k] + im[k] * im[k]) as f64;
                }
            }
            for (p, c) in power.iter_mut().zip(&count) {
                if *c > 0.0 {
                    *p /= c;
                }
            }
            let floor = power[FLOOR_FROM..HALF - 1].iter().sum::<f64>() / (HALF - 1 - FLOOR_FROM) as f64;
            (floor > 0.0).then(|| power.iter().map(|p| (p / floor) as f32).collect())
        })
        .collect();
    if spectra.len() < 8 {
        return None;
    }
    // the tiles with the most detail, judged in the middle of the range
    let score = |s: &Vec<f32>| s[HALF / 8..HALF * 3 / 8].iter().sum::<f32>();
    let mut order: Vec<usize> = (0..spectra.len()).collect();
    order.sort_by(|&a, &b| score(&spectra[b]).partial_cmp(&score(&spectra[a])).unwrap_or(std::cmp::Ordering::Equal));
    let tiles = ((spectra.len() as f64 * DETAIL_SHARE).ceil() as usize).max(8).min(spectra.len());
    let mut spectrum = vec![0f32; HALF];
    for &i in &order[..tiles] {
        spectrum.iter_mut().zip(&spectra[i]).for_each(|(a, v)| *a += v / tiles as f32);
    }
    Some(read_spectrum(spectrum, dpi, tiles))
}

/// Where the detail ends in `spectrum` (power over the noise level at each of
/// `HALF` frequency steps up to the finest a scan at `dpi` holds).
fn read_spectrum(spectrum: Vec<f32>, dpi: f64, tiles: usize) -> Detail {
    let mean = |a: usize, b: usize| spectrum[a..b].iter().sum::<f32>() / (b - a) as f32;
    // the first frequency at which the picture has faded into the noise
    // (the spectrum is smoothed over five steps first)
    let mut end = 3;
    while end < HALF - 3 && mean(end - 2, end + 3) - 1.0 >= MIN_SIGNAL {
        end += 1;
    }
    // Still falling where the noise level was read, or not faded before it:
    // that level is not the noise alone, and the picture goes on.
    let middle = (FLOOR_FROM + HALF - 1) / 2;
    let reaches_limit = end >= FLOOR_FROM || mean(FLOOR_FROM, middle) > LEVEL_RATIO * mean(middle, HALF - 1);
    let useful_dpi = if reaches_limit { dpi } else { end as f64 / HALF as f64 * dpi };
    Detail { useful_dpi, reaches_limit, tiles, spectrum }
}

/// The lowest of the app's resolutions that shows most of the detail up to
/// `useful_dpi`.
pub fn enough(useful_dpi: f64) -> i32 {
    RESOLUTIONS
        .iter()
        .copied()
        .find(|&r| useful_dpi <= COMFORTABLE * r as f64)
        .unwrap_or(RESOLUTIONS[RESOLUTIONS.len() - 1])
}

/// One sentence for the window: does a scan at `dpi` suit this picture?
pub fn advice(d: &Detail, dpi: i32) -> String {
    let found = (d.useful_dpi / 50.0).round() as i64 * 50;
    if d.reaches_limit || d.useful_dpi >= NEAR_LIMIT * dpi as f64 {
        let reach = if d.reaches_limit {
            format!("Detail: as fine as a {dpi} dpi scan can show.")
        } else {
            format!("Detail: reaches about {found} dpi, close to what a {dpi} dpi scan shows above its noise.")
        };
        return match RESOLUTIONS.iter().copied().find(|&r| r > dpi) {
            Some(next) => format!("{reach} {next} dpi would show more."),
            None => format!("{reach} This is the finest resolution."),
        };
    }
    let enough = enough(d.useful_dpi);
    if enough < dpi {
        let fewer = (dpi as f64 / enough as f64).powi(2);
        format!(
            "Detail: ends at about {found} dpi. {enough} dpi would show most of it, with {fewer:.0} times fewer pixels."
        )
    } else {
        format!("Detail: reaches about {found} dpi. {dpi} dpi suits this picture.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencv::core::{Scalar, Size, CV_32F, CV_8UC3};

    /// A scan of a picture whose detail fades out smoothly (random texture
    /// blurred by `blur` pixels, 60 times as strong as the noise at the
    /// coarsest sizes), plus noise of standard deviation 2 that differs from
    /// pixel to pixel.  `strength` 0 gives noise alone.
    fn scan(h: usize, w: usize, blur: f64, strength: f64) -> Mat {
        let mut rng = 0x2545f4914f6cdd1du64;
        let mut normal = move || {
            // sum of twelve uniform numbers: close to a normal distribution
            (0..12)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    (rng >> 11) as f64 / (1u64 << 53) as f64
                })
                .sum::<f64>()
                - 6.0
        };
        let mut texture = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_32F, Scalar::all(0.0)).unwrap();
        texture.data_typed_mut::<f32>().unwrap().iter_mut().for_each(|v| *v = normal() as f32);
        let mut soft = Mat::default();
        crate::cvx::gaussian_blur(&texture, &mut soft, Size::new(0, 0), blur, blur, opencv::core::BORDER_REFLECT_101).unwrap();
        let soft = soft.data_typed::<f32>().unwrap();
        let mut m = Mat::new_rows_cols_with_default(h as i32, w as i32, CV_8UC3, Scalar::all(0.0)).unwrap();
        for (k, px) in m.data_bytes_mut().unwrap().chunks_exact_mut(3).enumerate() {
            let v = (128.0 + strength * soft[k] as f64 + 2.0 * normal()).round().clamp(0.0, 255.0) as u8;
            px.copy_from_slice(&[v, v, v]);
        }
        m
    }

    // With strength 60 and noise 2, the picture's power is 900 times the
    // noise's at the coarsest sizes and falls to a quarter of it at
    // 0.4555 / blur cycles per pixel.

    #[test]
    fn detail_coarser_than_the_scan_is_measured() {
        // blurred by 4 pixels in a 2400 dpi scan: detail ends at 547 dpi
        let d = analyse(&scan(1100, 1300, 4.0, 60.0), 2400.0).unwrap();
        assert!(!d.reaches_limit, "{:?}", &d.spectrum[90..]);
        assert!((480.0..=600.0).contains(&d.useful_dpi), "useful {}", d.useful_dpi);
        // 600 dpi would be close to its limit with that; 1200 dpi shows it comfortably
        assert_eq!(enough(d.useful_dpi), 1200);
        let text = advice(&d, 2400);
        assert!(text.ends_with("1200 dpi would show most of it, with 4 times fewer pixels."), "{text}");
        assert!(advice(&d, 4800).ends_with("1200 dpi would show most of it, with 16 times fewer pixels."));
    }

    #[test]
    fn a_resolution_that_fits_is_said_to_suit() {
        // blurred by 2 pixels in a 1200 dpi scan: detail ends at 547 dpi, too fine for 600 dpi
        let d = analyse(&scan(1100, 1300, 2.0, 60.0), 1200.0).unwrap();
        assert!(!d.reaches_limit, "{:?}", &d.spectrum[90..]);
        assert!((480.0..=600.0).contains(&d.useful_dpi), "useful {}", d.useful_dpi);
        assert!(advice(&d, 1200).ends_with("1200 dpi suits this picture."), "{}", advice(&d, 1200));
    }

    #[test]
    fn detail_close_to_what_the_scan_shows_asks_for_more() {
        // blurred by 1.6 pixels in a 1200 dpi scan: detail ends at 683 dpi, over half the resolution
        let d = analyse(&scan(1100, 1300, 1.6, 60.0), 1200.0).unwrap();
        assert!(!d.reaches_limit, "{:?}", &d.spectrum[90..]);
        assert!((620.0..=760.0).contains(&d.useful_dpi), "useful {}", d.useful_dpi);
        let text = advice(&d, 1200);
        assert!(text.contains("close to what a 1200 dpi scan shows") && text.ends_with("2400 dpi would show more."), "{text}");
    }

    #[test]
    fn detail_as_fine_as_the_scan_asks_for_more() {
        // blurred by 0.8 pixels: the picture is still well above the noise at the finest frequencies
        let d = analyse(&scan(1100, 1300, 0.8, 60.0), 1200.0).unwrap();
        assert!(d.reaches_limit, "{:?}", &d.spectrum[90..]);
        assert_eq!(advice(&d, 1200), "Detail: as fine as a 1200 dpi scan can show. 2400 dpi would show more.");
        assert_eq!(advice(&d, 4800), "Detail: as fine as a 4800 dpi scan can show. This is the finest resolution.");
    }

    #[test]
    fn noise_alone_is_not_detail() {
        let d = analyse(&scan(900, 900, 1.0, 0.0), 4800.0).unwrap();
        assert!(!d.reaches_limit && d.useful_dpi < 300.0, "{d:?}");
        assert_eq!(enough(d.useful_dpi), 600);
    }

    #[test]
    fn the_test_photo_gets_the_same_advice_at_every_resolution() {
        // what its scans measured on 15 November 2026 (see the top of this file)
        let found = |useful_dpi: f64| Detail { useful_dpi, reaches_limit: false, tiles: 100, spectrum: Vec::new() };
        assert!(advice(&found(750.0), 1200).ends_with("2400 dpi would show more."));
        assert!(advice(&found(1150.0), 2400).ends_with("2400 dpi suits this picture."));
        assert!(advice(&found(1450.0), 4800).ends_with("2400 dpi would show most of it, with 4 times fewer pixels."));
        assert!(advice(&found(1650.0), 4800).ends_with("2400 dpi would show most of it, with 4 times fewer pixels."));
    }

    #[test]
    fn a_scan_that_cannot_be_judged_gives_nothing() {
        // too small
        assert!(analyse(&scan(200, 900, 1.0, 0.0), 1200.0).is_none());
        // all white: clipped, no noise to compare with
        let white = Mat::new_rows_cols_with_default(900, 900, CV_8UC3, Scalar::all(255.0)).unwrap();
        assert!(analyse(&white, 1200.0).is_none());
    }
}
