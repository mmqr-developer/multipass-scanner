//! Align and combine several scans of the same photo.
//!
//! Every scan is registered to the first one:
//!
//!   1. coarse placement   SIFT features + RANSAC similarity transform
//!   2. rigid refinement   ECC affine refinement inside the area of the photo
//!   3. local refinement   tile-by-tile phase correlation against the consensus
//!                         of all scans, fitted with a smooth polynomial warp;
//!                         the average warp is removed
//!   3b. carriage wobble   dense optical flow against the other scans, fitted
//!                         per scan as a shift along its own y axis that depends
//!                         only on the scan line, plus a very smooth remainder;
//!                         every round must improve the agreement or is undone
//!   4. resampling         one Lanczos resampling per scan, at full resolution
//!   5. exposure match     gain + offset per scan and channel
//!   6. combining          sigma-clipped mean, 16-bit output
//!
//! Scans may differ in size (each is cropped around the photo).

use opencv::core::{self, Mat, Point, Point2f, Scalar, Size, Vector, CV_32F, CV_32FC1, CV_8U, CV_8UC1};
use opencv::prelude::*;
use opencv::{features2d, imgproc, video};

use crate::cvx;
use rayon::prelude::*;

type Res<T> = Result<T, String>;

fn cv<T>(r: opencv::Result<T>) -> Res<T> {
    r.map_err(|e| format!("OpenCV: {e}"))
}

pub struct Combined {
    /// RGB, 16 bits per sample
    pub image: Mat,
    pub report: Vec<String>,
}

// ------------------------------------------------------------------ helpers

/// With MULTIPASS_MEM=2, print the peak memory since the last mark (where the
/// system can tell).
fn mem_mark(label: &str) {
    if !crate::platform::MEMORY_TRACE || std::env::var("MULTIPASS_MEM").as_deref() != Ok("2") {
        return;
    }
    let (peak, now) = crate::platform::memory_gb();
    eprintln!("      peak {peak:5.2} GB, now {now:5.2} GB   {label}");
    crate::platform::reset_peak_memory();
}

type M23 = [[f64; 3]; 2];
type M33 = [[f64; 3]; 3];

fn h(m: &M23) -> M33 {
    [m[0], m[1], [0.0, 0.0, 1.0]]
}

fn mul(a: &M33, b: &M33) -> M33 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    r
}

fn inv33(m: &M33) -> M33 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            let (a, b) = ((j + 1) % 3, (j + 2) % 3);
            let (c, d) = ((i + 1) % 3, (i + 2) % 3);
            r[i][j] = (m[a][c] * m[b][d] - m[a][d] * m[b][c]) / det;
        }
    }
    r
}

/// Pixel-centre-correct map from full-resolution to scaled coordinates.
fn scale_matrix(s: f64) -> M33 {
    let c = 0.5 * s - 0.5;
    [[s, 0.0, c], [0.0, s, c], [0.0, 0.0, 1.0]]
}

/// Re-express an affine (ref -> scan) defined at scale s_from at scale s_to.
fn rescale_affine(m: &M23, s_from: f64, s_to: f64) -> M23 {
    let a = mul(&scale_matrix(s_to), &inv33(&scale_matrix(s_from)));
    let r = mul(&mul(&a, &h(m)), &inv33(&a));
    [r[0], r[1]]
}

fn mat_from_m23(m: &M23) -> Res<Mat> {
    let mut out = cv(Mat::new_rows_cols_with_default(2, 3, core::CV_64F, Scalar::all(0.0)))?;
    for i in 0..2 {
        for j in 0..3 {
            *cv(out.at_2d_mut::<f64>(i as i32, j as i32))? = m[i][j];
        }
    }
    Ok(out)
}

fn m23_from_mat(m: &Mat) -> Res<M23> {
    let mut r = [[0.0; 3]; 2];
    let m64 = if m.typ() == core::CV_64F {
        m.clone()
    } else {
        let mut t = Mat::default();
        cv(m.convert_to(&mut t, core::CV_64F, 1.0, 0.0))?;
        t
    };
    for i in 0..2 {
        for j in 0..3 {
            r[i][j] = *cv(m64.at_2d::<f64>(i as i32, j as i32))?;
        }
    }
    Ok(r)
}

fn new_f32(rows: usize, cols: usize, v: f32) -> Res<Mat> {
    cv(Mat::new_rows_cols_with_default(rows as i32, cols as i32, CV_32FC1, Scalar::all(v as f64)))
}

fn f32s(m: &Mat) -> Res<&[f32]> {
    cv(m.data_typed::<f32>())
}

fn f32s_mut(m: &mut Mat) -> Res<&mut [f32]> {
    cv(m.data_typed_mut::<f32>())
}

/// Borrow a slice as a single-channel f32 matrix, without copying it.
fn view(rows: usize, cols: usize, v: &[f32]) -> Res<impl core::ToInputArray + '_> {
    cv(Mat::new_rows_cols_with_data(rows as i32, cols as i32, v))
}

fn mat_from_vec(rows: usize, cols: usize, v: &[f32]) -> Res<Mat> {
    let mut m = new_f32(rows, cols, 0.0)?;
    f32s_mut(&mut m)?.copy_from_slice(v);
    Ok(m)
}

fn resize_area(m: &Mat, scale: f64) -> Res<Mat> {
    if scale == 1.0 {
        return Ok(m.clone());
    }
    let s = cv(m.size())?;
    let size = Size::new(((s.width as f64 * scale).round() as i32).max(1), ((s.height as f64 * scale).round() as i32).max(1));
    let mut out = Mat::default();
    cv(imgproc::resize(m, &mut out, size, 0.0, 0.0, imgproc::INTER_AREA))?;
    Ok(out)
}

fn resize_to(m: &Mat, w: i32, h: i32, interp: i32) -> Res<Mat> {
    let mut out = Mat::default();
    cv(imgproc::resize(m, &mut out, Size::new(w, h), 0.0, 0.0, interp))?;
    Ok(out)
}

fn blur(m: &impl core::ToInputArray, sigma: f64) -> Res<Mat> {
    let mut out = Mat::default();
    cv(cvx::gaussian_blur(m, &mut out, Size::new(0, 0), sigma, sigma, core::BORDER_DEFAULT))?;
    Ok(out)
}

fn bandpass(m: &impl core::ToInputArray, lo: f64, hi: f64) -> Res<Mat> {
    let a = blur(m, lo)?;
    let b = blur(m, hi)?;
    let mut out = Mat::default();
    cv(core::subtract(&a, &b, &mut out, &core::no_array(), -1))?;
    Ok(out)
}

/// RGB 8-bit (full resolution) -> grey float 0..1
fn gray_f32(rgb: &Mat) -> Res<Mat> {
    let mut g = Mat::default();
    cv(cvx::cvt_color(rgb, &mut g, imgproc::COLOR_RGB2GRAY, 0))?;
    let mut f = Mat::default();
    cv(g.convert_to(&mut f, CV_32F, 1.0 / 255.0, 0.0))?;
    Ok(f)
}

fn percentile(v: &mut [f32], p: f64) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let k = (((v.len() - 1) as f64) * p / 100.0).round() as usize;
    let (_, x, _) = v.select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    *x
}

fn to_u8(g: &Mat) -> Res<Mat> {
    let d = f32s(g)?;
    let mut sub: Vec<f32> = d.iter().step_by(16).copied().filter(|v| v.is_finite()).collect();
    let lo = percentile(&mut sub, 0.5);
    let mut hi = percentile(&mut sub, 99.5);
    if hi <= lo {
        hi = lo + 1e-3;
    }
    let mut out = Mat::default();
    cv(g.convert_to(&mut out, CV_8U, 255.0 / (hi - lo) as f64, -(lo as f64) * 255.0 / (hi - lo) as f64))?;
    Ok(out)
}

// ------------------------------------------------------------ smooth field

/// Optical flow on the whole frame needs about 7 GB for a 27-megapixel photo;
/// in tiles it needs under 3 GB and gives the same result to about 0.002 px.
/// Tiles are used only on machines without more than this much memory.
const WHOLE_FRAME_ABOVE_RAM: u64 = 16 * 1024 * 1024 * 1024;
/// Tile area and overlap when the optical flow is tiled.
const FLOW_TILE_PX: usize = 4_000_000;
const FLOW_HALO: usize = 256;

/// Installed memory in bytes, if it can be found out.
pub fn total_ram() -> Option<u64> {
    // MULTIPASS_RAM_GB pretends the machine has that much memory (for testing).
    if let Some(gb) = std::env::var("MULTIPASS_RAM_GB").ok().and_then(|v| v.parse::<f64>().ok()) {
        return Some((gb * 1024.0 * 1024.0 * 1024.0) as u64);
    }
    crate::platform::total_ram()
}

/// Largest area to give the optical flow at once (usize::MAX: the whole frame).
/// `forced_mp` is MULTIPASS_FLOW_TILE_MP: a tile size in megapixels, 0 = whole frame.
fn flow_tile_limit(ram: Option<u64>, forced_mp: Option<f64>) -> usize {
    match forced_mp {
        Some(mp) if mp <= 0.0 => usize::MAX,
        Some(mp) => (mp * 1e6) as usize,
        // more than 16 GB: whole frame.  16 GB or less, or unknown: tiles.
        None => match ram {
            Some(bytes) if bytes > WHOLE_FRAME_ABOVE_RAM => usize::MAX,
            _ => FLOW_TILE_PX,
        },
    }
}

const DEG: usize = 3;
const NTERMS: usize = (DEG + 1) * (DEG + 2) / 2;

fn terms(u: f64, v: f64, deg: usize, out: &mut [f64; NTERMS]) -> usize {
    let mut k = 0;
    for i in 0..=deg {
        for j in 0..=(deg - i) {
            out[k] = u.powi(i as i32) * v.powi(j as i32);
            k += 1;
        }
    }
    k
}

#[derive(Clone)]
struct SmoothField {
    cx: f64,
    cy: f64,
    half: f64,
    cxf: [f64; NTERMS], // always stored at degree 3
    cyf: [f64; NTERMS],
    /// area the field was measured on (x0, y0, x1, y1); outside it the value at
    /// the nearest point is used, because a cubic grows fast when extrapolated
    bounds: (f64, f64, f64, f64),
}

impl SmoothField {
    fn zero(cx: f64, cy: f64, half: f64) -> Self {
        SmoothField { cx, cy, half, cxf: [0.0; NTERMS], cyf: [0.0; NTERMS], bounds: (f64::MIN, f64::MIN, f64::MAX, f64::MAX) }
    }

    fn eval(&self, x: f64, y: f64) -> (f64, f64) {
        let x = x.clamp(self.bounds.0, self.bounds.2);
        let y = y.clamp(self.bounds.1, self.bounds.3);
        let mut t = [0.0; NTERMS];
        terms((x - self.cx) / self.half, (y - self.cy) / self.half, DEG, &mut t);
        let mut dx = 0.0;
        let mut dy = 0.0;
        for k in 0..NTERMS {
            dx += t[k] * self.cxf[k];
            dy += t[k] * self.cyf[k];
        }
        (dx, dy)
    }

    /// Robust weighted least squares (a few rounds of outlier rejection).
    fn fit(pts: &[(f64, f64, f64, f64, f64)], deg: usize, cx: f64, cy: f64, half: f64) -> Option<Self> {
        let nt = (deg + 1) * (deg + 2) / 2;
        let rows: Vec<[f64; NTERMS]> = pts
            .iter()
            .map(|p| {
                let mut t = [0.0; NTERMS];
                terms((p.0 - cx) / half, (p.1 - cy) / half, deg, &mut t);
                t
            })
            .collect();
        let mut keep = vec![true; pts.len()];
        let mut cxf = vec![0.0; nt];
        let mut cyf = vec![0.0; nt];
        for _ in 0..4 {
            if keep.iter().filter(|&&k| k).count() < nt * 2 {
                return None;
            }
            // normal equations
            let mut a = vec![0.0; nt * nt];
            let mut bx = vec![0.0; nt];
            let mut by = vec![0.0; nt];
            for (i, p) in pts.iter().enumerate() {
                if !keep[i] {
                    continue;
                }
                let w = p.4;
                let t = &rows[i];
                for r in 0..nt {
                    bx[r] += w * t[r] * p.2;
                    by[r] += w * t[r] * p.3;
                    for c in 0..nt {
                        a[r * nt + c] += w * t[r] * t[c];
                    }
                }
            }
            cxf = solve(&a, &bx, nt)?;
            cyf = solve(&a, &by, nt)?;
            let res: Vec<f64> = pts
                .iter()
                .zip(&rows)
                .map(|(p, t)| {
                    let ex: f64 = (0..nt).map(|k| t[k] * cxf[k]).sum::<f64>() - p.2;
                    let ey: f64 = (0..nt).map(|k| t[k] * cyf[k]).sum::<f64>() - p.3;
                    ex.hypot(ey)
                })
                .collect();
            let mut kept: Vec<f32> = res.iter().zip(&keep).filter(|(_, &k)| k).map(|(r, _)| *r as f32).collect();
            let mad = percentile(&mut kept, 50.0) as f64 * 1.4826 + 1e-3;
            let new_keep: Vec<bool> = res.iter().map(|&r| r < (4.0 * mad).max(0.05)).collect();
            if new_keep == keep {
                break;
            }
            keep = new_keep;
        }
        // bring to degree 3 so fields can be averaged
        let mut f = SmoothField::zero(cx, cy, half);
        let mut k = 0;
        for i in 0..=deg {
            for j in 0..=(deg - i) {
                // index of (i, j) in the degree-3 ordering
                let mut idx = 0;
                'outer: for a in 0..=DEG {
                    for b in 0..=(DEG - a) {
                        if a == i && b == j {
                            break 'outer;
                        }
                        idx += 1;
                    }
                }
                f.cxf[idx] = cxf[k];
                f.cyf[idx] = cyf[k];
                k += 1;
            }
        }
        Some(f)
    }
}

/// Solve a small symmetric system (Gaussian elimination with partial pivoting).
fn solve(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut m: Vec<f64> = Vec::with_capacity(n * (n + 1));
    let scale = (0..n).map(|i| a[i * n + i].abs()).fold(0.0, f64::max).max(1e-300);
    for r in 0..n {
        for c in 0..n {
            m.push(a[r * n + c] + if r == c { 1e-12 * scale } else { 0.0 });
        }
        m.push(b[r]);
    }
    let w = n + 1;
    for col in 0..n {
        let piv = (col..n).max_by(|&i, &j| m[i * w + col].abs().partial_cmp(&m[j * w + col].abs()).unwrap())?;
        if m[piv * w + col].abs() < 1e-300 {
            return None;
        }
        for c in 0..w {
            m.swap(col * w + c, piv * w + c);
        }
        for r in 0..n {
            if r != col {
                let f = m[r * w + col] / m[col * w + col];
                for c in col..w {
                    m[r * w + c] -= f * m[col * w + c];
                }
            }
        }
    }
    Some((0..n).map(|r| m[r * w + n] / m[r * w + r]).collect())
}

// ------------------------------------------------------------- the warp

/// Where each output pixel is found in one scan.  Output coordinates q and scan
/// coordinates p are both at the registration (fine) scale:
///   p0 = M · (q + F(q) + R(q))   (affine, smooth polynomial, smooth residual grid)
///   p  = p0 + (0, j(p0_y))       (carriage error along the scan's own y axis)
#[derive(Clone)]
struct ScanWarp {
    m: M23,
    field: Option<SmoothField>,
    resid: Option<Vec<[f32; 2]>>, // gh x gw, output-frame displacement
    gh: usize,
    gw: usize,
    rstep: f64,
    jitter: Option<Vec<f64>>, // fine px, sampled jscale times per fine scan line
    jscale: f64,
}

impl ScanWarp {
    fn new(m: M23, field: Option<SmoothField>) -> Self {
        ScanWarp { m, field, resid: None, gh: 0, gw: 0, rstep: 8.0, jitter: None, jscale: 1.0 }
    }

    #[inline]
    fn sample_resid(&self, qx: f64, qy: f64) -> (f64, f64) {
        let g = self.resid.as_ref().unwrap();
        let c = (self.rstep - 1.0) / 2.0;
        let gx = ((qx - c) / self.rstep).clamp(0.0, self.gw as f64 - 1.000001);
        let gy = ((qy - c) / self.rstep).clamp(0.0, self.gh as f64 - 1.000001);
        let x0 = gx.floor() as usize;
        let y0 = gy.floor() as usize;
        let x1 = (x0 + 1).min(self.gw - 1);
        let y1 = (y0 + 1).min(self.gh - 1);
        let fx = gx - x0 as f64;
        let fy = gy - y0 as f64;
        let at = |y: usize, x: usize, k: usize| g[y * self.gw + x][k] as f64;
        let mut out = [0.0; 2];
        for (k, o) in out.iter_mut().enumerate() {
            let top = at(y0, x0, k) * (1.0 - fx) + at(y0, x1, k) * fx;
            let bot = at(y1, x0, k) * (1.0 - fx) + at(y1, x1, k) * fx;
            *o = top * (1.0 - fy) + bot * fy;
        }
        (out[0], out[1])
    }

    /// (px, py, p0y)
    #[inline]
    fn map(&self, qx: f64, qy: f64) -> (f64, f64, f64) {
        let (mut ux, mut uy) = (qx, qy);
        if let Some(f) = &self.field {
            let (dx, dy) = f.eval(qx, qy);
            ux += dx;
            uy += dy;
        }
        if self.resid.is_some() {
            let (rx, ry) = self.sample_resid(qx, qy);
            ux += rx;
            uy += ry;
        }
        let m = &self.m;
        let px = m[0][0] * ux + m[0][1] * uy + m[0][2];
        let py0 = m[1][0] * ux + m[1][1] * uy + m[1][2];
        let mut py = py0;
        if let Some(j) = &self.jitter {
            py += interp(py0 * self.jscale, j);
        }
        (px, py, py0)
    }
}

/// np.interp over sample positions 0..len-1 (constant beyond the ends).
#[inline]
fn interp(x: f64, v: &[f64]) -> f64 {
    if x <= 0.0 {
        return v[0];
    }
    let n = v.len();
    if x >= (n - 1) as f64 {
        return v[n - 1];
    }
    let i = x.floor() as usize;
    let f = x - i as f64;
    v[i] * (1.0 - f) + v[i + 1] * f
}

/// Build remap maps for the output pixels (xs[c], ys[r]) through `f`.
fn build_maps<F>(xs: &[f64], ys: &[f64], f: F, want_p0y: bool) -> Res<(Mat, Mat, Vec<f32>)>
where
    F: Fn(f64, f64) -> (f64, f64, f64) + Sync,
{
    let (w, hh) = (xs.len(), ys.len());
    let mut mxm = new_f32(hh, w, 0.0)?;
    let mut mym = new_f32(hh, w, 0.0)?;
    let mut p0 = if want_p0y { vec![0f32; w * hh] } else { Vec::new() };
    {
        let mx = f32s_mut(&mut mxm)?;
        let my = f32s_mut(&mut mym)?;
        if want_p0y {
            mx.par_chunks_mut(w)
                .zip(my.par_chunks_mut(w))
                .zip(p0.par_chunks_mut(w))
                .enumerate()
                .for_each(|(r, ((rx, ry), rp))| {
                    for c in 0..w {
                        let (px, py, p0y) = f(xs[c], ys[r]);
                        rx[c] = px as f32;
                        ry[c] = py as f32;
                        rp[c] = p0y as f32;
                    }
                });
        } else {
            mx.par_chunks_mut(w).zip(my.par_chunks_mut(w)).enumerate().for_each(|(r, (rx, ry))| {
                for c in 0..w {
                    let (px, py, _) = f(xs[c], ys[r]);
                    rx[c] = px as f32;
                    ry[c] = py as f32;
                }
            });
        }
    }
    Ok((mxm, mym, p0))
}

fn remap(img: &Mat, mx: &Mat, my: &Mat, interp: i32, border: i32, value: f64) -> Res<Mat> {
    let mut out = Mat::default();
    cv(cvx::remap(img, &mut out, mx, my, interp, border, Scalar::all(value)))?;
    Ok(out)
}

/// Side of the output tiles in the full-resolution resampling.
const RESAMPLE_TILE: usize = 2048;

/// The part of a scan, `len` pixels long on this axis, that the source
/// coordinates `coords` reach: (start, length), with room for the resampling
/// kernel.  Where it stops short of the coordinates, it stops at the scan's
/// own edge, so repeating the edge pixel gives what the whole scan would.
fn source_span(coords: &[f32], len: i32) -> (i32, i32) {
    const KERNEL: i64 = 8;
    let lo = coords.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = coords.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !(lo <= hi) {
        return (0, 1); // no usable coordinate at all
    }
    let len = len as i64;
    let start = (lo.floor() as i64 - KERNEL).clamp(0, len - 1);
    let end = (hi.ceil() as i64 + KERNEL + 1).clamp(start + 1, len);
    (start as i32, (end - start) as i32)
}

/// Sample a fine-scale grey image for output pixels in box (y0, y1, x0, x1).
fn warp_with(img: &Mat, warp: &ScanWarp, b: (usize, usize, usize, usize)) -> Res<Mat> {
    let xs: Vec<f64> = (b.2..b.3).map(|v| v as f64).collect();
    let ys: Vec<f64> = (b.0..b.1).map(|v| v as f64).collect();
    let (mx, my, _) = build_maps(&xs, &ys, |x, y| warp.map(x, y), false)?;
    remap(img, &mx, &my, imgproc::INTER_LINEAR, core::BORDER_CONSTANT, f64::NAN)
}

// ------------------------------------------------------------ registration

fn sift_similarity(
    sift: &mut core::Ptr<features2d::SIFT>,
    ref_kd: &(Vector<core::KeyPoint>, Mat),
    mov: &Mat,
    report: &mut Vec<String>,
) -> Res<Option<(M23, Vec<(f32, f32)>)>> {
    let mut kp2 = Vector::<core::KeyPoint>::new();
    let mut d2 = Mat::default();
    cv(sift.detect_and_compute(mov, &core::no_array(), &mut kp2, &mut d2, false))?;
    let (kp1, d1) = ref_kd;
    if kp1.len() < 8 || kp2.len() < 8 {
        return Ok(None);
    }
    let matcher = cv(features2d::BFMatcher::create(core::NORM_L2, false))?;
    let mut knn = Vector::<Vector<core::DMatch>>::new();
    cv(matcher.knn_train_match(d1, &d2, &mut knn, 2, &core::no_array(), false))?;
    let mut p1 = Vec::new();
    let mut p2 = Vec::new();
    for m in knn.iter() {
        if m.len() == 2 {
            let (a, b) = (cv(m.get(0))?, cv(m.get(1))?);
            if a.distance < 0.75 * b.distance {
                p1.push(cv(kp1.get(a.query_idx as usize))?.pt());
                p2.push(cv(kp2.get(a.train_idx as usize))?.pt());
            }
        }
    }
    if p1.len() < 8 {
        return Ok(None);
    }
    let solve_rs = |a: &[Point2f], b: &[Point2f]| -> Res<Option<(M23, Vec<bool>)>> {
        let va: Vector<Point2f> = a.iter().copied().collect();
        let vb: Vector<Point2f> = b.iter().copied().collect();
        let mut inl = Mat::default();
        let m = cv(cvx::estimate_affine_partial_2d(&va, &vb, &mut inl, cvx::RANSAC, 3.0, 5000, 0.999, 10))?;
        if m.empty() {
            return Ok(None);
        }
        let flags: Vec<bool> = (0..a.len()).map(|i| inl.at::<u8>(i as i32).map(|v| *v != 0).unwrap_or(false)).collect();
        Ok(Some((m23_from_mat(&m)?, flags)))
    };
    let Some((mut m, mut inl)) = solve_rs(&p1, &p2)? else { return Ok(None) };
    let mut used = p1.clone();
    // A static feature (glass edge, lid mark) matches itself at the identity.
    let ang = m[1][0].atan2(m[0][0]).to_degrees();
    if ang.abs() < 1.0 && m[0][2].hypot(m[1][2]) < 3.0 {
        let moved: Vec<usize> =
            (0..p1.len()).filter(|&i| ((p2[i].x - p1[i].x) as f64).hypot((p2[i].y - p1[i].y) as f64) > 4.0).collect();
        if moved.len() >= 12 {
            let a: Vec<Point2f> = moved.iter().map(|&i| p1[i]).collect();
            let b: Vec<Point2f> = moved.iter().map(|&i| p2[i]).collect();
            let n = inl.iter().filter(|&&f| f).count();
            if let Some((m2, inl2)) = solve_rs(&a, &b)? {
                let n2 = inl2.iter().filter(|&&f| f).count();
                if n2 >= 12.max(n / 2) {
                    report.push(format!("    ignoring {n} static matches (glass/lid features)"));
                    m = m2;
                    inl = inl2;
                    used = a;
                }
            }
        }
    }
    let pts: Vec<(f32, f32)> = used.iter().zip(&inl).filter(|(_, &k)| k).map(|(p, _)| (p.x, p.y)).collect();
    if pts.len() < 8 {
        return Ok(None);
    }
    Ok(Some((m, pts)))
}

fn ecc_refine(reference: &Mat, mov: &Mat, m: &M23, mask: &Mat) -> Result<M23, String> {
    let mut warp = Mat::default();
    cv(mat_from_m23(m)?.convert_to(&mut warp, CV_32F, 1.0, 0.0))?;
    let criteria = cv(core::TermCriteria::new(core::TermCriteria_EPS + core::TermCriteria_COUNT, 200, 1e-6))?;
    cv(video::find_transform_ecc(reference, mov, &mut warp, video::MOTION_AFFINE, criteria, mask, 5))?;
    m23_from_mat(&warp)
}

/// Residual shift of `mov_bp` (scan frame) against the consensus (output frame)
/// on a grid of tiles: (x, y, dx, dy, weight).
fn tile_shifts(
    cons_bp: &Mat,
    mov_bp: &Mat,
    warp: &ScanWarp,
    mask: &[bool],
    tile: usize,
    step: usize,
) -> Res<Vec<(f64, f64, f64, f64, f64)>> {
    let s = cv(cons_bp.size())?;
    let (w, hh) = (s.width as usize, s.height as usize);
    let half = tile / 2;
    let mut win = Mat::default();
    cv(imgproc::create_hanning_window(&mut win, Size::new(tile as i32, tile as i32), CV_32F))?;
    let cons = f32s(cons_bp)?;
    let mut centres = Vec::new();
    let mut cy = half;
    while cy + half <= hh {
        let mut cx = half;
        while cx + half <= w {
            if mask[cy * w + cx] {
                centres.push((cx, cy));
            }
            cx += step;
        }
        cy += step;
    }
    let results: Vec<Option<(f64, f64, f64, f64, f64)>> = centres
        .par_iter()
        .map(|&(cx, cy)| -> Option<(f64, f64, f64, f64, f64)> {
            let mut ref_t = vec![0f32; tile * tile];
            for r in 0..tile {
                let src = &cons[(cy - half + r) * w + cx - half..(cy - half + r) * w + cx + half];
                ref_t[r * tile..(r + 1) * tile].copy_from_slice(src);
            }
            if ref_t.iter().any(|v| !v.is_finite()) {
                return None;
            }
            let mean = ref_t.iter().map(|&v| v as f64).sum::<f64>() / ref_t.len() as f64;
            let sd = (ref_t.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / ref_t.len() as f64).sqrt();
            if sd < 2e-3 {
                return None;
            }
            let (ddx, ddy) = warp.field.as_ref().map_or((0.0, 0.0), |f| f.eval(cx as f64, cy as f64));
            let sh: M33 = [[1.0, 0.0, (cx - half) as f64 + ddx], [0.0, 1.0, (cy - half) as f64 + ddy], [0.0, 0.0, 1.0]];
            let mt = mul(&h(&warp.m), &sh);
            let mt = [mt[0], mt[1]];
            let mtm = mat_from_m23(&mt).ok()?;
            let mut mov_t = Mat::default();
            cvx::warp_affine(
                mov_bp,
                &mut mov_t,
                &mtm,
                Size::new(tile as i32, tile as i32),
                imgproc::INTER_LINEAR | imgproc::WARP_INVERSE_MAP,
                core::BORDER_CONSTANT,
                Scalar::all(f64::NAN),
            )
            .ok()?;
            if mov_t.data_typed::<f32>().ok()?.iter().any(|v| !v.is_finite()) {
                return None;
            }
            let ref_m = mat_from_vec(tile, tile, &ref_t).ok()?;
            let mut resp = 0.0;
            let p = imgproc::phase_correlate(&ref_m, &mov_t, &win, Some(&mut resp)).ok()?;
            let lim = tile as f64 / 4.0;
            if resp < 0.08 || p.x.abs() > lim || p.y.abs() > lim {
                return None;
            }
            Some((cx as f64, cy as f64, ddx + p.x, ddy + p.y, resp * sd))
        })
        .collect();
    Ok(results.into_iter().flatten().collect())
}

// ------------------------------------------------------------ carriage

/// Weighted Gaussian smoothing; where there is little weight the value is
/// carried in from a four times wider neighbourhood.
fn wblur(values: &[&[f32]], w: &[f32], rows: usize, cols: usize, sigma: f64) -> Res<Vec<Vec<f32>>> {
    let k = ((sigma / 6.0) as usize).max(1);
    let (sw, sh) = ((cols / k).max(1) as i32, (rows / k).max(1) as i32);
    let small = |v: &[f32]| -> Res<Mat> {
        let m = mat_from_vec(rows, cols, v)?;
        if k > 1 {
            resize_to(&m, sw, sh, imgproc::INTER_AREA)
        } else {
            Ok(m)
        }
    };
    let ws = small(w)?;
    let ws1 = blur(&ws, sigma / k as f64)?;
    let ws2 = blur(&ws, 4.0 * sigma / k as f64)?;
    let w1 = f32s(&ws1)?;
    let w2 = f32s(&ws2)?;
    let mut pos: Vec<f32> = w1.iter().copied().filter(|&v| v > 0.0).collect();
    let c = if pos.is_empty() { 1.0 } else { 0.05 * percentile(&mut pos, 50.0) };
    let tiny = 1e-6 * c;
    let mut out = Vec::new();
    for v in values {
        let vw: Vec<f32> = v.iter().zip(w).map(|(a, b)| a * b).collect();
        let s = small(&vw)?;
        let r1 = blur(&s, sigma / k as f64)?;
        let r2 = blur(&s, 4.0 * sigma / k as f64)?;
        let (a1, a2) = (f32s(&r1)?, f32s(&r2)?);
        let r: Vec<f32> = (0..a1.len())
            .map(|i| {
                let alpha = w1[i] / (w1[i] + c);
                alpha * a1[i] / (w1[i] + tiny) + (1.0 - alpha) * a2[i] / (w2[i] + tiny)
            })
            .collect();
        let rm = mat_from_vec(sh as usize, sw as usize, &r)?;
        let full = if k > 1 { resize_to(&rm, cols as i32, rows as i32, imgproc::INTER_LINEAR)? } else { rm };
        out.push(f32s(&full)?.to_vec());
    }
    Ok(out)
}

fn gaussian_1d(v: &[f64], sigma: f64) -> Vec<f64> {
    let r = (4.0 * sigma).ceil() as i64;
    let kern: Vec<f64> = (-r..=r).map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f64 = kern.iter().sum();
    let n = v.len() as i64;
    (0..n)
        .map(|i| {
            let mut acc = 0.0;
            for (k, kv) in kern.iter().enumerate() {
                let j = i + k as i64 - r;
                if j >= 0 && j < n {
                    acc += kv * v[j as usize];
                }
            }
            acc / s
        })
        .collect()
}

/// like gaussian_1d but with mirrored borders (cv2.BORDER_REFLECT)
fn gaussian_1d_reflect(v: &[f64], sigma: f64) -> Vec<f64> {
    let r = (4.0 * sigma).ceil() as i64;
    let kern: Vec<f64> = (-r..=r).map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f64 = kern.iter().sum();
    let n = v.len() as i64;
    let at = |mut j: i64| {
        // reflect: ... 2 1 0 | 0 1 2 ... n-1 | n-1 n-2 ...
        loop {
            if j < 0 {
                j = -j - 1;
            } else if j >= n {
                j = 2 * n - j - 1;
            } else {
                return v[j as usize];
            }
        }
    };
    (0..n).map(|i| kern.iter().enumerate().map(|(k, kv)| kv * at(i + k as i64 - r)).sum::<f64>() / s).collect()
}

/// Dense optical flow from `c8` to `m8` (8-bit, bh x bw), as two planes.
///
/// OpenCV's DIS flow needs about 190 bytes of working memory per pixel (5 GB
/// for a 27-megapixel frame).  Above `max_px` the frame is therefore processed
/// in tiles that overlap by `halo` pixels, and only the middle of each tile is
/// kept.  The flow here is at most a few pixels, far less than the halo, so the
/// tiles agree with the whole-frame result to a small fraction of a pixel.
fn dis_flow(
    c8: &Mat,
    m8: &Mat,
    bh: usize,
    bw: usize,
    max_px: usize,
    halo: usize,
) -> Res<(Vec<f32>, Vec<f32>)> {
    // A fresh flow object each time: it keeps its working memory for as long as
    // it lives (and OpenCV's collectGarbage() on it crashes).
    let mut dis = cv(video::DISOpticalFlow::create(video::DISOpticalFlow_PRESET_MEDIUM))?;
    cv(dis.set_finest_scale(0))?;
    cv(dis.set_patch_size(16))?;
    cv(dis.set_patch_stride(4))?;
    let mut fx = vec![0f32; bh * bw];
    let mut fy = vec![0f32; bh * bw];
    let side = (max_px as f64).sqrt();
    let (nx, ny) = if bh * bw <= max_px { (1, 1) } else { ((bw as f64 / side).ceil() as usize, (bh as f64 / side).ceil() as usize) };
    for ty in 0..ny {
        for tx in 0..nx {
            // the part of the frame this tile is responsible for ...
            let (x0, x1) = (tx * bw / nx, (tx + 1) * bw / nx);
            let (y0, y1) = (ty * bh / ny, (ty + 1) * bh / ny);
            // ... and what the flow gets to see
            let (ex0, ex1) = (x0.saturating_sub(halo), (x1 + halo).min(bw));
            let (ey0, ey1) = (y0.saturating_sub(halo), (y1 + halo).min(bh));
            let rect = core::Rect::new(ex0 as i32, ey0 as i32, (ex1 - ex0) as i32, (ey1 - ey0) as i32);
            let mut flow = Mat::default();
            if nx * ny == 1 {
                cv(dis.calc(c8, m8, &mut flow))?;
            } else {
                // tiles are copied so that the flow sees ordinary, continuous images
                let a = cv(cv(Mat::roi(c8, rect))?.try_clone())?;
                let b = cv(cv(Mat::roi(m8, rect))?.try_clone())?;
                cv(dis.calc(&a, &b, &mut flow))?;
            }
            let tw = ex1 - ex0;
            let fl = cv(flow.data_typed::<core::Vec2f>())?;
            for y in y0..y1 {
                let src = &fl[(y - ey0) * tw + (x0 - ex0)..(y - ey0) * tw + (x1 - ex0)];
                for (k, v) in src.iter().enumerate() {
                    fx[y * bw + x0 + k] = v[0];
                    fy[y * bw + x0 + k] = v[1];
                }
            }
        }
    }
    Ok((fx, fy))
}

/// How much of a correction measured against the other scans to apply.
///
/// Each scan is measured against the average of the *other* scans.  Where the
/// others share the error being measured, that overstates the scan's distance
/// from the average of all n by n/(n-1), and applying it in full overshoots by
/// 1/(n-1): a third with four scans, but all of it with two, which then swap
/// places every round and never meet.  The step that lands on the average is
/// (n-1)/n of the measurement.
fn shared_gain(n: usize) -> f64 {
    (n as f64 - 1.0) / n as f64
}

/// The step for each scan's carriage profile.  `axes` are the scans' carriage
/// directions in the output frame (unit vectors).  A profile is shared only with
/// scans whose carriage runs along the same line, as placed or turned half
/// round; a scan with no such partner is measured against scans that cannot
/// move along its axis line by line, so its measurement is applied in full.
fn profile_gains(axes: &[(f64, f64)]) -> Vec<f64> {
    let n = axes.len();
    (0..n)
        .map(|i| {
            let partner = (0..n).any(|j| j != i && (axes[i].0 * axes[j].0 + axes[i].1 * axes[j].1).abs() > 0.866);
            if partner {
                shared_gain(n)
            } else {
                1.0
            }
        })
        .collect()
}

struct CarriageResult {
    shifts: Vec<(f64, f64)>,
    left: f64,
}

#[allow(clippy::too_many_arguments)]
fn carriage_refine(
    scans: &[Mat],
    warps: &mut [ScanWarp],
    mask: &[bool],
    fh: usize,
    fw: usize,
    fs: f64,
    dpi: f64,
    progress: &dyn Fn(f32, &str),
    log: &mut Vec<String>,
) -> Res<CarriageResult> {
    let n = scans.len();
    let rounds = 4;
    let s0 = cv(scans[0].size())?;
    let (w0, h0) = (s0.width as f64, s0.height as f64);
    let hmax = scans.iter().map(|s| s.rows()).max().unwrap_or(0) as f64;
    let (mut ymin, mut ymax, mut xmin, mut xmax) = (usize::MAX, 0, usize::MAX, 0);
    for y in 0..fh {
        for x in 0..fw {
            if mask[y * fw + x] {
                ymin = ymin.min(y);
                ymax = ymax.max(y);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
            }
        }
    }
    // The whole output frame: the photo's edges are good alignment detail, and the
    // correction must be defined everywhere the output reaches.
    let _ = (ymin, ymax, xmin, xmax);
    let (fy0, fy1, fx0, fx1) = (0usize, fh, 0usize, fw);
    let to_full = |v: f64| (v + 0.5) / fs - 0.5;
    let x0f = to_full(fx0 as f64 - 0.5).max(0.0);
    let x1f = to_full(fx1 as f64 - 0.5).min(w0 - 1.0);
    let y0f = to_full(fy0 as f64 - 0.5).max(0.0);
    let y1f = to_full(fy1 as f64 - 0.5).min(h0 - 1.0);
    let cs = (CARRIAGE_WORK_PX / ((x1f - x0f) * (y1f - y0f)).max(1.0)).sqrt().min(1.0);
    let bh = ((y1f - y0f) * cs).round() as usize;
    let bw = ((x1f - x0f) * cs).round() as usize;
    let kk = fs / cs; // fine pixels per working pixel
    let cxs: Vec<f64> = (0..bw).map(|c| ((x0f + (c as f64 + 0.5) / cs - 0.5) + 0.5) * fs - 0.5).collect();
    let cys: Vec<f64> = (0..bh).map(|r| ((y0f + (r as f64 + 0.5) / cs - 0.5) + 0.5) * fs - 0.5).collect();

    let ls = cs * dpi / 1200.0;
    let (s_lo, s_hi) = ((0.8 * ls).max(0.6), (12.0 * ls).max(4.0));
    let sig_prof = (8.0 * ls).max(1.5);
    let sig_res = (150.0 * ls).max(20.0);
    let sig_rob = (60.0 * ls).max(10.0);
    let jscale = cs / fs;
    let ll = (hmax * cs * 1.05).ceil() as usize + 2;

    let step = 8usize;
    let gh = fh.div_ceil(step);
    let gw = fw.div_ceil(step);
    let gcol: Vec<Option<usize>> = (0..gw)
        .map(|c| {
            let v = ((to_full(c as f64 * step as f64 + (step as f64 - 1.0) / 2.0) - x0f + 0.5) * cs - 0.5).round();
            (v >= 0.0 && (v as usize) < bw).then_some(v as usize)
        })
        .collect();
    let grow: Vec<Option<usize>> = (0..gh)
        .map(|r| {
            let v = ((to_full(r as f64 * step as f64 + (step as f64 - 1.0) / 2.0) - y0f + 0.5) * cs - 0.5).round();
            (v >= 0.0 && (v as usize) < bh).then_some(v as usize)
        })
        .collect();
    for w in warps.iter_mut() {
        w.jitter.get_or_insert_with(|| vec![0.0; ll]);
        w.jscale = jscale;
        if w.resid.is_none() {
            w.resid = Some(vec![[0.0; 2]; gh * gw]);
            w.gh = gh;
            w.gw = gw;
            w.rstep = step as f64;
        }
    }

    // each scan's part that the output area needs, at the working scale (grey)
    let mut crops: Vec<(Mat, f64, f64)> = Vec::new();
    {
        let sx: Vec<f64> = cxs.iter().step_by(32).copied().collect();
        let sy: Vec<f64> = cys.iter().step_by(32).copied().collect();
        for (i, scan) in scans.iter().enumerate() {
            progress(0.15 * (i + 1) as f32 / n as f32, &format!("Preparing scan {} for the carriage correction", i + 1));
            let (mut pxmin, mut pxmax, mut pymin, mut pymax) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for &y in &sy {
                for &x in &sx {
                    let (px, py, _) = warps[i].map(x, y);
                    let (px, py) = (to_full(px), to_full(py));
                    pxmin = pxmin.min(px);
                    pxmax = pxmax.max(px);
                    pymin = pymin.min(py);
                    pymax = pymax.max(py);
                }
            }
            let ssz = cv(scan.size())?;
            // room for the corrections still to come, which are a length on
            // the print: more pixels at a higher resolution
            let m = 48.0 * (dpi / 1200.0).max(1.0);
            let ox0 = (pxmin.floor() - m).max(0.0) as i32;
            let ox1 = ((pxmax.ceil() + m) as i32).min(ssz.width);
            let oy0 = (pymin.floor() - m).max(0.0) as i32;
            let oy1 = ((pymax.ceil() + m) as i32).min(ssz.height);
            if ox1 <= ox0 || oy1 <= oy0 {
                return Err("scan does not cover the photo".into());
            }
            let roi = cv(Mat::roi(scan, core::Rect::new(ox0, oy0, ox1 - ox0, oy1 - oy0)))?;
            // At full working scale the grey image is kept in its original 8 bits
            // (a quarter of the memory) and converted to float when it is used,
            // which gives exactly the same values.
            let mut g8 = Mat::default();
            cv(cvx::cvt_color(&roi, &mut g8, imgproc::COLOR_RGB2GRAY, 0))?;
            let g = if cs < 1.0 {
                let mut f = Mat::default();
                cv(g8.convert_to(&mut f, CV_32F, 1.0 / 255.0, 0.0))?;
                drop(g8);
                resize_to(
                    &f,
                    (((ox1 - ox0) as f64 * cs).round() as i32).max(1),
                    (((oy1 - oy0) as f64 * cs).round() as i32).max(1),
                    imgproc::INTER_AREA,
                )?
            } else {
                g8
            };
            crops.push((g, ox0 as f64, oy0 as f64));
        }
    }

    let warp_c = |w: &ScanWarp, crop: &(Mat, f64, f64), want: bool| -> Res<(Mat, Vec<f32>)> {
        let (img, ox, oy) = crop;
        let (mx, my, p0) = build_maps(
            &cxs,
            &cys,
            |x, y| {
                let (px, py, p0y) = w.map(x, y);
                (((px + 0.5) / fs - 0.5 - ox + 0.5) * cs - 0.5, ((py + 0.5) / fs - 0.5 - oy + 0.5) * cs - 0.5, p0y)
            },
            want,
        )?;
        if img.typ() == CV_8U {
            let mut f = Mat::default();
            cv(img.convert_to(&mut f, CV_32F, 1.0 / 255.0, 0.0))?;
            Ok((remap(&f, &mx, &my, imgproc::INTER_CUBIC, core::BORDER_CONSTANT, f64::NAN)?, p0))
        } else {
            Ok((remap(img, &mx, &my, imgproc::INTER_CUBIC, core::BORDER_CONSTANT, f64::NAN)?, p0))
        }
    };

    let npx = bh * bw;
    let ram = total_ram();
    let flow_tile_px =
        flow_tile_limit(ram, std::env::var("MULTIPASS_FLOW_TILE_MP").ok().and_then(|v| v.parse::<f64>().ok()));
    let ram_text = ram.map_or("memory size unknown".to_string(), |b| format!("{:.0} GB of memory", b as f64 / 1073741824.0));
    log.push(if flow_tile_px >= npx {
        format!("optical flow on the whole frame ({ram_text})")
    } else {
        format!("optical flow in {:.0}-megapixel tiles ({ram_text})", flow_tile_px as f64 / 1e6)
    });
    let mut support = vec![vec![false; ll]; n];
    let total = (rounds * n) as f32;
    let mut region: Option<Vec<bool>> = None;
    let (mut e_first, mut e_prev): (Option<f64>, Option<f64>) = (None, None);
    let mut snap: Vec<(Vec<f64>, Vec<[f32; 2]>)> = Vec::new();
    for rnd in 0..=rounds {
        let mut acc = vec![0f64; npx];
        let mut cnt = vec![0u8; npx];
        let mut b1 = vec![0f32; npx];
        let mut b2 = vec![0f32; npx];
        for i in 0..n {
            let (wv, _) = warp_c(&warps[i], &crops[i], false)?;
            let d = f32s(&wv)?;
            let mut sum = 0.0;
            let mut num = 0usize;
            for (k, &v) in d.iter().enumerate() {
                if v.is_finite() {
                    acc[k] += v as f64;
                    cnt[k] += 1;
                    sum += v as f64;
                    num += 1;
                }
            }
            let fillv = if num > 0 { (sum / num as f64) as f32 } else { 0.0 };
            let filled: Vec<f32> = d.iter().map(|&v| if v.is_finite() { v } else { fillv }).collect();
            drop(wv);
            let b = bandpass(&view(bh, bw, &filled)?, s_lo, s_hi)?;
            drop(filled);
            for (k, &v) in f32s(&b)?.iter().enumerate() {
                b1[k] += v;
                b2[k] += v * v;
            }
        }
        // Detail every scan shows is part of the photo; detail only some scans
        // show (dust or marks on the glass) must not steer the alignment.
        let nf = n as f32;
        // every buffer in this stage is the size of the photo at full resolution,
        // so each is freed as soon as it has been used
        let agree: Vec<f32> = {
            let mean_b: Vec<f32> = b1.iter().map(|v| v / nf).collect();
            let spread = {
                let raw: Vec<f32> = (0..npx).map(|k| (b2[k] / nf - mean_b[k] * mean_b[k]).max(0.0)).collect();
                let out = blur(&view(bh, bw, &raw)?, 3.0)?;
                out
            };
            let common = {
                let raw: Vec<f32> = mean_b.iter().map(|v| v * v).collect();
                drop(mean_b);
                let out = blur(&view(bh, bw, &raw)?, 3.0)?;
                out
            };
            let (sp, cm) = (f32s(&spread)?, f32s(&common)?);
            (0..npx)
                .map(|k| {
                    let r = sp[k] / (cm[k] + 1e-12);
                    1.0 / (1.0 + r * r)
                })
                .collect()
        };
        mem_mark("accumulation pass over all scans");
        let reg = region.get_or_insert_with(|| cnt.iter().map(|&c| c as usize == n).collect());
        let e: f64 = (0..npx).filter(|&k| reg[k]).map(|k| (b2[k] - b1[k] * b1[k] / nf) as f64).sum();
        drop(b1);
        drop(b2);
        if let Some(p) = e_prev {
            if e > p * 0.999 {
                for (w, (j, r)) in warps.iter_mut().zip(snap.drain(..)) {
                    w.jitter = Some(j);
                    w.resid = Some(r);
                }
                log.push(format!("carriage round {rnd} did not improve the match; undone"));
                break;
            }
        }
        e_first.get_or_insert(e);
        e_prev = Some(e);
        if rnd == rounds {
            break;
        }
        snap = warps.iter().map(|w| (w.jitter.clone().unwrap(), w.resid.clone().unwrap())).collect();

        let mut updates: Vec<Option<(Vec<f64>, Vec<[f32; 2]>, Vec<[f32; 2]>)>> = Vec::new();
        let mut moved = Vec::new();
        for i in 0..n {
            progress(
                0.15 + 0.85 * (rnd * n + i + 1) as f32 / total,
                &format!("Correcting carriage wobble in scan {}", i + 1),
            );
            let (wv, p0y) = warp_c(&warps[i], &crops[i], true)?;
            let use_: Vec<bool> = {
                let d = f32s(&wv)?;
                (0..npx).map(|k| d[k].is_finite() && cnt[k] as usize == n).collect()
            };
            mem_mark("warp of the scan");
            let nuse = use_.iter().filter(|&&u| u).count();
            if nuse < 10000 {
                updates.push(None);
                continue;
            }
            // band-passed consensus of the other scans (cb) and this scan (mb)
            let (cb, mb) = {
                let d = f32s(&wv)?;
                let cons_at =
                    |k: usize| ((acc[k] - if d[k].is_finite() { d[k] as f64 } else { 0.0 }) / (n - 1) as f64) as f32;
                let fill = (0..npx).filter(|&k| use_[k]).map(|k| cons_at(k) as f64).sum::<f64>() / nuse as f64;
                let cb = {
                    let cfill: Vec<f32> = (0..npx).map(|k| if use_[k] { cons_at(k) } else { fill as f32 }).collect();
                    let out = bandpass(&view(bh, bw, &cfill)?, s_lo, s_hi)?;
                    out
                };
                let mb = {
                    let mfill: Vec<f32> = (0..npx).map(|k| if use_[k] { d[k] } else { fill as f32 }).collect();
                    let out = bandpass(&view(bh, bw, &mfill)?, s_lo, s_hi)?;
                    out
                };
                (cb, mb)
            };
            drop(wv);
            mem_mark("band-pass of consensus and scan");
            let sc = {
                let cbd = f32s(&cb)?;
                let mut absb: Vec<f32> = (0..npx).filter(|&k| use_[k]).map(|k| cbd[k].abs()).collect();
                100.0 / (percentile(&mut absb, 99.0) as f64 + 1e-9)
            };
            // cons(q) ~ scan_i(map(q + flow(q)))
            let (mut fx, mut fy) = {
                let mut c8 = Mat::default();
                let mut m8 = Mat::default();
                cv(cb.convert_to(&mut c8, CV_8U, sc, 128.0))?;
                cv(mb.convert_to(&mut m8, CV_8U, sc, 128.0))?;
                drop(mb);
                mem_mark("before optical flow");
                let (mut fx, mut fy) = dis_flow(&c8, &m8, bh, bw, flow_tile_px, FLOW_HALO)?;
                mem_mark("optical flow (DIS)");
                drop(c8);
                drop(m8);
                fx.iter_mut().for_each(|v| *v *= kk as f32);
                fy.iter_mut().for_each(|v| *v *= kk as f32);
                (fx, fy)
            };
            let mut wt: Vec<f32> = {
                let g2: Vec<f32> = {
                    let mut gx = Mat::default();
                    let mut gy = Mat::default();
                    cv(imgproc::sobel(&cb, &mut gx, CV_32F, 1, 0, 3, 1.0, 0.0, core::BORDER_DEFAULT))?;
                    cv(imgproc::sobel(&cb, &mut gy, CV_32F, 0, 1, 3, 1.0, 0.0, core::BORDER_DEFAULT))?;
                    let (gxd, gyd) = (f32s(&gx)?, f32s(&gy)?);
                    (0..npx).map(|k| gxd[k] * gxd[k] + gyd[k] * gyd[k]).collect()
                };
                drop(cb);
                let g2b = blur(&view(bh, bw, &g2)?, 4.0)?;
                drop(g2);
                let use8: Vec<u8> = use_.iter().map(|&u| u as u8).collect();
                let um = cv(Mat::new_rows_cols_with_data(bh as i32, bw as i32, &use8))?;
                let mut inner = Mat::default();
                let kern = cv(Mat::new_rows_cols_with_default(9, 9, CV_8UC1, Scalar::all(1.0)))?;
                cv(imgproc::erode(&um, &mut inner, &kern, Point::new(-1, -1), 1, core::BORDER_CONSTANT, Scalar::all(0.0)))?;
                let innerd = cv(inner.data_typed::<u8>())?;
                let g2bd = f32s(&g2b)?;
                (0..npx).map(|k| if innerd[k] != 0 { g2bd[k] * agree[k] } else { 0.0 }).collect()
            };
            mem_mark("weights");
            let wmax = wt.iter().copied().fold(0.0f32, f32::max) + 1e-12;
            wt.iter_mut().for_each(|v| *v /= wmax);
            // drop flow that wandered in flat areas
            {
                let sm = wblur(&[&fx, &fy], &wt, bh, bw, sig_rob)?;
                for k in 0..npx {
                    if (fx[k] - sm[0][k]).hypot(fy[k] - sm[1][k]) >= 3.0 * kk as f32 {
                        wt[k] = 0.0;
                    }
                }
            }
            mem_mark("outlier check (smoothing)");
            let a = warps[i].m;
            let det = a[0][0] * a[1][1] - a[0][1] * a[1][0];
            let (ainv01, ainv11) = (-a[0][1] / det, a[0][0] / det);
            let mut num = vec![0f64; ll];
            let mut den = vec![0f64; ll];
            for k in 0..npx {
                if wt[k] > 0.0 {
                    let u = a[1][0] * fx[k] as f64 + a[1][1] * fy[k] as f64; // along the scan's y axis
                    let b = ((p0y[k] as f64 * jscale).round().max(0.0) as usize).min(ll - 1);
                    num[b] += u * wt[k] as f64;
                    den[b] += wt[k] as f64;
                }
            }
            let num_s = gaussian_1d(&num, sig_prof);
            let den_s = gaussian_1d(&den, sig_prof);
            let mut pos: Vec<f32> = den_s.iter().filter(|&&v| v > 0.0).map(|&v| v as f32).collect();
            if pos.is_empty() {
                updates.push(None);
                continue;
            }
            // lines with enough measurements; beyond them the profile is held constant
            let thr = 0.2 * percentile(&mut pos, 90.0) as f64;
            let sup: Vec<bool> = den_s.iter().map(|&v| v > thr).collect();
            let idx: Vec<usize> = (0..ll).filter(|&b| sup[b]).collect();
            if idx.is_empty() {
                updates.push(None);
                continue;
            }
            let mut prof = vec![0f64; ll];
            {
                let vals: Vec<f64> = idx.iter().map(|&b| num_s[b] / den_s[b]).collect();
                let mut j = 0;
                for (b, p) in prof.iter_mut().enumerate() {
                    while j + 1 < idx.len() && idx[j + 1] <= b {
                        j += 1;
                    }
                    *p = if b <= idx[0] {
                        vals[0]
                    } else if b >= idx[idx.len() - 1] {
                        vals[idx.len() - 1]
                    } else {
                        let (b0, b1) = (idx[j], idx[j + 1]);
                        let f = (b - b0) as f64 / (b1 - b0) as f64;
                        vals[j] * (1.0 - f) + vals[j + 1] * f
                    };
                }
            }
            moved.push(idx.iter().map(|&b| prof[b].abs()).sum::<f64>() / idx.len() as f64);
            // keep only what changes faster than the 2-D remainder can follow
            let slow = gaussian_1d_reflect(&prof, 3.0 * sig_res);
            prof.iter_mut().zip(&slow).for_each(|(p, s)| *p -= s);
            for b in 0..ll {
                support[i][b] |= sup[b];
            }
            // what the profile does not explain, in output coordinates, very smooth
            let mut ppx = vec![0f32; npx];
            let mut ppy = vec![0f32; npx];
            for k in 0..npx {
                let pj = interp(p0y[k] as f64 * jscale, &prof);
                ppx[k] = (ainv01 * pj) as f32;
                ppy[k] = (ainv11 * pj) as f32;
                // the flow becomes the remainder, in place
                fx[k] -= ppx[k];
                fy[k] -= ppy[k];
            }
            drop(p0y);
            let mut g = vec![[0f32; 2]; gh * gw];
            let mut gp = vec![[0f32; 2]; gh * gw];
            {
                let sr = wblur(&[&fx, &fy], &wt, bh, bw, sig_res)?;
                for r in 0..gh {
                    let Some(rr) = grow[r] else { continue };
                    for c in 0..gw {
                        let Some(cc) = gcol[c] else { continue };
                        let k = rr * bw + cc;
                        g[r * gw + c] = [sr[0][k], sr[1][k]];
                    }
                }
            }
            mem_mark("smooth remainder");
            drop(fx);
            drop(fy);
            drop(wt);
            {
                let usef: Vec<f32> = use_.iter().map(|&u| u as u8 as f32).collect();
                let sp = wblur(&[&ppx, &ppy], &usef, bh, bw, sig_res)?;
                for r in 0..gh {
                    let Some(rr) = grow[r] else { continue };
                    for c in 0..gw {
                        let Some(cc) = gcol[c] else { continue };
                        let k = rr * bw + cc;
                        gp[r * gw + c] = [sp[0][k], sp[1][k]];
                    }
                }
            }
            mem_mark("slow part of the profile");
            updates.push(Some((prof, g, gp)));
        }
        let good: Vec<&(Vec<f64>, Vec<[f32; 2]>, Vec<[f32; 2]>)> = updates.iter().flatten().collect();
        if good.is_empty() {
            break;
        }
        // The output geometry stays the average of the scans; this also stops
        // scans with the same carriage direction drifting together.
        let mut common = vec![[0f32; 2]; gh * gw];
        for u in &good {
            for k in 0..gh * gw {
                common[k][0] += (u.1[k][0] + u.2[k][0]) / good.len() as f32;
                common[k][1] += (u.1[k][1] + u.2[k][1]) / good.len() as f32;
            }
        }
        // See shared_gain: the smooth remainder is shared by all the scans, a
        // carriage profile only by the scans along the same line.
        let axes: Vec<(f64, f64)> = warps
            .iter()
            .map(|w| {
                let (ax, ay) = (w.m[1][0], w.m[1][1]);
                let len = ax.hypot(ay).max(1e-12);
                (ax / len, ay / len)
            })
            .collect();
        let pgain = profile_gains(&axes);
        let rgain = shared_gain(n) as f32;
        for (i, u) in updates.into_iter().enumerate() {
            let Some((prof, g, _)) = u else { continue };
            let j = warps[i].jitter.as_mut().unwrap();
            j.iter_mut().zip(&prof).for_each(|(a, b)| *a += pgain[i] * b);
            let r = warps[i].resid.as_mut().unwrap();
            for k in 0..gh * gw {
                r[k][0] += rgain * (g[k][0] - common[k][0]);
                r[k][1] += rgain * (g[k][1] - common[k][1]);
            }
        }
        let mean_moved = moved.iter().sum::<f64>() / moved.len().max(1) as f64;
        log.push(format!("carriage round {}: mean remaining shift {:.2} px", rnd + 1, mean_moved / fs));
    }
    let shifts = warps
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let j: Vec<f64> =
                w.jitter.as_ref().unwrap().iter().zip(&support[i]).filter(|(_, &s)| s).map(|(v, _)| v / fs).collect();
            if j.is_empty() {
                (0.0, 0.0)
            } else {
                (j.iter().fold(0.0f64, |a, v| a.max(v.abs())), (j.iter().map(|v| v * v).sum::<f64>() / j.len() as f64).sqrt())
            }
        })
        .collect();
    let left = match (e_first, e_prev) {
        (Some(a), Some(b)) if a > 0.0 => b / a,
        _ => 1.0,
    };
    Ok(CarriageResult { shifts, left })
}

// ------------------------------------------------------------ main routine

/// Roughly the most memory (bytes) the app holds while combining `n` scans of
/// `px` pixels each, the scans themselves included.
pub fn memory_needed(n: usize, px: u64) -> u64 {
    let forced = std::env::var("MULTIPASS_FLOW_TILE_MP").ok().and_then(|v| v.parse::<f64>().ok());
    memory_model(n, px, flow_tile_limit(total_ram(), forced) == usize::MAX)
}

/// `whole_frame`: the optical flow runs on the whole frame, not in tiles.
fn memory_model(n: usize, px: u64, whole_frame: bool) -> u64 {
    let n = n as u64;
    // Aligning: the scans (3 bytes a pixel) and working images.  Those stop
    // growing at CARRIAGE_WORK_PX, whatever the resolution.
    let work = px.min(CARRIAGE_WORK_PX as u64) * if whole_frame { FLOW_BYTES_WHOLE } else { FLOW_BYTES_TILED };
    let aligning = 3 * n * px + work;
    // Resampling and combining: the scans, each one resampled to 16 bits (6)
    // with its map of usable pixels (1), and the result (6).
    let combining = px * (3 * n + 7 * n + 6);
    aligning.max(combining) + (1 << 30)
}

/// The carriage correction works on at most this many pixels.
const CARRIAGE_WORK_PX: f64 = 30e6;
/// Measured peak of the carriage correction per working pixel, with the
/// optical flow on the whole frame and in tiles.
const FLOW_BYTES_WHOLE: u64 = 260;
const FLOW_BYTES_TILED: u64 = 100;

/// Align and combine `scans` (RGB, 8 bits per sample, any sizes) taken at `dpi`.
/// The output has the geometry of the first scan.
pub fn combine(scans: &[Mat], dpi: f64, progress: &dyn Fn(f32, &str)) -> Res<Combined> {
    let n = scans.len();
    if n < 2 {
        return Err("need at least two scans".into());
    }
    let mut report = Vec::new();
    progress(0.0, "Preparing scans");
    let s0 = cv(scans[0].size())?;
    let (w0, h0) = (s0.width as usize, s0.height as usize);
    let maxdim = scans.iter().map(|s| s.rows().max(s.cols())).max().unwrap() as f64;
    let fine_scale = (6000.0 / maxdim).min(1.0);
    let coarse_scale = (1600.0 / maxdim).min(1.0);
    let ecc_scale = (2000.0 / maxdim).min(1.0);

    let mut fine = Vec::new();
    for (i, s) in scans.iter().enumerate() {
        fine.push(resize_area(&gray_f32(s)?, fine_scale)?);
        progress(0.10 * (i + 1) as f32 / n as f32, "Preparing scans");
    }
    let fsz = cv(fine[0].size())?;
    let (fh, fw) = (fsz.height as usize, fsz.width as usize);
    let mut models: Vec<M23> = vec![[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]; n];

    // ------------------------------------------------ 1. coarse: SIFT similarity
    let mut sift = cv(features2d::SIFT::create(8000, 3, 0.04, 10.0, 1.6, false))?;
    let coarse: Vec<Mat> = fine.iter().map(|g| to_u8(&resize_area(g, coarse_scale / fine_scale)?)).collect::<Res<_>>()?;
    let mut kp1 = Vector::<core::KeyPoint>::new();
    let mut d1 = Mat::default();
    cv(sift.detect_and_compute(&coarse[0], &core::no_array(), &mut kp1, &mut d1, false))?;
    let ref_kd = (kp1, d1);
    let mut hull_pts: Vec<Point2f> = Vec::new();
    for i in 1..n {
        progress(0.10 + 0.15 * i as f32 / n as f32, &format!("Finding the photo in scan {}", i + 1));
        let Some((m, pts)) = sift_similarity(&mut sift, &ref_kd, &coarse[i], &mut report)? else {
            return Err(format!(
                "Could not find the photo in scan {}. Is it the same picture, and does it have enough detail?",
                i + 1
            ));
        };
        let ang = m[1][0].atan2(m[0][0]).to_degrees();
        report.push(format!("scan {}: rotated {:+.1} deg, {} matching features", i + 1, ang, pts.len()));
        models[i] = rescale_affine(&m, coarse_scale, fine_scale);
        let k = (fine_scale / coarse_scale) as f32;
        hull_pts.extend(pts.iter().map(|&(x, y)| Point2f::new(x * k, y * k)));
    }
    // the area of the photo: hull of all matched features, slightly grown
    let mut mask_u8 = cv(Mat::new_rows_cols_with_default(fh as i32, fw as i32, CV_8UC1, Scalar::all(0.0)))?;
    {
        let pts: Vector<Point> = hull_pts.iter().map(|p| Point::new(p.x.round() as i32, p.y.round() as i32)).collect();
        let mut hull = Vector::<Point>::new();
        cv(cvx::convex_hull(&pts, &mut hull, false, true))?;
        cv(imgproc::fill_convex_poly(&mut mask_u8, &hull, Scalar::all(1.0), imgproc::LINE_8, 0))?;
        let grow = 3.max((0.01 * fh.max(fw) as f64) as i32);
        let se = cv(imgproc::get_structuring_element(
            imgproc::MORPH_ELLIPSE,
            Size::new(2 * grow + 1, 2 * grow + 1),
            Point::new(-1, -1),
        ))?;
        let mut grown = Mat::default();
        cv(imgproc::dilate(&mask_u8, &mut grown, &se, Point::new(-1, -1), 1, core::BORDER_CONSTANT, Scalar::all(0.0)))?;
        mask_u8 = grown;
    }
    let mask_fine: Vec<bool> = cv(mask_u8.data_typed::<u8>())?.iter().map(|&v| v != 0).collect();

    // ------------------------------------------------ 2. rigid: ECC affine
    {
        let es = ecc_scale / fine_scale;
        let ecc: Vec<Mat> = fine.iter().map(|g| blur(&resize_area(g, es)?, 1.0)).collect::<Res<_>>()?;
        let esz = cv(ecc[0].size())?;
        let mut m255 = Mat::default();
        cv(mask_u8.convert_to(&mut m255, CV_8U, 255.0, 0.0))?;
        let em = resize_to(&m255, esz.width, esz.height, imgproc::INTER_AREA)?;
        let mut emask = Mat::default();
        cv(imgproc::threshold(&em, &mut emask, 127.0, 1.0, imgproc::THRESH_BINARY))?;
        // With a mask, ECC wants every image the same size: pad at the bottom and
        // right, which leaves all coordinates unchanged.
        let (pw, ph) = ecc.iter().fold((0, 0), |a, m| (a.0.max(m.cols()), a.1.max(m.rows())));
        let pad = |m: &Mat| -> Res<Mat> {
            let mut out = Mat::default();
            cv(core::copy_make_border(m, &mut out, 0, ph - m.rows(), 0, pw - m.cols(), core::BORDER_CONSTANT, Scalar::all(0.0)))?;
            Ok(out)
        };
        let ecc: Vec<Mat> = ecc.iter().map(pad).collect::<Res<_>>()?;
        let emask = pad(&emask)?;
        for i in 1..n {
            progress(0.25 + 0.15 * i as f32 / n as f32, &format!("Aligning scan {}", i + 1));
            let m_e = rescale_affine(&models[i], fine_scale, ecc_scale);
            match ecc_refine(&ecc[0], &ecc[i], &m_e, &emask) {
                Ok(r) => models[i] = rescale_affine(&r, ecc_scale, fine_scale),
                Err(e) => report.push(format!("scan {}: fine alignment did not converge ({e}), using feature alignment", i + 1)),
            }
        }
    }

    // ------------------------------------------------ 3. local: smooth warp to consensus
    let mut fields: Vec<Option<SmoothField>> = vec![None; n];
    {
        let tile = if fh.max(fw) > 2500 { 128 } else { 64 };
        let (mut ymin, mut ymax, mut xmin, mut xmax) = (usize::MAX, 0, usize::MAX, 0);
        for y in 0..fh {
            for x in 0..fw {
                if mask_fine[y * fw + x] {
                    ymin = ymin.min(y);
                    ymax = ymax.max(y);
                    xmin = xmin.min(x);
                    xmax = xmax.max(x);
                }
            }
        }
        let span = (xmax - xmin).max(ymax - ymin);
        let step = (tile / 2).max(span / 32);
        let (cx, cy, half) = (fw as f64 / 2.0, fh as f64 / 2.0, fw.max(fh) as f64 / 2.0);
        let bp: Vec<Mat> = fine.iter().map(|g| bandpass(g, 1.0, 12.0)).collect::<Res<_>>()?;
        for rnd in 0..2 {
            let mut acc = vec![0f64; fh * fw];
            let mut cnt = vec![0u8; fh * fw];
            for i in 0..n {
                let wv = warp_with(&fine[i], &ScanWarp::new(models[i], fields[i].clone()), (0, fh, 0, fw))?;
                for (k, &v) in f32s(&wv)?.iter().enumerate() {
                    if v.is_finite() {
                        acc[k] += v as f64;
                        cnt[k] += 1;
                    }
                }
            }
            let (s, c) = acc.iter().zip(&cnt).filter(|(_, &c)| c > 0).fold((0.0, 0usize), |a, (v, &c)| (a.0 + v / c as f64, a.1 + 1));
            let meanv = if c > 0 { s / c as f64 } else { 0.0 };
            let cons: Vec<f32> = (0..fh * fw).map(|k| if cnt[k] > 0 { (acc[k] / cnt[k] as f64) as f32 } else { meanv as f32 }).collect();
            let mut cons_bp = bandpass(&mat_from_vec(fh, fw, &cons)?, 1.0, 12.0)?;
            for (k, v) in f32s_mut(&mut cons_bp)?.iter_mut().enumerate() {
                if (cnt[k] as usize) < n {
                    *v = f32::NAN;
                }
            }
            let mut new_fields = Vec::new();
            for i in 0..n {
                progress(
                    0.40 + 0.12 * (rnd * n + i + 1) as f32 / (2 * n) as f32,
                    &format!("Matching detail in scan {}", i + 1),
                );
                let meas = tile_shifts(&cons_bp, &bp[i], &ScanWarp::new(models[i], fields[i].clone()), &mask_fine, tile, step)?;
                let deg: i32 = match meas.len() {
                    l if l >= 40 => 3,
                    l if l >= 20 => 2,
                    l if l >= 8 => 1,
                    _ => -1,
                };
                let f = if deg >= 0 { SmoothField::fit(&meas, deg as usize, cx, cy, half) } else { None };
                let mut f = f.unwrap_or_else(|| SmoothField::zero(cx, cy, half));
                f.bounds = (xmin as f64, ymin as f64, xmax as f64, ymax as f64);
                new_fields.push(f);
            }
            // remove the average warp: the output geometry is the consensus of all scans
            let mut mx = [0.0; NTERMS];
            let mut my = [0.0; NTERMS];
            for f in &new_fields {
                for k in 0..NTERMS {
                    mx[k] += f.cxf[k] / n as f64;
                    my[k] += f.cyf[k] / n as f64;
                }
            }
            for f in new_fields.iter_mut() {
                for k in 0..NTERMS {
                    f.cxf[k] -= mx[k];
                    f.cyf[k] -= my[k];
                }
            }
            fields = new_fields.into_iter().map(Some).collect();
        }
        for (i, f) in fields.iter().enumerate() {
            let mut big: f64 = 0.0;
            for y in (0..fh).step_by(8) {
                for x in (0..fw).step_by(8) {
                    if mask_fine[y * fw + x] {
                        let (dx, dy) = f.as_ref().unwrap().eval(x as f64, y as f64);
                        big = big.max(dx.hypot(dy));
                    }
                }
            }
            report.push(format!("scan {}: local correction up to {:.2} px", i + 1, big / fine_scale));
        }
    }

    let mut warps: Vec<ScanWarp> = (0..n).map(|i| ScanWarp::new(models[i], fields[i].clone())).collect();
    // The registration images are not needed any more; only their sizes are.
    let fine_sizes: Vec<(usize, usize)> = fine.iter().map(|g| (g.rows() as usize, g.cols() as usize)).collect();
    drop(fine);

    // ------------------------------------------------ 3b. carriage wobble
    if dpi >= 400.0 {
        let mut log = Vec::new();
        let mut trial = warps.clone();
        match carriage_refine(scans, &mut trial, &mask_fine, fh, fw, fine_scale, dpi, &|f, t| progress(0.52 + 0.13 * f, t), &mut log) {
            Ok(r) => {
                report.extend(log);
                if r.left >= 0.999 {
                    report.push("carriage correction: no improvement found, not applied".into());
                } else {
                    for (i, (mx, rms)) in r.shifts.iter().enumerate() {
                        report.push(format!("scan {}: carriage correction up to {:.2} px (rms {:.2} px)", i + 1, mx, rms));
                    }
                    report.push(format!(
                        "carriage correction: disagreement between scans reduced by {:.0}%",
                        100.0 * (1.0 - r.left)
                    ));
                    warps = trial;
                }
            }
            Err(e) => report.push(format!("carriage correction skipped: {e}")),
        }
    } else {
        report.push(format!("carriage correction not needed at {dpi} dpi"));
    }

    // ------------------------------------------------ output box
    // Everything any scan covers.  The scans are cropped to the photo plus at most
    // 8 mm, so this is the whole photo with a little lid around it.  Each side of
    // the photo lies against an edge of the glass in one or two of the passes (no
    // margin, and a little hidden under the frame), so requiring more scans would
    // cut strips off the photo.  Near the edges the combination uses whichever
    // scans reach there; a corner that none reaches is white.
    let mut valid_count = vec![0usize; fh * fw];
    for i in 0..n {
        let ones = new_f32(fine_sizes[i].0, fine_sizes[i].1, 1.0)?;
        let wv = warp_with(&ones, &warps[i], (0, fh, 0, fw))?;
        for (k, &v) in f32s(&wv)?.iter().enumerate() {
            if v.is_finite() && v > 0.999 {
                valid_count[k] += 1;
            }
        }
    }
    let need = 1;
    let (mut ymin, mut ymax, mut xmin, mut xmax) = (usize::MAX, 0, usize::MAX, 0);
    for y in 0..fh {
        for x in 0..fw {
            let k = y * fw + x;
            if valid_count[k] >= need {
                ymin = ymin.min(y);
                ymax = ymax.max(y);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
            }
        }
    }
    if ymin == usize::MAX {
        return Err("the scans do not overlap".into());
    }
    let inv = 1.0 / fine_scale;
    let x0 = ((xmin as f64 * inv).floor() as usize).min(w0);
    let y0 = ((ymin as f64 * inv).floor() as usize).min(h0);
    let x1 = (((xmax + 1) as f64 * inv).ceil() as usize).min(w0);
    let y1 = (((ymax + 1) as f64 * inv).ceil() as usize).min(h0);
    let (oh, ow) = (y1 - y0, x1 - x0);
    let npx = oh * ow;

    // ------------------------------------------------ 4. resample at full resolution
    let d = scale_matrix(fine_scale);
    let dinv = inv33(&d);
    let mut stack: Vec<Vec<u16>> = Vec::with_capacity(n);
    let mut valid: Vec<Vec<bool>> = Vec::with_capacity(n);
    // In tiles, each resampled from just the part of the scan it needs.  OpenCV's
    // remap takes no image of 32767 pixels or more on a side, which a 4800 dpi
    // scan of a 7 inch photo already is; and only that part of the scan is
    // widened to 16 bits.  The result is the same as resampling the whole scan.
    for i in 0..n {
        progress(0.65 + 0.20 * i as f32 / n as f32, &format!("Resampling scan {}", i + 1));
        let (sw, sh) = (scans[i].cols() as f32, scans[i].rows() as f32);
        let mut out = vec![0u16; npx * 3];
        let mut ok = vec![false; npx];
        let xs: Vec<f64> = (x0..x1).map(|x| d[0][0] * x as f64 + d[0][2]).collect();
        let ys: Vec<f64> = (y0..y1).map(|y| d[1][1] * y as f64 + d[1][2]).collect();
        for ty in (0..oh).step_by(RESAMPLE_TILE) {
            let ey = (ty + RESAMPLE_TILE).min(oh);
            for tx in (0..ow).step_by(RESAMPLE_TILE) {
                let ex = (tx + RESAMPLE_TILE).min(ow);
                let tw = ex - tx;
                let (mut mx, mut my, _) = build_maps(
                    &xs[tx..ex],
                    &ys[ty..ey],
                    |x, y| {
                        let (px, py, _) = warps[i].map(x, y);
                        (dinv[0][0] * px + dinv[0][2], dinv[1][1] * py + dinv[1][2], 0.0)
                    },
                    false,
                )?;
                {
                    let (mxd, myd) = (f32s(&mx)?, f32s(&my)?);
                    for r in 0..ey - ty {
                        let o = (ty + r) * ow + tx;
                        for c in 0..tw {
                            let k = r * tw + c;
                            ok[o + c] = mxd[k] >= 1.0 && myd[k] >= 1.0 && mxd[k] <= sw - 2.0 && myd[k] <= sh - 2.0;
                        }
                    }
                }
                let (bx, bw) = source_span(f32s(&mx)?, scans[i].cols());
                let (by, bh) = source_span(f32s(&my)?, scans[i].rows());
                f32s_mut(&mut mx)?.iter_mut().for_each(|v| *v -= bx as f32);
                f32s_mut(&mut my)?.iter_mut().for_each(|v| *v -= by as f32);
                let part = cv(Mat::roi(&scans[i], core::Rect::new(bx, by, bw, bh)))?;
                let mut part16 = Mat::default();
                cv(part.convert_to(&mut part16, core::CV_16U, 257.0, 0.0))?;
                let r = remap(&part16, &mx, &my, imgproc::INTER_LANCZOS4, core::BORDER_REPLICATE, 0.0)?;
                let rd = cv(r.data_typed::<core::Vec3w>())?;
                for r in 0..ey - ty {
                    let o = (ty + r) * ow + tx;
                    for c in 0..tw {
                        out[(o + c) * 3..(o + c) * 3 + 3].copy_from_slice(&rd[r * tw + c].0);
                    }
                }
            }
        }
        stack.push(out);
        valid.push(ok);
    }

    // For measuring how well the scans agree: MULTIPASS_KEEP_ALIGNED=DIR saves
    // each scan as it was resampled into the output frame (8 bits; black where
    // the scan does not reach).
    if let Some(dir) = std::env::var_os("MULTIPASS_KEEP_ALIGNED") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        for (i, (px, ok)) in stack.iter().zip(&valid).enumerate() {
            progress(0.85, &format!("Saving aligned scan {}", i + 1));
            let mut m = cv(Mat::new_rows_cols_with_default(oh as i32, ow as i32, core::CV_8UC3, Scalar::all(0.0)))?;
            cv(m.data_bytes_mut())?.par_chunks_mut(3).enumerate().for_each(|(k, out)| {
                if ok[k] {
                    for c in 0..3 {
                        out[c] = ((px[k * 3 + c] as u32 + 128) / 257) as u8;
                    }
                }
            });
            let path = dir.join(format!("aligned_{}.png", i + 1));
            std::fs::write(&path, crate::output::encode_png(&m, dpi)?).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
    }

    // ------------------------------------------------ 5. exposure matching
    progress(0.86, "Matching exposure");
    let ns = 300_000.min(npx);
    let mut rng = 0x9e3779b97f4a7c15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let samples: Vec<usize> = (0..ns).map(|_| (next() % npx as u64) as usize).filter(|&k| (0..n).all(|i| valid[i][k])).collect();
    let mut gains = vec![[1.0f64; 3]; n];
    let mut offs = vec![[0.0f64; 3]; n];
    if n > 2 && samples.len() > 1000 {
        for c in 0..3 {
            let med: Vec<f64> = samples
                .iter()
                .map(|&k| {
                    let mut v: Vec<f64> = (0..n).map(|i| stack[i][k * 3 + c] as f64 / 65535.0).collect();
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    if n % 2 == 1 {
                        v[n / 2]
                    } else {
                        0.5 * (v[n / 2 - 1] + v[n / 2])
                    }
                })
                .collect();
            for i in 0..n {
                let x: Vec<f64> = samples.iter().map(|&k| stack[i][k * 3 + c] as f64 / 65535.0).collect();
                let mut keep = vec![true; x.len()];
                let (mut g, mut o) = (1.0, 0.0);
                for _ in 0..3 {
                    let (mut sx, mut sy, mut sxx, mut sxy, mut m) = (0.0, 0.0, 0.0, 0.0, 0.0);
                    for j in 0..x.len() {
                        if keep[j] {
                            sx += x[j];
                            sy += med[j];
                            sxx += x[j] * x[j];
                            sxy += x[j] * med[j];
                            m += 1.0;
                        }
                    }
                    let den = m * sxx - sx * sx;
                    if den.abs() < 1e-12 {
                        break;
                    }
                    g = (m * sxy - sx * sy) / den;
                    o = (sy - g * sx) / m;
                    let mut r: Vec<f32> = (0..x.len()).map(|j| (g * x[j] + o - med[j]).abs() as f32).collect();
                    let mad = percentile(&mut r.clone(), 50.0) as f64 * 1.4826 + 1e-4;
                    for j in 0..x.len() {
                        keep[j] = (r[j] as f64) < 3.0 * mad;
                    }
                    r.clear();
                }
                gains[i][c] = g;
                offs[i][c] = o;
            }
            let gm: f64 = (0..n).map(|i| gains[i][c]).sum::<f64>() / n as f64;
            let om: f64 = (0..n).map(|i| offs[i][c]).sum::<f64>() / n as f64;
            for i in 0..n {
                gains[i][c] /= gm;
                offs[i][c] -= om;
            }
        }
        for i in 0..n {
            report.push(format!("scan {}: exposure gain {:.3} {:.3} {:.3}", i + 1, gains[i][0], gains[i][1], gains[i][2]));
        }
    }
    // noise level for clipping: robust spread around the per-pixel median
    let mut sigma = [1.0f64 / 65535.0; 3];
    if n >= 3 && samples.len() > 1000 {
        for c in 0..3 {
            let mut dev: Vec<f32> = Vec::with_capacity(samples.len() * n);
            for &k in &samples {
                let mut v: Vec<f64> =
                    (0..n).map(|i| stack[i][k * 3 + c] as f64 / 65535.0 * gains[i][c] + offs[i][c]).collect();
                let mut s = v.clone();
                s.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let med = if n % 2 == 1 { s[n / 2] } else { 0.5 * (s[n / 2 - 1] + s[n / 2]) };
                for x in v.iter_mut() {
                    dev.push((*x - med).abs() as f32);
                }
            }
            let s = 1.4826 * percentile(&mut dev, 50.0) as f64 * (n as f64 / (n as f64 - 1.0)).sqrt();
            sigma[c] = s.max(1.0 / 65535.0);
        }
        report.push(format!("noise level {:.4} {:.4} {:.4} (of full scale)", sigma[0], sigma[1], sigma[2]));
    }

    // ------------------------------------------------ 6. sigma-clipped mean
    progress(0.88, "Combining");
    let mut result = vec![0u16; npx * 3];
    result.par_chunks_mut(ow * 3).enumerate().for_each(|(row, out)| {
        let mut vals = [0f64; 32];
        let mut sorted = [0f64; 32];
        for x in 0..ow {
            let k = row * ow + x;
            for c in 0..3 {
                let mut m = 0;
                for i in 0..n {
                    if valid[i][k] {
                        vals[m] = stack[i][k * 3 + c] as f64 / 65535.0 * gains[i][c] + offs[i][c];
                        m += 1;
                    }
                }
                let v = if m == 0 {
                    1.0 // no scan reaches here (a corner outside the photo): lid white
                } else if m < 3 {
                    vals[..m].iter().sum::<f64>() / m as f64
                } else {
                    sorted[..m].copy_from_slice(&vals[..m]);
                    sorted[..m].sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let med = if m % 2 == 1 { sorted[m / 2] } else { 0.5 * (sorted[m / 2 - 1] + sorted[m / 2]) };
                    let thr = 3.0 * sigma[c];
                    let (mut s, mut cntk) = (0.0, 0);
                    for &v in &vals[..m] {
                        if (v - med).abs() <= thr {
                            s += v;
                            cntk += 1;
                        }
                    }
                    if cntk > 0 {
                        s / cntk as f64
                    } else {
                        med
                    }
                };
                out[x * 3 + c] = (v * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16;
            }
        }
    });
    drop(stack);
    let mut image = cv(Mat::new_rows_cols_with_default(oh as i32, ow as i32, core::CV_16UC3, Scalar::all(0.0)))?;
    cv(image.data_typed_mut::<core::Vec3w>())?
        .iter_mut()
        .enumerate()
        .for_each(|(k, p)| p.0 = [result[k * 3], result[k * 3 + 1], result[k * 3 + 2]]);
    progress(1.0, "Done");
    report.push(format!("output {}x{} px (crop of scan 1 at {},{})", ow, oh, x0, y0));
    Ok(Combined { image, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn carriage_step_lands_on_the_average() {
        // n scans at positions x; each is measured against the mean of the others
        for n in [2usize, 3, 4] {
            let x: Vec<f64> = (0..n).map(|i| (i * i) as f64 - 1.5).collect();
            let mean = x.iter().sum::<f64>() / n as f64;
            for &xi in &x {
                let others = (mean * n as f64 - xi) / (n as f64 - 1.0);
                let moved = xi - shared_gain(n) * (xi - others);
                assert!((moved - mean).abs() < 1e-12, "n = {n}");
            }
        }
    }

    #[test]
    fn profile_step_depends_on_a_partner_along_the_same_line() {
        let (up, down, across) = ((0.0, 1.0), (0.02, -1.0), (1.0, 0.01));
        // as placed and half round: the two share the profile
        assert_eq!(profile_gains(&[up, down]), vec![0.5, 0.5]);
        // as placed and a quarter turn: nothing shared
        assert_eq!(profile_gains(&[up, across]), vec![1.0, 1.0]);
        // three scans: only the pair along one line shares
        let g = profile_gains(&[up, across, down]);
        assert!((g[0] - 2.0 / 3.0).abs() < 1e-12 && g[1] == 1.0 && (g[2] - 2.0 / 3.0).abs() < 1e-12);
        // four quarter turns: every scan has a partner
        assert_eq!(profile_gains(&[up, across, down, (-1.0, 0.0)]), vec![0.75; 4]);
    }

    #[test]
    fn a_tile_takes_the_part_of_the_scan_it_needs() {
        // in the middle of a scan: the coordinates, and room for the kernel
        assert_eq!(source_span(&[40000.25, 40100.5, 40050.0], 50000), (39992, 118));
        // at the scan's edges it stops there
        assert_eq!(source_span(&[-3.0, 5.5], 50000), (0, 15));
        assert_eq!(source_span(&[49990.0, 50020.0], 50000), (49982, 18));
        // a tile that lies outside the scan altogether still gets a valid piece
        assert_eq!(source_span(&[-500.0, -400.0], 50000), (0, 1));
        assert_eq!(source_span(&[60000.0, 60010.0], 50000), (49999, 1));
        assert_eq!(source_span(&[f32::NAN, f32::NAN], 50000), (0, 1));
        // coordinates that cannot be used are skipped
        assert_eq!(source_span(&[f32::NAN, 100.0, 120.0], 50000), (92, 37));
    }

    #[test]
    fn memory_grows_with_the_pixels_once_the_scans_are_large() {
        let gb = |b: u64| b as f64 / GB as f64;
        // a 4 x 6 inch photo with margins at 1200 dpi (44 megapixels), four scans:
        // the working images of the alignment decide
        let small = gb(memory_model(4, 44_000_000, true));
        assert!((8.0..10.0).contains(&small), "{small}");
        // the same in tiles, as on a 16 GB machine
        let tiled = gb(memory_model(4, 44_000_000, false));
        assert!((3.0..5.0).contains(&tiled), "{tiled}");
        // at 4800 dpi (708 megapixels) the resampled scans decide: 46 bytes a pixel
        let large = gb(memory_model(4, 708_000_000, true));
        assert!((30.0..33.0).contains(&large), "{large}");
        // fewer scans need less
        assert!(memory_model(2, 708_000_000, true) < memory_model(3, 708_000_000, true));
    }

    #[test]
    fn tiling_follows_installed_memory() {
        // more than 16 GB: the whole frame
        assert_eq!(flow_tile_limit(Some(32 * GB), None), usize::MAX);
        assert_eq!(flow_tile_limit(Some(61 * GB), None), usize::MAX);
        assert_eq!(flow_tile_limit(Some(18 * GB), None), usize::MAX);
        // 16 GB or less: tiles.  A "16 GB" machine reports a little under 16.
        assert_eq!(flow_tile_limit(Some(16 * GB), None), FLOW_TILE_PX);
        assert_eq!(flow_tile_limit(Some(15 * GB + GB / 2), None), FLOW_TILE_PX);
        assert_eq!(flow_tile_limit(Some(8 * GB), None), FLOW_TILE_PX);
        // unknown: tiles, the safe choice
        assert_eq!(flow_tile_limit(None, None), FLOW_TILE_PX);
        // MULTIPASS_FLOW_TILE_MP overrides either way
        assert_eq!(flow_tile_limit(Some(8 * GB), Some(0.0)), usize::MAX);
        assert_eq!(flow_tile_limit(Some(64 * GB), Some(4.0)), 4_000_000);
    }
}
