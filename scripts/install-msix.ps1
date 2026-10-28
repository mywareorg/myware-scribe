#Requires -Version 5.1
<#
.SYNOPSIS
Install a built MSIX on this machine, trusting the test certificate first.

.DESCRIPTION
Add-AppxPackage validates signatures in the Appx deployment service, which
reads Cert:\LocalMachine, not the per-user stores. A test certificate imported
into Cert:\CurrentUser therefore still fails with "a certificate chain
processed, but terminated in a root certificate which is not trusted". Importing
into Cert:\LocalMachine\TrustedPeople needs elevation, so this script
re-launches itself through UAC and does the import there.

Elevation keeps the same user, so the package is registered for the account
that ran the script.

Pass -SkipTrust to install without touching any store, which only works with
Developer Mode enabled.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\install-msix.ps1

.EXAMPLE
.\scripts\install-msix.ps1 -Msix dist\hippo-transcribe-0.1.0-windows-x64.msix
#>
[CmdletBinding()]
param(
    # Package to install. Defaults to the newest .msix under dist.
    [string]$Msix,
    # Certificate to trust. make-test-cert.ps1 writes one beside the .pfx.
    [string]$CerPath = "C:\keys\hippo-test.cer",
    # Install without importing the certificate (needs Developer Mode).
    [switch]$SkipTrust,
    # Also delete the package's private data, so the app starts as a first run
    # with no models downloaded (the folder is recreated on install).
    [switch]$Fresh
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

function Remove-PackageData {
    param([Parameter(Mandatory = $true)][string]$FamilyName)

    $path = Join-Path $env:LOCALAPPDATA "Packages\$FamilyName"
    if (-not (Test-Path $path)) {
        Write-Host "  No package data at $path"
        return
    }

    $bytes = (Get-ChildItem -Path $path -Recurse -Force -ErrorAction SilentlyContinue |
        Measure-Object -Property Length -Sum).Sum
    if (-not $bytes) { $bytes = 0 }
    Write-Host ("  Deleting {0} ({1:N2} GB)" -f $path, ($bytes / 1GB))

    Remove-Item -Path $path -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $path) {
        throw "Could not delete $path. Close the app and any Explorer window showing it, then run this script again."
    }
    Write-Host "  Deleted"
}

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if ($Msix) {
    $package = Resolve-RepoPath $Msix
    if (-not (Test-Path $package)) { throw "Package not found: $package" }
} else {
    $candidates = @(Get-ChildItem -Path (Join-Path $root "dist") -Filter *.msix -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch 'msix-probe' } |
        Sort-Object LastWriteTime -Descending)
    if ($candidates.Count -eq 0) {
        throw "No .msix under $(Join-Path $root 'dist'). Build one first, or pass -Msix."
    }
    $package = $candidates[0].FullName
    if ($candidates.Count -gt 1) {
        Write-Warn "$($candidates.Count) packages found; installing the newest ($($candidates[0].Name))"
    }
}

$certificate = Resolve-RepoPath $CerPath

# UAC: re-run this script elevated. -Wait keeps the exit code meaningful and
# the argument list is quoted because the repo path can contain spaces.
if (-not $SkipTrust -and -not (Test-Elevated)) {
    if (-not (Test-Path $certificate)) {
        throw "Certificate not found: $certificate (create one with .\scripts\make-test-cert.ps1)"
    }
    Write-Step "Elevating to trust $certificate"
    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass",
        "-File", "`"$PSCommandPath`"",
        "-Msix", "`"$package`"",
        "-CerPath", "`"$certificate`""
    )
    if ($Fresh) { $arguments += "-Fresh" }
    $arguments = $arguments -join " "
    $elevated = Start-Process -FilePath "powershell" -ArgumentList $arguments -Verb RunAs -Wait -PassThru
    if ($elevated.ExitCode -ne 0) {
        throw "The elevated install failed with exit code $($elevated.ExitCode). Run this script from an elevated Prompt to see the output, or enable Developer Mode and pass -SkipTrust."
    }
    Write-Host ""
    Write-Host "Installed" -ForegroundColor Green
    Write-Host "  Package: $package"
    exit 0
}

if (-not $SkipTrust) {
    Write-Step "Trusting the signing certificate"
    $cerFile = Get-Item -Path $certificate
    $thumbprint = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($cerFile.FullName)).Thumbprint
    $alreadyTrusted = Get-ChildItem -Path "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $thumbprint } |
        Select-Object -First 1
    if ($alreadyTrusted) {
        Write-Host "  Already in Cert:\LocalMachine\TrustedPeople"
    } else {
        Import-Certificate -FilePath $cerFile.FullName -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" | Out-Null
        Write-Host "  Imported into Cert:\LocalMachine\TrustedPeople"
    }
}

$identityFile = Join-Path $root "packaging\identity.json"
$packageName = $null
$applicationId = $null
if (Test-Path $identityFile) {
    $identity = Get-Content -Path $identityFile -Raw | ConvertFrom-Json
    if ($identity.packageName) { $packageName = $identity.packageName }
    if ($identity.applicationId) { $applicationId = $identity.applicationId }
}

# An environment override moves the models out of the package, where purging
# the package data would not touch them.
$modelsOverride = [Environment]::GetEnvironmentVariable("HIPPO_TRANSCRIBE_MODELS_DIR", "User")
if (-not $modelsOverride) {
    $modelsOverride = [Environment]::GetEnvironmentVariable("HIPPO_TRANSCRIBE_MODELS_DIR", "Machine")
}
if ($modelsOverride) {
    Write-Warn "HIPPO_TRANSCRIBE_MODELS_DIR is set to $modelsOverride; -Fresh does not delete that folder."
}

# A stale registration blocks the install with 0x80073CF9, so replace it.
if ($packageName) {
    $existing = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Step "Removing the previous install"
        $familyName = $existing.PackageFamilyName
        Remove-AppxPackage -Package $existing.PackageFullName
        Write-Host "  Uninstalling removes the package data, so models download again."
        if ($Fresh) {
            Write-Step "Deleting the package data (models included)"
            Remove-PackageData -FamilyName $familyName
        }
    } elseif ($Fresh) {
        Write-Warn "Nothing is installed, so there is no package data to delete."
    }
}

if ($Fresh) {
    # Belt and braces: prove the folder is gone before claiming a clean first run.
    $leftover = if ($packageName) {
        @(Get-ChildItem -Path (Join-Path $env:LOCALAPPDATA "Packages") -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like "$($packageName)_*" })
    } else { @() }
    if ($leftover.Count -gt 0) {
        throw "Package data still present: $($leftover[0].FullName)"
    }
}

Write-Step "Installing $(Split-Path $package -Leaf)"
Add-AppxPackage -Path $package

$installed = if ($packageName) {
    Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue | Select-Object -First 1
}
if (-not $installed) {
    throw "Add-AppxPackage returned without registering $packageName"
}

Write-Host ""
Write-Host "Installed" -ForegroundColor Green
Write-Host "  Package: $package"
Write-Host "  Version: $($installed.Version)"
if ($applicationId) {
    Write-Host "  Launch:  explorer.exe shell:AppsFolder\$($installed.PackageFamilyName)!$applicationId"
}
$modelDir = if ($modelsOverride) {
    $modelsOverride
} else {
    Join-Path $env:LOCALAPPDATA "Packages\$($installed.PackageFamilyName)\LocalCache\Local\hippo-transcribe\models"
}
Write-Host "  Models:  $modelDir"
