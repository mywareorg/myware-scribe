#Requires -Version 5.1
<#
.SYNOPSIS
Check the Windows GPU build without guessing which prerequisite is missing.

.DESCRIPTION
Runs read-only checks for the Vulkan SDK, Ninja, the MSVC toolchain, OpenMP
runtime, and stale whisper-rs CMake caches. Writes a report to
dist\gpu-test.txt and exits 1 when a hard prerequisite is missing.

Use -Probe to replay ggml's nested Vulkan shader configure, -Build to run the
full GPU build, and -Artifact to inspect a finished executable.

.EXAMPLE
.\scripts\test-gpu-build.ps1

.EXAMPLE
.\scripts\test-gpu-build.ps1 -Probe

.EXAMPLE
.\scripts\test-gpu-build.ps1 -Build -SkipTests -SkipModels -NoZip

.EXAMPLE
.\scripts\test-gpu-build.ps1 -Artifact dist\hippo-transcribe-0.1.0-windows-x64\hippo-transcribe.exe
#>
[CmdletBinding()]
param(
    [string]$OutDir = "dist",
    [switch]$Probe,
    [switch]$Build,
    [switch]$SkipTests,
    [switch]$SkipModels,
    [switch]$NoZip,
    [string]$Artifact = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "windows-env.ps1")

$script:Failures = 0
$script:Warnings = 0
$script:Lines = New-Object System.Collections.Generic.List[string]

function Add-TestLine {
    param(
        [Parameter(Mandatory = $true)][string]$Text,
        [ValidateSet("info", "pass", "warn", "fail")][string]$Kind = "info"
    )
    $script:Lines.Add($Text) | Out-Null
    switch ($Kind) {
        "pass" { Write-Host $Text -ForegroundColor Green }
        "warn" {
            Write-Host $Text -ForegroundColor Yellow
            $script:Warnings++
        }
        "fail" {
            Write-Host $Text -ForegroundColor Red
            $script:Failures++
        }
        default { Write-Host $Text -ForegroundColor Gray }
    }
}

function Test-PrereqCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Why,
        [switch]$Optional
    )
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $command) {
        if ($Optional) {
            Add-TestLine "WARN $Name is not on PATH ($Why)" "warn"
        } else {
            Add-TestLine "FAIL $Name was not found ($Why)" "fail"
        }
        return $null
    }
    Add-TestLine "PASS $Name -> $($command.Source)" "pass"
    return $command.Source
}

function Find-Cmake {
    param([string]$VsRoot)
    $command = Get-Command cmake.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    if ($VsRoot) {
        $candidate = Join-Path $VsRoot "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    return $null
}

function Find-Ninja {
    param([string]$VsRoot)
    $command = Get-Command ninja.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    if ($VsRoot) {
        $candidate = Join-Path $VsRoot "Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe"
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    return $null
}

function Get-OpenMpRuntime {
    param([string]$VsRoot)
    $found = @()
    if (-not $VsRoot) { return $found }
    foreach ($base in @(
        (Join-Path $VsRoot "VC\Redist\MSVC"),
        (Join-Path $VsRoot "VC\Tools\MSVC")
    )) {
        if (-not (Test-Path -LiteralPath $base)) { continue }
        $found += @(Get-ChildItem -LiteralPath $base -Recurse -Filter "vcomp140.dll" -ErrorAction SilentlyContinue |
            ForEach-Object { $_.FullName })
    }
    return $found
}

function Resolve-Artifact {
    param([string]$Explicit)
    if ($Explicit) {
        if (Test-Path -LiteralPath $Explicit) { return (Resolve-Path -LiteralPath $Explicit).Path }
        return $null
    }

    $distRoot = Join-Path $root $OutDir
    if (Test-Path -LiteralPath $distRoot) {
        $found = Get-ChildItem -LiteralPath $distRoot -Recurse -Filter "hippo-transcribe.exe" -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }

    $targetRoot = Resolve-TargetRoot -Root $root
    foreach ($name in @("hippo-transcribe.exe", "hippo-transcribe-gui.exe")) {
        $candidate = Join-Path $targetRoot "release\$name"
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    return $null
}

function Test-Artifact {
    param([Parameter(Mandatory = $true)][string]$Path)
    Add-TestLine "Artifact: $Path" "info"

    $dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if (-not $dumpbin) {
        Add-TestLine "WARN dumpbin.exe was not found; skipping dependency checks" "warn"
        return
    }

    $dependents = (& $dumpbin.Source /dependents $Path 2>&1 | Out-String)
    foreach ($expected in @("vcruntime140.dll", "vcomp140.dll")) {
        if ($dependents -match [regex]::Escape($expected)) {
            Add-TestLine "PASS artifact imports $expected" "pass"
        } elseif ($expected -eq "vcomp140.dll") {
            Add-TestLine "WARN artifact does not import vcomp140.dll; OpenMP may not be enabled" "warn"
        } else {
            Add-TestLine "FAIL artifact does not import $expected" "fail"
        }
    }
    if ($dependents -match "vulkan-1\.dll") {
        Add-TestLine "PASS artifact imports vulkan-1.dll" "pass"
    } else {
        Add-TestLine "WARN vulkan-1.dll is not a static dependency; ggml may load it dynamically" "warn"
    }

    $dir = Split-Path -Parent $Path
    foreach ($dll in @("vcomp140.dll", "sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        if (Test-Path -LiteralPath (Join-Path $dir $dll)) {
            Add-TestLine "PASS $dll is staged beside the executable" "pass"
        } else {
            Add-TestLine "FAIL $dll is missing beside the executable" "fail"
        }
    }
}

function Invoke-BuildScript {
    param([string[]]$Arguments)
    $scriptPath = Join-Path $PSScriptRoot "build-windows.ps1"
    $all = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $scriptPath) + $Arguments
    & powershell.exe @all
    return $LASTEXITCODE
}

Add-TestLine "GPU build readiness $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" "info"

if ($env:OS -eq "Windows_NT") {
    Add-TestLine "PASS running on Windows" "pass"
} else {
    Add-TestLine "FAIL this script must run on Windows" "fail"
}

$vsRoot = $null
try { $vsRoot = Get-VsInstallPath } catch { }
if ($vsRoot -and (Test-Path -LiteralPath $vsRoot)) {
    Add-TestLine "PASS Visual Studio at $vsRoot" "pass"
} else {
    Add-TestLine "FAIL Visual Studio with the C++ workload was not found" "fail"
}

$cl = Test-PrereqCommand "cl.exe" "Visual Studio C++ workload" -Optional
if ($cl) {
    if ($cl -match "Host(X64|ARM64)\\x64") {
        Add-TestLine "PASS cl.exe is the x64-hosted compiler" "pass"
    } else {
        Add-TestLine "FAIL cl.exe is not x64-hosted: $cl" "fail"
    }
}
Test-PrereqCommand "rc.exe" "Windows SDK resource compiler" -Optional | Out-Null
Test-PrereqCommand "cargo.exe" "Rust MSVC toolchain" | Out-Null

$rustc = Test-PrereqCommand "rustc.exe" "Rust MSVC toolchain"
if ($rustc) {
    $version = (& $rustc -vV | Out-String)
    if ($version -match "host:\s*x86_64-pc-windows-msvc") {
        Add-TestLine "PASS rustc host is x86_64-pc-windows-msvc" "pass"
    } else {
        Add-TestLine "FAIL rustc is not the MSVC toolchain:`n$version" "fail"
    }
}

$cmake = Find-Cmake -VsRoot $vsRoot
if ($cmake) {
    Add-TestLine "PASS cmake -> $cmake" "pass"
} else {
    Add-TestLine "FAIL cmake.exe was not found" "fail"
}

$ninja = Find-Ninja -VsRoot $vsRoot
if ($ninja) {
    Add-TestLine "PASS ninja -> $ninja" "pass"
} else {
    Add-TestLine "FAIL ninja.exe was not found; the Vulkan shader generator needs Ninja" "fail"
}

$sdk = Resolve-VulkanSdk
if (-not $sdk) {
    Add-TestLine "FAIL Vulkan SDK was not found; install it from LunarG or set VULKAN_SDK" "fail"
} else {
    Add-TestLine "PASS Vulkan SDK at $sdk" "pass"
    foreach ($relative in @(
        "Include\vulkan\vulkan.h",
        "Lib\vulkan-1.lib",
        "Bin\glslc.exe"
    )) {
        $path = Join-Path $sdk $relative
        if (Test-Path -LiteralPath $path) {
            Add-TestLine "PASS $relative" "pass"
        } else {
            Add-TestLine "FAIL $relative is missing from the Vulkan SDK" "fail"
        }
    }
}

$openMp = @(Get-OpenMpRuntime -VsRoot $vsRoot)
if ($openMp.Count -gt 0) {
    Add-TestLine "PASS vcomp140.dll -> $($openMp[0])" "pass"
} else {
    Add-TestLine "WARN vcomp140.dll was not found; the GPU build may not run on a clean machine" "warn"
}

$vulkanRuntime = Join-Path $env:SystemRoot "System32\vulkan-1.dll"
if (Test-Path -LiteralPath $vulkanRuntime) {
    Add-TestLine "PASS vulkan-1.dll is installed by the graphics driver" "pass"
} else {
    Add-TestLine "WARN vulkan-1.dll is not installed; build can succeed but the app will fall back to the CPU" "warn"
}

$targetRoot = Resolve-TargetRoot -Root $root
$buildRoot = Join-Path $targetRoot "release\build"
$caches = @()
if (Test-Path -LiteralPath $buildRoot) {
    $caches = @(Get-ChildItem -LiteralPath $buildRoot -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName "out\build\CMakeCache.txt" } |
        Where-Object { Test-Path -LiteralPath $_ })
}
if ($caches.Count -eq 0) {
    Add-TestLine "INFO no whisper-rs-sys CMake cache in $buildRoot" "info"
} else {
    foreach ($cache in $caches) {
        $line = Get-Content -LiteralPath $cache -ErrorAction SilentlyContinue |
            Where-Object { $_ -like "CMAKE_GENERATOR:*" } |
            Select-Object -First 1
        $generator = if ($line) { ($line -split "=", 2)[1] } else { "unknown" }
        if ($generator -match "Visual Studio") {
            Add-TestLine "WARN stale Visual Studio cache in $cache; the build script discards it for GPU builds" "warn"
        } else {
            Add-TestLine "PASS CMake generator $generator in $cache" "pass"
        }
    }
}

if ($Probe) {
    Add-TestLine "Running the ggml Vulkan shader probe" "info"
    $code = Invoke-BuildScript -Arguments @("-Probe")
    if ($code -eq 0) {
        Add-TestLine "PASS shader probe completed" "pass"
    } else {
        Add-TestLine "FAIL shader probe exited $code; run scripts\diagnose-gpu-build.ps1" "fail"
    }
}

if ($Build) {
    Add-TestLine "Building with -Gpu" "info"
    $buildArgs = @("-Gpu")
    if ($SkipTests) { $buildArgs += "-SkipTests" }
    if ($SkipModels) { $buildArgs += "-SkipModels" }
    if ($NoZip) { $buildArgs += "-NoZip" }
    $code = Invoke-BuildScript -Arguments $buildArgs
    if ($code -eq 0) {
        Add-TestLine "PASS GPU build completed" "pass"
    } else {
        Add-TestLine "FAIL GPU build exited $code" "fail"
    }
}

if ($Build -or $Artifact) {
    $exe = Resolve-Artifact -Explicit $Artifact
    if ($exe) {
        Test-Artifact -Path $exe
    } else {
        Add-TestLine "FAIL no built executable was found; pass -Artifact <path>" "fail"
    }
}

$reportDir = Join-Path $root $OutDir
New-Item -ItemType Directory -Path $reportDir -Force | Out-Null
$report = Join-Path $reportDir "gpu-test.txt"
$script:Lines | Set-Content -LiteralPath $report -Encoding UTF8

Write-Host ""
Write-Host "Report: $report"
if ($script:Failures -gt 0) {
    Write-Host "FAILED: $($script:Failures) hard failure(s), $($script:Warnings) warning(s)" -ForegroundColor Red
    exit 1
}
Write-Host "PASSED with $($script:Warnings) warning(s)" -ForegroundColor Green
exit 0
