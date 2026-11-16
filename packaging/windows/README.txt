Multi-pass Scanner for Windows
==============================

Scans a photo two to four times on an Epson Perfection V19 II, turning it
between passes, and combines the scans into one cleaner PNG at the scan
resolution (600, 1200, 2400 or 4800 dpi).

To run: unzip this folder anywhere and start multipass-scanner.exe.
Keep all the files together in one folder.  Nothing is installed.

The computer needs
------------------
- Windows 10 or 11, 64-bit, on an Intel or AMD processor.
- Epson's scanner driver for the Perfection V19 II ("Drivers and
  Utilities Combo Package" from Epson's support page).  Close Epson Scan 2
  before starting this app: only one program can use the scanner at a time.
- On a "Windows N" edition: the Media Feature Pack (Settings > Apps >
  Optional features), which OpenCV needs.

The first time, Windows may say it protected your PC from an unknown
publisher, because the program is not signed.  Click "More info", then
"Run anyway".

Results are saved as img00001.png, img00002.png, ... in your Pictures
folder.  An existing file is never overwritten.

Files
-----
multipass-scanner.exe   the program
opencv_world4140.dll    OpenCV 4.14, the image processing library
msvcp140.dll, vcruntime140.dll, vcruntime140_1.dll, concrt140.dll
                        Microsoft Visual C++ runtime (redistributable)
