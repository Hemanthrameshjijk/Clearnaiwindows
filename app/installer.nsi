; ClearNAI Windows installer (NSIS script).
;
; Builds a proper Windows installer instead of the earlier raw-exe +
; uninstall.ps1 approach: a per-user install (no admin elevation needed),
; Start Menu + Desktop shortcuts, and a real entry in Windows'
; "Apps & Features" / "Add or Remove Programs" backed by an
; NSIS-auto-generated Uninstall.exe.
;
; Deliberately installs to the SAME directory the app already uses for its
; self-extracted BVC assets (%LOCALAPPDATA%\ClearNAI\weya_nc.dll, the model
; tar.gz, settings.json, clearnai.log - see app/src/setup.rs::app_data_dir)
; rather than Program Files: this app never needs admin rights for anything
; else it does, so an admin-requiring install would be an unnecessary extra
; UAC prompt, and installing to the very directory the app already treats
; as "its own" folder means uninstalling can just remove that one directory
; and be done, with no separate "app files" vs "user data" split to keep in
; sync.
;
; Built by CI via `makensis installer.nsi` (see .github/workflows/windows.yml)
; from the repo root's `app` directory, after `cargo build --release` has
; already produced target\release\clearnairt.exe two directories up.

!define APP_NAME "ClearNAI"
!define APP_EXE "clearnairt.exe"
!define APP_PUBLISHER "ClearNAI"
!define APP_VERSION "0.1.0"
!define UNINSTALL_REG_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_NAME}"

Name "${APP_NAME}"
OutFile "..\target\release\ClearNAI-Setup.exe"
InstallDir "$LOCALAPPDATA\ClearNAI"
; Per-user install: no admin/UAC prompt required, matching this app's own
; behavior (it already only ever writes under the current user's
; %LOCALAPPDATA%, never Program Files or HKLM).
RequestExecutionLevel user

Page directory
Page instfiles

UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  ; Built by the `cargo build --release` step that runs immediately before
  ; this one in CI (see .github/workflows/windows.yml).
  File "..\target\release\${APP_EXE}"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk" "$INSTDIR\Uninstall.exe"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Per-user uninstall registry entry (HKCU, not HKLM - matches
  ; RequestExecutionLevel user above) so ClearNAI shows up in Windows'
  ; "Apps & Features" / "Add or Remove Programs" with a working Uninstall
  ; button, the same as any normally-installed app.
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "${UNINSTALL_REG_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${UNINSTALL_REG_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_REG_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  ; $INSTDIR is also where the running app self-extracts weya_nc.dll, the
  ; ONNX model bundle, settings.json, and clearnai.log (see
  ; app/src/setup.rs::app_data_dir) - removing the whole directory cleans
  ; all of that up in one step, matching what the earlier uninstall.ps1
  ; script did explicitly.
  ;
  ; Deliberately does NOT touch any VB-Audio Virtual Cable / VoiceMeeter
  ; driver install - those are separate third-party products the user
  ; installed themselves (see docs/VIRTUAL_DEVICES.md) and may still be
  ; needed by other apps.
  RMDir /r "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  DeleteRegKey HKCU "${UNINSTALL_REG_KEY}"
SectionEnd
