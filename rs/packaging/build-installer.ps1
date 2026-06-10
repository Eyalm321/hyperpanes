<#
.SYNOPSIS
  Build the per-user NSIS installer for the native Rust hyperpanes app.

.DESCRIPTION
  1. cargo build --release -p hyperpanes  (skip with -SkipBuild)
  2. Best-effort: embed build/icon.ico into the .exe via rcedit (downloaded if
     missing). This is purely packaging-side; if it fails the installer still
     builds (shortcuts + Add/Remove Programs get the icon regardless).
  3. makensis rs/packaging/installer.nsi -> rs/packaging/dist/Hyperpanes-<ver>-setup.exe

  Mirrors the Electron `npm run pack:win` step for the Rust app.

.EXAMPLE
  pwsh rs/packaging/build-installer.ps1
  pwsh rs/packaging/build-installer.ps1 -Version 0.1.0 -SkipBuild
#>
[CmdletBinding()]
param(
  # Installer version (semver). Defaults to the app crate's Cargo.toml version.
  [string]$Version,
  # Reuse an existing release build instead of running cargo.
  [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Repo root = two levels up from rs/packaging.
$PackagingDir = $PSScriptRoot
$RepoRoot     = (Resolve-Path (Join-Path $PackagingDir '..\..')).Path
$AppManifest  = Join-Path $RepoRoot 'rs\crates\app\Cargo.toml'
$IconIco      = Join-Path $RepoRoot 'build\icon.ico'
$ExePath      = Join-Path $RepoRoot 'rs\crates\app\target\release\hyperpanes.exe'
$DistDir      = Join-Path $PackagingDir 'dist'
$Nsi          = Join-Path $PackagingDir 'installer.nsi'

if (-not (Test-Path $IconIco)) { throw "Icon not found: $IconIco (run scripts/make-icon to generate it)" }

# --- version -----------------------------------------------------------------
if (-not $Version) {
  $verLine = Select-String -Path $AppManifest -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
  if (-not $verLine) { throw "Could not read version from $AppManifest" }
  $Version = $verLine.Matches[0].Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
  throw "Version must be semver x.y.z (got '$Version'); pass -Version explicitly."
}
Write-Host "==> Hyperpanes installer  version=$Version" -ForegroundColor Cyan

# --- 1. build ----------------------------------------------------------------
if ($SkipBuild) {
  Write-Host "==> -SkipBuild: reusing existing release binary" -ForegroundColor Yellow
} else {
  Write-Host "==> cargo build --release -p hyperpanes" -ForegroundColor Cyan
  & cargo build --release --manifest-path $AppManifest -p hyperpanes
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
if (-not (Test-Path $ExePath)) { throw "Release binary not found: $ExePath" }

# --- 2. embed icon into the .exe (best effort) -------------------------------
# Embedding into the bare .exe needs PE resource editing. We do it with rcedit so
# no app-source (build.rs/winres) change is required. Non-fatal if unavailable.
try {
  $rcedit = (Get-Command rcedit -ErrorAction SilentlyContinue)?.Source
  if (-not $rcedit) {
    $toolsDir = Join-Path $PackagingDir '.tools'
    $rcedit   = Join-Path $toolsDir 'rcedit-x64.exe'
    if (-not (Test-Path $rcedit)) {
      New-Item -ItemType Directory -Force -Path $toolsDir | Out-Null
      $url = 'https://github.com/electron/rcedit/releases/download/v2.0.0/rcedit-x64.exe'
      Write-Host "==> downloading rcedit -> $rcedit" -ForegroundColor Cyan
      Invoke-WebRequest -Uri $url -OutFile $rcedit -UseBasicParsing
    }
  }
  Write-Host "==> embedding icon + version info into hyperpanes.exe (rcedit)" -ForegroundColor Cyan
  & $rcedit $ExePath `
      --set-icon $IconIco `
      --set-version-string 'ProductName' 'Hyperpanes' `
      --set-version-string 'FileDescription' 'Hyperpanes' `
      --set-version-string 'CompanyName' 'Hyperpanes' `
      --set-file-version $Version `
      --set-product-version $Version
  if ($LASTEXITCODE -ne 0) { throw "rcedit exited $LASTEXITCODE" }
} catch {
  Write-Warning "Could not embed icon into the .exe ($_). Installer will still build; shortcuts + Add/Remove Programs use icon.ico. To embed into the bare .exe, install rcedit or add a build.rs/winres icon to the app crate."
}

# --- 3. makensis -------------------------------------------------------------
$makensis = (Get-Command makensis -ErrorAction SilentlyContinue)?.Source
if (-not $makensis) {
  foreach ($p in @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe")) {
    if (Test-Path $p) { $makensis = $p; break }
  }
}
if (-not $makensis) { throw "makensis not found. Install NSIS (choco install nsis -y) or add it to PATH." }

New-Item -ItemType Directory -Force -Path $DistDir | Out-Null
$OutFile = Join-Path $DistDir "Hyperpanes-$Version-setup.exe"
if (Test-Path $OutFile) { Remove-Item $OutFile -Force }

Write-Host "==> makensis -> $OutFile" -ForegroundColor Cyan
$ResourcesDir = Join-Path $RepoRoot 'resources'
foreach ($f in @('conpty\conpty.dll', 'conpty\OpenConsole.exe', 'shell-integration\hp-init.ps1', 'shell-integration\hp-init.sh')) {
  if (-not (Test-Path (Join-Path $ResourcesDir $f))) { throw "Missing packaged resource: resources\$f" }
}
& $makensis `
    "/DVERSION=$Version" `
    "/DAPP_EXE=$ExePath" `
    "/DICON=$IconIco" `
    "/DOUTFILE=$OutFile" `
    "/DRESOURCES=$ResourcesDir" `
    $Nsi
if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE)" }
if (-not (Test-Path $OutFile)) { throw "Installer was not produced: $OutFile" }

Write-Host ""
Write-Host "==> Installer ready: $OutFile" -ForegroundColor Green
Get-Item $OutFile | Select-Object Name, @{n='SizeMB';e={[math]::Round($_.Length/1MB,2)}}, FullName | Format-List
