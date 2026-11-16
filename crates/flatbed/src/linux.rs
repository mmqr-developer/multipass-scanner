//! Linux backend: Epson Scan 2.
//!
//! Loads Epson's `libepsonscan2.so` at run time and drives it through its
//! public C interface (`Controller/EpsonScan2.h` in the epsonscan2 source).
//! Every automatic or lossy correction the driver offers is switched off, so
//! each scan differs only by where the original lies on the glass.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::time::{Duration, Instant};

use libloading::Library;

use crate::{AreaMm, RawImage, Scanner};

// ------------------------------------------------------------------ C types

#[repr(C)]
#[derive(Clone, Copy)]
struct DeviceInfo {
    version: i32,
    model_id: [c_char; 20],
    product_id: i32,
    ip_address: [c_char; 64],
    display_name: [c_char; 50],
}

impl DeviceInfo {
    fn display_name(&self) -> String {
        cstr_field(&self.display_name)
    }
}

fn cstr_field(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[repr(C)]
struct Capability {
    version: i32,
    support_level: i32,
    capability_type: i32,
    min_value: i32,
    max_value: i32,
    all_min_value: i32,
    all_max_value: i32,
    list: [i32; 20],
    count_of_list: i32,
    all_list: [i32; 20],
    count_of_all_list: i32,
}

// Sizes checked against the C header (g++: SDIDeviceInfo 144, SDICapability 196).
const _: () = assert!(std::mem::size_of::<DeviceInfo>() == 144);
const _: () = assert!(std::mem::size_of::<Capability>() == 196);

type FinderCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type FinderStart = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
type FinderStop = unsafe extern "C" fn(*mut c_void) -> i32;
type FinderGetDevices = unsafe extern "C" fn(*mut c_void, *mut *mut DeviceInfo, *mut i32) -> i32;
type FinderDispose = unsafe extern "C" fn(*mut c_void) -> i32;
type DriverCreate = unsafe extern "C" fn(*mut *mut c_void, *const DeviceInfo, *mut c_void, *mut c_void) -> i32;
type DriverSimple = unsafe extern "C" fn(*mut c_void) -> i32;
type SetValue = unsafe extern "C" fn(*mut c_void, *const c_char, i32, *mut c_void, i32) -> i32;
type GetValue = unsafe extern "C" fn(*mut c_void, *const c_char, i32, *mut c_void, i32) -> i32;
type GetCapability = unsafe extern "C" fn(*mut c_void, *const c_char, *mut Capability) -> i32;
type DoScanJob = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type CheckNextEvent = unsafe extern "C" fn(*mut c_void, bool) -> bool;
type GetNextEvent = unsafe extern "C" fn(*mut c_void, *mut i32, *mut c_void, *mut i32) -> i32;
type ImageCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type ImageGetInt = unsafe extern "C" fn(*mut c_void) -> i32;
type ImageGetPath = unsafe extern "C" fn(*mut c_void, *mut c_char, i32) -> i32;

// Keys and values from EpsonScan2.h (dumped with g++ from the header).
const K_FUNCTIONAL_UNIT: &str = "FunctionalUnit";
const K_COLOR_TYPE: &str = "ColorType";
const K_RESOLUTION: &str = "Resolution";
const K_IMAGE_FORMAT: &str = "ImageFormat";
const K_TRANSFER_COMPRESSION: &str = "TransferCompression";
const K_AUTO_SIZE: &str = "AutoSize";
const K_PAPER_DESKEW: &str = "PaperDeskew";
const K_ORIENTATION: &str = "Orientation";
const K_BACKGROUND_REMOVAL: &str = "BackgroundRemoval";
const K_BLANK_PAGE_SKIP: &str = "BlankPageSkip";
const K_DROPOUT_COLOR: &str = "DropoutColor";
const K_TEXT_ENHANCE: &str = "textEnhance";
const K_REMOVE_PUNCHHOLE: &str = "RemovePunchHole";
const K_DEFICIENCY_CORRECTION: &str = "DeficienryCorrection"; // (sic)
const K_AUTO_CROPPING_INSCRIBED: &str = "AutoCroppingInscribed";
const K_EDGE_FILL: &str = "EdgeFill";
const K_BRIGHTNESS: &str = "Brightness";
const K_CONTRAST: &str = "Contrast";
const K_MAX_WIDTH: &str = "ScanAreaMaxWidth";
const K_MAX_HEIGHT: &str = "ScanAreaMaxHeight";
const K_AREA_WIDTH: &str = "ScanAreaWidth";
const K_AREA_HEIGHT: &str = "ScanAreaHeight";
const K_AREA_X: &str = "ScanAreaOffsetX";
const K_AREA_Y: &str = "ScanAreaOffsetY";
const K_UNSCANNABLE: &str = "UnscannebleScanParameter"; // (sic)

const FUNCTIONAL_UNIT_FLATBED: i32 = 0;
const COLOR_RGB24: i32 = 0;
const COLOR_MONO8: i32 = 1;
const IMAGE_FORMAT_RAW: i32 = 0;
const OP_NEW: i32 = 0;
const OP_CANCEL: i32 = 1;
const EVENT_IMAGE: i32 = 0;
const EVENT_COMPLETE: i32 = 1;
const EVENT_CANCEL: i32 = 2;
const SUPPORT_NONE: i32 = 0;
const CAP_RANGE: i32 = 1;
const VALUE_INT: i32 = 0;

fn error_text(code: i32) -> &'static str {
    match code {
        2 => "invalid parameter",
        3 => "a required driver file was not found",
        10 => "device not opened",
        12 => "the scanner is in use by another program",
        13 => "the scanner is busy",
        17 => "the cover is open",
        20 => "the carriage is locked (transportation lock)",
        21 => "lamp error",
        22 => "lamp warm-up timed out",
        24 => "no permission to scan",
        26 => "failed to send data to the scanner",
        27 => "failed to receive data from the scanner",
        29 => "invalid response from the scanner",
        30 => "cannot open the scanner (check USB permissions)",
        32 => "connection timed out",
        33 => "the scanner was disconnected",
        34 => "scanner not found",
        35 => "fatal scanner error",
        100 => "disk full",
        102 => "cannot write the image file",
        151 => "out of memory",
        209 => "scan area too large",
        210 => "scan area too small",
        253 => "a required driver module was not found",
        _ => "scanner error",
    }
}

fn err(code: i32, what: &str) -> String {
    format!("{what}: {} (error {code})", error_text(code))
}

const LIB_PATHS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/epsonscan2/libepsonscan2.so",
    "/usr/lib/aarch64-linux-gnu/epsonscan2/libepsonscan2.so",
    "/usr/lib64/epsonscan2/libepsonscan2.so",
    "/usr/lib/epsonscan2/libepsonscan2.so",
    "/usr/local/lib/epsonscan2/libepsonscan2.so",
];

struct Api {
    _lib: Library,
    finder_create: FinderCreate,
    finder_start: FinderStart,
    finder_stop: FinderStop,
    finder_get_devices: FinderGetDevices,
    finder_dispose: FinderDispose,
    driver_create: DriverCreate,
    driver_open: DriverSimple,
    driver_close: DriverSimple,
    driver_dispose: DriverSimple,
    set_value: SetValue,
    get_value: GetValue,
    get_capability: GetCapability,
    do_scan_job: DoScanJob,
    check_next_event: CheckNextEvent,
    get_next_event: GetNextEvent,
    image_create: ImageCreate,
    image_width: ImageGetInt,
    image_height: ImageGetInt,
    image_spp: ImageGetInt,
    image_bps: ImageGetInt,
    image_path: ImageGetPath,
    image_dispose: ImageGetInt,
}

impl Api {
    fn load() -> Result<Api, String> {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("ES2_LIB") {
            candidates.push(p);
        }
        candidates.extend(LIB_PATHS.iter().map(|s| s.to_string()));
        let mut tried = String::new();
        for path in &candidates {
            if !Path::new(path).exists() {
                continue;
            }
            match unsafe { Library::new(path) } {
                Ok(lib) => return Api::bind(lib),
                Err(e) => tried.push_str(&format!("\n  {path}: {e}")),
            }
        }
        Err(format!(
            "The Epson Scan 2 driver (libepsonscan2.so) was not found. Install Epson's epsonscan2 \
             package, or set ES2_LIB to the library's path.{tried}"
        ))
    }

    fn bind(lib: Library) -> Result<Api, String> {
        unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
            lib.get::<T>(name.as_bytes())
                .map(|s| *s)
                .map_err(|_| format!("libepsonscan2.so has no symbol {name}"))
        }
        unsafe {
            Ok(Api {
                finder_create: sym(&lib, "SDIDeviceFinder_Create")?,
                finder_start: sym(&lib, "SDIDeviceFinder_StartDiscovery")?,
                finder_stop: sym(&lib, "SDIDeviceFinder_StopDiscovery")?,
                finder_get_devices: sym(&lib, "SDIDeviceFinder_GetDevices")?,
                finder_dispose: sym(&lib, "SDIDeviceFinder_Dispose")?,
                driver_create: sym(&lib, "SDIScannerDriver_Create")?,
                driver_open: sym(&lib, "SDIScannerDriver_Open")?,
                driver_close: sym(&lib, "SDIScannerDriver_Close")?,
                driver_dispose: sym(&lib, "SDIScannerDriver_Dispose")?,
                set_value: sym(&lib, "SDIScannerDriver_SetValue")?,
                get_value: sym(&lib, "SDIScannerDriver_GetValue")?,
                get_capability: sym(&lib, "SDIScannerDriver_GetCapability")?,
                // The exported name really is "SDIScannerDrive_DoScanJob" (sic).
                do_scan_job: sym(&lib, "SDIScannerDrive_DoScanJob")?,
                check_next_event: sym(&lib, "SDIScannerDriver_CheckNextTransferEvent")?,
                get_next_event: sym(&lib, "SDIScannerDriver_GetNextTransferEvent")?,
                image_create: sym(&lib, "SDIImage_Create")?,
                image_width: sym(&lib, "SDIImage_GetWidth")?,
                image_height: sym(&lib, "SDIImage_GetHeight")?,
                image_spp: sym(&lib, "SDIImage_GetSamplesPerPixel")?,
                image_bps: sym(&lib, "SDIImage_GetBitsPerSample")?,
                image_path: sym(&lib, "SDIImage_GetPath")?,
                image_dispose: sym(&lib, "SDIImage_Dispose")?,
                _lib: lib,
            })
        }
    }
}

pub fn open_first() -> Result<Box<dyn Scanner>, String> {
    Ok(Box::new(EpsonScanner::open_first()?))
}

/// The driver is polled, not event-driven: no main loop is needed.
pub fn run_with_main_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    work()
}

pub struct EpsonScanner {
    api: Api,
    driver: *mut c_void,
    name: String,
    /// glass size in mm
    max_w_mm: f64,
    max_h_mm: f64,
    /// how long the scan being taken may last
    time_limit: Duration,
}

// The driver handle is only ever used from the one worker thread that owns it.
unsafe impl Send for EpsonScanner {}

impl Scanner for EpsonScanner {
    fn name(&self) -> &str {
        &self.name
    }

    fn glass_mm(&self) -> (f64, f64) {
        (self.max_w_mm, self.max_h_mm)
    }

    fn scan(&mut self, dpi: i32, gray: bool, area: AreaMm) -> Result<RawImage, String> {
        self.scan_area(dpi, gray, area)
    }
}

impl EpsonScanner {
    /// Find the first Epson scanner on USB and open it.
    fn open_first() -> Result<EpsonScanner, String> {
        let api = Api::load()?;
        let devices = unsafe {
            let mut finder: *mut c_void = std::ptr::null_mut();
            let e = (api.finder_create)(&mut finder);
            if e != 0 || finder.is_null() {
                return Err(err(e, "cannot search for scanners"));
            }
            (api.finder_start)(finder, std::ptr::null_mut(), std::ptr::null_mut()); // USB: synchronous
            (api.finder_stop)(finder);
            let mut devs: *mut DeviceInfo = std::ptr::null_mut();
            let mut count: i32 = 0;
            (api.finder_get_devices)(finder, &mut devs, &mut count);
            let list: Vec<DeviceInfo> = if devs.is_null() || count <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(devs, count as usize).to_vec() // copy before dispose
            };
            (api.finder_dispose)(finder);
            list
        };
        let info = *devices
            .first()
            .ok_or("No Epson scanner was found on USB. Is it switched on and connected?")?;
        let mut driver: *mut c_void = std::ptr::null_mut();
        unsafe {
            let e = (api.driver_create)(&mut driver, &info, std::ptr::null_mut(), std::ptr::null_mut());
            if e != 0 || driver.is_null() {
                return Err(err(e, "cannot create the scanner driver"));
            }
            let e = (api.driver_open)(driver);
            if e != 0 {
                (api.driver_dispose)(driver);
                return Err(err(e, &format!("cannot open {}", info.display_name())));
            }
        }
        let mut s = EpsonScanner {
            api,
            driver,
            name: info.display_name(),
            max_w_mm: 0.0,
            max_h_mm: 0.0,
            time_limit: crate::scan_time_limit(0),
        };
        s.set_int(K_FUNCTIONAL_UNIT, FUNCTIONAL_UNIT_FLATBED)
            .map_err(|e| format!("this scanner has no flatbed ({e})"))?;
        let w = s.get_int(K_MAX_WIDTH).unwrap_or(0);
        let h = s.get_int(K_MAX_HEIGHT).unwrap_or(0);
        if w <= 0 || h <= 0 {
            return Err("the scanner did not report the size of its glass".into());
        }
        // reported in 1/100 inch
        s.max_w_mm = w as f64 * 0.254;
        s.max_h_mm = h as f64 * 0.254;
        Ok(s)
    }

    fn get_int(&self, key: &str) -> Option<i32> {
        let k = CString::new(key).unwrap();
        let mut v: i32 = 0;
        let e = unsafe {
            (self.api.get_value)(self.driver, k.as_ptr(), VALUE_INT, &mut v as *mut i32 as *mut c_void, 4)
        };
        (e == 0).then_some(v)
    }

    fn set_int(&mut self, key: &str, value: i32) -> Result<(), String> {
        let k = CString::new(key).unwrap();
        let mut v = value;
        let e = unsafe {
            (self.api.set_value)(self.driver, k.as_ptr(), VALUE_INT, &mut v as *mut i32 as *mut c_void, 4)
        };
        if e == 0 {
            Ok(())
        } else {
            Err(err(e, &format!("cannot set {key}")))
        }
    }

    fn capability(&self, key: &str) -> Option<Capability> {
        let k = CString::new(key).unwrap();
        let mut cap: Capability = unsafe { std::mem::zeroed() };
        let e = unsafe { (self.api.get_capability)(self.driver, k.as_ptr(), &mut cap) };
        (e == 0 && cap.support_level != SUPPORT_NONE).then_some(cap)
    }

    fn set_if_supported(&mut self, key: &str, value: i32) {
        if self.capability(key).is_some() {
            let _ = self.set_int(key, value);
        }
    }

    fn allows(cap: &Capability, v: i32) -> bool {
        if cap.capability_type == CAP_RANGE {
            v >= cap.min_value && v <= cap.max_value
        } else {
            cap.list[..(cap.count_of_list.clamp(0, 20) as usize)].contains(&v)
        }
    }

    /// Scan `area` (mm) at `dpi`, in grey or colour, 8 bits per sample.
    fn scan_area(&mut self, dpi: i32, gray: bool, area: AreaMm) -> Result<RawImage, String> {
        let color = if gray { COLOR_MONO8 } else { COLOR_RGB24 };
        self.set_int(K_COLOR_TYPE, color)?;
        if let Some(cap) = self.capability(K_RESOLUTION) {
            if !Self::allows(&cap, dpi) {
                return Err(format!("this scanner does not support {dpi} dpi"));
            }
        }
        self.set_int(K_RESOLUTION, dpi)?;
        self.time_limit = crate::scan_time_limit(dpi);
        self.set_int(K_IMAGE_FORMAT, IMAGE_FORMAT_RAW)?;
        for (k, v) in [
            (K_TRANSFER_COMPRESSION, 0),
            (K_AUTO_SIZE, 0),
            (K_PAPER_DESKEW, 0),
            (K_ORIENTATION, 0),
            (K_BACKGROUND_REMOVAL, 0),
            (K_BLANK_PAGE_SKIP, 0),
            (K_DROPOUT_COLOR, 0),
            (K_TEXT_ENHANCE, 0),
            (K_REMOVE_PUNCHHOLE, 0),
            (K_DEFICIENCY_CORRECTION, 0),
            (K_AUTO_CROPPING_INSCRIBED, 0),
            (K_EDGE_FILL, 0),
            (K_BRIGHTNESS, 0),
            (K_CONTRAST, 0),
        ] {
            self.set_if_supported(k, v);
        }
        // The driver takes the area in pixels at the scan resolution.
        let (x, y, w, h) = crate::area_px(area, dpi, (self.max_w_mm, self.max_h_mm));
        // width and height first, then the offsets (as Epson's own front end does)
        self.set_int(K_AREA_WIDTH, w)?;
        self.set_int(K_AREA_HEIGHT, h)?;
        self.set_int(K_AREA_X, x)?;
        self.set_int(K_AREA_Y, y)?;
        if self.get_int(K_UNSCANNABLE).unwrap_or(0) != 0 {
            return Err("the scanner rejected this combination of settings".into());
        }
        self.run_job()
    }

    fn run_job(&mut self) -> Result<RawImage, String> {
        let api = &self.api;
        let e = unsafe { (api.do_scan_job)(self.driver, OP_NEW) };
        if e != 0 {
            return Err(err(e, "the scan did not start"));
        }
        let start = Instant::now();
        let mut image: Option<RawImage> = None;
        let mut image_at = start;
        loop {
            if start.elapsed() > self.time_limit {
                unsafe { (api.do_scan_job)(self.driver, OP_CANCEL) };
                return Err("the scan timed out".into());
            }
            if image.is_some() && image_at.elapsed() > Duration::from_secs(20) {
                break; // the completion event never came
            }
            if !unsafe { (api.check_next_event)(self.driver, false) } {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut img: *mut c_void = std::ptr::null_mut();
            let mut kind: i32 = EVENT_IMAGE;
            let mut out_err: i32 = 0;
            unsafe {
                (api.image_create)(&mut img);
                (api.get_next_event)(self.driver, &mut kind, img, &mut out_err);
            }
            if out_err != 0 {
                unsafe { (api.image_dispose)(img) };
                return Err(err(out_err, "the scanner reported a problem"));
            }
            if kind == EVENT_IMAGE {
                if image.is_some() {
                    unsafe { (api.image_dispose)(img) };
                    continue;
                }
                let mut buf = vec![0 as c_char; 4096];
                let (w, h, spp, bps) = unsafe {
                    (api.image_path)(img, buf.as_mut_ptr(), buf.len() as i32);
                    let r = ((api.image_width)(img), (api.image_height)(img), (api.image_spp)(img), (api.image_bps)(img));
                    (api.image_dispose)(img);
                    r
                };
                let path = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
                let result = read_raw(&path, w, h, spp, bps);
                let _ = std::fs::remove_file(&path);
                image = Some(result?);
                image_at = Instant::now();
                continue;
            }
            unsafe { (api.image_dispose)(img) };
            if kind == EVENT_COMPLETE {
                break;
            }
            if kind == EVENT_CANCEL {
                return Err("the scan was cancelled".into());
            }
        }
        image.ok_or_else(|| "the scan finished without an image".into())
    }
}

impl Drop for EpsonScanner {
    fn drop(&mut self) {
        unsafe {
            (self.api.driver_close)(self.driver);
            (self.api.driver_dispose)(self.driver);
        }
    }
}

fn read_raw(path: &str, w: i32, h: i32, spp: i32, bps: i32) -> Result<RawImage, String> {
    if w <= 0 || h <= 0 {
        return Err("the driver returned an empty image".into());
    }
    if !(spp == 1 || spp == 3) || bps != 8 {
        return Err(format!("unexpected image from the driver: {spp} samples x {bps} bits"));
    }
    let (w, h, spp) = (w as usize, h as usize, spp as usize);
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the driver's image {path}: {e}"))?;
    let row = w * spp;
    if bytes.len() < row * h {
        return Err(format!("the driver's image is short ({} bytes, expected {})", bytes.len(), row * h));
    }
    // rows are packed; tolerate padding
    let stride = if bytes.len() % h == 0 { bytes.len() / h } else { row };
    let mut data = Vec::with_capacity(row * h);
    for y in 0..h {
        data.extend_from_slice(&bytes[y * stride..y * stride + row]);
    }
    Ok(RawImage { width: w, height: h, channels: spp, data })
}
