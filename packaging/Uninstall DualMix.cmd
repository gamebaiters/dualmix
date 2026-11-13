@echo off
rem Uninstall DualMix (double-click).
rem Also uninstall virtual cable drivers:  "Uninstall DualMix.cmd" -RemoveDrivers
rem Also delete settings and logs:         "Uninstall DualMix.cmd" -Purge
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\uninstall.ps1" %*
echo.
pause
