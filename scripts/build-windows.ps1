# FetchForge Windows build. Run on Windows 10 22H2+ / Windows 11 x64 in
# PowerShell from the repo root. Requires: Rust (stable), Node.js 20+,
# Tauri CLI (cargo install tauri-cli --locked), NSIS 3.x, WebView2 Runtime
# (preinstalled on Win11 and on updated Win10).
# Produces: installer/FetchForge-Setup-0.2.0-x64.exe
#           dist/FetchForge-portable-x64.zip
#           installer/stage/FetchForge.exe, installer/stage/fetchforge-bridge.exe,
#           installer/stage/yt-dlp.exe
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
# P07 (final round): --no-bundle skips Tauri's own bundling step even
# though tauri.conf.json has bundle.active=true. Tauri's own NSIS bundle
# would NOT contain the bridge binary, and a bundle failure would fail
# this script before the custom NSIS installer (the real deliverable,
# which does include the bridge) is produced. Only the custom installer
# and the portable zip are uploaded by the workflow; Tauri's bundle is
# never used. NOT VERIFIED until a Windows build runs.
Push-Location apps/desktop/src-tauri
cargo tauri build --no-bundle
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
# STEP 2 FIX (final round): previously this baked in the BUILD machine's
# absolute stage path, which does not exist on a user's machine, so
# native messaging would fail for anyone installing from the GitHub
# artifact. On Windows, Chrome and Firefox both allow the manifest
# path to be relative to the manifest file itself (Chrome docs:
# developer.chrome.com/docs/extensions/develop/concepts/native-messaging;
# Firefox: path may be relative to the manifest on Windows).
# The installer puts manifests in $INSTDIR\native-host\ and the bridge
# in $INSTDIR\, so "../fetchforge-bridge.exe" is correct on every
# install location, with no build-machine path and no JSON escaping issue.
foreach ($f in @("$stage/host-manifest-chrome.json", "$stage/host-manifest-firefox.json")) {
  (Get-Content $f -Raw) -replace '__BRIDGE_EXE_PATH__', '../fetchforge-bridge.exe' | Set-Content $f
}
# yt-dlp (bundled, v0.2.0): the media-extraction engine. Download the
# PINNED release and verify its SHA-256 before staging, so a changed or
# compromised release can never be baked into the installer. Pinned
# yt-dlp 2026.10.26, 23,575,744 bytes; the engine pins the same version
# and hashes (crates/engine/src/extract.rs) — bump both together.
# ffmpeg is NOT bundled (its exe alone is ~105 MB); it is downloaded on
# first merge, SHA-256-verified, by the engine itself.
$ytDlpVersion = "2026.10.26"
$ytDlpSha256 = "F5389D3698ECDE4EF06F4E6B1CE0294FE9AC1396228C8990B8D3416D6D177A7D"
$ytDlpUrl = "https://github.com/yt-dlp/yt-dlp/releases/download/$ytDlpVersion/yt-dlp.exe"
Invoke-WebRequest -Uri $ytDlpUrl -OutFile "$stage/yt-dlp.exe"
$ytDlpActual = (Get-FileHash -Algorithm SHA256 "$stage/yt-dlp.exe").Hash
if ($ytDlpActual -ne $ytDlpSha256) { throw "yt-dlp.exe SHA-256 mismatch: expected $ytDlpSha256, got $ytDlpActual" }
# Run makensis from inside installer/ so that File "stage\..." paths and
# the OutFile both resolve under installer/ regardless of NSIS's cwd rules.
Push-Location installer
makensis fetchforge.nsi
if ($LASTEXITCODE -ne 0) { throw "NSIS build failed" }
Pop-Location
if (-not (Test-Path "installer/FetchForge-Setup-0.2.0-x64.exe")) { throw "NSIS output not found at installer/FetchForge-Setup-0.2.0-x64.exe" }
# Portable build (no installer, no registry): app + bridge + yt-dlp + readme.
$port = "dist/FetchForge-portable"; New-Item -ItemType Directory -Force $port | Out-Null
Copy-Item "$stage/FetchForge.exe", "$stage/fetchforge-bridge.exe", "$stage/yt-dlp.exe" $port -Force
Copy-Item README.md $port -Force
Compress-Archive -Path "$port/*" -DestinationPath "dist/FetchForge-portable-x64.zip" -Force
Write-Host "Done. Installer: installer/FetchForge-Setup-0.2.0-x64.exe ; Portable: dist/FetchForge-portable-x64.zip"
