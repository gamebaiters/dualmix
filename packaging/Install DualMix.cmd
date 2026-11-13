@echo off
rem Install or update DualMix (double-click). No administrator privileges required.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\setup.ps1" %*
echo.
pause
