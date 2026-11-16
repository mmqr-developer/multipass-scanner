//! The worker thread: owns the scanner, captures up to four scans and, when
//! asked, combines them.  The GUI talks to it through two channels and never
//! blocks.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{self, ColorImage};
use opencv::core::{Mat, Scalar, Size, CV_8UC3};
use opencv::imgproc;
use opencv::prelude::*;

use flatbed::{AreaMm, RawImage, Scanner};

use crate::{combine, detail, detect, output, post};

pub const PASSES: usize = 4;
/// The resolutions the colour scans can be taken at, and the one the app
/// starts with.
pub const RESOLUTIONS: [i32; 4] = [600, 1200, 2400, 4800];
pub const DEFAULT_DPI: i32 = 1200;
const PREVIEW_DPI: i32 = 75;
const MARGIN_MM: f64 = 8.0;
const TOP_STRIP_MM: f64 = 25.4;

/// Combining needs at least this many scans.
pub const MIN_SCANS: usize = 2;

pub enum Cmd {
    /// Scan into `slot`, replacing what is there; `None` is the first empty
    /// slot (after a result was saved: the first scan of the next photo).
    /// `full_size`: skip finding the photo and scan the whole glass.
    /// `dpi`: the resolution of the colour scan; all scans of one photo must
    /// have the same.
    Scan { slot: Option<usize>, full_size: bool, dpi: i32 },
    /// Combine the scans taken so far, clean the result up as `post` says and
    /// save it.
    Process { post: post::Options },
    /// Forget one scan.
    Remove(usize),
    Reset,
}

/// The size of the scan the result takes its frame from (the lowest-numbered
/// one held), which is the size the result will have, give or take a few
/// pixels at the edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanSize {
    pub width_px: usize,
    pub height_px: usize,
    pub dpi: i32,
}

impl ScanSize {
    pub fn pixels(&self) -> u64 {
        self.width_px as u64 * self.height_px as u64
    }

    /// (width, height) on the glass
    pub fn mm(&self) -> (f64, f64) {
        (self.width_px as f64 * 25.4 / self.dpi as f64, self.height_px as f64 * 25.4 / self.dpi as f64)
    }
}

pub enum Ev {
    Busy(bool),
    Status(String),
    Scanner(String),
    /// `half_turn_only`: the photo is too long to lie across the glass, so it
    /// can only be scanned as placed and turned half round.
    Captured { slot: usize, thumb: ColorImage, preview: ColorImage, half_turn_only: bool },
    Removed { slot: usize },
    Cleared,
    /// What the result of the scans held will be like; `None` without scans.
    /// Sent whenever the scans change.
    Expected(Option<ScanSize>),
    /// Whether the resolution suits the picture, judged from its first scan
    /// (a sentence for the window); `None` without scans or if it cannot be
    /// judged.
    Detail(Option<String>),
    Progress(Option<(f32, String)>),
    /// `size`: of the saved image; `bytes`: of the file.
    Saved { path: PathBuf, preview: ColorImage, report: Vec<String>, post: post::Options, size: ScanSize, bytes: u64 },
    Error(String),
}

pub struct Worker {
    tx: Sender<Ev>,
    ctx: egui::Context,
    scanner: Option<Box<dyn Scanner>>,
    scans: [Option<Mat>; PASSES],
    /// the resolution the scans held were taken at
    dpi: i32,
    /// the scans, exactly as they are now, have been combined and saved
    processed: bool,
    /// the scans as they are now, combined (before any post-processing); kept
    /// so that processing again with other options does not combine again
    combined: Option<combine::Combined>,
    /// the photo's sides (mm), measured when no other scan of it was held
    photo_mm: Option<(f64, f64)>,
    half_turn_only: bool,
}

impl Worker {
    pub fn spawn(rx: Receiver<Cmd>, tx: Sender<Ev>, ctx: egui::Context) {
        std::thread::Builder::new()
            .name("scanner".into())
            .spawn(move || {
                let mut w = Worker {
                    tx,
                    ctx,
                    scanner: None,
                    scans: Default::default(),
                    dpi: DEFAULT_DPI,
                    processed: false,
                    combined: None,
                    photo_mm: None,
                    half_turn_only: false,
                };
                w.connect_quietly();
                while let Ok(cmd) = rx.recv() {
                    w.handle(cmd);
                }
            })
            .expect("cannot start the scanner thread");
    }

    fn send(&self, ev: Ev) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    fn status(&self, s: impl Into<String>) {
        self.send(Ev::Status(s.into()));
    }

    fn connect_quietly(&mut self) {
        if std::env::var_os("MULTIPASS_FAKE_SCANS").is_some() {
            self.send(Ev::Scanner("test mode: no scanner".into()));
            return;
        }
        if let Ok(s) = flatbed::open_first() {
            self.send(Ev::Scanner(s.name().to_string()));
            self.scanner = Some(s);
        }
    }

    fn scanner(&mut self) -> Result<&mut dyn Scanner, String> {
        if self.scanner.is_none() {
            self.status("Opening the scanner");
            let s = flatbed::open_first()?;
            self.send(Ev::Scanner(s.name().to_string()));
            self.scanner = Some(s);
        }
        Ok(self.scanner.as_mut().unwrap().as_mut())
    }

    fn count(&self) -> usize {
        self.scans.iter().filter(|s| s.is_some()).count()
    }

    /// The size of the lowest-numbered scan held: the combiner's reference.
    fn expected(&self) -> Option<ScanSize> {
        let first = self.scans.iter().flatten().next()?;
        Some(ScanSize { width_px: first.cols() as usize, height_px: first.rows() as usize, dpi: self.dpi })
    }

    fn clear(&mut self) {
        self.scans = Default::default();
        self.processed = false;
        self.combined = None;
        self.photo_mm = None;
        self.half_turn_only = false;
        self.send(Ev::Cleared);
        self.send(Ev::Expected(None));
        self.send(Ev::Detail(None));
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Reset => self.clear(),
            Cmd::Remove(slot) => {
                if slot < PASSES && self.scans[slot].take().is_some() {
                    self.processed = false;
                    self.combined = None;
                    if self.count() == 0 {
                        self.photo_mm = None;
                        self.half_turn_only = false;
                    }
                    self.send(Ev::Removed { slot });
                    self.send(Ev::Expected(self.expected()));
                    if self.count() == 0 {
                        self.send(Ev::Detail(None));
                    }
                }
            }
            Cmd::Scan { slot, full_size, dpi } => {
                self.send(Ev::Busy(true));
                if let Err(e) = self.scan_into(slot, full_size, dpi) {
                    self.send(Ev::Error(e));
                }
                self.send(Ev::Busy(false));
            }
            Cmd::Process { post } => {
                self.send(Ev::Busy(true));
                if let Err(e) = self.combine_and_save(post) {
                    self.send(Ev::Progress(None));
                    self.send(Ev::Error(e));
                }
                self.send(Ev::Busy(false));
            }
        }
    }

    fn scan_into(&mut self, target: Option<usize>, full_size: bool, dpi: i32) -> Result<(), String> {
        let slot = match target {
            Some(s) if s < PASSES => s,
            _ => {
                if self.processed {
                    self.clear(); // that photo is saved: this is the next one
                }
                self.scans
                    .iter()
                    .position(|s| s.is_none())
                    .ok_or("All four scans are taken. Select one to scan it again, or press Process.")?
            }
        };
        // Scans of different resolutions cannot be combined.
        let others = (0..PASSES).filter(|&i| i != slot && self.scans[i].is_some()).count();
        if others > 0 && dpi != self.dpi {
            return Err(format!(
                "The other scans of this photo were taken at {} dpi. Set the resolution back to {} dpi, \
                 or press Start over to scan the photo at {dpi} dpi.",
                self.dpi, self.dpi
            ));
        }
        let img = match self.capture(slot, full_size, dpi, others + 1) {
            Ok(m) => m,
            Err(e) => {
                // A driver error may leave the device in a bad state: reopen next time.
                self.scanner = None;
                return Err(e);
            }
        };
        let thumb = to_color_image(&img, 360)?;
        let preview = to_color_image(&img, 1400)?;
        // The first scan of a photo shows how fine its detail is, and so
        // whether this resolution suits it.
        let advice = (others == 0).then(|| detail::analyse(&img, dpi as f64).map(|d| detail::advice(&d, dpi)));
        self.scans[slot] = Some(img);
        self.dpi = dpi;
        self.processed = false;
        self.combined = None;
        self.send(Ev::Captured { slot, thumb, preview, half_turn_only: self.half_turn_only });
        self.send(Ev::Expected(self.expected()));
        if let Some(advice) = advice {
            self.send(Ev::Detail(advice));
        }
        Ok(())
    }

    /// Steps 1-3 for one orientation, or the whole glass for a full-size photo.
    /// `held`: how many scans of this photo there will be with this one.
    fn capture(&mut self, slot: usize, full_size: bool, dpi: i32, held: usize) -> Result<Mat, String> {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let status = move |s: String| {
            let _ = tx.send(Ev::Status(s));
            ctx.request_repaint();
        };
        if full_size {
            // A photo that fills the glass cannot be turned a quarter turn, and
            // its size was not measured.
            self.half_turn_only = true;
            self.photo_mm = None;
        }
        // Testing without a scanner: MULTIPASS_FAKE_SCANS="a.png,b.png,c.png,d.png"
        // feeds existing scans in place of steps 1-3.
        if let Ok(list) = std::env::var("MULTIPASS_FAKE_SCANS") {
            let path = list.split(',').nth(slot).ok_or("MULTIPASS_FAKE_SCANS has too few files")?.to_string();
            status(format!("Scan {} \u{2014} (test mode) loading {path}", slot + 1));
            return crate::load_rgb(&path);
        }
        // Before the long colour scan: is there memory to combine scans this large?
        let fits = move |px: u64| fits_in_memory(px, dpi, held);
        if full_size {
            return capture_whole_glass(self.scanner()?, slot + 1, dpi, &status, &fits);
        }
        // The photo's size is known while another scan of it is held.
        let others = (0..PASSES).any(|i| i != slot && self.scans[i].is_some());
        let known = if others { self.photo_mm } else { None };
        let sc = self.scanner()?;
        let c = capture_photo(sc, slot + 1, known, dpi, &status, &fits)?;
        if known.is_none() {
            self.photo_mm = Some(c.photo_mm);
            self.half_turn_only = !c.quarter_turn_fits;
        }
        Ok(c.image)
    }

    fn combine_and_save(&mut self, opts: post::Options) -> Result<(), String> {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let progress = move |f: f32, s: &str| {
            let _ = tx.send(Ev::Progress(Some((f, s.to_string()))));
            ctx.request_repaint();
        };
        let slots: Vec<usize> = (0..PASSES).filter(|&i| self.scans[i].is_some()).collect();
        if slots.len() < MIN_SCANS {
            return Err("Combining needs at least two scans.".into());
        }
        // Post-processing works on whole-image buffers: check before the long
        // combining that there is memory for it.
        if opts.any() {
            if let Some(ram) = combine::total_ram() {
                let pixels = |m: &Mat| m.rows() as u64 * m.cols() as u64;
                let held: u64 = slots.iter().map(|&i| 3 * pixels(self.scans[i].as_ref().unwrap())).sum();
                // the result is about the size of the first scan
                let px = match &self.combined {
                    Some(c) => pixels(&c.image),
                    None => pixels(self.scans[slots[0]].as_ref().unwrap()),
                };
                let need = held + 6 * px + post::memory_needed(px);
                if need > usable_memory(ram) {
                    const GB: f64 = 1073741824.0;
                    return Err(format!(
                        "Post-processing an image of {:.0} megapixels needs about {:.0} GB of memory, and this \
                         computer has {:.0} GB. Untick \u{201c}Post-process the combined image\u{201d} and press \
                         Process again.",
                        px as f64 / 1e6,
                        need as f64 / GB,
                        ram as f64 / GB
                    ));
                }
            }
        }
        // share of the progress bar that combining takes
        let share = if opts.any() { 0.85 } else { 1.0 };
        if self.combined.is_none() {
            self.status(format!("Combining the {} scans", ["two", "three", "four"][slots.len() - MIN_SCANS]));
            progress(0.0, "Starting");
            // The combiner wants the scans side by side: lend them to it and put
            // them back, whatever it returns.  The lowest slot is the reference.
            let mats: Vec<Mat> = slots.iter().map(|&i| self.scans[i].take().unwrap()).collect();
            let res = combine::combine(&mats, self.dpi as f64, &|f, s| progress(f * share, s));
            for (&i, m) in slots.iter().zip(mats) {
                self.scans[i] = Some(m);
            }
            self.combined = Some(res?);
        } else {
            self.status("These scans are already combined: processing that result again");
            progress(share, "Combined before");
        }
        let combined = self.combined.as_ref().unwrap();
        let mut report = combined.report.clone();
        // post-processing works on a copy: the combined image is kept as it is
        let cleaned;
        let image = if opts.any() {
            let mut m = combined.image.clone();
            post::apply(&mut m, opts, self.dpi as f64, &|f, s| progress(share + (0.98 - share) * f, s), &mut report)?;
            cleaned = m;
            &cleaned
        } else {
            &combined.image
        };
        progress(if opts.any() { 0.98 } else { 1.0 }, "Saving");
        let png = output::encode_png(image, self.dpi as f64)?;
        let path = output::save_next(&output::pictures_dir(), &png)?;
        let size = ScanSize { width_px: image.cols() as usize, height_px: image.rows() as usize, dpi: self.dpi };
        let bytes = png.len() as u64;
        let mut preview8 = Mat::default();
        image.convert_to(&mut preview8, opencv::core::CV_8U, 1.0 / 257.0, 0.0).map_err(|e| e.to_string())?;
        let preview = to_color_image(&preview8, 1400)?;
        self.processed = true;
        self.send(Ev::Progress(None));
        self.send(Ev::Saved { path, preview, report, post: opts, size, bytes });
        Ok(())
    }
}

pub struct Capture {
    pub image: Mat,
    pub strip: RawImage,
    /// the grey length scan, when it was needed
    pub column: Option<RawImage>,
    pub area: AreaMm,
    /// the photo's size (width across the glass, length down it), without margins
    pub photo_mm: (f64, f64),
    /// whether the photo's longer side fits across the glass, so that it can
    /// also be scanned turned a quarter turn
    pub quarter_turn_fits: bool,
}

/// One orientation.
///
/// 1. grey 75 dpi strip across the top inch of the glass: the photo's left and
///    right edges (and where its top is);
/// 2. only when the photo's size is not yet known (the first pass): grey 75 dpi
///    scan of that column range down 8.5 inches, the longest photo that can be
///    turned a quarter turn on the glass, to find how long it is.  A photo that
///    reaches the end of that scan is measured again down the whole glass; it
///    can then only be scanned as placed and turned half round.  On later
///    passes the length is the side that the measured width does not match;
/// 3. colour scan of just that area at `dpi`, with 8 mm margins.  `fits` is
///    asked first, with the number of pixels that scan will have.
pub fn capture_photo(
    sc: &mut dyn Scanner,
    pass: usize,
    known: Option<(f64, f64)>,
    dpi: i32,
    status: &dyn Fn(String),
    fits: &dyn Fn(u64) -> Result<(), String>,
) -> Result<Capture, String> {
    let (glass_w, glass_h) = sc.glass_mm();
    let mm_per_px = 25.4 / PREVIEW_DPI as f64;
    let head = format!("Scan {pass} \u{2014}");

    // 1. the top inch: left and right edges
    status(format!("{head} finding the sides of the photo"));
    let strip = sc.scan(PREVIEW_DPI, true, AreaMm { x: 0.0, y: 0.0, w: glass_w, h: TOP_STRIP_MM })?;
    // With the lid open, the glass shows the dark room instead of the white lid.
    if detect::lid_level(&strip) < 140 {
        return Err("The top of the glass is dark. Is the lid closed?".into());
    }
    let (x0, x1) = detect::find_columns(&strip, PREVIEW_DPI as f64).ok_or(
        "No photo was found along the top edge of the glass. Place it face down, \
         touching the top edge, and press Scan again.",
    )?;
    let width = (x1 - x0) as f64 * mm_per_px;
    if width > glass_w * 0.97 {
        return Err("Something dark spans the whole width of the glass. Is the lid closed, \
                    and is only one photo on the glass?"
            .into());
    }
    let left = (x0 as f64 * mm_per_px - MARGIN_MM).max(0.0);
    let right = (x1 as f64 * mm_per_px + MARGIN_MM).min(glass_w);
    // the photo may be turned a degree or two: that widens it by up to this much
    const TURN_TOLERANCE_MM: f64 = 6.0;
    let longest = glass_w.min(glass_h); // a photo longer than this cannot be turned a quarter turn

    let known_length = known.and_then(|dims| detect::length_from_known(width, dims, TURN_TOLERANCE_MM));
    let (top, bottom, column, photo_mm) = match (known, known_length) {
        (Some(dims), Some((length, off))) => {
            // 2. not needed: the size is known from the first pass
            let y0 = detect::find_top(&strip, x0, x1) as f64 * mm_per_px;
            let top = (y0 - MARGIN_MM).max(0.0);
            // a slightly turned photo also reaches a little further down
            let bottom = (y0 + length + MARGIN_MM + off).min(glass_h);
            (top, bottom, None, dims)
        }
        _ => {
            if known.is_some() {
                status(format!(
                    "{head} the photo is {width:.0} mm wide, which matches neither side measured in the \
                     first scan; measuring its length again"
                ));
            }
            // 2. grey scan of that column range, 8.5 inches down: how long the photo is
            status(format!("{head} finding how long the photo is"));
            let column_area = |h: f64| AreaMm { x: left, y: 0.0, w: right - left, h };
            let mut column = sc.scan(PREVIEW_DPI, true, column_area(longest))?;
            let mut rows = detect::find_rows(&column, PREVIEW_DPI as f64);
            // A photo that reaches the end of that scan is longer than the glass
            // is wide: look down the whole glass.  One that reaches the end of
            // the glass too is scanned as far as the glass goes.
            let reaches_end = rows.map_or(false, |(_, y1)| y1 + 2 >= column.height);
            if reaches_end && glass_h > longest + 1.0 {
                status(format!("{head} the photo is longer than {longest:.0} mm; looking down the whole glass"));
                column = sc.scan(PREVIEW_DPI, true, column_area(glass_h))?;
                rows = detect::find_rows(&column, PREVIEW_DPI as f64);
            }
            let (y0, y1) = rows.ok_or("Could not find the bottom of the photo. Is the lid closed?")?;
            let top = (y0 as f64 * mm_per_px - MARGIN_MM).max(0.0);
            let bottom = (y1 as f64 * mm_per_px + MARGIN_MM).min(glass_h);
            let dims = known.unwrap_or((width, (y1 - y0) as f64 * mm_per_px));
            (top, bottom, Some(column), dims)
        }
    };

    // 3. colour, just that area
    let area = AreaMm { x: left, y: top, w: right - left, h: bottom - top };
    fits(pixels(area, dpi))?;
    status(format!("{head} scanning {:.0} \u{d7} {:.0} mm in colour at {dpi} dpi", area.w, area.h));
    let raw = sc.scan(dpi, false, area)?;
    // Lying across the glass, the photo must leave some lid showing beside it
    // (the same limit as the width check above).
    let quarter_turn_fits = photo_mm.0.max(photo_mm.1) <= longest * 0.97;
    Ok(Capture { image: raw_to_mat(&raw)?, strip, column, area, photo_mm, quarter_turn_fits })
}

/// A full-size photo: no grey scans, the whole glass in colour at `dpi`.
/// `fits` is asked first, with the number of pixels the scan will have.
pub fn capture_whole_glass(
    sc: &mut dyn Scanner,
    pass: usize,
    dpi: i32,
    status: &dyn Fn(String),
    fits: &dyn Fn(u64) -> Result<(), String>,
) -> Result<Mat, String> {
    let (glass_w, glass_h) = sc.glass_mm();
    let area = AreaMm { x: 0.0, y: 0.0, w: glass_w, h: glass_h };
    fits(pixels(area, dpi))?;
    status(format!(
        "Scan {pass} \u{2014} scanning the whole glass, {:.0} \u{d7} {:.0} mm, in colour at {dpi} dpi",
        area.w, area.h
    ));
    raw_to_mat(&sc.scan(dpi, false, area)?)
}

/// How many pixels a scan of `area` at `dpi` has.
fn pixels(area: AreaMm, dpi: i32) -> u64 {
    let px = |mm: f64| (mm / 25.4 * dpi as f64).round().max(1.0) as u64;
    px(area.w) * px(area.h)
}

/// Whether this computer has the memory to combine `held` scans of `px`
/// pixels each (at least two: one scan alone is never the goal).  Asked before
/// a colour scan, which at the high resolutions takes many minutes.
fn fits_in_memory(px: u64, dpi: i32, held: usize) -> Result<(), String> {
    let n = held.max(MIN_SCANS);
    match combine::total_ram() {
        Some(ram) => memory_verdict(px, dpi, held, combine::memory_needed(n, px), ram),
        None => Ok(()),
    }
}

/// `need`: what combining takes with this scan; `ram`: installed memory.
fn memory_verdict(px: u64, dpi: i32, held: usize, need: u64, ram: u64) -> Result<(), String> {
    if need <= usable_memory(ram) {
        return Ok(());
    }
    const GB: f64 = 1073741824.0;
    let n = held.max(MIN_SCANS);
    let size = format!("{:.0} megapixels at {dpi} dpi", px as f64 / 1e6);
    let figures =
        format!("needs about {:.0} GB of memory, and this computer has {:.0} GB", need as f64 / GB, ram as f64 / GB);
    Err(match held {
        0 | 1 => {
            format!("This scan would be {size}. Combining two scans of that size {figures}. Choose a lower resolution.")
        }
        2 => format!(
            "This scan would be {size}. Combining two scans of that size {figures}. \
             Press Start over and choose a lower resolution."
        ),
        _ => format!(
            "Another scan would be {size}. Combining {n} scans of that size {figures}. \
             Press Process to combine the scans you have."
        ),
    })
}

/// What of the installed memory the app may count on: the system and other
/// programs keep some.
pub fn usable_memory(ram: u64) -> u64 {
    ram - (ram / 10).max(2 << 30).min(ram / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_has_as_many_pixels_as_its_area_and_resolution_give() {
        // 4 x 6 inches
        let area = AreaMm { x: 10.0, y: 0.0, w: 101.6, h: 152.4 };
        assert_eq!(pixels(area, 600), 2400 * 3600);
        assert_eq!(pixels(area, 4800), 19200 * 28800);
    }

    #[test]
    fn a_scan_too_large_to_combine_is_refused_with_advice() {
        const GB: u64 = 1 << 30;
        // fits
        assert!(memory_verdict(44_000_000, 1200, 1, 9 * GB, 32 * GB).is_ok());
        // the first scan of a photo: a lower resolution helps
        let e = memory_verdict(708_000_000, 4800, 1, 19 * GB, 16 * GB).unwrap_err();
        assert!(e.contains("708 megapixels at 4800 dpi") && e.contains("about 19 GB") && e.contains("has 16 GB"), "{e}");
        assert!(e.ends_with("Choose a lower resolution."), "{e}");
        // the second: the first one has to go
        let e = memory_verdict(708_000_000, 4800, 2, 19 * GB, 16 * GB).unwrap_err();
        assert!(e.contains("Start over"), "{e}");
        // a third: the two that are there can be combined
        let e = memory_verdict(708_000_000, 4800, 3, 26 * GB, 24 * GB).unwrap_err();
        assert!(e.contains("Combining 3 scans") && e.contains("Press Process"), "{e}");
    }

    /// A worker without a scanner or a window, fed picture files.
    fn test_worker() -> (Worker, Receiver<Ev>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let w = Worker {
            tx,
            ctx: egui::Context::default(),
            scanner: None,
            scans: Default::default(),
            dpi: DEFAULT_DPI,
            processed: false,
            combined: None,
            photo_mm: None,
            half_turn_only: false,
        };
        (w, rx)
    }

    #[test]
    fn all_scans_of_a_photo_have_one_resolution() {
        // two small picture files to stand in for the scanner
        let dir = std::env::temp_dir().join(format!("multipass-worker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for i in 0..2 {
            let m = Mat::new_rows_cols_with_default(40, 60, CV_8UC3, Scalar::all(100.0 + i as f64)).unwrap();
            let path = dir.join(format!("{i}.png"));
            std::fs::write(&path, output::encode_png(&m, 1200.0).unwrap()).unwrap();
            files.push(path.to_string_lossy().into_owned());
        }
        std::env::set_var("MULTIPASS_FAKE_SCANS", files.join(","));
        let (mut w, rx) = test_worker();

        // the first scan sets the resolution, and its size is what the result will have
        assert_eq!(w.expected(), None);
        w.scan_into(None, false, 2400).unwrap();
        assert_eq!((w.count(), w.dpi), (1, 2400));
        let size = w.expected().unwrap();
        assert_eq!(size, ScanSize { width_px: 60, height_px: 40, dpi: 2400 });
        assert_eq!(size.pixels(), 2400);
        assert!((size.mm().0 - 0.635).abs() < 1e-9 && (size.mm().1 - 0.4233).abs() < 1e-4);
        // a second scan at another resolution is refused, and nothing changes
        let e = w.scan_into(None, false, 1200).unwrap_err();
        assert!(e.contains("taken at 2400 dpi") && e.contains("Start over"), "{e}");
        assert_eq!((w.count(), w.dpi), (1, 2400));
        // at the same resolution it is taken
        w.scan_into(None, false, 2400).unwrap();
        assert_eq!((w.count(), w.dpi), (2, 2400));
        // taking one of two again at another resolution is refused too
        assert!(w.scan_into(Some(0), false, 600).is_err());
        // the only scan held may be replaced at any resolution
        w.handle(Cmd::Remove(1));
        w.scan_into(Some(0), false, 600).unwrap();
        assert_eq!((w.count(), w.dpi), (1, 600));
        // after Start over, any resolution
        w.handle(Cmd::Reset);
        w.scan_into(None, false, 4800).unwrap();
        assert_eq!((w.count(), w.dpi), (1, 4800));

        std::env::remove_var("MULTIPASS_FAKE_SCANS");
        let _ = std::fs::remove_dir_all(&dir);
        let events: Vec<Ev> = rx.try_iter().collect();
        assert_eq!(events.iter().filter(|ev| matches!(ev, Ev::Captured { .. })).count(), 4);
        // the window is told the expected size after every change: four scans, a removal, a reset
        let told: Vec<Option<i32>> =
            events.iter().filter_map(|ev| if let Ev::Expected(s) = ev { Some(s.map(|s| s.dpi)) } else { None }).collect();
        assert_eq!(told, [Some(2400), Some(2400), Some(2400), Some(600), None, Some(4800)]);
        // the detail of a photo is judged on its first scan only: the first one, the one that replaced
        // the only scan held, and the first after Start over (which also withdraws the old verdict).
        // These test pictures are too small to judge.
        let judged: Vec<bool> =
            events.iter().filter_map(|ev| if let Ev::Detail(d) = ev { Some(d.is_some()) } else { None }).collect();
        assert_eq!(judged, [false, false, false, false]);
    }

    #[test]
    fn some_memory_is_left_to_the_system() {
        const GB: u64 = 1 << 30;
        assert_eq!(usable_memory(8 * GB), 6 * GB);
        assert_eq!(usable_memory(16 * GB), 14 * GB);
        assert_eq!(usable_memory(64 * GB), 64 * GB - 64 * GB / 10);
        // a very small machine: half
        assert_eq!(usable_memory(2 * GB), GB);
    }
}

/// Scanner output (8-bit RGB) -> OpenCV matrix
fn raw_to_mat(raw: &RawImage) -> Result<Mat, String> {
    if raw.channels != 3 {
        return Err("the colour scan came back in grey".into());
    }
    let mut m = Mat::new_rows_cols_with_default(raw.height as i32, raw.width as i32, CV_8UC3, Scalar::all(0.0))
        .map_err(|e| e.to_string())?;
    m.data_bytes_mut().map_err(|e| e.to_string())?.copy_from_slice(&raw.data);
    Ok(m)
}

/// RGB 8-bit matrix -> egui image no larger than `max_side`
fn to_color_image(m: &Mat, max_side: i32) -> Result<ColorImage, String> {
    let (w, h) = (m.cols(), m.rows());
    let s = (max_side as f64 / w.max(h) as f64).min(1.0);
    let size = Size::new(((w as f64 * s).round() as i32).max(1), ((h as f64 * s).round() as i32).max(1));
    let mut small = Mat::default();
    imgproc::resize(m, &mut small, size, 0.0, 0.0, imgproc::INTER_AREA).map_err(|e| e.to_string())?;
    let bytes = small.data_bytes().map_err(|e| e.to_string())?;
    Ok(ColorImage::from_rgb([size.width as usize, size.height as usize], bytes))
}
