# FetchForge Windows build. Run on Windows 10 22H2+ / Windows 11 x64 in
# PowerShell from the repo root. Requires: Rust (stable), Node.js 20+,
# Tauri CLI (cargo install tauri-cli --locked), NSIS 3.x, WebView2 Runtime
# (preinstalled on Win11 and on updated Win10).
# Produces: installer/FetchForge-Setup-0.1.0-x64.exe
#           dist/FetchForge-portable-x64.zip
#           installer/stage/FetchForge.exe, installer/stage/fetchforge-bridge.exe
# Usage: powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..
if (-not (Get-Command cargo-tauri -ErrorAction SilentlyContinue)) {
  throw "Tauri CLI not found. Install it first with: cargo install tauri-cli --locked"
}
cargo --version; node --version; npm --version
cargo build --release -p fetchforge-engine -p fetchforge-bridge
cargo test
# Tauri app (needs WebView2; see docs/BUILD.md if this step fails).
Push-Location apps/desktop/src-tauri
cargo tauri build
if ($LASTEXITCODE -ne 0) { throw "Tauri build failed" }
Pop-Location
# Stage installer inputs (staged copies only; source manifests are never overwritten).
$stage = "installer/stage"; New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item target/release/fetchforge-bridge.exe "$stage/fetchforge-bridge.exe" -Force
$appExe = Get-ChildItem apps/desktop/src-tauri/target/release/fetchforge-desktop.exe -ErrorAction SilentlyContinue
if (-not $appExe) { throw "Tauri exe not found at apps/desktop/src-tauri/target/release/fetchforge-desktop.exe" }
Copy-Item $appExe.FullName "$stage/FetchForge.exe" -Force
Copy-Item installer/host-manifest-chrome.json, installer/host-manifest-firefox.json $stage -Force
# Patch bridge path placeholders in the STAGED manifests only.
$bridgePath = (Join-Path (Get-Location) "$stage/fetchforge-bridge.exe") -replace '\\', '\\'
foreach ($f in @("$stage/host-manifest-chrome.json", "$stage/host-manifest-firefox.json")) {
  (Get-Content $f -Raw) -replace '__BRIDGE_EXE_PATH__', $bridgePath | Set-Content $f
}
# Run makensis from inside installer/ so that File "stage\..." paths and
# the OutFile both resolve under installer/ regardless of NSIS's cwd rules.
Push-Location installer
makensis fetchforge.nsi
if ($LASTEXITCODE -ne 0) { throw "NSIS build failed" }
Pop-Location
if (-not (Test-Path "installer/FetchForge-Setup-0.1.0-x64.exe")) { throw "NSIS output not found at installer/FetchForge-Setup-0.1.0-x64.exe" }
# Portable build (no installer, no registry): app + bridge + readme.
$port = "dist/FetchForge-portable"; New-Item -ItemType Directory -Force $port | Out-Null
Copy-Item "$stage/FetchForge.exe", "$stage/fetchforge-bridge.exe" $port -Force
Copy-Item README.md $port -Force
Compress-Archive -Path "$port/*" -DestinationPath "dist/FetchForge-portable-x64.zip" -Force
Write-Host "Done. Installer: installer/FetchForge-Setup-0.1.0-x64.exe ; Portable: dist/FetchForge-portable-x64.zip"
