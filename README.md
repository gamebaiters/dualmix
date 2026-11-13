# DualMix

A Game/Chat mixer for Windows, modelled on SteelSeries **ChatMix** (GameDAC / Sonar).
One control balances the game or film against the voice chat (Discord, TeamSpeak…):

- **Center**: Game 100%, Chat 100%
- **Towards Game** (left): Game stays at 100%, Chat goes down
- **Towards Chat** (right): Chat stays at 100%, Game goes down

It starts with Windows and lives in the notification area. You can drive it from the window,
from the tray icon, or from the **NuPhy Kick75 knob**.

---

## Installation

The program is already installed in `%LOCALAPPDATA%\Programs\DualMix` and starts with Windows.
To reinstall or update after changing the code:

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1
```

You do not need admin rights. If the .NET 8 SDK is missing, the script downloads it into `%USERPROFILE%\.dotnet`.

To uninstall:

```powershell
powershell -ExecutionPolicy Bypass -File uninstall.ps1            # removes the program
powershell -ExecutionPolicy Bypass -File uninstall.ps1 -Purge     # also removes settings and logs
```

---

## How it separates Game and Chat

### 1. Per application (default, no drivers)

DualMix finds every application that plays audio, on every output device. It puts each one in a channel:

- **Chat** (automatic): Discord (and PTB/Canary/Vesktop), TeamSpeak 3/5/6, Mumble, Skype, Teams, Zoom,
  Slack, Signal, WhatsApp, Telegram, Element, Guilded…
- **Game** (automatic): everything else, including games, browsers, VLC, Spotify and so on.
- You can change the channel of any app from the **Applicazioni** tab ("Automatico", "Game", "Chat",
  "Ignora"). Each app also has its own volume inside its channel.

DualMix then sets each app's volume in the Windows mixer to *channel volume × mix × app volume*.

Details:
- On first sight, DualMix **inherits** the volume each app already had in the Windows mixer.
  Starting DualMix never makes anything louder.
- If you move an app's volume in the Windows mixer, DualMix adopts it as that app's new volume
  instead of fighting you.
- When DualMix exits, it puts back each app's own volume, without the mix applied.
- "Anche non attive" shows the remembered apps. "Aggiungi…" lets you pre-assign an .exe that has
  not played anything yet.

### 2. Virtual devices (like Sonar)

This mode gives two real, separate audio outputs, **Game** and **Chat**. DualMix captures them and
mixes them onto your headset. It needs a virtual audio cable driver, because Windows cannot create
audio devices without a signed driver:

1. Install **VB-Cable A+B** from vb-audio.com (or Voicemeeter Banana), then restart.
2. In DualMix go to **Modalità e dispositivi → Dispositivi virtuali**.
3. Choose `CABLE-A Input` as Game, `CABLE-B Input` as Chat, and your headset as the real output.
4. Click **Applica**.

With the options turned on, DualMix does the following:
- It makes Game the default Windows device and Chat the default communications device.
- It routes each app to the right output by itself, based on its channel. It uses the same Windows
  feature as "Volume app e preferenze dispositivo". Leave the output on "Predefinito" in
  Discord and TeamSpeak.
- When it exits or changes mode, it **puts everything back**: default devices and per-app outputs.
  If it crashes, it cleans up on the next start.

The audio engine captures each virtual cable in loopback and resamples it with clock-drift
compensation. It sums the two with the mix gains, using smooth gain ramps and a soft limiter,
and plays the result on the headset. Latency is configurable (10–50 ms). If the output device
disappears, for example when you switch the headset off, it restarts by itself.

---

## NuPhy Kick75 knob

The Kick75 knob sends the standard multimedia keys **Volume +**, **Volume −** and **Mute**.
DualMix intercepts them with a global keyboard hook. In the **Rotella** tab you choose the behaviour:

| Mode | Turn the knob | Ctrl + turn | Press |
|---|---|---|---|
| Mix with modifier (default) | Windows volume | **Game/Chat mix** | mute (Ctrl+press = mix to center) |
| Always mix | **Game/Chat mix** | – | mute, or "mix to center" |
| Press to switch | volume **or** mix | – | switches Volume ↔ Mix |
| Off | Windows volume | – | mute |

- Left (counter-clockwise) moves towards **Game**; right moves towards **Chat**. You can invert it.
- The step per click is configurable (default 5%). When the knob crosses the center it **stops at
  the center** for one click, so the balanced position is easy to find.
- Each change shows a small indicator above the game (bottom center). It is click-through and does
  not steal focus.
- The **Prova** box shows what the keyboard sends, so you can check the knob is recognised.

### Dedicated keys with VIA (optional)

To keep the knob for Windows volume and still have a separate control, you can remap keys with VIA.
The Kick75 is QMK/VIA compatible (usevia.app, or the NuPhy configurator).
1. Assign the knob (on one layer, for example the Fn layer) or other keys to **F13 / F14 / F15**.
   These keys do not exist on the keyboard, so they never clash with anything.
2. In DualMix, go to **Rotella → Tasti dedicati**. Click "Assegna…" and press the key.
   Assign "Sposta verso Game", "Sposta verso Chat" and "Centra il mix".

Dedicated keys always work, whatever the knob mode is.

---

## Other things DualMix does

- **Windows call ducking turned off.** By default Windows lowers every other sound by 80% when it
  detects a call, which ruins the mix. DualMix turned this off on first start. You can turn it back on
  in *Generale*.
- **Tray icon:** left click opens the window. Right click offers Tutto Game / Bilanciato / Tutto Chat,
  start with Windows, and exit. The tooltip shows the current levels.
- **Single instance:** launching it again only reopens the window.
- `DualMix.exe --exit` closes the running instance cleanly. The install/uninstall scripts use it.
- `DualMix.exe --diag` runs a self-test of the whole audio chain. It writes the result to
  `%APPDATA%\DualMix\diag.txt`.

## Files

- Settings: `%APPDATA%\DualMix\settings.json`
- Log: `%APPDATA%\DualMix\dualmix.log`
- Program: `%LOCALAPPDATA%\Programs\DualMix\`
- Autostart: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\DualMix`

## Known limitations

- In per-application mode, Discord in a **browser tab** counts as part of the browser. Use the app,
  or put the browser in Chat.
- Some anti-cheat systems run games as administrator. While an elevated window has focus, Windows
  does not deliver keys to non-elevated programs, so the knob does not reach DualMix. The mix still
  works; only the knob control is affected.
- An exclusive-fullscreen game hides the on-screen indicator. Borderless fullscreen shows it.

## Code structure

```
DualMix/
  Core/      settings, mix math, app catalog, controller (application state)
  Audio/     session engine (per-app volumes), virtual router (WASAPI loopback → mix → output),
             devices / default device / per-app routing, minimal Core Audio interop
  Input/     global keyboard hook + knob logic
  UI/        WPF window, OSD, tray icon, dark theme
  Diagnostics.cs   --diag self-test
tools/make-icon.ps1   generates the icon
install.ps1 / uninstall.ps1
```

Build by hand: `dotnet build DualMix\DualMix.csproj` (.NET 8 SDK).
