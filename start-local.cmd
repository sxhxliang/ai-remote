@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\start-local.ps1" %*
if errorlevel 1 (
  echo.
  echo Startup failed. Read the error above or check .local\launcher.error.log.
  pause
  exit /b 1
)
echo.
echo You may close this window. The local services keep running.
pause
