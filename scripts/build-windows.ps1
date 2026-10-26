# FetchForge Windows build. Run on Windows 10 22H2+ / Windows 11 x64 in
# PowerShell from the repo root. Requires: Rust (stable), Node.js 20+,
# Tauri CLI, NSIS 3.x, WebView2 Runtime (preinstalled on Win11 and on
# updated Win10). Produces: installer/fetchforge.nsi output + portable zip.
# Usage: powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..
cargo --version; node --version; npm --version
cargo build --release -p fetchforge-engine -p fetchforge-bridge
cargo test -p fetchforge-engine
# Tauri app (needs WebView2; see docs/BUILD.md if this step fails).
Push-Location apps/desktop/src-tauri
cargo tauri build
Pop-Location
# Stage installer inputs.
$stage = "installer/stage"; New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item target/release/fetchforge-bridge.exe "$stage/fetchforge-bridge.exe" -Force
$appExe = Get-ChildItem apps/desktop/src-tauri/target/release/fetchforge-desktop.exe -ErrorAction SilentlyContinue
if ($appExe) { Copy-Item $appExe.FullName "$stage/FetchForge.exe" -Force } else { Write-Warning "Tauri exe not found; copy it to $stage/FetchForge.exe manually before running makensis." }
Copy-Item installer/host-manifest-chrome.json, installer/host-manifest-firefox.json $stage -Force
# Patch bridge path placeholders in staged manifests.
$bridgePath = (Join-Path (Get-Location) "$stage/fetchforge-bridge.exe") -replace '\\', '\\'
foreach ($f in @("$stage/host-manifest-chrome.json", "$stage/host-manifest-firefox.json")) {
  (Get-Content $f -Raw) -replace '__BRIDGE_EXE_PATH__', $bridgePath | Set-Content $f
}
Copy-Item "$stage/fetchforge-bridge.exe", "$stage/FetchForge.exe" installer/ -ErrorAction SilentlyContinue
Copy-Item "$stage/host-manifest-chrome.json", "$stage/host-manifest-firefox.json" installer/ -Force
makensis installer/fetchforge.nsi
# Portable build (no installer, no registry): app + bridge + readme.
$port = "dist/FetchForge-portable"; New-Item -ItemType Directory -Force $port | Out-Null
Copy-Item "$stage/FetchForge.exe", "$stage/fetchforge-bridge.exe" $port -Force
Copy-Item README.md $port -Force
Compress-Archive -Path "$port/*" -DestinationPath "dist/FetchForge-portable-x64.zip" -Force
Write-Host "Done. Installer: installer/FetchForge-Setup-0.1.0-x64.exe ; Portable: dist/FetchForge-portable-x64.zip"
