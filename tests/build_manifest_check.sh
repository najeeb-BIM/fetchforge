#!/usr/bin/env bash
# STEP 2 check (final round, Linux-verifiable): the Windows build script
# must NOT bake a build-machine absolute path into the native-host
# manifests. It must patch to "../fetchforge-bridge.exe", which is
# relative to $INSTDIR\native-host\ and resolves to $INSTDIR\fetchforge-bridge.exe
# on any install location, for Chrome and Firefox on Windows.
set -u
cd "$(dirname "$0")/.." || exit 1
grep -q "'../fetchforge-bridge.exe'" scripts/build-windows.ps1 || { echo "FAIL: build script does not patch to relative path"; exit 1; }
grep -q "Get-Location" scripts/build-windows.ps1 && grep -q "bridgePath" scripts/build-windows.ps1 && { echo "FAIL: build script still uses build-machine path"; exit 1; }
# Simulate the PowerShell replace and verify JSON + relative resolution.
for f in installer/host-manifest-chrome.json installer/host-manifest-firefox.json; do
  python3 - "$f" <<'PY'
import json, sys, os
src = open(sys.argv[1]).read()
patched = src.replace("__BRIDGE_EXE_PATH__", "../fetchforge-bridge.exe")
d = json.loads(patched)
assert d["path"] == "../fetchforge-bridge.exe", d
# Manifest lives in $INSTDIR/native-host/, bridge in $INSTDIR/.
resolved = os.path.normpath(os.path.join("/fake/INSTDIR/native-host", d["path"]))
assert resolved == "/fake/INSTDIR/fetchforge-bridge.exe", resolved
print(f"OK {sys.argv[1]} -> {d['path']} -> {resolved}")
PY
done
# NSIS must install bridge to $INSTDIR and manifests to $INSTDIR\native-host.
grep -q 'File "stage\\fetchforge-bridge.exe"' installer/fetchforge.nsi || { echo "FAIL: NSIS does not install bridge"; exit 1; }
grep -q 'SetOutPath "$INSTDIR\\native-host"' installer/fetchforge.nsi || { echo "FAIL: NSIS native-host folder missing"; exit 1; }
echo "BUILD_MANIFEST_CHECK PASS"
