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
  Directory holding hippo-transcribe.exe and the sherpa/ONNX Runtime DLLs.

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
  .\scripts\make-msix.ps1 -Stage dist\hippo-transcribe-0.1.0-windows-x64 `
      -OutDir dist -Name hippo-transcribe-0.1.0-windows-x64 -Version 0.1.0 `
      -IdentityPath packaging\identity.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Stage,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$IdentityPath,
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
Copy-Item (Join-Path $Stage "hippo-transcribe.exe") $layout
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

if (-not $KeepLayout) {
    Remove-Item (Join-Path $OutDir "$Name-msix") -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ("  Package: " + $msix) -ForegroundColor Green
