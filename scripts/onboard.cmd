@echo off
setlocal EnableExtensions

set "VERSION="
set "FORCE=0"
set "CHECK_ONLY=0"
set "DRY_RUN=0"

:parse
if "%~1"=="" goto after_parse
if /I "%~1"=="--help" goto usage
if /I "%~1"=="-h" goto usage
if /I "%~1"=="--version" (
  if "%~2"=="" (
    echo error: --version requires a value 1>&2
    exit /b 2
  )
  set "VERSION=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--force" (
  set "FORCE=1"
  shift
  goto parse
)
if /I "%~1"=="--check-only" (
  set "CHECK_ONLY=1"
  shift
  goto parse
)
if /I "%~1"=="--dry-run" (
  set "DRY_RUN=1"
  shift
  goto parse
)
echo error: unknown argument: %~1 1>&2
exit /b 2

:after_parse
where sing-box >nul 2>nul
if %errorlevel%==0 (
  if "%FORCE%"=="0" (
    for /f "delims=" %%P in ('where sing-box 2^>nul') do (
      echo sing-box already found: %%P
      exit /b 0
    )
    exit /b 0
  )
)

if "%CHECK_ONLY%"=="1" (
  echo sing-box not found on PATH
  exit /b 1
)

where winget >nul 2>nul
if not %errorlevel%==0 (
  echo error: winget was not found. Install App Installer from Microsoft Store, then rerun this script. 1>&2
  exit /b 1
)

set "CMD=winget install sing-box --accept-package-agreements --accept-source-agreements"
if not "%VERSION%"=="" set "CMD=%CMD% --version %VERSION%"

if "%DRY_RUN%"=="1" (
  echo %CMD%
  exit /b 0
)

echo Installing sing-box with winget...
%CMD%
exit /b %errorlevel%

:usage
echo Usage: scripts\onboard.cmd [--version VERSION] [--force] [--check-only] [--dry-run]
echo.
echo Installs sing-box with: winget install sing-box
echo Skips installation when sing-box is already on PATH unless --force is set.
exit /b 0
