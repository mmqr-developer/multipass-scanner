//! The few things the app asks the operating system directly.  (Scanner access
//! is in the `flatbed` crate; everything else is the same on every system.)

use std::path::PathBuf;

/// Installed memory in bytes, if this system can tell.  `None` makes the
/// combiner take the careful, low-memory path.
pub fn total_ram() -> Option<u64> {
    imp::total_ram()
}

/// Whether [`memory_gb`] and [`reset_peak_memory`] work here.
pub const MEMORY_TRACE: bool = imp::MEMORY_TRACE;

/// This process's memory in GB: (peak since the last reset, held now).
pub fn memory_gb() -> (f64, f64) {
    imp::memory_gb()
}

/// Start measuring the peak afresh.
pub fn reset_peak_memory() {
    imp::reset_peak_memory()
}

/// The user's Pictures folder, where results are saved.
pub fn pictures_dir() -> PathBuf {
    imp::pictures_dir()
}

/// Call first thing in `main`.  On Windows the app is built without a console
/// window of its own; started from a terminal, this connects its output to
/// that terminal, so the command-line modes and error messages can be read.
/// Elsewhere it does nothing.
pub fn attach_console() {
    imp::attach_console()
}

/// `~/Pictures`, the convention on Linux and macOS.
#[cfg(not(target_os = "windows"))]
fn home_pictures() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Pictures")
}

#[cfg(target_os = "linux")]
mod imp {
    pub const MEMORY_TRACE: bool = true;

    pub fn total_ram() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = text.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }

    fn status_gb(field: &str) -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|t| {
                t.lines().find(|l| l.starts_with(field)).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
            })
            .unwrap_or(0.0)
            / 1048576.0
    }

    pub fn memory_gb() -> (f64, f64) {
        (status_gb("VmHWM:"), status_gb("VmRSS:"))
    }

    pub fn reset_peak_memory() {
        let _ = std::fs::write("/proc/self/clear_refs", "5");
    }

    pub(super) use super::home_pictures as pictures_dir;

    pub fn attach_console() {}
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_int, c_void};

    pub const MEMORY_TRACE: bool = false;

    extern "C" {
        // <sys/sysctl.h>, in the system library every program links
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    pub fn total_ram() -> Option<u64> {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: "hw.memsize" is a 64-bit integer; the buffer and its length match.
        let r = unsafe {
            sysctlbyname(c"hw.memsize".as_ptr(), &mut bytes as *mut u64 as *mut c_void, &mut len, std::ptr::null_mut(), 0)
        };
        (r == 0 && len == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
    }

    pub fn memory_gb() -> (f64, f64) {
        (0.0, 0.0)
    }

    pub fn reset_peak_memory() {}

    pub(super) use super::home_pictures as pictures_dir;

    pub fn attach_console() {}
}

// Built on Windows 11; not yet used there with a scanner.
#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    pub const MEMORY_TRACE: bool = false;

    pub fn total_ram() -> Option<u64> {
        let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        // SAFETY: the struct is live and says its own size, as the call requires.
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        (status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
    }

    pub fn memory_gb() -> (f64, f64) {
        (0.0, 0.0)
    }

    pub fn reset_peak_memory() {}

    /// The Pictures folder as Windows knows it, which follows a folder moved
    /// to OneDrive or another drive.
    pub fn pictures_dir() -> PathBuf {
        // SAFETY: on success the call returns a NUL-terminated string that is
        // ours to free.
        unsafe {
            if let Ok(wide) = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None) {
                let path = PathBuf::from(OsString::from_wide(wide.as_wide()));
                CoTaskMemFree(Some(wide.0 as *const c_void));
                if !path.as_os_str().is_empty() {
                    return path;
                }
            }
        }
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Pictures")
    }

    pub fn attach_console() {
        // Fails when there is no terminal (started from the Start menu) or the
        // output already goes somewhere (redirected to a file): both are fine.
        // SAFETY: plain call.
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    pub const MEMORY_TRACE: bool = false;

    pub fn total_ram() -> Option<u64> {
        None // not implemented on this system yet
    }

    pub fn memory_gb() -> (f64, f64) {
        (0.0, 0.0)
    }

    pub fn reset_peak_memory() {}

    pub(super) use super::home_pictures as pictures_dir;

    pub fn attach_console() {}
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn installed_memory_is_found() {
        let ram = super::total_ram().expect("this system reports its memory");
        assert!(ram > 256 * 1024 * 1024, "{ram} bytes");
    }

    #[test]
    fn the_pictures_folder_has_a_name() {
        let dir = super::pictures_dir();
        assert!(dir.file_name().is_some(), "{}", dir.display());
    }
}
