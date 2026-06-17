@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0windows\install-python.ps1" %*
exit /b %ERRORLEVEL%
