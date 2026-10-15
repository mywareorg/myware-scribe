@echo off
rem transiel deploy - install per-user, add to PATH, place models.
rem Run from the extracted release folder. Pass /force to redeploy even if current.
setlocal EnableExtensions EnableDelayedExpansion

set "FORCE="
if /i "%~1"=="/force"  set "FORCE=1"
if /i "%~1"=="-force"  set "FORCE=1"
if /i "%~1"=="--force" set "FORCE=1"

set "SRC=%~dp0"
if "%SRC:~-1%"=="\" set "SRC=%SRC:~0,-1%"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\transiel"

if not exist "%SRC%\transiel-gui.exe" (
  echo [!] transiel-gui.exe not found next to deploy.bat.
  echo     Run deploy.bat from the extracted release folder.
  exit /b 1
)

rem --- up-to-date short-circuit -------------------------------------------------
set "SRC_VERSION="
if exist "%SRC%\VERSION" set /p SRC_VERSION=<"%SRC%\VERSION"
if not defined FORCE if defined SRC_VERSION if exist "%INSTALL_DIR%\VERSION" (
  set "INSTALLED_VERSION="
  set /p INSTALLED_VERSION=<"%INSTALL_DIR%\VERSION"
  if "!INSTALLED_VERSION!"=="!SRC_VERSION!" (
    echo transiel !SRC_VERSION! is already deployed at:
    echo   %INSTALL_DIR%
    echo Nothing to do. Use "deploy.bat /force" to overwrite.
    exit /b 0
  )
)

rem --- refuse to overwrite a running copy ---------------------------------------
tasklist /fi "imagename eq transiel-gui.exe" 2>nul | find /i "transiel-gui.exe" >nul
if not errorlevel 1 (
  echo [!] transiel-gui.exe is running. Close it, then re-run deploy.bat.
  exit /b 1
)

echo Deploying transiel to:
echo   %INSTALL_DIR%
if not exist "%INSTALL_DIR%" mkdir "%INSTALL_DIR%" >nul

rem --- program files (silent overwrite); models handled separately --------------
robocopy "%SRC%" "%INSTALL_DIR%" /e /xd models /nfl /ndl /njh /njs /np >nul
if errorlevel 8 (
  echo [!] failed to copy program files to "%INSTALL_DIR%".
  exit /b 1
)

rem --- models: copy only when missing or not matching the packager's marker -----
if exist "%SRC%\models" (
  set "NEED_MODELS=1"
  if exist "%SRC%\models\.complete" if exist "%INSTALL_DIR%\models\.complete" (
    set "A="
    set "B="
    for /f "usebackq delims=" %%v in ("%SRC%\models\.complete") do set "A=%%v"
    for /f "usebackq delims=" %%v in ("%INSTALL_DIR%\models\.complete") do set "B=%%v"
    if "!A!"=="!B!" set "NEED_MODELS=0"
  )
  if "!NEED_MODELS!"=="1" (
    echo Copying models...
    robocopy "%SRC%\models" "%INSTALL_DIR%\models" /e /nfl /ndl /njh /njs /np >nul
    if errorlevel 8 (
      echo [!] failed to copy models to "%INSTALL_DIR%\models".
      exit /b 1
    )
  ) else (
    echo Models already present and complete.
  )
)

rem --- PATH (user), idempotent --------------------------------------------------
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $cur=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $cur) { $cur='' }; $parts=@($cur.Split(';') | Where-Object { $_ -ne '' }); $hit=$parts | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }; if ($hit) { Write-Host 'PATH already contains' $dir } else { $new=(@($parts + $dir) -join ';'); [Environment]::SetEnvironmentVariable('Path',$new,'User'); Write-Host 'Added to PATH:' $dir }"

echo.
echo Done. Open a NEW terminal and run:  transiel-gui
exit /b 0
