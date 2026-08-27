# install.ps1 — installs opencc (wrapper + proxy) on Windows.
# Linux/macOS: use install.sh.
#
# Binary source, in order of preference:
#   1. a freshly built cross binary:  <script-dir>\target\<triple>\release\
#   2. a freshly built host binary:   <script-dir>\target\release\
#   3. a prebuilt binary copied next to this script
#   4. the latest release from GitHub (sha256-verified)
#
# The local binaries are only used if they actually run on this machine
# (compatibility check via `opencc --version`).
#
# Usage (from a PowerShell prompt in the repository directory):
#   .\install.ps1
# or, with the execution policy:
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#
# Environment overrides:
#   OPENCC_REPO            GitHub repo (default: kokiddp/opencc)
#   OPENCC_DOWNLOAD_BASE   base URL for the downloads (tests/mirrors)
#   OPENCC_INSTALL_DIR     install directory (default: ~\.opencc)
#   OPENCC_BIN_DIR         bin directory (default: ~\.local\bin)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$InstallDir = if ($env:OPENCC_INSTALL_DIR) { $env:OPENCC_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.opencc' }
$BinDir = if ($env:OPENCC_BIN_DIR) { $env:OPENCC_BIN_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }

function Log  { Write-Host $args }
function Die  { Write-Host "Error: $args" -ForegroundColor Red; exit 1 }

# --- 0) Detect architecture → release asset name -------------------------------
# 32-bit PowerShell on a 64-bit OS reports x86 in PROCESSOR_ARCHITECTURE but
# the WOW64 variable carries the real architecture.
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'x86_64' }
  'x86' {
    if ($env:PROCESSOR_ARCHITEW6432 -eq 'AMD64') { 'x86_64' } else { 'i686' }
  }
  default { Die "unsupported architecture '$($env:PROCESSOR_ARCHITECTURE)' (supported: x86_64, i686) — build from source (cargo build --release)." }
}

$Triple = switch ($Arch) {
  'x86_64' { 'x86_64-pc-windows-gnu' }
  'i686'   { 'i686-pc-windows-gnu' }
}
Log "Detected: windows/$Arch (release asset: $Triple)"

# --- 1) Claude Code ------------------------------------------------------------
$Claude = Get-Command claude -ErrorAction SilentlyContinue
if ($Claude) {
  Log "Claude Code found: $($Claude.Source)"
} else {
  Log "Claude Code is not installed (required by opencc)."
  $Ans = Read-Host "Install it now via npm? [y/N]"
  if ($Ans -match '^[yY]') {
    if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
      Die "npm not found. Install Claude Code (https://claude.com/download) and re-run install.ps1."
    }
    npm install -g @anthropic-ai/claude-code
    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
      Die "Claude Code not found after installation. Re-run install.ps1."
    }
  } else {
    Die "Install Claude Code (https://claude.com/download) and re-run install.ps1."
  }
}

# --- 2) Pick the source of the binaries ----------------------------------------
$SrcDir = $null

# 2a) Local build directories, newest first. Compatibility check: the binary
# must actually run on this machine (`opencc --version`).
$BestDir = $null
$BestTime = [DateTime]::MinValue
foreach ($d in @(
    (Join-Path $ScriptDir "target\$Triple\release"),
    (Join-Path $ScriptDir 'target\release'),
    $ScriptDir
  )) {
  if ((Test-Path (Join-Path $d 'opencc.exe')) -and (Test-Path (Join-Path $d 'opencc-proxy.exe'))) {
    $t = (Get-Item (Join-Path $d 'opencc.exe')).LastWriteTime
    if ($t -gt $BestTime) { $BestDir = $d; $BestTime = $t }
  }
}
if ($BestDir) {
  $ok = $false
  try {
    $null = & (Join-Path $BestDir 'opencc.exe') --version 2>$null
    if ($LASTEXITCODE -eq 0) { $ok = $true }
  } catch { $ok = $false }
  if ($ok) {
    $SrcDir = $BestDir
    Log "Using locally built binaries: $SrcDir"
  }
}

# 2b) GitHub latest release.
if (-not $SrcDir) {
  $Repo = if ($env:OPENCC_REPO) { $env:OPENCC_REPO } else { 'kokiddp/opencc' }
  $Base = if ($env:OPENCC_DOWNLOAD_BASE) { $env:OPENCC_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/latest/download" }
  Log "No usable local binaries: downloading from $Base ..."

  if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue)) {
    Die "curl.exe not found (Windows 10+ ships it) — build from source (cargo build --release)."
  }
  $Tmp = Join-Path ([System.IO.Path]::GetTempPath()) "opencc-install-$PID"
  New-Item -ItemType Directory -Path $Tmp | Out-Null
  $downloaded = $false
  try {
    foreach ($asset in @("opencc-$Triple.exe", "opencc-proxy-$Triple.exe", 'sha256sums.txt')) {
      curl.exe -fsSL -o (Join-Path $Tmp $asset) "$Base/$asset"
      if ($LASTEXITCODE -ne 0) {
        Die "cannot download $Base/$asset — is the release published? (drafts are not downloadable)"
      }
    }
    # Verify the checksums.
    $sums = Get-Content (Join-Path $Tmp 'sha256sums.txt')
    foreach ($name in @("opencc-$Triple.exe", "opencc-proxy-$Triple.exe")) {
      $expected = ($sums | Where-Object { $_ -match [regex]::Escape($name) } | ForEach-Object { ($_ -split '\s+')[0] }) -join ''
      if (-not $expected) { Die "no checksum found for $name" }
      $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $name)).Hash.ToLower()
      if ($actual -ne $expected.ToLower()) { Die "checksum verification failed for $name" }
    }
    # Rename to the plain names the install step expects.
    Rename-Item (Join-Path $Tmp "opencc-$Triple.exe") 'opencc.exe'
    Rename-Item (Join-Path $Tmp "opencc-proxy-$Triple.exe") 'opencc-proxy.exe'
    $SrcDir = $Tmp
    $downloaded = $true
  } finally {
    # On failure, drop the partial download; on success it is kept until the
    # install step (cleaned at the end of the script).
    if (-not $downloaded) { Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue }
  }
}

# --- 3) Install ----------------------------------------------------------------
# On Windows we copy instead of symlinking (symlinks need elevation/dev mode).
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
Copy-Item -Force (Join-Path $SrcDir 'opencc.exe') (Join-Path $InstallDir 'opencc.exe')
Copy-Item -Force (Join-Path $SrcDir 'opencc-proxy.exe') (Join-Path $InstallDir 'opencc-proxy.exe')
Copy-Item -Force (Join-Path $InstallDir 'opencc.exe') (Join-Path $BinDir 'opencc.exe')
Copy-Item -Force (Join-Path $InstallDir 'opencc-proxy.exe') (Join-Path $BinDir 'opencc-proxy.exe')

Log "opencc installed:"
Log "  $InstallDir\opencc.exe        (+ opencc-proxy.exe)"
Log "  $BinDir\opencc.exe        (+ opencc-proxy.exe)"
Log "Version: $(& (Join-Path $BinDir 'opencc.exe') --version)"

# Add ~\.local\bin to the user PATH if missing.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $UserPath -or ($UserPath -notlike "*$BinDir*")) {
  $NewPath = if ($UserPath) { "$UserPath;$BinDir" } else { $BinDir }
  [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
  Log "Added $BinDir to your user PATH (new terminals only)."
}

# Clean up the download directory once the install is done.
if (Test-Path $Tmp) { Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue }
