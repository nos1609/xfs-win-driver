#requires -Version 5.1
<#
.SYNOPSIS
  Removes the xfs-win-driver watcher and its WinFsp launcher class.

.DESCRIPTION
  Reverses install.ps1 and nothing else: the XfsWatcher service, the
  xfs-mount launcher class, and the installed files. WinFsp itself is left
  alone -- it was not installed by us, and other drivers on this machine may
  register classes of their own.
#>
[CmdletBinding()]
param(
    [string]$InstallDir     = (Join-Path ${env:ProgramFiles} 'xfs-win-driver'),
    [string]$ServiceName    = 'XfsWatcher',
    [string]$LauncherClass  = 'xfs-mount',
    [switch]$KeepFiles
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'machine-path.ps1')

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Administrator rights are required: this removes an HKLM key and a service.'
}

# Stopping the service releases the volumes it published, through the
# service's own shutdown path. Killing the process instead would leave the
# children behind -- observed directly.
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    & sc.exe delete $ServiceName | Out-Null
    Write-Host "service           : $ServiceName removed"
}
else {
    Write-Host "service           : $ServiceName not present"
}

# Deleting the class does not unmount an already-published volume; the
# launcher owns those until they are stopped or the service dies.
$classKey = "HKLM:\SOFTWARE\WOW6432Node\WinFsp\Services\$LauncherClass"
if (Test-Path $classKey) {
    Remove-Item -Path $classKey -Recurse -Force
    Write-Host "launcher class    : $LauncherClass removed"
}
else {
    Write-Host "launcher class    : $LauncherClass not present"
}

if (-not $KeepFiles) {
    foreach ($f in @('xfs.exe', 'install.ps1', 'uninstall.ps1', 'Mount-Xfs.ps1', 'winfsp-pin.ps1', 'machine-path.ps1')) {
        $p = Join-Path $InstallDir $f
        if (Test-Path $p) { Remove-Item -Path $p -Force }
    }
    if ((Test-Path $InstallDir) -and -not @(Get-ChildItem -Path $InstallDir -Force)) {
        Remove-Item -Path $InstallDir -Force
    }
    Write-Host "files             : removed from $InstallDir"
}

# Rebuild PATH from the existing entries minus ours, so anything a user added
# after us survives untouched and no other entry is reordered or recased.
$machinePath = Get-MachinePath
$kept = @($machinePath -split ';' | Where-Object { $_ -and $_ -ne $InstallDir })
if ($kept.Count -lt (@($machinePath -split ';' | Where-Object { $_ }).Count)) {
    Set-MachinePath ($kept -join ';')
    Write-Host "system PATH       : removed $InstallDir (new windows only)"
}
else {
    Write-Host "system PATH       : nothing to remove"
}

Start-Sleep -Seconds 2
$remaining = @(Get-PSDrive -PSProvider FileSystem | Where-Object { $_.DisplayRoot })
if ($remaining) {
    Write-Host 'Note: these network/WinFsp drives are still present and are not owned by this script:'
    foreach ($r in $remaining) { Write-Host ("  {0}: -> {1}" -f $r.Name, $r.DisplayRoot) }
}
