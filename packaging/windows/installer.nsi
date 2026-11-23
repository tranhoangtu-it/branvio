!define APPNAME "Branvio"
!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\..\dist\branvio-setup-x64.exe"
!endif

Name "${APPNAME} ${VERSION}"
OutFile "${OUTFILE}"
; App icon (regenerate with scripts/build_app_icons.py).
Icon "branvio.ico"
UninstallIcon "branvio.ico"
InstallDir "$LOCALAPPDATA\Programs\Branvio"
RequestExecutionLevel user

Section "Install"
  SetOutPath "$INSTDIR"
  File "/oname=branvio.exe" "..\..\target\release\app-ui.exe"
  ; License texts + third-party attributions must ship with every binary distribution.
  ; Paths are relative to this script (repository root files).
  File "..\..\LICENSE-MIT"
  File "..\..\LICENSE-APACHE"
  File "..\..\THIRD_PARTY_NOTICES.md"
  File "..\..\THIRD_PARTY_LICENSES.md"
  File "branvio.ico"
  CreateDirectory "$SMPROGRAMS\Branvio"
  CreateShortCut "$SMPROGRAMS\Branvio\Branvio.lnk" "$INSTDIR\branvio.exe" "" "$INSTDIR\branvio.ico" 0
  WriteUninstaller "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\branvio.exe"
  Delete "$INSTDIR\branvio.ico"
  Delete "$INSTDIR\LICENSE-MIT"
  Delete "$INSTDIR\LICENSE-APACHE"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
  Delete "$INSTDIR\THIRD_PARTY_LICENSES.md"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\Branvio\Branvio.lnk"
  RMDir "$SMPROGRAMS\Branvio"
  RMDir "$INSTDIR"
SectionEnd
