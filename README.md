<img src="packaging/multipass-scanner.svg" width="96" align="right" alt="">

# Multi-pass Scanner

A desktop app that scans one photo two to four times on a flatbed scanner, turned between scans, and combines the scans into one cleaner image.

A flatbed scan contains errors that come from the scanner, not the photo: random noise in every pixel, dust and marks on the glass, streaks from the sensor, and uneven carriage movement that shifts parts of a scan by a few pixels. These errors stay fixed to the scanner. When the photo is turned between scans they land on a different part of the picture each time, so they can be averaged out or rejected.

- **Four scans:** about half the noise of one scan, at full sharpness. Dust and marks on the glass are rejected.
- **Two scans** (photos too long to turn a quarter turn): about 30% less noise. Dust on the glass is not rejected; that needs three scans or more.
- **Output:** a 16-bit PNG at the scan resolution, saved as `img00001.png`, `img00002.png`, … in your Pictures folder. An existing file is never overwritten.

Marks on the print itself appear in every scan, so combining does not remove them. An optional clean-up step fills in small specks and fibres afterwards.

## Status

Written for and tested with one scanner, the **Epson Perfection V19 II**.

| System | Scanner access | State |
|---|---|---|
| Linux (x86-64) | Epson's Epson Scan 2 driver | Works. Developed and tested on Ubuntu with OpenCV 4.10. |
| macOS (Apple silicon) | Apple's ImageCaptureCore | Works at 1200 dpi, with OpenCV 5.1 from Homebrew. The other resolutions have not been scanned there. |
| Windows 10/11 (x64) | Windows Image Acquisition 2.0 | Builds, passes its unit tests and opens its window on Windows 11 with OpenCV 4.14. **Scanning has not been tried with a scanner.** |

Other scanners are untested. On Linux the app drives whatever scanner Epson Scan 2 finds; on macOS and Windows, whatever flatbed the system's own scanning interface offers. The app reads the size of the glass from the scanner and expects a white lid.

## Build and install

One howto per system, each listing every tool and library to install:

- [Linux](docs/howto-linux.md)
- [macOS](docs/howto-macos.md)
- [Windows](docs/howto-windows.md)

## Use

1. Place the photo face down, **touching the top edge of the glass**, and close the lid.
2. Press **Scan** (or Space or Enter). The app finds the photo with two quick grey scans at 75 dpi, then scans only that area, plus an 8 mm margin, in colour.
3. Turn the photo a quarter turn, either way, slide it to a different spot along the top edge, and press **Scan** again. Repeat for up to four scans. A new spot each time helps separate dust on the glass from the photo.
4. Look at the thumbnails. To take a scan again, click its thumbnail and press **Scan N again**.
5. Press **Process**. It is available once there are two scans. The result is saved and shown.
6. Press **Scan next photo** for a new set.

On a Perfection V19 II at 1200 dpi, with a 9 × 9 cm photo, the first scan takes about 60 s, each later scan about 42 s, and combining four scans about 2 minutes.

| Control | What it does |
|---|---|
| **Full-size photo: scan the whole glass** | For a print that covers most of the glass. Skips the grey scans and scans the entire glass, as placed and then turned half round. The result is not cropped. |
| **Resolution** | 600, 1200 (the default), 2400 or 4800 dpi. One resolution per photo: the pulldown is locked while scans are held. Each step up has 4 times the pixels. |
| **Post-process the combined image** | Optional clean-up before saving, see below. Change the boxes after a save and press **Process** again to save another version in a few seconds. |
| **Remove scan N** | Forgets the selected scan without replacing it. |
| **Start over** | Clears all scans. |

- **Long photos:** a photo longer than about 209 mm cannot be turned a quarter turn on a 216 mm glass. The app notices on the first scan and asks for two scans instead: as placed, then turned half round. Slots 3 and 4 take optional repeats.
- **Size of the result:** after the first scan, the window shows the scanned area, the megapixels the result will have and its estimated file size (about 4.8 bytes a pixel).
- **Resolution advice:** the first scan is analysed for how fine the picture's detail is. The window then says whether the chosen resolution suits it or which one would. It is advice; the app scans at the resolution you chose.
- **Memory check:** before a colour scan, the app works out whether the computer can combine scans of that size, and refuses the scan if not.
- **Open lid:** if the glass comes out dark, the app asks whether the lid is closed.
- **The scanner's own buttons** are not used. Press the on-screen button or the keyboard.

### Post-processing

- **Remove texture patterns:** takes out the regular surface pattern of prints on silk or honeycomb paper. A regular pattern is a few sharp peaks in the image's spectrum; they are lowered to the level beside them. A print without such a pattern is left exactly as it is. A print whose whole surface is one regular pattern, such as a newspaper halftone, would lose that pattern.
- **Remove lint:** fills in dust specks and fibres lying on the print. A mark counts as lint when it stands out by at least 8 levels of 255, is at most 0.15 mm wide, has a hard edge and lies on a plain background. Nothing is done if more than 2% of the image looks like lint (snow, stars, glitter). On a very sharp print, real detail of that size can be taken for lint; compare with the version saved without it if in doubt.

## Command line

```sh
# combine existing scans without a scanner
multipass-scanner --combine out.png scan1.png scan2.png [scan3.png scan4.png]

# find, measure and scan a photo once, keeping every image
multipass-scanner --capture DIR

# one full-size scan: the whole glass in colour
multipass-scanner --capture-full DIR

# how fine the detail of a saved scan is, and the advice the window would give
multipass-scanner --detail scan.png

# post-process an image combined before: texture, lint or texture,lint
multipass-scanner --post texture,lint in.png out.png

# any of these at another resolution than 1200 dpi: --dpi N first
multipass-scanner --dpi 2400 --combine out.png scan1.png scan2.png

# run the window with files instead of the scanner
MULTIPASS_FAKE_SCANS=a.png,b.png,c.png,d.png multipass-scanner
```

| Variable | Effect |
|---|---|
| `ES2_LIB` | Linux: path of `libepsonscan2.so`, if it is not in a usual place |
| `MULTIPASS_SCANNER` | Windows: part of the name of the scanner to use, when Windows knows several |
| `MULTIPASS_FAKE_SCANS=a.png,b.png` | the window takes its scans from these files |
| `MULTIPASS_FLOW_TILE_MP=4` | force optical-flow tiles of that many megapixels; `0` forces the whole frame |
| `MULTIPASS_RAM_GB=16` | behave as if the computer had that much memory |
| `MULTIPASS_KEEP_ALIGNED=DIR` | save each scan as it was aligned to the first (`DIR/aligned_1.png`, …) |
| `MULTIPASS_MEM=1` | Linux, with `--combine`: print the peak memory of each stage |
| `MULTIPASS_ICA_KEEP_FEATURES=1` | macOS: leave the driver's correction features as they are |
| `MULTIPASS_ICA_Y_FROM_FAR_END=1` | macOS: for a driver that measures the scan area from the far end of the glass |
| `MULTIPASS_WIA_SYSTEM_STREAM=1` | Windows: let Windows hold the scan in its own memory stream |

## How it works

**Scanning.** Every automatic or lossy correction of the scanner driver is switched off, so the scans differ only in where the photo lies. A pixel of the 75 dpi grey scans counts as photo when it is darker than the lid by more than the lid's own variation.

**Combining.** Every scan is registered to the first one:

1. **Coarse placement:** SIFT features and a RANSAC similarity transform.
2. **Rigid refinement:** ECC, inside the area of the photo.
3. **Local refinement:** tile-by-tile phase correlation against the consensus of all scans, fitted with a smooth warp.
4. **Carriage wobble:** dense optical flow against the other scans, fitted per scan as a shift that depends only on the scan line. Every round must improve the agreement or is undone.
5. **Resampling:** one Lanczos resampling per scan, at full resolution.
6. **Exposure match:** gain and offset per scan and colour channel.
7. **Combining:** sigma-clipped mean, 16-bit output. With three or more scans, a value that disagrees with the others at a pixel is left out.

The result keeps the orientation of the lowest-numbered scan and includes the photo's border plus up to 8 mm of lid.

## Memory and time

The app chooses by installed memory: with more than 16 GB the optical flow runs on the whole frame; with 16 GB or less it runs in 4-megapixel tiles, which is slower (182 s instead of 141 s on one four-scan set) and peaks at 2.8 GB instead of 6.9 GB.

Estimates for four scans of a 4 × 6 inch photo with its margins, on a computer with more than 16 GB:

| Resolution | Pixels per scan | Memory to combine | Result file |
|---|---|---|---|
| 600 dpi | 11 megapixels | about 4 GB | about 53 MB |
| 1200 dpi | 44 megapixels | about 9 GB | about 210 MB |
| 2400 dpi | 177 megapixels | about 11 GB | about 850 MB |
| 4800 dpi | 708 megapixels | about 34 GB | about 3.4 GB |

These come from the app's own estimate, which it uses for the memory check. Measured: four 174-megapixel scans (a 71 × 48 mm print at 4800 dpi) combined in 4.8 minutes with a 10 GB peak, on a 16-thread desktop.

## Which resolution?

Measured on the Perfection V19 II with one small colour print, scanned at 1200, 2400 and 4800 dpi:

- **The scanner really samples at 4800 dpi,** and the noise per pixel is the same at every resolution. A finer scan is therefore a cleaner scan of the same detail: one 4800 dpi scan shrunk to 1200 dpi has the noise of 16 scans at 1200 dpi.
- **Higher resolution needs fewer passes, not more.** The passes are still needed for what one scan cannot do: rejecting dust (three or more), and averaging out the surface of the print as lit from one side.
- **For that print, 4800 dpi showed no picture detail that 2400 dpi lacks,** at 4 times the pixels and about 3 times the scan time (38 s, 2 minutes and 5.5 to 7.6 minutes per scan at 1200, 2400 and 4800 dpi).

One print and one scanner: a sharper print, or film, may hold more.

## Known limits

- The photo must leave some lid visible beside it, so it must be narrower than about 209 mm on a 216 mm glass, unless the full-size box is ticked.
- Scans are aligned on working images of at most 30 megapixels. At 4800 dpi, detailed areas of the aligned scans still differed by 1 to 2 pixels (5 to 10 µm).
- The limits of the resolution advice were set on one photo.
- The lint filter's thresholds at 600 dpi are untested on real scans.
- The app saves at the scan resolution. Saving a 2400 or 4800 dpi scan shrunk to 1200 dpi is not an option yet.
- macOS: the app bundle runs only on a Mac with OpenCV installed. Windows: no installer, and the program is not signed.

## Source

One source tree serves every system. Everything except scanner access and a few system lookups is shared.

| Path | Contents |
|---|---|
| `src/main.rs` | the window (egui) and the command-line modes |
| `src/worker.rs` | scanner thread: finding the photo, scanning, combining, saving |
| `src/detect.rs` | finding the photo in the 75 dpi grey scans |
| `src/detail.rs` | how fine the detail of the first scan is |
| `src/combine.rs` | alignment and combining |
| `src/post.rs` | texture patterns and lint |
| `src/output.rs` | 16-bit PNG with its resolution, the next free `imgNNNNN.png` |
| `src/platform.rs` | installed memory and the Pictures folder, one section per system |
| `src/cvx.rs` | the few OpenCV calls that differ between OpenCV 4 and 5 |
| `crates/flatbed/` | scanner access, with no OpenCV or GUI dependency: `linux.rs` (Epson Scan 2), `macos.rs` (ImageCaptureCore), `wia.rs` (Windows) |
| `crates/flatbed/examples/scan-test.rs` | scanner test without the app |
| `packaging/` | launcher, icons, the macOS bundle script, the Windows build, install and zip scripts |

`cargo test --workspace` runs the unit tests; none needs a scanner. A new system needs one backend file in `crates/flatbed/src`, a section in `src/platform.rs` and a folder under `packaging/`.

## Licence

GPL-3.0-or-later, see [LICENSE](LICENSE).

- **Epson Scan 2** is not part of this repository. On Linux the app loads Epson's `libepsonscan2.so` (LGPL-2.1-or-later) at run time; the Perfection V19 II also needs Epson's closed-source plug-in, which Epson distributes with it.
- **OpenCV** (Apache-2.0) and **egui/eframe** (MIT or Apache-2.0) are used as libraries.
- This project is not affiliated with or endorsed by Seiko Epson Corporation. Epson and Perfection are trademarks of their owner.
