@echo off
rem Polis — double-click me.
rem
rem Offers four things: watch a session you already ran, map a repository,
rem start an agent with the map watching, or save a picture. A folder dragged
rem onto this file skips the repository menu.

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
