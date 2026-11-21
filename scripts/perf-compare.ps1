<#
.SYNOPSIS
  FPS comparison helper (ClipVault off vs on) using PresentMon.
.DESCRIPTION
  Measure:  .\perf-compare.ps1 -Measure off -Process RocketLeague.exe -Seconds 120
            (run again with -Measure on while ClipVault is capturing; same map, same actions)
  Compare:  .\perf-compare.ps1 -Compare
            Reads the newest 'off' and 'on' CSVs from -OutDir, prints average FPS, 1% low and the delta.
            PASS if the average-FPS drop is < 3 %. Exit code 1 on FAIL.
  Needs PresentMon (https://github.com/GameTechDev/PresentMon, winget install Intel.PresentMon.Console)
  on PATH or via -PresentMon <exe>. Without it use an in-game FPS counter and compare by hand
  (see docs/VERIFY.md).
#>
[CmdletBinding()]
param(
  [ValidateSet('off', 'on')][string]$Measure,
  [switch]$Compare,
  [string]$Process = 'RocketLeague.exe',
  [int]$Seconds = 120,
  [string]$PresentMon,
  [string]$OutDir = (Join-Path $env:TEMP 'clipvault-perf'),
  [double]$MaxDropPercent = 3.0
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null

function Get-Stats([string]$csv) {
  $rows = Import-Csv $csv
  if (-not $rows) { throw "No frames in $csv" }
  $col = if ($rows[0].PSObject.Properties.Name -contains 'FrameTime') { 'FrameTime' } else { 'msBetweenPresents' }
  $ft = @($rows | ForEach-Object { [double]::Parse($_.$col, [Globalization.CultureInfo]::InvariantCulture) } | Where-Object { $_ -gt 0 })
  $avgFps = 1000.0 / (($ft | Measure-Object -Average).Average)
  $sorted = $ft | Sort-Object -Descending
  $worst = [Math]::Max(1, [int][Math]::Ceiling($sorted.Count * 0.01))
  $low1 = 1000.0 / (($sorted | Select-Object -First $worst | Measure-Object -Average).Average)
  [pscustomobject]@{ Frames = $ft.Count; AvgFps = [Math]::Round($avgFps, 1); Low1Fps = [Math]::Round($low1, 1) }
}

if ($Measure) {
  $pm = $PresentMon
  if (-not $pm) { $c = Get-Command PresentMon*.exe, PresentMon -ErrorAction SilentlyContinue | Select-Object -First 1; if ($c) { $pm = $c.Source } }
  if (-not $pm) { throw 'PresentMon not found. Install it (winget install Intel.PresentMon.Console) or pass -PresentMon <exe>.' }
  $csv = Join-Path $OutDir ("{0}-{1}.csv" -f $Measure, (Get-Date -Format 'yyyyMMdd-HHmmss'))
  $help = (& $pm --help 2>&1) -join "`n"
  if ($help -match '--process_name') {
    $a = @('--process_name', $Process, '--timed', $Seconds, '--terminate_after_timed', '--output_file', $csv, '--no_console_stats')
  } else {
    $a = @('-process_name', $Process, '-timed', $Seconds, '-terminate_after_timed', '-output_file', $csv, '-no_top')
  }
  Write-Host "Measuring '$Process' for $Seconds s (ClipVault $Measure). Play the same scenario now."
  & $pm @a
  Write-Host "Saved $csv"
  Get-Stats $csv | Format-Table | Out-String | Write-Host
  return
}
if ($Compare) {
  $off = Get-ChildItem $OutDir -Filter 'off-*.csv' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
  $on = Get-ChildItem $OutDir -Filter 'on-*.csv' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
  if (-not $off -or -not $on) { throw "Need both off-*.csv and on-*.csv in $OutDir" }
  $so = Get-Stats $off.FullName; $sn = Get-Stats $on.FullName
  $drop = ($so.AvgFps - $sn.AvgFps) / $so.AvgFps * 100.0
  Write-Host ("off: avg {0} fps, 1% low {1} ({2} frames)" -f $so.AvgFps, $so.Low1Fps, $so.Frames)
  Write-Host ("on : avg {0} fps, 1% low {1} ({2} frames)" -f $sn.AvgFps, $sn.Low1Fps, $sn.Frames)
  Write-Host ("avg FPS drop: {0:N2} % (limit {1} %)" -f $drop, $MaxDropPercent)
  if ($drop -lt $MaxDropPercent) { Write-Host 'PASS'; exit 0 } else { Write-Host 'FAIL'; exit 1 }
}
Get-Help $MyInvocation.MyCommand.Path -Detailed | Out-String | Write-Host
