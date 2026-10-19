#Requires -Version 5.1
# Shared helpers for the Windows build and run scripts.

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Warn {
    param([string]$Message)
    Write-Warning $Message
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

function Ensure-SherpaDlls {
    param(
        [string]$CacheRoot
    )

    if (-not $CacheRoot) {
        $CacheRoot = Join-Path $env:LOCALAPPDATA "transiel\cache"
    }

    $version = "v1.12.9"
    $release = "sherpa-onnx-$version-win-x64-shared"
    $root = Join-Path $CacheRoot $release
    $lib = Join-Path $root "lib"
    $cApi = Join-Path $lib "sherpa-onnx-c-api.dll"
    $ort = Join-Path $lib "onnxruntime.dll"
    if ((Test-Path $cApi) -and (Test-Path $ort)) {
        return $lib
    }

    $archive = "$release.tar.bz2"
    $url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/$version/$archive"
    $expected = "57a0dff63c468ed067c6ffd1a9c9b6f66f4cf73304d44b5f17518010ce2e0977"

    New-Item -ItemType Directory -Path $CacheRoot -Force | Out-Null
    $path = Join-Path $CacheRoot $archive

    if (-not (Test-Path $path)) {
        Write-Step "Downloading sherpa-onnx shared libraries"
        Invoke-WebRequest -Uri $url -OutFile $path -UseBasicParsing
    }

    $actual = (Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Remove-Item $path -Force -ErrorAction SilentlyContinue
        throw "sherpa-onnx archive checksum mismatch (expected $expected, got $actual)"
    }

    Write-Step "Extracting sherpa-onnx shared libraries"
    if (Test-Path $root) { Remove-Item $root -Recurse -Force }
    & tar.exe -xf $path -C $CacheRoot
    if ($LASTEXITCODE -ne 0) {
        throw "could not extract $path (tar exit code $LASTEXITCODE)"
    }
    if (-not (Test-Path $cApi)) {
        throw "sherpa-onnx-c-api.dll was not found after extracting $path"
    }
    return $lib
}
