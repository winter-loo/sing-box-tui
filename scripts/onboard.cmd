@echo off
setlocal EnableExtensions

set "VERSION="
set "REPO=winter-loo/sing-box"
set "SHA256=dcf5be84da3361eadd22efb23df5d5426826ad51b2a7d0c07f90d938da684ec9"
set "INSTALL_DIR=%LOCALAPPDATA%\sing-box-tui\core"
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
if /I "%~1"=="--repo" (
  if "%~2"=="" (
    echo error: --repo requires a value 1>&2
    exit /b 2
  )
  set "REPO=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--sha256" (
  if "%~2"=="" (
    echo error: --sha256 requires a value 1>&2
    exit /b 2
  )
  set "SHA256=%~2"
  shift
  shift
  goto parse
)
if /I "%~1"=="--install-dir" (
  if "%~2"=="" (
    echo error: --install-dir requires a value 1>&2
    exit /b 2
  )
  set "INSTALL_DIR=%~2"
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

where powershell.exe >nul 2>nul
if not %errorlevel%==0 (
  echo error: powershell.exe was not found 1>&2
  exit /b 1
)

if "%VERSION%"=="" set "VERSION=v1.13.13-winterloo.2"
set "CORE_INSTALLER=%~dp0windows\install-sing-box-core.ps1"
if not exist "%CORE_INSTALLER%" set "CORE_INSTALLER=%~dp0scripts\windows\install-sing-box-core.ps1"
set "FORCE_ARG="
set "DRY_RUN_ARG="
if "%FORCE%"=="1" set "FORCE_ARG=-Force"
if "%DRY_RUN%"=="1" set "DRY_RUN_ARG=-DryRun"

if "%DRY_RUN%"=="1" (
  echo powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%CORE_INSTALLER%" -Repo "%REPO%" -Version "%VERSION%" -InstallDir "%INSTALL_DIR%" -Sha256 "%SHA256%" -DryRun
)

echo Installing sing-box from %REPO% %VERSION%...
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%CORE_INSTALLER%" -Repo "%REPO%" -Version "%VERSION%" -InstallDir "%INSTALL_DIR%" -Sha256 "%SHA256%" %FORCE_ARG% %DRY_RUN_ARG%
exit /b %errorlevel%

:usage
echo Usage: scripts\onboard.cmd [--version VERSION] [--repo OWNER/REPO] [--sha256 SHA256] [--install-dir DIR] [--force] [--check-only] [--dry-run]
echo.
echo Installs sing-box from a GitHub release asset.
echo Skips installation when sing-box is already on PATH unless --force is set.
exit /b 0
