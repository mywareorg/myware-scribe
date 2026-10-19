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
