#Requires -Version 5.1
<#
.SYNOPSIS
  Packs a staged Windows build into an MSIX for the Microsoft Store.

.DESCRIPTION
  build-windows.ps1 invokes this as a separate process. The packing used to be
  an in-script helper, but Windows PowerShell 5.1 refused to bind its all-string
  parameters, reporting a spurious SwitchParameter conversion, while the very
  same call succeeds from a fresh runspace.

  The identity values are read from packaging/identity.json, which must match
  the Partner Center product identity character for character.

.PARAMETER Stage
  Directory holding myware-scribe.exe and the sherpa/ONNX Runtime DLLs.

.PARAMETER OutDir
  Directory for the .msix and its .sha256.

.PARAMETER Name
  Package base name, without the .msix extension.

.PARAMETER Version
  Version to render into the manifest. Three parts are padded to four.

.PARAMETER IdentityPath
  identity.json to render into the manifest.

.PARAMETER KeepLayout
  Leave the intermediate layout directory in place.

.EXAMPLE
  .\scripts\make-msix.ps1 -Stage dist\myware-scribe-0.1.0-windows-x64 `
      -OutDir dist -Name myware-scribe-0.1.0-windows-x64 -Version 0.1.0 `
      -IdentityPath packaging\identity.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Stage,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$IdentityPath,
    # Leave the model files out (a small package for quick iteration).
    [switch]$NoModels,
    [switch]$KeepLayout
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

trap {
    Write-Host ""
    Write-Host "Packing failed at:" -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    Write-Host ("  Message: " + $_.Exception.Message)
    Write-Host ("  Stack:   " + ($_.ScriptStackTrace -replace "`r?`n", " | "))
    exit 1
}

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir

. (Join-Path $scriptDir "windows-env.ps1")

$makeappx = Resolve-SdkTool -Name "makeappx.exe"
if (-not $makeappx) {
    throw "makeappx.exe was not found. Install the Windows 10/11 SDK, or run this from a developer prompt."
}
if (-not (Test-Path $IdentityPath)) {
    throw "Identity file not found: $IdentityPath"
}

$identity = Get-Content -Path $IdentityPath -Raw | ConvertFrom-Json
foreach ($key in @("packageName", "publisher", "publisherDisplayName", "displayName", "description", "applicationId", "backgroundColor")) {
    $value = $identity.PSObject.Properties[$key]
    if (-not $value -or -not $value.Value) {
        throw "$IdentityPath is missing '$key'"
    }
}
if ($identity.publisher -match '^CN=0{8}-') {
    Write-Warn "The publisher in $IdentityPath is still the placeholder; the Store will reject this package."
}

$layout = Join-Path $OutDir "$Name-msix\layout"
if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
New-Item -ItemType Directory -Path $layout -Force | Out-Null

# Only the executable and its runtime DLLs. deploy.bat, the sample audio and
# the models are direct-download baggage; the Store build downloads models on
# first run instead. The Visual C++ runtime DLLs are part of that set: the
# executable links the DLL CRT, and the certification machine has no
# redistributable installed.
Copy-Item (Join-Path $Stage "myware-scribe.exe") $layout
# The briefing runs in its own process so llama.cpp's ggml never meets
# whisper.cpp's inside one binary.
Copy-Item (Join-Path $Stage "myware-summarize.exe") $layout
$runtimeDlls = @("sherpa-onnx-c-api.dll", "onnxruntime.dll", "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll")
if (Test-Path -LiteralPath (Join-Path $Stage "vcomp140.dll")) {
    $runtimeDlls += "vcomp140.dll"
}
foreach ($dll in $runtimeDlls) {
    Copy-Item (Join-Path $Stage $dll) $layout
}
$assets = Join-Path $layout "Assets"
New-Item -ItemType Directory -Path $assets -Force | Out-Null
Copy-Item (Join-Path $root "packaging\assets\*.png") $assets

# The GPU calibration benchmark transcribes this clip, and it looks for it
# beside the executable, so leaving it out makes the calibration fail with "the
# bundled calibration sample is missing" instead of ever testing the GPU.
$sample = Join-Path $Stage "sample-short.mp3"
if (-not (Test-Path $sample)) { $sample = Join-Path $root "audio\sample-short.mp3" }
if (-not (Test-Path $sample)) {
    throw "sample-short.mp3 was not found; the GPU calibration cannot run without it."
}
Copy-Item $sample $layout

# Third-party notices: model weights and runtimes are redistributed here, and
# the Store package has no README for a user to find.
$notices = Join-Path $Stage "LICENSES.md"
if (-not (Test-Path $notices)) {
    $notices = Join-Path $root "LICENSES.md"
}
Copy-Item $notices $layout

# The application's own licence. LICENSES.md points at it, and the Store
# package has no README or source checkout to fall back on.
$appLicense = Join-Path $Stage "LICENSE"
if (-not (Test-Path $appLicense)) {
    $appLicense = Join-Path $root "LICENSE"
}
Copy-Item $appLicense $layout

# Optionally ship the models inside the package. The core looks for <exe>\models
# before the per-user folder, so a bundled copy needs no download at all. The
# Store build leaves them out and downloads them once on first run, which keeps
# the package and its updates small.
if (-not $NoModels) {
    $modelsSource = Join-Path $Stage "models"
    if (-not (Test-Path $modelsSource)) {
        throw "No models in $Stage. Pack with -NoModels for the Store package, or build with -BundleModels."
    }
    $modelsTarget = Join-Path $layout "models"
    New-Item -ItemType Directory -Path $modelsTarget -Force | Out-Null
    Copy-Item (Join-Path $modelsSource "*") $modelsTarget -Recurse -Force
    $modelBytes = (Get-ChildItem -Path $modelsTarget -Recurse -File |
        Measure-Object -Property Length -Sum).Sum
    if (-not $modelBytes) { $modelBytes = 0 }
    Write-Host ("  Models:  {0:N0} MB" -f ($modelBytes / 1MB))
}

$template = Get-Content -Path (Join-Path $root "packaging\AppxManifest.xml.template") -Raw
if (-not $template) {
    throw "packaging\AppxManifest.xml.template is missing or empty"
}
$values = [ordered]@{
    PackageName          = $identity.packageName
    Publisher            = $identity.publisher
    PublisherDisplayName = $identity.publisherDisplayName
    DisplayName          = $identity.displayName
    Description          = $identity.description
    ApplicationId        = $identity.applicationId
    BackgroundColor      = $identity.backgroundColor
    Version              = (ConvertTo-FourPartVersion -Version $Version)
}
foreach ($key in $values.Keys) {
    $template = $template.Replace("{{$key}}", [string]$values[$key])
}
if ($template -match '\{\{') {
    throw "The rendered manifest still has placeholders; check packaging\AppxManifest.xml.template"
}
Set-Content -Path (Join-Path $layout "AppxManifest.xml") -Value $template -Encoding UTF8

# Windows resolves the tile label and the scaled asset variants through
# resources.pri. A package built without one still installs, but Windows has
# nothing to map the app's display name to and falls back to the executable's
# file name in the Start menu. This is the documented manual-packaging step.
$makepri = Resolve-SdkTool -Name "makepri.exe"
if (-not $makepri) {
    throw "makepri.exe was not found. Install the Windows 10/11 SDK, or run this from a developer prompt."
}
$priConfig = Join-Path $OutDir "$Name-msix\priconfig.xml"
Write-Step "Indexing resources.pri"
& $makepri createconfig /cf $priConfig /dq en-US /o | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "makepri createconfig failed with exit code $LASTEXITCODE"
}
& $makepri new /pr $layout /cf $priConfig /of (Join-Path $layout "resources.pri") /mn $identity.packageName /o | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "makepri new failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path (Join-Path $layout "resources.pri"))) {
    throw "makepri returned without producing resources.pri"
}

$msix = Join-Path $OutDir "$Name.msix"
if (Test-Path $msix) { Remove-Item $msix -Force }
Write-Step "Packing $msix"
# Out-Host keeps makeappx's chatter out of the pipeline.
& $makeappx pack /d $layout /p $msix /o | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "makeappx failed with exit code $LASTEXITCODE"
}

$hash = (Get-FileHash $msix -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $(Split-Path $msix -Leaf)" | Set-Content "$msix.sha256" -Encoding ASCII

# Unpack what was just built and diff it against the layout. A package that
# quietly loses a DLL or a model installs fine and only fails at the point of
# use, on someone else's machine, so the build refuses to produce one.
Write-Step "Verifying package contents"
$verifyDir = Join-Path $OutDir "$Name-verify"
if (Test-Path $verifyDir) { Remove-Item $verifyDir -Recurse -Force }
& $makeappx unpack /p $msix /d $verifyDir /o | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "makeappx unpack failed with exit code $LASTEXITCODE"
}
$expected = Get-ChildItem -LiteralPath $layout -Recurse -File |
    ForEach-Object { $_.FullName.Substring((Resolve-Path $layout).Path.Length).TrimStart('\') }
$packed = Get-ChildItem -LiteralPath $verifyDir -Recurse -File |
    ForEach-Object { $_.FullName.Substring((Resolve-Path $verifyDir).Path.Length).TrimStart('\') }
$absent = @($expected | Where-Object { $packed -notcontains $_ })
if ($absent.Count -gt 0) {
    throw ("The package is missing " + $absent.Count + " file(s) from the layout: " + ($absent -join ", "))
}
$required = @(
    "myware-scribe.exe", "myware-summarize.exe", "AppxManifest.xml", "resources.pri",
    "sherpa-onnx-c-api.dll", "onnxruntime.dll",
    "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll",
    "sample-short.mp3", "LICENSES.md", "LICENSE", "Assets\StoreLogo.png"
)
foreach ($file in $required) {
    if ($packed -notcontains $file) {
        throw "$file is required in the package but is not in it."
    }
}
Remove-Item $verifyDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ("  Verified {0:N0} files in the package" -f $packed.Count) -ForegroundColor Green

if (-not $KeepLayout) {
    Remove-Item (Join-Path $OutDir "$Name-msix") -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ("  Package: " + $msix) -ForegroundColor Green
