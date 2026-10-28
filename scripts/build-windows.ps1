#Requires -Version 5.1
<#
.SYNOPSIS
Build Hippo Transcribe for Windows, stage a release folder, and optionally zip it
or package it as an MSIX for the Microsoft Store.

.DESCRIPTION
Enters the Visual Studio developer environment (so cl.exe, rc.exe, the Windows
SDK, and CMake are on PATH), configures the static CRT for the executable,
stages the sherpa-onnx shared DLLs beside it, builds the release binary, checks
its runtime dependencies, stages deploy.bat/undeploy.bat/README/LICENSE plus
the sample audio, and writes a zip with a SHA-256 checksum.

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
    # Vulkan + OpenMP acceleration. Off by default for MSIX builds and enabled
    # automatically for local builds when the Vulkan SDK is present; pass -Gpu
    # to force it (and fail loudly if the SDK is missing).
    [switch]$Gpu
)

$ErrorActionPreference = "Stop"

Set-StrictMode -Version Latest

# PowerShell 7.3+ can turn native stderr into a terminating error. We check
# $LASTEXITCODE ourselves, so keep that behavior off when the switch exists.
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

. (Join-Path $PSScriptRoot "windows-env.ps1")

# The gpu feature compiles whisper against Vulkan, which needs the SDK at build
# time and a Vulkan-capable driver at run time. A Store package cannot assume
# the latter, so -Msix stays CPU-only unless -Gpu is passed explicitly.
$gpuExplicit = $PSBoundParameters.ContainsKey("Gpu")
$useGpu = $false
if ($gpuExplicit) {
    $useGpu = [bool]$Gpu
} elseif ($Msix) {
    Write-Step "Packaging for the Store with the CPU-only build"
} else {
    $vulkanSdk = Resolve-VulkanSdk
    if ($vulkanSdk) {
        $env:VULKAN_SDK = $vulkanSdk
        $useGpu = $true
        Write-Step "Vulkan SDK at $vulkanSdk"
    } else {
        Write-Warn "Vulkan SDK not found; building CPU-only. Install it for GPU inference, or pass -Gpu to require it."
    }
}
if ($useGpu -and -not $env:VULKAN_SDK) {
    $vulkanSdk = Resolve-VulkanSdk
    if (-not $vulkanSdk) {
        throw "-Gpu needs the Vulkan SDK (VULKAN_SDK). Install it from LunarG, or drop -Gpu for a CPU-only build."
    }
    $env:VULKAN_SDK = $vulkanSdk
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

function New-MsixPackage {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Stage,
        [Parameter(Mandatory = $true)][string]$OutRoot,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$IdentityFile
    )

    $makeappx = Resolve-SdkTool -Name "makeappx.exe"
    if (-not $makeappx) {
        throw "makeappx.exe was not found. Install the Windows 10/11 SDK, or run this from a developer prompt."
    }
    if (-not (Test-Path $IdentityFile)) {
        throw "Identity file not found: $IdentityFile"
    }

    $identity = Get-Content -Path $IdentityFile -Raw | ConvertFrom-Json
    foreach ($key in @("packageName", "publisher", "publisherDisplayName", "displayName", "description", "applicationId", "backgroundColor")) {
        $value = $identity.PSObject.Properties[$key]
        if (-not $value -or -not $value.Value) {
            throw "$IdentityFile is missing '$key'"
        }
    }
    if ($identity.publisher -match '^CN=0{8}-') {
        Write-Warn "The publisher in $IdentityFile is still the placeholder; the Store will reject this package."
    }

    $layout = Join-Path $OutRoot "$Name-msix\layout"
    if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
    New-Item -ItemType Directory -Path $layout -Force | Out-Null

    # Only the executable and its two runtime DLLs. deploy.bat, the sample audio
    # and the models are direct-download baggage; the Store build downloads
    # models on first run instead.
    Copy-Item (Join-Path $Stage "hippo-transcribe.exe") $layout
    foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll")) {
        Copy-Item (Join-Path $Stage $dll) $layout
    }
    $assets = Join-Path $layout "Assets"
    New-Item -ItemType Directory -Path $assets -Force | Out-Null
    Copy-Item (Join-Path $Root "packaging\assets\*.png") $assets

    $template = Get-Content -Path (Join-Path $Root "packaging\AppxManifest.xml.template") -Raw
    if (-not $template) {
        throw "packaging\AppxManifest.xml.template is missing or empty"
    }
    $values = [ordered]@{
        PackageName          = $identity.packageName
        Publisher            = $identity.publisher
        PublisherDisplayName = $identity.publisherDisplayName
        DisplayName          = $identity.displayName
        Description          = $identity.description
        ApplicationId        = $identity.applicationId
        BackgroundColor      = $identity.backgroundColor
        Version              = (ConvertTo-FourPartVersion -Version $Version)
    }
    foreach ($key in $values.Keys) {
        $template = $template.Replace("{{$key}}", [string]$values[$key])
    }
    if ($template -match '\{\{') {
        throw "The rendered manifest still has placeholders; check packaging\AppxManifest.xml.template"
    }
    Set-Content -Path (Join-Path $layout "AppxManifest.xml") -Value $template -Encoding UTF8

    $msix = Join-Path $OutRoot "$Name.msix"
    if (Test-Path $msix) { Remove-Item $msix -Force }
    Write-Step "Packing $msix"
    # Out-Host keeps makeappx's chatter out of the function's return value.
    & $makeappx pack /d $layout /p $msix /o | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "makeappx failed with exit code $LASTEXITCODE"
    }

    $hash = (Get-FileHash $msix -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $(Split-Path $msix -Leaf)" | Set-Content "$msix.sha256" -Encoding ASCII
    return $msix
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
}

function Invoke-Wack {
    param(
        [Parameter(Mandatory = $true)][string]$Msix,
        [Parameter(Mandatory = $true)][string]$ReportPath
    )

    $appcert = Resolve-SdkTool -Name "appcert.exe"
    if (-not $appcert) {
        throw "appcert.exe (Windows App Certification Kit) was not found. Install the Windows 10/11 SDK."
    }

    Write-Step "Running the Windows App Certification Kit"
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
        & (Join-Path $PSScriptRoot "fetch-models.ps1") -Dest (Join-Path $stage "models")
    }

    $zip = Join-Path $outRoot "$name.zip"
    if (-not $NoZip) {
        Write-Step "Writing $zip"
        if (Test-Path $zip) { Remove-Item $zip -Force }
        Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path $zip -Leaf)" | Set-Content "$zip.sha256" -Encoding ASCII
    }

    $msix = $null
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

        $msix = New-MsixPackage -Root $root -Stage $stage -OutRoot $outRoot -Name $name -Version $releaseVersion -IdentityFile $identityFile

        if ($Sign -or $CertificatePath) {
            if (-not $CertificatePath) {
                throw "-Sign needs -CertificatePath pointing at a .pfx"
            }
            $certificate = if ([System.IO.Path]::IsPathRooted($CertificatePath)) {
                $CertificatePath
            } else {
                Join-Path $root $CertificatePath
            }
            Invoke-MsixSigning -Msix $msix -Certificate $certificate -Password $CertificatePassword
        }

        if ($Wack) {
            if (-not $CertificatePath) {
                throw "-Wack needs a signed package; pass -Sign -CertificatePath as well"
            }
            Invoke-Wack -Msix $msix -ReportPath (Join-Path $outRoot "$name-wack.xml")
        }
    }

    Write-Host ""
    Write-Host "Build complete" -ForegroundColor Green
    Write-Host "  Binary: $binary"
    Write-Host "  Stage:  $stage"
    if (-not $NoZip) { Write-Host "  Zip:    $zip" }
    if ($msix) { Write-Host "  MSIX:   $msix" }
}
finally {
    Pop-Location
}
