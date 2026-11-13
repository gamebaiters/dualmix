@echo off
rem Installa (o aggiorna) DualMix: doppio clic. Non servono diritti di amministratore.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\setup.ps1" %*
echo.
pause
