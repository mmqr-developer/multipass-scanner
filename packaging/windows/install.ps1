# Install Multi-pass Scanner for the current user.  Run from the folder that
# holds Cargo.toml, after packaging\windows\build.ps1:
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1
#     powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1 -Uninstall
#
# NOT YET RUN: written on Linux, and PowerShell scripts cannot be checked there.
#
# It copies the program and OpenCV's DLL to
#     %LOCALAPPDATA%\Programs\Multi-pass Scanner
# and puts "Multi-pass Scanner" in the Start menu.  No administrator rights
# are needed, and nothing is written outside those two places.

param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'

$dest = Join-Path $env:LOCALAPPDATA 'Programs\Multi-pass Scanner'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Multi-pass Scanner.lnk'

if ($Uninstall) {
    if (Test-Path $shortcut) { Remove-Item $shortcut -Force }
    if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
    Write-Host 'Multi-pass Scanner is removed. (Pictures it saved are untouched.)'
    exit 0
}

$source = 'target\release'
$exe = Join-Path $source 'multipass-scanner.exe'
if (-not (Test-Path $exe)) {
    throw 'target\release\multipass-scanner.exe not found. Build first: packaging\windows\build.ps1'
}

New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item $exe $dest -Force
$dlls = @(Get-ChildItem -Path $source -Filter 'opencv_world*.dll')
foreach ($d in $dlls) { Copy-Item $d.FullName $dest -Force }
if ($dlls.Count -eq 0) {
    Write-Warning "No opencv_world*.dll in ${source}. The program will only start if OpenCV's DLL is on PATH."
}

$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = Join-Path $dest 'multipass-scanner.exe'
$link.WorkingDirectory = $dest
$link.Description = 'Scan a photo several times and combine the scans into one cleaner image'
$link.Save()

Write-Host "Installed to $dest"
Write-Host 'Start it from the Start menu: Multi-pass Scanner'
