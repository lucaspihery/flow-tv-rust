# Compile Flow TV en mode release puis génère l'installateur avec Inno Setup.
#
# Usage : powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path "vendor\mpv\libmpv-2.dll")) {
    Write-Host "libmpv-2.dll absente, téléchargement..."
    & (Join-Path $PSScriptRoot "fetch-mpv.ps1")
}

Write-Host "Compilation release..."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "La compilation a échoué." }
Copy-Item "vendor\mpv\libmpv-2.dll" "target\release\libmpv-2.dll" -Force

$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $iscc) {
    Write-Host "Inno Setup 6 introuvable : installez-le (https://jrsoftware.org/isdl.php) puis relancez." -ForegroundColor Yellow
    Write-Host "L'application est déjà utilisable dans target\release\."
    exit 0
}

& $iscc "installer\flow-tv.iss"
if ($LASTEXITCODE -ne 0) { throw "Inno Setup a échoué." }
Write-Host "Installateur créé dans installer\Output\" -ForegroundColor Green
