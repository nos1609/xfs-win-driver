# xfs-win-driver installer

Packaging and installation for the read-only XFS driver. One artefact: a zip
holding `xfs.exe` and the scripts that register it.

## What ships

`installer/package.ps1` produces `xfs-win-driver-<version>-<arch>.zip`:

| File | Purpose |
|---|---|
| `xfs.exe` | CLI, WinFsp file system and SCM watcher -- one binary, subcommand selects |
| `install.ps1` | registers the service and the WinFsp launcher class |
| `uninstall.ps1` | reverses it |
| `Mount-Xfs.ps1` | ad-hoc mount of one image or partition, no service involved |
| `winfsp-pin.ps1` | the WinFsp release this driver is tested against |
| `README.md` | this file |

## Prerequisite: WinFsp

WinFsp is not bundled. `install.ps1` refuses to run without it and quotes the
pinned release, version and checksum from `winfsp-pin.ps1`. Refresh the pin
with `installer/update-winfsp-pin.sh --apply`; it needs an authenticated `gh`
and nothing else (it uses `gh --jq`, not the `jq` binary).

Install WinFsp from its own MSI first. The launcher service has to be running
before a volume can be published, so reboot if its installer asks.

## Install

As administrator, from the unpacked zip -- unpack the whole archive, the
scripts look for `xfs.exe` beside them:

```powershell
Set-ExecutionPolicy -Scope Process Bypass -Force
.\install.ps1
```

It does four things, and three of them are Windows facts that were each paid
for once:

1. **Files** into `%ProgramFiles%\xfs-win-driver`, and that directory onto the
   system PATH (a new window is needed to see it -- no settings broadcast is
   sent).
2. **`XfsWatcher`** registered as LocalSystem with **Automatic (Delayed
   Start)** plus restart-on-failure. Delayed rather than plain auto because the
   watcher enumerates every disk and publishes volumes at startup; it belongs
   after the boot-critical work, not inside it. The script reads the service
   key back and fails if the delay did not land, because `Start=2` alone reads
   as "Auto" everywhere and would hide a silent miss.
3. **The `xfs-mount` launcher class** under
   `HKLM\SOFTWARE\WOW6432Node\WinFsp\Services\xfs-mount` with
   `mount %2 --drive %1 --part %3 --ro`.
4. **Start**, then report what appeared.

Two details in step 3 are the whole game. `WOW6432Node` is not decoration:
WinFsp.Launcher is a 32-bit service and reads classes from the 32-bit view, so
a class written into the 64-bit view is invisible and the mount fails later
with `c0000034` (OBJECT_NAME_NOT_FOUND). And `--ro` must live in this
template, because argv for a service-driven mount is assembled from it --
nothing in the driver's code ever sees that flag, so a template without it is a
silently writable overlay on whatever partition the machine also boots from.

## Verify

```powershell
& 'C:\Program Files (x86)\WinFsp\bin\launchctl-a64.exe' list   # -> xfs-mount E
(Get-PSDrive -PSProvider FileSystem).Name                      # the new letter
Get-Content <letter>:\etc\os-release -TotalCount 1             # plain, unprivileged
```

The volume must be visible to a normal non-elevated shell -- that is the point
of going through the launcher instead of mounting inside the service's own
session. A write attempt should come back "The media is write protected".

"Nothing mounted" is a legitimate outcome, not a failure: either no supported
filesystem was found, or the XFS volume's log still needs replaying and the
driver refuses to guess. Boot Linux normally rather than hibernating to clear
the second case.

## Uninstall

```powershell
.\uninstall.ps1
```

Stops and deletes `XfsWatcher`, removes the `xfs-mount` class, removes the
installed files and drops the PATH entry -- rebuilding PATH from the existing
entries minus ours, so anything added after us survives and no other entry is
reordered. WinFsp is left alone: it may be serving sshfs-win, rclone or
another driver from this family.

## Tests

The scripts are covered by Pester specs in `tests/pester/installer.Tests.ps1`:

```powershell
pwsh -File tests/pester/run.ps1
```

They are not coverage for its own sake -- each spec is a mistake that actually
happened. A running service locking its own binary; `sc config` quoting that
works under PowerShell 7 and returns 1639 under 5.1; `[Environment]`
rewriting the system PATH from `REG_EXPAND_SZ` to `REG_SZ`; an installer that
copies a file the packager never staged; a workflow still calling
`installer/build.ps1` after it was deleted. The specs also assert that
`--ro`, the `WOW6432Node` path and delayed start are still present, because
each of those can go missing without any build failing.

`.github/workflows/release.yml` runs the same specs before packaging, and then
installs from the finished zip on the runner and uninstalls it, asserting the
service, the registry values, the PATH round trip and that `xfs.exe` executes
on the architecture it claims.

## Build

```powershell
installer\package.ps1 -Arch arm64            # cargo build, then pack
installer\package.ps1 -Arch x64 -SkipBuild   # pack an already-built exe
```

`package.ps1` refuses to package a binary whose PE `Machine` field disagrees
with `-Arch` (0xAA64 for arm64, 0x8664 for x64): a mislabelled artefact
installs cleanly and then does not run, which is the worst kind of release bug
to discover on somebody else's machine. It prints SHA-256 for the exe and the
zip. `--target` in the cargo call is load-bearing, not cosmetics -- without it
output lands in `target\release\` while packaging reads the arch directory, and
the zip then quietly ships whatever the previous `--target` build left there.
That has happened here.

**Building on this machine is currently unreliable, and that is a host
property, not a code one.** Smart App Control blocks loading
`rustc_driver-*.dll` from the toolchain (Code Integrity events 3077, policy
`{0283ac0f-fff1-49ae-ada1-8a933130cad6}`). The toolchain is unsigned
(`Get-AuthenticodeSignature` says NotSigned for both `rustc.exe` and the driver
DLL), the file has not changed since 2026-09-01, and it built fine one
afternoon -- then the next morning `rustc -vV` printed nothing, because the
reputation verdict for the same bytes flipped across a reboot. SAC is not being
turned off for this. Practical shape of the pipeline: build in CI on a runner
without SAC, use this machine to test the resulting zip, and `-SkipBuild` to
package what is already there.

## What the MSI did that this does not

The previous installer was WiX, and it also added an Explorer right-click
"Mount as xfs" verb on `.img` files, two Start Menu shortcuts, `LICENSE.txt`,
and upgrade detection against a stable `UpgradeCode`. None of that is here:
the context menu and shortcuts are convenience, and the release is not at the
point where inventing them by script beats saying which ones are missing. If
they matter, they are registry writes under `HKCR\SystemFileAssociations\.img`
and `\$\env:ProgramData\Microsoft\Windows\Start Menu\Programs`.

## Why there is no MSI here

WiX v7 -- the version these installer sources were written for -- refuses to
run at all until the Open Source Maintenance Fee EULA is accepted
(`error WIX7015`). Under its own terms the fee does not apply to this use
(section 1: only revenue-generating users at or above US\$10 000 gross annual
revenue pay; below that they are exempt), so accepting costs nothing. But it is
still a click every fork and every CI runner must make in order to produce an
artefact whose source is under the Microsoft Reciprocal License, and section 4
of the same agreement says the OSI licence governs any conflict and that
self-compiling from source needs no agreement at all. That is a poor trade for
a hobby driver.

This is a toolchain decision, not an MSI limitation, and the distinction
matters for anyone tempted to conclude MSI cannot do it:

- Delayed auto start **is** expressible in MSI -- a row in the
  `MsiServiceConfig` table (`ConfigType` =
  `SERVICE_CONFIG_DELAYED_AUTO_START` (3), `Argument` = 1) applied by the
  `MsiConfigureServices` standard action, sequenced after `InstallServices`
  and before `StartServices`, valid only for a service installed with
  `SERVICE_AUTO_START`
  ([MsiServiceConfig Table](https://learn.microsoft.com/windows/win32/msi/msiserviceconfig-table),
  [MsiConfigureServices Action](https://learn.microsoft.com/windows/win32/msi/msiconfigureservices-action)).
  What cannot express it is `ServiceInstall/@Start`, which is the actual gap in
  the old `Product.wxs`.
- Microsoft documents MSI's sibling `MsiServiceConfigFailureActions` -- service
  recovery actions -- as "not working as expected" and tells developers to run
  `sc.exe` from a custom action. `install.ps1` runs `sc.exe` for both, so it is
  not routing around a working feature.

A real MSI remains available: WiX v3.14 is MS-RL with no fee gate, its own
schema already carries `<ServiceConfig DelayedAutoStart>`, and `arm64` is a
valid package architecture there. The cost is rewriting the sources from the v4
namespace back to v3 and driving `candle`/`light` instead of `wix build`. That
is a separate decision from shipping, which is why it is not done here.

The shared templates in
[`winfsp-fs-skeleton/templates/installer/`](https://github.com/antimatter-studios/winfsp-fs-skeleton/tree/main/templates/installer)
still describe the WiX route, and ext4-win-driver and erofs-win-driver were cut
from them. Moving the family off WiX is a family decision, not a
single-repository one -- which is why those files are still there.
