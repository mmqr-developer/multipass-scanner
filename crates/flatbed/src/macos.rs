//! macOS backend: Apple's ImageCaptureCore, the framework behind the Image
//! Capture app.
//!
//! STATUS: works with an Epson Perfection V19 II on an Apple silicon Mac, at
//! 1200 dpi; the other resolutions have not been scanned there.  On a new Mac
//! or scanner, `cargo run -p flatbed --example scan-test` is the first thing
//! to run: it checks sizes, the origin of the scan area, repeatability and
//! shear by itself.
//!
//! # Threads
//!
//! ImageCaptureCore is asynchronous and delivers every callback on the main
//! thread.  The rest of the app wants a blocking `scan()` on a worker thread.
//! So:
//!
//! - every ImageCaptureCore object lives on the main thread, in [`SESSION`];
//! - the worker posts small closures to the main queue (`start_session`,
//!   `start_scan`, `end_session`) and then waits on a channel;
//! - the delegate's callbacks, on the main thread, answer through that channel.
//!
//! The main thread never blocks.  It does have to run an event loop: the GUI's
//! does, and command-line programs use [`run_with_main_loop`].

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult};
use objc2_foundation::{NSError, NSIndexSet, NSNotFound, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSURL};
use objc2_image_capture_core::{
    ICDevice, ICDeviceBrowser, ICDeviceBrowserDelegate, ICDeviceDelegate, ICDeviceLocationTypeMask, ICDeviceType,
    ICDeviceTypeMask, ICEXIFOrientationType, ICScannerBandData, ICScannerBitDepth, ICScannerDevice,
    ICScannerDeviceDelegate, ICScannerFeatureBoolean, ICScannerFeatureEnumeration, ICScannerFunctionalUnit,
    ICScannerFunctionalUnitType, ICScannerMeasurementUnit, ICScannerPixelDataType, ICScannerTransferMode,
};

use crate::bands::{Assembler, BandInfo};
use crate::{AreaMm, RawImage, Scanner};

/// How long to wait for a scanner to be found, opened and made ready.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// Once macOS has listed its local devices and none is a scanner, how much
/// longer to wait for one to turn up.
const NO_DEVICE_GRACE: Duration = Duration::from_secs(4);
/// Longest silence during a scan (the lamp may have to warm up first).
const SCAN_SILENCE: Duration = Duration::from_secs(180);
/// Smallest band to ask for, in bytes.  (NAPS2 uses this size with the same
/// framework.)
const MIN_BAND_BYTES: u32 = 65536;

// Error codes from ImageCaptureConstants.h
const ERR_SCAN_CANCELED: isize = -9924;
const ERR_IN_USE_LOCAL: isize = -9925;
const ERR_IN_USE_REMOTE: isize = -9926;
const ERR_UNSOLICITED_STATUS: isize = -9932;
const ERR_SESSION_ALREADY_OPEN: isize = -21347;
const ERR_NOT_AUTHORIZED: isize = -21343;

/// What the main thread tells the waiting worker.
enum Event {
    /// The session is open and the flatbed is selected.
    Ready { name: String, glass_mm: (f64, f64), notes: Vec<String> },
    /// Opening failed, or the device reported an error.
    Failed(String),
    /// macOS has listed every local device.
    Enumerated,
    /// A scanner was found and is being opened.
    Found(String),
    Band(BandInfo, Vec<u8>),
    /// The scan is over; `Some` is the error.
    ScanDone(Option<String>),
    /// The scanner was unplugged or switched off.
    Gone,
}

// ------------------------------------------------------- main thread: delegate

struct Ivars {
    tx: Sender<Event>,
    device: RefCell<Option<Retained<ICScannerDevice>>>,
    /// `deviceDidBecomeReady:` has arrived
    ready: Cell<bool>,
    /// the flatbed has been asked for
    select_requested: Cell<bool>,
    /// `Event::Ready` has been sent
    reported: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Delegate` does not
    // implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MultipassScannerDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl ICDeviceBrowserDelegate for Delegate {
        #[unsafe(method(deviceBrowser:didAddDevice:moreComing:))]
        fn browser_did_add(&self, _browser: &ICDeviceBrowser, device: &ICDevice, _more_coming: bool) {
            self.device_added(device);
        }

        #[unsafe(method(deviceBrowser:didRemoveDevice:moreGoing:))]
        fn browser_did_remove(&self, _browser: &ICDeviceBrowser, device: &ICDevice, _more_going: bool) {
            self.device_gone(device);
        }

        #[unsafe(method(deviceBrowserDidEnumerateLocalDevices:))]
        fn browser_did_enumerate(&self, _browser: &ICDeviceBrowser) {
            self.send(Event::Enumerated);
        }
    }

    unsafe impl ICDeviceDelegate for Delegate {
        #[unsafe(method(didRemoveDevice:))]
        fn did_remove_device(&self, device: &ICDevice) {
            self.device_gone(device);
        }

        #[unsafe(method(device:didOpenSessionWithError:))]
        fn did_open_session(&self, _device: &ICDevice, error: Option<&NSError>) {
            if let Some(e) = error {
                self.send(Event::Failed(format!("cannot open the scanner: {}", error_text(e))));
            }
            // Without an error there is nothing to do yet: `deviceDidBecomeReady:` follows.
        }

        #[unsafe(method(device:didCloseSessionWithError:))]
        fn did_close_session(&self, _device: &ICDevice, _error: Option<&NSError>) {}

        #[unsafe(method(deviceDidBecomeReady:))]
        fn did_become_ready(&self, _device: &ICDevice) {
            self.ivars().ready.set(true);
            self.select_flatbed();
        }

        #[unsafe(method(device:didEncounterError:))]
        fn did_encounter_error(&self, _device: &ICDevice, error: Option<&NSError>) {
            match error {
                // status reports such as "warming up" arrive this way too
                Some(e) if e.code() == ERR_UNSOLICITED_STATUS => {}
                Some(e) => self.send(Event::Failed(format!("the scanner reported a problem: {}", error_text(e)))),
                None => {}
            }
        }
    }

    unsafe impl ICScannerDeviceDelegate for Delegate {
        #[unsafe(method(scannerDevice:didSelectFunctionalUnit:error:))]
        fn did_select_unit(
            &self,
            _scanner: &ICScannerDevice,
            unit: Option<&ICScannerFunctionalUnit>,
            error: Option<&NSError>,
        ) {
            self.unit_selected(unit, error);
        }

        #[unsafe(method(scannerDevice:didScanToBandData:))]
        fn did_scan_band(&self, _scanner: &ICScannerDevice, data: Option<&ICScannerBandData>) {
            let Some(data) = data else { return };
            // SAFETY: plain property reads on the object the framework handed us.
            let (info, bytes) = unsafe {
                (
                    BandInfo {
                        width: data.fullImageWidth(),
                        height: data.fullImageHeight(),
                        bits_per_component: data.bitsPerComponent(),
                        bits_per_pixel: data.bitsPerPixel(),
                        components: data.numComponents(),
                        bytes_per_row: data.bytesPerRow(),
                        start_row: data.dataStartRow(),
                        rows: data.dataNumRows(),
                    },
                    data.dataBuffer().map(|d| d.to_vec()).unwrap_or_default(),
                )
            };
            self.send(Event::Band(info, bytes));
        }

        #[unsafe(method(scannerDevice:didScanToURL:))]
        fn did_scan_to_url(&self, _scanner: &ICScannerDevice, _url: Option<&NSURL>) {
            self.send(Event::Failed(
                "the scanner driver saved the scan to a file instead of handing it over in memory, \
                 which this app does not handle yet"
                    .into(),
            ));
        }

        #[unsafe(method(scannerDevice:didCompleteScanWithError:))]
        fn did_complete_scan(&self, _scanner: &ICScannerDevice, error: Option<&NSError>) {
            self.send(Event::ScanDone(error.map(|e| {
                if e.code() == ERR_SCAN_CANCELED {
                    "the scan was cancelled".to_string()
                } else {
                    format!("the scan failed: {}", error_text(e))
                }
            })));
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, tx: Sender<Event>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            tx,
            device: RefCell::new(None),
            ready: Cell::new(false),
            select_requested: Cell::new(false),
            reported: Cell::new(false),
        });
        // SAFETY: NSObject's plain `init`.
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, ev: Event) {
        // the receiver is gone when the worker gave up waiting: nothing to do then
        let _ = self.ivars().tx.send(ev);
    }

    fn device(&self) -> Option<Retained<ICScannerDevice>> {
        self.ivars().device.borrow().clone()
    }

    fn is_ours(&self, device: &ICDevice) -> bool {
        self.device().map_or(false, |d| std::ptr::eq::<ICDevice>(&**d, device))
    }

    /// The first scanner found is the one that is used.
    fn device_added(&self, device: &ICDevice) {
        if self.ivars().device.borrow().is_some() {
            return;
        }
        // SAFETY: property read.
        if unsafe { device.r#type() }.0 & ICDeviceType::Scanner.0 == 0 {
            return;
        }
        let Some(scanner) = device.downcast_ref::<ICScannerDevice>() else { return };
        let scanner = scanner.retain();
        *self.ivars().device.borrow_mut() = Some(scanner.clone());
        self.send(Event::Found(device_name(device)));
        // SAFETY: the delegate (self) is kept alive by `Session` for as long as
        // the device has it; `end_session` clears it first.
        unsafe {
            scanner.setDelegate(Some(ProtocolObject::from_ref(self)));
            scanner.requestOpenSession();
        }
    }

    fn device_gone(&self, device: &ICDevice) {
        if self.is_ours(device) {
            self.send(Event::Gone);
        }
    }

    /// Ask for the flatbed, once the device is ready.
    fn select_flatbed(&self) {
        let iv = self.ivars();
        if !iv.ready.get() || iv.select_requested.get() {
            return;
        }
        let Some(dev) = self.device() else { return };
        // SAFETY: property read and an asynchronous request.
        unsafe {
            let has_flatbed = dev
                .availableFunctionalUnitTypes()
                .iter()
                .any(|n| n.unsignedIntegerValue() == ICScannerFunctionalUnitType::Flatbed.0);
            if !has_flatbed {
                self.send(Event::Failed("this scanner has no flatbed".into()));
                return;
            }
            iv.select_requested.set(true);
            dev.requestSelectFunctionalUnit(ICScannerFunctionalUnitType::Flatbed);
        }
    }

    /// A functional unit was selected: once on its own when the device comes
    /// up (whichever unit the driver prefers), and again in answer to
    /// `select_flatbed`.
    fn unit_selected(&self, unit: Option<&ICScannerFunctionalUnit>, error: Option<&NSError>) {
        let iv = self.ivars();
        if !iv.select_requested.get() {
            self.select_flatbed(); // the driver's own first choice: wait for ours
            return;
        }
        if iv.reported.get() {
            return;
        }
        if let Some(e) = error {
            self.send(Event::Failed(format!("cannot select the flatbed: {}", error_text(e))));
            return;
        }
        let Some(unit) = unit else { return };
        // SAFETY: property reads and writes on the selected unit, on the main thread.
        unsafe {
            if unit.r#type().0 != ICScannerFunctionalUnitType::Flatbed.0 {
                return; // the driver's first choice arriving late
            }
            if !unit.supportedMeasurementUnits().containsIndex(ICScannerMeasurementUnit::Inches.0) {
                self.send(Event::Failed("the scanner driver does not measure in inches".into()));
                return;
            }
            unit.setMeasurementUnit(ICScannerMeasurementUnit::Inches);
            let size = unit.physicalSize();
            if !(size.width > 0.0 && size.height > 0.0) {
                self.send(Event::Failed("the scanner did not report the size of its glass".into()));
                return;
            }
            let mut notes = vec![
                format!("resolutions: {}", index_list(&unit.supportedResolutions())),
                format!("bit depths: {}", index_list(&unit.supportedBitDepths())),
                format!("native resolution: {} x {} dpi", unit.nativeXResolution(), unit.nativeYResolution()),
            ];
            switch_off_corrections(unit, &mut notes);
            iv.reported.set(true);
            let name = self.device().map(|d| device_name(&d)).unwrap_or_default();
            self.send(Event::Ready { name, glass_mm: (size.width * 25.4, size.height * 25.4), notes });
        }
    }
}

fn device_name(device: &ICDevice) -> String {
    // SAFETY: property read.
    unsafe { device.name() }.map(|n| n.to_string()).unwrap_or_else(|| "scanner".into())
}

fn error_text(e: &NSError) -> String {
    let hint = match e.code() {
        ERR_IN_USE_LOCAL | ERR_IN_USE_REMOTE | ERR_SESSION_ALREADY_OPEN => {
            " Another program is using the scanner: close Image Capture, Preview or Epson's software."
        }
        ERR_NOT_AUTHORIZED => " macOS did not allow this app to use the scanner.",
        _ => "",
    };
    format!("{} (error {}).{hint}", e.localizedDescription(), e.code())
}

/// "75, 150, 300" from an index set (at most 40 entries).
fn index_list(set: &NSIndexSet) -> String {
    let mut out = Vec::new();
    let mut i = set.firstIndex();
    while i != NSNotFound as usize && out.len() < 40 {
        out.push(i.to_string());
        i = set.indexGreaterThanIndex(i);
    }
    if i != NSNotFound as usize {
        out.push("\u{2026}".into());
    }
    out.join(", ")
}

/// Scans must differ only by where the photo lies, so every correction the
/// driver offers is switched off: each on/off feature goes off, and each
/// feature with a list of choices goes to "None" or "Off" where it has one.
/// `MULTIPASS_ICA_KEEP_FEATURES=1` leaves the driver's settings alone.
///
/// Which features Epson's driver has for a given scanner is only known on a
/// Mac with that scanner, so `notes` records each one and what was done.
///
/// # Safety
/// Main thread, with `unit` the device's selected unit.
unsafe fn switch_off_corrections(unit: &ICScannerFunctionalUnit, notes: &mut Vec<String>) {
    let keep = std::env::var_os("MULTIPASS_ICA_KEEP_FEATURES").is_some();
    let Some(features) = unit.vendorFeatures() else {
        notes.push("driver features: none".into());
        return;
    };
    if features.is_empty() {
        notes.push("driver features: none".into());
    }
    for f in features.iter() {
        let name = f
            .humanReadableName()
            .or_else(|| f.internalName())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "(unnamed)".into());
        if let Some(b) = f.downcast_ref::<ICScannerFeatureBoolean>() {
            let was = b.value();
            if was && !keep {
                b.setValue(false);
            }
            notes.push(format!(
                "driver feature \"{name}\": was {}{}",
                if was { "on" } else { "off" },
                if was && !keep { ", switched off" } else { "" }
            ));
        } else if let Some(e) = f.downcast_ref::<ICScannerFeatureEnumeration>() {
            let labels: Vec<String> = e.menuItemLabels().iter().map(|l| l.to_string()).collect();
            let values = e.values();
            let off = labels.iter().position(|l| {
                let l = l.trim().to_lowercase();
                l == "none" || l == "off"
            });
            let mut note = format!("driver feature \"{name}\": choices [{}]", labels.join(", "));
            match off {
                Some(i) if !keep && i < values.len() => {
                    let v = values.objectAtIndex(i);
                    let v: &AnyObject = &v;
                    e.setCurrentValue(v);
                    note.push_str(&format!(", set to \"{}\"", labels[i]));
                }
                _ => note.push_str(", left as it is"),
            }
            notes.push(note);
        } else {
            notes.push(format!("driver feature \"{name}\": left as it is"));
        }
    }
}

// -------------------------------------------------------- main thread: session

struct Session {
    id: u64,
    browser: Retained<ICDeviceBrowser>,
    delegate: Retained<Delegate>,
}

thread_local! {
    /// The open session.  Only ever touched on the main thread.
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// The session closed last.  Closing is asynchronous, so its objects are
    /// kept until the next one is closed rather than freed under the framework.
    static CLOSED: RefCell<Option<Session>> = const { RefCell::new(None) };
}

fn start_session(id: u64, tx: Sender<Event>) {
    let Some(mtm) = MainThreadMarker::new() else {
        let _ = tx.send(Event::Failed("internal error: the scanner session was not started on the main thread".into()));
        return;
    };
    end_session(None); // one at a time
    let delegate = Delegate::new(mtm, tx);
    // SAFETY: the delegate is kept alive by the `Session` stored below, and
    // `end_session` clears it from the browser before the session is dropped.
    let browser = unsafe {
        let browser = ICDeviceBrowser::new();
        browser.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // scanners connected to this Mac (USB), not shared or network ones
        browser.setBrowsedDeviceTypeMask(ICDeviceTypeMask(
            ICDeviceTypeMask::Scanner.0 | ICDeviceLocationTypeMask::Local.0,
        ));
        browser
    };
    SESSION.with(|s| *s.borrow_mut() = Some(Session { id, browser: browser.clone(), delegate }));
    // SAFETY: starts the search; devices arrive through the delegate.
    unsafe { browser.start() };
}

/// Close the session `id` (or whichever is open, for `None`).
fn end_session(id: Option<u64>) {
    let session = SESSION.with(|s| {
        let mut s = s.borrow_mut();
        match (&*s, id) {
            (Some(open), Some(id)) if open.id != id => None, // a newer session: not ours to close
            _ => s.take(),
        }
    });
    let Some(session) = session else { return };
    // SAFETY: main thread; the objects are alive (we hold them).
    unsafe {
        if let Some(dev) = session.delegate.ivars().device.borrow_mut().take() {
            if dev.selected_unit().map_or(false, |u| u.scanInProgress()) {
                dev.cancelScan();
            }
            dev.setDelegate(None);
            if dev.hasOpenSession() {
                dev.requestCloseSession();
            }
        }
        session.browser.setDelegate(None);
        session.browser.stop();
    }
    CLOSED.with(|c| *c.borrow_mut() = Some(session));
}

trait SelectedUnit {
    fn selected_unit(&self) -> Option<Retained<ICScannerFunctionalUnit>>;
}

impl SelectedUnit for ICScannerDevice {
    /// `selectedFunctionalUnit`, which is nil until the device is ready.  (The
    /// generated binding declares it non-null and would abort on nil.)
    fn selected_unit(&self) -> Option<Retained<ICScannerFunctionalUnit>> {
        // SAFETY: a property returning an object or nil.
        unsafe { msg_send![self, selectedFunctionalUnit] }
    }
}

struct Request {
    dpi: usize,
    gray: bool,
    /// inches, from the corner where a scan starts
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// bytes in one row of the image
    row_bytes: usize,
}

/// Set the scan up and start it.  The image arrives as `Event::Band`s.
fn start_scan(id: u64, req: &Request) -> Result<(), String> {
    let dev = SESSION
        .with(|s| s.borrow().as_ref().filter(|s| s.id == id).and_then(|s| s.delegate.device()))
        .ok_or("the scanner is not open any more")?;
    let unit = dev.selected_unit().ok_or("the scanner has no flatbed selected")?;
    // SAFETY: main thread; property reads and writes on live objects, then an
    // asynchronous request.
    unsafe {
        if unit.r#type().0 != ICScannerFunctionalUnitType::Flatbed.0 {
            return Err("the scanner's flatbed is not selected any more".into());
        }
        if unit.scanInProgress() || unit.overviewScanInProgress() {
            return Err("the scanner is busy with another scan".into());
        }
        if !unit.supportedResolutions().containsIndex(req.dpi) {
            return Err(format!("this scanner does not support {} dpi", req.dpi));
        }
        if !unit.supportedBitDepths().containsIndex(ICScannerBitDepth::Depth8Bits.0) {
            return Err("this scanner does not scan at 8 bits per sample".into());
        }
        let pixels = if req.gray { ICScannerPixelDataType::Gray } else { ICScannerPixelDataType::RGB };
        unit.setMeasurementUnit(ICScannerMeasurementUnit::Inches);
        unit.setPixelDataType(pixels);
        unit.setBitDepth(ICScannerBitDepth::Depth8Bits);
        unit.setResolution(req.dpi);
        if unit.supportedScaleFactors().containsIndex(100) {
            unit.setScaleFactor(100);
        }
        unit.setScanAreaOrientation(ICEXIFOrientationType::Orientation1);
        unit.setScanArea(NSRect::new(NSPoint::new(req.x, req.y), NSSize::new(req.w, req.h)));
        // The driver may refuse a setting silently: look at what it kept.
        if unit.resolution() != req.dpi {
            return Err(format!("the scanner chose {} dpi instead of {}", unit.resolution(), req.dpi));
        }
        if unit.pixelDataType().0 != pixels.0 {
            return Err(format!(
                "the scanner does not scan in {} at {} dpi",
                if req.gray { "grey" } else { "colour" },
                req.dpi
            ));
        }
        let kept = unit.scanArea();
        let slack = 2.0 / req.dpi as f64; // two pixels
        if (kept.origin.x - req.x).abs() > slack
            || (kept.origin.y - req.y).abs() > slack
            || (kept.size.width - req.w).abs() > slack
            || (kept.size.height - req.h).abs() > slack
        {
            return Err(format!(
                "the scanner changed the scan area from {:.3},{:.3} {:.3} x {:.3} in to {:.3},{:.3} {:.3} x {:.3} in",
                req.x, req.y, req.w, req.h, kept.origin.x, kept.origin.y, kept.size.width, kept.size.height
            ));
        }
        dev.setTransferMode(ICScannerTransferMode::MemoryBased);
        // at least two rows per band
        dev.setMaxMemoryBandSize(MIN_BAND_BYTES.max(u32::try_from(req.row_bytes * 2).unwrap_or(u32::MAX)));
        dev.requestScan();
    }
    Ok(())
}

fn cancel_scan(id: u64) {
    let dev = SESSION.with(|s| s.borrow().as_ref().filter(|s| s.id == id).and_then(|s| s.delegate.device()));
    if let Some(dev) = dev {
        // SAFETY: main thread; asynchronous request.
        unsafe { dev.cancelScan() };
    }
}

// ---------------------------------------------------------- the worker's side

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn on_main(work: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(work);
}

pub struct IcaScanner {
    id: u64,
    name: String,
    glass_mm: (f64, f64),
    notes: Vec<String>,
    /// events from the main thread; `tx` lets the worker's own requests report
    /// a failure the same way
    rx: Receiver<Event>,
    tx: Sender<Event>,
}

pub fn open_first() -> Result<Box<dyn Scanner>, String> {
    if MainThreadMarker::new().is_some() {
        // Waiting here would stop the very loop that delivers the answers.
        return Err("internal error: the scanner must be opened from a worker thread".into());
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = channel();
    let tx_main = tx.clone();
    on_main(move || start_session(id, tx_main));

    let fail = |msg: String| -> Result<Box<dyn Scanner>, String> {
        on_main(move || end_session(Some(id)));
        Err(msg)
    };
    let mut deadline = Instant::now() + OPEN_TIMEOUT;
    let mut found: Option<String> = None;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Event::Ready { name, glass_mm, notes }) => {
                return Ok(Box::new(IcaScanner { id, name, glass_mm, notes, rx, tx }));
            }
            Ok(Event::Found(name)) => {
                found = Some(name);
                deadline = Instant::now() + OPEN_TIMEOUT;
            }
            Ok(Event::Enumerated) => {
                if found.is_none() {
                    deadline = deadline.min(Instant::now() + NO_DEVICE_GRACE);
                }
            }
            Ok(Event::Failed(e)) => return fail(e),
            Ok(Event::Gone) => return fail("The scanner was disconnected.".into()),
            Ok(Event::Band(..)) | Ok(Event::ScanDone(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                return fail(match found {
                    Some(name) => format!("{name} did not become ready. Is another program using it?"),
                    None => "No scanner was found. Is it switched on and connected? It should also \
                             appear in Apple's Image Capture app."
                        .into(),
                });
            }
            Err(RecvTimeoutError::Disconnected) => return fail("internal error: the scanner session ended".into()),
        }
    }
}

impl Scanner for IcaScanner {
    fn name(&self) -> &str {
        &self.name
    }

    fn glass_mm(&self) -> (f64, f64) {
        self.glass_mm
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }

    fn scan(&mut self, dpi: i32, gray: bool, area: AreaMm) -> Result<RawImage, String> {
        // events left over from before this scan
        loop {
            match self.rx.try_recv() {
                Ok(Event::Gone) => return Err("The scanner was disconnected.".into()),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // The same pixel rectangle as on Linux, handed over in inches.
        let (x, y, w, h) = crate::area_px(area, dpi, self.glass_mm);
        let inch = |px: i32| px as f64 / dpi as f64;
        // MULTIPASS_ICA_Y_FROM_FAR_END=1: for a driver that measures y from the
        // far end of the glass.  The V19 II's driver measures from the end where
        // a scan starts and does not need it.
        let y_in = if std::env::var_os("MULTIPASS_ICA_Y_FROM_FAR_END").is_some() {
            (self.glass_mm.1 / 25.4 - inch(y + h)).max(0.0)
        } else {
            inch(y)
        };
        let req = Request {
            dpi: dpi as usize,
            gray,
            x: inch(x),
            y: y_in,
            w: inch(w),
            h: inch(h),
            row_bytes: w as usize * if gray { 1 } else { 3 },
        };
        let (id, tx) = (self.id, self.tx.clone());
        on_main(move || {
            if let Err(e) = start_scan(id, &req) {
                let _ = tx.send(Event::ScanDone(Some(e)));
            }
        });

        let start = Instant::now();
        let mut image = Assembler::default();
        let cancel = |msg: String| -> Result<RawImage, String> {
            on_main(move || cancel_scan(id));
            Err(msg)
        };
        loop {
            if start.elapsed() > crate::scan_time_limit(dpi) {
                return cancel("the scan timed out".into());
            }
            match self.rx.recv_timeout(SCAN_SILENCE) {
                Ok(Event::Band(info, bytes)) => {
                    if let Err(e) = image.add(&info, &bytes) {
                        return cancel(e);
                    }
                }
                Ok(Event::ScanDone(None)) => {
                    if image.start_rows_disagree {
                        eprintln!("note: the driver's band positions did not match the order the bands arrived in");
                    }
                    return image.finish(gray);
                }
                Ok(Event::ScanDone(Some(e))) => return Err(e),
                Ok(Event::Failed(e)) => return cancel(e),
                Ok(Event::Gone) => return Err("The scanner was disconnected during the scan.".into()),
                Ok(Event::Ready { .. }) | Ok(Event::Found(_)) | Ok(Event::Enumerated) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return cancel(format!(
                        "the scanner sent nothing for {} s ({} rows had arrived)",
                        SCAN_SILENCE.as_secs(),
                        image.rows_done()
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => return Err("internal error: the scanner session ended".into()),
            }
        }
    }
}

impl Drop for IcaScanner {
    fn drop(&mut self) {
        let id = self.id;
        let (done_tx, done_rx) = channel::<()>();
        on_main(move || {
            end_session(Some(id));
            let _ = done_tx.send(());
        });
        // Give the close request time to go out before, say, the program exits.
        if MainThreadMarker::new().is_none() {
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

/// Run `work` on a second thread while the main thread runs the event loop
/// that ImageCaptureCore's callbacks need.  Must be called on the main thread
/// of a program that has no event loop of its own.
pub fn run_with_main_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let worker = std::thread::Builder::new()
        .name("scanner".into())
        .spawn(work)
        .expect("cannot start the scanner thread");
    while !worker.is_finished() {
        // SAFETY: reading a constant the system provides.
        let mode = unsafe { kCFRunLoopDefaultMode };
        // Serves the main dispatch queue and the framework's sources for a
        // quarter of a second, then looks at the worker again.
        if CFRunLoop::run_in_mode(mode, 0.25, false) == CFRunLoopRunResult::Finished {
            std::thread::sleep(Duration::from_millis(20)); // nothing to wait on yet
        }
    }
    match worker.join() {
        Ok(out) => out,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
