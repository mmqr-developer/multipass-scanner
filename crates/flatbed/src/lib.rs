//! Flatbed scanner access.
//!
//! The rest of the app sees one interface, [`Scanner`]: a name, the size of
//! the glass, and a blocking `scan` of an area in millimetres.  Each operating
//! system has its own backend behind it:
//!
//! | System  | File             | Driver                                     |
//! |---------|------------------|--------------------------------------------|
//! | Linux   | `linux.rs`       | Epson Scan 2 (`libepsonscan2.so`)          |
//! | macOS   | `macos.rs`       | Apple's ImageCaptureCore                   |
//! | Windows | `wia.rs`         | Windows Image Acquisition 2.0 (not yet run with a scanner) |
//! | other   | `unsupported.rs` | none: opening a scanner reports that       |
//!
//! Every backend must switch off each automatic or lossy correction its driver
//! offers, so that scans differ only by where the original lies on the glass.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

// Not called `windows`: that is the name of Microsoft's bindings crate.
#[cfg(target_os = "windows")]
mod wia;
#[cfg(target_os = "windows")]
use wia as backend;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported as backend;

// for drivers that deliver a scan a few rows at a time
#[cfg(any(target_os = "macos", test))]
mod bands;

// for drivers that deliver a scan as a BMP file in memory
#[cfg(any(target_os = "windows", test))]
mod bmp;

/// A scanned image: 8 bits per sample, rows packed (no padding), 1 (grey) or
/// 3 (RGB) channels.  The first row is the one nearest the top edge of the
/// glass (where the carriage starts); rows run across the glass.
pub struct RawImage {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub data: Vec<u8>,
}

/// Rectangle on the glass in millimetres.  `x` runs across the glass (along the
/// sensor), `y` down it (the way the carriage travels), both from the corner
/// where a scan starts.
#[derive(Clone, Copy, Debug)]
pub struct AreaMm {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// An open flatbed scanner.  Dropping it closes the device.
pub trait Scanner: Send {
    /// The name to show the user.
    fn name(&self) -> &str;

    /// Size of the glass in mm: (across, down).
    fn glass_mm(&self) -> (f64, f64);

    /// Scan `area` at `dpi`, in grey or colour, 8 bits per sample.  Blocks until
    /// the image has arrived.  An area that reaches past the glass is cut to it.
    fn scan(&mut self, dpi: i32, gray: bool, area: AreaMm) -> Result<RawImage, String>;

    /// What the backend found out about the device and what it changed in the
    /// driver's settings when it opened it.  For diagnostics only.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Find the first flatbed scanner and open it.
///
/// Must not be called on the main thread where [`run_with_main_loop`] says the
/// main thread has other work to do.
pub fn open_first() -> Result<Box<dyn Scanner>, String> {
    backend::open_first()
}

/// Run `work`, which may use a scanner, from a program without a GUI.
///
/// On Linux this just calls `work`.  Some systems deliver scanner events
/// through the main thread's event loop; there, `work` runs on a second thread
/// while the main thread runs that loop.  A program with a GUI has such a loop
/// already and only has to keep scanner calls off its main thread.
pub fn run_with_main_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    backend::run_with_main_loop(work)
}

/// How long one scan may take before it is given up: half an hour up to
/// 1200 dpi, and twice as long for each doubling of the resolution.
#[allow(dead_code)] // not every backend has a time limit
pub(crate) fn scan_time_limit(dpi: i32) -> std::time::Duration {
    std::time::Duration::from_secs(30 * 60 * (dpi.max(1200) as u64) / 1200)
}

/// Pixel rectangle `(x, y, w, h)` for `area` at `dpi`, cut to a glass of
/// `glass_mm`.  Never empty.
pub(crate) fn area_px(area: AreaMm, dpi: i32, glass_mm: (f64, f64)) -> (i32, i32, i32, i32) {
    let px = |mm: f64| (mm / 25.4 * dpi as f64).round() as i32;
    let max_w = ((glass_mm.0 / 25.4 * dpi as f64).floor() as i32).max(1);
    let max_h = ((glass_mm.1 / 25.4 * dpi as f64).floor() as i32).max(1);
    let x = px(area.x).clamp(0, max_w - 1);
    let y = px(area.y).clamp(0, max_h - 1);
    let w = px(area.w).clamp(1, max_w - x);
    let h = px(area.h).clamp(1, max_h - y);
    (x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Epson Perfection V19 II: 8.5 x 11.7 inches
    const GLASS: (f64, f64) = (215.9, 297.18);

    #[test]
    fn a_finer_scan_gets_more_time() {
        let minutes = |dpi| scan_time_limit(dpi).as_secs() / 60;
        assert_eq!([minutes(75), minutes(600), minutes(1200), minutes(2400), minutes(4800)], [30, 30, 30, 60, 120]);
    }

    #[test]
    fn whole_glass_at_1200_dpi() {
        let a = AreaMm { x: 0.0, y: 0.0, w: GLASS.0, h: GLASS.1 };
        assert_eq!(area_px(a, 1200, GLASS), (0, 0, 10200, 14040));
    }

    #[test]
    fn an_area_past_the_glass_is_cut_to_it() {
        let a = AreaMm { x: 200.0, y: 290.0, w: 50.0, h: 50.0 };
        let (x, y, w, h) = area_px(a, 75, GLASS);
        assert_eq!((x, y), (591, 856));
        assert_eq!((x + w, y + h), (637, 877));
        let off = AreaMm { x: 500.0, y: -3.0, w: 0.0, h: 10.0 };
        let (x, y, w, h) = area_px(off, 75, GLASS);
        assert_eq!((x, y, w), (636, 0, 1));
        assert_eq!(h, 30);
    }
}
