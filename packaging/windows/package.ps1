#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Build and package Nieuwser for Windows.

.DESCRIPTION
  Produces, in $env:DIST (default: dist\release):
    nieuwser-<version>-windows-x64.msi            per-machine installer (WiX v5, unsigned)
    nieuwser-<version>-windows-x64-portable.zip   nieuwser.exe + nieuwser-cli.exe + README.md

  Needs Rust (MSVC toolchain) and WiX v5:
    dotnet tool install --global wix --version 5.0.2

  No signing certificate is used. The MSI and executables are unsigned and will trigger a
  SmartScreen/unknown-publisher warning.

.EXAMPLE
  pwsh packaging/windows/package.ps1
  pwsh packaging/windows/package.ps1 -SkipBuild
#>
param(
  [ValidateSet('x64')] [string] $Arch = 'x64',
  [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Version = $env:NIEUWSER_VERSION
if (-not $Version) {
  $inPkg = $false
  foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
    if ($line -match '^\[workspace\.package\]') { $inPkg = $true; continue }
    if ($line -match '^\[') { $inPkg = $false }
    if ($inPkg -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw 'could not read [workspace.package] version from Cargo.toml' }

$Dist = if ($env:DIST) { $env:DIST } else { Join-Path $Root 'dist\release' }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
$Base = "nieuwser-$Version-windows-$Arch"
$Target = 'x86_64-pc-windows-msvc'
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$Bin = Join-Path $TargetDir "$Target\release"

if (-not $SkipBuild) {
  Push-Location $Root
  try {
    & cargo build --release --locked -p nieuwser -p nieuwser-cli --target $Target
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
  } finally {
    Pop-Location
  }
}
if (-not (Test-Path (Join-Path $Bin 'nieuwser.exe'))) { throw "missing $Bin\nieuwser.exe" }

$Work = Join-Path $TargetDir 'windows-package'
if (Test-Path $Work) { Remove-Item -Recurse -Force $Work }
$Portable = Join-Path $Work 'portable'
New-Item -ItemType Directory -Force -Path $Portable | Out-Null
Copy-Item (Join-Path $Bin 'nieuwser.exe') $Portable
Copy-Item (Join-Path $Bin 'nieuwser-cli.exe') $Portable
foreach ($doc in 'README.md', 'LICENSE', 'LICENSE-MIT', 'LICENSE-APACHE', 'COPYRIGHT') {
  $src = Join-Path $Root $doc
  if (Test-Path $src) { Copy-Item $src $Portable }
}

$Zip = Join-Path $Dist "$Base-portable.zip"
Add-Type -AssemblyName System.IO.Compression.FileSystem
if (Test-Path $Zip) { Remove-Item -Force $Zip }
[System.IO.Compression.ZipFile]::CreateFromDirectory(
  $Portable, $Zip, [System.IO.Compression.CompressionLevel]::Optimal, $false)
Write-Output "wrote $Zip"

$Wix = Get-Command wix -ErrorAction SilentlyContinue
if (-not $Wix) {
  throw 'WiX not found. Install it with: dotnet tool install --global wix --version 5.0.2'
}
# MSI ProductVersion is numeric and has at most three fields; drop a pre-release suffix.
$MsiVersion = ($Version -split '-')[0]
$Msi = Join-Path $Dist "$Base.msi"
$Wxs = Join-Path $PSScriptRoot 'nieuwser.wxs'
$IconPath = Join-Path $Root 'assets\app-icon\nieuwser.ico'
if (Test-Path $Msi) { Remove-Item -Force $Msi }
& wix build $Wxs -arch x64 `
  -d "Version=$MsiVersion" `
  -d "BinDir=$Bin" `
  -d "IconPath=$IconPath" `
  -o $Msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed with exit code $LASTEXITCODE" }
Write-Output "wrote $Msi"

Write-Output '==> release assets:'
Get-ChildItem $Dist | Where-Object { $_.Name -like "*$Version*" } | Format-Table Name, Length
