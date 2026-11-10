@echo off
setlocal
if exist "%~dp0AAEmu Launcher.exe" (
  start "" "%~dp0AAEmu Launcher.exe"
  exit /b
)
powershell.exe -NoProfile -ExecutionPolicy Bypass -STA -File "%~dp0Launcher\Start-Client.ps1"
if errorlevel 1 pause
