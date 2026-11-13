# DualMix

Mixer audio per Windows 11 nello stile del ChatMix di SteelSeries: due uscite virtuali, **DualMix Game** e
**DualMix Chat**, mixate sull'uscita reale (Sound Blaster AE-5 Plus). La rotella della tastiera (Volume +, Volume −,
Muto) regola una di quattro funzioni: Volume generale, Mix Game/Chat, Volume Game, Volume Chat.

Versione 2.2.0 · .NET 10 WPF (`net10.0-windows`, win-x64) · WPF-UI 4.3.0 · NAudio.Wasapi 2.2.1 · interfaccia in italiano.

## Installare

Dalla cartella dei rilasci (Nextcloud `_claude/condivisi/DualMix/`, in futuro le release GitHub):

- **`DualMix-Setup-<v>.exe`**: doppio clic. Niente diritti di amministratore.
- oppure **`DualMix-<v>-win-x64.zip`** → estrai → `Installa DualMix.cmd` (stesse operazioni, senza `.exe`).

Il setup scarica il runtime .NET 10 Desktop se manca (da Microsoft, solo per l'utente), chiude un DualMix in esecuzione,
toglie quello installato alla vecchia maniera, installa in `%LOCALAPPDATA%\Programs\DualMix`, crea collegamento e voce
in «App installate», avvia. Scrive tutto in `%APPDATA%\DualMix\setup.log`.

- Togliere solo il vecchio DualMix (fino alla 2.1.0): `Rimuovi-vecchio-DualMix.zip` → `Rimuovi vecchio DualMix.cmd`.
- Disinstallare: «App installate» di Windows, oppure `Disinstalla DualMix.cmd` (`-RemoveDrivers`, `-Purge`).

## La rotella

| Funzione | Gira | Premi |
|---|---|---|
| **Volume generale** (predefinita a ogni avvio) | volume di Windows, applicato alla scheda audio | muto |
| **Mix Game/Chat** | sinistra più Game, destra più Chat | **muto di tutto** (opzione: torna a «Bilanciato») |
| Volume Game / Volume Chat | volume del canale | muto del canale |

- Cambiare funzione: doppia pressione, Ctrl + pressione, o la scorciatoia (Ctrl+Alt+M, con ripiego automatico su
  K, R, F12 se è già occupata).
- Ctrl + gira: l'altra funzione, al momento.
- «Funzione all'avvio» (Rotella e tasti → Comportamento): da dove riparte la rotella a ogni avvio di DualMix.
  Predefinito: Volume generale. «L'ultima usata» ripristina il comportamento delle versioni fino alla 2.1.

## Aggiornamenti

Schema preso dall'update checker di RP Soundboard: un documento di versione a un indirizzo fisso dice qual è l'ultima
versione e dove sta il pacchetto; il programma confronta, scarica nella cartella temporanea e lancia un installer che
aspetta la chiusura del programma, lo sostituisce e lo riavvia.

- Documento: `version.json` (`product`, `version`, `url`, `sha256`, `notes`). Predefinito:
  `https://raw.githubusercontent.com/gamebaiters/DualMix/main/version.json`.
- `url` può essere relativo al documento. Il pacchetto è lo zip del rilascio; viene rifiutato se lo SHA-256 non
  corrisponde. Solo https.
- Controllo un minuto dopo l'avvio e poi una volta al giorno. **Automatici** (predefinito): la versione trovata
  all'avvio si installa da sola e DualMix riparte; una trovata più tardi viene solo annunciata (niente interruzioni
  dell'audio a metà partita) e si installa al prossimo avvio o da Generale → Aggiornamenti. **Avvisa**, **Mai**.
- La sorgente si cambia con `UpdateFeed` in `%APPDATA%\DualMix\settings.json`: un indirizzo https oppure una
  cartella che contiene `version.json` e lo zip (per esempio una cartella sincronizzata).

Codice: `DualMix/Core/Updater.cs`; pianificazione in `App.xaml.cs` (`AutoUpdateAsync`); setup in
`packaging/setup/setup.ps1` (`-Update`).

## Sviluppo

```
DualMix/                 il programma
  Core/                  Controller (stato, rotella, volumi), Settings, Updater, integrazione con Windows
  Audio/                 AudioRouter (loopback WASAPI → mix → uscita), SessionEngine, Devices, DriverSetup
  Input/                 KeyboardHook (RegisterHotKey + hook), KnobController
  UI/                    finestra, pagine, icona nell'area di notifica, indicatore a schermo (OsdWindow)
packaging/               setup\ (setup, uninstall, rimuovi-vecchio), i .cmd, installer.nsi, LEGGIMI.txt
tools/build-release.sh   pacchetto zip + installer exe + version.json   (Linux: SDK .NET 10, NSIS, zip)
tools/rilascia.sh        da claudenest: build su dev-linux → artifacts → disco condiviso
tools/make_icon.py       disegna l'icona (Pillow)
install.ps1              sul PC con il sorgente: compila e installa con lo stesso setup dei rilasci
.github/workflows/       release.yml: al push di un tag v<versione> crea la release e aggiorna version.json
```

- Compilare: `dotnet build -c Release` in `DualMix/` (Windows, oppure Linux grazie a `EnableWindowsTargeting`).
- Nuova versione: alza `<Version>` in `DualMix.csproj`, commit, poi `tools/rilascia.sh "novità"` (Nextcloud) oppure
  `git tag -a v<versione> -m "novità" && git push --tags` (GitHub).
- Diagnostica: `dotnet DualMix.dll --diag [--quick] [--stress]` → `%APPDATA%\DualMix\diag.txt`.
  Schermate di tutte le pagine senza toccare l'audio: `--snapshot <cartella>`.
- Log: `%APPDATA%\DualMix\dualmix.log`.

## Come gira

`conhost.exe --headless dotnet.exe DualMix.dll --supervise --tray`: nessuna finestra, dentro l'host .NET di Microsoft
(un eseguibile non firmato veniva trattenuto dall'antivirus), con un supervisore che lo riavvia se termina in modo
anomalo. Avvio all'accesso con l'attività pianificata «DualMix» (Generale → Avvia con Windows).

## Cavi virtuali

VB-CABLE A+B di VB-Audio (pack acquistato): cavo A = «DualMix Game» (5.1, 48 kHz, 24 bit), cavo B = «DualMix Chat»
(stereo). Il VB-CABLE gratuito singolo non basta. Il volume dell'endpoint di un cavo non tocca l'audio catturato dal
loopback. L'uscita reale non deve mai diventare un cavo.
