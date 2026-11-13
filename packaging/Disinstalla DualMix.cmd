@echo off
rem Disinstalla DualMix (doppio clic).
rem Per rimuovere anche i driver dei cavi virtuali:  "Disinstalla DualMix.cmd" -RemoveDrivers
rem Per eliminare anche impostazioni e log:          "Disinstalla DualMix.cmd" -Purge
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\uninstall.ps1" %*
echo.
pause
