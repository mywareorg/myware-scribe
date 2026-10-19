#Requires -Version 5.1
<#
.SYNOPSIS
Build transiel for Windows, stage a release folder, and optionally zip it.

.DESCRIPTION
Enters the Visual Studio developer environment (so cl.exe, rc.exe, the Windows
SDK, and CMake are on PATH), configures the MSVC CRT (dynamic by default, or
/MT with -StaticCrt), adds the Windows system libraries the native dependencies
need, builds the release binary, checks its runtime dependencies, stages
deploy.bat/undeploy.bat/README/LICENSE plus the sample audio, and writes a zip
with a SHA-256 checksum.

Requires:
  - Visual Studio 2022 Build Tools with "Desktop development with C++"
  - Rust stable MSVC (rustup default stable-x86_64-pc-windows-msvc)
  - LLVM (winget install LLVM.LLVM) unless -SkipBindgen is used
  - CMake (included with the VS C++ workload, or winget install Kitware.CMake)

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1

.EXAMPLE
.\scripts\build-windows.ps1 -SkipTests -Version 0.2.0 -OutDir C:\builds
#>
[CmdletBinding()]
param(
    [string]$OutDir = "dist",
    [string]$Version,
    [string]$TargetDir,
    [switch]$SkipTests,
    [switch]$SkipBindgen,
    [switch]$StaticCrt,
    [switch]$Clean,
    [switch]$NoZip
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Warn {
    param([string]$Message)
    Write-Warning $Message
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @()
    )
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath failed with exit code $LASTEXITCODE"
    }
}

function Get-VsInstallPath {
    $base = ${env:ProgramFiles(x86)}
    if (-not $base) { $base = "C:\Program Files (x86)" }
    $vswhere = Join-Path $base "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        throw "vswhere.exe was not found. Install Visual Studio 2022 Build Tools with the 'Desktop development with C++' workload."
    }
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $path) {
        throw "No Visual Studio installation with the C++ build tools was found. Add the 'Desktop development with C++' workload."
    }
    return $path.Trim()
}

function Enter-DevEnvironment {
    $vs = Get-VsInstallPath
    $cmd = Join-Path $vs "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path $cmd)) {
        throw "VsDevCmd.bat was not found under $vs"
    }
    Write-Step "Entering the Visual Studio developer environment"
    # Import the environment into this process. Dot-sourcing Launch-VsDevShell
    # from a function would scope the PATH changes to the function.
    $commandLine = '""{0}" -arch=x64 -host_arch=x64 >nul && set"' -f $cmd
    $lines = & cmd.exe /c $commandLine
    if ($LASTEXITCODE -ne 0) {
        throw "VsDevCmd.bat failed with exit code $LASTEXITCODE"
    }
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
        }
    }
}

function Resolve-LibClang {
    if ($env:LIBCLANG_PATH -and (Test-Path (Join-Path $env:LIBCLANG_PATH "libclang.dll"))) {
        return $env:LIBCLANG_PATH
    }
    $programFiles = ${env:ProgramFiles}
    if (-not $programFiles) { $programFiles = "C:\Program Files" }
    $programW6432 = ${env:ProgramW6432}
    $candidates = @(
        (Join-Path $programFiles "LLVM\bin"),
        "C:\Program Files\LLVM\bin"
    )
    if ($programW6432) { $candidates += (Join-Path $programW6432 "LLVM\bin") }
    $found = @($candidates | Where-Object {
            $_ -and (Test-Path (Join-Path $_ "libclang.dll"))
        }) | Select-Object -First 1
    if ($found) { return $found }
    return $null
}

function Add-CMakeIfNeeded {
    if (Get-Command cmake.exe -ErrorAction SilentlyContinue) { return }
    $candidate = "C:\Program Files\CMake\bin"
    if (Test-Path (Join-Path $candidate "cmake.exe")) {
        $env:PATH = "$candidate;$env:PATH"
    }
}

function Get-CargoMetadata {
    $json = (& cargo metadata --no-deps --format-version 1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    return ($json | ConvertFrom-Json)
}

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir

Push-Location $root
try {
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        Enter-DevEnvironment
    }
    Add-CMakeIfNeeded

    foreach ($command in @("cl.exe", "rc.exe", "cmake.exe", "cargo.exe", "rustc.exe")) {
        if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
            throw "$command was not found. Use a machine with the Visual Studio C++ workload and the Rust MSVC toolchain."
        }
    }

    $hostLine = (& rustc -vV) | Where-Object { $_ -like "host:*" } | Select-Object -First 1
    if ($hostLine -notmatch "windows-msvc") {
        throw "The active Rust toolchain is not MSVC ($hostLine). Run: rustup default stable-x86_64-pc-windows-msvc"
    }

    $libClang = Resolve-LibClang
    if ($libClang) {
        $env:LIBCLANG_PATH = $libClang
        Write-Step "Using libclang at $libClang"
    } elseif ($SkipBindgen) {
        Write-Warn "LLVM was not found; falling back to the bundled sherpa bindings."
        $env:SHERPA_SKIP_GENERATE_BINDINGS = "1"
    } else {
        throw "libclang was not found. Install LLVM (winget install LLVM.LLVM) or pass -SkipBindgen."
    }

    # The prebuilt ONNX Runtime static library is built against the dynamic
    # CRT (/MD). Use the dynamic CRT by default so the native libraries and
    # Rust agree; otherwise link.exe reports LNK2005/LNK1169 duplicate runtime
    # symbols. Pass -StaticCrt only when every native library is /MT.
    $env:SHERPA_STATIC_CRT = if ($StaticCrt) { "1" } else { "0" }
    $rustFlags = @()
    if ($env:RUSTFLAGS) {
        # Preserve extra flags, but drop any existing crt-static selection so
        # this script fully controls the CRT mode.
        $tokens = @($env:RUSTFLAGS -split '\s+' | Where-Object { $_ })
        for ($i = 0; $i -lt $tokens.Count; $i++) {
            $token = $tokens[$i]
            if ($token -eq "-C" -and ($i + 1) -lt $tokens.Count -and
                $tokens[$i + 1] -match 'crt-static') {
                $i++
                continue
            }
            if ($token -match 'crt-static') { continue }
            $rustFlags += $token
        }
    }
    if ($StaticCrt) {
        $rustFlags += @("-C", "target-feature=+crt-static")
    }
    # sherpa-onnx's static libraries do not carry their Windows system
    # dependencies, so pass the libraries ONNX Runtime (ETW/registry) and
    # PortAudio need to the final link.
    foreach ($lib in @("advapi32", "ole32", "oleaut32", "uuid", "winmm")) {
        $rustFlags += @("-C", "link-arg=$lib.lib")
    }
    $env:RUSTFLAGS = $rustFlags -join " "
    if ($TargetDir) {
        $env:CARGO_TARGET_DIR = $TargetDir
    }

    if ($Clean) {
        Write-Step "Cleaning previous build output"
        Invoke-Native cargo @("clean")
    }

    if (-not $SkipTests) {
        Write-Step "Running tests"
        Invoke-Native cargo @("test", "--workspace")
    }

    Write-Step "Building release binary"
    Invoke-Native cargo @("build", "--release", "--locked")

    $metadata = Get-CargoMetadata
    $package = $metadata.packages | Where-Object { $_.name -eq "transiel-gui" } | Select-Object -First 1
    if ($Version) {
        $releaseVersion = $Version
    } elseif ($package) {
        $releaseVersion = $package.version
    } else {
        $releaseVersion = "dev"
    }

    $binary = Join-Path $metadata.target_directory "release\transiel.exe"
    if (-not (Test-Path $binary)) {
        $fallback = Join-Path $metadata.target_directory "release\transiel-gui.exe"
        if (Test-Path $fallback) {
            $binary = $fallback
        } else {
            throw "Release binary not found under $($metadata.target_directory)\release"
        }
    }

    $dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($dumpbin) {
        $dependencies = & $dumpbin.Source /dependents $binary | Out-String
        if ($dependencies -match "VCRUNTIME140|MSVCP140") {
            if ($StaticCrt) {
                Write-Warn "The executable still depends on the dynamic C++ runtime despite -StaticCrt."
            } else {
                Write-Step "Using the dynamic C++ runtime (VC++ Redistributable required)."
            }
        }
    }

    $outRoot = if ([System.IO.Path]::IsPathRooted($OutDir)) {
        $OutDir
    } else {
        Join-Path $root $OutDir
    }
    New-Item -ItemType Directory -Path $outRoot -Force | Out-Null

    $name = "transiel-$releaseVersion-windows-x64"
    $stage = Join-Path $outRoot $name
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null

    Copy-Item $binary (Join-Path $stage "transiel.exe")
    foreach ($file in @("deploy.bat", "undeploy.bat", "README.md", "LICENSE")) {
        $source = Join-Path $root $file
        if (Test-Path $source) {
            Copy-Item $source (Join-Path $stage $file)
        }
    }
    $sample = Join-Path $root "audio\sample-short.mp3"
    if (Test-Path $sample) {
        Copy-Item $sample (Join-Path $stage "sample-short.mp3")
    }

    $zip = Join-Path $outRoot "$name.zip"
    if (-not $NoZip) {
        Write-Step "Writing $zip"
        if (Test-Path $zip) { Remove-Item $zip -Force }
        Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path $zip -Leaf)" | Set-Content "$zip.sha256" -Encoding ASCII
    }

    Write-Host ""
    Write-Host "Build complete" -ForegroundColor Green
    Write-Host "  Binary: $binary"
    Write-Host "  Stage:  $stage"
    if (-not $NoZip) { Write-Host "  Zip:    $zip" }
}
finally {
    Pop-Location
}
