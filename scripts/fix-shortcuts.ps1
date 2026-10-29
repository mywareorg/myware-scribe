#Requires -Version 5.1
<#
.SYNOPSIS
    Installs the git shortcuts, reloads the profile, and reports what happened.

.DESCRIPTION
    One command that does the whole round trip for the shortcut problem:

      1. optionally pulls origin/main (skip with -NoPull),
      2. runs install-git-shortcuts.ps1,
      3. reloads the profile and checks that ga, gph, gpl and gcm are defined,
      4. writes dist\shortcuts-diagnose.txt and prints the same lines.

    Loading the profile here only proves it parses and defines the functions; it
    happens in this script's scope, not in your interactive window. Open a new
    terminal (or run . $PROFILE) to use the shortcuts.

.EXAMPLE
    .\scripts\fix-shortcuts.ps1
#>
[CmdletBinding()]
param(
    # Skip the git pull; use when the working tree has changes you want to keep.
    [switch]$NoPull
)

$ErrorActionPreference = "Stop"

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
$installer = Join-Path $scriptDir "install-git-shortcuts.ps1"
$reportPath = Join-Path $root "dist\shortcuts-diagnose.txt"
New-Item -ItemType Directory -Path (Split-Path -Parent $reportPath) -Force | Out-Null

$report = New-Object System.Collections.Generic.List[string]
function Add-Line { param([string]$Text = "") ; $report.Add($Text) | Out-Null }

function Invoke-Git {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    $stdout = Join-Path $env:TEMP ("git-" + [guid]::NewGuid().ToString("N") + ".out")
    $stderr = Join-Path $env:TEMP ("git-" + [guid]::NewGuid().ToString("N") + ".err")
    $process = Start-Process -FilePath "git" -ArgumentList $Arguments -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $text = @()
    foreach ($file in @($stdout, $stderr)) {
        if (Test-Path $file) { $text += (Get-Content -Path $file -ErrorAction SilentlyContinue) ; Remove-Item $file -Force -ErrorAction SilentlyContinue }
    }
    return @{ ExitCode = $process.ExitCode ; Text = @($text) }
}

Add-Line ("=== " + (Get-Date -Format "yyyy-MM-dd HH:mm:ss") + " ===")
Add-Line ("Repo      : " + $root)
Add-Line ("Edition   : " + $PSVersionTable.PSEdition + " " + $PSVersionTable.PSVersion)
Add-Line ("Profile   : " + $PROFILE)
Add-Line ("Exists    : " + (Test-Path $PROFILE))
Add-Line ("Policy    : " + (Get-ExecutionPolicy))

if ($NoPull) {
    Add-Line ""
    Add-Line "=== pull skipped ==="
} else {
    Add-Line ""
    Add-Line "=== git pull --rebase origin main ==="
    try {
        $pull = Invoke-Git -Arguments @("pull", "--rebase", "origin", "main")
        foreach ($line in $pull.Text) { Add-Line $line }
        Add-Line ("exit code : " + $pull.ExitCode)
    } catch {
        Add-Line ("pull failed: " + $_.Exception.Message)
    }
}

Add-Line ""
Add-Line "=== install ==="
if (-not (Test-Path $installer)) {
    Add-Line "install-git-shortcuts.ps1 is missing; pull first."
} else {
    try {
        & $installer 2>&1 | ForEach-Object { Add-Line $_.ToString() }
    } catch {
        Add-Line ("install failed: " + $_.Exception.Message)
    }
}

Add-Line ""
Add-Line "=== reload ==="
if (-not $PROFILE) {
    Add-Line "This host reports no profile path."
} elseif (-not (Test-Path $PROFILE)) {
    Add-Line "The profile was not created."
} else {
    try {
        . $PROFILE
        Add-Line "profile loaded"
    } catch {
        Add-Line ("profile failed: " + $_.Exception.Message)
    }
    Add-Line ""
    Add-Line "=== profile tail ==="
    Get-Content -Path $PROFILE -Tail 16 -ErrorAction SilentlyContinue | ForEach-Object { Add-Line $_ }
}

Add-Line ""
Add-Line "=== shortcuts ==="
$missing = 0
foreach ($name in @("ga", "gph", "gpl", "gcm")) {
    $command = Get-Command $name -ErrorAction SilentlyContinue
    if ($command) {
        Add-Line ("{0,-4} : ok" -f $name)
    } else {
        Add-Line ("{0,-4} : missing" -f $name)
        $missing++
    }
}

Set-Content -Path $reportPath -Value $report -Encoding UTF8
$report | ForEach-Object { Write-Host $_ }
Write-Host ""
Write-Host ("Report: " + $reportPath) -ForegroundColor Green

if ($missing -gt 0) {
    Write-Host "Open a new terminal and try again; if they are still missing, send the report." -ForegroundColor Yellow
    exit 1
}
Write-Host "The shortcuts work in a new terminal (or after: . `$PROFILE)." -ForegroundColor Green
