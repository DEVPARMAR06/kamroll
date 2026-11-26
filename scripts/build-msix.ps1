<#
Build the Microsoft Store package: dist-store\Kamroll_<version>_x64.msix

  pwsh scripts\build-msix.ps1            # builds the app, then packages it
  pwsh scripts\build-msix.ps1 -NoBuild   # packages the program already in desktop\src-tauri\target\release

The package is deliberately unsigned: upload it to Partner Center and the Store signs it. (An unsigned package
cannot be installed by double-clicking, so it is not a download for users.)

The Store forbids a version starting with 0 and keeps the fourth number for itself, so the app's version a.b.c
becomes (a+1).b.c.0. For example 0.1.0 is packaged as 1.1.0.0.
#>
param([switch]$NoBuild)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$desktop = Join-Path $root "desktop"

if (-not $NoBuild) {
    Push-Location $desktop
    try {
        npx tauri build --no-bundle
        if ($LASTEXITCODE -ne 0) { throw "The app did not build." }
    } finally { Pop-Location }
}

$exe = Join-Path $desktop "src-tauri\target\release\kamroll.exe"
if (-not (Test-Path $exe)) { throw "Not built yet: $exe" }

$app = (Get-Content (Join-Path $desktop "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json).version
$a, $b, $c = $app.Split("-")[0].Split(".") | ForEach-Object { [int]$_ }
$version = "{0}.{1}.{2}.0" -f ($a + 1), $b, $c

$kits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$sdk = Get-ChildItem $kits -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "x64\makeappx.exe") } |
    Sort-Object { [version]$_.Name } | Select-Object -Last 1
if (-not $sdk) { throw "makeappx.exe not found. Install the Windows SDK (Visual Studio Installer, or winget install Microsoft.WindowsSDK.10.0.26100)." }
$makeappx = Join-Path $sdk.FullName "x64\makeappx.exe"
$makepri = Join-Path $sdk.FullName "x64\makepri.exe"

$work = Join-Path $desktop "src-tauri\target\msix"
$stage = Join-Path $work "package"
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage
Copy-Item (Join-Path $desktop "store\Assets") $stage -Recurse
Copy-Item (Join-Path $root "LICENSE") (Join-Path $stage "LICENSE.txt")
Copy-Item (Join-Path $root "THIRD_PARTY_NOTICES.md") $stage
$manifest = (Get-Content (Join-Path $desktop "store\AppxManifest.xml") -Raw).Replace('$VERSION$', $version)
Set-Content (Join-Path $stage "AppxManifest.xml") $manifest -Encoding utf8NoBOM

# resources.pri lets Windows pick the right icon size. One file for everything (no separate resource packs).
$priConfig = Join-Path $work "priconfig.xml"
& $makepri createconfig /cf $priConfig /dq en-US /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed" }
$xml = [xml](Get-Content $priConfig -Raw)
$packaging = $xml.resources.SelectSingleNode("packaging")
if ($packaging) { [void]$xml.resources.RemoveChild($packaging) }
$xml.Save($priConfig)
& $makepri new /pr $stage /cf $priConfig /mn (Join-Path $stage "AppxManifest.xml") /of (Join-Path $stage "resources.pri") /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri new failed" }

$out = Join-Path $root "dist-store"
New-Item -ItemType Directory -Force $out | Out-Null
$msix = Join-Path $out "Kamroll_${version}_x64.msix"
& $makeappx pack /d $stage /p $msix /o | Out-Null
if ($LASTEXITCODE -ne 0) { & $makeappx pack /d $stage /p $msix /o; throw "makeappx pack failed" }
Write-Host "App $app -> Store package $version"
Write-Host "$msix ($([math]::Round((Get-Item $msix).Length / 1MB, 1)) MB)"
