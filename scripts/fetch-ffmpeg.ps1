<#
.SYNOPSIS
  Copies ffmpeg.exe into the Tauri sidecar location and checks it has what ClipVault needs.
.DESCRIPTION
  Destination: clipvault/src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
  (referenced by bundle.externalBin = ["binaries/ffmpeg"]; the installer puts it next to
  the app exe as ffmpeg.exe, where app::plan::resolve_ffmpeg finds it).
  ffprobe is NOT bundled: the app never calls it at runtime (only tests and soak.ps1 do).
  Requires: ddagrab filter, h264_nvenc and hevc_nvenc encoders.
  NOTE: the gyan/BtbN "gpl" builds are GPL. Fine for personal use; redistributing the
  installer carries GPL obligations (offer source, license text).
.PARAMETER Source
  Path to ffmpeg.exe. Default: the WinGet yt-dlp.FFmpeg package, else `where ffmpeg`.
.PARAMETER Check
  Only validate the source, copy nothing.
#>
[CmdletBinding()]
param(
  [string]$Source,
  [switch]$Check
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $repo 'clipvault\src-tauri\binaries\ffmpeg-x86_64-pc-windows-msvc.exe'

if (-not $Source) {
  $winget = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
  $cand = if (Test-Path $winget) {
    Get-ChildItem $winget -Directory -Filter 'yt-dlp.FFmpeg*' -ErrorAction SilentlyContinue |
      ForEach-Object { Get-ChildItem $_.FullName -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue } |
      Select-Object -First 1
  }
  if ($cand) { $Source = $cand.FullName }
  else {
    $w = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($w) { $Source = $w.Source }
  }
}
if (-not $Source -or -not (Test-Path -LiteralPath $Source)) { throw "ffmpeg.exe not found; pass -Source <path>" }
Write-Host "Source: $Source"

$ver = (& $Source -hide_banner -version | Select-Object -First 1)
Write-Host $ver
$filters = (& $Source -hide_banner -filters) -join "`n"
$encoders = (& $Source -hide_banner -encoders) -join "`n"
$missing = @()
if ($filters -notmatch '\bddagrab\b') { $missing += 'filter ddagrab' }
foreach ($e in 'h264_nvenc', 'hevc_nvenc') { if ($encoders -notmatch "\b$e\b") { $missing += "encoder $e" } }
if ($missing) { throw "ffmpeg is missing: $($missing -join ', ')" }
Write-Host 'OK: ddagrab, h264_nvenc, hevc_nvenc present.'

if ($Check) { return }
New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
Copy-Item -LiteralPath $Source -Destination $dest -Force
Write-Host "Copied to $dest ($([math]::Round((Get-Item $dest).Length/1MB,1)) MB)"
