#requires -Version 5.1
<#
.SYNOPSIS
  Builds xfs.exe for one architecture and packs the release zip.

.DESCRIPTION
  Replaces the old WiX packaging step. What an installer generator actually
  bought us here was three things -- copy files, register a service, write a
  registry class -- and install.ps1 does all three with tools that are part
  of Windows. Dropping WiX also drops its release gate: WiX v7 refuses to run
  at all until the Open Source Maintenance Fee EULA is accepted, which every
  fork and every CI runner would have to inherit for a package whose source
  is under the Microsoft Reciprocal License.

  Two things from the old script are deliberately carried over, because they
  each encode a failure that was paid for: the PE header check that catches an
  x64 binary labelled arm64, and the WinFsp pin with its checksum.
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Arch = 'arm64',

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [string]$Version,

    [string]$OutputDir,

    # Skip the cargo invocation and package an exe that is already built.
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$scriptDir = $PSScriptRoot
$repoRoot  = Split-Path -Parent $scriptDir
# Release notes and the install check share one pin.
. (Join-Path $scriptDir 'winfsp-pin.ps1')
if (-not $OutputDir) { $OutputDir = Join-Path $repoRoot 'dist' }
if (-not (Test-Path $OutputDir)) { New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null }

$target = switch ($Arch) {
    'x64'   { 'x86_64-pc-windows-msvc' }
    'arm64' { 'aarch64-pc-windows-msvc' }
}

if (-not $Version) {
    $inPackage = $false
    foreach ($line in Get-Content (Join-Path $repoRoot 'Cargo.toml')) {
        if ($line -match '^\s*\[package\]\s*$') { $inPackage = $true; continue }
        if ($line -match '^\s*\[') { $inPackage = $false; continue }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
    }
    if (-not $Version) { throw "could not read [package] version from Cargo.toml; pass -Version" }
}

# ---------------------------------------------------------------------------
# Build. --target is not optional cosmetics: without it the output lands in
# target\release\ and the packaging step quietly ships whatever the previous
# --target build left in the arch directory. That has happened on this repo.
# ---------------------------------------------------------------------------
$env:CARGO_TARGET_DIR = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repoRoot 'target' }
if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        & cargo build --locked --release --features mount,service --target $target
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    }
    finally { Pop-Location }
}

$exePath = Join-Path (Join-Path $env:CARGO_TARGET_DIR $target) (Join-Path $Configuration.ToLower() 'xfs.exe')
if (-not (Test-Path $exePath)) { throw "expected build output not found: $exePath" }
$exePath = (Resolve-Path $exePath).Path

# ---------------------------------------------------------------------------
# PE sniff: IMAGE_FILE_HEADER.Machine must agree with -Arch. A mislabelled
# binary installs cleanly and then does not run, which is the worst kind of
# release artifact to discover at a user's machine.
# ---------------------------------------------------------------------------
$fs = [IO.File]::OpenRead($exePath)
try {
    $buf = New-Object byte[] 4096
    $n = $fs.Read($buf, 0, $buf.Length)
    if ($n -lt 0x40 -or $buf[0] -ne 0x4D -or $buf[1] -ne 0x5A) {
        throw "not a PE file: $exePath"
    }
    $eLfanew = [BitConverter]::ToInt32($buf, 0x3C)
    if ($eLfanew -le 0 -or ($eLfanew + 6) -ge $n -or $buf[$eLfanew] -ne 0x50 -or $buf[$eLfanew + 1] -ne 0x45) {
        throw "no PE\0\0 signature in $exePath"
    }
    $machine = [BitConverter]::ToUInt16($buf, $eLfanew + 4)
    $detected = switch ($machine) {
        0x8664  { 'x64' }
        0xAA64  { 'arm64' }
        default { ('unknown(0x{0:X4})' -f $machine) }
    }
    if ($detected -ne $Arch) {
        throw "PE Machine says $detected but -Arch = $Arch for $exePath -- refusing to package"
    }
    Write-Host "binary            : $detected (PE Machine 0x$($machine.ToString('X4')))"
}
finally { $fs.Dispose() }

# ---------------------------------------------------------------------------
# Stage and zip.
# ---------------------------------------------------------------------------
$staging = Join-Path $OutputDir "xfs-win-driver-$Version-$Arch"
if (Test-Path $staging) { Remove-Item -Path $staging -Recurse -Force }
New-Item -ItemType Directory -Path $staging -Force | Out-Null

Copy-Item -Path $exePath -Destination (Join-Path $staging 'xfs.exe') -Force
foreach ($f in @('install.ps1', 'uninstall.ps1', 'Mount-Xfs.ps1', 'winfsp-pin.ps1', 'machine-path.ps1', 'README.md')) {
    $src = Join-Path $scriptDir $f
    if (-not (Test-Path $src)) { throw "release payload missing: $src" }
    Copy-Item -Path $src -Destination (Join-Path $staging $f) -Force
}

$zip = Join-Path $OutputDir "xfs-win-driver-$Version-$Arch.zip"
if (Test-Path $zip) { Remove-Item -Path $zip -Force }
Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -CompressionLevel Optimal

$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
$zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
Write-Host ''
Write-Host "archive           : $zip"
Write-Host "archive sha256    : $zipHash"
Write-Host "xfs.exe sha256    : $exeHash"
Write-Host ''
Write-Host 'WinFsp is a prerequisite, not a bundled payload. install.ps1 checks for it'
Write-Host "and refuses with this pin in the message (winfsp-pin.ps1):"
Write-Host "  version    $WinFspVersion"
Write-Host "  url        $WinFspUrl"
Write-Host "  sha256     $WinFspSha256"
