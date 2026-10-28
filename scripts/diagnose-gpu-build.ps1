#Requires -Version 5.1
<#
.SYNOPSIS
  Collects the facts needed to explain a failed `-features gpu` build.

.DESCRIPTION
  A Vulkan build compiles ggml from source through CMake, and ggml generates its
  shader sources with a nested CMake project (vulkan-shaders-gen). That nested
  project inherits the generator and compiler of the outer configure, so when the
  outer project used a Visual Studio generator the sub-project is left without a
  CMAKE_C_COMPILER and fails with "No CMAKE_C_COMPILER could be found."

  This script prints the toolchain, the generator and compiler recorded in each
  CMakeCache.txt, and the shader sub-build's own output, so the failing layer is
  identifiable without guesswork.

  Nothing is built or modified. Output is written to dist\gpu-diagnose.txt so the
  file can be committed and pushed instead of retyped.

.PARAMETER Log
  Build log to mine for CMake errors. Default: build-windows.log at the repo root.

.PARAMETER KeepTarget
  Keep the full cache listing instead of summarising shader sub-project caches.
#>
[CmdletBinding()]
param(
    [string]$Log = "build-windows.log",
    [switch]$KeepTarget
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $root "dist"
New-Item -ItemType Directory -Path $outDir -Force | Out-Null
$out = Join-Path $outDir "gpu-diagnose.txt"
if (Test-Path $out) { Remove-Item $out -Force }

function Say {
    param([string]$Text)
    Add-Content -Path $out -Value $Text -Encoding UTF8
    Write-Host $Text
}

function Section {
    param([string]$Title)
    Say ""
    Say ("=== " + $Title + " ===")
}

function Show-Tool {
    param([string]$Name)
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) {
        Say ("  " + $Name.PadRight(12) + $cmd.Source)
    } else {
        Say ("  " + $Name.PadRight(12) + "(not on PATH)")
    }
}

function Show-Var {
    param([string]$Name, [string]$Value)
    if ([string]::IsNullOrEmpty($Value)) { $Value = "(unset)" }
    Say ("  " + $Name.PadRight(16) + $Value)
}

Say "Hippo Transcribe GPU build diagnostics"
Say ("  host           " + [System.Environment]::MachineName)
Say ("  powershell     " + $PSVersionTable.PSVersion.ToString())
Say ("  repo           " + $root)

Section "Toolchain"
foreach ($tool in @("cargo", "rustc", "cmake", "ninja", "cl", "link", "glslc", "makeappx")) {
    Show-Tool $tool
}

Section "Environment"
Show-Var "VULKAN_SDK" $env:VULKAN_SDK
Show-Var "VSCMD_VER" $env:VSCMD_VER
Show-Var "VCINSTALLDIR" $env:VCINSTALLDIR
Show-Var "CMAKE_GENERATOR" $env:CMAKE_GENERATOR
Show-Var "RUSTFLAGS" $env:RUSTFLAGS
Show-Var "CC" $env:CC
Show-Var "CXX" $env:CXX
foreach ($sdk in @("C:\VulkanSDK", (Join-Path $env:ProgramFiles "VulkanSDK"))) {
    if (Test-Path $sdk) {
        $versions = Get-ChildItem $sdk -Directory -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty Name
        Say ("  " + $sdk + " -> " + ($versions -join ", "))
    }
}

Section "CMake caches"
$buildRoot = Join-Path $root "target\release\build"
$caches = @()
if (Test-Path $buildRoot) {
    $caches = @(Get-ChildItem $buildRoot -Recurse -Filter "CMakeCache.txt" -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "whisper-rs-sys" })
}
if ($caches.Count -eq 0) {
    Say "  none under target\release\build"
} else {
    $wanted = @("CMAKE_GENERATOR:", "CMAKE_C_COMPILER:", "CMAKE_CXX_COMPILER:",
                "GGML_VULKAN:", "VULKAN_SDK:", "Vulkan_FOUND:", "GGML_NATIVE:")
    foreach ($cache in $caches) {
        Say ""
        Say ("  " + $cache.FullName)
        foreach ($line in (Get-Content -Path $cache.FullName -ErrorAction SilentlyContinue)) {
            foreach ($key in $wanted) {
                if ($line.StartsWith($key)) { Say ("    " + $line) }
            }
        }
    }
}

Section "Shader sub-build output"
$stamps = @()
if (Test-Path $buildRoot) {
    $stamps = @(Get-ChildItem $buildRoot -Recurse -Directory -Filter "vulkan-shaders-gen-stamp" -ErrorAction SilentlyContinue)
}
if ($stamps.Count -eq 0) {
    Say "  no vulkan-shaders-gen-stamp directory: the shader project never configured"
} else {
    foreach ($stamp in $stamps) {
        Say ""
        Say ("  " + $stamp.FullName)
        $logs = @(Get-ChildItem $stamp.Parent.FullName -Recurse -Filter "*.log" -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 3)
        foreach ($entry in $logs) {
            Say ("    " + $entry.Name)
            foreach ($line in (Get-Content -Path $entry.FullName -Tail 20 -ErrorAction SilentlyContinue)) {
                Say ("      " + $line)
            }
        }
    }
}

Section "Build log"
$logPath = $Log
if (-not [System.IO.Path]::IsPathRooted($logPath)) { $logPath = Join-Path $root $Log }
if (-not (Test-Path $logPath)) {
    Say ("  " + $logPath + " not found")
} else {
    Say ("  " + $logPath)
    $hits = @(Select-String -Path $logPath -Pattern "CMake Error|No CMAKE_|CMAKE_C_COMPILER|cannot be run in parallel|vulkan-shaders|error MSB|error C" -ErrorAction SilentlyContinue |
        Select-Object -First 40)
    if ($hits.Count -eq 0) {
        Say "  no matching lines"
    } else {
        foreach ($hit in $hits) {
            Say ("    " + $hit.LineNumber + ": " + $hit.Line.Trim())
        }
    }
}

Section "Verdict"
$generators = @()
foreach ($cache in $caches) {
    $line = Get-Content -Path $cache.FullName -ErrorAction SilentlyContinue |
        Where-Object { $_.StartsWith("CMAKE_GENERATOR:") } |
        Select-Object -First 1
    if ($line) { $generators += ($line -split "=")[1] }
}
$generators = @($generators | Sort-Object -Unique)
if ($generators -match "Visual Studio") {
    Say ("  Generator: " + ($generators -join ", "))
    Say "  ggml's vulkan-shaders-gen sub-project inherits no CMAKE_C_COMPILER from a"
    Say "  Visual Studio generator, which is the usual cause of the configure failure."
    Say "  Retry with Ninja, from a Visual Studio developer shell:"
    Say ""
    Say '    $env:CMAKE_GENERATOR = "Ninja"'
    Say '    Remove-Item -Recurse -Force target\release\build\whisper-rs-sys-*'
    Say '    .\scripts\build-windows.ps1 -SkipTests -SkipModels -NoZip -Msix -Sign -Gpu -CertificatePath C:\keys\hippo-test.pfx -CertificatePassword hippo-dev'
} elseif ($generators.Count -gt 0) {
    Say ("  Generator: " + ($generators -join ", "))
} else {
    Say "  No CMake cache found: the configure step never ran, or ran in another directory."
}
Say ""
Say ("Full report: " + $out)
