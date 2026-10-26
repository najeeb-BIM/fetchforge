#!/usr/bin/env bash
# P07 check: the Windows build script must use `cargo tauri build --no-bundle`
# so Tauri's own NSIS bundle (which lacks the bridge binary) cannot fail the
# build before the custom NSIS installer is produced. The workflow's
# artifact list must stay exactly: custom installer + portable zip.
set -u
cd "$(dirname "$0")/.." || exit 1
grep -q "cargo tauri build --no-bundle" scripts/build-windows.ps1 || { echo "FAIL: build script does not use --no-bundle"; exit 1; }
grep -q "FetchForge-Setup-0.1.0-x64.exe" .github/workflows/windows-build.yml || { echo "FAIL: workflow installer artifact missing"; exit 1; }
grep -q "FetchForge-portable-x64.zip" .github/workflows/windows-build.yml || { echo "FAIL: workflow portable artifact missing"; exit 1; }
echo "BUILD_NO_BUNDLE_CHECK PASS"
