# Machine PATH read/write, shared by install.ps1 and uninstall.ps1.
#
# [Environment]::SetEnvironmentVariable(..., 'Machine') rewrites HKLM
# ...\Session Manager\Environment!Path, and it picks the registry value type
# from the *content*: REG_EXPAND_SZ only if the string contains a '%'. The
# system PATH is normally REG_EXPAND_SZ whether or not it currently uses one,
# so round-tripping through that API can silently downgrade a machine from
# expand-strings to plain strings and stop every %VAR% entry in it from being
# resolved. Read and write through the registry directly and carry the original
# value kind through, so the type is not something an installer changes.
#
# Reading with DoNotExpandEnvironmentNames matters for the same reason:
# expanding on the way in and writing back would replace %SystemRoot% with a
# literal path permanently.

function Get-MachinePath {
    $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey(
        'SYSTEM\CurrentControlSet\Control\Session Manager\Environment')
    try {
        return [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    }
    finally { $key.Close() }
}

function Set-MachinePath {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Value)

    $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey(
        'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', $true)
    try {
        $kind = $key.GetValueKind('Path')
        $key.SetValue('Path', $Value, $kind)
    }
    finally { $key.Close() }
}
