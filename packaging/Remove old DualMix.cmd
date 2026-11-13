@echo off
rem Remove legacy DualMix installations (up to 2.1.0). Keeps settings and virtual cables.
rem Also delete settings and logs:  "Remove old DualMix.cmd" -Purge
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\rimuovi-vecchio.ps1" %*
echo.
pause
