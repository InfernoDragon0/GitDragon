# Packages the release build for Windows: portable zip, symbols zip, optional Inno Setup installer, SHA256SUMS.
# Local only: nothing is uploaded or signed unless the signing hook below is enabled.
#
# Usage (from the repo root, after `cargo build --release --locked -p gitdragon`):
#   powershell -File packaging/windows/build.ps1 [-OutDir dist] [-SkipInstaller]
param(
    [string]$Exe = "target/release/gitdragon.exe",
    [string]$OutDir = "dist",
    [switch]$SkipInstaller
)
$ErrorActionPreference = "Stop"
if (-not (Test-Path $Exe)) { throw "No binary at $Exe; run: cargo build --release --locked -p gitdragon" }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "gitdragon-$version-windows-x64"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$stage = Join-Path $OutDir $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory $stage | Out-Null

# --- Signing hook (disabled): Authenticode-sign the exe BEFORE it is zipped or wrapped in the installer.
# Needs a certificate; see docs/release.md. Enable with GITDRAGON_SIGN=1 and signtool on PATH.
function Invoke-Sign([string]$path) {
    if ($env:GITDRAGON_SIGN -ne "1") { return }
    # TODO(signing): adapt once a certificate exists (cert thumbprint / Azure Trusted Signing / HSM token).
    signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /sha1 $env:GITDRAGON_SIGN_THUMBPRINT $path
    if ($LASTEXITCODE) { throw "signtool failed for $path" }
}

Copy-Item $Exe $stage
Invoke-Sign (Join-Path $stage "gitdragon.exe")
if (Test-Path README.md) { Copy-Item README.md $stage }
Copy-Item LICENSE-MIT, LICENSE-APACHE $stage

$zip = Join-Path $OutDir "$name-portable.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item -Recurse -Force $stage

# Debug symbols travel separately so the exe stays small; keep them to symbolicate crash reports.
$pdb = [IO.Path]::ChangeExtension($Exe, ".pdb")
if (Test-Path $pdb) {
    $sym = Join-Path $OutDir "$name-symbols.zip"
    if (Test-Path $sym) { Remove-Item $sym }
    Compress-Archive -Path $pdb -DestinationPath $sym
}

# --- Installer (Inno Setup 6). Skipped with a hint when iscc is not installed.
if (-not $SkipInstaller) {
    $iscc = (Get-Command iscc -ErrorAction SilentlyContinue).Source
    if (-not $iscc) {
        $iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
            Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if ($iscc) {
        $exeAbs = (Resolve-Path $Exe).Path
        $outAbs = (Resolve-Path $OutDir).Path
        & $iscc "/DAppVersion=$version" "/DSourceExe=$exeAbs" "/DOutDir=$outAbs" "$PSScriptRoot\gitdragon.iss"
        if ($LASTEXITCODE) { throw "iscc failed" }
        Invoke-Sign (Join-Path $OutDir "GitDragon-$version-windows-x64-setup.exe")
    } else {
        Write-Warning "Inno Setup not found; installer skipped. Install once with: winget install JRSoftware.InnoSetup, then re-run this script."
    }
}

# --- Checksums. TODO(signing): also produce a detached signature (minisign / cosign) over this file in CI.
$sums = Get-ChildItem $OutDir -File | Where-Object { $_.Name -match '\.(zip|exe)$' } | Sort-Object Name |
    ForEach-Object { "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name }
$sums | Set-Content -Encoding ascii (Join-Path $OutDir "SHA256SUMS-windows.txt")
$sums
