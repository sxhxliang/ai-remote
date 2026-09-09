@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\stop-local.ps1"
set "stopResult=%ERRORLEVEL%"
pause
exit /b %stopResult%
