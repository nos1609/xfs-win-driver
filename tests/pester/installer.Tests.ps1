#requires -Version 5.1
<#
    Checks over the installer scripts.

    Each one encodes a way this installer was observed to be wrong, so the
    class of mistake fails a build instead of someone's machine:

      - Windows PowerShell 5.1 reads a BOM-less .ps1 as Windows-1252, so
        non-ASCII bytes garble later string literals;
      - a running service holds its own binary, so copying over it fails and
        every upgrade would break;
      - the launcher CommandLine template is the only place `--ro` exists for
        the service path, and it can go missing with nothing complaining;
      - three separate lists name the zip contents and drift independently;
      - the packaging toolchain was replaced, and a workflow still pointing at
        the deleted files looks green until someone pushes a tag.

    Paths are derived from $PSScriptRoot inside each test rather than shared
    state at the top of the file: Pester runs an It in its own scope, and a
    file-level function or $script: variable is not reliably visible there.
#>

Describe 'installer scripts survive Windows PowerShell 5.1' {
    $names = 'install.ps1', 'uninstall.ps1', 'package.ps1', 'machine-path.ps1', 'winfsp-pin.ps1', 'Mount-Xfs.ps1'

    It 'parses without syntax errors: <name>' -TestCases ($names | ForEach-Object { @{ name = $_ } }) {
        param($name)

        $path = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer' |
            ForEach-Object { Join-Path $_ $name }
        Test-Path $path | Should -BeTrue -Because "$name ships inside the release zip"
        $errors = $null
        [void][System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$errors)
        $errors | Should -BeNullOrEmpty -Because "$name must parse in whichever shell unpacks the zip"
    }

    It 'contains no non-ASCII bytes: <name>' -TestCases ($names | ForEach-Object { @{ name = $_ } }) {
        param($name)

        $installer = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer'
        $bytes = [IO.File]::ReadAllBytes((Join-Path $installer $name))
        $offenders = @($bytes | Where-Object { $_ -gt 127 })
        $because = @(
            '5.1 decodes a BOM-less .ps1 as Windows-1252, so one em dash in a',
            'comment shifts the bytes of a later literal and the script stops',
            'parsing -- on the user machine rather than on this one'
        ) -join ' '
        $offenders.Count | Should -Be 0 -Because $because
    }
}

Describe 'install.ps1 ordering and safety' {
    BeforeAll {
        $installer = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer'
        $script:installText = [IO.File]::ReadAllText((Join-Path $installer 'install.ps1'))
        $script:machinePath = [IO.File]::ReadAllText((Join-Path $installer 'machine-path.ps1'))
    }

    It 'stops a running service before replacing its binary' {
        # A running service keeps xfs.exe open as its image, so the copy fails
        # with a sharing violation -- which is every upgrade. The MSI had the
        # StopServices action for exactly this.
        $stop = $installText.IndexOf('Stop-Service')
        $copy = $installText.IndexOf('Copy-Item -Path $ExePath')
        $stop | Should -BeGreaterThan -1 -Because 'the service must be stopped before its own file is overwritten'
        $copy | Should -BeGreaterThan -1
        $stop | Should -BeLessThan $copy -Because 'stopping after the copy attempt is too late'
    }

    It 'refuses to copy rather than racing the file lock' {
        $installText | Should -Match 'refusing to copy over a running binary'
    }

    It 'does not let the system PATH value type change under it' {
        # SetEnvironmentVariable('Path', ...) on Machine scope writes REG_SZ
        # unless the string contains a '%', silently downgrading a machine
        # whose PATH still relies on %VAR% entries.
        $installText | Should -Match 'Get-MachinePath'
        $installText | Should -Not -Match '\[Environment\]::SetEnvironmentVariable\(\s*.Path.'
        $machinePath | Should -Match 'DoNotExpandEnvironmentNames'
        $machinePath | Should -Match 'GetValueKind'
    }

    It 'assembles sc.exe arguments instead of trusting the shell to quote them' {
        # The same source line passes under pwsh 7 and fails under 5.1 with
        # 1639 (ERROR_INVALID_USER_BUFFER): the two escape embedded quotes
        # differently. ProcessStartInfo takes the shell out of the picture.
        $installText | Should -Match 'ProcessStartInfo'
        $installText | Should -Not -Match '&\s*sc\.exe\s'
    }

    It 'confirms delayed start from the registry rather than from sc output' {
        # Start=2 by itself reports as plain Automatic everywhere, so a delay
        # that silently failed to apply would still look like success.
        $installText | Should -Match 'start= delayed-auto'
        $installText | Should -Match 'DelayedAutostart'
    }
}

Describe 'the read-only guarantee cannot be lost quietly' {
    BeforeAll {
        $installer = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer'
        $script:installText = [IO.File]::ReadAllText((Join-Path $installer 'install.ps1'))
    }

    It 'keeps --ro in the launcher CommandLine template' {
        # A service-driven mount takes its argv from this registry value; no
        # line of driver code ever sees the flag, so the template is the only
        # place it can live. Without it the volume comes up writable through
        # the overlay on a partition that is somebody's only copy.
        $installText | Should -Match 'mount %2 --drive %1 --part %3 --ro'
    }

    It 'writes the launcher class into the view the launcher reads' {
        # WinFsp.Launcher is 32-bit. A class in the 64-bit view is invisible
        # to it and the mount fails later with c0000034 OBJECT_NAME_NOT_FOUND.
        $installText | Should -Match 'SOFTWARE\\WOW6432Node\\WinFsp\\Services'
    }

    It 'keeps --ro in the sibling watcher spawn too' {
        $repo = Join-Path $PSScriptRoot '..' '..'
        $watch = Join-Path (Split-Path -Parent (Resolve-Path $repo)) 'winfsp-fs-skeleton'
        $watch = Join-Path $watch 'src'
        $watch = Join-Path $watch 'watch.rs'
        Test-Path $watch | Should -BeTrue -Because 'the watcher is a sibling checkout, not a vendored copy'
        [IO.File]::ReadAllText($watch) | Should -Match '\.arg\("--ro"\)'
    }
}

Describe 'one list defines the zip contents' {
    # The payload is named in three places: what package.ps1 stages, what
    # install.ps1 copies forward, and what the release workflow asserts is in
    # the zip. Each list is checked against this one declaration rather than
    # being parsed out of the other files -- a regex that silently matches too
    # little would turn this test back into the drift it exists to catch.
    $expected = 'xfs.exe', 'install.ps1', 'uninstall.ps1', 'Mount-Xfs.ps1',
                'winfsp-pin.ps1', 'machine-path.ps1', 'README.md'

    It 'package.ps1 stages every file: <name>' -TestCases ($expected | ForEach-Object { @{ name = $_ } }) {
        param($name)

        $repo = Join-Path $PSScriptRoot '..' '..'
        $text = [IO.File]::ReadAllText((Join-Path (Join-Path $repo 'installer') 'package.ps1'))
        [bool]$text.Contains("'$name'") | Should -BeTrue -Because 'the zip has to carry it'
    }

    It 'install.ps1 copies forward every file the zip carries: <name>' -TestCases ($expected | ForEach-Object { @{ name = $_ } }) {
        param($name)

        $repo = Join-Path $PSScriptRoot '..' '..'
        $text = [IO.File]::ReadAllText((Join-Path (Join-Path $repo 'installer') 'install.ps1'))
        if ($name -eq 'xfs.exe') {
            # the binary is copied by name, not through the helper loop
            [bool]$text.Contains("'xfs.exe'") | Should -BeTrue
            return
        }
        [bool]$text.Contains("'$name'") | Should -BeTrue -Because 'install.ps1 re-copies what it dot-sources'
    }

    It 'the release workflow asserts every file in the zip: <name>' -TestCases ($expected | ForEach-Object { @{ name = $_ } }) {
        param($name)

        $repo = Join-Path $PSScriptRoot '..' '..'
        $flow = Join-Path (Join-Path (Join-Path $repo '.github') 'workflows') 'release.yml'
        $text = [IO.File]::ReadAllText($flow)
        [bool]$text.Contains("'$name'") | Should -BeTrue -Because 'a zip missing a helper installs and then dies on the dot-source'
    }

    It 'uninstall.ps1 removes every file install.ps1 copied' {
        $repo = Join-Path $PSScriptRoot '..' '..'
        $installer = Join-Path $repo 'installer'
        $install = [IO.File]::ReadAllText((Join-Path $installer 'install.ps1'))
        $uninstall = [IO.File]::ReadAllText((Join-Path $installer 'uninstall.ps1'))
        foreach ($name in ($expected | Where-Object { $_ -ne 'README.md' })) {
            [bool]$install.Contains("'$name'") | Should -BeTrue -Because "$name is installed"
            [bool]$uninstall.Contains("'$name'") | Should -BeTrue -Because "$name must not be orphaned in Program Files"
        }
    }
}

Describe 'nothing still points at the removed WiX stage' {
    BeforeAll {
        $repo = Join-Path $PSScriptRoot '..' '..'
        $script:workflowDir = Join-Path (Join-Path $repo '.github') 'workflows'
    }

    It 'workflows do not reference deleted packaging files' {
        $forbidden = 'installer\\build\.ps1', 'verify-silent\.ps1', 'Product\.wxs', 'Bundle\.wxs', 'Setup\.exe', '\.msi\b'
        foreach ($file in Get-ChildItem $workflowDir -Filter *.yml) {
            # Comments talk about the WiX stage that was removed; only a live
            # step can fail a release, so scan the executable lines.
            $text = ([IO.File]::ReadAllLines($file.FullName) |
                Where-Object { $_.TrimStart() -notlike '#*' }) -join "`n"
            foreach ($pattern in $forbidden) {
                $text | Should -Not -Match $pattern -Because "$($file.Name) would fail on $pattern, which no longer exists"
            }
        }
    }

    It 'workflows do not wait for submodules that are not there' {
        # The dependencies are siblings. A recursive submodule checkout with no
        # .gitmodules leaves ../rust-fs-core absent and cargo fails while
        # parsing the manifest, before compiling anything.
        foreach ($file in Get-ChildItem $workflowDir -Filter *.yml) {
            $text = ([IO.File]::ReadAllLines($file.FullName) |
                Where-Object { $_.TrimStart() -notlike '#*' }) -join "`n"
            $text | Should -Not -Match 'submodules:\s*recursive' -Because $file.Name
        }
    }

    It 'both Windows workflows share one sibling-checkout implementation' {
        $ci = [IO.File]::ReadAllText((Join-Path $workflowDir 'ci.yml'))
        $rel = [IO.File]::ReadAllText((Join-Path $workflowDir 'release.yml'))
        $ci | Should -Match '\./\.github/actions/siblings'
        $rel | Should -Match '\./\.github/actions/siblings'
        ($ci + $rel) | Should -Not -Match 'pin\(\) \{ sed' -Because 'the composite action owns that logic now'
    }
}

Describe 'the WinFsp pin stays machine-readable' {
    It 'declares the four variables at column zero, as the updater expects' {
        $installer = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer'
        $pin = [IO.File]::ReadAllText((Join-Path $installer 'winfsp-pin.ps1'))
        foreach ($v in 'WinFspVersion', 'WinFspMsiName', 'WinFspUrl', 'WinFspSha256') {
            $pin | Should -Match ('(?m)^\$' + $v + '\s*=') -Because "update-winfsp-pin.sh anchors at column 0 for $v"
        }
        $pin | Should -Match '(?m)^\$WinFspUrl\s*=\s*".*\$WinFspMsiName"' -Because 'one field must not go stale against another'
    }

    It 'is the file the updater rewrites, and it needs no jq binary' {
        $installer = Join-Path (Join-Path $PSScriptRoot '..' '..') 'installer'
        $u = [IO.File]::ReadAllText((Join-Path $installer 'update-winfsp-pin.sh'))
        $u | Should -Match 'winfsp-pin\.ps1'
        $u | Should -Not -Match 'build\.ps1'
        $u | Should -Not -Match 'command -v jq' -Because 'it uses gh --jq; demanding jq made it unrunnable here'
    }
}

Describe 'the sibling pins cargo needs are all declared' {
    It 'gives a URL and a REF for every path dependency' {
        $chores = [IO.File]::ReadAllText((Join-Path (Join-Path $PSScriptRoot '..' '..') 'chores.yml'))
        foreach ($key in 'FS_CORE', 'FS_XFS', 'SKELETON', 'HARNESS', 'WINFSP_RS') {
            $chores | Should -Match "(?m)^  ${key}_URL: https://"
            $chores | Should -Match "(?m)^  ${key}_REF: \S+"
        }
    }

    It 'resolves every REF against the URL it is paired with' {
        # A pin that exists only in somebody's local object database is the
        # worst kind: cargo fails in CI with a message about a remote that
        # "does not have" the commit, and the URL and the ref are two
        # different lines of the same file, so they drift independently.
        # Checked against the remote rather than a local clone, because the
        # remote is what CI is going to ask. Skipped without network or
        # without git, which is the case on some dev boxes.
        $repo = (Resolve-Path (Join-Path $PSScriptRoot '..' '..')).Path
        $chores = [IO.File]::ReadAllLines((Join-Path $repo 'chores.yml'))

        $vars = @{}
        foreach ($line in $chores) {
            if ($line -match '^\s{2}([A-Z_]+_(?:URL|REF)):\s*(\S+)') { $vars[$Matches[1]] = $Matches[2] }
        }

        $git = Get-Command git -ErrorAction SilentlyContinue
        if (-not $git) { Set-ItResult -Skipped -Because 'git is not installed here'; return }
        # An unreachable or private remote must skip this, not stall the run
        # behind an authentication prompt.
        $env:GIT_TERMINAL_PROMPT = '0'

        foreach ($key in 'FS_CORE', 'FS_XFS', 'SKELETON', 'HARNESS', 'WINFSP_RS') {
            $url = $vars["${key}_URL"]
            $ref = $vars["${key}_REF"]
            $listing = & $git ls-remote $url 2>$null
            if ($LASTEXITCODE -ne 0) {
                Set-ItResult -Skipped -Because "the remote $url was not reachable from this run"
                return
            }
            # A tag or branch name appears as a ref; a bare commit hash
            # appears as the object id of some ref. Both must be there.
            $escaped = [regex]::Escape($ref)
            $hit = @($listing | Where-Object {
                $_ -match "(^|/)$escaped($|\s)" -or $_ -match "^\S+\s+$escaped\s"
            })
            $hit.Count | Should -BeGreaterThan 0 -Because "$key pins $ref, and $url must actually serve it"
        }
    }
}
