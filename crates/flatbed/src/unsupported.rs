//! Systems without a scanner backend yet.  Everything that does not need a
//! scanner (combining saved scans, post-processing, the test mode with
//! `MULTIPASS_FAKE_SCANS`) still works there.

use crate::Scanner;

pub fn open_first() -> Result<Box<dyn Scanner>, String> {
    Err(format!("Scanning is not supported on this system yet ({}).", std::env::consts::OS))
}

pub fn run_with_main_loop<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    work()
}
