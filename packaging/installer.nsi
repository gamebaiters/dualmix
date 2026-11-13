; DualMix - Windows installer (NSIS). Built by tools/build-release.sh:
;   makensis -DVERSION=2.2.0 -DSTAGE=<package folder> -DOUTFILE=<DualMix-Setup-2.2.0.exe> packaging/installer.nsi
;
; It is only a self-extracting wrapper: it unpacks the release package to a temporary folder and runs
; setup\setup.ps1, the one place where the installation logic lives (the same script the automatic update uses).
; No administrator rights. Silent installation: DualMix-Setup-x.y.z.exe /S

Unicode true
!include "LogicLib.nsh"

!ifndef VERSION
  !error "VERSION is not defined"
!endif

Name "DualMix ${VERSION}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "..\DualMix\Assets\dualmix.ico"
BrandingText "DualMix ${VERSION}"
Caption "Installazione di DualMix ${VERSION}"
ShowInstDetails show
AutoCloseWindow false
InstallButtonText "Installa"
CompletedText "Fatto"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "DualMix"
VIAddVersionKey "FileDescription" "Installazione di DualMix"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Marco"

Page instfiles

Section "DualMix"
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\pkg"
  DetailPrint "Preparo i file..."
  SetDetailsPrint none
  File /r "${STAGE}\*.*"
  SetDetailsPrint both

  ; 64-bit PowerShell (this installer is a 32-bit program: $SYSDIR would be the 32-bit folder)
  StrCpy $1 "$WINDIR\sysnative\WindowsPowerShell\v1.0\powershell.exe"
  ${IfNot} ${FileExists} "$1"
    StrCpy $1 "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe"
  ${EndIf}
  StrCpy $2 ""
  ${If} ${Silent}
    StrCpy $2 " -Silent"
  ${EndIf}

  DetailPrint "Installo DualMix ${VERSION}..."
  nsExec::ExecToLog '"$1" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\pkg\setup\setup.ps1"$2'
  Pop $0
  SetOutPath "$TEMP"
  ${If} $0 != "0"
    DetailPrint "Installazione non riuscita (codice $0)."
    SetErrorLevel 1
    MessageBox MB_OK|MB_ICONSTOP "L'installazione di DualMix non e' riuscita.$\r$\n$\r$\nI dettagli sono nella finestra e in %APPDATA%\DualMix\setup.log." /SD IDOK
    Abort
  ${EndIf}
  DetailPrint "DualMix ${VERSION} installato: lo trovi nell'area di notifica e nel menu Start."
SectionEnd
