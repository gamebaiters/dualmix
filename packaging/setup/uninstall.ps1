<#
  DualMix - uninstall

  Removes DualMix completely:
    1. closes it (which gives every app back its volume and its output device, and restores the Windows
       default devices);
    2. gives the virtual cables their original names back ("CABLE-A Input", "CABLE-B Input"), or removes
       their drivers with -RemoveDrivers  (needs administrator rights: Windows asks once);
    3. removes the sign-in task, the Start menu shortcut, the "Installed apps" entry and the program files.

  Usage:  powershell -ExecutionPolicy Bypass -File uninstall.ps1 [-RemoveDrivers] [-Purge] [-RestoreDucking] [-Pause]
    -RemoveDrivers   also uninstalls the VB-Audio virtual cable drivers
    -Purge           also deletes settings, logs and the saved driver pack (%APPDATA%\DualMix)
    -RestoreDucking  puts back the Windows default "reduce other sounds by 80% during calls"
    -Pause           waits for a key at the end (used when started from Windows "Installed apps")

  You can also double-click "Disinstalla DualMix.cmd", or use Settings > Apps > Installed apps > DualMix.
#>
param([switch]$RemoveDrivers, [switch]$Purge, [switch]$RestoreDucking, [switch]$Pause)

$ErrorActionPreference = 'Continue'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\DualMix'
$dll = Join-Path $dest 'DualMix.dll'
Set-Location $env:TEMP   # never keep the folder being deleted as the current directory

function Find-DesktopHost {
    foreach ($dir in @((Join-Path $env:ProgramFiles 'dotnet'), (Join-Path $env:USERPROFILE '.dotnet'))) {
        $exe = Join-Path $dir 'dotnet.exe'
        $desktop = Join-Path $dir 'shared\Microsoft.WindowsDesktop.App'
        if ((Test-Path $exe) -and (Test-Path $desktop) -and (Get-ChildItem $desktop -Directory -Filter '10.*')) { return $exe }
    }
    return $null
}
function Get-DualMixProcesses {
    Get-CimInstance Win32_Process | Where-Object {
        $_.Name -eq 'DualMix.exe' -or (($_.Name -eq 'dotnet.exe' -or $_.Name -eq 'conhost.exe') -and $_.CommandLine -like '*DualMix.dll*')
    }
}
$hostExe = Find-DesktopHost

# 1. close DualMix (clean exit: volumes, per-app outputs and default devices are restored by DualMix itself)
if (Get-DualMixProcesses) {
    Write-Host 'Chiudo DualMix...'
    if ($hostExe -and (Test-Path $dll)) { Start-Process $hostExe -ArgumentList "`"$dll`" --exit" -Wait -WindowStyle Hidden }
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-DualMixProcesses) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
    Get-DualMixProcesses | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# 2. virtual cables: factory names back, or drivers removed (elevated helper inside DualMix)
if ($hostExe -and (Test-Path $dll)) {
    $result = Join-Path $env:TEMP 'dualmix-uninstall.json'
    Remove-Item $result -ErrorAction SilentlyContinue
    $what = if ($RemoveDrivers) { '--remove-drivers' } else { '--unconfigure' }
    Write-Host 'Ripristino i cavi virtuali (Windows chiede l''autorizzazione di amministratore)...'
    try {
        Start-Process $hostExe -ArgumentList "`"$dll`" --setup `"$result`" $what" -Verb RunAs -Wait -WindowStyle Hidden
        if (Test-Path $result) { Write-Host ((Get-Content $result -Raw | ConvertFrom-Json).Message); Remove-Item $result -Force }
    } catch {
        Write-Host 'Autorizzazione negata: i cavi virtuali restano installati con i nomi "DualMix Game" / "DualMix Chat".'
    }
}

# 3. start-up, shortcut, "Installed apps" entry, files
Unregister-ScheduledTask -TaskName 'DualMix' -Confirm:$false -ErrorAction SilentlyContinue
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'DualMix' -ErrorAction SilentlyContinue
Remove-Item (Join-Path ([Environment]::GetFolderPath('Programs')) 'DualMix.lnk') -ErrorAction SilentlyContinue
Remove-Item 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DualMix' -Recurse -ErrorAction SilentlyContinue
Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item "$dest.old", "$dest.new", (Join-Path $env:TEMP 'DualMix-update') -Recurse -Force -ErrorAction SilentlyContinue

if ($RestoreDucking) {
    Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Multimedia\Audio' -Name 'UserDuckingPreference' -Value 1 -Type DWord
}
if ($Purge) {
    Remove-Item (Join-Path $env:APPDATA 'DualMix') -Recurse -Force -ErrorAction SilentlyContinue
}

$left = @()
if (Test-Path $dest) { $left += $dest }
if (Get-ScheduledTask -TaskName 'DualMix' -ErrorAction SilentlyContinue) { $left += 'attivita'' pianificata DualMix' }
if ($left) { Write-Host ('Attenzione, non rimosso: ' + ($left -join ', ')) }
else { Write-Host 'DualMix disinstallato.' }
if (-not $Purge) { Write-Host "Impostazioni e log conservati in $(Join-Path $env:APPDATA 'DualMix') (usa -Purge per eliminarli)." }
if ($Pause) { Write-Host ''; Read-Host 'Premi Invio per chiudere' | Out-Null }
