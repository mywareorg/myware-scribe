#Requires -Version 5.1
<#
.SYNOPSIS
Build transiel for Windows, stage a release folder, and optionally zip it.

.DESCRIPTION
Enters the Visual Studio developer environment (so cl.exe, rc.exe, the Windows
SDK, and CMake are on PATH), configures the static CRT for the executable,
stages the sherpa-onnx shared DLLs beside it, builds the release binary, checks
its runtime dependencies, stages deploy.bat/undeploy.bat/README/LICENSE plus
the sample audio, and writes a zip with a SHA-256 checksum.

The app loads sherpa-onnx and ONNX Runtime from the shipped DLLs at runtime, so
only whisper.cpp is compiled locally. Tests run in release mode because the
debug profile links msvcrtd on Windows.

Every cargo command is teed to build-windows.log in the repository root. If a
build fails, the failing command line and the tail of that log are printed.

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
    [switch]$Diagnose,
    [switch]$Clean,
    [switch]$NoZip
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# PowerShell 7.3+ can turn native stderr into a terminating error. We check
# $LASTEXITCODE ourselves, so keep that behavior off when the switch exists.
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

. (Join-Path $PSScriptRoot "windows-env.ps1")

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @()
    )

    $commandLine = (@($FilePath) + $Arguments) -join " "
    Write-Host "    $commandLine" -ForegroundColor DarkGray

    # Every native command is teed to one log so a failure leaves the real
    # cargo/rustc error behind instead of only the rethrown exit code.
    $log = Join-Path $root "build-windows.log"
    $savedPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $FilePath @Arguments 2>&1 | Tee-Object -FilePath $log -Append
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedPreference
    }

    if ($exitCode -ne 0) {
        $tail = @(Get-Content -Path $log -Tail 60 -ErrorAction SilentlyContinue)
        if ($tail.Count -gt 0) {
            Write-Host ""
            Write-Host "Last lines of ${log}:" -ForegroundColor Yellow
            foreach ($line in $tail) {
                Write-Host "    $line"
            }
            Write-Host ""
        }
        throw "$commandLine failed with exit code $exitCode (full output in $log)"
    }
}

# Environment helpers live in windows-env.ps1.

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
    Remove-Item (Join-Path $root "build-windows.log") -Force -ErrorAction SilentlyContinue

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

    if ($SkipBindgen) {
        $env:WHISPER_DONT_GENERATE_BINDINGS = "1"
    } else {
        $libClang = Resolve-LibClang
        if ($libClang) {
            $env:LIBCLANG_PATH = $libClang
            Write-Step "Using libclang at $libClang"
        } else {
            Write-Warn "LLVM was not found; using the bundled whisper bindings."
            $env:WHISPER_DONT_GENERATE_BINDINGS = "1"
        }
    }

    # Keep the executable self-contained. sherpa-onnx and ONNX Runtime are
    # loaded from DLLs at runtime, so no native static libraries are linked.
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
    $rustFlags += @("-C", "target-feature=+crt-static")
    $env:RUSTFLAGS = $rustFlags -join " "
    if ($TargetDir) {
        $env:CARGO_TARGET_DIR = $TargetDir
    }

    if ($Clean) {
        Write-Step "Cleaning previous build output"
        Invoke-Native cargo @("clean")
    }

    if (-not $SkipTests) {
        Write-Step "Running tests (release)"
        Invoke-Native cargo @("test", "--workspace", "--release")
    }

    if ($Diagnose) {
        Write-Step "Building with verbose output (diagnose)"
        $log = Join-Path $root "build-windows.log"
        try {
            Invoke-Native cargo @("build", "--release", "--locked", "-vv")
        } catch {
            Select-String -Path $log -Pattern 'LNK2005|LNK2019|LNK1169|LNK2038|LNK1104' -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host $_.Line -ForegroundColor Yellow }
            throw
        }
    } else {
        Write-Step "Building release binary"
        Invoke-Native cargo @("build", "--release", "--locked")
    }

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
            Write-Warn "The executable depends on the dynamic C++ runtime, but the prebuilt sherpa/ONNX Runtime libraries expect the static CRT."
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
    $sherpaLib = Ensure-SherpaDlls
    foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        $source = Join-Path $sherpaLib $dll
        if (-not (Test-Path $source)) {
            throw "$dll was not found in $sherpaLib"
        }
        Copy-Item $source (Join-Path $stage $dll)
    }
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
