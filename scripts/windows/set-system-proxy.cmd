@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0set-system-proxy.ps1" %*
