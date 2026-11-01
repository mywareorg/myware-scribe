#Requires -Version 5.1
<#
.SYNOPSIS
Sign a copy of a built MSIX and (re)install it over whatever is installed now.

.DESCRIPTION
The Store re-signs its upload, so the artifact in dist\ must stay unsigned.
Local testing therefore needs a second, signed copy. Doing that by hand is
three commands whose order matters (copy, sign, install) and whose failure
modes are all 0x800B0100, which reads like a corrupt package rather than
"you installed the upload by hand". This runs the sequence in order, removes
the previous install first, and then proves the new package actually contains
the DLLs the app loads at runtime.

.INPUTS
None.

.OUTPUTS
None. Exits non-zero when any step fails.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\reinstall-msix.ps1

.EXAMPLE
.\scripts\reinstall-msix.ps1 -Fresh

.EXAMPLE
.\scripts\reinstall-msix.ps1 -Msix dist\hippo-transcribe-0.1.0-windows-x64.msix
#>
[CmdletBinding()]
param(
    # Package to install. Defaults to the newest .msix under dist that is not
    # already a -test copy.
    [string]$Msix,
    # Signing certificate. make-test-cert.ps1 writes one here.
    [string]$PfxPath = "C:\keys\hippo-test.pfx",
    [string]$Password = "hippo-dev",
    # Public certificate to trust before Add-AppxPackage runs.
    [string]$CerPath = "C:\keys\hippo-test.cer",
    # Also delete the package's data directory, so the app starts as a first
    # run with no downloaded models.
    [switch]$Fresh,
    # Skip the post-install DLL check.
    [switch]$NoVerify
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot

function Resolve-RepoPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return (Join-Path $root $Path)
}

# Run a sibling script in its own process. Windows PowerShell 5.1 fails to bind
# some in-script parameters (the packer hits this too), and a child process also
# keeps each script's strict-mode traps from aborting this one.
#
# The child's own error text is the only thing that explains a failure, so it is
# captured through files: $ErrorActionPreference = "Stop" turns a native
# command's stderr into a terminating ErrorRecord as soon as it is merged into
# the pipeline, which would abort here before the exit code is even readable.
function Invoke-Sibling {
    param(
        [Parameter(Mandatory = $true)][string]$ScriptName,
        [string[]]$Arguments = @()
    )

    $path = Join-Path $PSScriptRoot $ScriptName
    if (-not (Test-Path $path)) { throw "Missing script: $path" }

    $log = Join-Path $root ("reinstall-" + [System.IO.Path]::GetFileNameWithoutExtension($ScriptName) + ".log")
    $stdoutFile = "$log.out"
    $stderrFile = "$log.err"
    Remove-Item $log, $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

    $startArgs = @{
        FilePath               = "powershell"
        NoNewWindow            = $true
        Wait                   = $true
        PassThru               = $true
        RedirectStandardOutput = $stdoutFile
        RedirectStandardError  = $stderrFile
        ArgumentList           = @(
            @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $path) + $Arguments |
                ForEach-Object {
                    $argument = [string]$_
                    if ($argument -match '[\s"]') {
                        '"' + ($argument -replace '"', '\"') + '"'
                    } else {
                        $argument
                    }
                }
        )
    }

    Write-Host "    powershell -File $ScriptName $($Arguments -join ' ')" -ForegroundColor DarkGray
    $child = Start-Process @startArgs
    $exitCode = $child.ExitCode

    foreach ($file in @($stdoutFile, $stderrFile)) {
        if (Test-Path $file) {
            Get-Content -Path $file | Tee-Object -FilePath $log -Append | Out-Host
        }
    }

    if ($exitCode -ne 0) {
        throw ("{0} failed with exit code {1} (output in {2})" -f $ScriptName, $exitCode, $log)
    }
}

if ($Msix) {
    $package = Resolve-RepoPath $Msix
    if (-not (Test-Path $package)) { throw "Package not found: $package" }
} else {
    $candidates = @(Get-ChildItem -Path (Join-Path $root "dist") -Filter *.msix -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch 'msix-probe' -and $_.FullName -notmatch '-test\.msix$' } |
        Sort-Object LastWriteTime -Descending)
    if ($candidates.Count -eq 0) {
        throw "No .msix under $(Join-Path $root 'dist'). Build one first, or pass -Msix."
    }
    $package = $candidates[0].FullName
    if ($candidates.Count -gt 1) {
        Write-Warn "$($candidates.Count) packages found; using the newest ($($candidates[0].Name))"
    }
}

$packageDir = Split-Path -Parent $package
$baseName = [System.IO.Path]::GetFileNameWithoutExtension($package)
if ($baseName -match '-test$') {
    # Already a test copy: signing it again is fine, but there is nothing to
    # preserve, so work on it directly.
    $testCopy = $package
    Write-Host "  Using the existing test copy"
} else {
    $testCopy = Join-Path $packageDir "$baseName-test.msix"
    Write-Step "Copying $(Split-Path $package -Leaf) to the test copy"
    if (Test-Path $testCopy) { Remove-Item -Path $testCopy -Force }
    Copy-Item -Path $package -Destination $testCopy -Force
    Write-Host "  $testCopy"
    Write-Host "  The upload artifact is left unsigned for Partner Center."
}

# Signing is idempotent, so re-sign every time rather than guessing whether the
# copy already carries a signature from an earlier run.
Invoke-Sibling -ScriptName "sign-msix.ps1" -Arguments @(
    "-Msix", $testCopy,
    "-PfxPath", (Resolve-RepoPath $PfxPath),
    "-Password", $Password
)

$installArguments = @(
    "-Msix", $testCopy,
    "-CerPath", (Resolve-RepoPath $CerPath)
)
if ($Fresh) { $installArguments += "-Fresh" }

Invoke-Sibling -ScriptName "install-msix.ps1" -Arguments $installArguments

if ($NoVerify) {
    Write-Host ""
    Write-Host "Reinstalled (verification skipped)" -ForegroundColor Green
    exit 0
}

$identityFile = Join-Path $root "packaging\identity.json"
if (-not (Test-Path $identityFile)) { throw "Missing $identityFile" }
$identity = Get-Content -Path $identityFile -Raw | ConvertFrom-Json
if (-not $identity.packageName) { throw "$identityFile has no packageName" }

$installed = Get-AppxPackage -Name $identity.packageName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installed) { throw "Add-AppxPackage returned without registering $($identity.packageName)" }

# Every one of these is loaded by name at runtime; a missing one is the
# "was not found next to the executable" crash, so fail here instead.
$requiredDlls = @(
    "sherpa-onnx-c-api.dll",
    "onnxruntime.dll",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll"
)
# vcomp140.dll ships only with the GPU build, so it is not required here.

$present = @(Get-ChildItem -Path $installed.InstallLocation -Name -ErrorAction SilentlyContinue)
$missing = @($requiredDlls | Where-Object { $present -notcontains $_ })
$required = @("hippo-transcribe.exe", "AppxManifest.xml")

$missingCore = @($required | Where-Object { $present -notcontains $_ })
if ($missingCore.Count -gt 0) {
    throw "Installed package is missing $($missingCore -join ', ') in $($installed.InstallLocation)"
}
if ($missing.Count -gt 0) {
    throw @"
Installed package is missing $($missing -join ', ').
The MSIX was packed without them; check .\scripts\make-msix.ps1 and rebuild with -KeepLayout.
  Install location: $($installed.InstallLocation)
"@
}

Write-Host ""
Write-Host "Reinstalled" -ForegroundColor Green
Write-Host "  Package: $testCopy"
Write-Host "  Version: $($installed.Version)"
Write-Host "  DLLs:    $($requiredDlls.Count) present in $($installed.InstallLocation)"
Write-Host "  Launch:  explorer.exe shell:AppsFolder\$($installed.PackageFamilyName)!$($identity.applicationId)"
