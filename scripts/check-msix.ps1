#Requires -Version 5.1
<#
.SYNOPSIS
Explain why a built MSIX is not accepted: signature status, trusted chain, and
publisher match.

.DESCRIPTION
Get-AuthenticodeSignature on its own reports UnknownError for a certificate
that is not trusted for the current user, without saying which of the three
preconditions failed. This script reads the publisher straight out of the
package's AppxManifest.xml and compares it with the certificate that actually
signed the file, then checks that certificate against the user's store.

It signs nothing and changes nothing. The exit code is 1 when any package has
a problem, so it can gate a build.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\check-msix.ps1

.EXAMPLE
.\scripts\check-msix.ps1 -Dist dist\msix-output
#>
[CmdletBinding()]
param(
    # Package to inspect. Defaults to every .msix under -Dist.
    [string]$Msix,
    [string]$Dist = "dist",
    # Certificate to suggest importing when the chain is untrusted.
    [string]$CerPath = "C:\keys\hippo-test.cer"
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

# The manifest publisher is the string Windows compares with the certificate
# subject, so read it from the package rather than trusting identity.json.
function Get-ManifestPublisher {
    param([Parameter(Mandatory = $true)][string]$Package)

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Package)
    try {
        $entry = $archive.Entries | Where-Object { $_.FullName -eq "AppxManifest.xml" } | Select-Object -First 1
        if (-not $entry) { return $null }
        $reader = New-Object System.IO.StreamReader($entry.Open())
        try {
            $xml = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $archive.Dispose()
    }

    $match = [regex]::Match($xml, 'Publisher\s*=\s*"([^"]*)"')
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value
}

function Test-CertInStore {
    param(
        [Parameter(Mandatory = $true)][string]$Thumbprint,
        [Parameter(Mandatory = $true)][string]$Store
    )

    $found = Get-ChildItem -Path "Cert:\CurrentUser\$Store" -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $Thumbprint } |
        Select-Object -First 1
    return [bool]$found
}

if ($Msix) {
    $packagePath = Resolve-RepoPath $Msix
    if (-not (Test-Path $packagePath)) { throw "Package not found: $packagePath" }
    $packages = @(Get-Item -Path $packagePath)
} else {
    $distPath = Resolve-RepoPath $Dist
    $packages = @(Get-ChildItem -Path $distPath -Filter *.msix -Recurse -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending)
    if ($packages.Count -eq 0) {
        throw "No .msix under $distPath. Build one first, or pass -Msix."
    }
}

$problems = 0

foreach ($package in $packages) {
    Write-Step $package.FullName

    $publisher = Get-ManifestPublisher -Package $package.FullName
    if ($publisher) {
        Write-Host "  Manifest publisher: $publisher"
    } else {
        Write-Host "  Manifest publisher: <not readable>"
    }

    $signature = Get-AuthenticodeSignature -FilePath $package.FullName
    $certificate = $signature.SignerCertificate
    Write-Host "  Signature:          $($signature.Status)"
    if ($signature.StatusMessage) {
        Write-Host "  Status message:     $($signature.StatusMessage)"
    }

    if ($certificate) {
        Write-Host "  Signer subject:     $($certificate.Subject)"
        Write-Host "  Signer thumbprint:  $($certificate.Thumbprint)"
        Write-Host "  Signer expires:     $($certificate.NotAfter.ToString('yyyy-MM-dd'))"

        $inTrustedPeople = Test-CertInStore -Thumbprint $certificate.Thumbprint -Store "TrustedPeople"
        $inRoot = Test-CertInStore -Thumbprint $certificate.Thumbprint -Store "Root"
        $trusted = ($inTrustedPeople -or $inRoot)
        Write-Host "  Trusted:            $(if ($trusted) { 'yes' } else { 'no' }) (TrustedPeople: $(if ($inTrustedPeople) { 'yes' } else { 'no' }), Root: $(if ($inRoot) { 'yes' } else { 'no' }))"

        $subjectMatches = $publisher -and ($certificate.Subject -eq $publisher)
        Write-Host "  Subject = publisher: $(if ($subjectMatches) { 'yes' } else { 'no' })"
    }

    $verdict = @()
    if ($signature.Status -eq "Valid") {
        $verdict += "OK"
        $verdict += "The package is signed and this user trusts the signature."
    } elseif ($signature.Status -eq "NotSigned") {
        $verdict += "FAIL"
        $verdict += "The file carries no signature at all."
        $verdict += "Fix: .\scripts\sign-msix.ps1 -Msix `"$($package.FullName)`""
    } elseif (-not $certificate) {
        $verdict += "FAIL"
        $verdict += "The signature could not be read ($($signature.Status))."
        $verdict += "Fix: re-sign the package with .\scripts\sign-msix.ps1."
    } elseif (-not $subjectMatches) {
        $verdict += "FAIL"
        $verdict += "The certificate subject does not match the manifest publisher."
        $verdict += "Windows refuses to install such a package regardless of trust."
        $verdict += "Fix: .\scripts\make-test-cert.ps1 -Subject `"$publisher`" -Force, then re-sign."
    } elseif (-not $trusted) {
        $verdict += "FAIL"
        $verdict += "The signing certificate is not trusted by this user."
        $cer = Resolve-RepoPath $CerPath
        if (Test-Path $cer) {
            $verdict += "Fix: Import-Certificate -FilePath `"$cer`" -CertStoreLocation Cert:\CurrentUser\TrustedPeople"
        } else {
            $verdict += "Fix: import the certificate that signed the package into Cert:\CurrentUser\TrustedPeople,"
            $verdict += "     or re-run .\scripts\make-test-cert.ps1 without -NoTrust and sign again."
        }
    } else {
        $verdict += "FAIL"
        $verdict += "The certificate is trusted but the signature is still $($signature.Status)."
        $verdict += "Common causes: the file was modified after signing, or the certificate has expired."
        $verdict += "Fix: re-sign the package with .\scripts\sign-msix.ps1."
    }

    $colour = if ($verdict[0] -eq "OK") { "Green" } else { "Red" }
    Write-Host ""
    Write-Host "  $($verdict[0]): $($verdict[1])" -ForegroundColor $colour
    for ($i = 2; $i -lt $verdict.Count; $i++) {
        Write-Host "        $($verdict[$i])"
    }
    Write-Host ""

    if ($verdict[0] -ne "OK") { $problems++ }
}

if ($problems -gt 0) {
    Write-Warn "$problems of $($packages.Count) packages need attention."
    exit 1
}

Write-Host "All $($packages.Count) packages are signed and trusted." -ForegroundColor Green
