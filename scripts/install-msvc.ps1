#Requires -Version 5.1
<#
.SYNOPSIS
    Find the MSVC C++ compiler (cl.exe) or install the Visual Studio C++
    workload that provides it.

.DESCRIPTION
    cl.exe is not a standalone package. It ships with the MSVC toolset in
    Visual Studio Build Tools, Community, Professional, or Enterprise.

    This script:
      1. Finds an existing Visual Studio installation with the C++ workload.
      2. Finds cl.exe under VC\Tools\MSVC.
      3. If no C++ workload exists, installs it into the newest existing
         Visual Studio installation, or installs Build Tools when none exists.
      4. Prints the cl.exe path and the command that loads the developer
         environment into the current PowerShell session.

    Installation requires an elevated PowerShell. Finding an existing compiler
    does not.

.PARAMETER InstallPath
    Installation directory used only when no Visual Studio installation exists.
    Defaults to the Visual Studio 2022 Build Tools directory.

.PARAMETER Quiet
    Run the Visual Studio installer without its progress UI.

.PARAMETER VerifyOnly
    Find and report cl.exe without installing anything.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\scripts\install-msvc.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\scripts\install-msvc.ps1 -VerifyOnly
#>
[CmdletBinding()]
param(
    [string]$InstallPath = "",
    [switch]$Quiet,
    [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Note {
    param([string]$Message)
    Write-Host "    $Message" -ForegroundColor DarkGray
}

function Get-ProgramFilesX86 {
    if (${env:ProgramFiles(x86)}) { return ${env:ProgramFiles(x86)} }
    return "C:\Program Files (x86)"
}

function Find-VsWhere {
    $base = Get-ProgramFilesX86
    $candidates = @(
        (Join-Path $base "Microsoft Visual Studio\Installer\vswhere.exe"),
        (Join-Path $env:ProgramFiles "Microsoft Visual Studio\Installer\vswhere.exe")
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
    return $null
}

function Get-VsPath {
    param(
        [string]$VsWhere,
        [switch]$RequireCpp
    )
    if (-not $VsWhere) { return $null }

    $arguments = @("-latest", "-products", "*", "-property", "installationPath")
    if ($RequireCpp) {
        $arguments += @("-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64")
    }

    $paths = @(& $VsWhere @arguments)
    if ($LASTEXITCODE -ne 0 -or $paths.Count -eq 0) { return $null }

    foreach ($path in $paths) {
        if (-not [string]::IsNullOrWhiteSpace($path)) {
            return $path.Trim()
        }
    }
    return $null
}

function Find-MsvcCl {
    param([string]$VsPath)
    if (-not $VsPath) { return $null }

    $toolRoot = Join-Path $VsPath "VC\Tools\MSVC"
    if (-not (Test-Path -LiteralPath $toolRoot)) { return $null }

    $matches = @(
        Get-ChildItem -LiteralPath $toolRoot -Filter "cl.exe" -Recurse -File `
            -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\bin\\Hostx64\\x64\\cl\.exe$' } |
            Sort-Object FullName -Descending
    )
    if ($matches.Count -eq 0) { return $null }
    return $matches[0].FullName
}

function Quote-Argument {
    param([string]$Value)
    if ($Value -match '[\s"]') {
        return '"' + ($Value -replace '"', '\"') + '"'
    }
    return $Value
}

function Assert-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Installing the Visual Studio C++ workload requires an elevated PowerShell. Re-run this script as Administrator."
    }
}

function Invoke-VsInstaller {
    param(
        [string]$VsInstaller,
        [string[]]$Arguments
    )
    $commandLine = ($Arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    Write-Note "vs_installer.exe $commandLine"
    $process = Start-Process -FilePath $VsInstaller `
        -ArgumentList $commandLine `
        -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "Visual Studio Installer exited with code $($process.ExitCode)."
    }
}

function Install-CppWorkload {
    param([string]$VsPath, [switch]$Quiet)

    Assert-Elevated

    $installer = Join-Path (Get-ProgramFilesX86) "Microsoft Visual Studio\Installer\vs_installer.exe"
    if (-not (Test-Path -LiteralPath $installer)) {
        throw "vs_installer.exe was not found at $installer. Reinstall Visual Studio Installer."
    }

    $arguments = @(
        "modify",
        "--installPath", $VsPath,
        "--add", "Microsoft.VisualStudio.Workload.VCTools",
        "--includeRecommended",
        "--norestart"
    )
    if ($Quiet) { $arguments += "--quiet" } else { $arguments += "--passive" }

    Write-Step "Adding the Desktop development with C++ workload to $VsPath"
    Write-Note "This downloads several GB and can take a while."
    Invoke-VsInstaller -VsInstaller $installer -Arguments $arguments
}

function Install-BuildTools {
    param([string]$Destination, [switch]$Quiet)

    Assert-Elevated

    if ([string]::IsNullOrWhiteSpace($Destination)) {
        $Destination = Join-Path (Get-ProgramFilesX86) "Microsoft Visual Studio\2022\BuildTools"
    }

    $bootstrapper = Join-Path $env:TEMP "vs_BuildTools.exe"
    Write-Step "Downloading the Visual Studio Build Tools bootstrapper"
    Invoke-WebRequest `
        -Uri "https://aka.ms/vs/17/release/vs_BuildTools.exe" `
        -OutFile $bootstrapper `
        -UseBasicParsing

    $arguments = @(
        "--wait",
        "--norestart",
        "--nocache",
        "--installPath", $Destination,
        "--add", "Microsoft.VisualStudio.Workload.VCTools",
        "--includeRecommended"
    )
    if ($Quiet) { $arguments += "--quiet" } else { $arguments += "--passive" }

    Write-Step "Installing Visual Studio Build Tools with the C++ workload"
    $commandLine = ($arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    Write-Note "vs_BuildTools.exe $commandLine"
    $process = Start-Process -FilePath $bootstrapper `
        -ArgumentList $commandLine `
        -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "Visual Studio Build Tools installer exited with code $($process.ExitCode)."
    }
}

function Show-Result {
    param(
        [string]$VsPath,
        [string]$ClPath
    )

    Write-Host ""
    Write-Host "MSVC is available." -ForegroundColor Green
    Write-Host "  Visual Studio : $VsPath"
    Write-Host "  cl.exe        : $ClPath"
    Write-Host ""
    Write-Host "Load it into the current PowerShell session:"
    $launch = Join-Path $VsPath "Common7\Tools\Launch-VsDevShell.ps1"
    if (Test-Path -LiteralPath $launch) {
        Write-Host "  & `"$launch`" -Arch amd64 -HostArch amd64"
    } else {
        Write-Host "  Open a Developer PowerShell for Visual Studio."
    }
    Write-Host ""
    Write-Host "Verify:"
    Write-Host "  where.exe cl"
    Write-Host "  cl"
}

$vsWhere = Find-VsWhere
$vsPath = Get-VsPath -VsWhere $vsWhere -RequireCpp

if ($vsPath) {
    $clPath = Find-MsvcCl -VsPath $vsPath
    if ($clPath) {
        Show-Result -VsPath $vsPath -ClPath $clPath
        exit 0
    }
    Write-Step "Found Visual Studio at $vsPath but no cl.exe"
} else {
    $vsPath = Get-VsPath -VsWhere $vsWhere
    if ($vsPath) {
        Write-Step "Found Visual Studio without the C++ workload at $vsPath"
    } else {
        Write-Step "No Visual Studio installation was found"
    }
}

if ($VerifyOnly) {
    Write-Host ""
    Write-Host "[!] cl.exe was not found." -ForegroundColor Yellow
    Write-Host "    Re-run without -VerifyOnly to install the C++ workload."
    exit 1
}

if ($vsPath) {
    Install-CppWorkload -VsPath $vsPath -Quiet:$Quiet
} else {
    Install-BuildTools -Destination $InstallPath -Quiet:$Quiet
}

# vswhere can lag briefly after the installer exits.
Start-Sleep -Seconds 2
$vsWhere = Find-VsWhere
$vsPath = Get-VsPath -VsWhere $vsWhere -RequireCpp
if (-not $vsPath -and $InstallPath) { $vsPath = $InstallPath }
if (-not $vsPath) {
    throw "The C++ workload was installed, but no Visual Studio installation with it was found. Open Visual Studio Installer and verify 'Desktop development with C++'."
}

$clPath = Find-MsvcCl -VsPath $vsPath
if (-not $clPath) {
    throw "The C++ workload was installed, but cl.exe was not found under $vsPath. Reboot, then run this script again with -VerifyOnly."
}

Show-Result -VsPath $vsPath -ClPath $clPath
