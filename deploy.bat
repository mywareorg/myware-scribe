@echo off
rem transiel deploy - install the exe, add it to PATH, download models.
rem Uses the built-in curl.exe so download progress shows in this window.
setlocal EnableExtensions EnableDelayedExpansion

set "SRC=%~dp0"
if "%SRC:~-1%"=="\" set "SRC=%SRC:~0,-1%"
set "INSTALL_DIR=%LOCALAPPDATA%\Programs\transiel"
set "MODELS=%LOCALAPPDATA%\transiel\models"

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

echo Downloading models to "%MODELS%"
if not exist "%MODELS%" mkdir "%MODELS%" >nul
call :get "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin" "%MODELS%\ggml-base.en-q5_1.bin"
call :get "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin" "%MODELS%\ggml-large-v3-turbo-q5_0.bin"
call :get "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx" "%MODELS%\wespeaker_en_voxceleb_resnet34_LM.onnx"
if not exist "%MODELS%\sherpa-onnx-pyannote-segmentation-3-0\model.onnx" (
  call :get "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2" "%MODELS%\seg3.tar.bz2"
  echo Extracting segmentation model...
  tar -xjf "%MODELS%\seg3.tar.bz2" -C "%MODELS%"
  del "%MODELS%\seg3.tar.bz2"
)

echo.
echo Done. Open a NEW terminal and run:  transiel
exit /b 0

:get
if exist "%~2" (
  echo Already have %~nx2
  exit /b 0
)
echo Downloading %~nx2 ...
curl.exe -L --fail --progress-bar -o "%~2" "%~1"
if errorlevel 1 (
  echo [!] download failed: %~1
  exit /b 1
)
exit /b 0
