<#
  DualMix - setup (installs or updates DualMix from a ready-made package; no administrator rights needed)

  A package is the folder this script comes in:
      setup\setup.ps1            this script
      setup\uninstall.ps1        copied next to the program, used by Windows "Installed apps"
      setup\rimuovi-vecchio.ps1  removes a DualMix installed the old way (built on the PC with install.ps1)
      app\                       the program

  What it does:
    1. makes sure the .NET 10 Desktop runtime is there (downloads it from Microsoft for this user if it is not);
    2. closes a running DualMix cleanly (it gives every app its volume and output device back);
    3. the first time, removes the DualMix that was installed the old way (settings and virtual cables are kept);
    4. puts the new program in %LOCALAPPDATA%\Programs\DualMix (the old folder is swapped out only when the new
       one is complete, and put back if anything fails);
    5. creates the Start menu shortcut and the Windows "Installed apps" entry, and starts DualMix.

  How it is started:
    - "Installa DualMix.cmd" (double click) or DualMix-Setup-x.y.z.exe;
    - by DualMix itself for an automatic update:  setup.ps1 -Update   (no window, DualMix comes back in the tray).

  Usage:  powershell -ExecutionPolicy Bypass -File setup\setup.ps1 [-Update] [-Silent] [-NoStart] [-Source <folder>]
    -Update    update started by DualMix: nothing is asked, DualMix restarts hidden, and the running version is
               started again if the update fails
    -Silent    start DualMix hidden in the notification area instead of opening its window
    -NoStart   do not start DualMix at the end
    -Source    folder with the program (default: ..\app)

  Everything it does is also written to %APPDATA%\DualMix\setup.log.
#>
param([string]$Source, [switch]$Update, [switch]$Silent, [switch]$NoStart)

$ErrorActionPreference = 'Stop'

# A 32-bit PowerShell (started by a 32-bit installer) sees another "Program Files": carry on in the 64-bit one.
if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) {
    $native = Join-Path $env:WINDIR 'sysnative\WindowsPowerShell\v1.0\powershell.exe'
    $forward = @()
    if ($Source) { $forward += '-Source'; $forward += $Source }
    if ($Update) { $forward += '-Update' }
    if ($Silent) { $forward += '-Silent' }
    if ($NoStart) { $forward += '-NoStart' }
    & $native -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath @forward
    exit $LASTEXITCODE
}

$here = $PSScriptRoot
if (-not $Source) { $Source = Join-Path (Split-Path $here -Parent) 'app' }
$dest = Join-Path $env:LOCALAPPDATA 'Programs\DualMix'
$new = "$dest.new"
$old = "$dest.old"
$data = Join-Path $env:APPDATA 'DualMix'
$userDotnet = Join-Path $env:USERPROFILE '.dotnet'
$conhost = Join-Path $env:SystemRoot 'System32\conhost.exe'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DualMix'
$logFile = Join-Path $data 'setup.log'
New-Item -ItemType Directory -Force $data | Out-Null
Set-Location $env:TEMP   # never keep a folder that is about to be renamed as the current directory

$isIt = [System.Globalization.CultureInfo]::CurrentUICulture.TwoLetterISOLanguageName -eq 'it'
function T([string]$en, [string]$it) { if ($isIt) { $it } else { $en } }

function Say([string]$text) {
    Write-Host $text
    try { Add-Content -Path $logFile -Value ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $text) -Encoding UTF8 } catch { }
}

function Find-DesktopHost {
    foreach ($dir in @((Join-Path $env:ProgramFiles 'dotnet'), $userDotnet)) {
        $exe = Join-Path $dir 'dotnet.exe'
        $core = Join-Path $dir 'shared\Microsoft.NETCore.App'
        $desktop = Join-Path $dir 'shared\Microsoft.WindowsDesktop.App'
        if ((Test-Path $exe) -and (Test-Path $core) -and (Test-Path $desktop) -and
            (Get-ChildItem $core -Directory -Filter '10.*') -and (Get-ChildItem $desktop -Directory -Filter '10.*')) { return $exe }
    }
    return $null
}

# Microsoft's own installation script for .NET, from either of its two official addresses, with a few attempts.
function Get-InstallScript([string]$target) {
    $last = $null
    foreach ($try in 1..3) {
        foreach ($url in 'https://dot.net/v1/dotnet-install.ps1', 'https://builds.dotnet.microsoft.com/dotnet/scripts/v1/dotnet-install.ps1') {
            try {
                Invoke-WebRequest $url -OutFile $target -UseBasicParsing
                if ((Get-Item $target).Length -gt 10000) { return }
            } catch { $last = $_ }
        }
        Start-Sleep -Seconds 2
    }
    throw (T "Could not download the .NET installer from Microsoft: $($last.Exception.Message)" "Impossibile scaricare l'installazione di .NET da Microsoft: $($last.Exception.Message)")
}

function Get-DualMixProcesses {
    Get-CimInstance Win32_Process | Where-Object {
        $_.Name -eq 'DualMix.exe' -or (($_.Name -eq 'dotnet.exe' -or $_.Name -eq 'conhost.exe') -and $_.CommandLine -like '*DualMix.dll*')
    }
}

# Asks the running DualMix to quit (the same signal as "DualMix.dll --exit"), waits, then insists.
function Stop-DualMix {
    if (-not (Get-DualMixProcesses)) { return }
    Say (T 'Closing running DualMix...' 'Chiudo DualMix in esecuzione...')
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

function Start-DualMix([string]$hostExe, [bool]$isNative, [bool]$hidden) {
    if ($isNative) {
        $exe = Join-Path $dest 'DualMix.exe'
        $args = @('--supervise')
        if ($hidden) { $args += '--tray' }
        Start-Process $exe -ArgumentList $args -WorkingDirectory $dest
        return
    }
    $arguments = "--headless `"$hostExe`" `"$(Join-Path $dest 'DualMix.dll')`" --supervise"
    if ($hidden) { $arguments += ' --tray' }
    Start-Process $conhost -ArgumentList $arguments -WorkingDirectory $dest
}

function Move-Folder([string]$from, [string]$to) {
    # A file can stay locked for a moment after its process ended (antivirus, indexer).
    for ($i = 0; ; $i++) {
        try { Rename-Item -LiteralPath $from -NewName (Split-Path $to -Leaf); return }
        catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 500 }
    }
}

$hadOld = $false
$isNative = $false
try {
    $srcExe = Join-Path $Source 'DualMix.exe'
    $srcDll = Join-Path $Source 'DualMix.dll'
    if ((Test-Path $srcExe) -and -not (Test-Path $srcDll)) {
        $isNative = $true
        $vi = (Get-Item $srcExe).VersionInfo
        $version = (($vi.ProductVersion, $vi.FileVersion | Where-Object { $_ } | Select-Object -First 1) -replace '\+.*$', '')
        if (-not $version) { $version = '2.4.0' }
    } elseif (Test-Path $srcDll) {
        $version = (Get-Item $srcDll).VersionInfo.ProductVersion -replace '\+.*$', ''
    } else {
        throw (T "Incomplete package: missing $Source\DualMix.exe" "Pacchetto incompleto: manca $Source\DualMix.exe")
    }
    $updateNote = if ($Update) { T ' (automatic update)' ' (aggiornamento automatico)' } else { '' }
    Say "=== $(T "DualMix $version Setup" "Setup di DualMix $version")$updateNote ==="

    # 1. Runtime check (skipped for standalone native Rust DualMix.exe)
    $hostExe = $null
    if ($isNative) {
        Say (T 'Native executable: no .NET runtime download required.' 'Eseguibile nativo: nessun runtime .NET da scaricare.')
    } else {
        $hostExe = Find-DesktopHost
        if (-not $hostExe) {
            Say (T '.NET 10 Desktop runtime not found: downloading from Microsoft into %USERPROFILE%\.dotnet (current user only)...' 'Runtime .NET 10 Desktop non trovato: lo scarico da Microsoft in %USERPROFILE%\.dotnet (solo per questo utente)...')
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            $script = Join-Path $env:TEMP 'dotnet-install.ps1'
            Get-InstallScript $script
            # a connection that drops half way is tried again: the script resumes with what is missing
            for ($try = 1; $try -le 4 -and -not $hostExe; $try++) {
                try {
                    & $script -Channel 10.0 -Runtime dotnet -InstallDir $userDotnet -NoPath
                    & $script -Channel 10.0 -Runtime windowsdesktop -InstallDir $userDotnet -NoPath
                } catch { Say (T "Attempt $try failed: $($_.Exception.Message)" "Tentativo $try non riuscito: $($_.Exception.Message)"); Start-Sleep -Seconds 3 }
                $hostExe = Find-DesktopHost
            }
            if (-not $hostExe) { throw (T 'Could not install the .NET 10 Desktop runtime (internet access required). Install it from https://dotnet.microsoft.com/download/dotnet/10.0 and try again.' 'Impossibile installare il runtime .NET 10 Desktop (serve internet). Installalo da https://dotnet.microsoft.com/download/dotnet/10.0 e riprova.') }
        }
        Say "$(T '.NET runtime' 'Runtime .NET'): $hostExe"
    }

    # 2. the new program, complete, next to the old one
    foreach ($leftover in @($new, $old)) { if (Test-Path $leftover) { Remove-Item $leftover -Recurse -Force } }
    Copy-Item -LiteralPath $Source -Destination $new -Recurse
    Copy-Item (Join-Path $here 'uninstall.ps1') (Join-Path $new 'uninstall.ps1') -Force
    $copiedOk = if ($isNative) { Test-Path (Join-Path $new 'DualMix.exe') } else { Test-Path (Join-Path $new 'DualMix.dll') }
    if (-not $copiedOk) { throw (T 'Failed to copy application files.' 'Copia del programma non riuscita.') }

    # 3. close DualMix; clear away any legacy/previous installation cleanly
    $previous = (Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue).DisplayVersion
    $oldWay = -not $Update -and ((Test-Path $dest) -or $previous) -and -not (Test-Path (Join-Path $dest 'setup-installed.txt'))
    if ($oldWay) {
        $prevTxt = if ($previous) { " ($(T 'version' 'versione') $previous)" } else { '' }
        Say (T "Found legacy DualMix installation${prevTxt}: uninstalling it (settings and virtual cables will be kept)." "Trovato un DualMix installato alla vecchia maniera${prevTxt}: lo disinstallo (impostazioni e cavi virtuali restano).")
        & (Join-Path $here 'rimuovi-vecchio.ps1') -Silent
    } elseif ($previous -or (Test-Path $dest)) {
        $prevTxt = if ($previous) { " ($(T 'version' 'versione') $previous)" } else { '' }
        Say (T "Found previous DualMix installation${prevTxt}: replacing it cleanly (settings and virtual cables will be kept)." "Trovata una precedente installazione di DualMix${prevTxt}: la sostituisco in modo pulito (impostazioni e cavi virtuali restano).")
    }
    Stop-DualMix

    # 4. swap the folders (removes old .NET DLLs/files completely)
    if (Test-Path $dest) { Move-Folder $dest $old; $hadOld = $true }
    Move-Folder $new $dest
    Set-Content -Path (Join-Path $dest 'setup-installed.txt') -Value "DualMix $version, installed by setup on $(Get-Date -Format 'yyyy-MM-dd HH:mm')" -Encoding ASCII
    if (Test-Path $old) { Remove-Item $old -Recurse -Force -ErrorAction SilentlyContinue }
    $hadOld = $false
    Say (T "Installed to $dest" "Programma installato in $dest")

    # 5. Start menu shortcut & legacy startup/task cleanup
    foreach ($legacyFolder in 'Startup', 'DesktopDirectory') {
        $legacyLnk = Join-Path ([Environment]::GetFolderPath($legacyFolder)) 'DualMix.lnk'
        if (Test-Path $legacyLnk) { Remove-Item $legacyLnk -ErrorAction SilentlyContinue }
    }
    if ($isNative) {
        $existingTask = Get-ScheduledTask -TaskName 'DualMix' -ErrorAction SilentlyContinue
        if ($existingTask) {
            $taskCmd = ($existingTask.Actions | Select-Object -First 1).Execute
            $taskArgs = ($existingTask.Actions | Select-Object -First 1).Arguments
            if (($taskCmd -like '*dotnet*' -or $taskArgs -like '*DualMix.dll*' -or $taskCmd -like '*conhost*')) {
                $act = New-ScheduledTaskAction -Execute (Join-Path $dest 'DualMix.exe') -Argument '--supervise --tray' -WorkingDirectory $dest
                Set-ScheduledTask -TaskName 'DualMix' -Action $act | Out-Null
                Say (T 'Updated Windows autostart task to native DualMix.exe.' 'Attivita'' di avvio automatico aggiornata al nuovo DualMix.exe nativo.')
            }
        }
    }
    $dll = Join-Path $dest 'DualMix.dll'
    $exe = Join-Path $dest 'DualMix.exe'
    $icon = Join-Path $dest 'dualmix.ico'
    if (-not (Test-Path $icon)) { $icon = $exe }
    $lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'DualMix.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $sc = $shell.CreateShortcut($lnk)
    if ($isNative) {
        $sc.TargetPath = $exe
        $sc.Arguments = "--supervise"
    } else {
        $sc.TargetPath = $conhost
        $sc.Arguments = "--headless `"$hostExe`" `"$dll`" --supervise"
    }
    $sc.WorkingDirectory = $dest
    $sc.IconLocation = "$icon,0"
    $sc.Description = T 'DualMix - Game/Chat audio mixer' 'DualMix - mixer Game/Chat'
    $sc.Save()

    # the oldest way of starting with Windows (a Run entry that opened a terminal) is gone
    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'DualMix' -ErrorAction SilentlyContinue

    # Windows "Installed apps" entry (Settings > Apps > Installed apps > DualMix > Uninstall)
    New-Item $uninstallKey -Force | Out-Null
    $sizeKb = [int]((Get-ChildItem $dest -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
    Set-ItemProperty $uninstallKey -Name DisplayName -Value 'DualMix'
    Set-ItemProperty $uninstallKey -Name DisplayVersion -Value "$version"
    Set-ItemProperty $uninstallKey -Name Publisher -Value 'Marco'
    Set-ItemProperty $uninstallKey -Name DisplayIcon -Value "$icon,0"
    Set-ItemProperty $uninstallKey -Name InstallLocation -Value $dest
    Set-ItemProperty $uninstallKey -Name UninstallString -Value "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $dest 'uninstall.ps1')`" -Pause"
    Set-ItemProperty $uninstallKey -Name NoModify -Value 1 -Type DWord
    Set-ItemProperty $uninstallKey -Name NoRepair -Value 1 -Type DWord
    Set-ItemProperty $uninstallKey -Name EstimatedSize -Value $sizeKb -Type DWord

    # 6. start (DualMix puts its own sign-in task back by itself when "Start with Windows" is on)
    if (-not $NoStart) {
        Start-DualMix "$hostExe" $isNative ([bool]($Update -or $Silent))
        Say (T 'DualMix started (tray icon in notification area).' 'DualMix avviato (icona nell''area di notifica).')
    }
    Say "=== $(T "DualMix $version installed" "DualMix $version installato") ==="
    exit 0
}
catch {
    Say "$(T 'ERROR' 'ERRORE'): $($_.Exception.Message)"
    # put the previous program back, and after a failed automatic update start it again
    try {
        if ($hadOld -and (Test-Path $old) -and -not (Test-Path $dest)) { Move-Folder $old $dest; Say (T 'Previous version restored.' 'Versione precedente ripristinata.') }
        if (Test-Path $new) { Remove-Item $new -Recurse -Force -ErrorAction SilentlyContinue }
        if ($Update -and -not (Get-DualMixProcesses)) {
            if ((Test-Path (Join-Path $dest 'DualMix.exe')) -and -not (Test-Path (Join-Path $dest 'DualMix.dll'))) {
                Start-DualMix "" $true $true; Say (T 'DualMix restarted with the previous version.' 'DualMix riavviato con la versione precedente.')
            } elseif (Test-Path (Join-Path $dest 'DualMix.dll')) {
                $h = Find-DesktopHost
                if ($h) { Start-DualMix $h $false $true; Say (T 'DualMix restarted with the previous version.' 'DualMix riavviato con la versione precedente.') }
            }
        }
    } catch { Say "$(T 'ERROR during rollback' 'ERRORE nel ripristino'): $($_.Exception.Message)" }
    exit 1
}
