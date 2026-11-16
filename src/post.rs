//! Optional clean-up of the combined image, after the scans are combined.
//!
//! * **Texture patterns.**  Prints on "silk" or "honeycomb" paper carry a
//!   regular surface pattern.  A regular pattern is a handful of sharp peaks in
//!   the image's spectrum, at the same frequencies everywhere on the print,
//!   while the picture itself is different in every part of it.  The peaks are
//!   found in the spectrum averaged over the whole image and then lowered, tile
//!   by tile, to the level of the spectrum beside them.
//! * **Lint.**  Dust and fibres lying on the print are sharper and thinner than
//!   anything in the picture, because the print itself is not that sharp.
//!   Small, thin, hard-edged specks that stand out from their surroundings are
//!   filled in from the pixels around them.
//!
//! The sizes in this file are lengths on the print.  They are written in pixels
//! at 1200 dpi, the resolution they were worked out at, and scaled to the
//! resolution of the image ([`Scale`]).

use opencv::core::{self, Mat, Point, Scalar, Size, Vec3w, CV_32F, CV_8U};
use opencv::imgproc;

use crate::cvx;
use opencv::prelude::*;
use rayon::prelude::*;

type Res<T> = Result<T, String>;

fn cv<T>(r: opencv::Result<T>) -> Res<T> {
    r.map_err(|e| format!("OpenCV: {e}"))
}

/// What to do to the combined image before it is saved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub texture: bool,
    pub lint: bool,
}

impl Options {
    pub fn any(&self) -> bool {
        self.texture || self.lint
    }
}

/// Roughly the memory (bytes) post-processing an image of `px` pixels takes,
/// on top of the image itself: a copy to work on and whole-image buffers.
pub fn memory_needed(px: u64) -> u64 {
    px * POST_BYTES_PER_PIXEL
}

/// Measured: the lint search holds about this much per pixel at its peak.
const POST_BYTES_PER_PIXEL: u64 = 44;

/// Pixels at the image's resolution for a length given in pixels at 1200 dpi.
#[derive(Clone, Copy)]
struct Scale {
    dpi: f64,
    k: f64,
}

impl Scale {
    fn new(dpi: f64) -> Scale {
        Scale { dpi, k: dpi / 1200.0 }
    }

    fn len(&self, px: f64) -> f64 {
        px * self.k
    }

    /// An odd filter size: the distance from its middle to its edge is scaled.
    fn odd(&self, px: i32) -> i32 {
        2 * ((((px - 1) / 2) as f64 * self.k).round() as i32).max(1) + 1
    }

    /// Tile size for the spectrum, a power of two: 256 at 1200 dpi, so that a
    /// tile is the same piece of the print at every resolution.
    fn tile(&self) -> usize {
        let exact = 256.0 * self.k;
        let bits = exact.log2().round().clamp(6.0, 11.0) as u32;
        1usize << bits
    }
}

/// Apply the chosen steps to `img` (RGB, 16 bits per sample, scanned at
/// `dpi`), in place.  `progress` gets a fraction of this work (0..1) and a text.
pub fn apply(img: &mut Mat, opts: Options, dpi: f64, progress: &dyn Fn(f32, &str), report: &mut Vec<String>) -> Res<()> {
    if img.typ() != core::CV_16UC3 || !img.is_continuous() {
        return Err("post-processing expects a 16-bit RGB image".into());
    }
    let scale = Scale::new(dpi);
    // texture first: without the pattern, lint stands out more clearly
    if opts.texture {
        progress(0.0, "Removing texture patterns");
        remove_texture(img, scale, report)?;
    }
    if opts.lint {
        progress(if opts.texture { 0.6 } else { 0.0 }, "Removing lint");
        remove_lint(img, scale, report)?;
    }
    Ok(())
}

// ------------------------------------------------------------------ FFT

/// Radix-2 FFT of length `t` (a power of two, the tile size), used along rows
/// and columns of a tile.  Tiles overlap by half.
pub(crate) struct Fft {
    t: usize,
    rev: Vec<usize>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    pub(crate) fn new(t: usize) -> Fft {
        let bits = t.trailing_zeros();
        let rev = (0..t).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        let (cos, sin) = (0..t / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / t as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        Fft { t, rev, cos, sin }
    }

    /// In place, on `t` values spaced `stride` apart starting at `off`.
    fn line(&self, re: &mut [f32], im: &mut [f32], off: usize, stride: usize, inverse: bool) {
        let t = self.t;
        for i in 0..t {
            let j = self.rev[i];
            if j > i {
                re.swap(off + i * stride, off + j * stride);
                im.swap(off + i * stride, off + j * stride);
            }
        }
        let mut len = 2;
        while len <= t {
            let step = t / len;
            for start in (0..t).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = (self.cos[k * step], if inverse { -self.sin[k * step] } else { self.sin[k * step] });
                    let a = off + (start + k) * stride;
                    let b = off + (start + k + len / 2) * stride;
                    let (tr, ti) = (re[b] * c - im[b] * s, re[b] * s + im[b] * c);
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len *= 2;
        }
    }

    /// Two-dimensional transform of a `t` x `t` tile (the inverse is scaled).
    pub(crate) fn tile(&self, re: &mut [f32], im: &mut [f32], inverse: bool) {
        let t = self.t;
        for y in 0..t {
            self.line(re, im, y * t, 1, inverse);
        }
        // A column is spread over the whole tile, one value per row.  It is
        // copied out, transformed and copied back, so that the many passes of
        // the transform run over neighbouring memory (the same arithmetic,
        // several times faster on the large tiles of the high resolutions).
        let (mut cre, mut cim) = (vec![0f32; t], vec![0f32; t]);
        for x in 0..t {
            for y in 0..t {
                cre[y] = re[y * t + x];
                cim[y] = im[y * t + x];
            }
            self.line(&mut cre, &mut cim, 0, 1, inverse);
            for y in 0..t {
                re[y * t + x] = cre[y];
                im[y * t + x] = cim[y];
            }
        }
        if inverse {
            let k = 1.0 / (t * t) as f32;
            re.iter_mut().for_each(|v| *v *= k);
            im.iter_mut().for_each(|v| *v *= k);
        }
    }
}

/// Mirror an index into 0..n (the edge pixel is not repeated).
fn reflect(i: isize, n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let period = 2 * (n as isize - 1);
    let mut k = i.rem_euclid(period);
    if k >= n as isize {
        k = period - k;
    }
    k as usize
}

// ------------------------------------------------------------------ texture

/// The pattern's place in the spectrum.
struct Pattern {
    /// (bin to lower, bins beside it that give the level to lower it to); the
    /// bins are indices into an unshifted `t` x `t` spectrum
    bins: Vec<(usize, Vec<usize>)>,
    peaks: usize,
    /// period of the strongest peak, in pixels
    period: f64,
}

/// The image laid out for overlapping tiles: it starts `hop` in from the top
/// left of a padded frame, so that every pixel is covered by four tiles.
struct Frame<'a> {
    px: &'a [Vec3w],
    h: usize,
    w: usize,
    /// padded size
    ph: usize,
    pw: usize,
    /// tile size
    t: usize,
}

impl<'a> Frame<'a> {
    fn new(px: &'a [Vec3w], h: usize, w: usize, t: usize) -> Frame<'a> {
        Frame { px, h, w, ph: (h + t).div_ceil(t) * t, pw: (w + t).div_ceil(t) * t, t }
    }

    /// Row and column lookups for the tile at (ty, tx) in the padded frame.
    fn index(&self, t0: usize, n: usize) -> Vec<usize> {
        let (t, hop) = (self.t, self.t / 2);
        (0..t).map(|i| reflect(t0 as isize + i as isize - hop as isize, n)).collect()
    }
}

/// sine window: applied before and after the transform, the overlapping tiles
/// add up to exactly one
fn window(t: usize) -> Vec<f32> {
    (0..t).map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / t as f64).sin() as f32).collect()
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let k = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    *m
}

/// Find the frequencies of a regular pattern: sharp, isolated peaks in the
/// spectrum averaged over all tiles.  `None`: there is no such pattern.
fn find_pattern(f: &Frame, fft: &Fft, scale: Scale) -> Res<Option<Pattern>> {
    let (t, hop) = (f.t, f.t / 2);
    let win = window(t);
    let xs: Vec<usize> = (0..=f.pw - t).step_by(hop).collect();
    let ys: Vec<usize> = (0..=f.ph - t).step_by(hop).collect();
    // mean log magnitude of the grey image's spectrum: the log keeps one tile
    // with strong picture detail from outweighing the rest
    let sum = ys
        .par_iter()
        .map(|&ty| {
            let mut acc = vec![0f64; t * t];
            let (mut re, mut im) = (vec![0f32; t * t], vec![0f32; t * t]);
            let rows = f.index(ty, f.h);
            for &tx in &xs {
                let cols = f.index(tx, f.w);
                let mut mean = 0f64;
                for y in 0..t {
                    for x in 0..t {
                        let p = f.px[rows[y] * f.w + cols[x]].0;
                        let g = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 65535.0;
                        re[y * t + x] = g;
                        mean += g as f64;
                    }
                }
                let mean = (mean / (t * t) as f64) as f32;
                for y in 0..t {
                    for x in 0..t {
                        let k = y * t + x;
                        let h = win[y] * win[x];
                        re[k] = (re[k] - mean) * h * h; // Hann window
                        im[k] = 0.0;
                    }
                }
                fft.tile(&mut re, &mut im, false);
                for k in 0..t * t {
                    acc[k] += ((re[k] * re[k] + im[k] * im[k]).sqrt() as f64 + 1e-6).ln();
                }
            }
            acc
        })
        .reduce(|| vec![0f64; t * t], |mut a, b| {
            a.iter_mut().zip(&b).for_each(|(x, y)| *x += y);
            a
        });
    let n = (xs.len() * ys.len()) as f64;
    // shifted, so that zero frequency is in the middle
    let c = t / 2;
    let mut shifted = vec![0f32; t * t];
    for y in 0..t {
        for x in 0..t {
            shifted[y * t + x] = (sum[((y + c) % t) * t + (x + c) % t] / n) as f32;
        }
    }
    // the smooth part of the spectrum, and how far each bin stands above it
    let level = {
        let l = cv(Mat::new_rows_cols_with_data(t as i32, t as i32, &shifted))?;
        let (mut a, mut b, mut out) = (Mat::default(), Mat::default(), Mat::default());
        cv(imgproc::median_blur(&l, &mut a, 5))?;
        cv(imgproc::median_blur(&a, &mut b, 5))?;
        cv(cvx::gaussian_blur(&b, &mut out, Size::new(0, 0), 3.0, 3.0, core::BORDER_DEFAULT))?;
        cv(out.data_typed::<f32>())?.to_vec()
    };
    let excess: Vec<f32> = shifted.iter().zip(&level).map(|(a, b)| a - b).collect();
    Ok(pattern_from_excess(&excess, t, scale))
}

/// Pick the pattern's bins out of `excess` (log of the average spectrum over
/// its smooth level; `t` x `t`, zero frequency at the centre).
fn pattern_from_excess(excess: &[f32], t: usize, scale: Scale) -> Option<Pattern> {
    let c = (t / 2) as isize;
    let radius = |y: usize, x: usize| ((y as isize - c) as f64).hypot((x as isize - c) as f64);
    // periods from about 0.05 mm (2 px at 1200 dpi; never less than 2 px) to
    // 1 mm; slower variations are the picture's own
    let (r_min, r_max) = (t as f64 / scale.len(48.0), t as f64 / scale.len(2.2).max(2.2));
    let in_band = |y: usize, x: usize| (r_min..=r_max).contains(&radius(y, x));
    let at = |y: isize, x: isize| excess[(y.rem_euclid(t as isize) as usize) * t + x.rem_euclid(t as isize) as usize];

    // scatter of the spectrum where there is no pattern
    let mut band: Vec<f32> = (0..t * t).filter(|&k| in_band(k / t, k % t)).map(|k| excess[k]).collect();
    let mid = median(&mut band);
    let mut dev: Vec<f32> = band.iter().map(|v| (v - mid).abs()).collect();
    let noise = 1.4826 * median(&mut dev);
    // at least 1.57 times the level beside it
    let thr = 0.45f32.max(6.0 * noise);

    // A peak is a local maximum that stands well above everything on a ring
    // around it.  That leaves out ridges, which are the picture's straight
    // edges and the scanner's streaks, not a pattern.
    let ring: Vec<(isize, isize)> = (-5isize..=5)
        .flat_map(|dy| (-5isize..=5).map(move |dx| (dy, dx)))
        .filter(|&(dy, dx)| (3.5..=4.6).contains(&(dy as f64).hypot(dx as f64)))
        .collect();
    let mut peaks: Vec<(usize, usize, f32)> = Vec::new();
    for y in 0..t {
        for x in 0..t {
            let e = excess[y * t + x];
            if e <= thr || !in_band(y, x) {
                continue;
            }
            let (yi, xi) = (y as isize, x as isize);
            let local_max = (-1..=1).all(|dy| (-1..=1).all(|dx| at(yi + dy, xi + dx) <= e));
            let around = ring.iter().map(|&(dy, dx)| at(yi + dy, xi + dx)).fold(0f32, f32::max);
            if local_max && e - around > 0.5 * e {
                peaks.push((y, x, e));
            }
        }
    }
    if peaks.len() < 2 {
        return None;
    }
    // the bins of each peak: those right beside it, and those a little further
    // out that still stand out
    let mut mask = vec![false; t * t];
    for &(y, x, _) in &peaks {
        for dy in -2isize..=2 {
            for dx in -2isize..=2 {
                let d = (dy as f64).hypot(dx as f64);
                let (yy, xx) = (y as isize + dy, x as isize + dx);
                if yy < 0 || xx < 0 || yy >= t as isize || xx >= t as isize {
                    continue;
                }
                if d <= 1.5 || (d <= 2.0 && at(yy, xx) > thr) {
                    mask[yy as usize * t + xx as usize] = true;
                }
            }
        }
    }
    // a real image's spectrum is symmetric about zero frequency
    for y in 1..t {
        for x in 1..t {
            if mask[y * t + x] {
                mask[(t - y) * t + (t - x)] = true;
            }
        }
    }
    for y in 0..t {
        for x in 0..t {
            if !in_band(y, x) {
                mask[y * t + x] = false;
            }
        }
    }
    let count = mask.iter().filter(|&&m| m).count();
    // A pattern is a few peaks.  More than this is not one this filter should touch.
    // (a twentieth of the spectrum of a tile at 1200 dpi: the band that is
    // searched has the same number of bins at every resolution)
    let tile_1200 = t as f64 / scale.k;
    if count == 0 || count > (tile_1200 * tile_1200 / 20.0) as usize {
        return None;
    }
    let unshift = |y: usize, x: usize| ((y + t - t / 2) % t) * t + (x + t - t / 2) % t;
    let mut bins = Vec::with_capacity(count);
    for y in 0..t {
        for x in 0..t {
            if !mask[y * t + x] {
                continue;
            }
            // The level to lower this bin to comes from the bins on the line
            // through zero frequency, 3 to 6 bins either side: a straight edge
            // in the picture puts its energy along such a line, and must keep it.
            let r = radius(y, x);
            let (uy, ux) = ((y as isize - c) as f64 / r, (x as isize - c) as f64 / r);
            let mut beside = Vec::with_capacity(8);
            for s in [-6.0, -5.0, -4.0, -3.0, 3.0, 4.0, 5.0, 6.0] {
                let (sy, sx) = ((y as f64 + s * uy).round() as isize, (x as f64 + s * ux).round() as isize);
                if sy < 0 || sx < 0 || sy >= t as isize || sx >= t as isize {
                    continue;
                }
                let (sy, sx) = (sy as usize, sx as usize);
                if !mask[sy * t + sx] && radius(sy, sx) >= 2.0 {
                    beside.push(unshift(sy, sx));
                }
            }
            if beside.len() < 3 {
                // crowded: any free bin nearby will do
                beside.clear();
                for dy in -4isize..=4 {
                    for dx in -4isize..=4 {
                        let (sy, sx) = (y as isize + dy, x as isize + dx);
                        if sy >= 0 && sx >= 0 && sy < t as isize && sx < t as isize && !mask[sy as usize * t + sx as usize] {
                            beside.push(unshift(sy as usize, sx as usize));
                        }
                    }
                }
            }
            if !beside.is_empty() {
                bins.push((unshift(y, x), beside));
            }
        }
    }
    let &(py, px, _) = peaks.iter().max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))?;
    Some(Pattern { bins, peaks: peaks.len(), period: t as f64 / radius(py, px) })
}

/// Lower the pattern's bins in every tile and put the tiles back together.
fn remove_pattern(img: &mut Mat, pattern: &Pattern, fft: &Fft) -> Res<f64> {
    let (h, w) = (img.rows() as usize, img.cols() as usize);
    let (t, hop) = (fft.t, fft.t / 2);
    let win = window(t);
    let acc = {
        let f = Frame::new(cv(img.data_typed::<Vec3w>())?, h, w, t);
        let row_len = f.pw * 3;
        let mut acc = vec![0f32; f.ph * row_len];
        let xs: Vec<usize> = (0..=f.pw - t).step_by(hop).collect();
        // Tile rows overlap by half.  Every second row of tiles is disjoint, so
        // those can be written at the same time: first the even rows, then the odd.
        for phase in 0..2 {
            let start = phase * hop;
            acc[start * row_len..].par_chunks_mut(t * row_len).enumerate().for_each(|(k, chunk)| {
                if chunk.len() < t * row_len {
                    return;
                }
                let rows = f.index(start + k * t, f.h);
                let (mut re, mut im) = (vec![0f32; t * t], vec![0f32; t * t]);
                let mut beside = [0f32; 81];
                for &tx in &xs {
                    let cols = f.index(tx, f.w);
                    for ch in 0..3 {
                        let mut mean = 0f64;
                        for y in 0..t {
                            for x in 0..t {
                                let v = f.px[rows[y] * f.w + cols[x]].0[ch] as f32 / 65535.0;
                                re[y * t + x] = v;
                                mean += v as f64;
                            }
                        }
                        let mean = (mean / (t * t) as f64) as f32;
                        for y in 0..t {
                            for x in 0..t {
                                let i = y * t + x;
                                re[i] = (re[i] - mean) * win[y] * win[x];
                                im[i] = 0.0;
                            }
                        }
                        fft.tile(&mut re, &mut im, false);
                        for (bin, around) in &pattern.bins {
                            let n = around.len().min(beside.len());
                            for (b, &s) in beside.iter_mut().zip(around.iter()) {
                                *b = (re[s] * re[s] + im[s] * im[s]).sqrt();
                            }
                            let level = median(&mut beside[..n]);
                            let a = (re[*bin] * re[*bin] + im[*bin] * im[*bin]).sqrt();
                            if a > level && a > 0.0 {
                                let g = level / a;
                                re[*bin] *= g;
                                im[*bin] *= g;
                            }
                        }
                        fft.tile(&mut re, &mut im, true);
                        for y in 0..t {
                            let line = &mut chunk[y * row_len..(y + 1) * row_len];
                            for x in 0..t {
                                let wxy = win[y] * win[x];
                                line[(tx + x) * 3 + ch] += (re[y * t + x] + mean * wxy) * wxy;
                            }
                        }
                    }
                }
            });
        }
        (acc, f.pw)
    };
    let (acc, pw) = acc;
    let px = cv(img.data_typed_mut::<Vec3w>())?;
    let sq: f64 = px
        .par_chunks_mut(w)
        .enumerate()
        .map(|(y, line)| {
            let src = &acc[(y + hop) * pw * 3..];
            let mut sq = 0f64;
            for (x, p) in line.iter_mut().enumerate() {
                for ch in 0..3 {
                    let v = (src[(x + hop) * 3 + ch] * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16;
                    let d = v as f64 - p.0[ch] as f64;
                    sq += d * d;
                    p.0[ch] = v;
                }
            }
            sq
        })
        .sum();
    // rms change in 8-bit levels
    Ok((sq / (h * w * 3) as f64).sqrt() / 257.0)
}

fn remove_texture(img: &mut Mat, scale: Scale, report: &mut Vec<String>) -> Res<()> {
    let (h, w) = (img.rows() as usize, img.cols() as usize);
    let t = scale.tile();
    if h < t || w < t {
        report.push("texture patterns: the image is too small to look for one".into());
        return Ok(());
    }
    let fft = Fft::new(t);
    let pattern = find_pattern(&Frame::new(cv(img.data_typed::<Vec3w>())?, h, w, t), &fft, scale)?;
    match pattern {
        None => report.push("texture patterns: no regular pattern found, image left as it is".into()),
        Some(p) => {
            let rms = remove_pattern(img, &p, &fft)?;
            report.push(format!(
                "texture patterns: removed a pattern with a period of {:.1} px ({:.2} mm), {} peaks in the spectrum; \
                 average change {:.2} levels of 255",
                p.period,
                p.period * 25.4 / scale.dpi,
                p.peaks,
                rms
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ lint

// The lengths below are in pixels at 1200 dpi (see `Scale`).

/// Lint is thinner than about half of this (the size of the median filter that
/// gives the picture without it).
const LINT_MEDIAN: i32 = 25;
/// Widest speck or fibre that counts as lint (0.15 mm).  Details of the
/// picture, such as the highlight in an eye, are wider and softer.
const LINT_MAX_THICKNESS: f32 = 7.0;
/// Largest area of one piece, in square pixels, and the longest it may be.
const LINT_MAX_AREA: i32 = 800;
const LINT_MAX_LENGTH: i32 = 400;
/// Lint has a hard edge: the steepest step at its edge, per pixel, is at least
/// this share of how far it stands out.  The edge is as wide on the print at
/// every resolution, so the share per pixel goes down as the pixels get
/// smaller; `LINT_SHARPNESS_LIMIT` is the most that is ever asked, because an
/// edge cannot be narrower than a pixel.
const LINT_MIN_SHARPNESS: f32 = 0.23;
const LINT_SHARPNESS_LIMIT: f32 = 0.35;
/// Lint is judged against a plain background.  Where the picture without small
/// things changes by more than this many levels (of 255) within 11 px, there
/// is an edge: the print's own edge, or a sharp outline in the picture, whose
/// corners look like specks.  Marks there are left alone.
const LINT_MAX_BACKGROUND_STEP: u8 = 6;
const LINT_BACKGROUND_REACH: i32 = 11;
/// The area over which the usual roughness of the picture is measured.
const LINT_USUAL_AREA: i32 = 129;
/// If more than this share of the image looks like lint, it is something else
/// (snow, stars, glitter): nothing is removed.
const LINT_MAX_SHARE: f64 = 0.02;

/// Find lint in `img` (RGB 16-bit): a mask (255 = lint, grown by 2 px at
/// 1200 dpi) and the number of pieces.
fn lint_mask(img: &Mat, scale: Scale) -> Res<(Mat, usize)> {
    let max_thickness = LINT_MAX_THICKNESS * scale.k as f32;
    let max_area = (LINT_MAX_AREA as f64 * scale.k * scale.k).round() as i32;
    let max_length = scale.len(LINT_MAX_LENGTH as f64).round() as i32;
    let min_sharpness = (LINT_MIN_SHARPNESS / scale.k as f32).min(LINT_SHARPNESS_LIMIT);
    let (h, w) = (img.rows() as usize, img.cols() as usize);
    // grey, in 8-bit levels
    let mut g16 = Mat::default();
    cv(cvx::cvt_color(img, &mut g16, imgproc::COLOR_RGB2GRAY, 0))?;
    let mut g = Mat::default();
    cv(g16.convert_to(&mut g, CV_32F, 1.0 / 257.0, 0.0))?;
    drop(g16);
    // the picture without small things
    let med = {
        let mut g8 = Mat::default();
        cv(g.convert_to(&mut g8, CV_8U, 1.0, 0.0))?;
        let mut med = Mat::default();
        cv(imgproc::median_blur(&g8, &mut med, scale.odd(LINT_MEDIAN)))?;
        med
    };
    // how far each pixel stands out from it
    let mut res = {
        let mut medf = Mat::default();
        cv(med.convert_to(&mut medf, CV_32F, 1.0, 0.0))?;
        let mut r = Mat::default();
        cv(core::subtract(&g, &medf, &mut r, &core::no_array(), -1))?;
        r
    };
    // how much that picture changes around each pixel: edges
    let step = {
        let reach = scale.odd(LINT_BACKGROUND_REACH);
        let se = cv(imgproc::get_structuring_element(imgproc::MORPH_RECT, Size::new(reach, reach), Point::new(-1, -1)))?;
        let (mut hi, mut lo, mut out) = (Mat::default(), Mat::default(), Mat::default());
        cv(imgproc::dilate(&med, &mut hi, &se, Point::new(-1, -1), 1, core::BORDER_REPLICATE, Scalar::all(0.0)))?;
        cv(imgproc::erode(&med, &mut lo, &se, Point::new(-1, -1), 1, core::BORDER_REPLICATE, Scalar::all(0.0)))?;
        cv(core::subtract(&hi, &lo, &mut out, &core::no_array(), -1))?;
        out
    };
    drop(med);
    // the usual size of that around each pixel: noise, grain and paper texture
    let usual = {
        let mut a = Mat::default();
        cv(core::absdiff(&res, &Scalar::all(0.0), &mut a))?;
        let mut capped = Mat::default();
        cv(core::min(&a, &Scalar::all(12.0), &mut capped))?;
        let mut out = Mat::default();
        let area = scale.odd(LINT_USUAL_AREA);
        cv(imgproc::blur(&capped, &mut out, Size::new(area, area), Point::new(-1, -1), core::BORDER_DEFAULT))?;
        out
    };
    // steepness, taking in the pixels just outside each speck
    let steep = {
        let mut gs = Mat::default();
        let soften = scale.len(0.7);
        cv(cvx::gaussian_blur(&g, &mut gs, Size::new(0, 0), soften, soften, core::BORDER_DEFAULT))?;
        let (mut gx, mut gy, mut mag) = (Mat::default(), Mat::default(), Mat::default());
        cv(imgproc::sobel(&gs, &mut gx, CV_32F, 1, 0, 1, 0.5, 0.0, core::BORDER_DEFAULT))?;
        cv(imgproc::sobel(&gs, &mut gy, CV_32F, 0, 1, 1, 0.5, 0.0, core::BORDER_DEFAULT))?;
        cv(core::magnitude(&gx, &gy, &mut mag))?;
        let around = scale.odd(5);
        let se = cv(imgproc::get_structuring_element(imgproc::MORPH_RECT, Size::new(around, around), Point::new(-1, -1)))?;
        let mut out = Mat::default();
        cv(imgproc::dilate(&mag, &mut out, &se, Point::new(-1, -1), 1, core::BORDER_CONSTANT, Scalar::all(0.0)))?;
        out
    };
    drop(g);

    let mut total = vec![0u8; h * w];
    let mut pieces = 0usize;
    let usual_v = cv(usual.data_typed::<f32>())?;
    let steep_v = cv(steep.data_typed::<f32>())?;
    let step_v = cv(step.data_typed::<u8>())?;
    for sign in [1.0f32, -1.0] {
        if sign < 0.0 {
            cv(res.data_typed_mut::<f32>())?.par_iter_mut().for_each(|v| *v = -*v);
        }
        let r = cv(res.data_typed::<f32>())?;
        // pieces: everything that stands out a little, kept if part of it stands out a lot
        let mut weak = vec![0u8; h * w];
        weak.par_iter_mut().enumerate().for_each(|(k, m)| {
            // 1.2533 turns a mean absolute value into a standard deviation
            if r[k] > (3.0 * 1.2533 * usual_v[k]).max(4.0) {
                *m = 255;
            }
        });
        let weak_m = cv(Mat::new_rows_cols_with_data(h as i32, w as i32, &weak))?;
        let (mut labels, mut stats, mut centroids) = (Mat::default(), Mat::default(), Mat::default());
        let n = cv(imgproc::connected_components_with_stats(&weak_m, &mut labels, &mut stats, &mut centroids, 8, core::CV_32S))?
            as usize;
        let mut dist = Mat::default();
        cv(imgproc::distance_transform(&weak_m, &mut dist, cvx::DIST_L2, 3, CV_32F))?;
        let lab = cv(labels.data_typed::<i32>())?;
        let dist_v = cv(dist.data_typed::<f32>())?;
        let (mut strong, mut peak, mut edge, mut half) = (vec![false; n], vec![0f32; n], vec![0f32; n], vec![0f32; n]);
        let mut on_step = vec![0u8; n];
        for k in 0..h * w {
            let l = lab[k] as usize;
            if l == 0 {
                continue;
            }
            if r[k] > (6.0 * 1.2533 * usual_v[k]).max(8.0) {
                strong[l] = true;
            }
            peak[l] = peak[l].max(r[k]);
            edge[l] = edge[l].max(steep_v[k]);
            half[l] = half[l].max(dist_v[k]);
            on_step[l] = on_step[l].max(step_v[k]);
        }
        let mut keep = vec![false; n];
        for l in 1..n {
            let area = *cv(stats.at_2d::<i32>(l as i32, imgproc::CC_STAT_AREA))?;
            let bw = *cv(stats.at_2d::<i32>(l as i32, imgproc::CC_STAT_WIDTH))?;
            let bh = *cv(stats.at_2d::<i32>(l as i32, imgproc::CC_STAT_HEIGHT))?;
            keep[l] = strong[l]
                && 2.0 * half[l] <= max_thickness
                && area <= max_area
                && bw.max(bh) <= max_length
                && edge[l] >= min_sharpness * peak[l]
                && on_step[l] <= LINT_MAX_BACKGROUND_STEP;
            if keep[l] {
                pieces += 1;
            }
        }
        total.par_iter_mut().zip(lab.par_iter()).for_each(|(t, &l)| {
            if keep[l as usize] {
                *t = 255;
            }
        });
    }
    // take in the soft rim of each piece
    let total_m = cv(Mat::new_rows_cols_with_data(h as i32, w as i32, &total))?;
    let rim = scale.odd(5);
    let se = cv(imgproc::get_structuring_element(imgproc::MORPH_ELLIPSE, Size::new(rim, rim), Point::new(-1, -1)))?;
    let mut mask = Mat::default();
    cv(imgproc::dilate(&total_m, &mut mask, &se, Point::new(-1, -1), 1, core::BORDER_CONSTANT, Scalar::all(0.0)))?;
    Ok((mask, pieces))
}

fn gauss(m: &impl core::ToInputArray, sigma: f64) -> Res<Mat> {
    let mut out = Mat::default();
    cv(cvx::gaussian_blur(m, &mut out, Size::new(0, 0), sigma, sigma, core::BORDER_REFLECT_101))?;
    Ok(out)
}

/// Replace the masked pixels by a smooth blend of the pixels around them.
fn fill_masked(img: &mut Mat, mask: &Mat, scale: Scale) -> Res<()> {
    let (h, w) = (img.rows() as usize, img.cols() as usize);
    let mask_v = cv(mask.data_typed::<u8>())?;
    // in blocks, so that the working memory stays small; each block is worked
    // on with a border wide enough for the widest blur
    const BLOCK: usize = 1024;
    let halo = scale.len(48.0).ceil() as usize;
    // how far the soft rim of a masked pixel reaches
    let rim = scale.len(2.0).ceil() as usize;
    // the blurs: the soft rim, the near surroundings and the wider ones
    let (sig_rim, sig_near, sig_wide) = (scale.len(1.0), scale.len(3.0), scale.len(12.0));
    for by in (0..h).step_by(BLOCK) {
        for bx in (0..w).step_by(BLOCK) {
            let (y1, x1) = ((by + BLOCK).min(h), (bx + BLOCK).min(w));
            // anything to do here?  (masked pixels just outside matter too: their soft rim reaches in)
            let (cy0, cx0) = (by.saturating_sub(rim), bx.saturating_sub(rim));
            let (cy1, cx1) = ((y1 + rim).min(h), (x1 + rim).min(w));
            if !(cy0..cy1).any(|y| mask_v[y * w + cx0..y * w + cx1].iter().any(|&m| m != 0)) {
                continue;
            }
            let (ry0, rx0) = (by.saturating_sub(halo), bx.saturating_sub(halo));
            let (ry1, rx1) = ((y1 + halo).min(h), (x1 + halo).min(w));
            let (rh, rw) = (ry1 - ry0, rx1 - rx0);
            let mut keep = vec![0f32; rh * rw];
            let mut hard = vec![0f32; rh * rw];
            for y in 0..rh {
                for x in 0..rw {
                    let m = mask_v[(ry0 + y) * w + rx0 + x] != 0;
                    keep[y * rw + x] = if m { 0.0 } else { 1.0 };
                    hard[y * rw + x] = if m { 1.0 } else { 0.0 };
                }
            }
            let keep_m = cv(Mat::new_rows_cols_with_data(rh as i32, rw as i32, &keep))?;
            let (den_a, den_b) = (gauss(&keep_m, sig_near)?, gauss(&keep_m, sig_wide)?);
            let soft = gauss(&cv(Mat::new_rows_cols_with_data(rh as i32, rw as i32, &hard))?, sig_rim)?;
            let (den_a, den_b, soft) =
                (cv(den_a.data_typed::<f32>())?, cv(den_b.data_typed::<f32>())?, cv(soft.data_typed::<f32>())?);
            let mut plane = vec![0f32; rh * rw];
            for ch in 0..3 {
                {
                    let px = cv(img.data_typed::<Vec3w>())?;
                    for y in 0..rh {
                        for x in 0..rw {
                            plane[y * rw + x] = px[(ry0 + y) * w + rx0 + x].0[ch] as f32 * keep[y * rw + x];
                        }
                    }
                }
                let plane_m = cv(Mat::new_rows_cols_with_data(rh as i32, rw as i32, &plane))?;
                let (num_a, num_b) = (gauss(&plane_m, sig_near)?, gauss(&plane_m, sig_wide)?);
                let (num_a, num_b) = (cv(num_a.data_typed::<f32>())?, cv(num_b.data_typed::<f32>())?);
                let px = cv(img.data_typed_mut::<Vec3w>())?;
                for y in by - ry0..y1 - ry0 {
                    for x in bx - rx0..x1 - rx0 {
                        let k = y * rw + x;
                        let s = soft[k].max(hard[k]);
                        if s < 0.004 {
                            continue;
                        }
                        // near surroundings where there are enough of them, wider ones inside a large piece
                        let fill = if den_a[k] > 0.05 {
                            num_a[k] / den_a[k]
                        } else if den_b[k] > 1e-4 {
                            num_b[k] / den_b[k]
                        } else {
                            continue;
                        };
                        let p = &mut px[(ry0 + y) * w + rx0 + x].0[ch];
                        *p = (*p as f32 * (1.0 - s) + fill * s + 0.5).clamp(0.0, 65535.0) as u16;
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_lint(img: &mut Mat, scale: Scale, report: &mut Vec<String>) -> Res<()> {
    let (mask, pieces) = lint_mask(img, scale)?;
    let covered = cv(core::count_non_zero(&mask))? as f64 / (img.rows() as f64 * img.cols() as f64);
    if pieces == 0 {
        report.push("lint: none found".into());
        return Ok(());
    }
    if covered > LINT_MAX_SHARE {
        report.push(format!(
            "lint: {pieces} small bright or dark specks cover {:.1}% of the image, too many to be lint; image left as it is",
            covered * 100.0
        ));
        return Ok(());
    }
    fill_masked(img, &mask, scale)?;
    report.push(format!("lint: removed {pieces} specks and fibres ({:.2}% of the image)", covered * 100.0));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit RGB image from a function giving a grey level 0..1.
    fn image(h: usize, w: usize, f: impl Fn(usize, usize) -> f64) -> Mat {
        let mut m = Mat::new_rows_cols_with_default(h as i32, w as i32, core::CV_16UC3, Scalar::all(0.0)).unwrap();
        for (k, p) in m.data_typed_mut::<Vec3w>().unwrap().iter_mut().enumerate() {
            let v = (f(k / w, k % w).clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
            p.0 = [v, v, v];
        }
        m
    }

    fn level(m: &Mat, y: usize, x: usize) -> f64 {
        m.data_typed::<Vec3w>().unwrap()[y * m.cols() as usize + x].0[1] as f64 / 65535.0
    }

    /// a soft picture: slow waves and one straight edge
    fn picture(y: usize, x: usize) -> f64 {
        let (yf, xf) = (y as f64, x as f64);
        0.5 + 0.15 * (xf / 90.0).sin() * (yf / 70.0).cos() + if x > 400 { 0.12 } else { 0.0 }
    }

    /// a regular pattern like a print's surface: two crossed waves, 16 px apart
    fn weave(y: usize, x: usize) -> f64 {
        let (yf, xf) = (y as f64, x as f64);
        let k = 2.0 * std::f64::consts::PI / 16.0;
        0.02 * (k * (0.87 * xf + 0.5 * yf)).cos() + 0.02 * (k * (0.87 * xf - 0.5 * yf)).cos()
    }

    #[test]
    fn fft_round_trip() {
        const T: usize = 256;
        let fft = Fft::new(T);
        let orig: Vec<f32> = (0..T * T).map(|k| ((k * 7919) % 251) as f32 / 251.0).collect();
        let (mut re, mut im) = (orig.clone(), vec![0f32; T * T]);
        fft.tile(&mut re, &mut im, false);
        // the zero-frequency bin is the sum
        assert!((re[0] - orig.iter().sum::<f32>()).abs() < 0.05);
        fft.tile(&mut re, &mut im, true);
        assert!(re.iter().zip(&orig).all(|(a, b)| (a - b).abs() < 1e-4));
    }

    #[test]
    fn reflect_mirrors_without_repeating_the_edge() {
        assert_eq!((-2..7).map(|i| reflect(i, 5)).collect::<Vec<_>>(), [2, 1, 0, 1, 2, 3, 4, 3, 2]);
    }

    #[test]
    fn texture_pattern_is_removed_and_the_picture_kept() {
        let (h, w) = (700, 800);
        let mut m = image(h, w, |y, x| picture(y, x) + weave(y, x));
        let mut report = Vec::new();
        remove_texture(&mut m, Scale::new(1200.0), &mut report).unwrap();
        assert!(report[0].contains("removed a pattern"), "{report:?}");
        // what is left of the pattern, away from the picture's edge
        let (mut left, mut n) = (0f64, 0);
        for y in (50..h - 50).step_by(3) {
            for x in (50..360).step_by(3) {
                left += (level(&m, y, x) - picture(y, x)).powi(2);
                n += 1;
            }
        }
        let rms = (left / n as f64).sqrt();
        // the pattern's own rms is 0.02
        assert!(rms < 0.004, "pattern left: rms {rms}");
        // the edge is still a step of 0.12, one pixel wide
        let step = level(&m, 300, 402) - level(&m, 300, 399);
        assert!((step - 0.12).abs() < 0.03, "edge step {step}");
    }

    #[test]
    fn sizes_follow_the_resolution() {
        let at = |dpi: f64| Scale::new(dpi);
        // unchanged at 1200 dpi, the resolution the sizes are written for
        assert_eq!([at(1200.0).odd(5), at(1200.0).odd(11), at(1200.0).odd(25), at(1200.0).odd(129)], [5, 11, 25, 129]);
        assert_eq!(at(1200.0).len(7.0), 7.0);
        // filter sizes stay odd, and never shrink to nothing
        assert_eq!([at(600.0).odd(5), at(600.0).odd(25), at(2400.0).odd(25), at(4800.0).odd(25)], [3, 13, 49, 97]);
        assert_eq!(at(150.0).odd(5), 3);
        // a tile is the same piece of the print at every resolution
        assert_eq!([at(600.0).tile(), at(1200.0).tile(), at(2400.0).tile(), at(4800.0).tile()], [128, 256, 512, 1024]);
    }

    #[test]
    fn the_same_pattern_is_found_at_other_resolutions() {
        // the weave of the 1200 dpi test, as it looks at 600 and at 2400 dpi
        for (dpi, k) in [(600.0, 0.5), (2400.0, 2.0)] {
            let (h, w) = ((700.0 * k) as usize, (800.0 * k) as usize);
            let at = |y: usize, x: usize| ((y as f64 / k) as usize, (x as f64 / k) as usize);
            let smooth = |y: usize, x: usize| {
                let (yf, xf) = (y as f64 / k, x as f64 / k);
                0.5 + 0.15 * (xf / 90.0).sin() * (yf / 70.0).cos() + if at(y, x).1 > 400 { 0.12 } else { 0.0 }
            };
            let wave = |y: usize, x: usize| {
                let (yf, xf) = (y as f64 / k, x as f64 / k);
                let f = 2.0 * std::f64::consts::PI / 16.0;
                0.02 * (f * (0.87 * xf + 0.5 * yf)).cos() + 0.02 * (f * (0.87 * xf - 0.5 * yf)).cos()
            };
            let mut m = image(h, w, |y, x| smooth(y, x) + wave(y, x));
            let mut report = Vec::new();
            remove_texture(&mut m, Scale::new(dpi), &mut report).unwrap();
            assert!(report[0].contains("removed a pattern"), "{dpi} dpi: {report:?}");
            // 16 px at 1200 dpi is 0.34 mm
            assert!(report[0].contains("(0.34 mm)"), "{dpi} dpi: {report:?}");
            let (mut left, mut n) = (0f64, 0);
            for y in ((50.0 * k) as usize..h - (50.0 * k) as usize).step_by(3) {
                for x in ((50.0 * k) as usize..(360.0 * k) as usize).step_by(3) {
                    left += (level(&m, y, x) - smooth(y, x)).powi(2);
                    n += 1;
                }
            }
            let rms = (left / n as f64).sqrt();
            assert!(rms < 0.004, "{dpi} dpi: pattern left: rms {rms}");
        }
    }

    #[test]
    fn lint_is_removed_at_twice_the_resolution() {
        // the 1200 dpi lint test, as it looks at 2400 dpi: everything twice the size
        let (h, w) = (1000, 1200);
        let speck = |y: usize, x: usize| (y as f64 - 240.0).hypot(x as f64 - 300.0) < 3.2;
        let fibre = |y: usize, x: usize| (600..680).contains(&x) && (500 + (x - 600) / 4..504 + (x - 600) / 4).contains(&y);
        let glow = |y: usize, x: usize| 0.25 * (-((y as f64 - 760.0).powi(2) + (x as f64 - 900.0).powi(2)) / (2.0 * 12.0 * 12.0)).exp();
        let base = |x: usize| 0.35 + 0.1 * (x as f64 / 240.0).sin();
        let mut m = image(h, w, |y, x| if speck(y, x) || fibre(y, x) { base(x) + 0.3 } else { base(x) + glow(y, x) });
        let before_glow = level(&m, 760, 900);
        let mut report = Vec::new();
        remove_lint(&mut m, Scale::new(2400.0), &mut report).unwrap();
        assert!(report[0].contains("removed 2 specks"), "{report:?}");
        assert!((level(&m, 240, 300) - base(300)).abs() < 0.01, "speck left: {}", level(&m, 240, 300));
        assert!((level(&m, 511, 640) - base(640)).abs() < 0.01, "fibre left: {}", level(&m, 511, 640));
        assert_eq!(level(&m, 760, 900), before_glow, "the soft highlight was touched");
    }

    #[test]
    fn a_picture_without_a_pattern_is_left_alone() {
        let before = image(600, 700, picture);
        let mut m = before.clone();
        let mut report = Vec::new();
        remove_texture(&mut m, Scale::new(1200.0), &mut report).unwrap();
        assert!(report[0].contains("no regular pattern"), "{report:?}");
        assert_eq!(m.data_bytes().unwrap(), before.data_bytes().unwrap());
    }

    #[test]
    fn lint_is_removed_and_soft_detail_kept() {
        let (h, w) = (500, 600);
        // a hard speck 3 px across, a hard fibre 2 px wide, and a soft highlight 12 px across
        let speck = |y: usize, x: usize| (y as f64 - 120.0).hypot(x as f64 - 150.0) < 1.6;
        let fibre = |y: usize, x: usize| (300..340).contains(&x) && (y == 250 + (x - 300) / 4 || y == 251 + (x - 300) / 4);
        let glow = |y: usize, x: usize| 0.25 * (-((y as f64 - 380.0).powi(2) + (x as f64 - 450.0).powi(2)) / (2.0 * 6.0 * 6.0)).exp();
        let mut m = image(h, w, |y, x| {
            let base = 0.35 + 0.1 * (x as f64 / 120.0).sin();
            if speck(y, x) || fibre(y, x) {
                base + 0.3
            } else {
                base + glow(y, x)
            }
        });
        let before_glow = level(&m, 380, 450);
        let mut report = Vec::new();
        remove_lint(&mut m, Scale::new(1200.0), &mut report).unwrap();
        assert!(report[0].contains("removed 2 specks"), "{report:?}");
        let base = |x: usize| 0.35 + 0.1 * (x as f64 / 120.0).sin();
        assert!((level(&m, 120, 150) - base(150)).abs() < 0.01, "speck left: {}", level(&m, 120, 150));
        assert!((level(&m, 255, 320) - base(320)).abs() < 0.01, "fibre left: {}", level(&m, 255, 320));
        assert_eq!(level(&m, 380, 450), before_glow, "the soft highlight was touched");
    }
}
