@echo off
rem Polis — double-click me.
rem
rem A folder dragged onto this file is mapped directly. Otherwise you get a
rem menu of the git repositories found on this machine.

setlocal
set "PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%PS%" set "PS=powershell.exe"

if not "%~1"=="" (
  "%PS%" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\polis-launcher.ps1" -Repo "%~1"
) else (
  "%PS%" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\polis-launcher.ps1"
)

if errorlevel 1 (
  echo.
  echo   Polis exited with an error. If the window closed too fast, open
  echo   PowerShell in this folder and run:
  echo.
  echo     .\scripts\polis-launcher.ps1
  echo.
  pause
)
endlocal
