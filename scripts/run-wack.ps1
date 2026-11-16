#Requires -Version 5.1
<#
.SYNOPSIS
Run the Windows App Certification Kit against a package that already exists.

.DESCRIPTION
build-windows.ps1 -Wack does the same check, but only as the last step of a
full rebuild. This script runs it against whatever is already in dist, so a
certification failure can be re-tested in seconds.

appcert.exe refuses to run unelevated, and it installs the package internally,
which AppXSvc only permits for a certificate in Cert:\LocalMachine. This script
therefore re-launches itself through UAC, trusts the package's signing
certificate if it is not trusted yet, then runs appcert.

Output lands next to the package: <name>-wack.xml is the WACK report and
<name>-wack.log is everything appcert printed, which is what to read first.

.EXAMPLE
.\scripts\run-wack.ps1

.EXAMPLE
.\scripts\run-wack.ps1 -Msix dist\myware-scribe-0.2.0-windows-x64.msix
#>
[CmdletBinding()]
param(
    # Package to certify. Defaults to the newest .msix under dist.
    [string]$Msix,
    # Where WACK writes its XML report. Defaults to <package>-wack.xml.
    [string]$ReportPath,
    # Where appcert's console output is captured. Defaults to <report base>.log.
    [string]$LogPath,
    # appcert.exe, if it is somewhere the SDK search does not cover.
    [string]$Appcert,
    # Run without importing the signing certificate into the machine store.
    [switch]$SkipTrust
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"

function Resolve-RepoPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return (Join-Path $root $Path)
}

# Default to the newest package, ignoring the throwaway probe builds.
if ($Msix) {
    $package = Resolve-RepoPath $Msix
} else {
    $candidates = @(Get-ChildItem -Path $dist -Filter *.msix -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch 'msix-probe' } |
        Sort-Object LastWriteTime -Descending)
    if ($candidates.Count -eq 0) {
        throw "No .msix under $dist. Build one first: .\scripts\build-windows.ps1 -Gpu -Msix"
    }
    $package = $candidates[0].FullName
    if ($candidates.Count -gt 1) {
        Write-Warn "$($candidates.Count) packages found; certifying the newest ($($candidates[0].Name))"
    }
}
if (-not (Test-Path $package)) { throw "Package not found: $package" }

$packageName = [System.IO.Path]::GetFileNameWithoutExtension($package)
if (-not $ReportPath) { $ReportPath = Join-Path $dist "$packageName-wack.xml" }
$ReportPath = Resolve-RepoPath $ReportPath
if (-not $LogPath) { $LogPath = [System.IO.Path]::ChangeExtension($ReportPath, ".log") }
$LogPath = Resolve-RepoPath $LogPath

if (-not $Appcert) {
    $Appcert = Resolve-SdkTool -Name "appcert.exe"
    if (-not $Appcert) {
        throw "appcert.exe was not found. Add the 'Windows App Certification Kit' component to the Windows 10/11 SDK in the Visual Studio installer, or install the standalone SDK."
    }
}

# appcert installs the package to test it, and AppXSvc reads the machine store.
function Confirm-SigningCertificate {
    param([Parameter(Mandatory = $true)][string]$Package)

    $signature = Get-AuthenticodeSignature -FilePath $Package
    if (-not $signature.SignerCertificate) {
        throw "$([System.IO.Path]::GetFileName($Package)) is not signed. Sign it first: .\scripts\sign-msix.ps1 -Msix $Package"
    }
    if ($signature.Status -ne "Valid") {
        Write-Warn "The signature chain is not trusted ($($signature.Status)); WACK may still refuse to install it."
    }

    $thumbprint = $signature.SignerCertificate.Thumbprint
    $trusted = Get-ChildItem "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $thumbprint } |
        Select-Object -First 1
    if ($trusted) { return }
    if ($SkipTrust) {
        Write-Warn "The signing certificate is not in Cert:\LocalMachine\TrustedPeople; appcert will fail to install the package (-SkipTrust given)."
        return
    }

    $cerPath = Join-Path $dist "$packageName-signing.cer"
    [System.IO.File]::WriteAllBytes($cerPath, $signature.SignerCertificate.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert))
    Write-Step "Trusting the signing certificate for this machine"
    Import-Certificate -FilePath $cerPath -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" | Out-Null
    Write-Host "  Trusted: $thumbprint ($cerPath)"
}

# UAC: re-run this script elevated. -Wait keeps the exit code meaningful and the
# argument list is quoted because the repo path can contain spaces.
if (-not (Test-Elevated)) {
    Write-Step "Elevating to run appcert.exe"
    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass",
        "-File", "`"$PSCommandPath`"",
        "-Msix", "`"$package`"",
        "-ReportPath", "`"$ReportPath`"",
        "-LogPath", "`"$LogPath`"",
        "-Appcert", "`"$Appcert`""
    )
    if ($SkipTrust) { $arguments += "-SkipTrust" }
    $child = Start-Process -FilePath "powershell" -ArgumentList ($arguments -join " ") -Verb RunAs -Wait -PassThru
    if ($child) { $child.Refresh() }
    $code = if ($child -and $null -ne $child.ExitCode) { $child.ExitCode } else { 0 }
    exit $code
}

Confirm-SigningCertificate -Package $package

foreach ($path in @($ReportPath, $LogPath)) {
    if (Test-Path $path) { Remove-Item -Path $path -Force }
}

# wack-elevated.ps1 owns the appcert invocation, so build-windows.ps1 -Wack and
# this script cannot drift apart.
$helper = Join-Path $PSScriptRoot "wack-elevated.ps1"
& $helper -Appcert $Appcert -Msix $package -ReportPath $ReportPath -LogPath $LogPath
$code = $LASTEXITCODE

Write-Host ""
if (Test-Path $LogPath) { Get-Content -Path $LogPath | Out-Host }

if (-not (Test-Path $ReportPath)) {
    Write-Warn "appcert exited $code and wrote no report to $ReportPath"
} else {
    Write-Host "  Report: $ReportPath" -ForegroundColor Green
}
Write-Host "  Log:    $LogPath"
if ($code -ne 0) {
    Write-Warn "WACK reported failures (exit $code). Read the log above, or open the report."
}
exit $code
