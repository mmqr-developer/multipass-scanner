//! Multi-pass Scanner: scan one photo in two to four orientations on an Epson
//! flatbed and combine the scans into one cleaner image.

// On Windows: a window program, without a console window opening beside it.
// `platform::attach_console` connects the output to a terminal it is run from.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod combine;
mod cvx;
mod detail;
mod detect;
mod output;
mod platform;
mod post;
mod worker;

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, Color32, RichText, TextureHandle, TextureOptions, Vec2};
use opencv::prelude::*;
use opencv::{imgcodecs, imgproc};

use worker::{Cmd, Ev, ScanSize, Worker, DEFAULT_DPI, MIN_SCANS, PASSES, RESOLUTIONS};

const SLOT_LABELS: [&str; PASSES] = ["1 \u{b7} as placed", "2 \u{b7} \u{bc} turn", "3 \u{b7} \u{bd} turn", "4 \u{b7} \u{be} turn"];
/// For a photo too long to lie across the glass.
const SLOT_LABELS_LONG: [&str; PASSES] =
    ["1 \u{b7} as placed", "2 \u{b7} \u{bd} turn", "3 \u{b7} as placed (extra)", "4 \u{b7} \u{bd} turn (extra)"];

struct App {
    tx: Sender<Cmd>,
    rx: Receiver<Ev>,
    /// thumbnail of each scan
    slots: [Option<TextureHandle>; PASSES],
    /// larger view of each scan, shown above the thumbnails
    previews: [Option<TextureHandle>; PASSES],
    /// the combined image, once processed
    result: Option<TextureHandle>,
    /// the scan taken most recently
    last: Option<usize>,
    /// the thumbnail the user picked, to scan it again or remove it
    selected: Option<usize>,
    /// the slot being scanned right now
    scanning: Option<usize>,
    /// the photo is too long to turn a quarter turn on the glass
    half_turn_only: bool,
    /// the "Full-size photo" box: scan the whole glass, without looking for the photo
    full_size: bool,
    /// the "Resolution" pulldown: what the colour scans are taken at
    dpi: i32,
    /// the size the result of the scans held will have, known from the first scan on
    expected: Option<ScanSize>,
    /// the size of the saved result and of its file
    saved_size: Option<(ScanSize, u64)>,
    /// whether the resolution suits this picture, judged from its first scan
    detail: Option<String>,
    status: String,
    scanner_name: Option<String>,
    busy: bool,
    progress: Option<(f32, String)>,
    error: Option<String>,
    saved: Option<PathBuf>,
    /// the post-processing the saved result was made with
    saved_post: Option<post::Options>,
    report: Vec<String>,
    /// the "Post-process the combined image" box and its two options
    post_enabled: bool,
    post_texture: bool,
    post_lint: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        Worker::spawn(cmd_rx, ev_tx, cc.egui_ctx.clone());
        cc.egui_ctx.set_zoom_factor(1.15);
        App {
            tx: cmd_tx,
            rx: ev_rx,
            slots: Default::default(),
            previews: Default::default(),
            result: None,
            last: None,
            selected: None,
            scanning: None,
            half_turn_only: false,
            full_size: false,
            dpi: DEFAULT_DPI,
            expected: None,
            saved_size: None,
            detail: None,
            status: String::new(),
            scanner_name: None,
            busy: false,
            progress: None,
            error: None,
            saved: None,
            saved_post: None,
            report: Vec::new(),
            post_enabled: false,
            post_texture: true,
            post_lint: true,
        }
    }

    fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    fn first_empty(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_none())
    }

    /// What Process would do to the combined image, as the boxes are set now.
    fn post_options(&self) -> post::Options {
        post::Options { texture: self.post_enabled && self.post_texture, lint: self.post_enabled && self.post_lint }
    }

    /// The scans changed, so a result made from them is out of date.
    fn forget_result(&mut self) {
        self.saved = None;
        self.saved_size = None;
        self.saved_post = None;
        self.result = None;
        self.report.clear();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Busy(b) => {
                    self.busy = b;
                    if b {
                        self.error = None;
                    } else {
                        self.scanning = None;
                    }
                }
                Ev::Status(s) => self.status = s,
                Ev::Scanner(n) => self.scanner_name = Some(n),
                Ev::Captured { slot, thumb, preview, half_turn_only } => {
                    self.slots[slot] = Some(ctx.load_texture(format!("slot{slot}"), thumb, TextureOptions::LINEAR));
                    self.previews[slot] =
                        Some(ctx.load_texture(format!("preview{slot}"), preview, TextureOptions::LINEAR));
                    self.last = Some(slot);
                    self.selected = None;
                    self.half_turn_only = half_turn_only;
                    self.forget_result();
                }
                Ev::Removed { slot } => {
                    self.slots[slot] = None;
                    self.previews[slot] = None;
                    self.selected = None;
                    self.last = (0..PASSES).rev().find(|&i| self.slots[i].is_some());
                    if self.last.is_none() {
                        self.half_turn_only = false;
                    }
                    self.forget_result();
                }
                Ev::Cleared => {
                    self.slots = Default::default();
                    self.previews = Default::default();
                    self.last = None;
                    self.selected = None;
                    self.half_turn_only = false;
                    self.forget_result();
                }
                Ev::Expected(size) => self.expected = size,
                Ev::Detail(text) => self.detail = text,
                Ev::Progress(p) => self.progress = p,
                Ev::Saved { path, preview, report, post, size, bytes } => {
                    // Also on stderr, so a run from a terminal keeps a log
                    // (the window shows it only as the result's tooltip).
                    eprintln!("saved {}", path.display());
                    for line in &report {
                        eprintln!("{line}");
                    }
                    self.result = Some(ctx.load_texture("result", preview, TextureOptions::LINEAR));
                    self.saved = Some(path);
                    self.saved_size = Some((size, bytes));
                    self.saved_post = Some(post);
                    self.report = report;
                    self.selected = None;
                }
                Ev::Error(e) => self.error = Some(e),
            }
        }
    }

    fn instruction(&self) -> String {
        if self.busy {
            return self.status.clone();
        }
        const AGAIN: &str = "To take one scan again, click its thumbnail.";
        if let Some(i) = self.selected {
            return format!(
                "Scan {n} is selected. To replace it, place the photo for that scan and press Scan {n} again.\n\
                 Click the thumbnail again to leave it as it is.",
                n = i + 1
            );
        }
        if let Some(p) = &self.saved {
            let shown = match std::env::var_os("HOME").map(PathBuf::from) {
                Some(home) => match p.strip_prefix(&home) {
                    Ok(rest) => format!("~/{}", rest.display()),
                    Err(_) => p.display().to_string(),
                },
                None => p.display().to_string(),
            };
            return format!(
                "Saved {shown}.\nPut the next photo face down against the top edge of the glass and press \
                 Scan next photo. To redo a scan of this one, click its thumbnail."
            );
        }
        let n = self.count();
        let turn = if self.half_turn_only || self.full_size {
            "Turn the photo half round, so that its other end is against the top edge"
        } else {
            "Turn the photo a quarter turn (either way) and slide it to a new spot along the top edge"
        };
        match n {
            0 if self.full_size => {
                "Place the photo face down on the glass, close the lid, then press Scan. The whole glass is scanned.".into()
            }
            0 => "Place the photo face down against the top edge of the glass, close the lid, then press Scan.".into(),
            1 if self.full_size => format!("{turn}, then press Scan."),
            1 if self.half_turn_only => format!(
                "This photo is too long to lie across the glass, so it gets two scans instead of four.\n\
                 {turn}, then press Scan."
            ),
            1 => format!("{turn}, then press Scan.  (1 scan done)"),
            _ if n == PASSES => format!("All four scans are done. Check them, then press Process.\n{AGAIN}"),
            _ if self.half_turn_only => {
                format!("Both ways round are scanned. Check them, then press Process.\n{AGAIN}")
            }
            _ => format!("{turn}, then press Scan. Or press Process to combine these {n} scans.\n{AGAIN}"),
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let n = self.count();
        ui.horizontal(|ui| {
            // Where a scan would go: the selected slot, else the first empty one
            // (after a save: the first slot, for the next photo).
            let target = match (self.selected, &self.saved) {
                (Some(i), _) => Some(i),
                (None, Some(_)) => Some(0),
                (None, None) => self.first_empty(),
            };
            let label = match (self.selected, &self.saved) {
                (Some(i), _) => format!("Scan {} again", i + 1),
                (None, Some(_)) => "Scan next photo".to_string(),
                (None, None) => "Scan".to_string(),
            };
            let can_scan = !self.busy && target.is_some();
            let scan = ui.add_enabled(
                can_scan,
                egui::Button::new(RichText::new(label).size(22.0).strong()).min_size(Vec2::new(180.0, 48.0)),
            );
            // Space and Enter scan, unless they belong to a widget with keyboard focus.
            let key = can_scan
                && ui.memory(|m| m.focused().is_none())
                && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter));
            if scan.clicked() || key {
                self.error = None;
                let _ = self.tx.send(Cmd::Scan { slot: self.selected, full_size: self.full_size, dpi: self.dpi });
                self.busy = true;
                self.scanning = target;
                self.status = "Starting\u{2026}".into();
            }
            ui.add_space(8.0);
            // Once saved, Process is for a different choice of post-processing:
            // that makes another file from the same scans.
            let post = self.post_options();
            let process = ui
                .add_enabled(
                    !self.busy && n >= MIN_SCANS && self.saved_post != Some(post),
                    egui::Button::new(RichText::new("Process").size(22.0).strong()).min_size(Vec2::new(150.0, 48.0)),
                )
                .on_hover_text(
                    "Combine the scans and save the result in Pictures. About 2 minutes at 1200 dpi; \
                     much longer at 2400 and 4800 dpi.",
                )
                .on_disabled_hover_text(if self.saved.is_some() {
                    "These scans are already combined and saved this way"
                } else {
                    "Needs at least two scans"
                });
            if process.clicked() {
                self.error = None;
                self.selected = None;
                let _ = self.tx.send(Cmd::Process { post });
                self.busy = true;
                self.status = "Starting\u{2026}".into();
            }
            ui.add_space(12.0);
            let remove = match self.selected {
                Some(i) => format!("Remove scan {}", i + 1),
                None => "Remove scan".to_string(),
            };
            if ui
                .add_enabled(!self.busy && self.selected.is_some(), egui::Button::new(remove))
                .on_hover_text("Forget the selected scan without taking it again")
                .on_disabled_hover_text("Click a thumbnail first")
                .clicked()
            {
                if let Some(i) = self.selected {
                    let _ = self.tx.send(Cmd::Remove(i));
                }
            }
            if ui.add_enabled(!self.busy && (n > 0 || self.saved.is_some()), egui::Button::new("Start over")).clicked() {
                let _ = self.tx.send(Cmd::Reset);
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_enabled(!self.busy, egui::Checkbox::new(&mut self.full_size, "Full-size photo: scan the whole glass"))
                .on_hover_text(
                    "For a photo that covers most of the glass. The app does not look for the photo's edges: \
                     it scans the whole glass in colour at the chosen resolution. The photo is then scanned \
                     as placed and turned half round.",
                );
            ui.add_space(20.0);
            // All scans of one photo have the same resolution: it can be
            // changed before the first scan and after a result is saved.
            let free = !self.busy && (n == 0 || self.saved.is_some());
            ui.add_enabled_ui(free, |ui| {
                ui.label("Resolution:");
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(format!("{} dpi", self.dpi))
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        for dpi in RESOLUTIONS {
                            ui.selectable_value(&mut self.dpi, dpi, format!("{dpi} dpi"));
                        }
                    })
                    .response
                    .on_hover_text(
                        "The resolution of the colour scans. 1200 dpi suits most prints. Each step up has \
                         4 times the pixels: scans and combining take much longer and need much more memory.",
                    )
                    .on_disabled_hover_text(
                        "All scans of one photo have the same resolution. Press Start over to change it.",
                    );
            });
            if let Some(note) = resolution_note(self.dpi) {
                ui.label(RichText::new(note).weak());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled(!self.busy, egui::Checkbox::new(&mut self.post_enabled, "Post-process the combined image:"))
                .on_hover_text(
                    "Clean up the combined image before it is saved. After a save, change these boxes and \
                     press Process again to save another version from the same scans.",
                );
            let on = !self.busy && self.post_enabled;
            ui.add_enabled(on, egui::Checkbox::new(&mut self.post_texture, "Remove texture patterns")).on_hover_text(
                "Takes out the regular surface pattern of prints on silk or honeycomb paper. \
                 A print without such a pattern is left as it is.",
            );
            ui.add_enabled(on, egui::Checkbox::new(&mut self.post_lint, "Remove lint")).on_hover_text(
                "Fills in dust specks and fibres lying on the print: small, thin, hard-edged marks. \
                 Larger or softer marks are left alone.",
            );
        });
        // How large the result will be: known once the first scan is there.
        // The line is always present, so that nothing below it moves.
        ui.label(RichText::new(result_note(self.expected, self.saved_size)).weak())
            .on_hover_text(
                "The result has the frame of the lowest-numbered scan. Its file size is estimated from \
                 earlier results (4.8 bytes a pixel) until it is saved.",
            );
        // Whether the resolution suits this picture, from its first scan.
        // Always a line, like the one above.
        ui.label(self.detail.as_deref().unwrap_or(" ")).on_hover_text(
            "From the first scan of the photo: how fine the finest detail is that stands out from the scanner's \
             noise (detail weaker than a quarter of the noise does not count). A finer scan has less noise \
             at every size of detail, so it shows finer detail of the same print, until the print has none left.",
        );
        if !self.busy && self.selected.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected = None;
        }
    }

    /// The four thumbnails.  A filled one can be clicked to select it.
    fn thumbnails(&mut self, ui: &mut egui::Ui) {
        let w = (ui.available_width() - 3.0 * 12.0) / PASSES as f32;
        let labels = if self.half_turn_only || self.full_size { &SLOT_LABELS_LONG } else { &SLOT_LABELS };
        // the slot the next scan goes to, outlined
        let target = match self.scanning {
            Some(i) => Some(i),
            None if self.busy => None, // combining: no scan is coming
            None if self.selected.is_some() => self.selected,
            None if self.saved.is_some() => None,
            None => self.first_empty(),
        };
        let mut clicked = None;
        ui.horizontal(|ui| {
            for (i, label) in labels.iter().enumerate() {
                let selected = self.selected == Some(i);
                // one stroke width for all, so that nothing moves when the outline changes
                let stroke = egui::Stroke::new(
                    2.0,
                    if selected || target == Some(i) {
                        ui.visuals().selection.stroke.color
                    } else {
                        ui.visuals().widgets.noninteractive.bg_stroke.color
                    },
                );
                let mut frame = egui::Frame::group(ui.style()).stroke(stroke);
                if selected {
                    frame = frame.fill(ui.visuals().selection.bg_fill.gamma_multiply(0.35));
                }
                let shown = frame.show(ui, |ui| {
                    ui.set_width(w - 16.0);
                    ui.set_height(210.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(*label).strong());
                        if self.scanning == Some(i) && self.busy {
                            ui.add_space(70.0);
                            ui.label(RichText::new("scanning\u{2026}").weak());
                        } else if let Some(t) = &self.slots[i] {
                            let s = t.size_vec2();
                            let k = ((w - 24.0) / s.x).min(180.0 / s.y);
                            ui.image((t.id(), s * k));
                        } else {
                            ui.add_space(70.0);
                            ui.label(RichText::new("not scanned yet").weak());
                        }
                    });
                });
                if self.slots[i].is_some() && !self.busy {
                    let r = ui
                        .interact(shown.response.rect, ui.scope_id().with(("slot", i)), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(if selected {
                            "Click to deselect"
                        } else {
                            "Click to look at this scan, take it again or remove it"
                        });
                    if r.clicked() {
                        clicked = Some(i);
                    }
                }
                if i + 1 < PASSES {
                    ui.add_space(4.0);
                }
            }
        });
        if let Some(i) = clicked {
            self.selected = if self.selected == Some(i) { None } else { Some(i) };
            self.error = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Multi-pass Scanner");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(match &self.scanner_name {
                        Some(n) => RichText::new(n.as_str()).weak(),
                        None => RichText::new("no scanner connected yet").weak(),
                    });
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("slots").exact_size(250.0).show(ui, |ui| {
            ui.add_space(6.0);
            self.thumbnails(ui);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(self.instruction()).size(17.0));
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{e}")).color(Color32::from_rgb(0xd0, 0x3b, 0x3b)).size(16.0));
            }
            ui.add_space(8.0);
            self.controls(ui);
            if let Some((f, t)) = &self.progress {
                ui.add_space(8.0);
                ui.add(egui::ProgressBar::new(*f).text(format!("{t} \u{2014} {:.0}%", f * 100.0)));
            } else if self.busy {
                ui.add_space(8.0);
                ui.add(egui::Spinner::new());
            }
            ui.add_space(8.0);
            // The large picture: the selected scan, else the combined result,
            // else the scan taken last.
            let picked = self.selected.and_then(|i| self.previews[i].as_ref());
            let is_result = picked.is_none() && self.result.is_some();
            let large = picked
                .or(self.result.as_ref())
                .or_else(|| self.last.and_then(|i| self.previews[i].as_ref()));
            if let Some(t) = large {
                let avail = ui.available_size();
                let s = t.size_vec2();
                let k = (avail.x / s.x).min((avail.y - 4.0) / s.y).min(1.0);
                ui.vertical_centered(|ui| {
                    let r = ui.image((t.id(), s * k));
                    if is_result && !self.report.is_empty() {
                        r.on_hover_text(self.report.join("\n"));
                    }
                });
            }
        });
    }
}

/// The line about the result's size: what was saved, else what the scans held
/// will give, else that it is not known yet.
fn result_note(expected: Option<ScanSize>, saved: Option<(ScanSize, u64)>) -> String {
    if let Some((size, bytes)) = saved {
        return format!(
            "Saved image: {} \u{d7} {} pixels at {} dpi, {}, {}",
            size.width_px,
            size.height_px,
            size.dpi,
            output::megapixels(size.pixels()),
            output::human_size(bytes)
        );
    }
    match expected {
        Some(size) => {
            let (w, h) = size.mm();
            format!(
                "Scanned area: {w:.0} \u{d7} {h:.0} mm at {} dpi. The result will be about {}, a file of about {}.",
                size.dpi,
                output::megapixels(size.pixels()),
                output::human_size(output::estimated_file_size(size.pixels()))
            )
        }
        None => "Result size: known after the first scan.".to_string(),
    }
}

/// What to expect of a resolution other than the usual one.
fn resolution_note(dpi: i32) -> Option<&'static str> {
    match dpi {
        600 => Some("a quarter of the pixels of 1200 dpi: quick, small files"),
        2400 => Some("4 times the pixels of 1200 dpi: slower, larger files"),
        4800 => Some("16 times the pixels of 1200 dpi: very slow, very large files"),
        _ => None,
    }
}

/// Read a picture file.  The file is read here and decoded from memory:
/// OpenCV's own file reading cannot open a path with non-ASCII characters on
/// Windows.  An empty result means the file is not a picture OpenCV knows.
fn imread(path: &str, flags: i32) -> Result<opencv::core::Mat, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let bytes = opencv::core::Vector::<u8>::from_slice(&bytes);
    imgcodecs::imdecode(&bytes, flags).map_err(|e| e.to_string())
}

pub fn load_rgb(path: &str) -> Result<opencv::core::Mat, String> {
    let bgr = imread(path, imgcodecs::IMREAD_COLOR)?;
    if bgr.empty() {
        return Err(format!("cannot read {path}"));
    }
    let mut rgb = opencv::core::Mat::default();
    cvx::cvt_color(&bgr, &mut rgb, imgproc::COLOR_BGR2RGB, 0).map_err(|e| e.to_string())?;
    Ok(rgb)
}

/// A PNG as 16-bit RGB (an 8-bit file is widened).
fn load_rgb16(path: &str) -> Result<opencv::core::Mat, String> {
    let bgr = imread(path, imgcodecs::IMREAD_UNCHANGED)?;
    if bgr.empty() || bgr.channels() != 3 {
        return Err(format!("cannot read {path} as a colour image"));
    }
    let mut rgb = opencv::core::Mat::default();
    cvx::cvt_color(&bgr, &mut rgb, imgproc::COLOR_BGR2RGB, 0).map_err(|e| e.to_string())?;
    if rgb.depth() == opencv::core::CV_16U {
        return Ok(rgb);
    }
    let mut wide = opencv::core::Mat::default();
    rgb.convert_to(&mut wide, opencv::core::CV_16U, 257.0, 0.0).map_err(|e| e.to_string())?;
    Ok(wide)
}

/// `multipass-scanner --post texture,lint IN.png OUT.png`: post-process an
/// image that was combined before.
fn cli_post(what: &str, input: &str, out: &str, dpi: i32) -> Result<(), String> {
    let mut opts = post::Options::default();
    for w in what.split(',') {
        match w.trim() {
            "texture" => opts.texture = true,
            "lint" => opts.lint = true,
            other => return Err(format!("unknown step '{other}': use texture, lint or texture,lint")),
        }
    }
    let mut img = load_rgb16(input)?;
    let t = std::time::Instant::now();
    let mut report = Vec::new();
    post::apply(&mut img, opts, dpi as f64, &|_, s| eprintln!("{:6.1} s  {s}", t.elapsed().as_secs_f64()), &mut report)?;
    for line in &report {
        eprintln!("{line}");
    }
    let png = output::encode_png(&img, dpi as f64)?;
    std::fs::write(out, png).map_err(|e| e.to_string())?;
    eprintln!("wrote {out} in {:.1} s", t.elapsed().as_secs_f64());
    Ok(())
}

/// `multipass-scanner --detail SCAN.png`: how fine the detail in a saved scan
/// is, and what the window would say about its resolution.
fn cli_detail(path: &str, dpi: i32) -> Result<(), String> {
    let scan = load_rgb(path)?;
    let t = std::time::Instant::now();
    let d = detail::analyse(&scan, dpi as f64).ok_or("the scan is too small, or too much of it is clipped, to be judged")?;
    println!("{} tiles decide; analysed in {:.1} s", d.tiles, t.elapsed().as_secs_f64());
    println!("  detail of ... dpi   power over the noise level");
    let n = d.spectrum.len();
    for r in (n / 16..n).step_by(n / 16) {
        let mean = d.spectrum[r - 2..r + 3].iter().sum::<f32>() / 5.0;
        println!("  {:>12.0}        {mean:8.2}", r as f64 / n as f64 * dpi as f64);
    }
    println!("{}", detail::advice(&d, dpi));
    Ok(())
}

/// `multipass-scanner --combine OUT.png SCAN1 SCAN2 ...` (scans taken at `dpi`, no scanner needed)
fn cli_combine(args: &[String], dpi: i32) -> Result<(), String> {
    let out = &args[0];
    let scans: Vec<_> = args[1..].iter().map(|p| load_rgb(p)).collect::<Result<_, _>>()?;
    let t = std::time::Instant::now();
    // MULTIPASS_MEM=1 prints the peak memory of every stage (where the system
    // can tell): the peak counter is read and reset each time the progress
    // text changes.
    let trace = platform::MEMORY_TRACE && std::env::var_os("MULTIPASS_MEM").is_some();
    let last = std::cell::RefCell::new(String::from("loading the scans"));
    let stage_done = |name: &str| {
        let (peak, held) = platform::memory_gb();
        eprintln!("\r  peak {peak:5.2} GB, held after {held:5.2} GB   {name:<50}");
        platform::reset_peak_memory();
    };
    let res = combine::combine(&scans, dpi as f64, &|f, s| {
        if trace {
            // "Resampling scan 3" and "Resampling scan 4" are one stage
            let key: String = s.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end().to_string();
            if *last.borrow() != key {
                stage_done(&last.borrow());
                *last.borrow_mut() = key;
            }
        } else {
            eprint!("\r{:3.0}%  {:<50}", f * 100.0, s)
        }
    })?;
    if trace {
        stage_done(&last.borrow());
    }
    eprintln!();
    for line in &res.report {
        eprintln!("{line}");
    }
    let png = output::encode_png(&res.image, dpi as f64)?;
    std::fs::write(out, png).map_err(|e| e.to_string())?;
    eprintln!("wrote {out} in {:.1} s", t.elapsed().as_secs_f64());
    Ok(())
}

/// `multipass-scanner --capture-full DIR`: one full-size scan (the whole glass
/// in colour, no grey scans), saved as DIR/full_scan.png.
fn cli_capture_full(dir: &str, dpi: i32) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut sc = flatbed::open_first()?;
    let (glass_w, glass_h) = sc.glass_mm();
    eprintln!("scanner: {} (glass {glass_w:.1} x {glass_h:.1} mm)", sc.name());
    for note in sc.notes() {
        eprintln!("  {note}");
    }
    let t = std::time::Instant::now();
    let img = worker::capture_whole_glass(
        sc.as_mut(),
        1,
        dpi,
        &|s| eprintln!("{:6.1} s  {s}", t.elapsed().as_secs_f64()),
        &|_| Ok(()),
    )?;
    eprintln!("colour scan {}x{} px; {:.1} s", img.cols(), img.rows(), t.elapsed().as_secs_f64());
    std::fs::write(format!("{dir}/full_scan.png"), output::encode_png(&img, dpi as f64)?).map_err(|e| e.to_string())?;
    Ok(())
}

/// `multipass-scanner --capture DIR [WIDTH_MM LENGTH_MM]`: run one pass and keep
/// every image.  With a known photo size the grey length scan is skipped, as on
/// the later scans in the app.
fn cli_capture(dir: &str, known: Option<(f64, f64)>, dpi: i32) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut sc = flatbed::open_first()?;
    let (glass_w, glass_h) = sc.glass_mm();
    eprintln!("scanner: {} (glass {glass_w:.1} x {glass_h:.1} mm)", sc.name());
    for note in sc.notes() {
        eprintln!("  {note}");
    }
    let t = std::time::Instant::now();
    let c = worker::capture_photo(
        sc.as_mut(),
        if known.is_some() { 2 } else { 1 },
        known,
        dpi,
        &|s| eprintln!("{:6.1} s  {s}", t.elapsed().as_secs_f64()),
        &|_| Ok(()),
    )?;
    eprintln!("photo {:.1} x {:.1} mm (width x length)", c.photo_mm.0, c.photo_mm.1);
    eprintln!("area {:.1},{:.1} {:.1} x {:.1} mm; colour scan {}x{} px; {:.1} s",
        c.area.x, c.area.y, c.area.w, c.area.h, c.image.cols(), c.image.rows(), t.elapsed().as_secs_f64());
    let gray = |r: &flatbed::RawImage| -> Result<opencv::core::Mat, String> {
        let mut m = opencv::core::Mat::new_rows_cols_with_default(r.height as i32, r.width as i32,
            opencv::core::CV_8UC1, opencv::core::Scalar::all(0.0)).map_err(|e| e.to_string())?;
        m.data_bytes_mut().map_err(|e| e.to_string())?.copy_from_slice(&r.data);
        Ok(m)
    };
    let write = |name: &str, m: &opencv::core::Mat| -> Result<(), String> {
        let p = format!("{dir}/{name}");
        imgcodecs::imwrite(&p, m, &opencv::core::Vector::new()).map_err(|e| e.to_string())?;
        Ok(())
    };
    write("step1_strip.png", &gray(&c.strip)?)?;
    if let Some(col) = &c.column {
        write("step2_column.png", &gray(col)?)?;
    }
    std::fs::write(format!("{dir}/step3_scan.png"), output::encode_png(&c.image, dpi as f64)?).map_err(|e| e.to_string())?;
    Ok(())
}

/// `multipass-scanner --detect STRIP.png [COLUMN.png]`: rerun photo detection on
/// saved 75 dpi grey scans (from --capture).
fn cli_detect(strip: &str, column: Option<&str>) -> Result<(), String> {
    let load = |p: &str| -> Result<flatbed::RawImage, String> {
        let m = imread(p, imgcodecs::IMREAD_GRAYSCALE)?;
        if m.empty() {
            return Err(format!("cannot read {p}"));
        }
        Ok(flatbed::RawImage {
            width: m.cols() as usize,
            height: m.rows() as usize,
            channels: 1,
            data: m.data_bytes().map_err(|e| e.to_string())?.to_vec(),
        })
    };
    let mmpp = 25.4 / 75.0;
    let s = load(strip)?;
    match detect::find_columns(&s, 75.0) {
        Some((x0, x1)) => println!(
            "{strip}: photo x {:.1}..{:.1} mm (width {:.1} mm), top at {:.1} mm",
            x0 as f64 * mmpp,
            x1 as f64 * mmpp,
            (x1 - x0) as f64 * mmpp,
            detect::find_top(&s, x0, x1) as f64 * mmpp
        ),
        None => println!("{strip}: no photo"),
    }
    if let Some(c) = column {
        match detect::find_rows(&load(c)?, 75.0) {
            Some((y0, y1)) => println!("{c}: photo y {:.1}..{:.1} mm (length {:.1} mm)",
                y0 as f64 * mmpp, y1 as f64 * mmpp, (y1 - y0) as f64 * mmpp),
            None => println!("{c}: no photo"),
        }
    }
    Ok(())
}

fn main() {
    platform::attach_console();
    let mut args: Vec<String> = std::env::args().collect();
    // `--dpi N` before a mode: the resolution of the scans it takes or is given
    // (1200 without it).
    let mut dpi = DEFAULT_DPI;
    if args.len() >= 3 && args[1] == "--dpi" {
        match args[2].parse::<i32>() {
            Ok(v) if (50..=9600).contains(&v) => dpi = v,
            _ => {
                eprintln!("usage: multipass-scanner --dpi N ...   (N: the scans' resolution, for example 2400)");
                std::process::exit(2);
            }
        }
        args.drain(1..3);
    }
    if (args.len() == 3 || args.len() == 4) && args[1] == "--detect" {
        if let Err(e) = cli_detect(&args[2], args.get(3).map(|s| s.as_str())) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.len() == 3 && args[1] == "--capture-full" {
        let dir = args[2].clone();
        // scanner modes: some systems need the main thread's event loop meanwhile
        if let Err(e) = flatbed::run_with_main_loop(move || cli_capture_full(&dir, dpi)) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if (args.len() == 3 || args.len() == 5) && args[1] == "--capture" {
        let known = if args.len() == 5 {
            match (args[3].parse::<f64>(), args[4].parse::<f64>()) {
                (Ok(a), Ok(b)) => Some((a, b)),
                _ => {
                    eprintln!("usage: multipass-scanner --capture DIR [WIDTH_MM LENGTH_MM]");
                    std::process::exit(2);
                }
            }
        } else {
            None
        };
        let dir = args[2].clone();
        if let Err(e) = flatbed::run_with_main_loop(move || cli_capture(&dir, known, dpi)) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.len() == 3 && args[1] == "--detail" {
        if let Err(e) = cli_detail(&args[2], dpi) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.len() == 5 && args[1] == "--post" {
        if let Err(e) = cli_post(&args[2], &args[3], &args[4], dpi) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.len() > 3 && args[1] == "--combine" {
        if let Err(e) = cli_combine(&args[2..], dpi) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    let viewport = egui::ViewportBuilder::default()
        .with_title("Multi-pass Scanner")
        .with_app_id("multipass-scanner")
        .with_inner_size([1180.0, 900.0])
        .with_min_inner_size([760.0, 600.0]);
    // Windows takes the icon in the title bar and the taskbar from the window,
    // not from the .exe; without this the GUI library shows its own.
    #[cfg(target_os = "windows")]
    let viewport = match eframe::icon_data::from_png_bytes(include_bytes!("../packaging/windows/icon-256.png")) {
        Ok(icon) => viewport.with_icon(icon),
        Err(_) => viewport,
    };
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    if let Err(e) = eframe::run_native("Multi-pass Scanner", options, Box::new(|cc| Ok(Box::new(App::new(cc))))) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_line_says_what_is_known() {
        assert_eq!(result_note(None, None), "Result size: known after the first scan.");
        // a 4 x 6 inch photo with its margins at 1200 dpi
        let scan = ScanSize { width_px: 5556, height_px: 7956, dpi: 1200 };
        assert_eq!(
            result_note(Some(scan), None),
            "Scanned area: 118 \u{d7} 168 mm at 1200 dpi. The result will be about 44.2 megapixels, a file of about 212 MB."
        );
        // once saved, the real figures
        let saved = ScanSize { width_px: 5550, height_px: 7950, dpi: 1200 };
        assert_eq!(
            result_note(Some(scan), Some((saved, 207_300_000))),
            "Saved image: 5550 \u{d7} 7950 pixels at 1200 dpi, 44.1 megapixels, 207 MB"
        );
    }
}
