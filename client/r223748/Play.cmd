@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -STA -File "%~dp0Launcher\Start-Client.ps1"
if errorlevel 1 pause
