# Build Multi-pass Scanner on Windows.  Run from the folder that holds Cargo.toml:
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1
#     powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1 -Test
#
# -Test also runs the unit tests after the build.
#
# Needs Rust, the Visual Studio Build Tools (C++), LLVM and OpenCV's Windows
# package.
#
# The script finds LLVM and OpenCV in their usual places, tells the build
# where they are, builds the release program and puts OpenCV's DLL beside it.
# Nothing it sets outlasts the script.  To use other places, set OPENCV_DIR
# (the folder that holds "build"), or set LIBCLANG_PATH, OPENCV_LINK_LIBS,
# OPENCV_LINK_PATHS and OPENCV_INCLUDE_PATHS yourself.

param([switch]$Test)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path 'Cargo.toml') -or -not (Test-Path 'crates\flatbed')) {
    throw 'Run this from the folder that holds Cargo.toml and crates.'
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found. Install Rust (winget install Rustlang.Rustup), then open a new terminal.'
}

# libclang: the OpenCV bindings are generated from OpenCV's C++ headers.
if (-not $env:LIBCLANG_PATH) {
    $llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
    if (-not (Test-Path (Join-Path $llvm 'libclang.dll'))) {
        throw "libclang.dll not found in $llvm. Install LLVM (winget install LLVM.LLVM) or set LIBCLANG_PATH."
    }
    $env:LIBCLANG_PATH = $llvm
}
$env:PATH = "$env:LIBCLANG_PATH;$env:PATH"

# OpenCV: the release library opencv_world<version>.lib of the official package.
$dll = $null
if (-not $env:OPENCV_LINK_LIBS) {
    $root = 'C:\opencv'
    if ($env:OPENCV_DIR) { $root = $env:OPENCV_DIR }
    $libs = @(Get-ChildItem -Path (Join-Path $root 'build\x64') -Recurse -Filter 'opencv_world*.lib' -ErrorAction SilentlyContinue |
        Where-Object { $_.BaseName -notmatch 'd$' } | Sort-Object FullName)
    if ($libs.Count -eq 0) {
        throw "No opencv_world*.lib under $root\build\x64. Unpack OpenCV's Windows package to C:\ or set OPENCV_DIR to the folder that holds 'build'."
    }
    $lib = $libs[$libs.Count - 1]
    $env:OPENCV_LINK_LIBS = $lib.BaseName
    $env:OPENCV_LINK_PATHS = $lib.DirectoryName
    $env:OPENCV_INCLUDE_PATHS = Join-Path $root 'build\include'
    $bin = Join-Path (Split-Path $lib.DirectoryName -Parent) 'bin'
    $dll = Join-Path $bin ($lib.BaseName + '.dll')
    # for programs cargo runs from other folders (the tests)
    $env:PATH = "$bin;$env:PATH"
}
Write-Host "libclang: $env:LIBCLANG_PATH"
Write-Host "OpenCV:   $env:OPENCV_LINK_LIBS in $env:OPENCV_LINK_PATHS"

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'The build failed (see above).' }

if ($Test) {
    cargo test --release --workspace
    if ($LASTEXITCODE -ne 0) { throw 'Tests failed (see above).' }
}

# The program needs OpenCV's DLL where Windows finds it: beside the .exe.
if ($dll) {
    if (Test-Path $dll) {
        Copy-Item $dll 'target\release' -Force
        Write-Host "copied $dll"
    } else {
        Write-Warning "$dll not found. The program will only start if OpenCV's DLL is on PATH."
    }
}
Write-Host ''
Write-Host 'Built target\release\multipass-scanner.exe'
Write-Host 'To install it: powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1'
