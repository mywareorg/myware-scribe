#Requires -Version 5.1
<#
.SYNOPSIS
Run Hippo Transcribe on Windows with the fast iteration profile.

.DESCRIPTION
Enters the Visual Studio developer environment if needed, points bindgen at
LLVM, builds the briefing worker (`hippo-summarize`) into the same profile, and
runs `cargo run --profile release-fast`.

Use this for the edit/run loop. Use `scripts/build-windows.ps1` for the final
LTO release and zip.

Pass -Free or -Pro to force the entitlement for this run. Without either
switch the persisted license/trial decides the tier.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-windows.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-windows.ps1 -Free

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-windows.ps1 -Pro
#>
[CmdletBinding()]
param(
    [string]$Profile = "release-fast",
    [switch]$Free,
    [switch]$Pro
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($Free -and $Pro) {
    throw "Choose either -Free or -Pro, not both."
}

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if ($Free) {
        $env:HIPPO_TRANSCRIBE_TIER = "free"
        Write-Step "Testing the Free tier"
    } elseif ($Pro) {
        $env:HIPPO_TRANSCRIBE_TIER = "pro"
        Write-Step "Testing the Pro tier"
    }

    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        Enter-DevEnvironment
    }
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        throw "cl.exe was not found. Install the Visual Studio C++ workload."
    }

    $libClang = Resolve-LibClang
    if ($libClang) {
        $env:LIBCLANG_PATH = $libClang
    } elseif (-not $env:WHISPER_DONT_GENERATE_BINDINGS) {
        Write-Warn "LLVM was not found; using the bundled whisper bindings for this run."
        $env:WHISPER_DONT_GENERATE_BINDINGS = "1"
    }

    # Cargo runs the binary from the profile directory, so stage the runtime
    # DLLs there. The release zip places them beside the exe instead.
    $sherpaLib = Ensure-SherpaDlls
    $json = (& cargo metadata --no-deps --format-version 1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $metadata = $json | ConvertFrom-Json
    $dirName = switch ($Profile) {
        "dev" { "debug" }
        "test" { "debug" }
        default { $Profile }
    }
    $profileDir = Join-Path $metadata.target_directory $dirName
    New-Item -ItemType Directory -Path $profileDir -Force | Out-Null
    foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        Copy-Item (Join-Path $sherpaLib $dll) $profileDir -Force
    }

    # The briefing worker is a separate package and is not a workspace default
    # member, so `cargo run` alone never builds it. The app looks for it beside
    # its own executable and Summarize Transcript fails without it.
    & cargo build --profile $Profile -p hippo-summarize
    if ($LASTEXITCODE -ne 0) {
        throw "building hippo-summarize failed with exit code $LASTEXITCODE"
    }

    & cargo run --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw "cargo run failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}
