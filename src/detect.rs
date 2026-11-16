//! Finding the photo on the glass from low-resolution grey scans.
//!
//! The lid is the brightest thing in the scan.  A pixel is "photo" when it is
//! darker than the lid by more than the lid itself varies (it falls off by
//! about 13 levels towards the edges of the glass).  Even a print with a white
//! border shows up, because its edges cast a thin dark line.
//!
//! A pale print (a light wall, sky) may only show its darker details, so the
//! photo is found as a group of stretches: stretches narrower than 2 mm are
//! dust and are ignored, and stretches less than 30 mm apart belong to the same
//! photo.  Scanning a little too much lid costs a few seconds; cutting off part
//! of the photo would spoil the result.

use flatbed::RawImage;

/// The photo along one axis.
///
/// `extent[i]` is the (first, last) position across the scan where index i is
/// dark (None: not dark enough to count), `across` the scan's size across.
/// Stretches are bridged over gaps up to 1 mm (noise).  A stretch is photo if it is at
/// least 8 mm long or its dark pixels reach across at least 40% of the scan;
/// anything else is a speck on the lid or the glass.  Photo stretches less than
/// 30 mm apart are one photo; the group that covers the most wins.
/// Returns (start, end_exclusive).
fn photo_span(extent: &[Option<(usize, usize)>], across: usize, dpi: f64) -> Option<(usize, usize)> {
    let mm = |v: f64| ((v / 25.4) * dpi).round() as usize;
    // Specks are dropped before nearby stretches are joined, so a speck just
    // beside the photo does not widen it.
    let (small_gap, long_enough, big_gap) = (mm(1.0), mm(8.0), mm(30.0));
    // (start, end, first across, last across)
    let mut runs: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (i, e) in extent.iter().enumerate() {
        let Some((a, b)) = *e else { continue };
        match runs.last_mut() {
            Some(r) if i <= r.1 + small_gap => {
                r.1 = i + 1;
                r.2 = r.2.min(a);
                r.3 = r.3.max(b);
            }
            _ => runs.push((i, i + 1, a, b)),
        }
    }
    runs.retain(|r| r.1 - r.0 >= long_enough || (r.3 - r.2 + 1) * 10 >= across * 4);
    let mut best: Option<((usize, usize), usize)> = None;
    let mut group: Option<((usize, usize), usize)> = None;
    for r in runs {
        group = match group {
            Some(((s, e), cover)) if r.0 <= e + big_gap => Some(((s, r.1), cover + r.1 - r.0)),
            Some(g) => {
                if best.map_or(true, |b| g.1 > b.1) {
                    best = Some(g);
                }
                Some(((r.0, r.1), r.1 - r.0))
            }
            None => Some(((r.0, r.1), r.1 - r.0)),
        };
    }
    if let Some(g) = group {
        if best.map_or(true, |b| g.1 > b.1) {
            best = Some(g);
        }
    }
    best.map(|b| b.0)
}

/// Brightness of the lid: the 95th percentile of the scan.
pub fn lid_level(img: &RawImage) -> u8 {
    let mut hist = [0usize; 256];
    for &v in &img.data {
        hist[v as usize] += 1;
    }
    let mut acc = 0;
    for v in (0..256).rev() {
        acc += hist[v];
        if acc * 20 >= img.data.len() {
            return v as u8;
        }
    }
    0
}

/// Lid brightness and the "clearly darker" threshold.
fn threshold(img: &RawImage) -> u8 {
    let mut hist = [0usize; 256];
    for &v in &img.data {
        hist[v as usize] += 1;
    }
    let total = img.data.len();
    let mut acc = 0;
    let mut bg = 255usize;
    for v in (0..256).rev() {
        acc += hist[v];
        if acc * 20 >= total {
            // 95th percentile
            bg = v;
            break;
        }
    }
    let delta = (bg as f64 * 0.07).max(18.0);
    (bg as f64 - delta).max(0.0) as u8
}

fn dark_mask(img: &RawImage) -> Vec<bool> {
    let t = threshold(img);
    // light 3x3 smoothing against noise and dust specks
    let (w, h) = (img.width, img.height);
    let px = |x: usize, y: usize| img.data[y * w + x] as u32;
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0;
            let mut n = 0;
            for yy in y.saturating_sub(1)..(y + 2).min(h) {
                for xx in x.saturating_sub(1)..(x + 2).min(w) {
                    s += px(xx, yy);
                    n += 1;
                }
            }
            out[y * w + x] = s < t as u32 * n;
        }
    }
    out
}

/// Step 1: in a grey strip across the top of the glass, find the photo's left
/// and right edges.  Returns (x0, x1) in pixels of the strip (x1 exclusive).
pub fn find_columns(strip: &RawImage, dpi: f64) -> Option<(usize, usize)> {
    assert_eq!(strip.channels, 1);
    let (w, h) = (strip.width, strip.height);
    let dark = dark_mask(strip);
    let skip = ((2.0 / 25.4) * dpi).ceil() as usize; // the very top rows sit against the frame
    // Rows that are dark across nearly the whole glass are the scanner's frame, not the photo.
    let rows: Vec<usize> = (skip.min(h.saturating_sub(1))..h)
        .filter(|&y| {
            let n = (0..w).filter(|&x| dark[y * w + x]).count();
            n * 100 < w * 97
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    let need = ((rows.len() as f64 * 0.08).ceil() as usize).max(2);
    let extent: Vec<Option<(usize, usize)>> = (0..w)
        .map(|x| {
            let d: Vec<usize> = rows.iter().copied().filter(|&y| dark[y * w + x]).collect();
            (d.len() >= need).then(|| (d[0], d[d.len() - 1]))
        })
        .collect();
    let (x0, x1) = photo_span(&extent, h, dpi)?;
    // something narrower than 15 mm is dust or a mark on the lid, not a photo
    ((x1 - x0) as f64 >= 15.0 / 25.4 * dpi).then_some((x0, x1))
}

/// Step 2: in a grey scan of the photo's column range down the whole glass,
/// find where the photo ends.  Returns (y0, y1) in pixels (y1 exclusive).
pub fn find_rows(img: &RawImage, dpi: f64) -> Option<(usize, usize)> {
    assert_eq!(img.channels, 1);
    let (w, h) = (img.width, img.height);
    let dark = dark_mask(img);
    let need = ((w as f64 * 0.05).ceil() as usize).max(2);
    let extent: Vec<Option<(usize, usize)>> = (0..h)
        .map(|y| {
            let d: Vec<usize> = (0..w).filter(|&x| dark[y * w + x]).collect();
            (d.len() >= need).then(|| (d[0], d[d.len() - 1]))
        })
        .collect();
    let (y0, y1) = photo_span(&extent, w, dpi)?;
    ((y1 - y0) as f64 >= 15.0 / 25.4 * dpi).then_some((y0, y1))
}

/// First row of the top strip where the photo (columns x0..x1) starts.  The
/// photo normally touches the top edge, so this is usually 0.
pub fn find_top(strip: &RawImage, x0: usize, x1: usize) -> usize {
    let (w, h) = (strip.width, strip.height);
    let dark = dark_mask(strip);
    let span = (x1 - x0).max(1);
    (0..h)
        .find(|&y| (x0..x1.min(w)).filter(|&x| dark[y * w + x]).count() * 4 >= span)
        .unwrap_or(0)
}

/// The photo's sides are `dims` (mm).  After a quarter turn its measured width
/// across the glass must match one side; its length is then the other one.
/// Returns (length, how far the width was off) or None if neither side fits.
pub fn length_from_known(width: f64, dims: (f64, f64), tol: f64) -> Option<(f64, f64)> {
    let (da, db) = ((width - dims.0).abs(), (width - dims.1).abs());
    if da.min(db) > tol {
        return None;
    }
    if (da - db).abs() < 1.5 {
        // a (nearly) square photo: either side fits, so take the longer one
        Some((dims.0.max(dims.1), da.min(db)))
    } else if da < db {
        Some((dims.1, da))
    } else {
        Some((dims.0, db))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_size() {
        let dims = (102.0, 95.0);
        assert_eq!(length_from_known(95.5, dims, 6.0).map(|r| r.0), Some(102.0));
        assert_eq!(length_from_known(101.0, dims, 6.0).map(|r| r.0), Some(95.0));
        assert_eq!(length_from_known(99.0, (100.0, 98.0), 6.0).map(|r| r.0), Some(100.0));
        assert_eq!(length_from_known(150.0, dims, 6.0), None);
    }

    #[test]
    fn spans() {
        // 75 dpi: 1 mm = 3 px, 8 mm = 24 px, 30 mm = 89 px; strip 75 px across
        let mut e: Vec<Option<(usize, usize)>> = vec![None; 600];
        let mut set = |a: usize, b: usize, top: usize, bot: usize| (a..b).for_each(|i| e[i] = Some((top, bot)));
        set(100, 112, 0, 74); // photo edge + detail, full height
        set(150, 220, 20, 60); // more detail 38 px (13 mm) further on: same photo
        set(60, 67, 30, 35); // a speck on the lid 33 px (11 mm) before it: ignored
        set(450, 460, 0, 74); // a mark 230 px (78 mm) away: a different group
        assert_eq!(photo_span(&e, 75, 75.0), Some((100, 220)));
        assert_eq!(photo_span(&[None; 40], 75, 75.0), None);
    }

    fn synthetic(w: usize, h: usize, rect: (usize, usize, usize, usize)) -> RawImage {
        let mut data = vec![235u8; w * h];
        for y in rect.1..rect.3.min(h) {
            for x in rect.0..rect.2 {
                data[y * w + x] = 90;
            }
        }
        RawImage { width: w, height: h, channels: 1, data }
    }

    #[test]
    fn finds_photo() {
        // 75 dpi glass, photo 100..400 x 0..300
        let strip = synthetic(637, 75, (100, 0, 400, 300));
        assert_eq!(find_columns(&strip, 75.0), Some((99, 401)));
        let col = synthetic(300, 877, (0, 0, 300, 300));
        assert_eq!(find_rows(&col, 75.0), Some((0, 301)));
    }
}
