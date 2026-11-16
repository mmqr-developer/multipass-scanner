#!/bin/sh
# Type-check, from Linux or macOS, the app's Windows-only code.  The app
# itself cannot be checked for Windows away from Windows (it needs OpenCV for
# Windows), so its three Windows-only pieces are checked in a small stand-in
# program: src/platform.rs, the window-icon lines of src/main.rs, and build.rs.
#
#     make check-windows        (needs: rustup target add x86_64-pc-windows-msvc)
#
# This proves names and types, not behaviour.
set -eu

here=$(cd "$(dirname "$0")/../.." && pwd)
work="$here/target/wincheck"
rm -rf "$work/crate"
mkdir -p "$work/crate/src/bin" "$work/crate/packaging/windows"
cp "$here/src/platform.rs" "$work/crate/src/"
cp "$here/packaging/windows/icon-256.png" "$work/crate/packaging/windows/"
cp "$here/Cargo.lock" "$work/crate/"
# build.rs as an ordinary program, with its "only when built on Windows" guard removed
grep -v '#\[cfg(windows)\]' "$here/build.rs" > "$work/crate/src/bin/buildrs.rs"

# the same dependency lines as ../../Cargo.toml
cat > "$work/crate/Cargo.toml" <<'T'
[package]
name = "wincheck"
version = "0.1.0"
edition = "2021"

[workspace]

[dependencies]
eframe = { version = "0.37", default-features = false, features = ["default_fonts", "glow", "wayland", "x11"] }
winresource = "0.1"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.62", features = [
    "Win32_Foundation", "Win32_System_Com", "Win32_System_Console", "Win32_System_SystemInformation", "Win32_UI_Shell",
] }
T

# the lines of src/main.rs that differ on Windows, around a use of everything in platform.rs
cat > "$work/crate/src/main.rs" <<'T'
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod platform;
use eframe::egui;

fn main() {
    platform::attach_console();
    let viewport = egui::ViewportBuilder::default().with_title("x");
    #[cfg(target_os = "windows")]
    let viewport = match eframe::icon_data::from_png_bytes(include_bytes!("../packaging/windows/icon-256.png")) {
        Ok(icon) => viewport.with_icon(icon),
        Err(_) => viewport,
    };
    let _options = eframe::NativeOptions { viewport, ..Default::default() };
    println!("{:?} {:?} {:?} {}", platform::total_ram(), platform::memory_gb(), platform::pictures_dir(), platform::MEMORY_TRACE);
    platform::reset_peak_memory();
}
T

cd "$work/crate"
export CARGO_TARGET_DIR="$work/target"
echo "check-app: src/platform.rs and the icon lines, for Windows"
cargo check --quiet --bin wincheck --target x86_64-pc-windows-msvc
echo "check-app: the body of build.rs"
cargo check --quiet --bin buildrs
echo "check-app: no errors"
