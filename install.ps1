<#
  DualMix - build & install (no admin rights needed for the program itself)

  Builds DualMix in Release and installs it to %LOCALAPPDATA%\Programs\DualMix, creates the Start menu
  shortcut, registers it in Windows "Installed apps" (so it can be uninstalled from Settings) and starts it.

  How DualMix runs:
    - no terminal window: it is started through Windows' own console host in headless mode
      (conhost.exe --headless dotnet.exe DualMix.dll --supervise);
    - like a service: a small supervisor keeps it running and restarts it if it ever ends abnormally;
    - at sign-in: through the task "DualMix" of the Windows Task Scheduler, which DualMix registers by itself
      (switch it off in DualMix > Generale > "Avvia con Windows", or in the Task Scheduler);
    - on first start it asks for administrator rights once, to install/configure the virtual-cable driver.

  Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-NoStart]
#>
param([switch]$NoStart)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$project = Join-Path $root 'DualMix\DualMix.csproj'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\DualMix'
$userDotnet = Join-Path $env:USERPROFILE '.dotnet'
$conhost = Join-Path $env:SystemRoot 'System32\conhost.exe'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_NOLOGO = '1'

function Get-DotnetInstallScript {
    $script = Join-Path $env:TEMP 'dotnet-install.ps1'
    if (-not (Test-Path $script)) { Invoke-WebRequest https://dot.net/v1/dotnet-install.ps1 -OutFile $script -UseBasicParsing }
    return $script
}

# --- .NET 10 SDK (system one, or a per-user one in %USERPROFILE%\.dotnet)
$sdk = $null
foreach ($candidate in @((Get-Command dotnet -ErrorAction SilentlyContinue).Source, (Join-Path $userDotnet 'dotnet.exe'))) {
    if ($candidate -and (Test-Path $candidate)) {
        if ((& $candidate --list-sdks 2>$null) -match '^1\d\.') { $sdk = $candidate; break }
    }
}
if (-not $sdk) {
    Write-Host 'SDK .NET 10 non trovato: lo installo in %USERPROFILE%\.dotnet (solo per questo utente)...'
    & (Get-DotnetInstallScript) -Channel 10.0 -InstallDir $userDotnet -NoPath
    $sdk = Join-Path $userDotnet 'dotnet.exe'
}

# --- .NET 10 Desktop runtime + host that will run DualMix
function Find-DesktopHost {
    foreach ($dir in @((Join-Path $env:ProgramFiles 'dotnet'), $userDotnet)) {
        $exe = Join-Path $dir 'dotnet.exe'
        $desktop = Join-Path $dir 'shared\Microsoft.WindowsDesktop.App'
        if ((Test-Path $exe) -and (Test-Path $desktop) -and (Get-ChildItem $desktop -Directory -Filter '10.*')) { return $exe }
    }
    return $null
}
$hostExe = Find-DesktopHost
if (-not $hostExe) {
    Write-Host 'Runtime .NET 10 Desktop non trovato: lo installo in %USERPROFILE%\.dotnet...'
    & (Get-DotnetInstallScript) -Channel 10.0 -Runtime windowsdesktop -InstallDir $userDotnet -NoPath
    $hostExe = Find-DesktopHost
    if (-not $hostExe) { throw 'Impossibile installare il runtime .NET 10 Desktop.' }
}

# --- stop a running copy gracefully (it restores app volumes, default devices and per-app outputs on exit)
function Get-DualMixProcesses {
    Get-CimInstance Win32_Process | Where-Object {
        $_.Name -eq 'DualMix.exe' -or (($_.Name -eq 'dotnet.exe' -or $_.Name -eq 'conhost.exe') -and $_.CommandLine -like '*DualMix.dll*')
    }
}
if (Get-DualMixProcesses) {
    Write-Host 'Chiudo DualMix in esecuzione...'
    $dll = Join-Path $dest 'DualMix.dll'
    if (Test-Path $dll) { Start-Process $hostExe -ArgumentList "`"$dll`" --exit" -Wait -WindowStyle Hidden }
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-DualMixProcesses) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
    Get-DualMixProcesses | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# --- build + publish (ReadyToRun: precompiled, no JIT pauses when the audio engine starts)
Write-Host "Compilo e installo in $dest ..."
if (Test-Path $dest) { Get-ChildItem $dest -Force | Remove-Item -Recurse -Force }
& $sdk publish $project -c Release -r win-x64 --self-contained false -o $dest -p:DebugType=none -nologo -v q
if ($LASTEXITCODE -ne 0) { throw "Compilazione fallita ($LASTEXITCODE)" }
Copy-Item (Join-Path $root 'uninstall.ps1') (Join-Path $dest 'uninstall.ps1') -Force

$dll = Join-Path $dest 'DualMix.dll'
$exe = Join-Path $dest 'DualMix.exe'
$background = "--headless `"$hostExe`" `"$dll`" --supervise"

# --- Start menu shortcut: windowless, supervised (same way the sign-in task starts it)
$programs = [Environment]::GetFolderPath('Programs')
$lnk = Join-Path $programs 'DualMix.lnk'
$shell = New-Object -ComObject WScript.Shell
$sc = $shell.CreateShortcut($lnk)
$sc.TargetPath = $conhost
$sc.Arguments = $background
$sc.WorkingDirectory = $dest
$sc.IconLocation = "$exe,0"
$sc.Description = 'DualMix - mixer Game/Chat'
$sc.Save()

# --- the old way of starting with Windows (a Run entry that opened a terminal) is gone
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'DualMix' -ErrorAction SilentlyContinue

# --- Windows "Installed apps" entry (Settings > Apps > Installed apps > DualMix > Uninstall)
$version = ([xml](Get-Content $project)).Project.PropertyGroup.Version | Where-Object { $_ } | Select-Object -First 1
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DualMix'
New-Item $uninstallKey -Force | Out-Null
$sizeKb = [int]((Get-ChildItem $dest -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
Set-ItemProperty $uninstallKey -Name DisplayName -Value 'DualMix'
Set-ItemProperty $uninstallKey -Name DisplayVersion -Value "$version"
Set-ItemProperty $uninstallKey -Name Publisher -Value 'Marco'
Set-ItemProperty $uninstallKey -Name DisplayIcon -Value "$exe,0"
Set-ItemProperty $uninstallKey -Name InstallLocation -Value $dest
Set-ItemProperty $uninstallKey -Name UninstallString -Value "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $dest 'uninstall.ps1')`" -Pause"
Set-ItemProperty $uninstallKey -Name NoModify -Value 1 -Type DWord
Set-ItemProperty $uninstallKey -Name NoRepair -Value 1 -Type DWord
Set-ItemProperty $uninstallKey -Name EstimatedSize -Value $sizeKb -Type DWord

Write-Host "Installato: $dest"
Write-Host "Collegamento: $lnk"

if (-not $NoStart) {
    Start-Process $conhost -ArgumentList "$background --tray" -WorkingDirectory $dest
    Write-Host 'DualMix avviato in background (icona nell''area di notifica, nessuna finestra).'
}
