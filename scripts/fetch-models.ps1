#Requires -Version 5.1
<#
.SYNOPSIS
Fetch the pinned model files into vendor\models for packaging.

.DESCRIPTION
Reads models.lock.json, the same manifest the application compiles in, so
build-time staging and runtime downloads can never disagree. Files are verified
by exact size and SHA-256; a cached valid copy is reused.

.PARAMETER Dest
Destination root. Defaults to vendor\models; the manifest version is appended.

.PARAMETER Mirror
Try $Mirror\<file> before the pinned upstream mirrors. Defaults to the
MYWARE_SCRIBE_MODEL_MIRROR environment variable.

.PARAMETER All
Also fetch models that release packages download on demand (the large
accuracy model).

.PARAMETER Force
Re-download even when the cached copy is valid.

.PARAMETER VerifyOnly
Check the cache without touching the network.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\fetch-models.ps1 -All -VerifyOnly
#>
[CmdletBinding()]
param(
    [string]$Dest,
    [string]$Mirror = $env:MYWARE_SCRIBE_MODEL_MIRROR,
    [switch]$All,
    [switch]$Force,
    [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path $PSScriptRoot -Parent
$manifestPath = Join-Path $root "models.lock.json"
if (-not (Test-Path $manifestPath)) {
    throw "missing $manifestPath"
}
$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json

if (-not $Dest) {
    $Dest = Join-Path $root "vendor\models\$($manifest.version)"
}
New-Item -ItemType Directory -Path $Dest -Force | Out-Null

$curl = Get-Command curl.exe -ErrorAction SilentlyContinue

function Test-Model {
    param([string]$Path, [long]$Size, [string]$Sha256)
    if (-not (Test-Path $Path)) { return $false }
    $item = Get-Item $Path
    if ($item.Length -ne $Size) { return $false }
    $hash = (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    return $hash -eq $Sha256
}

$ok = 0
$failed = 0

# Reuse any already-verified copy before touching the network. This makes
# repeat builds and offline VMs work after one successful fetch, and lets an
# existing install seed a package.
$seedDirs = @()
$localModels = if ($env:LOCALAPPDATA) {
    Join-Path $env:LOCALAPPDATA "myware-scribe\models"
} else {
    $null
}
foreach ($candidate in @(
    (Join-Path $root "vendor\models\$($manifest.version)"),
    $env:MYWARE_SCRIBE_MODELS_DIR,
    $env:TRANSIEL_MODELS_DIR,
    $localModels
)) {
    if (-not $candidate) { continue }
    $full = [System.IO.Path]::GetFullPath($candidate)
    if ((Test-Path -LiteralPath $full) -and
        ($full.TrimEnd('\') -ine ([System.IO.Path]::GetFullPath($Dest)).TrimEnd('\'))) {
        $seedDirs += $full
    }
}

foreach ($model in $manifest.models) {
    if (-not $All -and -not $model.bundled) {
        continue
    }

    $relative = $model.file -replace '/', [System.IO.Path]::DirectorySeparatorChar
    $target = Join-Path $Dest $relative
    $name = Split-Path $target -Leaf

    if (-not $Force -and (Test-Model -Path $target -Size $model.size -Sha256 $model.sha256)) {
        Write-Host "[ok]   $($model.id) ($name)" -ForegroundColor DarkGray
        $ok++
        continue
    }

    if (-not $Force) {
        $seeded = $false
        foreach ($seed in $seedDirs) {
            $candidate = Join-Path $seed $relative
            if (Test-Model -Path $candidate -Size $model.size -Sha256 $model.sha256) {
                New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
                Copy-Item -LiteralPath $candidate -Destination $target -Force
                Write-Host "[ok]   $($model.id) <- cached $candidate" -ForegroundColor Green
                $ok++
                $seeded = $true
                break
            }
        }
        if ($seeded) { continue }
    }

    if ($VerifyOnly) {
        Write-Host "[bad]  $($model.id) -> $target" -ForegroundColor Red
        $failed++
        continue
    }

    New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null

    $urls = @()
    if ($Mirror) {
        $urls += ("{0}/{1}" -f $Mirror.TrimEnd('/'), $name)
    }
    $urls += $model.mirrors[0]

    $part = "$target.part"
    $downloaded = $false
    foreach ($url in $urls) {
        Write-Host "[get]  $($model.id) <- $url"
        if (Test-Path $part) { Remove-Item $part -Force }
        try {
            if ($curl) {
                & $curl.Source --fail --location --retry 3 --silent --show-error -C - --output $part $url
                if ($LASTEXITCODE -ne 0) { throw "curl exited $LASTEXITCODE" }
            } else {
                Invoke-WebRequest -Uri $url -OutFile $part -UseBasicParsing
            }
            if (-not (Test-Model -Path $part -Size $model.size -Sha256 $model.sha256)) {
                throw "size or sha256 mismatch"
            }
            Move-Item -Path $part -Destination $target -Force
            $downloaded = $true
            break
        } catch {
            Write-Host "[warn] mirror failed: $($_.Exception.Message)" -ForegroundColor Yellow
            if (Test-Path $part) { Remove-Item $part -Force }
        }
    }

    if ($downloaded) {
        Write-Host "[ok]   $($model.id) -> $target" -ForegroundColor Green
        $ok++
    } else {
        Write-Host "[fail] $($model.id): every mirror failed" -ForegroundColor Red
        $failed++
    }
}

Write-Host "model set v$($manifest.version): $ok ok, $failed failed ($Dest)"
if ($failed -gt 0) {
    throw "$failed model file(s) could not be staged"
}
