//! Windows only: put the icon and the program's name into the .exe, where
//! Explorer and the Start menu look for them.  On other systems this does
//! nothing (their icons are installed by the packaging).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=packaging/windows/multipass-scanner.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/windows/multipass-scanner.ico");
        res.set("FileDescription", "Multi-pass Scanner");
        res.set("ProductName", "Multi-pass Scanner");
        // The resource compiler comes with the Windows SDK.  Without it the
        // app still builds, with a plain icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=the .exe gets no icon: {e}");
        }
    }
}
