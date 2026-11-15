; blurt's Windows installer: per user, so no admin rights. It installs to
; %LOCALAPPDATA%\Programs\blurt, with a Start menu shortcut and an entry in
; Settings > Apps. scripts/windows-package.sh builds it (on Linux, with
; makensis) and defines VERSION, VERSION_QUAD (x.y.z.0), STAGE (the folder
; of files to install), ICON and OUTFILE.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION | VERSION_QUAD | STAGE | ICON | OUTFILE
  !error "Build with scripts/windows-package.sh, which defines VERSION, VERSION_QUAD, STAGE, ICON and OUTFILE"
!endif

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\blurt"

Name "blurt"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\blurt"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"

VIProductVersion "${VERSION_QUAD}"
VIAddVersionKey "ProductName" "blurt"
VIAddVersionKey "FileDescription" "blurt installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "AGPL-3.0-or-later"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\blurt.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start blurt"

!insertmacro MUI_PAGE_LICENSE "${STAGE}/LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "blurt" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${STAGE}/blurt.exe"
  File "${STAGE}/LICENSE.txt"
  File "${STAGE}/README.md"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\blurt.lnk" "$INSTDIR\blurt.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "blurt"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\blurt.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "blurt"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/haydenmc/blurt"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\blurt.lnk" "$INSTDIR\blurt.exe"
SectionEnd

; Settings, servers and your identity (%APPDATA%\blurt) stay, so a
; reinstall picks up where you left off.
Section "Uninstall"
  Delete "$INSTDIR\blurt.exe"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\blurt.lnk"
  Delete "$DESKTOP\blurt.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
