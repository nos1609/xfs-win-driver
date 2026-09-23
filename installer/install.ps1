#requires -Version 5.1
<#
.SYNOPSIS
  Installs the xfs-win-driver auto-mount watcher.

.DESCRIPTION
  Everything an installer has to do here is three facts about Windows, and
  each one was learned the hard way, so they are written down at the point of
  use:

  1. The volume is published by WinFsp.Launcher, not by our process, if the
     mount is to be visible in the user's session. The launcher is a 32-bit
     service and reads its service classes from its own registry view, which
     is WOW6432Node. Writing the class into the 64-bit view leaves the
     launcher blind to it and the mount fails with STATUS_OBJECT_NAME_NOT_FOUND
     (c0000034) -- so the path below is spelled out explicitly rather than
     left to whatever view the caller happens to have.

  2. Automatic (Delayed Start) is not a column of the MSI Services table and
     is not the same as Start=auto. `sc config start= delayed-auto` is what
     sets it; observed as Start=2 plus DelayedAutostart=1 under the service
     key. A watcher that walks every disk and publishes volumes belongs
     after the boot-critical work.

  3. Everything is read-only. `--ro` lives in the launcher's CommandLine
     template, because for the service path that template is the only place
     the flag exists: argv is assembled from it, so nothing in our code ever
     sees it. A template without --ro is a silently writable overlay on
     whatever partition the machine happens to also boot from.

  Idempotent: safe to re-run for repair or upgrade. It only ever touches the
  XfsWatcher service and the xfs-mount class.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path ${env:ProgramFiles} 'xfs-win-driver'),

    # Install from a build tree instead of an unpacked zip. Defaults to the
    # xfs.exe shipped beside this script.
    [string]$ExePath,

    [string]$ServiceName    = 'XfsWatcher',
    [string]$LauncherClass  = 'xfs-mount',
    [switch]$Start = $true
)

$ErrorActionPreference = 'Stop'

# The expected WinFsp release: $WinFspVersion, $WinFspUrl, $WinFspSha256.
. (Join-Path $PSScriptRoot 'winfsp-pin.ps1')
. (Join-Path $PSScriptRoot 'machine-path.ps1')

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Administrator rights are required: this writes HKLM and registers a service.'
}

# Source binary: shipped next to this script inside the zip, or pointed at
# explicitly when installing straight out of a build tree.
if (-not $ExePath) { $ExePath = Join-Path $PSScriptRoot 'xfs.exe' }
if (-not (Test-Path $ExePath)) {
    throw "xfs.exe not found at '$ExePath'. Unpack the whole zip, or pass -ExePath at the build output."
}

# sc.exe takes its `name= value` pairs in a form where the quoting rules of the
# calling shell decide whether it works: the same call succeeds under PowerShell
# 7 and fails under 5.1 with 1639 (ERROR_INVALID_USER_BUFFER), because 5.1
# re-quotes embedded quotes differently. Building the command line by hand and
# handing it to the process directly removes the shell from the picture.
function Invoke-Sc {
    param([Parameter(Mandatory = $true)][string]$Arguments)

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = 'sc.exe'
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    $output = $proc.StandardOutput.ReadToEnd()
    $proc.WaitForExit()
    if ($proc.ExitCode -ne 0) {
        throw "sc $Arguments -> exit $($proc.ExitCode): $($output.Trim())"
    }
    return $output
}

# ---------------------------------------------------------------------------
# WinFsp must already be there. We do not chain its MSI -- that was Burn's
# job, and Burn meant WiX.
# ---------------------------------------------------------------------------
$winfspKey = 'HKLM:\SOFTWARE\WOW6432Node\WinFsp'
$installDirValue = $null
if (Test-Path $winfspKey) {
    $installDirValue = (Get-ItemProperty -Path $winfspKey -ErrorAction SilentlyContinue).InstallDir
}
if (-not $installDirValue -or -not (Test-Path (Join-Path $installDirValue 'bin\launchctl-a64.exe')) -and
    -not (Test-Path (Join-Path $installDirValue 'bin\launchctl.exe'))) {
    throw @"
WinFsp does not appear to be installed (no InstallDir under $winfspKey).
Install WinFsp $WinFspVersion first:
    $WinFspUrl
SHA-256: $WinFspSha256
Verify the download before running it:  Get-FileHash winfsp-$WinFspVersion.msi -Algorithm SHA256
"@
}

# ---------------------------------------------------------------------------
# Stop before touching files. A running service holds xfs.exe as its image, so
# copying over it fails -- which is exactly the upgrade case an installer
# exists for. The old MSI got this for free from the StopServices action.
# ---------------------------------------------------------------------------
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host "service           : $ServiceName exists, stopping before replacing its files"
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        $state = (Get-CimInstance Win32_Service -Filter "Name='$ServiceName'").State
        if ($state -eq 'Stopped') { break }
        Start-Sleep -Milliseconds 500
    }
    if ($state -ne 'Stopped') { throw "$ServiceName did not stop (state $state); refusing to copy over a running binary" }
}

# ---------------------------------------------------------------------------
# Files.
# ---------------------------------------------------------------------------
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}
Copy-Item -Path $ExePath -Destination (Join-Path $InstallDir 'xfs.exe') -Force
foreach ($helper in @('install.ps1', 'uninstall.ps1', 'Mount-Xfs.ps1', 'winfsp-pin.ps1', 'machine-path.ps1', 'README.md')) {
    $src = Join-Path $PSScriptRoot $helper
    if (Test-Path $src) { Copy-Item -Path $src -Destination (Join-Path $InstallDir $helper) -Force }
}
$targetExe = Join-Path $InstallDir 'xfs.exe'
Write-Host "installed files   : $InstallDir"

# Machine PATH, so `xfs info ...` works from a shell the way it did with the
# MSI. Broadcast is not sent: processes already running keep the PATH they
# were started with, so a new window is needed to see this.
$machinePath = Get-MachinePath
if (($machinePath -split ';') -notcontains $InstallDir) {
    Set-MachinePath "$($machinePath.TrimEnd(';'));$InstallDir"
    Write-Host "system PATH       : added $InstallDir (new windows only)"
}
else {
    Write-Host "system PATH       : $InstallDir already present"
}

# ---------------------------------------------------------------------------
# The service.
# ---------------------------------------------------------------------------
# ImagePath is stored with the path quoted, because %ProgramFiles% has a space
# in it: without the inner quotes the SCM splits the command line at that space
# and tries to launch `"C:\Program`".
$inner = '\"' + $targetExe + '\" service'
$existing = Get-CimInstance Win32_Service -Filter "Name='$ServiceName'" -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "service           : reconfiguring $ServiceName"
    Invoke-Sc "config $ServiceName binPath= `"$inner`" start= delayed-auto obj= LocalSystem" | Out-Null
}
else {
    Invoke-Sc "create $ServiceName binPath= `"$inner`" start= delayed-auto obj= LocalSystem DisplayName= `"xfs-win-driver auto-mount watcher`"" | Out-Null
    Write-Host "service           : $ServiceName created"
}

# Recovery. Microsoft documents MSI's own equivalent
# (MsiServiceConfigFailureActions) as "not working as expected" and points at
# sc.exe, so this is not a workaround around a working feature.
Invoke-Sc "failure $ServiceName reset= 86400 actions= restart/5000/restart/10000/restart/30000" | Out-Null
Invoke-Sc "failureflag $ServiceName 1" | Out-Null

# Confirm the delayed flag actually landed -- Start=2 alone would also read
# as "Auto" in WMI and hide a silent failure to apply the delay.
$svcKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
$svc = Get-ItemProperty -Path $svcKey
if ([int]$svc.Start -ne 2 -or [int]$svc.DelayedAutostart -ne 1) {
    throw "expected Start=2 and DelayedAutostart=1 under $svcKey, found Start=$($svc.Start) DelayedAutostart=$($svc.DelayedAutostart)"
}
Write-Host "startup           : Automatic (Delayed Start), verified in the registry"

# ---------------------------------------------------------------------------
# The launcher class. Explicit WOW6432Node -- see the module comment.
# ---------------------------------------------------------------------------
$classKey = "HKLM:\SOFTWARE\WOW6432Node\WinFsp\Services\$LauncherClass"
New-Item -Path $classKey -Force | Out-Null
New-ItemProperty -Path $classKey -Name Executable    -PropertyType String  -Value $targetExe                      -Force | Out-Null
New-ItemProperty -Path $classKey -Name CommandLine   -PropertyType String  -Value 'mount %2 --drive %1 --part %3 --ro' -Force | Out-Null
New-ItemProperty -Path $classKey -Name WorkDirectory -PropertyType String  -Value $InstallDir                     -Force | Out-Null
New-ItemProperty -Path $classKey -Name JobControl    -PropertyType DWord   -Value 1                               -Force | Out-Null
New-ItemProperty -Path $classKey -Name Credentials   -PropertyType DWord   -Value 0                               -Force | Out-Null
New-ItemProperty -Path $classKey -Name Security      -PropertyType String  -Value 'D:P(A;;RPWPLC;;;WD)'           -Force | Out-Null
Write-Host "launcher class    : $LauncherClass (read-only mount template)"

# ---------------------------------------------------------------------------
# Start it. The watcher enumerates the disks that already exist, so a volume
# is published on startup and not only on the next arrival.
# ---------------------------------------------------------------------------
if ($Start) {
    Start-Service -Name $ServiceName
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $state = (Get-CimInstance Win32_Service -Filter "Name='$ServiceName'").State
        if ($state -eq 'Running') { break }
        Start-Sleep -Seconds 1
    }
    $state = (Get-CimInstance Win32_Service -Filter "Name='$ServiceName'").State
    Write-Host "service state     : $state"
    if ($state -ne 'Running') { throw "$ServiceName did not reach Running" }

    $letters = @((Get-PSDrive -PSProvider FileSystem -ErrorAction SilentlyContinue).Name)
    Write-Host "drives now        : $($letters -join ', ')"
    Write-Host ''
    Write-Host 'Note: a volume appears only if a supported filesystem was found. If nothing mounted,'
    Write-Host "check that a Linux/XFS partition exists and that Linux was shut down cleanly -- the"
    Write-Host 'driver refuses to mount a volume whose log still needs replaying.'
}
else {
    Write-Host 'not started (-Start suppressed)'
}

Write-Host ''
Write-Host "Remove with: uninstall.ps1 (as administrator)."
