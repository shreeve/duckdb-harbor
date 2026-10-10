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

# Windows will not overwrite a file a running program holds — a server, a
# REPL, or the `harbor update` that ran this — but it will rename one. So each
# file is moved aside first and the new one copied into its place; the copies
# moved aside by an earlier install go now, those no longer running. A copy
# that fails puts every file back as it was: harbor.exe and duckdb.dll are
# one install, never half of each.
Get-ChildItem $bin -Filter '*.old-*' | Remove-Item -Force -ErrorAction SilentlyContinue
$moved = @()
try {
  foreach ($f in Get-ChildItem $src -File) {
    $dest = Join-Path $bin $f.Name
    if (Test-Path $dest) {
      $aside = "$dest.old-" + [Guid]::NewGuid().ToString('N').Substring(0, 8)
      Move-Item $dest $aside
      $moved += ,@($dest, $aside)
    }
    Copy-Item $f.FullName $dest
  }
} catch {
  foreach ($m in $moved) {
    Remove-Item $m[0] -Force -ErrorAction SilentlyContinue
    Move-Item $m[1] $m[0] -ErrorAction SilentlyContinue
  }
  Write-Error "install: cannot replace the files in $bin — $($_.Exception.Message); nothing was changed"
  exit 1
}

Write-Host "installed: harbor -> $bin"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $bin) {
  [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ";$bin").TrimStart(';')), 'User')
  $env:Path = "$env:Path;$bin"
  Write-Host "added to your PATH — open a new terminal for it to take effect elsewhere"
}
