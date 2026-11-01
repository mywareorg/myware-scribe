#Requires -Version 5.1
<#
.SYNOPSIS
Sign a built MSIX with a local code-signing certificate.

.DESCRIPTION
build-windows.ps1 -Sign already signs the package it produces; this script is
for signing one that already exists, such as a package from an earlier build
or one that came out of make-msix.ps1 directly.

The certificate must have a private key, and its subject must match the
Publisher in the manifest exactly. make-test-cert.ps1 creates a suitable
self-signed certificate and trusts it for this user.

The Store ignores local signatures and re-signs submitted packages, so the
result only matters for Add-AppxPackage and the Windows App Certification Kit.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\sign-msix.ps1

.EXAMPLE
.\scripts\sign-msix.ps1 -Msix dist\hippo-transcribe-0.1.0-windows-x64.msix `
    -PfxPath C:\keys\hippo-test.pfx -Password hippo-test
#>
[CmdletBinding()]
param(
    # Package to sign. Defaults to the newest .msix under dist.
    [string]$Msix,
    [string]$PfxPath = "C:\keys\hippo-test.pfx",
    # Password for the PFX. Left empty it comes from HIPPO_TEST_PFX_PASSWORD,
    # then falls back to "hippo-test" (what make-test-cert.ps1 uses by default).
    [string]$Password,
    # Optional RFC 3161 timestamp server. A self-signed test certificate does
    # not need one; a real certificate should have it.
    [string]$TimestampUrl
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (-not $PSBoundParameters.ContainsKey("Password")) {
    $Password = if ($env:HIPPO_TEST_PFX_PASSWORD) { $env:HIPPO_TEST_PFX_PASSWORD } else { "hippo-test" }
}

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot

function Resolve-RepoPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return (Join-Path $root $Path)
}

if ($Msix) {
    $package = Resolve-RepoPath $Msix
    if (-not (Test-Path $package)) {
        throw "Package not found: $package"
    }
} else {
    $candidates = @(Get-ChildItem -Path (Join-Path $root "dist") -Filter *.msix -Recurse -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending)
    if ($candidates.Count -eq 0) {
        throw "No .msix under $(Join-Path $root 'dist'). Build one first, or pass -Msix."
    }
    $package = $candidates[0].FullName
    if ($candidates.Count -gt 1) {
        Write-Warn "$($candidates.Count) packages found; signing the newest ($($candidates[0].Name))"
    }
}

$certificate = Resolve-RepoPath $PfxPath
if (-not (Test-Path $certificate)) {
    throw "Certificate not found: $certificate (create one with .\scripts\make-test-cert.ps1)"
}

$signtool = Resolve-SdkTool -Name "signtool.exe"
if (-not $signtool) {
    throw "signtool.exe was not found. Install the Windows 10/11 SDK."
}

$arguments = @("sign", "/fd", "SHA256", "/f", $certificate)
if ($Password) { $arguments += @("/p", $Password) }
if ($TimestampUrl) { $arguments += @("/tr", $TimestampUrl, "/td", "SHA256") }
$arguments += $package

Write-Step "Signing $(Split-Path $package -Leaf)"

# signtool reports its diagnosis on stderr, and PowerShell 5.1 turns a native
# command's stderr into a terminating error as soon as it is merged into the
# pipeline. Redirect both streams to files so the message survives to be read
# and matched against the known failures below.
$log = Join-Path $root "sign-msix.log"
$stdoutFile = "$log.out"
$stderrFile = "$log.err"
Remove-Item $log, $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

$startArgs = @{
    FilePath               = $signtool
    NoNewWindow            = $true
    Wait                   = $true
    PassThru               = $true
    RedirectStandardOutput = $stdoutFile
    RedirectStandardError  = $stderrFile
    ArgumentList           = @($arguments | ForEach-Object {
        $argument = [string]$_
        if ($argument -match '[\s"]') {
            '"' + ($argument -replace '"', '\"') + '"'
        } else {
            $argument
        }
    })
}
$child = Start-Process @startArgs
$exitCode = $child.ExitCode

$lines = @()
foreach ($file in @($stdoutFile, $stderrFile)) {
    if (Test-Path $file) { $lines += @(Get-Content -Path $file) }
}
$lines | Tee-Object -FilePath $log | Out-Host

if ($exitCode -ne 0) {
    if (($lines -join "`n") -match "PFX password is not correct") {
        throw @"
signtool rejected the password for $certificate.
Pass the real one with -Password '<password>', or set HIPPO_TEST_PFX_PASSWORD.
"hippo-test" is only the default of .\scripts\make-test-cert.ps1.
"@
    }
    throw "signtool failed with exit code $exitCode (output in $log)"
}

# signtool can exit 0 without leaving a signature, so read the result back
# rather than trusting the exit code.
$signature = Get-AuthenticodeSignature -FilePath $package
switch ($signature.Status) {
    "Valid" {
        Write-Host ("  Signature: valid (" + $signature.SignerCertificate.Subject + ")") -ForegroundColor Green
    }
    "NotSigned" {
        throw "signtool reported success but $package is still unsigned"
    }
    default {
        # Hash mismatch, untrusted chain, expired certificate, and so on. The
        # signature exists but this machine will not accept it yet.
        Write-Warn "Signature present but not trusted: $($signature.Status)"
        Write-Warn "Add the certificate to Cert:\CurrentUser\TrustedPeople, or use a real code-signing certificate."
    }
}

Write-Host ""
Write-Host "Signed" -ForegroundColor Green
Write-Host "  Package:     $package"
Write-Host "  Certificate: $certificate"
