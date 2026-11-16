# Linux: compile, install and run

Tested on Ubuntu (x86-64) with OpenCV 4.10 and an Epson Perfection V19 II. The Fedora commands are the equivalents and have not been run.

## What you need

| What | Why |
|---|---|
| Epson Scan 2 | Epson's scanner driver. The app loads its `libepsonscan2.so` at run time. |
| Rust 1.88 or newer | the app is written in it |
| A C/C++ compiler and `pkg-config` | Rust's linker, and the C++ glue of the OpenCV bindings |
| OpenCV 4 or 5, with headers | the image processing library |
| clang and libclang | read OpenCV's C++ headers to generate the Rust bindings |

About 1 GB of disk space for the build, besides the packages.

## 1. Epson Scan 2

Epson distributes it as a bundle of two packages: the driver, and a closed-source plug-in that the Perfection V19 II and similar models need.

1. Download the bundle for your distribution from Epson: <https://support.epson.net/linux/en/epsonscan2.php> (`epsonscan2-bundle-<version>.x86_64.deb.tar.gz` for Debian and Ubuntu, `.rpm.tar.gz` for Fedora and openSUSE).
2. Unpack it and run its installer:

   ```sh
   tar xf epsonscan2-bundle-*.x86_64.deb.tar.gz
   cd epsonscan2-bundle-*.x86_64.deb
   ./install.sh
   ```

3. Plug the scanner in, start `epsonscan2` and scan once, to know that driver, cable and permissions work. Then close it: only one program can use the scanner at a time.

The app looks for the library in these places, and in `ES2_LIB` first if that is set:

```
/usr/lib/x86_64-linux-gnu/epsonscan2/libepsonscan2.so
/usr/lib/aarch64-linux-gnu/epsonscan2/libepsonscan2.so
/usr/lib64/epsonscan2/libepsonscan2.so
/usr/lib/epsonscan2/libepsonscan2.so
/usr/local/lib/epsonscan2/libepsonscan2.so
```

## 2. Compiler and libraries

Debian, Ubuntu:

```sh
sudo apt install build-essential pkg-config clang libclang-dev libopencv-dev git curl
```

Fedora (not run):

```sh
sudo dnf install gcc gcc-c++ pkgconf-pkg-config clang clang-devel opencv-devel git curl
```

## 3. Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
cargo --version
```

A distribution's own `cargo` package also works if it is 1.88 or newer.

## 4. Get the source

```sh
git clone <URL of this repository> multipass-scanner
cd multipass-scanner
```

Every command below is run in this folder.

## 5. Optional: test the scanner alone

Needs neither OpenCV nor the app. Put a photo face down against the top edge of the glass, off to one side, close the lid, then:

```sh
make scan-test
```

It opens the scanner, prints what the driver offers, takes five small scans in about a minute and checks sizes, the origin of the scan area, repeatability and shear by itself. Pictures go to `scan-test/`: in `2_whole_glass.bmp` the photo must be at the top and not mirrored.

## 6. Build and test

```sh
make          # cargo build --release, as your own user
make test     # the unit tests; none needs a scanner
```

The first build compiles a few hundred packages and generates the OpenCV bindings. It took 43 s on a 16-thread desktop; allow several minutes on a small machine. The program is `target/release/multipass-scanner`.

## 7. Install

```sh
sudo make install
```

| File | What it is |
|---|---|
| `/usr/local/bin/multipass-scanner` | the program |
| `/usr/local/share/applications/multipass-scanner.desktop` | the launcher: **Multi-pass Scanner** in the application menu |
| `/usr/local/share/icons/hicolor/scalable/apps/multipass-scanner.svg` | the icon |

- **Without root:** `make install PREFIX=$HOME/.local` installs for one user; the launcher then points to `~/.local/bin`.
- **Remove:** `sudo make uninstall` (with the same `PREFIX`, if you set one).
- **Update:** `git pull`, `make`, `sudo make install`, with the app closed.
- **Packaging:** `DESTDIR` works as usual.

## 8. Run

Start **Multi-pass Scanner** from the application menu, or run `multipass-scanner`. Without installing: `target/release/multipass-scanner`.

Results are saved as `~/Pictures/img00001.png`, `img00002.png`, … To keep the combining report of a session, start the app from a terminal with `2> run.log`.

To try the app without a scanner:

```sh
MULTIPASS_FAKE_SCANS=a.png,b.png multipass-scanner          # the window, fed from files
multipass-scanner --combine out.png scan1.png scan2.png      # no window
```

## If something goes wrong

| What you see | What to do |
|---|---|
| `The Epson Scan 2 driver (libepsonscan2.so) was not found` | Install Epson Scan 2 (step 1), or set `ES2_LIB` to the library's path. |
| No scanner found, or the scanner is busy | Close Epson Scan 2 and any other scanning program. Check that `epsonscan2` itself finds the scanner. |
| The build stops with `Unable to find libclang` | Install `libclang-dev` (Fedora: `clang-devel`). If it is installed in an unusual place, set `LIBCLANG_PATH` to the folder that holds `libclang.so`. |
| The build stops with `Failed to find installed OpenCV package` or a `pkg-config` error | Install `libopencv-dev` (Fedora: `opencv-devel`) and `pkg-config`. Check with `pkg-config --modversion opencv4`. |
| `linker 'cc' not found` | Install `build-essential` (Fedora: `gcc gcc-c++`). |
| "Is the lid closed?" with the lid closed | The app expects a white lid behind the photo. A dark backing sheet reads as an open lid. |
| The app refuses a scan for lack of memory | Choose a lower resolution, or scan a smaller area. See "Memory and time" in the README. |
