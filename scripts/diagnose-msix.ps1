#Requires -Version 5.1
<#
.SYNOPSIS
  Inspects the MSIX helpers in build-windows.ps1 and can exercise them without
  a Rust build.

.DESCRIPTION
  Loads the function definitions out of build-windows.ps1 without running the
  build, prints the signature of New-MsixPackage so a stray [switch] is
  obvious, and with -Probe packs a throwaway layout through makeappx. The
  probe uses empty placeholder files, so it reproduces packing and signing
  failures in seconds instead of after a full cargo build.

.PARAMETER Probe
  Run New-MsixPackage against a scratch layout under dist\msix-probe.

.PARAMETER Version
  Version to render into the probe manifest. Defaults to 0.1.0.

.PARAMETER Keep
  Leave the probe layout and the produced .msix in place for inspection.

.EXAMPLE
  .\scripts\diagnose-msix.ps1

.EXAMPLE
  .\scripts\diagnose-msix.ps1 -Probe -Keep
#>
param(
    [switch]$Probe,
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

$buildScript = Join-Path $scriptDir "build-windows.ps1"
$ast = [System.Management.Automation.Language.Parser]::ParseFile($buildScript, [ref]$null, [ref]$null)
$definitions = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.FunctionDefinitionAst]
})
if ($definitions.Count -eq 0) {
    throw "No function definitions were found in $buildScript"
}

# Dot-sourcing each definition makes the helpers callable without executing
# the build body underneath them.
foreach ($definition in $definitions) {
    . ([scriptblock]::Create($definition.Extent.Text))
}

Write-Step "PowerShell $($PSVersionTable.PSVersion)"
Write-Step "Functions found in build-windows.ps1: $($definitions.Name -join ', ')"

Write-Step "New-MsixPackage signature"
(Get-Command New-MsixPackage).Parameters.GetEnumerator() | Sort-Object Key | ForEach-Object {
    $type = $_.Value.ParameterType.Name
    $flag = if ($type -eq "SwitchParameter") { "  <-- switch" } else { "" }
    Write-Host ("    {0,-18} {1,-16}{2}" -f $_.Key, $type, $flag)
}

if (-not $Probe) {
    Write-Host ""
    Write-Host "Pass -Probe to pack a scratch layout through makeappx." -ForegroundColor DarkGray
    return
}

$makeappx = Resolve-SdkTool -Name "makeappx.exe"
if (-not $makeappx) {
    throw "makeappx.exe was not found. Install the Windows 10/11 SDK."
}
Write-Step "makeappx: $makeappx"

$probeRoot = Join-Path $root "dist\msix-probe"
$stage = Join-Path $probeRoot "stage"
if (Test-Path $probeRoot) { Remove-Item $probeRoot -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

# Empty files are enough for manifest validation and packing; the point is to
# reach makeappx without compiling anything.
foreach ($file in @("hippo-transcribe.exe", "sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
    New-Item -ItemType File -Path (Join-Path $stage $file) -Force | Out-Null
}

$identityFile = Join-Path $root "packaging\identity.json"
try {
    $msix = New-MsixPackage -Root $root -Stage $stage -OutRoot $probeRoot `
        -Name "probe" -Version $Version -IdentityFile $identityFile
    Write-Host ""
    Write-Host "Packing succeeded: $msix" -ForegroundColor Green
} catch {
    Write-Host ""
    Write-Host "Packing failed." -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    Write-Host ("  Message: " + $_.Exception.Message)
    Write-Host ("  Stack:   " + ($_.ScriptStackTrace -replace "`r?`n", " | "))
    exit 1
}

if ($Keep) {
    Write-Host "  Kept: $probeRoot" -ForegroundColor DarkGray
} else {
    Remove-Item $probeRoot -Recurse -Force -ErrorAction SilentlyContinue
}
