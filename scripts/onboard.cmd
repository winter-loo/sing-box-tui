@echo off
where py >nul 2>nul
if %ERRORLEVEL%==0 (
  py -3 "%~dp0onboard.py" %*
  exit /b %ERRORLEVEL%
)
python "%~dp0onboard.py" %*
