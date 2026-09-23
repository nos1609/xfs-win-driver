# WinFsp release this driver is pinned to -- the only place it is written
# down. Dot-sourced by install.ps1 and package.ps1.
#
# Refresh with `installer/update-winfsp-pin.sh --apply`, which rewrites these
# four lines from the latest release on GitHub and takes the checksum from the
# release asset digest. The line format is load-bearing: the updater's patterns
# are anchored at column 0 and $WinFspUrl interpolates $WinFspMsiName. Do not
# hand-edit or re-indent.
$WinFspVersion = '2.1.25156'
$WinFspMsiName = 'winfsp-2.1.25156.msi'
$WinFspUrl     = "https://github.com/winfsp/winfsp/releases/download/v2.1/$WinFspMsiName"
$WinFspSha256  = '073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a'
