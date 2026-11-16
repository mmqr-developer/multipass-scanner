# macOS: compile, install and run

Tested on an Apple silicon Mac with Homebrew's OpenCV 5.1 and an Epson Perfection V19 II, at 1200 dpi. Intel Macs have not been tried; the paths below that start with `/opt/homebrew` are `/usr/local` there.

## What you need

| What | Why |
|---|---|
| The scanner's driver from Apple or Epson | The app uses Apple's ImageCaptureCore, the framework behind the Image Capture app. Epson Scan 2 is not needed. |
| Xcode command-line tools | compiler, linker, `git`, `make`, `codesign` |
| Homebrew | installs the two libraries below |
| OpenCV | the image processing library |
| LLVM | its `libclang` reads OpenCV's C++ headers to generate the Rust bindings |
| Rust 1.88 or newer | the app is written in it |

Several GB of disk space, most of it for Homebrew's OpenCV and LLVM and the packages they bring along.

## 1. Check the scanner in Image Capture

1. Plug the scanner in and open Apple's **Image Capture** app.
2. The scanner must appear in the sidebar. If macOS offers to download Epson's scanner driver, accept.
3. Click **Show Details** and scan once.
4. Quit Image Capture: only one program can use the scanner at a time.

If the scanner does not appear in Image Capture, this app cannot use it either.

## 2. Tools and libraries

```sh
xcode-select --install

/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

brew install opencv llvm

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
cargo --version
```

- **Homebrew:** skip its line if `brew --version` already works. After a fresh install, follow the two "Next steps" lines it prints, which put `brew` on your `PATH`.
- **OpenCV version:** Homebrew installs OpenCV 5. The code builds against OpenCV 4 and 5.

## 3. Get the source

```sh
git clone <URL of this repository> multipass-scanner
cd multipass-scanner
```

Every command below is run in this folder.

## 4. Test the scanner alone

Needs neither OpenCV nor the app. Put a photo face down against the top edge of the glass, off to one side, close the lid, then:

```sh
make scan-test
```

It opens the scanner, prints every feature the driver offers and what was done to it, takes five small scans in about a minute and checks sizes, the origin of the scan area, repeatability and shear by itself. Pictures go to `scan-test/`:

- **`2_whole_glass.bmp`:** the photo is at the top and not mirrored.
- **`3_patch.bmp`:** colours look right; no sharpening halos along edges.

On a scanner other than the V19 II this is the test that says whether the app can work with it.

## 5. Build and test

```sh
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
make          # cargo build --release
make test     # the unit tests; none needs a scanner
```

The first build compiles a few hundred packages and generates the OpenCV bindings, which takes a few minutes. `LIBCLANG_PATH` is needed for building only.

## 6. Make the app and install it

```sh
make app
cp -R "target/release/Multi-pass Scanner.app" /Applications/
```

`make app` puts the program, `Info.plist` and the icon into `target/release/Multi-pass Scanner.app` and signs it ad hoc.

- **This Mac only:** the app links OpenCV where Homebrew installed it, and the ad-hoc signature is enough only for the Mac that built it. Another Mac needs its own build. Giving the app to others would need OpenCV built into it, an Apple Developer ID signature and notarisation; none of that is set up.
- **Keep OpenCV installed:** `brew uninstall opencv` breaks the app. After `brew upgrade` moves OpenCV to a new version, build again.
- **Remove:** drag **Multi-pass Scanner** from Applications to the Trash.
- **Update:** `git pull`, then steps 5 and 6 again, with the app closed.

## 7. Run

Open **Multi-pass Scanner** from Applications or Launchpad. Results are saved as `~/Pictures/img00001.png`, `img00002.png`, …

From Terminal, which also keeps the combining report:

```sh
APP="/Applications/Multi-pass Scanner.app/Contents/MacOS/multipass-scanner"
"$APP" 2> run.log                                        # the window, with a log
"$APP" --combine out.png scan1.png scan2.png             # no scanner, no window
MULTIPASS_FAKE_SCANS=a.png,b.png "$APP"                  # the window, fed from files
```

## If something goes wrong

| What you see | What to do |
|---|---|
| No scanner found | Step 1. Quit Image Capture, Epson Scan 2 and Epson ScanSmart. Unplug the scanner and plug it in again. |
| The build stops with `Unable to find libclang` or `dyld: Library not loaded: @rpath/libclang.dylib` | `brew install llvm`, and set `LIBCLANG_PATH` as in step 5, in the same Terminal window as `make`. If the library is still not loaded, also `export DYLD_FALLBACK_LIBRARY_PATH="$(brew --prefix llvm)/lib"` (the opencv crate's own advice; not needed on the test Mac). |
| The build stops with `Failed to find installed OpenCV package` | `brew install opencv`; check with `brew --prefix opencv`. |
| The app does not start after a Homebrew upgrade | OpenCV moved. Repeat steps 5 and 6. |
| macOS asks whether the app may access the Pictures folder | Allow it: results are saved there. |
| The photo is found at the wrong end, or the scan is upside down, on another scanner | Run step 4 and look at its "origin" check. `MULTIPASS_ICA_Y_FROM_FAR_END=1` flips the direction the scan area is measured in. |
| Scans come out sharpened or colour-corrected on another scanner | The app switches off every on/off feature the driver offers. Step 4 prints each one; a correction without a switch cannot be turned off. |
