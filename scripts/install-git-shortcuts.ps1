#Requires -Version 5.1
<#
.SYNOPSIS
    Installs short git aliases into the current user's PowerShell profile.

.DESCRIPTION
    Adds three functions that make the round trip to the build machine shorter:

        ga <paths>       git add <paths>
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

$markerStart = "# >>> myware-scribe git shortcuts >>>"
$markerEnd = "# <<< myware-scribe git shortcuts <<<"

function Get-DefaultProfilePath {
    # $PROFILE is the path this host actually loads, so prefer it: Documents may
    # be redirected into OneDrive, and the edition decides the folder name.
    if ($PROFILE) { return $PROFILE }
    $documents = [Environment]::GetFolderPath("MyDocuments")
    if ($PSVersionTable.PSEdition -eq "Core") {
        return Join-Path $documents "PowerShell\profile.ps1"
    }
    return Join-Path $documents "WindowsPowerShell\profile.ps1"
}

function Remove-Block {
    param(
        # Not Mandatory: an empty string is a legitimate input (a profile that
        # does not exist yet), and Mandatory rejects it at binding time.
        [AllowEmptyString()][string]$Text = ""
    )
    if (-not $Text) { return "" }
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

$parent = Split-Path -Parent $target
if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }

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
    "# ga / gph / gpl / gcm - installed by scripts/install-git-shortcuts.ps1",
    "function gph { git push origin main @args }",
    "function gpl { git pull --rebase origin main @args }",
    "function ga { git add @args }",
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
Write-Host "  ga <paths>       git add <paths>" -ForegroundColor DarkGray
Write-Host "  gph              git push origin main" -ForegroundColor DarkGray
Write-Host "  gpl              git pull --rebase origin main" -ForegroundColor DarkGray
Write-Host "  gcm <message>    git commit -m <message>" -ForegroundColor DarkGray
Write-Host "Open a new terminal, or run: . `$PROFILE" -ForegroundColor DarkGray
if ($PROFILE) {
    Write-Host ("This host loads: " + $PROFILE) -ForegroundColor DarkGray
}
$resolvedProfile = if ($PROFILE) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($PROFILE) } else { "" }
if ($resolvedProfile -and ($target -ne $resolvedProfile)) {
    Write-Host "That is not the profile this host loads; start a new terminal before testing." -ForegroundColor Yellow
}
