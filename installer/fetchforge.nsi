; FetchForge installer (NSIS 3.x). Per-user install, no admin required.
; Build on Windows with: scripts/build-windows.ps1 (which stages files
; under installer/stage/ and then runs makensis on this script).
; Requires, staged under installer/stage/ at build time:
;   stage/FetchForge.exe          (Tauri app binary, from cargo tauri build)
;   stage/fetchforge-bridge.exe   (cargo build --release -p fetchforge-bridge)
;   stage/host-manifest-*.json    (placeholders patched by the build script;
;                                  the source manifests in installer/ are
;                                  NEVER overwritten with patched paths)
; This script is NOT signed. Do not claim a signed installer unless a
; code-signing certificate was actually applied and verified (see docs/BUILD.md).
!define APPNAME "FetchForge"
!define VERSION "0.1.0"
!define COMPANY "FetchForge"
!define INSTDIR_USER "$LOCALAPPDATA\Programs\FetchForge"

Name "${APPNAME} ${VERSION}"
OutFile "FetchForge-Setup-${VERSION}-x64.exe"
InstallDir "${INSTDIR_USER}"
RequestExecutionLevel user
ShowInstDetails show
ShowUninstDetails show

!include "MUI2.nsh"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File "stage\FetchForge.exe"
  File "stage\fetchforge-bridge.exe"
  ; Native-messaging host manifests, with a relative bridge path
  ; ("../fetchforge-bridge.exe", relative to this native-host folder)
  ; patched in by the build script (staged copies only, never the sources).
  ; Relative paths work on Windows for Chrome and Firefox, and stay
  ; correct whatever $INSTDIR the user chooses.
  SetOutPath "$INSTDIR\native-host"
  File "stage\host-manifest-chrome.json"
  File "stage\host-manifest-firefox.json"
  ; Register Chrome/Edge native host (per-user, HKCU).
  WriteRegStr HKCU "Software\Google\Chrome\NativeMessagingHosts\com.fetchforge.bridge" "" "$INSTDIR\native-host\host-manifest-chrome.json"
  WriteRegStr HKCU "Software\Microsoft\Edge\NativeMessagingHosts\com.fetchforge.bridge" "" "$INSTDIR\native-host\host-manifest-chrome.json"
  WriteRegStr HKCU "Software\Mozilla\NativeMessagingHosts\com.fetchforge.bridge" "" "$INSTDIR\native-host\host-manifest-firefox.json"
  ; Shortcuts.
  CreateDirectory "$SMPROGRAMS\FetchForge"
  CreateShortcut "$SMPROGRAMS\FetchForge\FetchForge.lnk" "$INSTDIR\FetchForge.exe"
  CreateShortcut "$DESKTOP\FetchForge.lnk" "$INSTDIR\FetchForge.exe"
  ; Uninstaller + Add/Remove entry.
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "DisplayName" "FetchForge ${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "UninstallString" "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "DisplayVersion" "${VERSION}"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\FetchForge.exe"
  Delete "$INSTDIR\fetchforge-bridge.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir /r "$INSTDIR\native-host"
  Delete "$SMPROGRAMS\FetchForge\FetchForge.lnk"
  Delete "$DESKTOP\FetchForge.lnk"
  RMDir "$SMPROGRAMS\FetchForge"
  DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\com.fetchforge.bridge"
  DeleteRegKey HKCU "Software\Microsoft\Edge\NativeMessagingHosts\com.fetchforge.bridge"
  DeleteRegKey HKCU "Software\Mozilla\NativeMessagingHosts\com.fetchforge.bridge"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FetchForge"
  ; User data is intentionally PRESERVED on uninstall (downloads + history
  ; in %LOCALAPPDATA%\FetchForge). The uninstaller asks below; removing
  ; data is a separate, explicit choice documented in docs/INSTALL.md.
  MessageBox MB_YESNO "Also delete FetchForge user data (download history and settings in %LOCALAPPDATA%\FetchForge)? Your downloaded files in Downloads are never deleted." IDYES deleteData IDNO keepData
  deleteData:
    RMDir /r "$LOCALAPPDATA\FetchForge"
  keepData:
  RMDir "$INSTDIR"
SectionEnd
