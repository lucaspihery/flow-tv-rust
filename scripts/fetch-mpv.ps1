# Télécharge la dernière libmpv 64 bits (builds de shinchiro/mpv-winbuild-cmake)
# et la copie dans vendor\mpv\ ainsi que dans target\debug et target\release.
#
# Usage (PowerShell, depuis la racine du projet) :
#   powershell -ExecutionPolicy Bypass -File scripts\fetch-mpv.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root "vendor\mpv"
New-Item -ItemType Directory -Force -Path $vendor | Out-Null

Write-Host "Recherche de la dernière version de libmpv..."
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest" -Headers @{ "User-Agent" = "flow-tv" }
$asset = $release.assets | Where-Object { $_.name -match '^mpv-dev-x86_64-\d' -and $_.name -notmatch 'v3' } | Select-Object -First 1
if (-not $asset) { throw "Archive mpv-dev-x86_64 introuvable dans la dernière release." }

$archive = Join-Path $env:TEMP $asset.name
Write-Host "Téléchargement de $($asset.name)..."
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive

$extract = Join-Path $env:TEMP "flowtv-mpv"
if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
New-Item -ItemType Directory -Force -Path $extract | Out-Null

# tar.exe (inclus dans Windows 10/11) sait lire les .7z ; sinon on essaie 7-Zip.
$ok = $false
try { tar -xf $archive -C $extract; $ok = $LASTEXITCODE -eq 0 } catch { }
if (-not $ok) {
    $sevenZip = @("$env:ProgramFiles\7-Zip\7z.exe", "${env:ProgramFiles(x86)}\7-Zip\7z.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $sevenZip) { throw "Impossible d'extraire $archive : installez 7-Zip (https://www.7-zip.org) puis relancez." }
    & $sevenZip x $archive "-o$extract" -y | Out-Null
}

$dll = Get-ChildItem -Path $extract -Recurse -Filter "libmpv-2.dll" | Select-Object -First 1
if (-not $dll) { throw "libmpv-2.dll introuvable dans l'archive." }

Copy-Item $dll.FullName (Join-Path $vendor "libmpv-2.dll") -Force
foreach ($dir in @("target\debug", "target\release")) {
    $d = Join-Path $root $dir
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    Copy-Item $dll.FullName (Join-Path $d "libmpv-2.dll") -Force
}
Write-Host "libmpv-2.dll installée dans vendor\mpv, target\debug et target\release." -ForegroundColor Green
