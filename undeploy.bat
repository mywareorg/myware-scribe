@echo off
rem Hippo Transcribe undeploy - remove the install and its PATH entry.
rem Pass /keepmodels to keep the downloaded models.
setlocal EnableExtensions EnableDelayedExpansion

set "KEEP="
if /i "%~1"=="/keepmodels" set "KEEP=1"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\hippo-transcribe"
set "MODELS=%LOCALAPPDATA%\hippo-transcribe\models"
set "SHORTCUT=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Hippo Transcribe.lnk"

tasklist /fi "imagename eq hippo-transcribe.exe" 2>nul | find /i "hippo-transcribe.exe" >nul
if not errorlevel 1 (
  echo [!] hippo-transcribe.exe is running. Close it, then re-run undeploy.bat.
  exit /b 1
)

if exist "%INSTALL_DIR%" (
  echo Removing "%INSTALL_DIR%"
  rmdir /s /q "%INSTALL_DIR%"
)
if exist "%SHORTCUT%" (
  echo Removing "%SHORTCUT%"
  del /q "%SHORTCUT%"
)
if not defined KEEP if exist "%MODELS%" (
  echo Removing models "%MODELS%"
  rmdir /s /q "%MODELS%"
)

rem Remove an installation left behind by the pre-rename version.
set "LEGACY_DIR=%LOCALAPPDATA%\Programs\transiel"
set "LEGACY_SHORTCUT=%APPDATA%\Microsoft\Windows\Start Menu\Programs\transiel.lnk"
if exist "%LEGACY_DIR%" (
  echo Removing legacy install "%LEGACY_DIR%"
  rmdir /s /q "%LEGACY_DIR%"
)
if exist "%LEGACY_SHORTCUT%" (
  echo Removing legacy shortcut "%LEGACY_SHORTCUT%"
  del /q "%LEGACY_SHORTCUT%"
)

powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $p) { $p='' }; $parts=@($p.Split(';') | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }); [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User'); Write-Host 'Removed from PATH'"

echo Done. Open a NEW terminal to pick up the PATH change.
exit /b 0
