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
.\scripts\install-msix.ps1 -Msix dist\myware-scribe-0.1.0-windows-x64.msix
#>
[CmdletBinding()]
param(
    # Package to install. Defaults to the newest .msix under dist.
    [string]$Msix,
    # Certificate to trust. make-test-cert.ps1 writes one beside the .pfx.
    [string]$CerPath = "C:\keys\myware-test.cer",
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

# Add-AppxPackage refuses an unsigned package with 0x800B0100, which reads like
# a deployment fault rather than "you are installing the Store upload by hand".
if (-not (Get-AuthenticodeSignature -FilePath $package).SignerCertificate) {
    $testCopy = Join-Path (Split-Path -Parent $package) ([System.IO.Path]::GetFileNameWithoutExtension($package) + "-test.msix")
    throw @"
$(Split-Path $package -Leaf) is not signed, so Windows will not install it.
The Store re-signs its upload, so keep this file for Partner Center and sign a copy to test:
    Copy-Item "$package" "$testCopy"
    .\scripts\sign-msix.ps1 -Msix "$testCopy"
    .\scripts\install-msix.ps1 -Msix "$testCopy"
"@
}


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
    if ($elevated) { $elevated.Refresh() }
    $childCode = if ($elevated -and $null -ne $elevated.ExitCode) { $elevated.ExitCode } else { 0 }
    if ($childCode -ne 0) {
        # The elevated window closes with its process, so the child leaves the
        # failure detail beside the package for the parent to print.
        $errorLog = Join-Path (Split-Path -Parent $package) "install-error.txt"
        if (Test-Path $errorLog) {
            Get-Content -Path $errorLog | Out-Host
        }
        throw "The elevated install failed with exit code $childCode. Run this script from an elevated Prompt to see the output, or enable Developer Mode and pass -SkipTrust."
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
$modelsOverride = [Environment]::GetEnvironmentVariable("MYWARE_SCRIBE_MODELS_DIR", "User")
if (-not $modelsOverride) {
    $modelsOverride = [Environment]::GetEnvironmentVariable("MYWARE_SCRIBE_MODELS_DIR", "Machine")
}
if ($modelsOverride) {
    Write-Warn "MYWARE_SCRIBE_MODELS_DIR is set to $modelsOverride; -Fresh does not delete that folder."
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
try {
    Add-AppxPackage -Path $package -ErrorAction Stop
} catch {
    # The HRESULT maps to a specific AppXSvc failure, and the log entry usually
    # names the cause; a bare "it failed" costs another round trip.
    $errorLog = Join-Path (Split-Path -Parent $package) "install-error.txt"
    $detail = @(
        "Install failed: $($_.Exception.Message)",
        "  Package:   $package",
        "  Signature: $((Get-AuthenticodeSignature -FilePath $package).Status)",
        "  AppXSvc log:"
    )
    $detail += Get-AppxLog -ErrorAction SilentlyContinue |
        Where-Object { $_.Level -ne "Info" } |
        Select-Object -Last 6 |
        ForEach-Object { "    " + $_.Message }
    $detail | Out-File -FilePath $errorLog -Encoding utf8
    $detail | Out-Host
    Write-Warn "Details written to $errorLog"
    exit 1
}

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
    Join-Path $env:LOCALAPPDATA "Packages\$($installed.PackageFamilyName)\LocalCache\Local\myware-scribe\models"
}
Write-Host "  Models:  $modelDir"
