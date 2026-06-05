; Auto-included by electron-builder (build/installer.nsh).
;
; Adds the install directory to the per-user PATH on install and removes it on
; uninstall, so `hyperpanes` works from any terminal. We shell out to PowerShell's
; [Environment]::SetEnvironmentVariable(...,'User') because it dedupes, preserves
; the rest of PATH, and broadcasts the change — and crucially fails *safe*: if
; PowerShell is unavailable or errors, PATH is left untouched rather than mangled.
;
; NSIS note: PowerShell's own `$` variables are written as `$$` so NSIS emits a
; literal `$`. `$INSTDIR` / `$PLUGINSDIR` are real NSIS variables and expand.

!macro customInstall
  DetailPrint "Adding Hyperpanes to your PATH..."
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
!macroend

!macro customUnInstall
  DetailPrint "Removing Hyperpanes from your PATH..."
  FileOpen $0 "$PLUGINSDIR\hyperpanes-unpath.ps1" w
  FileWrite $0 "param([string]$$Dir)$\r$\n"
  FileWrite $0 "$$p=[Environment]::GetEnvironmentVariable('Path','User')$\r$\n"
  FileWrite $0 "if([string]::IsNullOrEmpty($$p)){ exit }$\r$\n"
  FileWrite $0 "$$items=@($$p.Split(';') | Where-Object { $$_ -ne '' -and $$_ -ne $$Dir })$\r$\n"
  FileWrite $0 "[Environment]::SetEnvironmentVariable('Path',($$items -join ';'),'User')$\r$\n"
  FileClose $0
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\hyperpanes-unpath.ps1" "$INSTDIR"'
  Pop $0
!macroend
