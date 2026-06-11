; Hyperpanes — per-user Windows installer for the native Rust app (binary: hyperpanes.exe).
;
; This is the Rust equivalent of the Electron `electron-builder` NSIS setup
; (electron-builder.yml) + `build/installer.nsh`. Mirrored behaviour:
;   - oneClick: false                       -> assisted MUI2 installer (Welcome/Dir/Install/Finish)
;   - perMachine: false                     -> per-user install, no elevation (HKCU, %LOCALAPPDATA%)
;   - allowToChangeInstallationDirectory    -> a Directory page
;   - artifactName Hyperpanes-<ver>-setup   -> OutFile passed in via /DOUTFILE
;   - build/installer.nsh PATH integration  -> AddToUserPath / RemoveFromUserPath below (verbatim port)
;
; Build-time inputs (passed by rs/packaging/build-installer.ps1 via makensis /D...):
;   VERSION   semver, e.g. 0.1.0           (defaults to 0.0.0)
;   APP_EXE   absolute path to the release hyperpanes.exe   (required)
;   ICON      absolute path to build/icon.ico               (required)
;   OUTFILE   absolute path of the installer to produce      (required)

Unicode true

!include "MUI2.nsh"
!include "FileFunc.nsh"

; ----- Identity (mirrors electron-builder.yml) -------------------------------
!define PRODUCT_NAME "Hyperpanes"
!define APP_ID       "com.hyperpanes.app"
!define PUBLISHER    "Hyperpanes"
!define MAIN_BINARY  "hyperpanes.exe"
!define UNINST_KEY   "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

; .hyperpanes workspace file association (per-user: HKCU\Software\Classes == per-user HKCR)
!define WS_EXT    ".hyperpanes"
!define WS_PROGID "Hyperpanes.Workspace"

; ----- Build-time inputs -----------------------------------------------------
!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef APP_EXE
  !error "APP_EXE must be defined: makensis /DAPP_EXE=<path to release hyperpanes.exe>"
!endif
!ifndef ICON
  !error "ICON must be defined: makensis /DICON=<path to icon.ico>"
!endif
!ifndef OUTFILE
  !define OUTFILE "Hyperpanes-${VERSION}-setup.exe"
!endif
; Repo `resources/` dir (conpty redistributable pair + shell-integration scripts).
; build-installer.ps1 passes it absolute; the default resolves relative to this .nsi.
!ifndef RESOURCES
  !define RESOURCES "..\..\resources"
!endif

Name "${PRODUCT_NAME}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${PRODUCT_NAME}"
InstallDirRegKey HKCU "Software\${PRODUCT_NAME}" "InstallLocation"
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey  "ProductName"   "${PRODUCT_NAME}"
VIAddVersionKey  "FileDescription" "${PRODUCT_NAME} installer"
VIAddVersionKey  "FileVersion"   "${VERSION}.0"
VIAddVersionKey  "ProductVersion" "${VERSION}"
VIAddVersionKey  "CompanyName"   "${PUBLISHER}"
VIAddVersionKey  "LegalCopyright" "Copyright (C) 2026"

; ----- UI --------------------------------------------------------------------
!define MUI_ICON   "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${MAIN_BINARY}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Function .onInit
  InitPluginsDir          ; PATH helpers drop a .ps1 into $PLUGINSDIR
FunctionEnd

Function un.onInit
  InitPluginsDir
FunctionEnd

; ----- Install ---------------------------------------------------------------
Section "Install"
  SetOutPath "$INSTDIR"
  File "/oname=${MAIN_BINARY}" "${APP_EXE}"
  ; Ship the icon so shortcuts + Add/Remove Programs render it without requiring
  ; the icon to be embedded in the bare .exe (that needs a build.rs/winres change).
  File "/oname=icon.ico" "${ICON}"

  ; Bundled ConPTY redistributable pair (resources/conpty/README.md): portable-pty
  ; prefers a conpty.dll next to the exe; the 1.24 host removes the in-box conhost's
  ; scroll-region/passthrough bottlenecks (6-44x measured). MUST ship as a matched pair.
  File "${RESOURCES}\conpty\conpty.dll"
  File "${RESOURCES}\conpty\OpenConsole.exe"

  ; Shell-integration init scripts (cwd OSC -> project tint / clickable paths). The app
  ; resolves exe_dir\resources\shell-integration; build.rs deploys the same for dev.
  SetOutPath "$INSTDIR\resources\shell-integration"
  File "${RESOURCES}\shell-integration\hp-init.ps1"
  File "${RESOURCES}\shell-integration\hp-init.sh"
  SetOutPath "$INSTDIR\resources\shell-integration\zdotdir"
  File "${RESOURCES}\shell-integration\zdotdir\.zshenv"
  File "${RESOURCES}\shell-integration\zdotdir\.zshrc"
  SetOutPath "$INSTDIR"

  CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}.lnk" "$INSTDIR\${MAIN_BINARY}" "" "$INSTDIR\icon.ico" 0
  CreateShortcut "$DESKTOP\${PRODUCT_NAME}.lnk"    "$INSTDIR\${MAIN_BINARY}" "" "$INSTDIR\icon.ico" 0

  WriteUninstaller "$INSTDIR\Uninstall ${PRODUCT_NAME}.exe"
  WriteRegStr HKCU "Software\${PRODUCT_NAME}" "InstallLocation" "$INSTDIR"

  ; Add/Remove Programs entry (per-user -> HKCU)
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayName"     "${PRODUCT_NAME}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${UNINST_KEY}" "Publisher"       "${PUBLISHER}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayIcon"     "$INSTDIR\icon.ico"
  WriteRegStr   HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKCU "${UNINST_KEY}" "UninstallString"      "$\"$INSTDIR\Uninstall ${PRODUCT_NAME}.exe$\""
  WriteRegStr   HKCU "${UNINST_KEY}" "QuietUninstallString" "$\"$INSTDIR\Uninstall ${PRODUCT_NAME}.exe$\" /S"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"

  ; .hyperpanes file association: double-clicking a workspace opens it in the app
  ; ("%1" arrives as argv[1] and flows through the CLI's positional-path capture).
  WriteRegStr HKCU "Software\Classes\${WS_EXT}"    "" "${WS_PROGID}"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}" "" "Hyperpanes Workspace"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}\DefaultIcon"        "" "$INSTDIR\icon.ico,0"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}\shell\open\command" "" '"$INSTDIR\${MAIN_BINARY}" "%1"'
  ; Tell the shell the association table changed so the icon/verb apply immediately.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'  ; SHCNE_ASSOCCHANGED

  Call AddToUserPath
SectionEnd

; ----- Uninstall -------------------------------------------------------------
Section "Uninstall"
  Call un.RemoveFromUserPath

  Delete "$SMPROGRAMS\${PRODUCT_NAME}.lnk"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"

  Delete "$INSTDIR\${MAIN_BINARY}"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\conpty.dll"
  Delete "$INSTDIR\OpenConsole.exe"
  Delete "$INSTDIR\resources\shell-integration\hp-init.ps1"
  Delete "$INSTDIR\resources\shell-integration\hp-init.sh"
  Delete "$INSTDIR\resources\shell-integration\zdotdir\.zshenv"
  Delete "$INSTDIR\resources\shell-integration\zdotdir\.zshrc"
  RMDir  "$INSTDIR\resources\shell-integration\zdotdir"
  RMDir  "$INSTDIR\resources\shell-integration"
  RMDir  "$INSTDIR\resources"
  Delete "$INSTDIR\Uninstall ${PRODUCT_NAME}.exe"
  RMDir  "$INSTDIR"

  ; Mirror the .hyperpanes association cleanup (leave the extension key alone if
  ; another app has since claimed it).
  DeleteRegKey   HKCU "Software\Classes\${WS_PROGID}"
  ReadRegStr $0 HKCU "Software\Classes\${WS_EXT}" ""
  StrCmp $0 "${WS_PROGID}" 0 +2
    DeleteRegValue HKCU "Software\Classes\${WS_EXT}" ""
  DeleteRegKey /ifempty HKCU "Software\Classes\${WS_EXT}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'

  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\${PRODUCT_NAME}"
SectionEnd

; ----- PATH integration ------------------------------------------------------
; Ported verbatim from build/installer.nsh. We shell out to PowerShell's
; [Environment]::SetEnvironmentVariable(...,'User') because it dedupes, preserves
; the rest of PATH, broadcasts the change, and fails *safe*: if PowerShell is
; unavailable or errors, PATH is left untouched rather than mangled.
;
; NSIS note: PowerShell's own `$` variables are written as `$$` so NSIS emits a
; literal `$`. `$INSTDIR` / `$PLUGINSDIR` are real NSIS variables and expand.

Function AddToUserPath
  DetailPrint "Adding ${PRODUCT_NAME} to your PATH..."
  FileOpen $0 "$PLUGINSDIR\hyperpanes-path.ps1" w
  FileWrite $0 "param([string]$$Dir)$\r$\n"
  FileWrite $0 "$$p=[Environment]::GetEnvironmentVariable('Path','User')$\r$\n"
  FileWrite $0 "if([string]::IsNullOrEmpty($$p)){ $$p='' }$\r$\n"
  FileWrite $0 "$$items=@($$p.Split(';') | Where-Object { $$_ -ne '' -and $$_ -ne $$Dir })$\r$\n"
  FileWrite $0 "$$items+=$$Dir$\r$\n"
  FileWrite $0 "[Environment]::SetEnvironmentVariable('Path',($$items -join ';'),'User')$\r$\n"
  FileClose $0
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\hyperpanes-path.ps1" "$INSTDIR"'
  Pop $0
FunctionEnd

Function un.RemoveFromUserPath
  DetailPrint "Removing ${PRODUCT_NAME} from your PATH..."
  FileOpen $0 "$PLUGINSDIR\hyperpanes-unpath.ps1" w
  FileWrite $0 "param([string]$$Dir)$\r$\n"
  FileWrite $0 "$$p=[Environment]::GetEnvironmentVariable('Path','User')$\r$\n"
  FileWrite $0 "if([string]::IsNullOrEmpty($$p)){ exit }$\r$\n"
  FileWrite $0 "$$items=@($$p.Split(';') | Where-Object { $$_ -ne '' -and $$_ -ne $$Dir })$\r$\n"
  FileWrite $0 "[Environment]::SetEnvironmentVariable('Path',($$items -join ';'),'User')$\r$\n"
  FileClose $0
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\hyperpanes-unpath.ps1" "$INSTDIR"'
  Pop $0
FunctionEnd
