# Windows: compile, install and run

**State of the Windows version:** it builds, passes its unit tests and opens its window on Windows 11 (x64) with OpenCV 4.14. **Scanning has not been tried with a scanner**, and `install.ps1` has not been run. Step 5 is the test that says whether scanning works on your machine.

Everything below is typed into **PowerShell** (Start menu → "PowerShell" or "Terminal"). No step needs an administrator window; the installers ask for permission themselves.

## What you need

- **Windows 10 or 11, 64-bit, Intel or AMD.** Windows on ARM is not covered.
- **About 15 GB of free disk space** (a rough figure; Microsoft's compiler is the largest part).

| What | Why | Step |
|---|---|---|
| Epson's scanner driver | gives Windows a WIA driver for the scanner; the app scans through Windows Image Acquisition | 1 |
| Git | fetches the source | 2 |
| Rust 1.95 or newer | the app is written in it | 2 |
| Visual Studio Build Tools, C++ workload | Rust on Windows uses Microsoft's linker; the OpenCV bindings have C++ glue; the Windows SDK puts the icon into the `.exe` | 2 |
| LLVM | its `libclang.dll` reads OpenCV's C++ headers to generate the Rust bindings | 2 |
| OpenCV 4 or 5, the official Windows package | the image processing library | 3 |

## 1. Epson's scanner driver

1. From Epson's support page for your scanner, install the **Drivers and Utilities Combo Package**, or at least **Scanner Driver and Epson Scan 2 Utility**.
2. Plug the scanner in. **Settings → Bluetooth & devices → Printers & scanners** should list it.
3. Scan once with **Epson Scan 2**, to know that scanner and cable work. Then **close it**: only one program can use the scanner at a time.

## 2. Tools

```powershell
winget install --id Git.Git -e
winget install --id Rustlang.Rustup -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id LLVM.LLVM -e
```

- **The third line** installs Microsoft's C++ compiler, linker and the Windows SDK, without the Visual Studio editor. It is several GB. A newer version of the Build Tools works too (the test build used Build Tools 2026), as does a full Visual Studio with "Desktop development with C++".
- **Without `winget`:** install "App Installer" from the Microsoft Store, or download each tool from its own site: git-scm.com, rustup.rs, visualstudio.microsoft.com/downloads ("Build Tools for Visual Studio", tick "Desktop development with C++"), and the LLVM releases on github.com/llvm/llvm-project.

**Close PowerShell and open it again**, so it sees the new programs, then check:

```powershell
git --version
cargo --version
Test-Path "$env:ProgramFiles\LLVM\bin\libclang.dll"     # must print True
```

## 3. OpenCV

Download OpenCV's own Windows package and unpack it to `C:\opencv`:

```powershell
curl.exe -L -o "$env:TEMP\opencv-4.14.0-windows.exe" https://github.com/opencv/opencv/releases/download/4.14.0/opencv-4.14.0-windows.exe
Start-Process -Wait "$env:TEMP\opencv-4.14.0-windows.exe" -ArgumentList '-oC:\', '-y'
Test-Path C:\opencv\build\x64\vc*\lib\opencv_world4140.lib     # must print True
```

- **Size:** the download is about 200 MB; unpacked it takes over 1 GB.
- **If a window asks "Extract to":** enter `C:\` and press Extract. The package creates the `opencv` folder itself.
- **Another version:** the build script takes whatever `opencv_world*.lib` it finds. 4.10, 4.14 and 5.1 are the versions the code has been built against on one system or another.
- **Another place:** unpack it anywhere and set `OPENCV_DIR` to the folder that contains `build`, for example `$env:OPENCV_DIR = 'D:\libs\opencv'`, in the PowerShell window you build in.

## 4. Get the source

```powershell
cd $env:USERPROFILE
git clone <URL of this repository> multipass-scanner
cd multipass-scanner
```

Every command below is run in this folder.

## 5. Test the scanner alone

Needs neither OpenCV nor the app. Put a photo face down against the top edge of the glass, off to one side, close the lid, then:

```powershell
cmd /c "cargo run --release -p flatbed --example scan-test -- scan-test > scan-test.log 2>&1"
Get-Content scan-test.log
```

The first line shows nothing while it runs (a few minutes the first time, for the compiling). The program opens the scanner, prints everything the driver offers, takes five small scans in about a minute and checks sizes, the origin of the scan area, repeatability and shear by itself. Pictures go to the `scan-test` folder:

- **`2_whole_glass.bmp`:** the photo is at the top and not mirrored; the lid around it is plain white, without a frame or crop.
- **`3_patch.bmp`:** colours look right; no sharpening halos along edges.

If this test fails, the app will not scan either. `scan-test.log` is what a bug report needs.

If Windows knows more than one scanner (a network printer with a scanner counts), the app prefers an Epson Perfection, then any Epson. To name one yourself, set part of its name first: `$env:MULTIPASS_SCANNER = 'V19'`.

## 6. Build and test

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1 -Test
```

The script finds LLVM and OpenCV, builds the program, runs the unit tests (`-Test`) and puts OpenCV's DLL beside the program. The first build compiles a few hundred packages and generates the OpenCV bindings: allow 10 to 20 minutes (an estimate). Later builds take seconds.

`-ExecutionPolicy Bypass` is needed because Windows does not run unsigned scripts by default. It applies to this one command only.

Try the result before installing it:

```powershell
target\release\multipass-scanner.exe
```

## 7. Install

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1
```

This copies the program and OpenCV's DLL to `%LOCALAPPDATA%\Programs\Multi-pass Scanner` and adds **Multi-pass Scanner** to the Start menu. It needs no administrator rights and writes nowhere else.

- **Remove:** the same command with `-Uninstall` at the end. Pictures the app saved are untouched.
- **Update:** `git pull`, then the build script and the install script again, with the app closed.

## 8. Run

Start **Multi-pass Scanner** from the Start menu. Results are saved as `img00001.png`, `img00002.png`, … in your **Pictures** folder, wherever Windows keeps it (including a Pictures folder moved to OneDrive).

The window draws with Direct3D 12, which also works without a graphics card driver, in a virtual machine and over Remote Desktop.

The app is a window program, so a terminal does not wait for it and shows its output only if asked to. In PowerShell, add `| Out-Host`:

```powershell
# combine existing scans without a scanner
target\release\multipass-scanner.exe --combine out.png scan1.png scan2.png | Out-Host

# the window, fed from files instead of the scanner
$env:MULTIPASS_FAKE_SCANS = 'a.png,b.png'; target\release\multipass-scanner.exe

# keep the combining report of a session
target\release\multipass-scanner.exe 2> run.log
```

In the old Command Prompt, use `start /wait "" target\release\multipass-scanner.exe --combine …` instead.

## A zip for other Windows machines

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
```

It writes `target\multipass-scanner-windows-x64.zip` (about 32 MB with OpenCV 4.14): one folder with the program, OpenCV's DLL, the four Visual C++ runtime DLLs they load, and a `README.txt`. Unzipped anywhere, it runs without installing anything. Tested unpacked on the build machine only.

- **The other machine needs** Epson's scanner driver, and on a "Windows N" edition the Media Feature Pack, which OpenCV's DLL loads at start-up.
- **Not signed:** Windows may warn about an unknown publisher. Click "More info", then "Run anyway".

## Working on the code

`build.ps1` sets what the build needs only for itself. To run `cargo` directly, set the same things in your PowerShell window first, with the folder and number of your OpenCV:

```powershell
$env:LIBCLANG_PATH        = "$env:ProgramFiles\LLVM\bin"
$env:OPENCV_LINK_LIBS     = 'opencv_world4140'
$env:OPENCV_LINK_PATHS    = 'C:\opencv\build\x64\vc16\lib'
$env:OPENCV_INCLUDE_PATHS = 'C:\opencv\build\include'
$env:PATH = "C:\opencv\build\x64\vc16\bin;$env:ProgramFiles\LLVM\bin;$env:PATH"
cargo build --release
cargo test --release --workspace
```

## If something goes wrong

| What you see | What to do |
|---|---|
| `cargo` or `git` is not recognised | Close PowerShell and open it again. If still missing, repeat step 2. |
| `link.exe not found`, or an error naming `cl.exe` | The Build Tools lack the C++ part. Run the third command of step 2 again. |
| `libclang.dll not found` | Install LLVM (step 2), or set `LIBCLANG_PATH` to the folder that holds `libclang.dll`. |
| `No opencv_world*.lib under C:\opencv\build\x64` | OpenCV is not where the script looks. Redo step 3, or set `OPENCV_DIR`. |
| `LNK1181: cannot open input file 'opencv_world….lib'` | `OPENCV_LINK_LIBS` or `OPENCV_LINK_PATHS` is set to something stale in this window. Open a new PowerShell and run `build.ps1` again. |
| The program does not start: "opencv_world….dll was not found" | The DLL is not beside the `.exe`. Run `build.ps1` again; it copies it. |
| "No scanner found" | Step 1. Close Epson Scan 2 and Windows Scan. |
| The app opens the wrong device | Set `MULTIPASS_SCANNER` to part of the right one's name (step 5). |
| "The scanner driver does not offer … dpi through Windows' scanning interface" | The driver limits what it offers through Windows' scanning interface. The app has no other route to the scanner on Windows. |
| A scan fails or arrives empty | Try once with `$env:MULTIPASS_WIA_SYSTEM_STREAM = '1'` set: Windows then holds the scan in its own memory stream instead of the app's. |
