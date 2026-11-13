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

LoadLanguageFile "${NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${NSISDIR}\Contrib\Language files\Italian.nlf"

LangString MSG_CAPTION ${LANG_ENGLISH} "DualMix ${VERSION} Setup"
LangString MSG_CAPTION ${LANG_ITALIAN} "Installazione di DualMix ${VERSION}"
LangString MSG_INSTALL_BTN ${LANG_ENGLISH} "Install"
LangString MSG_INSTALL_BTN ${LANG_ITALIAN} "Installa"
LangString MSG_DONE ${LANG_ENGLISH} "Completed"
LangString MSG_DONE ${LANG_ITALIAN} "Fatto"
LangString MSG_PREPARE ${LANG_ENGLISH} "Extracting files..."
LangString MSG_PREPARE ${LANG_ITALIAN} "Preparo i file..."
LangString MSG_INSTALLING ${LANG_ENGLISH} "Installing DualMix ${VERSION}..."
LangString MSG_INSTALLING ${LANG_ITALIAN} "Installo DualMix ${VERSION}..."
LangString MSG_FAIL_LOG ${LANG_ENGLISH} "Installation failed (exit code $0)."
LangString MSG_FAIL_LOG ${LANG_ITALIAN} "Installazione non riuscita (codice $0)."
LangString MSG_FAIL_BOX ${LANG_ENGLISH} "DualMix installation failed.$\r$\n$\r$\nCheck the details above and %APPDATA%\DualMix\setup.log."
LangString MSG_FAIL_BOX ${LANG_ITALIAN} "L'installazione di DualMix non e' riuscita.$\r$\n$\r$\nI dettagli sono nella finestra e in %APPDATA%\DualMix\setup.log."
LangString MSG_OK ${LANG_ENGLISH} "DualMix ${VERSION} is installed: find it in the notification area and the Start menu."
LangString MSG_OK ${LANG_ITALIAN} "DualMix ${VERSION} installato: lo trovi nell'area di notifica e nel menu Start."

Name "DualMix ${VERSION}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "..\DualMix\Assets\dualmix.ico"
BrandingText "DualMix ${VERSION}"
Caption "$(MSG_CAPTION)"
ShowInstDetails show
AutoCloseWindow false
InstallButtonText "$(MSG_INSTALL_BTN)"
CompletedText "$(MSG_DONE)"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "DualMix"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "DualMix Setup"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "Marco"

Page instfiles

Section "DualMix"
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\pkg"
  DetailPrint "$(MSG_PREPARE)"
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

  DetailPrint "$(MSG_INSTALLING)"
  nsExec::ExecToLog '"$1" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\pkg\setup\setup.ps1"$2'
  Pop $0
  SetOutPath "$TEMP"
  ${If} $0 != "0"
    DetailPrint "$(MSG_FAIL_LOG)"
    SetErrorLevel 1
    MessageBox MB_OK|MB_ICONSTOP "$(MSG_FAIL_BOX)" /SD IDOK
    Abort
  ${EndIf}
  DetailPrint "$(MSG_OK)"
SectionEnd
