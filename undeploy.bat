@echo off
rem transiel undeploy - remove the install and its PATH entry.
rem Pass /keepmodels to keep the downloaded models.
setlocal EnableExtensions EnableDelayedExpansion

set "KEEP="
if /i "%~1"=="/keepmodels" set "KEEP=1"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\transiel"
set "MODELS=%LOCALAPPDATA%\transiel\models"

tasklist /fi "imagename eq transiel.exe" 2>nul | find /i "transiel.exe" >nul
if not errorlevel 1 (
  echo [!] transiel.exe is running. Close it, then re-run undeploy.bat.
  exit /b 1
)

if exist "%INSTALL_DIR%" (
  echo Removing "%INSTALL_DIR%"
  rmdir /s /q "%INSTALL_DIR%"
)
if not defined KEEP if exist "%MODELS%" (
  echo Removing models "%MODELS%"
  rmdir /s /q "%MODELS%"
)

powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $p) { $p='' }; $parts=@($p.Split(';') | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }); [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User'); Write-Host 'Removed from PATH'"

echo Done. Open a NEW terminal to pick up the PATH change.
exit /b 0
