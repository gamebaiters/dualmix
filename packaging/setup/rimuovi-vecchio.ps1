<#
  DualMix - removes the program installed the old way (built on the PC and installed with install.ps1, up to 2.1.0)

  It can be used on its own (double click "Rimuovi vecchio DualMix.cmd") and is run by the setup the first time.

  What it removes:
    - the running DualMix: it is asked to close, so that it gives every app its volume and output device back and
      restores the Windows default devices; only if it does not answer it is ended;
    - the sign-in task "DualMix" of the Task Scheduler and the older start-up entry in the registry (Run);
    - the Start menu / desktop / start-up shortcuts;
    - the Windows "Installed apps" entry;
    - the program folder %LOCALAPPDATA%\Programs\DualMix.

  What it leaves alone:
    - settings, logs and the saved driver pack in %APPDATA%\DualMix  (delete them with -Purge);
    - the virtual cables "DualMix Game" / "DualMix Chat" and their drivers: the new DualMix uses the same ones.
      To take them away as well, use the uninstaller of the new version ("Disinstalla DualMix.cmd -RemoveDrivers");
    - the source code, wherever it is (for example Desktop\mixer).

  Usage:  powershell -ExecutionPolicy Bypass -File rimuovi-vecchio.ps1 [-Purge] [-Silent] [-Pause]
    -Purge    also deletes settings and logs (%APPDATA%\DualMix)
    -Silent   prints only problems
    -Pause    waits for a key at the end
#>
param([switch]$Purge, [switch]$Silent, [switch]$Pause)

$ErrorActionPreference = 'Continue'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\DualMix'
Set-Location $env:TEMP   # never keep the folder being deleted as the current directory

function Say([string]$text) { if (-not $Silent) { Write-Host $text } }

function Get-DualMixProcesses {
    Get-CimInstance Win32_Process | Where-Object {
        $_.Name -eq 'DualMix.exe' -or (($_.Name -eq 'dotnet.exe' -or $_.Name -eq 'conhost.exe') -and $_.CommandLine -like '*DualMix.dll*')
    }
}

# 1. close DualMix cleanly (the same signal as "DualMix.dll --exit")
if (Get-DualMixProcesses) {
    Say 'Chiudo DualMix...'
    try {
        $signal = [System.Threading.EventWaitHandle]::OpenExisting('Local\DualMix.Exit.7f3c2a')
        [void]$signal.Set()
        $signal.Dispose()
    } catch { }
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-DualMixProcesses) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
    Get-DualMixProcesses | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 500
}

# 2. start with Windows: the task, and the registry entry of the very first versions
Unregister-ScheduledTask -TaskName 'DualMix' -Confirm:$false -ErrorAction SilentlyContinue
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'DualMix' -ErrorAction SilentlyContinue

# 3. shortcuts and the "Installed apps" entry
foreach ($folder in 'Programs', 'Startup', 'DesktopDirectory') {
    Remove-Item (Join-Path ([Environment]::GetFolderPath($folder)) 'DualMix.lnk') -ErrorAction SilentlyContinue
}
Remove-Item 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DualMix' -Recurse -ErrorAction SilentlyContinue

# 4. program files (also what an interrupted update may have left)
foreach ($folder in @($dest, "$dest.old")) {
    for ($i = 0; (Test-Path $folder) -and $i -lt 10; $i++) {
        Remove-Item $folder -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $folder) { Start-Sleep -Milliseconds 500 }
    }
}
Remove-Item (Join-Path $env:TEMP 'DualMix-update') -Recurse -Force -ErrorAction SilentlyContinue

if ($Purge) { Remove-Item (Join-Path $env:APPDATA 'DualMix') -Recurse -Force -ErrorAction SilentlyContinue }

$left = @()
if (Test-Path $dest) { $left += $dest }
if (Get-DualMixProcesses) { $left += 'un processo di DualMix ancora attivo' }
if (Get-ScheduledTask -TaskName 'DualMix' -ErrorAction SilentlyContinue) { $left += 'attivita'' pianificata DualMix' }
if ($left) { Write-Host ('Attenzione, non rimosso: ' + ($left -join ', ')) }
else { Say 'Vecchio DualMix rimosso.' }
if (-not $Purge) { Say "Impostazioni e log conservati in $(Join-Path $env:APPDATA 'DualMix'); i cavi virtuali restano installati." }
if ($Pause) { Write-Host ''; Read-Host 'Premi Invio per chiudere' | Out-Null }
if ($left) { exit 1 } else { exit 0 }
