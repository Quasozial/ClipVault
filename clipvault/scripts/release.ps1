<#
.SYNOPSIS
  Builds a signed ClipVault installer and publishes it as a GitHub release
  that the in-app updater picks up.

.DESCRIPTION
  1. Bump "version" in src-tauri/tauri.conf.json (semver, e.g. 0.2.0) and commit.
  2. Run:  .\scripts\release.ps1 -Notes "What changed"
     (-DryRun builds and writes latest.json without tagging or uploading.)

  The updater reads
  https://github.com/Quasozial/ClipVault/releases/latest/download/latest.json,
  so the repository must be public and the release must not be a draft or
  prerelease. When that fails (some antivirus, firewalls or rate limits answer
  the app but not the browser), it falls back to the copy committed at
  updater/latest.json on main, read through raw.githubusercontent.com and
  jsDelivr: this script commits and pushes that copy after publishing, so
  releases must be made from main. Each installer is signed with the updater key
  (default: ~/.tauri/clipvault.key); the app only installs updates whose
  signature matches the public key in tauri.conf.json. Lose the private key
  and installed copies can never update again: back it up.
#>
param(
  [string]$Notes = "",
  [string]$NotesFile = "",
  [string]$KeyPath = "$HOME\.tauri\clipvault.key",
  [string]$TargetDir = "C:/cvt",
  [string]$Repo = "Quasozial/ClipVault",
  [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$conf = Get-Content "src-tauri/tauri.conf.json" -Raw | ConvertFrom-Json
$version = $conf.version
$tag = "v$version"
Write-Host "ClipVault $version ($tag)"

if ($NotesFile) { $Notes = Get-Content $NotesFile -Raw }

# ----- preflight -----
if (-not (Test-Path "src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe")) {
  throw "FFmpeg sidecar missing: src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe"
}
if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
  if (-not (Test-Path $KeyPath)) { throw "Signing key not found: $KeyPath" }
  $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $KeyPath -Raw
}
if ($null -eq $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) { $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "" }

if (-not $DryRun) {
  if (git status --porcelain) { throw "Working tree has uncommitted changes; commit them first." }
  if ((git rev-parse --abbrev-ref HEAD) -ne "main") { throw "Release from main: the fallback manifest is read from main." }
  git fetch --tags --quiet origin
  if (git tag --list $tag) { throw "Tag $tag already exists: bump the version in src-tauri/tauri.conf.json." }
  $null = gh auth status 2>&1
  if ($LASTEXITCODE -ne 0) { throw "gh is not signed in (gh auth login)." }
}

# ----- build (signs the installer: createUpdaterArtifacts) -----
$env:CARGO_TARGET_DIR = $TargetDir
npm run tauri build
if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }

$nsis = Join-Path $TargetDir "release/bundle/nsis"
$setupName = "ClipVault_${version}_x64-setup.exe"
$setup = Join-Path $nsis $setupName
$sigFile = "$setup.sig"
if (-not (Test-Path $setup)) { throw "Installer not found: $setup" }
if (-not (Test-Path $sigFile)) { throw "Signature not found: $sigFile (is bundle.createUpdaterArtifacts on?)" }

# ----- latest.json (the updater manifest) -----
$manifest = [ordered]@{
  version   = $version
  notes     = $Notes
  pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
  platforms = [ordered]@{
    "windows-x86_64" = [ordered]@{
      signature = (Get-Content $sigFile -Raw).Trim()
      url       = "https://github.com/$Repo/releases/download/$tag/$setupName"
    }
  }
}
$latest = Join-Path $nsis "latest.json"
[IO.File]::WriteAllText($latest, ($manifest | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding $false))
Write-Host "Built: $setup"
Write-Host "Manifest: $latest"

if ($DryRun) {
  Write-Host "Dry run: not tagged, not uploaded."
  exit 0
}

# ----- publish -----
git tag $tag
git push origin $tag
if ($LASTEXITCODE -ne 0) { throw "Pushing tag $tag failed" }
# Notes go through a file: Windows PowerShell 5.1 splits arguments with
# embedded double quotes when calling native commands.
$notesText = if ($Notes) { $Notes } else { "ClipVault $version" }
$notesPath = Join-Path $nsis "release-notes.md"
[IO.File]::WriteAllText($notesPath, $notesText, (New-Object Text.UTF8Encoding $false))
gh release create $tag $setup $sigFile $latest --repo $Repo --title "ClipVault $version" --notes-file $notesPath --verify-tag
if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }

# ----- fallback manifest (after the release exists, so its URL works) -----
Copy-Item $latest "updater/latest.json" -Force
git add "updater/latest.json"
git commit -m "chore(release): $version update manifest"
if ($LASTEXITCODE -ne 0) { throw "Committing updater/latest.json failed" }
git push origin HEAD:main
if ($LASTEXITCODE -ne 0) { throw "Pushing updater/latest.json failed (the GitHub release is published; push main by hand)" }
# jsDelivr caches branch files for up to 12 h; ask it to refetch now.
try {
  $null = Invoke-WebRequest -UseBasicParsing "https://purge.jsdelivr.net/gh/$Repo@main/clipvault/updater/latest.json"
} catch {
  Write-Warning "jsDelivr purge failed ($_); its copy refreshes within 12 h."
}
Write-Host "Published $tag. Installed copies will offer the update on their next check."
