#Requires -Version 5.1
<#
install.ps1 — install harbor + pilot with one command (Windows):

    irm https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.ps1 | iex

Pin a version:

    & ([scriptblock]::Create((irm .../install.ps1))) -Tag v0.15.0

Downloads the release zip for this architecture, verifies its SHA-256 against
the published checksums, and installs into %LOCALAPPDATA%\Programs\harbor —
the per-user convention on Windows, so nothing here needs Administrator.

duckdb.dll sits beside the executables, which is where Windows looks first.

Harbor's own directories follow the same split as everywhere else:
config at %USERPROFILE%\.config\harbor, state at %LOCALAPPDATA%\harbor.
#>
[CmdletBinding()]
param(
  [string]$Tag,
  [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\harbor')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# PowerShell 5.1 can still default to TLS 1.0, which github.com refuses.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo = 'shreeve/duckdb-harbor'
function Fail($msg) { Write-Error "install: $msg"; exit 1 }

# --- platform -> release asset suffix ---------------------------------------
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'amd64' }
  'ARM64' { 'arm64' }
  default { Fail "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}
$plat = "windows-$arch"

# --- version: the argument, or whatever `latest` resolves to -----------------
if (-not $Tag) {
  try {
    $Tag = (Invoke-RestMethod -UseBasicParsing -Headers @{ 'User-Agent' = 'harbor-install' } `
      "https://api.github.com/repos/$repo/releases/latest").tag_name
  } catch { Fail "cannot reach github.com: $($_.Exception.Message)" }
}
if (-not $Tag) { Fail "no releases found for $repo" }
if ($Tag -notmatch '^v') { $Tag = "v$Tag" }

$asset = "harbor-$Tag-$plat.zip"
$base  = "https://github.com/$repo/releases/download/$Tag"
$tmp   = Join-Path ([IO.Path]::GetTempPath()) ("harbor-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
  Write-Host "installing harbor $Tag ($plat)"
  $zip = Join-Path $tmp $asset
  try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $zip }
  catch { Fail "download failed: $base/$asset" }

  # --- verify against the release's published checksums ----------------------
  $sums = Join-Path $tmp 'checksums.txt'
  try { Invoke-WebRequest -UseBasicParsing -Uri "$base/harbor-$Tag-checksums.txt" -OutFile $sums }
  catch { Fail "download failed: harbor-$Tag-checksums.txt" }

  $want = (Get-Content $sums | ForEach-Object {
    $f = $_ -split '\s+'
    if ($f.Count -ge 2 -and $f[1] -eq $asset) { $f[0] }
  }) | Select-Object -First 1
  if (-not $want) { Fail "no checksum published for $asset" }
  $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash
  if ($got -ne $want.ToUpperInvariant()) { Fail "checksum mismatch for $asset" }

  # --- unpack and place ------------------------------------------------------
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $src = Join-Path $tmp "harbor-$Tag-$plat\bin"
  if (-not (Test-Path $src)) { Fail "archive did not contain bin\ — got $(Get-ChildItem $tmp | Select-Object -Expand Name)" }

  $bin = Join-Path $InstallDir 'bin'
  New-Item -ItemType Directory -Path $bin -Force | Out-Null

  # A running berth holds its own .exe open, so replacing it fails with a
  # sharing violation. Say which one and what to do rather than half-installing.
  foreach ($f in Get-ChildItem $src -File) {
    $dest = Join-Path $bin $f.Name
    try { Copy-Item $f.FullName $dest -Force }
    catch { Fail "cannot replace $dest — stop any running berths first (harbor stop <name>), then re-run" }
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "installed: harbor + pilot -> $bin"

# --- PATH, for this user only ------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $bin) {
  [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ";$bin").TrimStart(';')), 'User')
  $env:Path = "$env:Path;$bin"
  Write-Host "added to your PATH — open a new terminal for it to take effect elsewhere"
}

Write-Host ""
Write-Host "try: harbor start mydata.duckdb --create ; pilot mydata"
