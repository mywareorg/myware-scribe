#Requires -Version 5.1
<#
.SYNOPSIS
    Installs short git aliases into the current user's PowerShell profile.

.DESCRIPTION
    Adds three functions that make the round trip to the build machine shorter:

        gph              git push origin main
        gpl              git pull --rebase origin main
        gcm <message>    git commit -m <message>

    The block is written between marker comments, so running this again replaces
    the previous version instead of appending a second copy.

.EXAMPLE
    .\scripts\install-git-shortcuts.ps1

.EXAMPLE
    .\scripts\install-git-shortcuts.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    # Overrides the detected profile path. Useful for testing without touching
    # the real profile.
    [string]$ProfilePath = "",
    # Remove the block again.
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$markerStart = "# >>> hippo-transcribe git shortcuts >>>"
$markerEnd = "# <<< hippo-transcribe git shortcuts <<<"

function Get-DefaultProfilePath {
    # Windows PowerShell 5.1 keeps its profile under WindowsPowerShell; PowerShell
    # 7 uses PowerShell. Pick the one that matches the running edition.
    $documents = [Environment]::GetFolderPath("MyDocuments")
    if ($PSVersionTable.PSEdition -eq "Core") {
        return Join-Path $documents "PowerShell\profile.ps1"
    }
    return Join-Path $documents "WindowsPowerShell\profile.ps1"
}

function Remove-Block {
    param(
        [Parameter(Mandatory = $true)][string]$Text
    )
    $lines = $Text -split "`r?`n"
    $kept = New-Object System.Collections.Generic.List[string]
    $skipping = $false
    foreach ($line in $lines) {
        if ($line.Trim() -eq $markerStart) { $skipping = $true; continue }
        if ($line.Trim() -eq $markerEnd) { $skipping = $false; continue }
        if (-not $skipping) { $kept.Add($line) }
    }
    return ($kept -join "`r`n")
}

$target = if ($ProfilePath) { $ProfilePath } else { Get-DefaultProfilePath }
$target = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($target)

New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null

$existing = ""
if (Test-Path $target) {
    $existing = Get-Content -Path $target -Raw -Encoding UTF8
    if ($null -eq $existing) { $existing = "" }
}

$body = Remove-Block -Text $existing
$body = $body.TrimEnd()

if ($Uninstall) {
    Set-Content -Path $target -Value $body -Encoding UTF8
    Write-Host "Removed the git shortcuts from $target" -ForegroundColor Green
    Write-Host "Open a new terminal, or run: . `$PROFILE" -ForegroundColor DarkGray
    exit 0
}

$block = @(
    $markerStart,
    "# gph / gpl / gcm - installed by scripts/install-git-shortcuts.ps1",
    "function gph { git push origin main @args }",
    "function gpl { git pull --rebase origin main @args }",
    "function gcm {",
    "    param(",
    "        [Parameter(Position = 0, Mandatory = `$true)][string]`$Message,",
    "        [Parameter(Position = 1, ValueFromRemainingArguments = `$true)][string[]]`$Rest = @()",
    "    )",
    "    git commit -m `$Message @Rest",
    "}",
    $markerEnd
)

$content = if ($body) { $body + "`r`n`r`n" + ($block -join "`r`n") } else { $block -join "`r`n" }
Set-Content -Path $target -Value $content -Encoding UTF8

Write-Host "Installed the git shortcuts in $target" -ForegroundColor Green
Write-Host "  gph              git push origin main" -ForegroundColor DarkGray
Write-Host "  gpl              git pull --rebase origin main" -ForegroundColor DarkGray
Write-Host "  gcm <message>    git commit -m <message>" -ForegroundColor DarkGray
Write-Host "Open a new terminal, or run: . `$PROFILE" -ForegroundColor DarkGray
