# Builds the release binary and assembles a portable folder + zip under dist/.
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1 [-DryRun] [-Installer]
[CmdletBinding()]
param([switch]$DryRun, [switch]$Installer)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$manifest = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
$section = [regex]::Match($manifest, '(?ms)^\[workspace\.package\]\s*(.*?)(?=^\[|\z)')
$ver = [regex]::Match($section.Groups[1].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $section.Success -or -not $ver.Success) {
    throw 'Could not read version from [workspace.package] in Cargo.toml'
}
$version = $ver.Groups[1].Value

$name = "WaClient-$version"
$dist = Join-Path $root 'dist'
$folder = Join-Path $dist $name
$zip = Join-Path $dist "$name-win-x64.zip"
$exe = Join-Path $root 'target\release\waclient.exe'
$iss = Join-Path $root 'installer\waclient.iss'
$setup = Join-Path $dist "$name-setup.exe"

function Find-Iscc {
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
    )
    foreach ($c in $candidates) { if ($c -and (Test-Path -LiteralPath $c)) { return $c } }
    return $null
}

$notice = @"
WaClient $version - third-party notices

This distribution bundles or statically links:
  - libopus (BSD 3-clause license), built from source via audiopus_sys.
  - SQLite (public domain), bundled through rusqlite.
  - Slint UI toolkit (https://slint.dev), licensed as
    GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0.
    Implication: redistributing this binary requires choosing one option. The royalty-free
    license is free for desktop/mobile/web apps but requires visible attribution to Slint
    (e.g. an About screen or the download page); GPLv3 requires this app to be GPLv3 with
    source available; otherwise a commercial license is needed. Check the terms at
    https://slint.dev/terms-and-conditions.
  - Many other Rust crates under permissive licenses (see Cargo.lock).

WaClient is unofficial and not affiliated with WhatsApp or Meta. Using it may violate the
WhatsApp Terms of Service and can lead to account bans.
"@

function Step($text) { Write-Host "==> $text" }

Step "Version: $version"
Step "cargo build --release -p waclient (in $root)"
Step "Create $folder"
Step "Copy $exe -> $folder\WaClient.exe"
Step "Copy README.md -> $folder"
Step "Write $folder\LICENSE-NOTICE.txt"
Step "Compress $folder -> $zip"
if ($Installer) {
    Step "Locate ISCC.exe (Inno Setup 6); if missing, print 'winget install JRSoftware.InnoSetup' and skip"
    Step "ISCC /DAppVersion=$version /DSourceExe=$exe /DOutDir=$dist $iss -> $setup"
}

if ($DryRun) {
    Write-Host 'Dry run: nothing was built or written.'
    return
}

Push-Location $root
try {
    & cargo build --release -p waclient
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}
if (-not (Test-Path -LiteralPath $exe)) { throw "Build output not found: $exe" }

if (Test-Path -LiteralPath $folder) { Remove-Item -Recurse -Force -LiteralPath $folder }
if (Test-Path -LiteralPath $zip) { Remove-Item -Force -LiteralPath $zip }
New-Item -ItemType Directory -Force -Path $folder | Out-Null

Copy-Item -LiteralPath $exe -Destination (Join-Path $folder 'WaClient.exe')
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $folder
Set-Content -LiteralPath (Join-Path $folder 'LICENSE-NOTICE.txt') -Value $notice -Encoding UTF8

Compress-Archive -Path $folder -DestinationPath $zip -Force
Write-Host "Done: $zip"

if ($Installer) {
    $iscc = Find-Iscc
    if (-not $iscc) {
        Write-Warning 'Inno Setup 6 not found; skipping the installer. Install it with: winget install JRSoftware.InnoSetup'
    } else {
        & $iscc "/DAppVersion=$version" "/DSourceExe=$exe" "/DOutDir=$dist" $iss
        if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
        Write-Host "Done: $setup"
    }
}
