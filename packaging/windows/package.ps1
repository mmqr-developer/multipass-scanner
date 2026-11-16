# Make a zip of Multi-pass Scanner that runs on another Windows computer.
# Run from the folder that holds Cargo.toml, after build.ps1:
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
#
# The zip holds one folder, "Multi-pass Scanner", with the program, OpenCV's
# DLL, Microsoft's Visual C++ runtime DLLs (redistributable, so the other
# computer needs no Visual C++ Redistributable installed) and README.txt.
# It is written to target\multipass-scanner-windows-x64.zip.  Nothing is
# installed, and nothing outside target\ is changed.

$ErrorActionPreference = 'Stop'

if (-not (Test-Path 'Cargo.toml') -or -not (Test-Path 'crates\flatbed')) {
    throw 'Run this from the folder that holds Cargo.toml and crates.'
}

$release = 'target\release'
$exe = Join-Path $release 'multipass-scanner.exe'
if (-not (Test-Path $exe)) {
    throw "$exe not found. Build it first with packaging\windows\build.ps1."
}
$opencv = @(Get-ChildItem -Path $release -Filter 'opencv_world*.dll' | Where-Object { $_.BaseName -notmatch 'd$' })
if ($opencv.Count -ne 1) {
    throw "Expected one opencv_world*.dll in $release, found $($opencv.Count). Run build.ps1 again."
}

# The Visual C++ runtime the program was built against, from the Build Tools.
# The program and OpenCV load these four; the rest of the runtime is part of
# Windows 10 and 11.
$runtime = 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll', 'concrt140.dll'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $vswhere)) {
    throw 'vswhere.exe not found. Install the Visual Studio Build Tools with the C++ workload.'
}
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) {
    throw 'No Visual Studio with the C++ tools found. Install the Visual Studio Build Tools with the C++ workload.'
}
$crt = @(Get-ChildItem -Path (Join-Path $vs 'VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT') -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName 'vcruntime140.dll') } | Sort-Object FullName)
if ($crt.Count -eq 0) {
    throw "The Visual C++ runtime DLLs were not found under $vs\VC\Redist\MSVC."
}
$crt = $crt[$crt.Count - 1].FullName

# Put the files together in target\package\Multi-pass Scanner, then zip that folder.
$stageRoot = 'target\package'
$stage = Join-Path $stageRoot 'Multi-pass Scanner'
if (Test-Path $stageRoot) { Remove-Item -Recurse -Force $stageRoot }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe, $opencv[0].FullName, 'packaging\windows\README.txt' $stage
foreach ($dll in $runtime) {
    Copy-Item (Join-Path $crt $dll) $stage
}

$zip = 'target\multipass-scanner-windows-x64.zip'
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

Write-Host "runtime:  $crt"
Get-ChildItem $stage | ForEach-Object { Write-Host ('  {0,-26} {1,6:N1} MB' -f $_.Name, ($_.Length / 1MB)) }
Write-Host ''
Write-Host ('Made {0} ({1:N1} MB)' -f $zip, ((Get-Item $zip).Length / 1MB))
