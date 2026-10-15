@echo off
rem transiel undeploy - remove the per-user install and its PATH entry.
rem Pass /keepmodels to remove the program but leave models/ in place.
setlocal EnableExtensions EnableDelayedExpansion

set "KEEP_MODELS="
if /i "%~1"=="/keepmodels" set "KEEP_MODELS=1"

set "INSTALL_DIR=%LOCALAPPDATA%\Programs\transiel"

tasklist /fi "imagename eq transiel-gui.exe" 2>nul | find /i "transiel-gui.exe" >nul
if not errorlevel 1 (
  echo [!] transiel-gui.exe is running. Close it, then re-run undeploy.bat.
  exit /b 1
)

if not exist "%INSTALL_DIR%" (
  echo transiel is not installed at "%INSTALL_DIR%".
) else if defined KEEP_MODELS (
  echo Removing program files, keeping models...
  for %%d in (scripts runtime) do (
    if exist "%INSTALL_DIR%\%%d" rmdir /s /q "%INSTALL_DIR%\%%d"
  )
  if exist "%INSTALL_DIR%\transiel-gui.exe" del /q "%INSTALL_DIR%\transiel-gui.exe"
  if exist "%INSTALL_DIR%\VERSION" del /q "%INSTALL_DIR%\VERSION"
  if exist "%INSTALL_DIR%\deploy.bat" del /q "%INSTALL_DIR%\deploy.bat"
  if exist "%INSTALL_DIR%\undeploy.bat" del /q "%INSTALL_DIR%\undeploy.bat"
) else (
  echo Removing "%INSTALL_DIR%"...
  rmdir /s /q "%INSTALL_DIR%"
)

rem --- drop the install dir from the user PATH (idempotent) ---------------------
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $cur=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $cur) { $cur='' }; $parts=@($cur.Split(';') | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }); [Environment]::SetEnvironmentVariable('Path',($parts -join ';'),'User'); Write-Host 'Removed from PATH:' $dir"

echo.
echo Done. Open a NEW terminal to pick up the PATH change.
exit /b 0
