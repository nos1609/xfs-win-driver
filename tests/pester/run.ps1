param([string]$Path = 'tests/pester')
$ErrorActionPreference = 'Stop'
$c = New-PesterConfiguration
$c.Run.Path = $Path
$c.Run.PassThru = $true
$c.Output.Verbosity = 'None'
$r = Invoke-Pester -Configuration $c
"Passed=$($r.PassedCount) Failed=$($r.FailedCount) Skipped=$($r.SkippedCount)"
foreach ($f in $r.Failed) {
    'FAIL: ' + $f.ExpandedPath
    $msg = $f.ErrorRecord.Exception.Message
    if ($msg.Length -gt 300) { $msg = $msg.Substring(0, 300) }
    '      ' + ($msg -replace '\s+', ' ')
}
