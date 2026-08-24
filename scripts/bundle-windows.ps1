<#
.SYNOPSIS
    Build and lay out the Windows OxiMux app directory.

.DESCRIPTION
    The Windows counterpart of scripts/bundle-macos.sh, and a much smaller
    script, because Windows has no bundle format to satisfy: an "app" here is a
    directory whose contents sit beside the executable. That flatness is the
    whole design. Everything OxiMux resolves at runtime, it resolves as a
    *sibling of the running exe*, so getting the layout right is the entire job.

    Output: dist/OxiMux/
      -Zip        also dist/OxiMux-<version>-windows-<arch>.zip
      -Installer  also dist/OxiMux-<version>-x64-setup.exe

    What ships, and what breaks without it:

      oximux.exe              the app.
      oximux-relay.exe        the PTY relay daemon. Missing, every terminal
                              falls back to an in-process backend that dies on
                              quit, so sessions never survive a relaunch.
      oximux-screen-gate.exe  the PreToolUse hook that decides an agent's
                              screen-control calls. Missing, every chat runs
                              unenforced: one warning in the log and otherwise
                              indistinguishable from normal. On Windows this is
                              the one that matters most, because synthesizing
                              input needs no permission there, so the hook is
                              the only thing watching. See
                              docs/windows-port-exclusions.md.
      rg.exe                  ripgrep, for the search panel and Quick Open.
                              Missing, search reports "ripgrep missing".
      *.dll                   the voice-dictation engine (onnxruntime +
                              sherpa-onnx). Missing, the app fails to start:
                              Windows resolves these at load time, not on first
                              use, so there is no degraded mode to fall back to.

    There is no signing step. Windows code signing needs a certificate from a
    CA; it cannot be self-issued the way an ad-hoc macOS signature can, so an
    unsigned build is the honest default rather than a placeholder. A signed
    release would add `signtool sign /fd sha256 /tr <timestamp-url> ...` over
    every .exe below, nested-first, before zipping. An installer produced by
    -Installer is not exempt: SmartScreen warns on the setup.exe exactly as it
    warns on an exe extracted from the zip, so the installer is a convenience,
    not a trust story. Signing would cover the setup.exe too, and must run over
    the payload *before* iscc reads it as well as over the setup.exe after.

    NOTE: this file is deliberately pure ASCII. Windows PowerShell 5.1 reads a
    .ps1 with no byte-order mark as ANSI, so a UTF-8 em dash in a comment turns
    into three characters and the parse fails several lines later with an error
    that names the wrong token entirely.

.PARAMETER Profile
    'release' (default) or 'debug'.

.PARAMETER Target
    Optional cargo target triple, e.g. x86_64-pc-windows-msvc. Omit for a plain
    host build. Only affects where artifacts are read from.

.PARAMETER Zip
    Also produce dist/OxiMux-<version>-windows-<arch>.zip.

.PARAMETER Installer
    Also compile packaging/windows/oximux.iss into
    dist/OxiMux-<version>-x64-setup.exe. Needs Inno Setup 6.3 or newer on the
    machine (`winget install JRSoftware.InnoSetup`); it is preinstalled on
    GitHub's windows-latest runners.

.PARAMETER SkipBuild
    Assemble from whatever is already in target/. For the inner loop; never for
    a release.

.EXAMPLE
    ./scripts/bundle-windows.ps1
.EXAMPLE
    ./scripts/bundle-windows.ps1 -Profile debug -SkipBuild
.EXAMPLE
    ./scripts/bundle-windows.ps1 -Target x86_64-pc-windows-msvc -Zip -Installer
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',
    [string]$Target,
    [switch]$Zip,
    [switch]$Installer,
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    $AppDir = Join-Path $RepoRoot 'dist/OxiMux'

    # Cargo puts artifacts under target/<triple>/<profile> when --target is
    # passed and target/<profile> when it is not. Reading the wrong one is how
    # a "successful" bundle ends up carrying yesterday's binaries.
    $ArtifactDir = if ($Target) {
        Join-Path $RepoRoot "target/$Target/$Profile"
    } else {
        Join-Path $RepoRoot "target/$Profile"
    }

    $CargoFlags = @()
    if ($Profile -eq 'release') { $CargoFlags += '--release' }
    if ($Target) { $CargoFlags += @('--target', $Target) }

    if (-not $SkipBuild) {
        Write-Host "==> Building oximux + oximux-relay + oximux-screen-gate ($Profile)"
        # One cargo invocation, not three: the three binaries share almost their
        # whole dependency graph, and three invocations serialise what cargo
        # would otherwise overlap.
        & cargo build @CargoFlags `
            -p oximux-app --bin oximux `
            -p oximux-relay --bin oximux-relay `
            -p oximux-computer-use --bin oximux-screen-gate
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    }

    Write-Host "==> Assembling $AppDir"
    # A previous bundle still running holds its DLLs mapped, and Windows refuses
    # to delete a mapped image. Left to Remove-Item that surfaces as
    # "Access to the path 'onnxruntime.dll' is denied" - which names a file
    # nobody touched and says nothing about the app in the taskbar. Check for
    # the process instead, and say what to do about it.
    $running = @(Get-Process -Name 'oximux', 'oximux-relay' -ErrorAction SilentlyContinue |
                 Where-Object { $_.Path -and $_.Path.StartsWith($AppDir, [StringComparison]::OrdinalIgnoreCase) })
    if ($running) {
        $list = ($running | ForEach-Object { "$($_.ProcessName) (pid $($_.Id))" }) -join ', '
        throw @"
$AppDir is in use by: $list
Windows cannot replace a running executable or a mapped DLL. Quit that OxiMux
(and its relay daemon, which outlives the app on purpose) and re-run:
  Get-Process oximux, oximux-relay | Stop-Process
"@
    }
    if (Test-Path $AppDir) { Remove-Item -Recurse -Force $AppDir }
    New-Item -ItemType Directory -Force -Path $AppDir | Out-Null

    foreach ($exe in 'oximux.exe', 'oximux-relay.exe', 'oximux-screen-gate.exe') {
        $src = Join-Path $ArtifactDir $exe
        if (-not (Test-Path $src)) {
            throw "$exe is missing from $ArtifactDir. Build it, or drop -SkipBuild."
        }
        Copy-Item -Path $src -Destination (Join-Path $AppDir $exe) -Force
    }
    Write-Host '==> Copied 3 executables'

    # The dictation engine's native libraries. Cargo stages every dynamic native
    # dependency at the profile root, so a glob is both complete and
    # self-maintaining: an added or renamed library rides along without a hand
    # edit here. The assertion below is what keeps the glob honest, because a
    # silently empty copy is the failure mode and it does not surface until
    # launch.
    $dlls = @(Get-ChildItem -Path (Join-Path $ArtifactDir '*.dll') -File -ErrorAction SilentlyContinue)
    foreach ($dll in $dlls) {
        Copy-Item -Path $dll.FullName -Destination (Join-Path $AppDir $dll.Name) -Force
    }
    foreach ($required in 'onnxruntime.dll', 'sherpa-onnx-c-api.dll') {
        if (-not (Test-Path (Join-Path $AppDir $required))) {
            throw @"
$required was not staged in $ArtifactDir.
The dictation engine links it at load time, so this bundle would fail to start
rather than start without dictation. Run a full 'cargo build' first.
"@
        }
    }
    Write-Host "==> Copied $($dlls.Count) native libraries"

    # Pinned ripgrep. Cache-aware, so a warm checkout does not hit the network.
    # Failures propagate as terminating errors (that script sets its own
    # ErrorActionPreference), so there is no exit code to inspect. A silent
    # no-op would leave a previous rg.exe in place, hence the existence check
    # rather than a trusted return.
    & (Join-Path $PSScriptRoot 'fetch-ripgrep.ps1')
    $rg = Join-Path $RepoRoot 'target/bundle-tools/rg.exe'
    if (-not (Test-Path $rg)) { throw "fetch-ripgrep.ps1 produced no $rg" }
    Copy-Item -Path $rg -Destination (Join-Path $AppDir 'rg.exe') -Force
    Write-Host '==> Bundled rg.exe'

    # The application icon is embedded by apps/desktop/build.rs, so nothing is
    # copied for it. But "nothing to copy" and "nothing embedded" look the same
    # from here, and a missing icon is invisible until someone sees a blank
    # taskbar entry, so ask the loader whether the resource is actually in there.
    $iconProbe = @'
using System;
using System.Runtime.InteropServices;
public static class OxiMuxIconProbe {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int PrivateExtractIconsW(
        string file, int index, int cx, int cy, IntPtr[] icons, int[] ids, int count, int flags);
}
'@
    if (-not ('OxiMuxIconProbe' -as [type])) { Add-Type -TypeDefinition $iconProbe }
    # Null arrays with a count of 0 is the documented "just count them" call.
    $found = [OxiMuxIconProbe]::PrivateExtractIconsW(
        (Join-Path $AppDir 'oximux.exe'), 0, 0, 0, $null, $null, 0, 0)
    if ($found -lt 1) {
        throw @"
oximux.exe carries no icon resource.
apps/desktop/build.rs should have embedded assets/windows/OxiMux.ico at id 1.
Check that a resource compiler was on PATH during the build; embed-resource
treats a missing one as optional and says so only in the build log.
"@
    }
    Write-Host "==> Icon resource present ($found icon group)"

    $version = (Select-String -Path (Join-Path $RepoRoot 'Cargo.toml') `
                              -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        'ARM64' { 'arm64' }
        default { 'x64' }
    }

    if ($Zip) {
        $zipPath = Join-Path $RepoRoot "dist/OxiMux-$version-windows-$arch.zip"
        if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
        # The directory, not its contents: extracting has to produce an
        # `OxiMux\` folder. Zipping `$AppDir\*` instead scatters nine loose
        # files into whatever folder the user extracted into, and the app still
        # runs from there - which is worse than failing, because it looks fine.
        Compress-Archive -Path $AppDir -DestinationPath $zipPath
        Write-Host "==> $zipPath ready"
    }

    if ($Installer) {
        # The .iss declares ArchitecturesAllowed=x64compatible and names its
        # output -x64-setup, so an arm64 payload would ship under a filename
        # that lies about it and then refuse to install natively. Refuse here
        # instead, where the message can say why.
        if ($arch -ne 'x64') {
            throw @"
-Installer only supports an x64 payload (this host reports $arch).
packaging/windows/oximux.iss is x64-only, and release.yml builds
x86_64-pc-windows-msvc only. Cross-bundle with -Target x86_64-pc-windows-msvc,
or teach the .iss an arm64 variant first.
"@
        }

        # Inno Setup's compiler. On GitHub's windows-latest it is preinstalled
        # and on PATH; a developer install does not touch PATH at all, so look
        # in the three places it lands rather than telling most callers their
        # install is broken.
        #
        # %LOCALAPPDATA%\Programs is first and is not a fallback: `winget
        # install JRSoftware.InnoSetup` runs the installer unelevated, and Inno
        # Setup's own installer then defaults to a per-user install there. The
        # Program Files entries only cover someone who ran the .exe as admin.
        #
        # String concatenation, not Join-Path: ProgramFiles(x86) is absent on a
        # 32-bit host and Join-Path throws on a null base, which would report a
        # missing environment variable as a bug in this script.
        $candidates = @("$env:LOCALAPPDATA\Programs", ${env:ProgramFiles(x86)}, $env:ProgramFiles) |
            Where-Object { $_ } |
            ForEach-Object { "$_\Inno Setup 6\ISCC.exe" }
        # Two statements, not `(Get-Command ...).Source`: under Set-StrictMode
        # -Version Latest, dereferencing a property on the $null a missed
        # lookup returns is a terminating error, so the one-liner turns "Inno
        # Setup is not installed" into "The property 'Source' cannot be found
        # on this object". CI never sees it - iscc is on PATH there - so it
        # would only ever fail on a developer machine.
        $onPath = Get-Command 'iscc.exe' -ErrorAction SilentlyContinue
        $iscc = if ($onPath) { $onPath.Source }
                else { $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1 }
        if (-not $iscc) {
            throw @"
ISCC.exe (the Inno Setup compiler) was not found.
Install it with:  winget install JRSoftware.InnoSetup
Version 6.3 or newer is required - the .iss uses ArchitecturesAllowed=x64compatible,
which older compilers reject as an unknown value.
Looked on PATH and in: $($candidates -join ', ')
"@
        }

        $iss = Join-Path $RepoRoot 'packaging/windows/oximux.iss'
        $setupPath = Join-Path $RepoRoot "dist/OxiMux-$version-x64-setup.exe"
        if (Test-Path $setupPath) { Remove-Item -Force $setupPath }

        # Backslashes throughout: $AppDir carries a forward slash from the
        # Join-Path literal above, and while Windows tolerates the mix, an .iss
        # that echoes the path into an error message should not.
        $payload = $AppDir.Replace('/', '\')
        $outDir = (Join-Path $RepoRoot 'dist').Replace('/', '\')

        # /Q keeps the compile quiet on success and still prints errors. The
        # payload is $AppDir - the directory this script just assembled and
        # asserted - so the .iss never has to know the file list.
        & $iscc /Q "/DAppVersion=$version" "/DSourceDir=$payload" `
                "/DOutputDir=$outDir" $iss
        if ($LASTEXITCODE -ne 0) { throw "iscc failed with exit code $LASTEXITCODE" }
        # iscc reports success even when OutputBaseFilename resolved to
        # something other than what we expect, and the release job globs for
        # this exact name, so check rather than trust.
        if (-not (Test-Path $setupPath)) {
            throw "iscc reported success but $setupPath is not there - did OutputBaseFilename change?"
        }
        $setupMb = [math]::Round(((Get-Item $setupPath).Length / 1MB), 1)
        Write-Host "==> $setupPath ready ($setupMb MB, unsigned)"
    }

    $size = [math]::Round(((Get-ChildItem $AppDir -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
    Write-Host "==> $AppDir ready (v$version, $arch, $size MB)"
    Write-Host "    $(Join-Path $AppDir 'oximux.exe')    # to launch"
}
catch {
    # `powershell.exe -File script.ps1` exits 0 even when the script throws, so
    # without this a caller that chains on success — CI, or a shell doing
    # `bundle && launch` — treats a failed build as a good bundle and carries on
    # with whatever stale binaries were already in dist/. Observed exactly that:
    # the cargo step failed, the script correctly refused to assemble, and the
    # caller launched the previous build anyway.
    Write-Host "bundle-windows failed: $($_.Exception.Message)"
    exit 1
}
finally {
    Pop-Location
}
