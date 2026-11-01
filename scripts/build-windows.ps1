#Requires -Version 5.1
<#
.SYNOPSIS
Build Hippo Transcribe for Windows, stage a release folder, and optionally zip it
or package it as an MSIX for the Microsoft Store.

.DESCRIPTION
Enters the Visual Studio developer environment (so cl.exe, rc.exe, the Windows
SDK, and CMake are on PATH), builds the release binary, stages the sherpa-onnx
and Visual C++ runtime DLLs beside it, stages deploy.bat/undeploy.bat/README/LICENSE
plus the sample audio, and writes a zip with a SHA-256 checksum.

With -Msix the staged binary is also packed into an MSIX: the manifest is
rendered from packaging/AppxManifest.xml.template and packaging/identity.json,
and only the executable plus its two runtime DLLs are included. deploy.bat, the
sample audio and the models stay out; the Store build downloads models on
first run.

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
  - Windows 10/11 SDK for -Msix (makeappx), -Sign (signtool) and -Wack (appcert)

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1

.EXAMPLE
.\scripts\build-windows.ps1 -SkipTests -Version 0.2.0 -OutDir C:\builds

.EXAMPLE
.\scripts\build-windows.ps1 -SkipTests -Msix -Sign -CertificatePath C:\keys\hippo-test.pfx -CertificatePassword hunter2

.EXAMPLE
# Iterate on the step a Vulkan build usually fails in, without compiling.
.\scripts\build-windows.ps1 -Probe
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
    [switch]$NoZip,
    [switch]$SkipModels,
    # Keep the persisted queue from earlier runs. By default the build clears
    # it so the freshly compiled app starts empty.
    [switch]$KeepQueue,
    # Microsoft Store packaging. Off by default so the portable zip build is
    # unchanged.
    [switch]$Msix,
    # Identity to render into the manifest. Defaults to
    # packaging/identity.json.
    [string]$IdentityPath,
    # Sign the MSIX with a code-signing certificate. Required before
    # Add-AppxPackage or the certification kit will accept the package.
    [switch]$Sign,
    [string]$CertificatePath,
    [string]$CertificatePassword,
    # Run the Windows App Certification Kit against the finished package.
    [switch]$Wack,
    # Vulkan + OpenMP acceleration. Enabled automatically whenever the Vulkan
    # SDK is installed; -Gpu requires the SDK and fails without it, -Cpu forces
    # the CPU-only build even on a machine that has the SDK.
    [switch]$Gpu,
    [switch]$Cpu,
    # Replay only ggml's nested Vulkan shader configure and exit. No cargo, no
    # compiler work: the step the build usually fails in, in a few seconds.
    [switch]$Probe
)

$ErrorActionPreference = "Stop"

Set-StrictMode -Version Latest

# A bare throw only prints the message, which hides which statement failed.
# Print the position and the offending command line instead.
trap {
    Write-Host ""
    Write-Host "Build failed:" -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    Write-Host ("  Message: " + $_.Exception.Message)
    Write-Host ("  Stack:   " + ($_.ScriptStackTrace -replace "`r?`n", " | "))

    # Also append to the build log, so the failure can be read from a file when
    # the console is not copyable (a VM console, for instance).
    try {
        $logPath = Join-Path (Split-Path -Parent $PSScriptRoot) "dist\build-failure.txt"
        New-Item -ItemType Directory -Path (Split-Path -Parent $logPath) -Force | Out-Null
        $detail = @(
            ("=== Build failed " + (Get-Date -Format "yyyy-MM-dd HH:mm:ss") + " ==="),
            $_.InvocationInfo.PositionMessage,
            ("Message: " + $_.Exception.Message),
            ("Stack:   " + $_.ScriptStackTrace)
        ) -join "`r`n"
        Set-Content -Path $logPath -Value $detail -Encoding UTF8

        # Attach the build's own explanation. A failed GPU build names the
        # generator it used, the compiler CMake detected, and the MSBuild or CMake
        # error, all of which are otherwise buried thousands of lines deep.
        $repo = Split-Path -Parent $PSScriptRoot
        $buildLog = Join-Path $repo "build-windows.log"
        if (Test-Path $buildLog) {
            $pattern = "CMake Error|CMake Warning|No CMAKE_|CMAKE_C_COMPILER|CMAKE_CXX_COMPILER|CMAKE_GENERATOR|error MSB|error C[0-9]|Generator|compilerid|vulkan-shaders|ninja"
            $extract = @("", "=== build-windows.log ===")
            $hits = Select-String -Path $buildLog -Pattern $pattern -ErrorAction SilentlyContinue
            foreach ($hit in ($hits | Select-Object -First 80)) {
                $extract += ("{0}: {1}" -f $hit.LineNumber, $hit.Line.Trim())
            }
            $extract += @("", "=== last 40 lines ===")
            $extract += (Get-Content -Path $buildLog -Tail 40 -ErrorAction SilentlyContinue)
            Add-Content -Path $logPath -Value ($extract -join "`r`n") -Encoding UTF8
        }

        # The step names matter as much as the error: they show which stage ran.
        # CMake's own warnings explain most configure failures and sit far earlier
        # in the log than the error does, so keep the log with the report.
        $logCopy = Join-Path $repo "dist\build-windows.log"
        if (Test-Path $buildLog) {
            New-Item -ItemType Directory -Path (Split-Path -Parent $logCopy) -Force | Out-Null
            Copy-Item -Path $buildLog -Destination $logCopy -Force
        }

        $console = Join-Path $repo "build-console.txt"
        if (Test-Path $console) {
            $consoleTail = @("", "=== console ===")
            $consoleTail += (Get-Content -Path $console -Tail 80 -ErrorAction SilentlyContinue)
            Add-Content -Path $logPath -Value ($consoleTail -join "`r`n") -Encoding UTF8
        }

        $reportTarget = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo "target" }
        $configured = Get-ChildItem -Path (Join-Path $reportTarget "release\build") -Recurse -Filter "CMakeCache.txt" -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "whisper-rs-sys" }
        if ($configured) {
            $cacheLines = @("", "=== configure state ===")
            foreach ($cache in $configured) {
                $generator = Get-Content -Path $cache.FullName -ErrorAction SilentlyContinue |
                    Where-Object { $_ -like "CMAKE_GENERATOR:*" } |
                    Select-Object -First 1
                $cacheLines += ("{0}`r`n    {1}" -f $cache.FullName, $generator)
            }
            Add-Content -Path $logPath -Value ($cacheLines -join "`r`n") -Encoding UTF8
        }

        Write-Host ("  Logged to: " + $logPath) -ForegroundColor DarkGray
    } catch {
        Write-Host "  (could not write the build log)"
    }
    try { Stop-Transcript | Out-Null } catch { }
    exit 1
}

# PowerShell 7.3+ can turn native stderr into a terminating error. We check
# $LASTEXITCODE ourselves, so keep that behavior off when the switch exists.
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

. (Join-Path $PSScriptRoot "windows-env.ps1")

# The gpu feature compiles whisper against Vulkan. The SDK is only needed to
# compile: at run time ggml loads vulkan-1.dll from the graphics driver and
# falls back to the CPU when no Vulkan device exists.
#
# Acceleration is opt-in. An installed SDK does not turn it on: the accelerated
# build has never been exercised on real hardware, and a build that changes
# backend because of what happens to be installed is not reproducible.
$gpuExplicit = $PSBoundParameters.ContainsKey("Gpu")
$cpuExplicit = $PSBoundParameters.ContainsKey("Cpu")
if ($gpuExplicit -and $cpuExplicit) {
    throw "-Gpu and -Cpu cannot be combined."
}

$useGpu = $false
if ($cpuExplicit) {
    Write-Step "Building the CPU-only backend (-Cpu)"
} elseif ($gpuExplicit) {
    $vulkanSdk = Resolve-VulkanSdk
    if (-not $vulkanSdk) {
        throw "-Gpu needs the Vulkan SDK (VULKAN_SDK). Install it from LunarG, or drop -Gpu for a CPU-only build."
    }
    $env:VULKAN_SDK = $vulkanSdk
    $useGpu = $true
    Write-Step "Vulkan SDK at $vulkanSdk"
} else {
    Write-Step "Building the CPU-only backend (pass -Gpu for the Vulkan build)"
}
if ($useGpu -and $Msix) {
    Write-Step "Packaging for the Store with Vulkan; machines without a Vulkan driver fall back to the CPU"
}
$featureArgs = if ($useGpu) { @("--features", "gpu") } else { @() }


function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @()
    )

    $commandLine = (@($FilePath) + $Arguments) -join " "
    Write-Host "    $commandLine" -ForegroundColor DarkGray

    # Every native command is teed to one log so a failure leaves the real
    # cargo/rustc error behind instead of only the rethrown exit code.
    #
    # PowerShell 5.1 turns a native command's stderr into an ErrorRecord as soon
    # as it is merged with 2>&1, and with $ErrorActionPreference = "Stop" that
    # aborts the script even when the tool succeeded. cargo reports progress on
    # stderr, so capture both streams through files instead of the pipeline.
    $log = Join-Path $root "build-windows.log"
    $stdoutFile = Join-Path $root "build-windows.out.log"
    $stderrFile = Join-Path $root "build-windows.err.log"
    Remove-Item $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

    $startArgs = @{
        FilePath               = $FilePath
        NoNewWindow            = $true
        Wait                   = $true
        PassThru               = $true
        RedirectStandardOutput = $stdoutFile
        RedirectStandardError  = $stderrFile
    }
    if ($Arguments.Count -gt 0) {
        # Start-Process joins an ArgumentList array with single spaces and does
        # not quote entries. Quote paths/values that contain whitespace so
        # staged directories with spaces survive the handoff.
        $startArgs.ArgumentList = @($Arguments | ForEach-Object {
            $argument = [string]$_
            if ($argument -match '[\s"]') {
                '"' + ($argument -replace '"', '\"') + '"'
            } else {
                $argument
            }
        })
    }

    $process = Start-Process @startArgs
    $exitCode = $process.ExitCode

    foreach ($file in @($stdoutFile, $stderrFile)) {
        if (Test-Path $file) {
            Get-Content -Path $file | Tee-Object -FilePath $log -Append | Out-Host
        }
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

function Invoke-MsixSigning {
    param(
        [Parameter(Mandatory = $true)][string]$Msix,
        [Parameter(Mandatory = $true)][string]$Certificate,
        [string]$Password
    )

    $signtool = Resolve-SdkTool -Name "signtool.exe"
    if (-not $signtool) {
        throw "signtool.exe was not found. Install the Windows 10/11 SDK."
    }
    if (-not (Test-Path $Certificate)) {
        throw "Certificate not found: $Certificate"
    }

    $arguments = @("sign", "/fd", "SHA256", "/f", $Certificate)
    if ($Password) { $arguments += @("/p", $Password) }
    $arguments += $Msix

    Write-Step "Signing the package"
    & $signtool @arguments | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "signtool failed with exit code $LASTEXITCODE"
    }

    # signtool can exit 0 and still leave the package untrusted when the
    # certificate is not in a trusted store; the Store needs a valid chain.
    $signature = Get-AuthenticodeSignature -FilePath $Msix
    if ($signature.Status -ne "Valid") {
        Write-Warn "The package is signed but its chain is not trusted ($($signature.Status)). Add the certificate to TrustedPeople, or use a real code-signing certificate."
    }
}

function Invoke-Wack {
    param(
        [Parameter(Mandatory = $true)][string]$Msix,
        [Parameter(Mandatory = $true)][string]$ReportPath
    )

    $appcert = Resolve-SdkTool -Name "appcert.exe"
    if (-not $appcert) {
        throw "appcert.exe (Windows App Certification Kit) was not found. Add the 'Windows App Certification Kit' component to the Windows 10/11 SDK in the Visual Studio installer, or install the standalone SDK."
    }

    # AppXSvc installs the package from appcert, and it only trusts machine-wide
    # certificates, so a cert trusted for the user alone fails inside appcert.
    $thumbprint = (Get-AuthenticodeSignature -FilePath $Msix).SignerCertificate.Thumbprint
    if ($thumbprint) {
        $machineTrust = Get-ChildItem "Cert:\LocalMachine\TrustedPeople" -ErrorAction SilentlyContinue |
            Where-Object { $_.Thumbprint -eq $thumbprint } |
            Select-Object -First 1
        if (-not $machineTrust) {
            Write-Warn "The signing certificate is only trusted for this user, so appcert cannot install the package. From an elevated prompt: Import-Certificate -FilePath <signing.cer> -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
        }
    }

    Write-Step "Running the Windows App Certification Kit"

    # appcert.exe is the one SDK tool that refuses to run unelevated, and it
    # also installs the package, which AppXSvc only allows for a certificate in
    # the machine store. Elevate just this step so the build stays unprivileged.
    if (-not (Test-Elevated)) {
        $helper = Join-Path $PSScriptRoot "wack-elevated.ps1"
        $logPath = [System.IO.Path]::ChangeExtension($ReportPath, ".log")
        $arguments = @(
            "-NoProfile", "-ExecutionPolicy", "Bypass",
            "-File", "`"$helper`"",
            "-Appcert", "`"$appcert`"",
            "-Msix", "`"$Msix`"",
            "-ReportPath", "`"$ReportPath`"",
            "-LogPath", "`"$logPath`""
        ) -join " "
        Write-Host "  appcert.exe needs administrator rights; approve the UAC prompt"
        try {
            $elevated = Start-Process -FilePath "powershell" -ArgumentList $arguments -Verb RunAs -Wait -PassThru
        } catch {
            throw "Could not elevate for appcert.exe ($($_.Exception.Message)). Run the build from an elevated prompt, or drop -Wack."
        }
        if (Test-Path $logPath) { Get-Content -Path $logPath | Out-Host }
        if ($elevated.ExitCode -ne 0) {
            throw "appcert failed with exit code $($elevated.ExitCode) (report: $ReportPath)"
        }
        Write-Host "  WACK report: $ReportPath"
        return
    }

    & $appcert reset | Out-Null
    # appcert installs the package, then removes it again. The package must be
    # signed by a certificate this machine trusts.
    & $appcert test -appxpackagepath $Msix -reportoutputpath $ReportPath | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "appcert failed with exit code $LASTEXITCODE (report: $ReportPath)"
    }
    Write-Host "  WACK report: $ReportPath"
}

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir

# The Windows VM cannot copy out of its console, so keep a transcript that the
# failure file can quote and that can be pushed when a run goes wrong.
$transcriptPath = Join-Path $root "build-console.txt"
try { Start-Transcript -Path $transcriptPath -Force | Out-Null } catch { }

if ($useGpu) {
    # ggml is configured with CMake, and its Vulkan shader generator is a nested
    # ExternalProject that inherits the parent generator. Under a Visual Studio
    # generator the parent has no CMAKE_C_COMPILER at configure time, so the nested
    # configure fails with "No CMAKE_C_COMPILER could be found." Ninja sets the
    # compiler explicitly, and is the configuration ggml is tested against.
    if (-not (Get-Command ninja.exe -ErrorAction SilentlyContinue)) {
        throw "Ninja is required for the Vulkan build; it ships with Visual Studio under CommonExtensions\Microsoft\CMake\Ninja."
    }
    Write-Step "Configuring CMake with Ninja for the Vulkan build"
    $env:CMAKE_GENERATOR = "Ninja"
    Write-Host "  CMAKE_GENERATOR=$env:CMAKE_GENERATOR" -ForegroundColor DarkGray

    # ggml builds its Vulkan shader generator with ExternalProject_Add, so it is
    # configured by a second cmake process that only inherits the environment.
    # whisper-rs-sys turns every CMAKE_* variable into -D for the parent, which
    # means flags cannot be injected that way; the toolchain has to be one that
    # ggml works with. Prefer the CMake that ships with Visual Studio, next to the
    # Ninja that ships with it, over a standalone install on PATH.
    $vsCmake = Join-Path (Get-VsInstallPath) "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
    if (Test-Path (Join-Path $vsCmake "cmake.exe")) {
        $env:PATH = $vsCmake + ";" + $env:PATH
        Write-Host "  Using the Visual Studio CMake at $vsCmake" -ForegroundColor DarkGray
    } else {
        Write-Warn "No CMake under the Visual Studio install; using the one on PATH."
    }

    # ggml's shader project writes its object files about 180 characters deep in
    # a default checkout, and cmake refuses to place them past CMAKE_OBJECT_PATH_MAX
    # (250): "cannot be safely placed under this directory". cl.exe then fails with
    # "Cannot open compiler generated file: ''". A target directory near the profile
    # root keeps the nested path clear of the limit.
    $resolvedTarget = Resolve-TargetRoot -Root $root -TargetDir $TargetDir
    Write-Step "Building in $resolvedTarget so the ggml paths stay under MAX_PATH"
    Reset-CmakeGenerator -Root $root -Generator "Ninja" -TargetRoot $resolvedTarget
}

Push-Location $root
try {
    Remove-Item (Join-Path $root "build-windows.log") -Force -ErrorAction SilentlyContinue

    # A Ninja build trusts whichever cl.exe is first on PATH, and a Developer
    # PowerShell may have been opened for x86. Only re-enter the environment when
    # the current compiler is missing or hosted on the wrong architecture;
    # VsDevCmd.bat is slow and fails for reasons of its own.
    $clPath = (Get-Command cl.exe -ErrorAction SilentlyContinue).Source
    if (-not $clPath -or $clPath -notmatch 'Host(X64|ARM64)\\x64') {
        Enter-DevEnvironment
        $clPath = (Get-Command cl.exe -ErrorAction SilentlyContinue).Source
    }
    if (-not $clPath) {
        throw "cl.exe is not on PATH; install the Visual Studio C++ workload or run this from a Developer PowerShell."
    }
    if ($clPath -notmatch 'Host(X64|ARM64)\\x64') {
        throw "cl.exe resolves to $clPath; this build needs the x64-hosted compiler under HostX64\x64 or HostARM64\x64."
    }
    Add-CMakeIfNeeded

    # The probe runs before the main build, so set the target root here as well;
    # probe-shaders.ps1 resolves the same root to find cargo's unpacked whisper.cpp.
    $targetRoot = Resolve-TargetRoot -Root $root -TargetDir $TargetDir
    $env:CARGO_TARGET_DIR = $targetRoot

    foreach ($command in @("cl.exe", "rc.exe", "cmake.exe", "cargo.exe", "rustc.exe")) {
        if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
            throw "$command was not found. Use a machine with the Visual Studio C++ workload and the Rust MSVC toolchain."
        }
    }

    if ($Probe) {
        # The environment above is exactly what the build hands to ggml, so run
        # the nested shader configure in it and stop. Iterating on that step
        # otherwise costs a full cargo build per attempt.
        Write-Step "Probing the nested shader configure (-Probe)"
        $probeScript = Join-Path $PSScriptRoot "probe-shaders.ps1"
        & $probeScript -NoSetup
        exit $LASTEXITCODE
    }
    Write-Host ("  cl: " + $clPath) -ForegroundColor DarkGray
    Write-Host ("  cmake: " + (Get-Command cmake.exe).Source + " " + ((& cmake --version) | Select-Object -First 1)) -ForegroundColor DarkGray

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

    # Both halves of the link use the DLL CRT, which is CMake's default and the
    # Rust default. Do not ask for the static CRT: whisper-rs-sys builds
    # whisper.cpp and ggml with CMake, and the two runtimes cannot be mixed
    # (LNK2019 __imp_* followed by LNK1120). The C++ runtime DLLs are staged
    # beside the executable instead, so nothing has to be installed first.
    Write-Step "Using cargo target directory $targetRoot"

    if ($env:RUSTFLAGS -match '(^|\s)\+crt-static(\s|$)') {
        throw "RUSTFLAGS requests +crt-static, but whisper.cpp/ggml are built with the DLL CRT. Remove +crt-static."
    }
    $cargoConfig = Join-Path $root ".cargo\config.toml"
    if ((Test-Path -LiteralPath $cargoConfig) -and
        (Select-String -LiteralPath $cargoConfig -Pattern '\+crt-static' -Quiet)) {
        throw "The cargo config requests +crt-static, but whisper.cpp/ggml are built with the DLL CRT. Remove +crt-static."
    }

    # Force CMake's DLL runtime for new configurations as well as discarding
    # stale /MT caches from earlier attempts.
    $env:CMAKE_MSVC_RUNTIME_LIBRARY = "MultiThreadedDLL"
    Reset-CmakeRuntime -Root $root -Runtime "MultiThreadedDLL" -TargetRoot $targetRoot

    if ($Clean) {
        Write-Step "Cleaning previous build output"
        Invoke-Native cargo @("clean")
    }

    if (-not $SkipTests) {
        Write-Step "Running tests (release)"
        Invoke-Native cargo (@("test", "--workspace", "--release") + $featureArgs)
    }

    if ($Diagnose) {
        Write-Step "Building with verbose output (diagnose)"
        $log = Join-Path $root "build-windows.log"
        try {
            Invoke-Native cargo (@("build", "--release", "--locked", "-vv") + $featureArgs)
        } catch {
            Select-String -Path $log -Pattern 'LNK2005|LNK2019|LNK1169|LNK2038|LNK1104' -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host $_.Line -ForegroundColor Yellow }
            throw
        }
    } else {
        Write-Step "Building release binary"
        Invoke-Native cargo (@("build", "--release", "--locked") + $featureArgs)
    }

    $metadata = Get-CargoMetadata
    $package = $metadata.packages | Where-Object { $_.name -eq "hippo-transcribe-gui" } | Select-Object -First 1
    if ($Version) {
        $releaseVersion = $Version
    } elseif ($package) {
        $releaseVersion = $package.version
    } else {
        $releaseVersion = "dev"
    }

    $binary = Join-Path $metadata.target_directory "release\hippo-transcribe.exe"
    if (-not (Test-Path $binary)) {
        $fallback = Join-Path $metadata.target_directory "release\hippo-transcribe-gui.exe"
        if (Test-Path $fallback) {
            $binary = $fallback
        } else {
            throw "Release binary not found under $($metadata.target_directory)\release"
        }
    }
    Assert-DllCrtArtifact -Binary $binary

    # The C++ runtime DLLs are staged below; sherpa-onnx/ONNX Runtime are
    # loaded through libloading, so no load-time dependency is missing.

    $outRoot = if ([System.IO.Path]::IsPathRooted($OutDir)) {
        $OutDir
    } else {
        Join-Path $root $OutDir
    }
    New-Item -ItemType Directory -Path $outRoot -Force | Out-Null

    $name = "hippo-transcribe-$releaseVersion-windows-x64"
    $stage = Join-Path $outRoot $name
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null

    Copy-Item $binary (Join-Path $stage "hippo-transcribe.exe")
    $sherpaLib = Ensure-SherpaDlls
    foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        $source = Join-Path $sherpaLib $dll
        if (-not (Test-Path $source)) {
            throw "$dll was not found in $sherpaLib"
        }
        Copy-Item $source (Join-Path $stage $dll)
    }
    $vcRuntime = Resolve-VcRuntime
    foreach ($dll in @("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll")) {
        if (-not $vcRuntime) {
            Write-Warn "Skipping $dll; the Visual C++ runtime was not found."
            continue
        }
        $source = Join-Path $vcRuntime $dll
        if (-not (Test-Path $source)) {
            Write-Warn "$dll was not found in $vcRuntime; skipping it."
            continue
        }
        Copy-Item $source (Join-Path $stage $dll)
    }
    if ($useGpu) {
        $openMpRuntime = Resolve-VcOpenMp -VcRuntime $vcRuntime
        if ($openMpRuntime) {
            $source = Join-Path $openMpRuntime "vcomp140.dll"
            if (Test-Path -LiteralPath $source) {
                Copy-Item $source (Join-Path $stage "vcomp140.dll")
            } else {
                Write-Warn "vcomp140.dll was not found in $openMpRuntime; the GPU build may need the OpenMP runtime."
            }
        } else {
            Write-Warn "vcomp140.dll was not found; the GPU build may need the Visual C++ OpenMP runtime."
        }
    }

    foreach ($file in @("deploy.bat", "undeploy.bat", "README.md", "PRIVACY.md", "LICENSE")) {
        $source = Join-Path $root $file
        if (Test-Path $source) {
            Copy-Item $source (Join-Path $stage $file)
        }
    }
    $sample = Join-Path $root "audio\sample-short.mp3"
    if (Test-Path $sample) {
        Copy-Item $sample (Join-Path $stage "sample-short.mp3")
    }

    if (-not $SkipModels) {
        Write-Step "Staging bundled models (models.lock.json)"
        try {
            & (Join-Path $PSScriptRoot "fetch-models.ps1") -Dest (Join-Path $stage "models")
        } catch {
            # A build machine may be offline even when the release machine is
            # not. Keep the artifact usable: the app downloads missing models
            # on first launch, so a staging failure must not discard a good
            # compile.
            Write-Warn "Models could not be staged: $($_.Exception.Message)"
            Write-Warn "The app will download missing models on first launch."
        }
    }

    if (-not $KeepQueue -and $env:APPDATA) {
        foreach ($queueFile in @(
            (Join-Path $env:APPDATA "hippo-transcribe\queue.json"),
            (Join-Path $env:APPDATA "transiel\queue.json")
        )) {
            if (Test-Path -LiteralPath $queueFile) {
                Remove-Item -LiteralPath $queueFile -Force
                Write-Step "Cleared persisted queue $queueFile"
            }
        }
    }

    $zip = Join-Path $outRoot "$name.zip"
    if (-not $NoZip) {
        Write-Step "Writing $zip"
        if (Test-Path $zip) { Remove-Item $zip -Force }
        Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path $zip -Leaf)" | Set-Content "$zip.sha256" -Encoding ASCII
    }

    $msixPath = $null
    if ($Msix -or $Sign -or $Wack -or $CertificatePath) {
        $identityFile = if ($IdentityPath) {
            if ([System.IO.Path]::IsPathRooted($IdentityPath)) {
                $IdentityPath
            } else {
                Join-Path $root $IdentityPath
            }
        } else {
            Join-Path $root "packaging\identity.json"
        }

        # Packing runs in its own process. Windows PowerShell 5.1 fails to bind
        # the in-script helper here with a spurious SwitchParameter error, while
        # the identical call works from a fresh runspace.
        $packer = Join-Path $PSScriptRoot "make-msix.ps1"
        $msixPath = Join-Path $outRoot "$name.msix"
        if (Test-Path $msixPath) { Remove-Item $msixPath -Force }
        Invoke-Native powershell @(
            "-NoProfile", "-ExecutionPolicy", "Bypass",
            "-File", $packer,
            "-Stage", $stage,
            "-OutDir", $outRoot,
            "-Name", $name,
            "-Version", $releaseVersion,
            "-IdentityPath", $identityFile
        )
        if (-not (Test-Path $msixPath)) {
            throw "Packing returned without producing $msixPath"
        }

        if ($Sign -or $CertificatePath) {
            if (-not $CertificatePath) {
                throw "-Sign needs -CertificatePath pointing at a .pfx"
            }
            $certificate = if ([System.IO.Path]::IsPathRooted($CertificatePath)) {
                $CertificatePath
            } else {
                Join-Path $root $CertificatePath
            }
            Invoke-MsixSigning -Msix $msixPath -Certificate $certificate -Password $CertificatePassword
        }

        if ($Wack) {
            if (-not $CertificatePath) {
                throw "-Wack needs a signed package; pass -Sign -CertificatePath as well"
            }
            Invoke-Wack -Msix $msixPath -ReportPath (Join-Path $outRoot "$name-wack.xml")
        }
    }

    Write-Host ""
    Write-Host "Build complete" -ForegroundColor Green
    Write-Host "  Binary: $binary"
    Write-Host "  Stage:  $stage"
    if (-not $NoZip) { Write-Host "  Zip:    $zip" }
    if ($msixPath) { Write-Host "  MSIX:   $msixPath" }
}
finally {
    Pop-Location
    try { Stop-Transcript | Out-Null } catch { }
}
