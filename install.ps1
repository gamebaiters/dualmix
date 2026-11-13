<#
  DualMix - build from source and install (for development, on a PC with the source code)

  Compiles the native Rust DualMix.exe in Release and installs it with the same setup the releases use
  (packaging\setup\setup.ps1): %LOCALAPPDATA%\Programs\DualMix, Start menu shortcut, Windows "Installed apps" entry, start.
  To install a released version there is no need for this: use DualMix-Setup-x.y.z.exe or "Install DualMix.cmd".

  Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-NoStart] [-Dotnet]
#>
param([switch]$NoStart, [switch]$Dotnet)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$stage = Join-Path $root 'out\stage\app'
$isIt = [System.Globalization.CultureInfo]::CurrentUICulture.TwoLetterISOLanguageName -eq 'it'
function T([string]$en, [string]$it) { if ($isIt) { $it } else { $en } }

if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null

if (-not $Dotnet) {
    $cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
    if (-not (Test-Path $cargo)) {
        throw (T 'Rust/Cargo not found. Install Rust from https://rustup.rs and retry.' 'Rust/Cargo non trovato. Installa Rust da https://rustup.rs e riprova.')
    }
    Write-Host (T 'Building native DualMix.exe (Release)...' 'Compilo DualMix.exe nativo (Release)...')
    $manifest = Join-Path $root 'dualmix-rs\Cargo.toml'
    & $cargo build --manifest-path $manifest --release
    if ($LASTEXITCODE -ne 0) { throw (T "Build failed ($LASTEXITCODE)" "Compilazione fallita ($LASTEXITCODE)") }
    $exe = Join-Path $root 'dualmix-rs\target\release\DualMix.exe'
    Copy-Item $exe (Join-Path $stage 'DualMix.exe') -Force
    Copy-Item (Join-Path $root 'DualMix\Assets\dualmix.ico') (Join-Path $stage 'dualmix.ico') -Force
} else {
    $project = Join-Path $root 'DualMix\DualMix.csproj'
    $userDotnet = Join-Path $env:USERPROFILE '.dotnet'
    $env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
    $env:DOTNET_NOLOGO = '1'

    $sdk = $null
    foreach ($candidate in @((Get-Command dotnet -ErrorAction SilentlyContinue).Source, (Join-Path $userDotnet 'dotnet.exe'))) {
        if ($candidate -and (Test-Path $candidate)) {
            if ((& $candidate --list-sdks 2>$null) -match '^1\d\.') { $sdk = $candidate; break }
        }
    }
    if (-not $sdk) {
        Write-Host (T '.NET 10 SDK not found: installing into %USERPROFILE%\.dotnet (current user only)...' 'SDK .NET 10 non trovato: lo installo in %USERPROFILE%\.dotnet (solo per questo utente)...')
        $script = Join-Path $env:TEMP 'dotnet-install.ps1'
        Invoke-WebRequest https://dot.net/v1/dotnet-install.ps1 -OutFile $script -UseBasicParsing
        & $script -Channel 10.0 -InstallDir $userDotnet -NoPath
        $sdk = Join-Path $userDotnet 'dotnet.exe'
    }

    Write-Host (T 'Building (.NET)...' 'Compilo (.NET)...')
    & $sdk publish $project -c Release -r win-x64 --self-contained false -o $stage -p:DebugType=none -nologo -v q
    if ($LASTEXITCODE -ne 0) { throw (T "Build failed ($LASTEXITCODE)" "Compilazione fallita ($LASTEXITCODE)") }
    Copy-Item (Join-Path $root 'DualMix\Assets\dualmix.ico') (Join-Path $stage 'dualmix.ico') -Force
}

& (Join-Path $root 'packaging\setup\setup.ps1') -Source $stage -NoStart:$NoStart
exit $LASTEXITCODE
