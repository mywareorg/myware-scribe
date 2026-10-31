#Requires -Version 5.1
<#
.SYNOPSIS
    Replays ggml's nested Vulkan shader configure in a few seconds.

.DESCRIPTION
    cargo spends minutes reaching the step that actually fails. That step is a
    second cmake process that ggml starts with ExternalProject_Add to build
    vulkan-shaders-gen, and it can be run on its own against the tree a previous
    build already produced:

        cmake ... -GNinja
              -S <target>\build\whisper-rs-sys-*\out\whisper.cpp\ggml\src\ggml-vulkan\vulkan-shaders
              -B <scratch>

    This reproduces it with the same arguments and the same environment the
    build sets up (x64 cl on PATH, the CMake that ships with Visual Studio,
    CMAKE_GENERATOR=Ninja), prints the whole output instead of the truncated
    excerptions the build log shows, and writes it to dist\probe-shaders.txt.

    Nothing is compiled and no rust code is touched, so a fix can be tried in
    seconds. Requires one real build to have run far enough to unpack
    whisper.cpp; if the tree is missing, run the build once first.

.PARAMETER Define
    Extra -D arguments for the nested configure, for testing a hypothesis
    without editing anything else. Repeatable.

.PARAMETER Keep
    Keep the scratch build directory instead of deleting it first, which
    preserves CMakeCache.txt between runs.

.PARAMETER BuildDir
    Configure into this directory instead of the scratch one. Point it at the
    deep path ggml uses inside the real build to reproduce a failure that only
    happens there, for example
    target\release\build\whisper-rs-sys-*\out\build\ggml\src\ggml-vulkan\vulkan-shaders-gen-prefix\src\vulkan-shaders-gen-build

.PARAMETER NoSetup
    Do not touch the environment; use the one this process inherited. Used by
    build-windows.ps1 -Probe, which has already set it up.

.EXAMPLE
    .\scripts\probe-shaders.ps1

.EXAMPLE
    .\scripts\probe-shaders.ps1 -Define CMAKE_C_COMPILER=C:\VS\Hostx64\x64\cl.exe
#>
[CmdletBinding()]
param(
    [string[]]$Define = @(),
    [string]$BuildDir = "",
    [switch]$Keep,
    [switch]$NoSetup
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
. (Join-Path $scriptDir "windows-env.ps1")

$outRoot = Join-Path $root "dist\probe-shaders"
$reportPath = Join-Path $root "dist\probe-shaders.txt"
New-Item -ItemType Directory -Path (Split-Path -Parent $reportPath) -Force | Out-Null

$report = New-Object System.Collections.Generic.List[string]
function Add-Line { param([string]$Text = "") ; $report.Add($Text) | Out-Null }
function Add-Section { param([string]$Title) ; Add-Line "" ; Add-Line "=== $Title ===" }
function Save-Report { Set-Content -Path $reportPath -Value $report -Encoding UTF8 }

Add-Line ("=== Shader probe " + (Get-Date -Format "yyyy-MM-dd HH:mm:ss") + " ===")

try {
if (-not $NoSetup) {
    $found = Get-Command cl.exe -ErrorAction SilentlyContinue
    $clPath = if ($found) { $found.Source } else { "" }
    if (-not $clPath -or $clPath -notmatch 'Host(X64|ARM64)\\x64') {
        Write-Step "Entering the x64 developer environment"
        Enter-DevEnvironment
        $found = Get-Command cl.exe -ErrorAction SilentlyContinue
        $clPath = if ($found) { $found.Source } else { "" }
    }
    $vsCmake = Join-Path (Get-VsInstallPath) "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
    if (Test-Path (Join-Path $vsCmake "cmake.exe")) {
        $env:PATH = $vsCmake + ";" + $env:PATH
    }
    $env:CMAKE_GENERATOR = "Ninja"
}

$clCommand = Get-Command cl.exe -ErrorAction SilentlyContinue
$cmakeCommand = Get-Command cmake.exe -ErrorAction SilentlyContinue
$ninjaCommand = Get-Command ninja.exe -ErrorAction SilentlyContinue
Add-Line ("cl    : " + $(if ($clCommand) { $clCommand.Source } else { "not on PATH" }))
Add-Line ("cmake : " + $(if ($cmakeCommand) { $cmakeCommand.Source } else { "not on PATH" }))
Add-Line ("ninja : " + $(if ($ninjaCommand) { $ninjaCommand.Source } else { "not on PATH" }))
Add-Line ("generator : " + $(if ($env:CMAKE_GENERATOR) { $env:CMAKE_GENERATOR } else { "(unset)" }))

if (-not $cmakeCommand) { throw "cmake.exe is not on PATH." }
if (-not $clCommand) { throw "cl.exe is not on PATH; run this from a Developer PowerShell or drop -NoSetup." }

# The unpacked whisper.cpp from the last build. Newest wins; there are normally
# two whisper-rs-sys directories, one configured with Vulkan on and one off.
$targetRoot = Resolve-TargetRoot -Root $root
$buildRoot = Join-Path $targetRoot "release\build"
$candidates = @()
if (Test-Path $buildRoot) {
    $candidates = @(Get-ChildItem -Path $buildRoot -Directory -Filter "whisper-rs-sys-*" -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName "out\whisper.cpp\ggml\src\ggml-vulkan\vulkan-shaders\CMakeLists.txt") } |
        Sort-Object LastWriteTime -Descending)
}
if ($candidates.Count -eq 0) {
    throw "No unpacked whisper.cpp under $buildRoot. Run the build once so cargo unpacks it; the shader configure happens before the failure."
}
$source = Join-Path $candidates[0].FullName "out\whisper.cpp\ggml\src\ggml-vulkan\vulkan-shaders"
Add-Line ("source: " + $source)

$buildDir = if ($BuildDir) { $BuildDir } else { Join-Path $outRoot "build" }
if (-not $Keep -and (Test-Path $buildDir)) {
    Remove-DirectoryTree -Path $buildDir
}
New-Item -ItemType Directory -Path $outRoot -Force | Out-Null

$cmakeArgs = @(
    ("-DCMAKE_INSTALL_PREFIX=" + (Join-Path $outRoot "install")),
    "-DCMAKE_INSTALL_BINDIR=.",
    "-DCMAKE_BUILD_TYPE=Release",
    "-DGGML_VULKAN_COOPMAT_GLSLC_SUPPORT=ON",
    "-DGGML_VULKAN_COOPMAT2_GLSLC_SUPPORT=ON",
    "-DGGML_VULKAN_INTEGER_DOT_GLSLC_SUPPORT=ON",
    "-DGGML_VULKAN_BFLOAT16_GLSLC_SUPPORT=ON",
    "-GNinja",
    "-S", $source,
    "-B", $buildDir
)
foreach ($item in $Define) { $cmakeArgs += @("-D" + $item) }

Add-Line ("build : " + $buildDir)
# cl silently misbehaves past MAX_PATH (260) on Windows unless long paths are
# enabled; a deep ExternalProject prefix is how that gets reached.
Add-Line ("length: " + $buildDir.Length + " characters (+ \CMakeFiles\cmTC_xxxxxx.dir\ is " + ($buildDir.Length + 27) + ")")

Add-Section "Command"
Add-Line ($cmakeCommand.Source + " " + ($cmakeArgs -join " "))

Add-Section "Output"
Write-Step "Configuring the shader sub-project (this is the step the build fails in)"

# Windows PowerShell 5.1 turns a native command's stderr into ErrorRecords as soon
# as it is merged with 2>&1, and StrictMode plus Stop turns those into a
# terminating error even when the tool succeeded. Redirect to files instead.
$stdoutFile = Join-Path $outRoot "stdout.txt"
$stderrFile = Join-Path $outRoot "stderr.txt"
Remove-Item $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

$quoted = ($cmakeArgs | ForEach-Object { '"' + $_ + '"' }) -join " "
$process = Start-Process -FilePath $cmakeCommand.Source -ArgumentList $quoted -NoNewWindow -Wait -PassThru `
    -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
$exitCode = $process.ExitCode

$output = @()
foreach ($file in @($stdoutFile, $stderrFile)) {
    if (Test-Path $file) { $output += @(Get-Content -Path $file) }
}
foreach ($line in $output) { Add-Line $line ; Write-Host $line }

Add-Section "Verdict"
Add-Line ("exit code: " + $exitCode)
if ($output -match 'The C compiler identification is') {
    Add-Line "Compiler identification succeeded: cmake accepted the toolchain."
}
foreach ($line in $output) {
    if ($line -match 'fatal error C[0-9]+|CMake Error|No CMAKE_C|No CMAKE_CXX|ninja: ') {
        Add-Line ("  " + $line.Trim())
    }
}
if ($exitCode -eq 0) {
    Add-Line "The nested configure passes here. A build failure elsewhere is not this step."
} else {
    Add-Line "The nested configure fails here, so a full build is not needed to iterate: change the arguments or the environment and run this again."
}

Save-Report
Write-Host ""
Write-Host ("  Report: " + $reportPath) -ForegroundColor Cyan
exit $exitCode
}
catch {
    Add-Section "Probe failed"
    Add-Line ("Message: " + $_.Exception.Message)
    Add-Line ("At: " + $_.InvocationInfo.PositionMessage)
    Save-Report
    Write-Host ("  Report: " + $reportPath) -ForegroundColor Cyan
    throw
}
