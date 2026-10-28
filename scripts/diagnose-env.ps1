#Requires -Version 5.1
<#
.SYNOPSIS
    Reports how this shell is set up for the Visual Studio C++ toolchain.

.DESCRIPTION
    The build calls VsDevCmd.bat when cl.exe is missing or hosted on the wrong
    architecture. On one machine that batch file exits with code 1 and prints
    nothing at all, so this writes what the build needs to know: where the
    compiler is, whether the include/lib variables are set, and what VsDevCmd
    actually does when its streams are captured properly.

    Writes dist\env-diagnose.txt. Push that file; it is small.
#>
[CmdletBinding()]
param([string]$Out = "")

$ErrorActionPreference = "Stop"
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
. (Join-Path $scriptDir "windows-env.ps1")

if (-not $Out) { $Out = Join-Path $root "dist\env-diagnose.txt" }
New-Item -ItemType Directory -Path (Split-Path -Parent $Out) -Force | Out-Null

$report = New-Object System.Collections.Generic.List[string]
function Add-Line { param([string]$Text = "") ; $report.Add($Text) | Out-Null }
function Add-Section { param([string]$Title) ; Add-Line "" ; Add-Line "=== $Title ===" }

Add-Line ("Shell         : " + $host.Name + " " + $PSVersionTable.PSVersion)
Add-Line ("Started by    : " + (Get-CimInstance Win32_Process -Filter ("ProcessId=" + $PID)).ParentProcessId)
Add-Line ("CMAKE_GENERATOR: " + $env:CMAKE_GENERATOR)

Add-Section "Visual Studio"
$vs = $null
try { $vs = Get-VsInstallPath; Add-Line ("Install : " + $vs) }
catch { Add-Line ("Install : ERROR " + $_.Exception.Message) }

if ($vs) {
    $devCmd = Join-Path $vs "Common7\Tools\VsDevCmd.bat"
    $launch = Join-Path $vs "Common7\Tools\Launch-VsDevShell.ps1"
    Add-Line ("VsDevCmd.bat            : " + (Test-Path $devCmd) + " " + $devCmd)
    Add-Line ("Launch-VsDevShell.ps1   : " + (Test-Path $launch))
    $msvc = Join-Path $vs "VC\Tools\MSVC"
    if (Test-Path $msvc) {
        foreach ($version in (Get-ChildItem $msvc -Directory | Sort-Object Name -Descending)) {
            $x64 = Join-Path $version.FullName "bin\Hostx64\x64\cl.exe"
            Add-Line ("MSVC " + $version.Name.PadRight(22) + " Hostx64\x64\cl.exe : " + (Test-Path $x64))
        }
    } else {
        Add-Line "MSVC tools directory missing"
    }
}

Add-Section "Compiler on PATH"
$cl = Get-Command cl.exe -ErrorAction SilentlyContinue
if ($cl) { Add-Line ("cl.exe : " + $cl.Source) } else { Add-Line "cl.exe : not on PATH" }
$ninja = Get-Command ninja.exe -ErrorAction SilentlyContinue
if ($ninja) { Add-Line ("ninja  : " + $ninja.Source) } else { Add-Line "ninja  : not on PATH" }
$cmake = Get-Command cmake.exe -ErrorAction SilentlyContinue
if ($cmake) { Add-Line ("cmake  : " + $cmake.Source) } else { Add-Line "cmake  : not on PATH" }

Add-Section "Toolchain variables"
foreach ($name in @("INCLUDE", "LIB", "LIBPATH", "VSCMD_ARG_TGT_ARCH", "VSCMD_ARG_HOST_ARCH", "WindowsSdkDir", "VCToolsInstallDir", "VisualStudioVersion")) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value) { Add-Line ("$name = " + ($value.Substring(0, [Math]::Min(300, $value.Length)))) }
    else { Add-Line "$name = (unset)" }
}

Add-Section "PATH entries mentioning Visual Studio, MSVC or Windows Kits"
$hits = @(($env:PATH -split ";") | Where-Object { $_ -match "Visual Studio|MSVC|Windows Kits|Vulkan" })
if ($hits.Count -eq 0) { Add-Line "(none)" } else { foreach ($hit in $hits) { Add-Line $hit } }

Add-Section "VsDevCmd.bat, streams captured to files"
if ($vs) {
    $devCmd = Join-Path $vs "Common7\Tools\VsDevCmd.bat"
    if (Test-Path $devCmd) {
        $outFile = Join-Path ([System.IO.Path]::GetTempPath()) "vsdevcmd-out.txt"
        $errFile = Join-Path ([System.IO.Path]::GetTempPath()) "vsdevcmd-err.txt"
        Remove-Item $outFile, $errFile -Force -ErrorAction SilentlyContinue
        try {
            $process = Start-Process -FilePath "cmd.exe" `
                -ArgumentList @("/c", "`"`"$devCmd`" -arch=x64 -host_arch=x64`"") `
                -NoNewWindow -Wait -PassThru `
                -RedirectStandardOutput $outFile -RedirectStandardError $errFile
            Add-Line ("Exit code : " + $process.ExitCode)
        } catch {
            Add-Line ("Failed to start cmd.exe: " + $_.Exception.Message)
        }
        foreach ($file in @($outFile, $errFile)) {
            Add-Line ""
            Add-Line ("--- " + (Split-Path -Leaf $file))
            if (Test-Path $file) {
                $lines = @(Get-Content -Path $file -ErrorAction SilentlyContinue)
                if ($lines.Count -eq 0) { Add-Line "(empty)" }
                else {
                    foreach ($line in ($lines | Select-Object -First 25)) { Add-Line ("  " + $line) }
                    if ($lines.Count -gt 25) { Add-Line ("  ... " + ($lines.Count - 25) + " more lines") }
                }
            } else {
                Add-Line "(missing)"
            }
        }
        Remove-Item $outFile, $errFile -Force -ErrorAction SilentlyContinue
    }
}

$text = ($report -join "`r`n")
Set-Content -Path $Out -Value $text -Encoding UTF8
Write-Host $text
Write-Host ""
Write-Host ("Report: " + $Out) -ForegroundColor Green
