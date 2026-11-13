DualMix - Universal Game/Chat Audio Mixer for Windows
=====================================================

INSTALL OR UPDATE
  Double-click  DualMix-Setup-<version>.exe
  No administrator privileges required. If an older DualMix installation is already present
  (even a legacy source build), setup shuts it down cleanly, removes it, and keeps your settings.
  When launched for the first time, DualMix opens a short 3-step Setup Wizard to pick your
  output device, choose Per-App Volume (zero drivers) or Virtual Devices (VB-CABLE), and configure
  the volume knob. UI automatically uses English, or Italian if Windows display language is Italian.

  If Windows SmartScreen or antivirus blocks the unsigned .exe:
    download  DualMix-<version>-win-x64.zip, extract it, and double-click  "Install DualMix.cmd".
  It runs the exact same setup script.

REMOVE ONLY A LEGACY DUALMIX INSTALL (up to 2.1.0, without installing anything)
  Remove-old-DualMix.zip  ->  extract  ->  "Remove old DualMix.cmd"
  Removes the program, autostart, shortcuts, and Installed Apps entry.
  Keeps settings, logs, and virtual cables (add -Purge to also delete settings).

UNINSTALL
  Windows Settings > Apps > Installed apps > DualMix > Uninstall
  or run "Uninstall DualMix.cmd" from the zip (-RemoveDrivers also removes virtual cables, -Purge also removes settings).

AUTOMATIC UPDATES
  DualMix checks for new versions on startup and once per day (General > Updates).
  It reads version.json (version, package URL, SHA-256 checksum).

FILES
  DualMix-Setup-<v>.exe        Double-click Windows installer
  DualMix-<v>-win-x64.zip      Portable package (also downloaded by auto-update)
  Remove-old-DualMix.zip       Standalone cleanup script for legacy installs
  version.json                 Latest release manifest
  SHA256SUMS.txt               File checksums

Setup log location: %APPDATA%\DualMix\setup.log
