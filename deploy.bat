@echo off
rem transiel deploy - install the exe, add it to PATH, download models.
rem Model downloading is delegated to the executable so there is one source of
rem truth for the model list (and progress is shown in this window).
setlocal EnableExtensions EnableDelayedExpansion

set "SRC=%~dp0"
if "%SRC:~-1%"=="\" set "SRC=%SRC:~0,-1%"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\transiel"

if not exist "%SRC%\transiel.exe" (
  echo [!] transiel.exe not found next to deploy.bat.
  echo     Run deploy.bat from the extracted release folder.
  exit /b 1
)

tasklist /fi "imagename eq transiel.exe" 2>nul | find /i "transiel.exe" >nul
if not errorlevel 1 (
  echo [!] transiel.exe is running. Close it, then re-run deploy.bat.
  exit /b 1
)

echo Installing transiel to "%INSTALL_DIR%"
if exist "%INSTALL_DIR%" rmdir /s /q "%INSTALL_DIR%"
mkdir "%INSTALL_DIR%" >nul
copy /y "%SRC%\transiel.exe" "%INSTALL_DIR%\transiel.exe" >nul

echo Adding "%INSTALL_DIR%" to the user PATH
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $p) { $p='' }; $parts=@($p.Split(';') | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }); [Environment]::SetEnvironmentVariable('Path', (@($parts + $dir) -join ';'), 'User'); Write-Host 'PATH updated'"

echo Downloading models...
"%INSTALL_DIR%\transiel.exe" --cli models download --verify
if errorlevel 1 (
  echo [!] model download failed.
  exit /b 1
)

echo.
echo Done. Open a NEW terminal and run:  transiel
exit /b 0
