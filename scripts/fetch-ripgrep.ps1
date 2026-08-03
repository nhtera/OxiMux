<#
.SYNOPSIS
    Fetch a pinned, checksum-verified ripgrep for bundling into the Windows app.

.DESCRIPTION
    The Windows counterpart of scripts/fetch-ripgrep.sh. The packaged app spawns
    `rg` for the search panel and Quick Open; bundling it means a fresh machine
    needs no manual install.

    ripgrep is MIT/Unlicense dual-licensed, so redistribution inside the app
    directory is permitted (VS Code ships rg the same way).

    Output: target/bundle-tools/rg.exe (+ rg.version stamp)

    Caching: if the binary exists and the stamp matches the pinned version and
    arch, this is a no-op, so offline rebuilds keep working once the tool has
    been fetched. A checksum mismatch is always fatal: a bundle whose search
    binary came from a truncated or tampered download is worse than a build that
    stopped.

    The pinned version is kept in step with fetch-ripgrep.sh by hand. They are
    separate files because the release assets, the archive format, and the
    checksum plumbing all differ; a shared version would have to live in a third
    file that neither script naturally reads.

    NOTE: this file is deliberately pure ASCII. Windows PowerShell 5.1 reads a
    .ps1 with no byte-order mark as ANSI, so a UTF-8 em dash in a comment turns
    into three characters and the parse fails several lines later with an error
    that names the wrong token entirely.

.PARAMETER Arch
    ripgrep release-triple arch component. Defaults to the host's.
#>
[CmdletBinding()]
param(
    [ValidateSet('x86_64', 'aarch64')]
    [string]$Arch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RgVersion = '15.2.0'
$BaseUrl = "https://github.com/BurntSushi/ripgrep/releases/download/$RgVersion"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $RepoRoot 'target/bundle-tools'
$OutBin = Join-Path $OutDir 'rg.exe'
$Stamp = Join-Path $OutDir 'rg.version'

if (-not $Arch) {
    $Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x86_64' }
        'ARM64' { 'aarch64' }
        default { throw "unsupported host arch '$env:PROCESSOR_ARCHITECTURE' for bundled rg" }
    }
}

$Want = "ripgrep $RgVersion [$Arch]"

if ((Test-Path $OutBin) -and (Test-Path $Stamp) -and ((Get-Content $Stamp -Raw).Trim() -eq $Want)) {
    Write-Host "==> Bundled rg up to date ($Want), skipping fetch"
    exit 0
}

$Name = "ripgrep-$RgVersion-$Arch-pc-windows-msvc"
$Zip = "$Name.zip"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Work = Join-Path ([System.IO.Path]::GetTempPath()) ("oximux-rg-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $Work | Out-Null

try {
    Write-Host "==> Fetching $Zip"
    $ZipPath = Join-Path $Work $Zip
    # -UseBasicParsing keeps this working on a machine where Internet Explorer's
    # first-run wizard has never been dismissed, which is most CI images.
    Invoke-WebRequest -Uri "$BaseUrl/$Zip" -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri "$BaseUrl/$Zip.sha256" -OutFile "$ZipPath.sha256" -UseBasicParsing

    # ripgrep's Windows .sha256 assets are certutil output, not the GNU
    # "<hex>  <filename>" line the .sh counterpart parses:
    #
    #   SHA256 hash of ripgrep-15.2.0-x86_64-pc-windows-msvc.zip:
    #   71b2fef8...
    #   CertUtil: -hashfile command completed successfully.
    #
    # Matching the hex itself rather than a field position reads both layouts,
    # and cannot quietly succeed against a header word the way splitting on
    # whitespace did.
    $ChecksumText = Get-Content "$ZipPath.sha256" -Raw
    $Match = [regex]::Match($ChecksumText, '(?m)^\s*([0-9a-fA-F]{64})\s*$')
    if (-not $Match.Success) {
        throw "no SHA-256 digest found in $Zip.sha256 - release layout changed?`n$ChecksumText"
    }
    $Expected = $Match.Groups[1].Value
    $Actual = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash
    if ($Actual -ine $Expected) {
        throw "checksum mismatch for ${Zip}: expected $Expected, got $Actual"
    }
    Write-Host "==> Checksum OK ($Expected)"

    Expand-Archive -Path $ZipPath -DestinationPath $Work -Force
    $Extracted = Join-Path $Work "$Name/rg.exe"
    if (-not (Test-Path $Extracted)) {
        throw "$Zip did not contain $Name/rg.exe - release layout changed?"
    }

    Copy-Item -Path $Extracted -Destination $OutBin -Force
    # Written last, and only after the copy: a stamp that lands before the
    # binary would make a failed run look cached on the next one.
    Set-Content -Path $Stamp -Value $Want -Encoding utf8
    Write-Host "==> $OutBin ready ($Want)"
}
finally {
    Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}
