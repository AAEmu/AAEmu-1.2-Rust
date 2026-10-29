@echo off
rem Builds the 32-bit 1.2 client oracle (physoracle_x86.exe) and the LoadLibrary smoke test.
rem Requires the untracked runtime made by patch_entry.py:
rem   python patch_entry.py
rem Output: %AAEMU_PHYS_ORACLE_OUT% or D:\aaemu-target\physics\oracle-x86\ (never committed).
setlocal
set SRC=%~dp0
if "%AAEMU_PHYS_ORACLE_OUT%"=="" (set OUT=D:\aaemu-target\physics\oracle-x86) else (set OUT=%AAEMU_PHYS_ORACLE_OUT%)
if not exist "%OUT%" mkdir "%OUT%"
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvarsall.bat" x86 >nul 2>nul
if errorlevel 1 (echo vcvarsall x86 failed & exit /b 1)
pushd "%OUT%"
rem /fp:precise keeps the harness's own float parsing/formatting predictable; the engine's
rem x87 precision is set at run time (--fpu 24|53).
cl /nologo /O2 /fp:precise /W3 /D_CRT_SECURE_NO_WARNINGS /DORACLE_X86 "%SRC%physoracle.c" /Fe:physoracle_x86.exe /link /nologo
if errorlevel 1 (popd & exit /b 1)
cl /nologo /O2 /W3 /D_CRT_SECURE_NO_WARNINGS "%SRC%smoke_x86.c" /Fe:smoke_x86.exe /link /nologo
if errorlevel 1 (popd & exit /b 1)
popd
echo built %OUT%\physoracle_x86.exe and %OUT%\smoke_x86.exe
