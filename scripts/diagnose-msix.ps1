#Requires -Version 5.1
<#
.SYNOPSIS
  Packs a throwaway MSIX without running a Rust build.

.DESCRIPTION
  Creates a scratch stage of empty placeholder files and runs make-msix.ps1
  against it, so manifest, asset and makeappx problems show up in seconds
  instead of after a full cargo build.

.PARAMETER Version
  Version to render into the probe manifest. Defaults to 0.1.0.

.PARAMETER Keep
  Leave the probe layout and the produced .msix in place for inspection.

.EXAMPLE
  .\scripts\diagnose-msix.ps1

.EXAMPLE
  .\scripts\diagnose-msix.ps1 -Keep
#>
[CmdletBinding()]
param(
    [string]$Version = "0.1.0",
    [switch]$Keep
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

trap {
    Write-Host ""
    Write-Host "Diagnosis failed at:" -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    Write-Host ("  Message: " + $_.Exception.Message)
    exit 1
}

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir

. (Join-Path $scriptDir "windows-env.ps1")

$makeappx = Resolve-SdkTool -Name "makeappx.exe"
if (-not $makeappx) {
    throw "makeappx.exe was not found. Install the Windows 10/11 SDK."
}
Write-Step "PowerShell $($PSVersionTable.PSVersion)"
Write-Step "makeappx: $makeappx"

$probeRoot = Join-Path $root "dist\msix-probe"
$stage = Join-Path $probeRoot "stage"
if (Test-Path $probeRoot) { Remove-Item $probeRoot -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

# Empty files are enough for manifest validation and packing; the point is to
# reach makeappx without compiling anything.
foreach ($file in @("myware-scribe.exe", "sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
    New-Item -ItemType File -Path (Join-Path $stage $file) -Force | Out-Null
}

$packer = Join-Path $scriptDir "make-msix.ps1"
$msix = Join-Path $probeRoot "probe.msix"
$packArguments = @(
    "-NoProfile", "-ExecutionPolicy", "Bypass",
    "-File", $packer,
    "-Stage", $stage,
    "-OutDir", $probeRoot,
    "-Name", "probe",
    "-Version", $Version,
    "-IdentityPath", (Join-Path $root "packaging\identity.json")
)
if ($Keep) { $packArguments += "-KeepLayout" }

& powershell @packArguments | Out-Host
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $msix)) {
    Write-Host ""
    Write-Host "Packing failed (exit $LASTEXITCODE)." -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "Packing succeeded: $msix" -ForegroundColor Green
if ($Keep) {
    Write-Host "  Kept: $probeRoot" -ForegroundColor DarkGray
} else {
    Remove-Item $probeRoot -Recurse -Force -ErrorAction SilentlyContinue
}
