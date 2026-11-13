@echo off
rem Rimuove il DualMix installato alla vecchia maniera (fino alla 2.1.0). Impostazioni e cavi virtuali restano.
rem Per eliminare anche impostazioni e log:  "Rimuovi vecchio DualMix.cmd" -Purge
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup\rimuovi-vecchio.ps1" %*
echo.
pause
