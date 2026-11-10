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

function Resolve-TargetRoot {
    # One value for cargo, the CMake generator reset, and the CMake runtime
    # reset. An explicit -TargetDir wins, then CARGO_TARGET_DIR, then the
    # short profile-root path needed by ggml's nested object files.
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string]$TargetDir = ""
    )
    # PowerShell has no long options: a switch typed with a double dash arrives
    # as a bare string and binds to whichever parameter has a free position. That
    # is how `--SkipTests` once became the target directory. A failed run also
    # leaves CARGO_TARGET_DIR set in the session, so the same flag-shaped name
    # comes back through the environment on the next attempt. Check both sources,
    # and anchor relative values: cargo resolves them against the current
    # directory, which is how `<repo>\--SkipTests` survived the first fix.
    $source = $null
    $value = $null
    if ($TargetDir) {
        $source = "-TargetDir"
        $value = $TargetDir
    } elseif ($env:CARGO_TARGET_DIR) {
        $source = "CARGO_TARGET_DIR"
        $value = $env:CARGO_TARGET_DIR
    }
    if ($value) {
        if ($value -like "-*") {
            $asSwitch = "-" + $value.TrimStart("-")
            throw "$source is '$value', which looks like a switch. Write it as $asSwitch; if it came from the environment, remove or correct CARGO_TARGET_DIR."
        }
        if ([System.IO.Path]::IsPathRooted($value)) { return $value }
        return (Join-Path $Root $value)
    }
    if ($env:USERPROFILE) { return (Join-Path $env:USERPROFILE "hb") }
    return (Join-Path $Root "target")
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
    #
    # cmd.exe is started through Start-Process because the command line needs a
    # path with spaces, a nested call and a captured environment, and hand-built
    # quoting for `cmd /c` silently loses its final quote. Both streams go to
    # files: in Windows PowerShell 5.1 a native command's stderr becomes an
    # ErrorRecord as soon as it joins the pipeline, and with
    # $ErrorActionPreference = "Stop" that aborts the script even on success.
    $stdoutFile = [System.IO.Path]::GetTempFileName()
    $stderrFile = [System.IO.Path]::GetTempFileName()
    $inner = '""{0}" -arch=x64 -host_arch=x64 && set"' -f $cmd
    $process = Start-Process -FilePath "cmd.exe" `
        -ArgumentList @("/c", $inner) `
        -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
    $code = $process.ExitCode

    $lines = @()
    if (Test-Path $stdoutFile) { $lines = @(Get-Content -Path $stdoutFile) }
    $errors = @()
    if (Test-Path $stderrFile) { $errors = @(Get-Content -Path $stderrFile) }
    Remove-Item $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

    if ($code -ne 0) {
        $why = @(@($errors) + @($lines) | Where-Object { $_ -and "$_".Trim() } | Select-Object -First 20) -join " / "
        throw "VsDevCmd.bat failed with exit code $code. Output: $why"
    }

    $imported = 0
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
            $imported++
        }
    }
    if ($imported -eq 0) {
        throw "VsDevCmd.bat produced no environment variables; the developer environment was not applied."
    }
}

function Remove-DirectoryTree {
    param([Parameter(Mandatory = $true)][string]$Path)

    # Visual Studio build trees contain TryCompile scratch directories whose full
    # path runs past MAX_PATH. Remove-Item then reports the misleading
    # "Could not find a part of the path 'CompilerIdCXX.lastbuildstate'", so hand
    # the path to .NET with the verbatim prefix, which has no length limit.
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $full = [System.IO.Path]::GetFullPath($Path)
    $verbatim = if ($full.StartsWith('\\?\')) { $full } else { '\\?\' + $full }
    try {
        [System.IO.Directory]::Delete($verbatim, $true)
    } catch {
        Write-Warn "Could not delete $Path through .NET ($($_.Exception.Message)); falling back to rd"
        & cmd.exe /c "rd /s /q `"$full`"" | Out-Null
    }
    if (Test-Path -LiteralPath $Path) {
        throw "Could not remove $Path"
    }
}

function Reset-CmakeGenerator {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Generator,
        # Where cargo keeps release build output. Defaults to <Root>\target;
        # overridden when CARGO_TARGET_DIR points somewhere shorter.
        [string]$TargetRoot = ""
    )

    # CMake refuses to reuse a build directory that was configured with another
    # generator, and cargo keeps one whisper-rs-sys build directory per feature
    # set. A previous Visual Studio configure therefore blocks a Ninja one.
    #
    # Deleting the build directory alone is not enough: cargo's fingerprint would
    # still say the build script is up to date and it would never run again.
    $targetRoot = if ($TargetRoot) { $TargetRoot } else { Join-Path $Root "target" }
    $buildRoot = Join-Path $targetRoot "release\build"
    if (-not (Test-Path $buildRoot)) { return }

    $stale = @()
    foreach ($directory in (Get-ChildItem -Path $buildRoot -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue)) {
        $cache = Join-Path $directory.FullName "out\build\CMakeCache.txt"
        if (-not (Test-Path -LiteralPath $cache)) { continue }
        $line = Get-Content -LiteralPath $cache -ErrorAction SilentlyContinue |
            Where-Object { $_ -like "CMAKE_GENERATOR:INTERNAL=*" } |
            Select-Object -First 1
        if (-not $line) { continue }
        $recorded = ($line -split "=", 2)[1]
        if ($recorded -eq $Generator) { continue }
        Write-Step "Discarding the $recorded configure in $($directory.Name)"
        Remove-DirectoryTree -Path $directory.FullName
        $stale += $directory.Name
    }

    if ($stale.Count -gt 0) {
        $fingerprints = Join-Path $targetRoot "release\.fingerprint"
        foreach ($entry in (Get-ChildItem -Path $fingerprints -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue)) {
            Remove-DirectoryTree -Path $entry.FullName
        }
    }
}

function Reset-CmakeRuntime {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Runtime,
        # Where cargo keeps release build output. Defaults to <Root>\target;
        # overridden when CARGO_TARGET_DIR points somewhere shorter.
        [string]$TargetRoot = ""
    )

    # whisper-rs-sys builds whisper.cpp and ggml through CMake, and CMake caches
    # the runtime library it was configured with. A cache left behind by an
    # earlier static-CRT attempt keeps compiling the C++ objects with /MT even
    # though the build now asks for the DLL runtime, and the link fails again
    # with LNK2019 __imp_* and LNK1120. Cargo does not watch this, so a stale
    # configure has to be discarded or the build silently repeats the old mode.
    $targetRoot = if ($TargetRoot) { $TargetRoot } else { Join-Path $Root "target" }
    $buildRoot = Join-Path $targetRoot "release\build"
    if (-not (Test-Path $buildRoot)) { return }

    $stale = @()
    foreach ($directory in (Get-ChildItem -Path $buildRoot -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue)) {
        $cache = Join-Path $directory.FullName "out\build\CMakeCache.txt"
        if (-not (Test-Path -LiteralPath $cache)) { continue }
        $line = Get-Content -LiteralPath $cache -ErrorAction SilentlyContinue |
            Where-Object { $_ -like "CMAKE_MSVC_RUNTIME_LIBRARY:*" } |
            Select-Object -First 1
        # Absent means CMake's own default, MultiThreadedDLL.
        $recorded = if ($line) { ($line -split "=", 2)[1] } else { "MultiThreadedDLL" }
        if ($recorded -eq $Runtime) { continue }
        Write-Step "Discarding the $recorded runtime in $($directory.Name)"
        Remove-DirectoryTree -Path $directory.FullName
        $stale += $directory.Name
    }

    if ($stale.Count -gt 0) {
        $fingerprints = Join-Path $targetRoot "release\.fingerprint"
        foreach ($entry in (Get-ChildItem -Path $fingerprints -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue)) {
            Remove-DirectoryTree -Path $entry.FullName
        }
    }
}

function Resolve-VcRuntime {
    # The executable links the DLL CRT, so the redistributable runtime DLLs have
    # to travel beside it: the Store certification machine and a fresh Windows
    # install cannot be assumed to have the Visual C++ redistributable. The
    # newest installed redist wins, matching the toolset that did the build.
    # Build Tools installs often omit VC\Redist, so fall back to the toolset's
    # own bin directory and warn instead of failing after a successful compile.
    $vs = Get-VsInstallPath
    $root = Join-Path $vs "VC\Redist\MSVC"
    if (-not (Test-Path -LiteralPath $root)) {
        $toolset = Get-ChildItem -LiteralPath (Join-Path $vs "VC\Tools\MSVC") -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -as [version] } |
            Sort-Object { [version]$_.Name } -Descending |
            Select-Object -First 1
        if ($toolset) {
            $bin = Join-Path $toolset.FullName "bin\Hostx64\x64"
            if (Test-Path -LiteralPath (Join-Path $bin "vcruntime140.dll")) {
                Write-Warn "VC\Redist\MSVC is missing; staging the CRT from $bin."
                return $bin
            }
        }
        Write-Warn "VC\Redist\MSVC and the toolset CRT DLLs were not found under $vs; the staged app may need the Visual C++ redistributable."
        return $null
    }

    $versions = Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -as [version] } |
        Sort-Object { [version]$_.Name } -Descending

    foreach ($version in $versions) {
        $x64 = Join-Path $version.FullName "x64"
        if (-not (Test-Path -LiteralPath $x64)) { continue }
        $crt = Get-ChildItem -LiteralPath $x64 -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($crt) { return $crt.FullName }
    }

    Write-Warn "No x64 Microsoft.VC*.CRT directory was found under $root; the staged app may need the Visual C++ redistributable."
    return $null
}

function Resolve-VcOpenMp {
    # whisper.cpp's OpenMP backend links vcomp140.dll. It lives in a separate
    # Microsoft.VC*.OpenMP redist directory, not in the CRT directory that
    # Resolve-VcRuntime returns, so the GPU build has to stage it explicitly.
    param([string]$VcRuntime = "")
    $candidates = @()

    if ($VcRuntime) {
        $x64 = Split-Path -Parent $VcRuntime
        if ($x64) {
            $candidates += @(Get-ChildItem -LiteralPath $x64 -Directory -Filter "Microsoft.VC*.OpenMP" -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "vcomp140.dll" })
        }
    }

    $vs = Get-VsInstallPath
    $root = Join-Path $vs "VC\Redist\MSVC"
    if (Test-Path -LiteralPath $root) {
        # Redist layouts differ: some toolsets put the OpenMP DLL under
        # <version>\x64, others under <version>\onecore\x64. Search the tree and
        # keep the x64 copy rather than assuming one layout.
        $candidates += @(Get-ChildItem -LiteralPath $root -Recurse -Filter "vcomp140.dll" -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } |
            Sort-Object LastWriteTime -Descending |
            ForEach-Object { $_.FullName })
        $candidates += @(Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -as [version] } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName "x64" } |
            Where-Object { Test-Path -LiteralPath $_ } |
            ForEach-Object {
                Get-ChildItem -LiteralPath $_ -Directory -Filter "Microsoft.VC*.OpenMP" -ErrorAction SilentlyContinue |
                    ForEach-Object { Join-Path $_.FullName "vcomp140.dll" }
            })
    }

    $toolset = Get-ChildItem -LiteralPath (Join-Path $vs "VC\Tools\MSVC") -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -as [version] } |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1
    if ($toolset) {
        $candidates += Join-Path $toolset.FullName "bin\Hostx64\x64\vcomp140.dll"
    }

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return (Split-Path -Parent $candidate)
        }
    }
    return $null
}

function Assert-DllCrtArtifact {
    # Catch a static-CRT link before packaging: a /MT executable does not
    # import vcruntime140.dll, so the runtime DLLs staged beside it are unused
    # and the app still carries the CRT mismatch that caused LNK2019.
    param([Parameter(Mandatory = $true)][string]$Binary)
    $dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if (-not $dumpbin) {
        Write-Warn "dumpbin.exe was not found; skipping the DLL CRT dependency check."
        return
    }
    $dependents = (& $dumpbin.Source /dependents $Binary 2>&1 | Out-String)
    if ($dependents -notmatch 'vcruntime140\.dll') {
        throw "$Binary does not import vcruntime140.dll; the build is not using the DLL CRT."
    }
    Write-Step "Verified the DLL CRT dependency of $(Split-Path -Leaf $Binary)"
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
#
# appcert.exe is not under bin\<version>\x64 like makeappx and signtool: the
# Windows App Certification Kit installs into its own folder beside bin, so the
# bin scan alone never finds it.
function Resolve-SdkTool {
    param([Parameter(Mandatory = $true)][string]$Name)

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }

    $kits = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10",
        "$env:ProgramFiles\Windows Kits\10"
    ) | Where-Object { $_ -and (Test-Path $_) }

    foreach ($kit in $kits) {
        $bin = Join-Path $kit "bin"
        $found = Get-ChildItem -Path $bin -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\$Name" } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
        if ($found) { return $found }
    }

    $certKit = "App Certification Kit"
    foreach ($kit in $kits) {
        $found = @(
            (Join-Path (Join-Path $kit $certKit) $Name),
            (Join-Path (Join-Path (Join-Path $kit $certKit) "x64") $Name)
        ) | Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

# appcert.exe and the AppXSvc trust store both need administrator rights.
function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
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
