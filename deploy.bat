@echo off
rem MyWare Scribe deploy - install the exe, add it to PATH, download models.
rem Model downloading is delegated to the executable so there is one source of
rem truth for the model list (and progress is shown in this window).
setlocal EnableExtensions EnableDelayedExpansion

echo MyWare Scribe - private, offline transcription
echo Audio and transcripts stay on this device.
echo.

set "SRC=%~dp0"
if "%SRC:~-1%"=="\" set "SRC=%SRC:~0,-1%"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\myware-scribe"

if not exist "%SRC%\myware-scribe.exe" (
  echo [!] myware-scribe.exe not found next to deploy.bat.
  echo     Run deploy.bat from the extracted release folder.
  exit /b 1
)

if not exist "%SRC%\myware-summarize.exe" (
  echo [!] myware-summarize.exe is missing from the release folder.
  exit /b 1
)

if not exist "%SRC%\sherpa-onnx-c-api.dll" (
  echo [!] sherpa-onnx-c-api.dll is missing from the release folder.
  exit /b 1
)

if not exist "%SRC%\onnxruntime.dll" (
  echo [!] onnxruntime.dll is missing from the release folder.
  exit /b 1
)

rem Files extracted from a downloaded zip may carry a Mark-of-the-Web that
rem blocks loading the DLLs. Clear it for the release files.
powershell -NoProfile -ExecutionPolicy Bypass -Command "Get-ChildItem -LiteralPath '%SRC%' -File | Unblock-File -ErrorAction SilentlyContinue"

tasklist /fi "imagename eq myware-scribe.exe" 2>nul | find /i "myware-scribe.exe" >nul
if not errorlevel 1 (
  echo [!] myware-scribe.exe is running. Close it, then re-run deploy.bat.
  exit /b 1
)

echo Installing MyWare Scribe to "%INSTALL_DIR%"
if exist "%INSTALL_DIR%" rmdir /s /q "%INSTALL_DIR%"
mkdir "%INSTALL_DIR%" >nul
copy /y "%SRC%\myware-scribe.exe" "%INSTALL_DIR%\myware-scribe.exe" >nul
copy /y "%SRC%\myware-summarize.exe" "%INSTALL_DIR%\myware-summarize.exe" >nul
rem Copy every DLL: sherpa-onnx, ONNX Runtime, and the Visual C++ runtime.
for %%f in ("%SRC%\*.dll") do copy /y "%%~f" "%INSTALL_DIR%\" >nul
if exist "%SRC%\sample-short.mp3" (
  copy /y "%SRC%\sample-short.mp3" "%INSTALL_DIR%\sample-short.mp3" >nul
) else if exist "%SRC%\audio\sample-short.mp3" (
  copy /y "%SRC%\audio\sample-short.mp3" "%INSTALL_DIR%\sample-short.mp3" >nul
)
if exist "%SRC%\models" (
  xcopy /e /i /y /q "%SRC%\models" "%INSTALL_DIR%\models\" >nul
)

rem Explorer and the Start menu show the shortcut name, so users see
rem "MyWare Scribe" instead of "myware-scribe.exe". The target keeps its .exe
rem extension because Windows needs it to launch the program.
set "START_MENU=%APPDATA%\Microsoft\Windows\Start Menu\Programs"
if not exist "%START_MENU%" mkdir "%START_MENU%" >nul 2>&1
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('%START_MENU%\MyWare Scribe.lnk'); $s.TargetPath='%INSTALL_DIR%\myware-scribe.exe'; $s.WorkingDirectory='%INSTALL_DIR%'; $s.IconLocation='%INSTALL_DIR%\myware-scribe.exe,0'; $s.Description='MyWare Scribe - Private, offline transcription'; $s.Save()" >nul 2>&1
if errorlevel 1 (
  echo [!] Could not create the Start Menu shortcut.
) else (
  echo Added a Start Menu shortcut named "MyWare Scribe"
)

echo Adding "%INSTALL_DIR%" to the user PATH
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir='%INSTALL_DIR%'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if ($null -eq $p) { $p='' }; $parts=@($p.Split(';') | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }); [Environment]::SetEnvironmentVariable('Path', (@($parts + $dir) -join ';'), 'User'); Write-Host 'PATH updated'"

echo Checking bundled models...
"%INSTALL_DIR%\myware-scribe.exe" --cli models
if errorlevel 1 (
  echo [!] Could not inspect bundled models. The app can download them on first launch.
)

echo.
echo Done. Open a NEW terminal and run:  myware-scribe
exit /b 0
