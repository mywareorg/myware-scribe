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
    } elseif (-not $env:SHERPA_SKIP_GENERATE_BINDINGS) {
        Write-Warn "LLVM was not found; using the bundled sherpa bindings for this run."
        $env:SHERPA_SKIP_GENERATE_BINDINGS = "1"
    }

    $env:SHERPA_STATIC_CRT = "1"
    & cargo run --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw "cargo run failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}
