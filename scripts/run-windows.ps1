#Requires -Version 5.1
<#
.SYNOPSIS
Run transiel on Windows with the fast iteration profile.

.DESCRIPTION
Enters the Visual Studio developer environment if needed, points bindgen at
LLVM, selects the static CRT used by the prebuilt sherpa-onnx libraries, and
runs `cargo run --profile release-fast`.

Use this for the edit/run loop. Use `scripts/build-windows.ps1` for the final
LTO release and zip.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-windows.ps1
#>
[CmdletBinding()]
param(
    [string]$Profile = "release-fast"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-env.ps1")

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
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

    & cargo run --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw "cargo run failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}
