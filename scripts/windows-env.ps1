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

function Resolve-VulkanSdk {
    if ($env:VULKAN_SDK -and (Test-Path (Join-Path $env:VULKAN_SDK "Include\vulkan\vulkan.h"))) {
        return $env:VULKAN_SDK
    }

    # The LunarG installer drops a versioned directory; use the newest one,
    # which is what the installer points VULKAN_SDK at in a fresh shell.
    $roots = @(
        "C:\VulkanSDK",
        "${env:ProgramFiles}\VulkanSDK",
        "${env:ProgramFiles(x86)}\VulkanSDK"
    ) | Where-Object { $_ -and (Test-Path $_) }
    foreach ($root in $roots) {
        $found = Get-ChildItem -Path $root -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { $_.FullName } |
            Where-Object { Test-Path (Join-Path $_ "Include\vulkan\vulkan.h") } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
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

function Find-SevenZip {
    $candidates = @(
        "C:\Program Files\7-Zip\7z.exe",
        "C:\Program Files (x86)\7-Zip\7z.exe"
    )
    $command = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($command) { $candidates = @($command.Source) + $candidates }
    return $candidates |
        Where-Object { $_ -and (Test-Path $_) } |
        Select-Object -First 1
}

function Get-PortableSevenZip {
    param([Parameter(Mandatory = $true)][string]$CacheRoot)

    $dir = Join-Path $CacheRoot "7za"
    $exe = Join-Path $dir "7za.exe"
    if (Test-Path $exe) {
        $env:PATH = "$dir;$env:PATH"
        return $exe
    }

    $url = "https://www.7-zip.org/a/7za920.zip"
    $expected = "2a3afe19c180f8373fa02ff00254d5394fec0349f5804e0ad2f6067854ff28ac"
    $zip = Join-Path $CacheRoot "7za920.zip"

    New-Item -ItemType Directory -Path $CacheRoot -Force | Out-Null
    if (-not (Test-Path $zip)) {
        Write-Step "Downloading the standalone 7-Zip extractor"
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    }

    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Remove-Item $zip -Force -ErrorAction SilentlyContinue
        throw "7za archive checksum mismatch (expected $expected, got $actual)"
    }

    Write-Step "Installing the standalone 7-Zip extractor"
    Expand-Archive -Path $zip -DestinationPath $dir -Force
    if (-not (Test-Path $exe)) {
        throw "7za.exe was not found after extracting $zip"
    }
    $env:PATH = "$dir;$env:PATH"
    return $exe
}

function Expand-SevenZip {
    param(
        [Parameter(Mandatory = $true)][string]$SevenZip,
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    & $SevenZip x -y "-o$Destination" $Archive
    if ($LASTEXITCODE -ne 0) { return $false }

    # 7-Zip extracts one compression layer at a time. A .tar.bz2 first becomes
    # a .tar; extract that too so the caller sees the package directories.
    $inner = Join-Path $Destination ([System.IO.Path]::GetFileNameWithoutExtension($Archive))
    if ((Test-Path $inner) -and ($inner -like "*.tar")) {
        & $SevenZip x -y "-o$Destination" $inner
        if ($LASTEXITCODE -ne 0) { return $false }
        Remove-Item $inner -Force -ErrorAction SilentlyContinue
    }
    return $true
}

function Expand-SherpaArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    if (-not (Test-Path $Destination)) {
        New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    }

    $sevenZip = Find-SevenZip
    if ($sevenZip) {
        Write-Step "Extracting sherpa-onnx archive with 7-Zip"
        if (Expand-SevenZip -SevenZip $sevenZip -Archive $Archive -Destination $Destination) {
            return
        }
        Write-Warn "7-Zip could not extract the archive; trying another extractor."
    }

    # Windows 10's built-in tar is bsdtar without bzip2 support; it tries to
    # launch an external `bzip2 -d`. Git for Windows ships one.
    $bzip2Command = Get-Command bzip2.exe -ErrorAction SilentlyContinue
    $bzip2Paths = @(
        "C:\Program Files\Git\usr\bin\bzip2.exe",
        "C:\Program Files\Git\mingw64\bin\bzip2.exe",
        "C:\Program Files (x86)\Git\usr\bin\bzip2.exe",
        "C:\Program Files\Git\bin\bzip2.exe"
    )
    if ($bzip2Command) { $bzip2Paths = @($bzip2Command.Source) + $bzip2Paths }
    $bzip2 = $bzip2Paths |
        Where-Object { $_ -and (Test-Path $_) } |
        Select-Object -First 1
    if ($bzip2) {
        $env:PATH = "$(Split-Path -Parent $bzip2);$env:PATH"
        Write-Step "Extracting sherpa-onnx archive with tar"
        & tar.exe -xf $Archive -C $Destination
        if ($LASTEXITCODE -eq 0) { return }
        Write-Warn "tar could not extract the archive; using the standalone 7-Zip extractor."
    }

    $sevenZip = Get-PortableSevenZip -CacheRoot $Destination
    Write-Step "Extracting sherpa-onnx archive with the standalone 7-Zip extractor"
    if (Expand-SevenZip -SevenZip $sevenZip -Archive $Archive -Destination $Destination) {
        return
    }

    throw "Could not extract $Archive. Remove the cache and rerun the script."
}

function Ensure-SherpaDlls {
    param(
        [string]$CacheRoot
    )

    if (-not $CacheRoot) {
        $CacheRoot = Join-Path $env:LOCALAPPDATA "hippo-transcribe\cache"
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
    Expand-SherpaArchive -Archive $path -Destination $CacheRoot
    if (-not (Test-Path $cApi)) {
        throw "sherpa-onnx-c-api.dll was not found after extracting $path"
    }
    return $lib
}

# Store tooling ships with the Windows SDK, which VsDevCmd does not always put
# on PATH. Fall back to the newest installed kit.
function Resolve-SdkTool {
    param([Parameter(Mandatory = $true)][string]$Name)

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }

    $kits = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    ) | Where-Object { $_ -and (Test-Path $_) }
    foreach ($kitsRoot in $kits) {
        $found = Get-ChildItem -Path $kitsRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\$Name" } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

# The manifest needs four parts; the crate version only has three.
function ConvertTo-FourPartVersion {
    param([Parameter(Mandatory = $true)][string]$Version)

    $parts = @($Version -split '\.')
    if ($parts.Count -gt 4) {
        throw "MSIX versions have at most four parts ($Version)"
    }
    foreach ($part in $parts) {
        if ($part -notmatch '^\d+$') {
            throw "MSIX versions must be numeric ($Version)"
        }
    }
    while ($parts.Count -lt 4) { $parts += "0" }
    return ($parts -join '.')
}
