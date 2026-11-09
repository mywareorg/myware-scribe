#Requires -Version 5.1
<#
.SYNOPSIS
Run Hippo Transcribe on Windows with the GPU backend and fast iteration profile.

.DESCRIPTION
Enters the Visual Studio developer environment if needed, points bindgen at
LLVM, selects the Vulkan SDK and Ninja generator, stages the sherpa-onnx,
ONNX Runtime, Visual C++ and OpenMP DLLs beside the development binary, and
builds the briefing worker into the same profile, then runs `cargo run
--profile release-fast --features gpu`.

The GPU feature is Pro-gated, so this sets HIPPO_TRANSCRIBE_TIER=pro by
default. Pass -Free to test the free-tier lock instead.

Use this for the GPU edit/run loop. Use `scripts/build-windows.ps1 -Gpu` for
the final LTO release and packaging.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-gpu.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-gpu.ps1 -Free

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\run-gpu.ps1 -Pro
#>
[CmdletBinding()]
param(
    [string]$Profile = "release-fast",
    [string]$TargetDir,
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

    $vulkanSdk = Resolve-VulkanSdk
    if (-not $vulkanSdk) {
        throw "Vulkan SDK not found. Install it from LunarG, or use .\scripts\run-windows.ps1 for the CPU build."
    }
    $env:VULKAN_SDK = $vulkanSdk

    # ggml's Vulkan shader generator is a nested CMake project and needs the
    # Ninja generator. The DLL CRT matches whisper.cpp's CMake default.
    $env:CMAKE_GENERATOR = "Ninja"
    $env:CMAKE_MSVC_RUNTIME_LIBRARY = "MultiThreadedDLL"

    if ($Free) {
        $env:HIPPO_TRANSCRIBE_TIER = "free"
    } else {
        $env:HIPPO_TRANSCRIBE_TIER = "pro"
    }

    # Use the short profile-root target directory by default; ggml's nested
    # shader object paths exceed CMake's object path limit in a deep checkout.
    $targetRoot = Resolve-TargetRoot -Root $root -TargetDir $TargetDir
    $env:CARGO_TARGET_DIR = $targetRoot
    Write-Step "Building in $targetRoot"

    $metadataJson = (& cargo metadata --no-deps --format-version 1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $metadata = $metadataJson | ConvertFrom-Json
    $dirName = switch ($Profile) {
        "dev" { "debug" }
        "test" { "debug" }
        default { $Profile }
    }
    $profileDir = Join-Path $metadata.target_directory $dirName
    New-Item -ItemType Directory -Path $profileDir -Force | Out-Null

    $sherpaLib = Ensure-SherpaDlls
    foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        Copy-Item (Join-Path $sherpaLib $dll) $profileDir -Force
    }

    $vcRuntime = Resolve-VcRuntime
    foreach ($dll in @("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll")) {
        if (-not $vcRuntime) {
            Write-Warn "Skipping $dll; the Visual C++ runtime was not found."
            continue
        }
        $source = Join-Path $vcRuntime $dll
        if (Test-Path -LiteralPath $source) {
            Copy-Item $source $profileDir -Force
        }
    }

    $openMp = Resolve-VcOpenMp -VcRuntime $vcRuntime
    if (-not $openMp) {
        throw "vcomp140.dll was not found; the GPU build needs the Visual C++ OpenMP runtime."
    }
    $vcomp = Join-Path $openMp "vcomp140.dll"
    if (-not (Test-Path -LiteralPath $vcomp)) {
        throw "vcomp140.dll was not found in $openMp."
    }
    Copy-Item $vcomp $profileDir -Force

    # Built here for the same reason: the briefing worker has its own package.
    & cargo build --profile $Profile -p hippo-summarize
    if ($LASTEXITCODE -ne 0) {
        throw "building hippo-summarize failed with exit code $LASTEXITCODE"
    }

    & cargo run --profile $Profile --features gpu
    if ($LASTEXITCODE -ne 0) {
        throw "cargo run failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}
