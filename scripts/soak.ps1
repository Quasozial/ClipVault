<#
.SYNOPSIS
  ClipVault soak test: presses the save hotkey on an interval, then verifies every press
  produced a valid clip. Read-only on the capture dir (never deletes or modifies files).
.DESCRIPTION
  1. Sends the save hotkey (default Ctrl+Shift+F10) every -IntervalSec for -Minutes via keybd_event.
  2. Records each press time (and the fault events) to a JSON log.
  3. For each press, finds a new .mp4 in -CaptureDir created within [press, press + window]
     and checks with ffprobe: duration >= SaveLengthSec - 5 and exactly 4 streams
     (1 video + 3 audio). After a fault, only duration >= 5 s is required (the buffer
     was legitimately lost), the stream layout is still required.
  4. Prints a PASS/FAIL table; exit code 1 if any FAIL.
  -FaultInjection: once (at ~1/3 of the run) suspends the ClipVault ffmpeg child processes for
  8 s via NtSuspendProcess/NtResumeProcess, once (at ~2/3) kills them. The supervisor must
  restart them. Both are logged and annotated in the table.
  Run it while a game (e.g. Rocket League) is in the foreground; set -SaveLengthSec to the
  app's save length. ClipVault must be running and capturing, hotkey registered.
.PARAMETER DryRun
  Prints the schedule and exits; sends nothing.
.PARAMETER VerifyOnly
  Path of a previous run's JSON log: skip pressing, just re-run the verification.
.EXAMPLE
  .\soak.ps1 -Minutes 240 -IntervalSec 120 -FaultInjection
  .\soak.ps1 -Minutes 3 -IntervalSec 30 -SaveLengthSec 20 -CaptureDir D:\tmp\cv
#>
[CmdletBinding()]
param(
  [int]$Minutes = 240,
  [int]$IntervalSec = 120,
  [string]$CaptureDir = 'E:\GameClips',
  [string]$Hotkey = 'Ctrl+Shift+F10',
  [int]$SaveLengthSec = 60,
  [string]$Ffprobe,
  [switch]$FaultInjection,
  [string]$LogPath,
  [switch]$DryRun,
  [string]$VerifyOnly
)
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class CvSoakNative {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("ntdll.dll")] public static extern int NtSuspendProcess(IntPtr h);
  [DllImport("ntdll.dll")] public static extern int NtResumeProcess(IntPtr h);
}
'@

function ConvertTo-Vk([string]$name) {
  switch -Regex ($name.ToLower()) {
    '^(ctrl|control)$' { return 0x11 }
    '^shift$' { return 0x10 }
    '^alt$' { return 0x12 }
    '^(win|super|meta)$' { return 0x5B }
    '^f([1-9]|1[0-9]|2[0-4])$' { return (0x70 + [int]$Matches[1] - 1) }
    '^[a-z]$' { return [int][char]$name.ToUpper() }
    '^[0-9]$' { return [int][char]$name }
    default { throw "Unsupported key '$name' in hotkey '$Hotkey'" }
  }
}
function Send-Hotkey([string]$combo) {
  $vks = @($combo -split '\+' | ForEach-Object { ConvertTo-Vk $_.Trim() })
  foreach ($v in $vks) { [CvSoakNative]::keybd_event([byte]$v, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 30 }
  Start-Sleep -Milliseconds 60
  [array]::Reverse($vks)
  foreach ($v in $vks) { [CvSoakNative]::keybd_event([byte]$v, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 30 }
}
# Capture ffmpeg = the long-running segment-muxer process. Short-lived children (save concat,
# thumbnails, probes) never have the segment muxer / seg_%08d.ts pattern on their command line.
function Select-CaptureProcess($records) {
  @($records | Where-Object { $_.Name -like 'ffmpeg*' -and $_.CommandLine -match '-f segment|seg_%08d' })
}
function Get-ClipVaultFfmpeg {
  $app = Get-Process -Name clipvault -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $app) { return @() }
  $recs = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($app.Id)")
  @(Select-CaptureProcess $recs | ForEach-Object { Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue })
}
function Test-CaptureSelection {
  $fake = @(
    [pscustomobject]@{ Name = 'ffmpeg.exe'; ProcessId = 1; CommandLine = 'ffmpeg -f lavfi -i x -f segment -segment_time 2 C:\seg_%08d.ts' },
    [pscustomobject]@{ Name = 'ffmpeg.exe'; ProcessId = 2; CommandLine = 'ffmpeg -f concat -safe 0 -i list.txt -c copy out.mp4.tmp' },
    [pscustomobject]@{ Name = 'ffmpeg.exe'; ProcessId = 3; CommandLine = 'ffmpeg -i clip.mp4 -frames:v 1 thumb.jpg' },
    [pscustomobject]@{ Name = 'webview2.exe'; ProcessId = 4; CommandLine = '-f segment seg_%08d' }
  )
  $pick = @(Select-CaptureProcess $fake)
  if ($pick.Count -ne 1 -or $pick[0].ProcessId -ne 1) { throw 'self-test failed: capture selection' }
  Write-Host 'Self-test OK: only the segment-muxer ffmpeg is selected (not concat/thumbnail/non-ffmpeg).'
}
function Find-Ffprobe {
  if ($Ffprobe) { return $Ffprobe }
  $c = Get-Command ffprobe.exe -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  $wg = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
  if (Test-Path $wg) {
    $f = Get-ChildItem $wg -Recurse -Filter ffprobe.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($f) { return $f.FullName }
  }
  throw 'ffprobe.exe not found; pass -Ffprobe <path>'
}

$presses = @(); $faults = @(); $startedAt = Get-Date
$firstDelay = [Math]::Max($IntervalSec, $SaveLengthSec + 10)   # let the buffer fill

if ($VerifyOnly) {
  $j = Get-Content -Raw -LiteralPath $VerifyOnly | ConvertFrom-Json
  $presses = @($j.presses | ForEach-Object { [pscustomobject]@{ Index = $_.Index; Time = [datetime]$_.Time } })
  $faults = @($j.faults | ForEach-Object { [pscustomobject]@{ Kind = $_.Kind; Time = [datetime]$_.Time; Detail = $_.Detail } })
  $CaptureDir = $j.captureDir; $SaveLengthSec = $j.saveLengthSec; $IntervalSec = $j.intervalSec
}
else {
  $total = $Minutes * 60
  $count = 0
  if ($total -ge $firstDelay) { $count = [int][Math]::Floor(($total - $firstDelay) / $IntervalSec) + 1 }
  $suspendAt = -1; $killAt = -1
  if ($FaultInjection) {
    # Never on a press tick: snap to the middle of an interval between two presses.
    $half = [int]($IntervalSec / 2)
    $suspendAt = $firstDelay + [int][Math]::Floor(($total / 3 - $firstDelay) / $IntervalSec) * $IntervalSec + $half
    $killAt = $firstDelay + [int][Math]::Floor(($total * 2 / 3 - $firstDelay) / $IntervalSec) * $IntervalSec + $half
  }
  Write-Host ("Soak: {0} min, hotkey {1} every {2}s (first at +{3}s) = {4} presses; capture dir {5}" -f $Minutes, $Hotkey, $IntervalSec, $firstDelay, $count, $CaptureDir)
  if ($FaultInjection) { Write-Host "Fault injection: suspend ffmpeg 8 s at +${suspendAt}s, kill at +${killAt}s" }
  if ($DryRun) { Test-CaptureSelection; Write-Host 'DryRun: nothing sent.'; return }
  if (-not (Test-Path -LiteralPath $CaptureDir)) { throw "Capture dir not found: $CaptureDir" }
  if (-not (Get-Process -Name clipvault -ErrorAction SilentlyContinue)) { throw 'ClipVault is not running.' }
  if (@(Get-ClipVaultFfmpeg).Count -eq 0) { Write-Warning 'No ffmpeg child of ClipVault yet (not capturing?). Presses will likely FAIL.' }
  [void](Find-Ffprobe)

  $events = @()
  for ($i = 0; $i -lt $count; $i++) { $events += [pscustomobject]@{ At = $firstDelay + $i * $IntervalSec; Kind = 'press'; Index = $i + 1 } }
  if ($FaultInjection) {
    $events += [pscustomobject]@{ At = $suspendAt; Kind = 'suspend'; Index = 0 }
    $events += [pscustomobject]@{ At = $killAt; Kind = 'kill'; Index = 0 }
  }
  $events = $events | Sort-Object At
  $t0 = Get-Date
  foreach ($e in $events) {
    $wait = $e.At - ((Get-Date) - $t0).TotalSeconds
    if ($wait -gt 0) { Start-Sleep -Milliseconds ([int]($wait * 1000)) }
    $now = Get-Date
    switch ($e.Kind) {
      'press' {
        Send-Hotkey $Hotkey
        $presses += [pscustomobject]@{ Index = $e.Index; Time = $now }
        Write-Host ("[{0}] press #{1}" -f $now.ToString('HH:mm:ss'), $e.Index)
      }
      'suspend' {
        $ps = @(Get-ClipVaultFfmpeg)
        foreach ($p in $ps) { [void][CvSoakNative]::NtSuspendProcess($p.Handle) }
        Write-Host ("[{0}] suspended {1} ffmpeg process(es) for 8 s" -f $now.ToString('HH:mm:ss'), $ps.Count)
        Start-Sleep -Seconds 8
        foreach ($p in $ps) { try { [void][CvSoakNative]::NtResumeProcess($p.Handle) } catch {} }
        $faults += [pscustomobject]@{ Kind = 'suspend'; Time = $now; Detail = "$($ps.Count) proc, 8 s" }
      }
      'kill' {
        $ps = @(Get-ClipVaultFfmpeg)
        foreach ($p in $ps) { try { $p.Kill() } catch {} }
        Write-Host ("[{0}] killed {1} ffmpeg process(es)" -f $now.ToString('HH:mm:ss'), $ps.Count)
        $faults += [pscustomobject]@{ Kind = 'kill'; Time = $now; Detail = "$($ps.Count) proc" }
      }
    }
  }
  Start-Sleep -Seconds 30   # let the last save finish
  if (-not $LogPath) { $LogPath = Join-Path $env:TEMP ("clipvault-soak-{0}.json" -f $startedAt.ToString('yyyyMMdd-HHmmss')) }
  [pscustomobject]@{
    captureDir = $CaptureDir; saveLengthSec = $SaveLengthSec; intervalSec = $IntervalSec; hotkey = $Hotkey
    presses = @($presses | ForEach-Object { @{ Index = $_.Index; Time = $_.Time.ToString('o') } })
    faults = @($faults | ForEach-Object { @{ Kind = $_.Kind; Time = $_.Time.ToString('o'); Detail = $_.Detail } })
  } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $LogPath -Encoding UTF8
  Write-Host "Press log: $LogPath (re-verify later with -VerifyOnly)"
}

# ---------- verification ----------
$probe = Find-Ffprobe
$minDur = $SaveLengthSec - 5
$window = [Math]::Min($IntervalSec, 60)
$clips = @(Get-ChildItem -LiteralPath $CaptureDir -Recurse -Filter *.mp4 -File)
$used = @{}
$rows = @()
foreach ($p in ($presses | Sort-Object Time)) {
  $lo = $p.Time.AddSeconds(-1); $hi = $p.Time.AddSeconds($window)
  $m = $clips | Where-Object { -not $used.ContainsKey($_.FullName) -and
      (($_.CreationTime -ge $lo -and $_.CreationTime -le $hi) -or ($_.LastWriteTime -ge $lo -and $_.LastWriteTime -le $hi)) } |
    Sort-Object CreationTime | Select-Object -First 1
  $near = $faults | Where-Object { $p.Time -ge $_.Time -and ($p.Time - $_.Time).TotalSeconds -le ($SaveLengthSec + $IntervalSec) } | Select-Object -Last 1
  $note = ''
  if ($near) { $note = "after $($near.Kind)" }
  $need = $minDur
  if ($near) { $need = 5 }
  $row = [ordered]@{ Press = $p.Index; At = $p.Time.ToString('HH:mm:ss'); File = ''; Dur = ''; Streams = ''; Result = 'FAIL'; Note = $note }
  if (-not $m) { $row.Note = ("no clip created " + $note).Trim() }
  else {
    $used[$m.FullName] = $true
    $row.File = $m.Name
    try {
      $out = & $probe -v error -show_entries 'format=duration:stream=codec_type' -of json $m.FullName | ConvertFrom-Json
      $dur = [double]$out.format.duration
      $v = @($out.streams | Where-Object codec_type -eq 'video').Count
      $a = @($out.streams | Where-Object codec_type -eq 'audio').Count
      $row.Dur = ('{0:N1}s' -f $dur); $row.Streams = "${v}v+${a}a"
      $problems = @()
      if ($dur -lt $need) { $problems += "duration < $need s" }
      if ($v -ne 1 -or $a -ne 3) { $problems += 'streams != 1v+3a' }
      if ($problems.Count -eq 0) { $row.Result = 'PASS' } else { $row.Note = (($problems -join ', ') + ' ' + $note).Trim() }
    }
    catch { $row.Note = "ffprobe failed: $($_.Exception.Message)" }
  }
  $rows += [pscustomobject]$row
}
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
foreach ($f in $faults) { Write-Host ("fault: {0} at {1} ({2})" -f $f.Kind, $f.Time.ToString('HH:mm:ss'), $f.Detail) }
$logDir = Join-Path $env:LOCALAPPDATA 'ClipVault\logs'
if (Test-Path $logDir) {
  $lf = Get-ChildItem $logDir -Filter 'clipvault*' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
  if ($lf) {
    $n = @(Get-Content -LiteralPath $lf.FullName -ErrorAction SilentlyContinue | Where-Object { $_ -match 'run \d+ ended|kicking output' }).Count
    Write-Host "Log $($lf.Name): $n supervisor 'run N ended' / 'kicking' line(s) (whole file; expect > 0 after fault injection)"
  }
}
$fail = @($rows | Where-Object Result -eq 'FAIL').Count
Write-Host ("SUMMARY: {0} presses, {1} PASS, {2} FAIL" -f $rows.Count, ($rows.Count - $fail), $fail)
if ($rows.Count -eq 0) { Write-Host 'No presses recorded.'; exit 1 }
if ($fail -gt 0) { exit 1 }
exit 0
