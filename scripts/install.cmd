@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set "EC=%ERRORLEVEL%"
echo.
if "%EC%"=="0" (
  echo FocusStage installation completed.
) else (
  echo FocusStage installation failed with exit code %EC%.
)
pause
exit /b %EC%
