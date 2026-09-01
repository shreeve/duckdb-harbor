#Requires -Version 5.1
<#
install.ps1 — install this harbor release from the extracted archive.

    bin\harbor.exe, bin\duckdb.dll
        -> %LOCALAPPDATA%\Programs\harbor\bin   (override: -InstallDir)

Into your own profile, so nothing here needs Administrator. duckdb.dll sits
beside the executables, which is where Windows looks first — bin travels as
one piece, and runs straight out of this directory without installing.
#>
[CmdletBinding()]
param([string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\harbor'))

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$src = Join-Path $PSScriptRoot 'bin'
if (-not (Test-Path $src)) { Write-Error "install: no bin\ beside this script"; exit 1 }

$bin = Join-Path $InstallDir 'bin'
New-Item -ItemType Directory -Path $bin -Force | Out-Null

# A running berth holds its own .exe open, so replacing it fails with a sharing
# violation. Name the file and the fix rather than half-installing.
foreach ($f in Get-ChildItem $src -File) {
  $dest = Join-Path $bin $f.Name
  try { Copy-Item $f.FullName $dest -Force }
  catch { Write-Error "install: cannot replace $dest — stop any running harbor servers first, then re-run"; exit 1 }
}

Write-Host "installed: harbor -> $bin"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $bin) {
  [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ";$bin").TrimStart(';')), 'User')
  $env:Path = "$env:Path;$bin"
  Write-Host "added to your PATH — open a new terminal for it to take effect elsewhere"
}
