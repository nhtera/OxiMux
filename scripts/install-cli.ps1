<#
.SYNOPSIS
    Install the `oximux` CLI and its relay on Windows.

.DESCRIPTION
    irm https://raw.githubusercontent.com/nhtera/OxiMux/main/scripts/install-cli.ps1 | iex

    What this trusts, and what it cannot
    ------------------------------------
    The release manifest is signed with the maintainer's minisign key and this
    script carries the public half ($ReleasePublicKey below). When `minisign`
    is on PATH the signature is checked before any checksum in the manifest is
    believed — the manifest and the artifacts come from one GitHub Release, so
    a checksum alone proves only that the download matches what the publisher
    said, which a stolen publish token rewrites.

    When `minisign` is absent this falls back to the manifest's sha256 over
    TLS, says so, and continues. Windows has no Ed25519 verifier in the box —
    .NET exposes none — so there is no way to check the signature here without
    that external tool. `-RequireSignature` turns the fallback into a failure;
    CI uses it.

    The installed binary does not inherit the weakness: it carries the same key
    compiled in, and every `oximux update` afterwards verifies or refuses. This
    is trust-on-first-install and only that.

    The archive is .tar.gz on every platform, unpacked with the bsdtar that
    ships in Windows 10 1803 and later — well below the ConPTY floor this
    project already requires.

.PARAMETER Dir
    Where to install. Default: $env:LOCALAPPDATA\Programs\oximux

.PARAMETER RequireSignature
    Fail unless the manifest signature is verified.
#>
[CmdletBinding()]
param(
    [string] $Dir = $(if ($env:OXIMUX_INSTALL_DIR) { $env:OXIMUX_INSTALL_DIR }
                      else { Join-Path $env:LOCALAPPDATA 'Programs\oximux' }),
    [switch] $RequireSignature
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'nhtera/OxiMux'
# Overridable so the installer itself can be tested against a local fake
# release. Unlike the compiled updater — whose equivalent override is
# debug-build-only, because a release binary must carry no way to repoint its
# own trust chain — this costs nothing: anyone who can set an environment
# variable for this script can equally well edit the script.
$BaseUrl = if ($env:OXIMUX_INSTALL_BASE_URL) { $env:OXIMUX_INSTALL_BASE_URL }
           else { "https://github.com/$Repo/releases" }
$Latest = "$BaseUrl/latest/download"

# release-pubkey: the base64 body of the minisign .pub file. Managed by
# scripts/gen-release-key.sh; packaging/release-pubkey.txt is the source of
# truth and apps/cli/tests/update_e2e.rs fails the build if they drift.
$ReleasePublicKey = 'RWQ4owMUFazkg7fHezLB688BjTGDGJBBQ4EPLVbLDp8baal1VsMJ71FJ'

function Die([string] $Message) {
    Write-Error "oximux: $Message"
    exit 1
}

function Get-Target {
    # PROCESSOR_ARCHITECTURE is the *process* view and reads AMD64 under
    # WOW64; the OS architecture is what picks the right asset.
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        'X64'   { return 'x86_64-pc-windows-msvc' }
        'Arm64' { return 'aarch64-pc-windows-msvc' }
        default { Die "unsupported architecture: $arch" }
    }
}

$target = Get-Target
Write-Host "oximux: installing for $target"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("oximux-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $manifestPath = Join-Path $tmp 'manifest.json'
    try {
        Invoke-WebRequest -Uri "$Latest/manifest.json" -OutFile $manifestPath -UseBasicParsing
    } catch {
        Die "could not download the release manifest - is there a published release? ($_)"
    }

    # --- the signature, before anything in the manifest is believed ---------

    $minisign = Get-Command minisign -ErrorAction SilentlyContinue
    if ($ReleasePublicKey -eq 'UNSET') {
        if ($RequireSignature) {
            Die 'this installer carries no release key, so -RequireSignature cannot be satisfied'
        }
        Write-Warning 'oximux: this installer carries no release key; falling back to checksum-only trust.'
    } elseif ($minisign) {
        $sigPath = Join-Path $tmp 'manifest.json.minisig'
        try {
            Invoke-WebRequest -Uri "$Latest/manifest.json.minisig" -OutFile $sigPath -UseBasicParsing
        } catch {
            Die 'the release has no manifest signature'
        }
        $pubPath = Join-Path $tmp 'release.pub'
        # ASCII with a trailing newline: minisign parses this file by line.
        [System.IO.File]::WriteAllText(
            $pubPath, "untrusted comment: oximux release key`n$ReleasePublicKey`n",
            [System.Text.Encoding]::ASCII)
        & $minisign.Source -V -p $pubPath -x $sigPath -m $manifestPath | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Die ("the release manifest signature did NOT verify. Do not retry blindly - this is " +
                 "what a tampered release looks like. Check https://github.com/$Repo/releases")
        }
        Write-Host 'oximux: manifest signature verified'
    } elseif ($RequireSignature) {
        Die 'minisign is not installed and -RequireSignature was given (winget install jedisct1.minisign)'
    } else {
        Write-Warning ('oximux: minisign is not installed, so the release signature was NOT checked. ' +
                       'Falling back to the manifest sha256 over TLS. Install minisign and re-run ' +
                       'with -RequireSignature for the full check.')
    }

    # --- the asset ----------------------------------------------------------

    $manifest = Get-Content -Raw -Path $manifestPath | ConvertFrom-Json
    # @(...) forces an array: with a single-target manifest `.Name` is a bare
    # string, and `-notcontains` would silently degrade to a substring test.
    $targetNames = @($manifest.targets.PSObject.Properties.Name)
    if ($targetNames -notcontains $target) {
        Die "this release has no build for $target (it has: $($targetNames -join ', '))"
    }
    $asset = $manifest.targets.$target
    $version = $manifest.version

    # The name becomes a path component below. The Rust parser refuses these
    # too; an installer that skipped the check would be the weaker reader.
    if ($asset.archive -match '[\\/]' -or $asset.archive -match '\.\.' -or
        $asset.archive.StartsWith('-') -or [string]::IsNullOrEmpty($asset.archive)) {
        Die "the manifest names an unsafe archive path: $($asset.archive)"
    }

    Write-Host "oximux: downloading $version ($($asset.archive))"
    $archivePath = Join-Path $tmp $asset.archive
    # Built from the *signed* version and file name rather than read out of the
    # manifest as a URL, so a manifest can never name a download host of its own.
    try {
        Invoke-WebRequest -Uri "$BaseUrl/download/v$version/$($asset.archive)" `
            -OutFile $archivePath -UseBasicParsing
    } catch {
        Die "could not download $($asset.archive) ($_)"
    }

    $got = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash.ToLowerInvariant()
    $want = $asset.sha256.ToLowerInvariant()
    if ($got -ne $want) {
        Die "checksum mismatch for $($asset.archive): expected $want, got $got"
    }
    Write-Host 'oximux: checksum ok'

    # --- install -------------------------------------------------------------

    $unpack = Join-Path $tmp 'unpack'
    New-Item -ItemType Directory -Path $unpack -Force | Out-Null
    & tar.exe -xzf $archivePath -C $unpack
    if ($LASTEXITCODE -ne 0) { Die "could not unpack $($asset.archive)" }

    $binaries = @('oximux.exe', 'oximux-relay.exe')
    foreach ($bin in $binaries) {
        if (-not (Test-Path (Join-Path $unpack $bin))) {
            Die "$($asset.archive) does not contain $bin"
        }
    }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null

    # Absolute from here on: the PATH block below writes this into the user's
    # persistent Path, and a relative -Dir would land there literally — an
    # entry that resolves differently from every working directory.
    $Dir = (Resolve-Path -LiteralPath $Dir).Path

    # Both binaries move together, or neither does.
    #
    # The CLI and the relay speak a handshake versioned in lockstep: an install
    # that lands one and not the other leaves an installation that cannot talk
    # to itself. Moving them in a plain loop produces exactly that whenever the
    # second fails, so this is the same two-pass swap-with-rollback that
    # `oximux update` performs.
    #
    # Staging INSIDE the install directory is load-bearing: a rename is atomic
    # only within one filesystem, and %TEMP% is frequently a different volume.
    $stage = Join-Path $Dir (".oximux-install-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    try {
        foreach ($bin in $binaries) {
            Copy-Item (Join-Path $unpack $bin) (Join-Path $stage $bin) -Force
        }

        # Pass 1: vacate the installed names. A running .exe cannot be
        # overwritten but its name CAN be vacated.
        #
        # The backups go in the INSTALL directory, not the staging one, and are
        # named `<bin>.old-<hex>` — the shape `sweep_backups` in the CLI looks
        # for at every start. On Windows the backup of a running binary cannot
        # be deleted here at all (its image is still mapped), so it has to
        # survive under a name something else will collect; leaving it inside a
        # staging directory would strand it where nothing ever looks.
        $suffix = '{0:x8}' -f (Get-Random -Minimum 0 -Maximum 2147483647)
        $moved = @()
        foreach ($bin in $binaries) {
            $dest = Join-Path $Dir $bin
            if (-not (Test-Path $dest)) { continue }
            $backup = "$dest.old-$suffix"
            try {
                Move-Item -Force $dest $backup
                $moved += @{ Name = $bin; Backup = $backup }
            } catch {
                foreach ($m in $moved) {
                    Move-Item -Force $m.Backup (Join-Path $Dir $m.Name) -ErrorAction SilentlyContinue
                }
                Die "could not replace $dest - is oximux still running? ($_)"
            }
        }

        # Pass 2: move the new binaries into the vacated names. Undo pass 2
        # first on failure, so pass 1's undo finds its destinations free.
        $placed = @()
        foreach ($bin in $binaries) {
            try {
                Move-Item -Force (Join-Path $stage $bin) (Join-Path $Dir $bin)
                $placed += $bin
            } catch {
                foreach ($p in $placed) {
                    Remove-Item -Force (Join-Path $Dir $p) -ErrorAction SilentlyContinue
                }
                foreach ($m in $moved) {
                    Move-Item -Force $m.Backup (Join-Path $Dir $m.Name) -ErrorAction SilentlyContinue
                }
                Die "could not install into $Dir - no write access? ($_)"
            }
        }

        # Best-effort: a backup still mapped into a running process cannot be
        # deleted, and that is not a reason to fail an install that succeeded.
        # The CLI sweeps whatever is left at its next start.
        foreach ($m in $moved) {
            Remove-Item -Force $m.Backup -ErrorAction SilentlyContinue
        }
    } finally {
        Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    }

    Write-Host "oximux: installed $version to $Dir"

    # --- PATH ----------------------------------------------------------------

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $onPath = $userPath.Split(';') | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }
    if (-not $onPath) {
        $newPath = if ($userPath) { "$userPath;$Dir" } else { $Dir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "oximux: added $Dir to your user PATH - open a new terminal to pick it up"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
