#Requires -Version 5.1
<#
.SYNOPSIS
    Reproduces the compiler-identification failure of ggml's shader generator.

.DESCRIPTION
    ggml configures its Vulkan shader generator as a nested ExternalProject, and
    the compiler identification step inside it fails with

        cl.exe ... /FdCMakeFiles\cmTC_*.dir\ ...
        fatal error C1083: Cannot open compiler generated file: '': Invalid argument

    This runs that exact command line by hand, with the /Fd flag in a few shapes,
    and quotes the surrounding text of every TryCompile warning in the build log,
    which the failure report can only show as truncated paths.

    Writes dist\cl-diagnose.txt.
#>
[CmdletBinding()]
param([string]$Out = "")

$ErrorActionPreference = "Continue"
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir

if (-not $Out) { $Out = Join-Path $root "dist\cl-diagnose.txt" }
New-Item -ItemType Directory -Path (Split-Path -Parent $Out) -Force | Out-Null

$report = New-Object System.Collections.Generic.List[string]
function Add-Line { param([string]$Text = "") ; $report.Add($Text) | Out-Null }
function Add-Section { param([string]$Title) ; Add-Line "" ; Add-Line "=== $Title ===" }

$cl = Get-Command cl.exe -ErrorAction SilentlyContinue
Add-Line ("cl.exe : " + $(if ($cl) { $cl.Source } else { "not on PATH" }))
if ($cl) { Add-Line ((& $cl.Source 2>&1 | Select-Object -First 3) -join " / ") }
$cmake = Get-Command cmake.exe -ErrorAction SilentlyContinue
Add-Line ("cmake  : " + $(if ($cmake) { $cmake.Source } else { "not on PATH" }))
if ($cmake) { Add-Line ((& $cmake.Source --version 2>&1 | Select-Object -First 1)) }
Add-Line ("ninja  : " + ((Get-Command ninja.exe -ErrorAction SilentlyContinue).Source))
Add-Line ("TEMP   : " + $env:TEMP)
Add-Line ("TMP    : " + $env:TMP)

# --- context around the warnings that the failure report truncates
$log = Join-Path $root "build-windows.log"
Add-Section "TryCompile warnings in build-windows.log"
if (Test-Path $log) {
    $all = @(Get-Content -Path $log)
    Add-Line ("log lines: " + $all.Count)
    $indexes = @()
    for ($i = 0; $i -lt $all.Count; $i++) {
        if ($all[$i] -match "CMake Warning in .*TryCompile") { $indexes += $i }
    }
    if ($indexes.Count -eq 0) { Add-Line "(none)" }
    foreach ($i in ($indexes | Select-Object -First 3)) {
        Add-Line ""
        Add-Line ("--- around line " + ($i + 1) + " ---")
        $from = [Math]::Max(0, $i - 1)
        $to = [Math]::Min($all.Count - 1, $i + 8)
        for ($j = $from; $j -le $to; $j++) { Add-Line ("  " + $all[$j]) }
    }
    $c1083 = -1
    for ($i = 0; $i -lt $all.Count; $i++) {
        if ($all[$i] -match "fatal error C1083") { $c1083 = $i; break }
    }
    if ($c1083 -ge 0) {
        Add-Line ""
        Add-Line ("--- around the first C1083 at line " + ($c1083 + 1) + " ---")
        $from = [Math]::Max(0, $c1083 - 12)
        for ($j = $from; $j -le $c1083; $j++) { Add-Line ("  " + $all[$j]) }
    }
} else {
    Add-Line "build-windows.log is missing"
}

# --- run the compiler identification command by hand
Add-Section "Reproducing the compiler identification"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("clprobe-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
$scratch = Join-Path $work "CMakeFiles\cmTC_probe.dir"
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
$source = Join-Path $work "testCCompiler.c"
Set-Content -Path $source -Value "int main(void) { return 0; }" -Encoding ASCII
Add-Line ("work dir : " + $work)

if ($cl) {
    $cases = @(
        @{ Name = "/Fd<dir>\ (what CMake emits)"; Fd = "/FdCMakeFiles\cmTC_probe.dir\" },
        @{ Name = "/Fd<dir>/vc140.pdb";            Fd = "/FdCMakeFiles\cmTC_probe.dir/vc140.pdb" },
        @{ Name = "/Fdvc140.pdb (bare name)";     Fd = "/Fdvc140.pdb" },
        @{ Name = "no /Fd at all";                Fd = "" }
    )
    Push-Location $work
    try {
        foreach ($case in $cases) {
            Add-Line ""
            Add-Line ("--- " + $case.Name)
            $arguments = @("/nologo", "/Zi", "/Ob0", "/Od", "/RTC1", "-MDd", "/showIncludes")
            if ($case.Fd) { $arguments += $case.Fd }
            $arguments += @("/FS", "/FoCMakeFiles\cmTC_probe.dir\testCCompiler.c.obj", "/c", "testCCompiler.c")
            Add-Line ("    cl " + ($arguments -join " "))
            $output = & $cl.Source @arguments 2>&1
            Add-Line ("    exit: " + $LASTEXITCODE)
            foreach ($line in @($output | Select-Object -First 6)) {
                if ("$line".Trim() -and "$line" -notmatch "^Note: including file") { Add-Line ("    " + $line) }
            }
            Remove-Item (Join-Path $scratch "testCCompiler.c.obj") -Force -ErrorAction SilentlyContinue
            Remove-Item (Join-Path $work "vc140.pdb") -Force -ErrorAction SilentlyContinue
            Remove-Item (Join-Path $scratch "vc140.pdb") -Force -ErrorAction SilentlyContinue
        }
    } finally {
        Pop-Location
    }
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

$text = ($report -join "`r`n")
Set-Content -Path $Out -Value $text -Encoding UTF8
Write-Host $text
Write-Host ""
Write-Host ("Report: " + $Out) -ForegroundColor Green
