@echo off
setlocal enabledelayedexpansion
title Polis - map a git repository as a city

set "EXE=%~dp0target\release\polis.exe"
if not exist "%EXE%" (
  echo.
  echo   polis.exe not found at:
  echo     %EXE%
  echo.
  echo   Build it first, from %~dp0 :
  echo     cargo build --release
  echo.
  pause
  exit /b 1
)

rem A folder dragged onto this file arrives as %1. Otherwise ask, defaulting to this repo.
set "REPO=%~1"
if "%REPO%"=="" (
  echo.
  echo   Polis renders a git repository as a city seen from above.
  echo.
  echo   Drag a git repo folder onto this file, or type a path below.
  echo   Press Enter on its own to map Polis itself.
  echo.
  set /p "REPO=  Repo path: "
)
if "%REPO%"=="" set "REPO=%~dp0."

if not exist "%REPO%\.git" (
  echo.
  echo   Not a git repository ^(no .git folder^):
  echo     %REPO%
  echo.
  echo   Polis builds the city from git history, so it needs a repo with commits.
  echo.
  pause
  exit /b 1
)

for %%I in ("%REPO%") do set "NAME=%%~nxI"
if "%NAME%"=="." set "NAME=polis"
set "OUTDIR=%USERPROFILE%\Desktop"
set "CITY=%OUTDIR%\polis-%NAME%.png"
set "JUNC=%OUTDIR%\polis-%NAME%-junctions.png"

echo.
echo   Mapping %REPO%
echo.
"%EXE%" -C "%REPO%" snapshot --out "%CITY%" --junctions "%JUNC%"
if errorlevel 1 (
  echo.
  echo   Snapshot failed. The output above says why.
  echo.
  pause
  exit /b 1
)

echo.
echo   Wrote:
echo     %CITY%
echo     %JUNC%
echo.
echo   Opening the city plan. The second image is the road graph alone,
echo   with junctions coloured by how many streets meet there.
echo.
start "" "%CITY%"
pause
