<#
.SYNOPSIS
Runs the Windows App Certification Kit against a package.

.DESCRIPTION
appcert.exe is the one SDK tool that refuses to run without administrator
rights, so Invoke-Wack in build-windows.ps1 starts this script through
Start-Process -Verb RunAs and keeps the rest of the build unelevated.

Output goes to a log file because the elevated window closes with the process.
The exit code is appcert's own, so the caller can report it.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Appcert,
    [Parameter(Mandatory = $true)][string]$Msix,
    [Parameter(Mandatory = $true)][string]$ReportPath,
    [Parameter(Mandatory = $true)][string]$LogPath
)

$ErrorActionPreference = "Stop"

$exitCode = 1
try {
    & $Appcert reset 2>&1 | Out-File -Path $LogPath -Encoding utf8
    # appcert installs the package, runs the tests, then removes it again. The
    # package must be signed by a certificate this machine trusts.
    & $Appcert test -appxpackagepath $Msix -reportoutputpath $ReportPath 2>&1 |
        Out-File -Path $LogPath -Encoding utf8 -Append
    $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
    "appcert exit code: $exitCode" | Out-File -Path $LogPath -Encoding utf8 -Append
}
catch {
    "appcert failed: $($_.Exception.Message)" | Out-File -Path $LogPath -Encoding utf8 -Append
    $exitCode = 1
}

exit $exitCode
