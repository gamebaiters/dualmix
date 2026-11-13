<#
  DualMix - build from source and install (for development, on a PC with the source code)

  Compiles DualMix in Release and installs it with the same setup the releases use (packaging\setup\setup.ps1):
  %LOCALAPPDATA%\Programs\DualMix, Start menu shortcut, Windows "Installed apps" entry, start.
  To install a released version there is no need for this: use DualMix-Setup-x.y.z.exe or "Installa DualMix.cmd".

  Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-NoStart]
#>
param([switch]$NoStart)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$project = Join-Path $root 'DualMix\DualMix.csproj'
$stage = Join-Path $root 'out\stage\app'
$userDotnet = Join-Path $env:USERPROFILE '.dotnet'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_NOLOGO = '1'

# .NET 10 SDK (the system one, or a per-user one in %USERPROFILE%\.dotnet)
$sdk = $null
foreach ($candidate in @((Get-Command dotnet -ErrorAction SilentlyContinue).Source, (Join-Path $userDotnet 'dotnet.exe'))) {
    if ($candidate -and (Test-Path $candidate)) {
        if ((& $candidate --list-sdks 2>$null) -match '^1\d\.') { $sdk = $candidate; break }
    }
}
if (-not $sdk) {
    Write-Host 'SDK .NET 10 non trovato: lo installo in %USERPROFILE%\.dotnet (solo per questo utente)...'
    $script = Join-Path $env:TEMP 'dotnet-install.ps1'
    Invoke-WebRequest https://dot.net/v1/dotnet-install.ps1 -OutFile $script -UseBasicParsing
    & $script -Channel 10.0 -InstallDir $userDotnet -NoPath
    $sdk = Join-Path $userDotnet 'dotnet.exe'
}

# build + publish (ReadyToRun: precompiled, no JIT pauses when the audio engine starts)
Write-Host 'Compilo...'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
& $sdk publish $project -c Release -r win-x64 --self-contained false -o $stage -p:DebugType=none -nologo -v q
if ($LASTEXITCODE -ne 0) { throw "Compilazione fallita ($LASTEXITCODE)" }
Copy-Item (Join-Path $root 'DualMix\Assets\dualmix.ico') (Join-Path $stage 'dualmix.ico') -Force

& (Join-Path $root 'packaging\setup\setup.ps1') -Source $stage -NoStart:$NoStart
exit $LASTEXITCODE
