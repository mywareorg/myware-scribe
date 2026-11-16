#Requires -Version 5.1
<#
.SYNOPSIS
Run the Windows App Certification Kit against this repo's package.

.DESCRIPTION
Everything this needs is set below: the appcert.exe location, the package, the
report and the log. Run it bare and it certifies the newest package in dist:

    .\scripts\wack-elevated.ps1

appcert.exe refuses to run without administrator rights, so the script
re-launches itself through UAC. appcert also installs the package to test it,
which AppXSvc only allows for a certificate in Cert:\LocalMachine; the script
says so instead of failing silently if the signing certificate is not trusted
for the machine.

build-windows.ps1 -Wack calls this same script with explicit paths, so the two
cannot drift apart. Every value below can still be overridden by parameter.

.EXAMPLE
.\scripts\wack-elevated.ps1

.EXAMPLE
.\scripts\wack-elevated.ps1 -Msix dist\myware-scribe-0.2.0-windows-x64.msix
#>
[CmdletBinding()]
param(
    # Defaults are filled in below from $PSScriptRoot and the literal paths.
    [string]$Appcert,
    [string]$Msix,
    [string]$ReportPath,
    [string]$LogPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-env.ps1")

# ---------------------------------------------------------------- settings ---
$dist = Join-Path (Split-Path -Parent $PSScriptRoot) "dist"

# The App Certification Kit installs beside bin, not under bin\<version>,
# which is why the SDK search in Resolve-SdkTool could not find it.
$AppcertDefault = "C:\Program Files (x86)\Windows Kits\10\App Certification Kit\appcert.exe"

# Built by: .\scripts\build-windows.ps1 -Gpu -Msix
$MsixDefault = Join-Path $dist "myware-scribe-0.1.0-windows-x64.msix"
$MsixName = [System.IO.Path]::GetFileNameWithoutExtension($MsixDefault)
$ReportDefault = Join-Path $dist "$MsixName-wack.xml"
$LogDefault = Join-Path $dist "$MsixName-wack.log"
# -----------------------------------------------------------------------------

if (-not $Appcert) { $Appcert = $AppcertDefault }
if (-not $Msix) { $Msix = $MsixDefault }
if (-not $ReportPath) { $ReportPath = $ReportDefault }
if (-not $LogPath) { $LogPath = $LogDefault }

function Resolve-AbsolutePath {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return (Join-Path (Split-Path -Parent $PSScriptRoot) $Path)
}

$Appcert = Resolve-AbsolutePath $Appcert
$Msix = Resolve-AbsolutePath $Msix
$ReportPath = Resolve-AbsolutePath $ReportPath
$LogPath = Resolve-AbsolutePath $LogPath

# A version bump changes the package name, so fall back to the newest build
# rather than certifying last release's file.
if (-not (Test-Path $Msix)) {
    $newest = Get-ChildItem -Path $dist -Filter *.msix -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch 'msix-probe' } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $newest) {
        throw "No package at $Msix and none under $dist. Build one first: .\scripts\build-windows.ps1 -Gpu -Msix"
    }
    Write-Warn "$([System.IO.Path]::GetFileName($Msix)) is missing; certifying the newest package instead ($($newest.Name))"
    $Msix = $newest.FullName
    if (-not $PSBoundParameters.ContainsKey("ReportPath")) {
        $ReportPath = Join-Path $dist "$([System.IO.Path]::GetFileNameWithoutExtension($Msix))-wack.xml"
    }
    if (-not $PSBoundParameters.ContainsKey("LogPath")) {
        $LogPath = [System.IO.Path]::ChangeExtension($ReportPath, ".log")
    }
}

if (-not (Test-Path $Appcert)) {
    throw "appcert.exe was not found at $Appcert. Add the 'Windows App Certification Kit' component to the Windows 10/11 SDK in the Visual Studio installer, or install the standalone SDK."
}

# appcert installs the package to test it, and AppXSvc reads the machine store,
# so a certificate trusted only for this user still fails inside appcert.
function Assert-MachineTrust {
    param([Parameter(Mandatory = $true)][string]$Package)

    $signature = Get-AuthenticodeSignature -FilePath $Package
    if (-not $signature.SignerCertificate) {
        throw "$([System.IO.Path]::GetFileName($Package)) is not signed. Sign it first: .\scripts\sign-msix.ps1"
    }
    $thumbprint = $signature.SignerCertificate.Thumbprint
    $trusted = Get-ChildItem "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $thumbprint } |
        Select-Object -First 1
    if ($trusted) { return }
    Write-Warn "The signing certificate is not in Cert:\LocalMachine\TrustedPeople, so appcert cannot install the package. From an elevated prompt: Import-Certificate -FilePath <signing.cer> -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
}

# UAC: re-run this script elevated. -Wait keeps the exit code meaningful.
if (-not (Test-Elevated)) {
    Write-Step "Elevating to run appcert.exe"
    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass",
        "-File", "`"$PSCommandPath`"",
        "-Appcert", "`"$Appcert`"",
        "-Msix", "`"$Msix`"",
        "-ReportPath", "`"$ReportPath`"",
        "-LogPath", "`"$LogPath`""
    ) -join " "
    $child = Start-Process -FilePath "powershell" -ArgumentList $arguments -Verb RunAs -Wait -PassThru
    if ($child) { $child.Refresh() }
    $code = if ($child -and $null -ne $child.ExitCode) { $child.ExitCode } else { 0 }
    if (Test-Path $LogPath) { Get-Content -Path $LogPath | Out-Host }
    exit $code
}

Assert-MachineTrust -Package $Msix

Write-Step "Certifying $([System.IO.Path]::GetFileName($Msix))"

# A short lead-in so a crash still leaves evidence of how far the run got.
"wack-elevated started $(Get-Date -Format 'u')" | Out-File -FilePath $LogPath -Encoding utf8
"  appcert:    $Appcert" | Out-File -FilePath $LogPath -Encoding utf8 -Append
"  package:    $Msix" | Out-File -FilePath $LogPath -Encoding utf8 -Append
"  report:     $ReportPath" | Out-File -FilePath $LogPath -Encoding utf8 -Append

$exitCode = 1
try {
    # reset clears any previous run's state; test does the work and writes the
    # report. Both need elevation, which is why this script exists.
    "--- appcert reset ---" | Out-File -FilePath $LogPath -Encoding utf8 -Append
    & $Appcert reset 2>&1 | Out-File -FilePath $LogPath -Encoding utf8 -Append
    "--- appcert test ---" | Out-File -FilePath $LogPath -Encoding utf8 -Append
    & $Appcert test -appxpackagepath $Msix -reportoutputpath $ReportPath 2>&1 |
        Out-File -FilePath $LogPath -Encoding utf8 -Append
    $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
    "appcert exit code: $exitCode" | Out-File -FilePath $LogPath -Encoding utf8 -Append
}
catch {
    "appcert failed: $($_.Exception.Message)" | Out-File -FilePath $LogPath -Encoding utf8 -Append
    $exitCode = 1
}

Get-Content -Path $LogPath | Out-Host

if (Test-Path $ReportPath) {
    Write-Host "  Report: $ReportPath" -ForegroundColor Green
} else {
    Write-Warn "No report was written to $ReportPath"
}
Write-Host "  Log:    $LogPath"

exit $exitCode
