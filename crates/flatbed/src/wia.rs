//! Windows backend: Windows Image Acquisition (WIA) 2.0, the scanning
//! interface built into Windows.  It needs the scanner's WIA driver, which
//! Epson's driver package is expected to install beside Epson Scan 2.
//!
//! NOT YET RUN WITH A SCANNER.  It was written on Linux against Microsoft's
//! `windows` bindings; it compiles on Windows, which proves names and types,
//! not behaviour.  `cargo run -p flatbed --example scan-test` is the first
//! run: it prints what the driver offers, takes five small scans and checks
//! sizes, the origin of the scan area, repeatability and shear by itself.
//!
//! How WIA works, as far as this file needs it:
//!
//! * The device manager lists devices; opening one gives a tree of items.  The
//!   root is the device, its child of category "flatbed" is the glass.
//! * Everything is set through numbered properties on the flatbed item:
//!   resolution, colour or grey, and the scan area in pixels at that resolution.
//! * `IWiaTransfer::Download` runs the scan and blocks until it is done.  It
//!   asks a callback object of ours for a stream and writes a BMP file into it.
//!
//! Threads: WIA is COM, and the driver lives in a Windows service, not in this
//! process.  The scanner must be used from threads in COM's multithreaded
//! apartment.  A GUI's main thread is not one (the window system makes it a
//! single-threaded apartment), so, as on every system, scanner calls stay off
//! the main thread.  Callbacks arrive on threads COM owns while `Download`
//! blocks; they share state with the scanning thread through a mutex.

use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use windows::core::{implement, Error, Interface, Ref, BSTR, GUID, HRESULT};
use windows::Win32::Devices::ImageAcquisition::*;
use windows::Win32::Foundation::{
    E_NOTIMPL, E_POINTER, RPC_E_DISCONNECTED, STG_E_INVALIDFUNCTION, STG_E_MEDIUMFULL, S_FALSE, S_OK,
};
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromCLSID, PROPSPEC, PROPSPEC_0, PROPVARIANT, PRSPEC_PROPID, STATPROPSTG,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoDecrementMTAUsage, CoGetApartmentType, CoIncrementMTAUsage, CoTaskMemFree,
    ISequentialStream_Impl, IStream, IStream_Impl, APTTYPE, APTTYPEQUALIFIER, APTTYPE_MAINSTA, APTTYPE_STA,
    CLSCTX_LOCAL_SERVER, CO_MTA_USAGE_COOKIE, LOCKTYPE, STATFLAG, STATSTG, STGC, STGTY_STREAM, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_FILE,
};
use windows::Win32::System::Variant::{VT_I4, VT_INT, VT_UI4, VT_UINT, VT_VECTOR};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::{area_px, bmp, AreaMm, RawImage, Scanner};

/// `StiDeviceTypeScanner`: the device type (high word of `WIA_DIP_DEV_TYPE`) of a scanner.
const DEVICE_TYPE_SCANNER: i32 = 1;

/// No scan the app makes is larger than this (the whole glass at 4800 dpi in
/// colour is 6.9 GB; at 1200 dpi, 0.43 GB); a driver that sends more has gone
/// wrong.
const STREAM_LIMIT: usize = 8 << 30;

/// Nothing to set up: scanner calls only have to stay off a GUI's main thread.
pub fn run_with_main_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    work()
}

// ---------------------------------------------------------------- errors

fn hex(code: HRESULT) -> String {
    format!("0x{:08X}", code.0 as u32)
}

/// A WIA or COM error in plain words.
fn explain(e: &Error) -> String {
    let code = e.code();
    let known = [
        (WIA_ERROR_OFFLINE, "The scanner is switched off or unplugged."),
        (WIA_ERROR_BUSY, "The scanner is busy. Close other scanning programs (Epson Scan 2, Windows Scan) and try again."),
        (WIA_ERROR_WARMING_UP, "The scanner is still warming up."),
        (WIA_ERROR_DEVICE_LOCKED, "Another program is using the scanner."),
        (WIA_ERROR_DEVICE_COMMUNICATION, "Lost the connection to the scanner. Check the USB cable."),
        (WIA_ERROR_COVER_OPEN, "The scanner reports an open cover."),
        (WIA_ERROR_LAMP_OFF, "The scanner's lamp is off."),
        (WIA_ERROR_USER_INTERVENTION, "The scanner needs attention (look at the scanner itself)."),
        (WIA_ERROR_EXCEPTION_IN_DRIVER, "The scanner driver crashed."),
        (WIA_ERROR_INVALID_DRIVER_RESPONSE, "The scanner driver gave an invalid answer."),
        (WIA_ERROR_GENERAL_ERROR, "The scanner driver reports an error without saying which."),
        (WIA_S_NO_DEVICE_AVAILABLE, "Windows knows no scanner."),
        (RPC_E_DISCONNECTED, "Lost the connection to Windows' scanning service."),
    ];
    if let Some((_, text)) = known.iter().find(|(c, _)| *c == code) {
        return format!("{text} ({})", hex(code));
    }
    let message = e.message();
    if message.is_empty() {
        format!("Windows error {}", hex(code))
    } else {
        format!("{} ({})", message.trim(), hex(code))
    }
}

// ---------------------------------------------------------------- COM threads

/// Keeps COM's multithreaded apartment alive for as long as a scanner is
/// open, whichever thread uses it.  A thread that never set up COM itself (the
/// app's worker thread, or the main thread of a command-line run) is then in
/// that apartment by default.
struct Mta(CO_MTA_USAGE_COOKIE);

impl Mta {
    fn enter() -> Result<Mta, String> {
        // SAFETY: plain call; the cookie is handed back exactly once, in Drop.
        let cookie = unsafe { CoIncrementMTAUsage() }.map_err(|e| format!("Cannot start COM: {}", explain(&e)))?;
        let mta = Mta(cookie);
        check_thread()?;
        Ok(mta)
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        // SAFETY: the cookie came from CoIncrementMTAUsage and is used once.
        unsafe {
            let _ = CoDecrementMTAUsage(self.0);
        }
    }
}

/// Refuse a thread that the window system has claimed: interface pointers made
/// there belong to that thread alone and cannot follow the scanner elsewhere.
fn check_thread() -> Result<(), String> {
    let mut kind = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    // SAFETY: both pointers are to live locals.
    let known = unsafe { CoGetApartmentType(&mut kind, &mut qualifier) }.is_ok();
    if known && (kind == APTTYPE_STA || kind == APTTYPE_MAINSTA) {
        return Err("Internal error: the scanner was used from the window thread (it must be used from a worker thread).".into());
    }
    Ok(())
}

// ---------------------------------------------------------------- properties

fn spec(id: u32) -> PROPSPEC {
    PROPSPEC { ulKind: PRSPEC_PROPID, Anonymous: PROPSPEC_0 { propid: id } }
}

/// A property's value, or `None` if the item does not have it.
fn read(props: &IWiaPropertyStorage, id: u32) -> Option<PROPVARIANT> {
    let mut value = PROPVARIANT::default();
    // SAFETY: one spec, one value, both live for the call.
    unsafe { props.ReadMultiple(1, &spec(id), &mut value) }.ok()?;
    (!value.is_empty()).then_some(value)
}

fn read_i32(props: &IWiaPropertyStorage, id: u32) -> Option<i32> {
    i32::try_from(&read(props, id)?).ok()
}

fn read_string(props: &IWiaPropertyStorage, id: u32) -> Option<String> {
    let text = BSTR::try_from(&read(props, id)?).ok()?.to_string();
    (!text.is_empty()).then_some(text)
}

fn write(props: &IWiaPropertyStorage, id: u32, value: &PROPVARIANT) -> windows::core::Result<()> {
    // SAFETY: one spec, one value, both live for the call.
    unsafe { props.WriteMultiple(1, &spec(id), value, WIA_IPA_FIRST) }
}

fn write_i32(props: &IWiaPropertyStorage, id: u32, value: i32) -> windows::core::Result<()> {
    write(props, id, &PROPVARIANT::from(value))
}

fn write_guid(props: &IWiaPropertyStorage, id: u32, value: &GUID) -> windows::core::Result<()> {
    // SAFETY: the pointer is to a live GUID; the function copies it.
    let value = unsafe { InitPropVariantFromCLSID(value) }?;
    write(props, id, &value)
}

/// Which values a property accepts.
#[derive(Debug, Clone, PartialEq)]
enum Valid {
    /// the driver does not say
    Unknown,
    Range { min: i32, nominal: i32, max: i32, step: i32 },
    List { nominal: i32, values: Vec<i32> },
}

impl Valid {
    fn allows(&self, v: i32) -> bool {
        match self {
            Valid::Unknown => true,
            Valid::Range { min, max, step, .. } => v >= *min && v <= *max && (*step <= 1 || (v - min) % step == 0),
            Valid::List { values, .. } => values.contains(&v),
        }
    }

    fn nominal(&self) -> Option<i32> {
        match self {
            Valid::Unknown => None,
            Valid::Range { nominal, .. } | Valid::List { nominal, .. } => Some(*nominal),
        }
    }

    fn describe(&self) -> String {
        match self {
            Valid::Unknown => "any".into(),
            Valid::Range { min, max, step, .. } if *step <= 1 => format!("{min} to {max}"),
            Valid::Range { min, max, step, .. } => format!("{min} to {max} in steps of {step}"),
            Valid::List { values, .. } => values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "),
        }
    }
}

/// A property's access flags (`WIA_PROP_*`) and valid values.
fn attributes(props: &IWiaPropertyStorage, id: u32) -> Option<(u32, Valid)> {
    let mut flags = 0u32;
    let mut value = PROPVARIANT::default();
    // SAFETY: one spec, one flag word, one value, all live for the call.
    unsafe { props.GetPropertyAttributes(1, &spec(id), &mut flags, &mut value) }.ok()?;
    // The valid values come as a vector of 32-bit integers.
    // SAFETY: the union is read according to its type tag; for a vector of
    // 32-bit integers the payload is a count and a pointer to that many.
    let numbers: Vec<i32> = unsafe {
        let inner = &value.Anonymous.Anonymous;
        let base = inner.vt.0 & !VT_VECTOR.0;
        let is_vector = inner.vt.0 & VT_VECTOR.0 != 0;
        let is_int = [VT_I4.0, VT_UI4.0, VT_INT.0, VT_UINT.0].contains(&base);
        let list = inner.Anonymous.cal;
        if is_vector && is_int && !list.pElems.is_null() {
            std::slice::from_raw_parts(list.pElems, list.cElems as usize).to_vec()
        } else {
            Vec::new()
        }
    };
    let valid = if flags & WIA_PROP_RANGE != 0 && numbers.len() > WIA_RANGE_STEP as usize {
        Valid::Range {
            min: numbers[WIA_RANGE_MIN as usize],
            nominal: numbers[WIA_RANGE_NOM as usize],
            max: numbers[WIA_RANGE_MAX as usize],
            step: numbers[WIA_RANGE_STEP as usize],
        }
    } else if flags & WIA_PROP_LIST != 0 && numbers.len() > WIA_LIST_VALUES as usize {
        Valid::List { nominal: numbers[WIA_LIST_NOM as usize], values: numbers[WIA_LIST_VALUES as usize..].to_vec() }
    } else {
        Valid::Unknown
    };
    Some((flags, valid))
}

/// Every property of an item, one line each: for the first runs on a new
/// driver, to see what it offers beyond the standard settings.
fn dump(props: &IWiaPropertyStorage) -> Vec<String> {
    let mut lines = Vec::new();
    // SAFETY: plain call.
    let Ok(list) = (unsafe { props.Enum() }) else { return lines };
    loop {
        let mut one = [STATPROPSTG::default()];
        let mut got = 0u32;
        // SAFETY: room for one entry; `got` is a live local.
        let hr = unsafe { list.Next(&mut one, Some(&mut got)) };
        if hr != S_OK || got == 0 {
            break;
        }
        let id = one[0].propid;
        let name = if one[0].lpwstrName.is_null() {
            String::new()
        } else {
            // SAFETY: a NUL-terminated string the enumerator allocated for us;
            // it is ours to free.
            unsafe {
                let name = one[0].lpwstrName.to_string().unwrap_or_default();
                CoTaskMemFree(Some(one[0].lpwstrName.0 as *const c_void));
                name
            }
        };
        let value = read(props, id).map(|v| v.to_string()).unwrap_or_default();
        let how = match attributes(props, id) {
            Some((flags, valid)) => {
                let access = if flags & WIA_PROP_WRITE != 0 { "can be set" } else { "read-only" };
                match valid {
                    Valid::Unknown => access.to_string(),
                    v => format!("{access}: {}", v.describe()),
                }
            }
            None => String::new(),
        };
        let private = if id >= WIA_PRIVATE_ITEMPROP { ", driver's own" } else { "" };
        lines.push(format!("  {name} [{id}{private}] = {value}  ({how})"));
    }
    lines
}

// ---------------------------------------------------------------- transfer

/// What the callbacks and the scanning thread share during one scan.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// the BMP the driver writes, and where its next write goes
    data: Vec<u8>,
    position: usize,
    /// how many streams the driver asked for (one, for a flatbed)
    streams: u32,
    /// status the device reported on the way (warming up and the like)
    statuses: Vec<HRESULT>,
    /// Windows' own memory stream, when that is used in place of ours
    system: Option<SystemStream>,
}

/// A stream from `SHCreateMemStream`, shared between COM's threads and ours.
struct SystemStream(IStream);
// SAFETY: every thread that touches it is in the multithreaded apartment,
// where an interface pointer may be used from any thread.
unsafe impl Send for SystemStream {}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The object WIA calls during `Download`.
#[implement(IWiaTransferCallback)]
struct Callback {
    shared: Arc<Shared>,
    /// expected size of the BMP, to reserve the memory once
    reserve: usize,
    /// use Windows' memory stream instead of ours (MULTIPASS_WIA_SYSTEM_STREAM=1)
    system_stream: bool,
}

impl IWiaTransferCallback_Impl for Callback_Impl {
    fn TransferCallback(&self, _flags: i32, params: *const WiaTransferParams) -> windows::core::Result<()> {
        // SAFETY: WIA passes a valid pointer or none.
        if let Some(p) = unsafe { params.as_ref() } {
            if p.lMessage == WIA_TRANSFER_MSG_DEVICE_STATUS as i32 {
                let mut st = self.shared.lock();
                if !st.statuses.contains(&p.hrErrorStatus) {
                    st.statuses.push(p.hrErrorStatus);
                }
            }
        }
        Ok(()) // S_OK: carry on
    }

    fn GetNextStream(&self, _flags: i32, _item: &BSTR, _full_item: &BSTR) -> windows::core::Result<IStream> {
        let mut st = self.shared.lock();
        st.streams += 1;
        if st.streams > 1 {
            // A flatbed sends one picture.  Anything more goes nowhere.
            return Ok(MemStream { shared: Arc::new(Shared::default()) }.into());
        }
        if self.system_stream {
            // SAFETY: plain call; no initial content.
            let stream = unsafe { SHCreateMemStream(None) }.ok_or_else(|| Error::from(STG_E_MEDIUMFULL))?;
            st.system = Some(SystemStream(stream.clone()));
            return Ok(stream);
        }
        st.data = Vec::with_capacity(self.reserve);
        st.position = 0;
        Ok(MemStream { shared: self.shared.clone() }.into())
    }
}

/// A stream that writes into `State::data`.  The driver writes the BMP from
/// start to end, and may go back to fill in the header.
#[implement(IStream)]
struct MemStream {
    shared: Arc<Shared>,
}

impl ISequentialStream_Impl for MemStream_Impl {
    fn Read(&self, buffer: *mut c_void, count: u32, read: *mut u32) -> HRESULT {
        if buffer.is_null() {
            return E_POINTER;
        }
        let mut st = self.shared.lock();
        let from = st.position.min(st.data.len());
        let n = (st.data.len() - from).min(count as usize);
        // SAFETY: the caller's buffer holds `count` bytes and n <= count; the
        // source range is inside `data`.
        unsafe {
            std::ptr::copy_nonoverlapping(st.data.as_ptr().add(from), buffer as *mut u8, n);
            if !read.is_null() {
                *read = n as u32;
            }
        }
        st.position = from + n;
        if n < count as usize {
            S_FALSE
        } else {
            S_OK
        }
    }

    fn Write(&self, buffer: *const c_void, count: u32, written: *mut u32) -> HRESULT {
        if buffer.is_null() && count > 0 {
            return E_POINTER;
        }
        let mut st = self.shared.lock();
        let from = st.position;
        let Some(end) = from.checked_add(count as usize).filter(|&e| e <= STREAM_LIMIT) else {
            return STG_E_MEDIUMFULL;
        };
        if count > 0 {
            // SAFETY: the caller's buffer holds `count` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, count as usize) };
            if from == st.data.len() {
                st.data.extend_from_slice(bytes); // the usual case: appending
            } else {
                if st.data.len() < end {
                    st.data.resize(end, 0);
                }
                st.data[from..end].copy_from_slice(bytes);
            }
        }
        st.position = end;
        if !written.is_null() {
            // SAFETY: a valid pointer from the caller.
            unsafe { *written = count };
        }
        S_OK
    }
}

impl IStream_Impl for MemStream_Impl {
    fn Seek(&self, by: i64, origin: STREAM_SEEK, new_position: *mut u64) -> windows::core::Result<()> {
        let mut st = self.shared.lock();
        let base = if origin == STREAM_SEEK_SET {
            0
        } else if origin == STREAM_SEEK_CUR {
            st.position as i64
        } else if origin == STREAM_SEEK_END {
            st.data.len() as i64
        } else {
            return Err(STG_E_INVALIDFUNCTION.into());
        };
        let to = base.checked_add(by).filter(|&p| p >= 0 && p as u64 <= STREAM_LIMIT as u64);
        let Some(to) = to else { return Err(STG_E_INVALIDFUNCTION.into()) };
        st.position = to as usize;
        if !new_position.is_null() {
            // SAFETY: a valid pointer from the caller.
            unsafe { *new_position = to as u64 };
        }
        Ok(())
    }

    fn SetSize(&self, size: u64) -> windows::core::Result<()> {
        if size > STREAM_LIMIT as u64 {
            return Err(STG_E_MEDIUMFULL.into());
        }
        self.shared.lock().data.resize(size as usize, 0);
        Ok(())
    }

    fn CopyTo(&self, _to: Ref<IStream>, _count: u64, _read: *mut u64, _written: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _flags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _offset: u64, _count: u64, _kind: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _offset: u64, _count: u64, _kind: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, stat: *mut STATSTG, _flags: &STATFLAG) -> windows::core::Result<()> {
        if stat.is_null() {
            return Err(E_POINTER.into());
        }
        let size = self.shared.lock().data.len() as u64;
        // SAFETY: a valid pointer from the caller; the struct is plain data.
        unsafe {
            *stat = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: size, ..Default::default() };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// Everything in a system memory stream.
fn read_all(stream: &IStream) -> Result<Vec<u8>, String> {
    let fail = |e: Error| format!("Cannot read the scan back from memory: {}", explain(&e));
    let mut size = 0u64;
    // SAFETY: `size` is a live local.
    unsafe {
        stream.Seek(0, STREAM_SEEK_END, Some(&mut size)).map_err(fail)?;
        stream.Seek(0, STREAM_SEEK_SET, None).map_err(fail)?;
    }
    let mut data = vec![0u8; size as usize];
    let mut done = 0usize;
    while done < data.len() {
        let want = (data.len() - done).min(1 << 24) as u32;
        let mut got = 0u32;
        // SAFETY: the buffer has room for `want` bytes from `done` on.
        let hr = unsafe { stream.Read(data[done..].as_mut_ptr() as *mut c_void, want, Some(&mut got)) };
        if hr.is_err() || got == 0 {
            break;
        }
        done += got as usize;
    }
    data.truncate(done);
    Ok(data)
}

// ---------------------------------------------------------------- the scanner

struct DeviceInfo {
    id: String,
    name: String,
    /// `WIA_DIP_DEV_TYPE`, if the device says
    kind: Option<i32>,
}

fn devices(manager: &IWiaDevMgr2) -> Result<Vec<DeviceInfo>, String> {
    // SAFETY: plain call.
    let list = unsafe { manager.EnumDeviceInfo(WIA_DEVINFO_ENUM_LOCAL as i32) }
        .map_err(|e| format!("Cannot list the scanners: {}", explain(&e)))?;
    let mut found = Vec::new();
    loop {
        let mut one: Option<IWiaPropertyStorage> = None;
        let mut got = 0u32;
        // SAFETY: room for one entry; `got` is a live local.
        if unsafe { list.Next(1, &mut one, &mut got) }.is_err() {
            break;
        }
        let Some(info) = one else { break };
        if let Some(id) = read_string(&info, WIA_DIP_DEV_ID) {
            let name = read_string(&info, WIA_DIP_DEV_NAME).unwrap_or_else(|| id.clone());
            found.push(DeviceInfo { id, name, kind: read_i32(&info, WIA_DIP_DEV_TYPE) });
        }
    }
    Ok(found)
}

/// The scanner to use: the one `MULTIPASS_SCANNER` names (part of its name),
/// else an Epson Perfection, else any Epson, else the first scanner.  Windows
/// also lists network printers with a scanner here, so the first is not
/// always the right one.
fn choose(found: &[DeviceInfo], wanted: Option<&str>) -> Result<usize, String> {
    let is_scanner = |d: &DeviceInfo| d.kind.is_none_or(|k| (k >> 16) & 0xffff == DEVICE_TYPE_SCANNER);
    let scanners: Vec<usize> = (0..found.len()).filter(|&i| is_scanner(&found[i])).collect();
    let named = |part: &str| scanners.iter().copied().find(|&i| found[i].name.to_lowercase().contains(&part.to_lowercase()));
    let names = || scanners.iter().map(|&i| found[i].name.as_str()).collect::<Vec<_>>().join(", ");
    if let Some(w) = wanted {
        return named(w).ok_or_else(|| format!("No scanner named \"{w}\" (MULTIPASS_SCANNER). Windows knows: {}.", names()));
    }
    named("perfection").or_else(|| named("epson")).or_else(|| scanners.first().copied()).ok_or_else(|| {
        "No scanner found. Check that it is plugged in and that Epson's driver for it is installed \
         (it must appear under \"Printers & scanners\" in Windows' Settings)."
            .to_string()
    })
}

/// Settings that would make two scans of the same photo differ, and the value
/// that switches each off.  A driver need not have any of them.
const NEUTRAL: [(u32, &str, Option<i32>); 6] = [
    // `None`: the driver's own nominal value (0 by convention)
    (WIA_IPS_BRIGHTNESS, "brightness", None),
    (WIA_IPS_CONTRAST, "contrast", None),
    (WIA_IPS_AUTO_CROP, "automatic cropping", Some(WIA_AUTO_CROP_DISABLED as i32)),
    (WIA_IPS_AUTO_DESKEW, "automatic straightening", Some(WIA_AUTO_DESKEW_OFF as i32)),
    (WIA_IPS_ROTATION, "rotation", Some(0)),
    (WIA_IPS_PREVIEW, "preview mode", Some(WIA_FINAL_SCAN as i32)),
];

/// Apply [`NEUTRAL`]; one line per setting the driver has.
fn neutral_settings(props: &IWiaPropertyStorage) -> Vec<String> {
    let mut lines = Vec::new();
    for (id, what, fixed) in NEUTRAL {
        let Some(now) = read_i32(props, id) else { continue };
        let (flags, valid) = attributes(props, id).unwrap_or((0, Valid::Unknown));
        let Some(want) = fixed.or(valid.nominal()) else {
            lines.push(format!("{what}: {now} (the driver names no neutral value; left alone)"));
            continue;
        };
        if now == want {
            lines.push(format!("{what}: {now} (neutral already)"));
        } else if flags & WIA_PROP_WRITE == 0 {
            lines.push(format!("{what}: {now}, wanted {want}, but the driver does not let it be set"));
        } else {
            match write_i32(props, id, want) {
                Ok(()) => lines.push(format!("{what}: set to {want} (was {now})")),
                Err(e) => lines.push(format!("{what}: {now}, could not be set to {want}: {}", explain(&e))),
            }
        }
    }
    lines
}

pub struct WiaScanner {
    name: String,
    glass_mm: (f64, f64),
    notes: Vec<String>,
    /// resolutions the driver accepts
    resolutions: Valid,
    // Interfaces first, the apartment last: fields are dropped in this order.
    props: IWiaPropertyStorage,
    item: IWiaItem2,
    _root: IWiaItem2,
    _mta: Mta,
}

// SAFETY: the interface pointers were made in the multithreaded apartment
// (`Mta::enter` refuses any other thread), where they may be used from any
// thread; `scan` checks the thread it runs on again.  `&mut self` keeps two
// threads from using them at once.
unsafe impl Send for WiaScanner {}

pub fn open_first() -> Result<Box<dyn Scanner>, String> {
    let mta = Mta::enter()?;
    // SAFETY: plain COM activation of Windows' device manager, which runs in
    // the scanning service.
    let manager: IWiaDevMgr2 = unsafe { CoCreateInstance(&WiaDevMgr2, None, CLSCTX_LOCAL_SERVER) }.map_err(|e| {
        format!(
            "Windows' scanning service (\"Windows Image Acquisition\") is not available: {}",
            explain(&e)
        )
    })?;
    let found = devices(&manager)?;
    let wanted = std::env::var("MULTIPASS_SCANNER").ok().filter(|s| !s.trim().is_empty());
    let chosen = &found[choose(&found, wanted.as_deref().map(str::trim))?];
    let mut notes = vec![format!("interface: Windows Image Acquisition 2.0, device \"{}\"", chosen.name)];
    for d in found.iter().filter(|d| d.id != chosen.id) {
        notes.push(format!("also listed by Windows, not used: \"{}\"", d.name));
    }

    // SAFETY: plain call with a valid string.
    let root = unsafe { manager.CreateDevice(0, &BSTR::from(chosen.id.as_str())) }
        .map_err(|e| format!("Cannot open {}: {}", chosen.name, explain(&e)))?;
    if let Ok(root_props) = root.cast::<IWiaPropertyStorage>() {
        let version = |id| read_string(&root_props, id).unwrap_or_else(|| "?".into());
        notes.push(format!(
            "driver version {}, WIA version {}, firmware {}",
            version(WIA_DIP_DRIVER_VERSION),
            version(WIA_DIP_WIA_VERSION),
            version(WIA_DPA_FIRMWARE_VERSION)
        ));
    }

    // The glass is the child item of category "flatbed".
    // SAFETY: plain calls; `Next` gets room for one entry.
    let item = unsafe {
        let children =
            root.EnumChildItems(None).map_err(|e| format!("Cannot look inside {}: {}", chosen.name, explain(&e)))?;
        let mut flatbed = None;
        let mut first = None;
        loop {
            let mut one: Option<IWiaItem2> = None;
            let mut got = 0u32;
            if children.Next(1, &mut one, &mut got).is_err() {
                break;
            }
            let Some(child) = one else { break };
            if child.GetItemCategory().is_ok_and(|c| c == WIA_CATEGORY_FLATBED) {
                flatbed = Some(child);
                break;
            }
            first.get_or_insert(child);
        }
        match (flatbed, first) {
            (Some(f), _) => f,
            (None, Some(f)) => {
                notes.push("no unit of category \"flatbed\": using the device's first unit".into());
                f
            }
            (None, None) => return Err(format!("{} offers nothing to scan from through Windows' scanning interface.", chosen.name)),
        }
    };
    let props: IWiaPropertyStorage =
        item.cast().map_err(|e| format!("Cannot read the settings of {}: {}", chosen.name, explain(&e)))?;

    // Glass size, in thousandths of an inch.
    let size = (read_i32(&props, WIA_IPS_MAX_HORIZONTAL_SIZE), read_i32(&props, WIA_IPS_MAX_VERTICAL_SIZE));
    let size = match size {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => root.cast::<IWiaPropertyStorage>().ok().and_then(|p| {
            Some((read_i32(&p, WIA_DPS_HORIZONTAL_BED_SIZE)?, read_i32(&p, WIA_DPS_VERTICAL_BED_SIZE)?))
        }),
    };
    let Some((w, h)) = size.filter(|&(w, h)| w > 0 && h > 0) else {
        return Err(format!("{} does not say how large its glass is.", chosen.name));
    };
    let glass_mm = (w as f64 * 0.0254, h as f64 * 0.0254);
    let unit = read_string(&props, WIA_IPA_ITEM_NAME).unwrap_or_else(|| "?".into());
    notes.push(format!("unit \"{unit}\", glass {:.1} x {:.1} mm", glass_mm.0, glass_mm.1));

    let resolutions = attributes(&props, WIA_IPS_XRES).map_or(Valid::Unknown, |(_, v)| v);
    notes.push(format!("resolutions offered: {} dpi", resolutions.describe()));
    notes.push(format!(
        "optical resolution: {} dpi",
        read_i32(&props, WIA_IPS_OPTICAL_XRES).map_or("not stated".to_string(), |v| v.to_string())
    ));
    notes.extend(neutral_settings(&props));
    notes.push("every setting of the flatbed unit, as the driver has it now:".into());
    notes.extend(dump(&props));

    Ok(Box::new(WiaScanner { name: chosen.name.clone(), glass_mm, notes, resolutions, props, item, _root: root, _mta: mta }))
}

impl WiaScanner {
    /// Set colour or grey, the resolution and the area; returns the area the
    /// driver settled on, in pixels.
    fn configure(&self, dpi: i32, gray: bool, area: AreaMm) -> Result<(i32, i32, i32, i32), String> {
        let p = &self.props;
        let kind = if gray { "grey" } else { "colour" };
        let data_type = if gray { WIA_DATA_GRAYSCALE } else { WIA_DATA_COLOR };
        write_i32(p, WIA_IPA_DATATYPE, data_type as i32)
            .map_err(|e| format!("The scanner driver refused {kind} scanning: {}", explain(&e)))?;
        // 8 bits per sample; a driver that derives the depth from the type refuses the write
        let _ = write_i32(p, WIA_IPA_DEPTH, if gray { 8 } else { 24 });

        if !self.resolutions.allows(dpi) {
            return Err(format!(
                "The scanner driver does not offer {dpi} dpi through Windows' scanning interface (it offers {}).",
                self.resolutions.describe()
            ));
        }
        for (id, axis) in [(WIA_IPS_XRES, "across"), (WIA_IPS_YRES, "down")] {
            write_i32(p, id, dpi).map_err(|e| format!("The scanner driver refused {dpi} dpi ({axis}): {}", explain(&e)))?;
        }
        // Changing type or resolution may put driver settings back to their defaults.
        let _ = neutral_settings(p);

        let (x, y, mut w, mut h) = area_px(area, dpi, self.glass_mm);
        // The driver's own limit, which may be a pixel or two under ours.
        if let Some((_, Valid::Range { max, .. })) = attributes(p, WIA_IPS_XEXTENT) {
            w = w.min(max - x).max(1);
        }
        if let Some((_, Valid::Range { max, .. })) = attributes(p, WIA_IPS_YEXTENT) {
            h = h.min(max - y).max(1);
        }
        // Position and size are checked against each other at every write, so
        // go through a state that is always valid: origin first, then the
        // size, then the position.
        let _ = write_i32(p, WIA_IPS_XPOS, 0);
        let _ = write_i32(p, WIA_IPS_YPOS, 0);
        let place = [(WIA_IPS_XEXTENT, w, "width"), (WIA_IPS_YEXTENT, h, "height"), (WIA_IPS_XPOS, x, "left edge"), (WIA_IPS_YPOS, y, "top edge")];
        for (id, value, what) in place {
            write_i32(p, id, value).map_err(|e| {
                format!("The scanner driver refused the scan area ({what} {value} px at {dpi} dpi): {}", explain(&e))
            })?;
        }
        let got = (
            read_i32(p, WIA_IPS_XPOS).unwrap_or(x),
            read_i32(p, WIA_IPS_YPOS).unwrap_or(y),
            read_i32(p, WIA_IPS_XEXTENT).unwrap_or(w),
            read_i32(p, WIA_IPS_YEXTENT).unwrap_or(h),
        );
        let res = (read_i32(p, WIA_IPS_XRES).unwrap_or(dpi), read_i32(p, WIA_IPS_YRES).unwrap_or(dpi));
        if res != (dpi, dpi) {
            return Err(format!("The scanner driver changed the resolution to {} x {} dpi; {dpi} was asked for.", res.0, res.1));
        }
        // A driver may round the area to its own grid; more than that is an error.
        let off = [got.0 - x, got.1 - y, got.2 - w, got.3 - h].iter().map(|d| d.abs()).max().unwrap_or(0);
        if off > 16 {
            return Err(format!(
                "The scanner driver changed the scan area: asked for {w} x {h} px at ({x}, {y}), it set {} x {} px at ({}, {}).",
                got.2, got.3, got.0, got.1
            ));
        }

        // The picture comes as an uncompressed BMP file, written to a stream.
        write_guid(p, WIA_IPA_FORMAT, &WiaImgFmt_BMP)
            .map_err(|e| format!("The scanner driver refused to deliver a BMP picture: {}", explain(&e)))?;
        let _ = write_i32(p, WIA_IPA_TYMED, TYMED_FILE.0);
        let _ = write_i32(p, WIA_IPA_COMPRESSION, WIA_COMPRESSION_NONE as i32);
        Ok(got)
    }

    /// Run the scan as configured; returns the BMP.
    fn download(&self, reserve: usize) -> Result<Vec<u8>, String> {
        let transfer: IWiaTransfer =
            self.item.cast().map_err(|e| format!("The scanner driver offers no way to fetch a scan: {}", explain(&e)))?;
        let system_stream = std::env::var_os("MULTIPASS_WIA_SYSTEM_STREAM").is_some_and(|v| v == "1");
        let mut tries = 0;
        loop {
            let shared = Arc::new(Shared::default());
            let callback: IWiaTransferCallback = Callback { shared: shared.clone(), reserve, system_stream }.into();
            // SAFETY: plain call; blocks until the scan is over.  The callback
            // object lives until WIA releases it.
            let result = unsafe { transfer.Download(0, &callback) };
            match result {
                Ok(()) => {
                    let (data, system, streams) = {
                        let mut st = shared.lock();
                        (std::mem::take(&mut st.data), st.system.take(), st.streams)
                    };
                    let data = match system {
                        Some(s) => read_all(&s.0)?,
                        None => data,
                    };
                    if data.is_empty() {
                        return Err(format!(
                            "The scanner driver finished without sending a picture (it asked for {streams} stream(s))."
                        ));
                    }
                    return Ok(data);
                }
                // The lamp of a scanner that has sat idle takes a moment.
                Err(e) if (e.code() == WIA_ERROR_BUSY || e.code() == WIA_ERROR_WARMING_UP) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(e) => {
                    let seen = shared.lock().statuses.iter().map(|s| hex(*s)).collect::<Vec<_>>();
                    let seen = if seen.is_empty() { String::new() } else { format!(" Device status on the way: {}.", seen.join(", ")) };
                    return Err(format!("The scan failed: {}{seen}", explain(&e)));
                }
            }
        }
    }
}

impl Scanner for WiaScanner {
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
        check_thread()?;
        let (_, _, w, h) = self.configure(dpi, gray, area)?;
        // header, palette, and rows padded to four bytes
        let row = (w as usize * if gray { 1 } else { 3 }).div_ceil(4) * 4;
        let data = self.download(54 + 1024 + row * h as usize)?;
        let image = bmp::decode(&data, gray)?;
        // The driver may round the size to its grid, but not make another picture.
        if image.width.abs_diff(w as usize) > 16 || image.height.abs_diff(h as usize) > 16 {
            return Err(format!(
                "The scanner driver sent {} x {} px; {w} x {h} px at {dpi} dpi was asked for.",
                image.width, image.height
            ));
        }
        Ok(image)
    }
}
