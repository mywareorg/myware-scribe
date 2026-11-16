#Requires -Version 5.1
<#
.SYNOPSIS
Create a self-signed code-signing certificate for sideloading a local MSIX.

.DESCRIPTION
The Microsoft Store re-signs submitted packages, so this certificate is only
for testing: it lets Add-AppxPackage and the Windows App Certification Kit
accept a package from build-windows.ps1 -Sign.

The private key stays in Cert:\CurrentUser\My. A .pfx and a .cer are written
beside each other under -PfxPath, and the .cer is imported into
Cert:\CurrentUser\TrustedPeople so this user's machine trusts packages signed
by it. Both are per-user stores; no elevation is needed.

Running again reuses the certificate already in the store, so repeated builds
do not pile up duplicates. Pass -Force to replace it, which also invalidates
previously signed packages.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\make-test-cert.ps1

.EXAMPLE
.\scripts\make-test-cert.ps1 -PfxPath D:\keys\myware.pfx -Password correct-horse -Force
#>
[CmdletBinding()]
param(
    # Windows only installs a package whose manifest Publisher matches the
    # signing certificate's subject exactly, so this defaults to the Publisher
    # in packaging/identity.json rather than a friendly name.
    [string]$Subject,
    [string]$PfxPath = "C:\keys\myware-test.pfx",
    [string]$Password = "myware-test",
    # Skip the TrustedPeople import when only the .pfx is wanted.
    [switch]$NoTrust,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot

if (-not $Subject) {
    $identityFile = Join-Path $root "packaging\identity.json"
    if (Test-Path $identityFile) {
        $identity = Get-Content -Path $identityFile -Raw | ConvertFrom-Json
        if ($identity.publisher) { $Subject = $identity.publisher }
    }
    if (-not $Subject) {
        throw "No -Subject given and packaging\identity.json has no publisher."
    }
}

$pfx = if ([System.IO.Path]::IsPathRooted($PfxPath)) {
    $PfxPath
} else {
    Join-Path $root $PfxPath
}
$cer = [System.IO.Path]::ChangeExtension($pfx, ".cer")
$directory = Split-Path -Parent $pfx
New-Item -ItemType Directory -Path $directory -Force | Out-Null

# Expired or key-less certificates would sign packages Windows then refuses to
# install, so only reuse a usable one.
$existing = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object {
        $_.Subject -eq $Subject -and
        $_.HasPrivateKey -and
        $_.NotAfter -gt (Get-Date).AddDays(7)
    } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

if ($existing -and -not $Force) {
    $certificate = $existing
    Write-Step "Reusing certificate $($certificate.Thumbprint)"
} else {
    Write-Step "Creating a self-signed code-signing certificate"
    # 1.3.6.1.5.5.7.3.3 is the code-signing EKU; without it signtool refuses to
    # use the certificate for packaging.
    $certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $Subject `
        -CertStoreLocation Cert:\CurrentUser\My `
        -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable `
        -NotAfter (Get-Date).AddYears(3) `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
}

$secure = ConvertTo-SecureString -String $Password -Force -AsPlainText

Write-Step "Exporting $pfx"
Export-PfxCertificate -Cert $certificate -FilePath $pfx -Password $secure -Force | Out-Null
Export-Certificate -Cert $certificate -FilePath $cer -Force | Out-Null

if (-not $NoTrust) {
    $trusted = Get-ChildItem Cert:\CurrentUser\TrustedPeople |
        Where-Object { $_.Thumbprint -eq $certificate.Thumbprint }
    if ($trusted) {
        Write-Step "Certificate is already trusted for this user"
    } else {
        Write-Step "Trusting the certificate for this user"
        Import-Certificate -FilePath $cer -CertStoreLocation Cert:\CurrentUser\TrustedPeople | Out-Null
    }
}

Write-Host ""
Write-Host "Certificate ready" -ForegroundColor Green
Write-Host "  Subject:    $Subject"
Write-Host "  Thumbprint: $($certificate.Thumbprint)"
Write-Host "  PFX:        $pfx"
Write-Host "  CER:        $cer"
Write-Host ""
Write-Host "Sign a test package with:"
Write-Host "  .\scripts\build-windows.ps1 -SkipTests -SkipModels -NoZip -Msix -Sign -CertificatePath `"$pfx`" -CertificatePassword $Password"
