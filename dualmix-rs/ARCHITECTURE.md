# DualMix Native Rust Rewrite (`dualmix-rs`)

Branch: `rust-rewrite` (branched from `main` at `v2.3.0`).
Target: Native `x86_64-pc-windows-msvc` executable (`DualMix.exe`), zero .NET runtime, zero GC pauses, single-digit-ms scheduling jitter.

## Workspace Layout

- `crates/dualmix-core` — Cross-platform pure-Rust DSP & domain crate (unit-tested):
  - `math`: square-law perceptual taper, SteelSeries-style ChatMix balance curve, snap-to-center, dB formatting.
  - `rules`: `AppCatalog` (30+ built-in VoIP/chat apps) + case-insensitive `RuleSet`.
  - `settings`: JSON schema 1:1 compatible with C# `v2.x` (`%APPDATA%\DualMix\settings.json`), v1→v2 migration, atomic save.
  - `knob`: `KnobState`, `KnobConfig`, binding builder (keeps `Volume` at startup when `KnobStartFunction = Volume`, single press mutes on Mix when `KnobMixPressMutes`, Ctrl+turn alternates Mix↔Volume, dedicated keys F13–F15).
  - `resampler`: 160-tap polyphase Kaiser-windowed sinc resampler (2048 phases, $\beta=14$, SNR > 120 dB across 997 Hz–20 kHz, bit-exact copy at ratio 1:1).
  - `ringbuf`: Cache-line aligned wait-free SPSC sample ring buffer (`Acquire`/`Release` atomics).
  - `channel_map`: Speaker-position matrix (`Mono`/`Stereo`/`Quad`/`5.1`/`7.1`) with linear gain ramping.
  - `loc`: Auto OS language detection (`GetUserDefaultUILanguage` → `it` on Italian Windows, `en` default everywhere else) + `--lang en|it` override.
  - `updater`: `version.json` parser & SHA-256 package verifier (RP Soundboard update scheme).
- `crates/dualmix-audio` — Native Windows CoreAudio / WASAPI / MMCSS:
  - `process_tuning`: `HIGH_PRIORITY_CLASS`, EcoQoS / timer-throttling opt-out, `MmcssGuard` (`AvSetMmThreadCharacteristicsW("Pro Audio")` + `AVRT_PRIORITY_CRITICAL`).
  - `devices`: `IMMDeviceEnumerator`, `IPolicyConfig` vtable, WinRT `Windows.Media.Internal.AudioPolicyConfig` per-app routing factory check.
- `crates/dualmix-input` — Dedicated `DualMix.Keyboard` thread (`RegisterHotKey` + `WH_KEYBOARD_LL` + modifier inspection).
- `crates/dualmix-ui` — Windows single-instance Mutex/Show/Exit IPC (`Local\DualMix.*`).
- `crates/dualmix-app` — `DualMix.exe` binary (`--diag`, `--exit`, `--lang`, `--version`).

## Remaining Milestones for Full 1:1 Parity with C# `v2.3.0`

1. **Full WASAPI Loopback + Render Router (`dualmix-audio::router`)**:
   - Event-driven WASAPI shared loopback captures (`DualMix Game`, `DualMix Chat`) + real output `IAudioRenderClient`.
   - PI clock-drift controller feeding `SincResampler`, 10-minute digital silence auto-suspend, adaptive dropout reserve learning (`LearnedQueueMs` / `LearnedSourceMs`).
   - Full `IAudioSessionManager2` / `IAudioSessionControl2` / `ISimpleAudioVolume` / `IAudioMeterInformation` MTA monitor + `AudioPolicyConfig` slots 25/26 (`SetPersistedDefaultAudioEndpoint` / `GetPersistedDefaultAudioEndpoint`).
   - Endpoint configuration (`IPolicyConfig`: `SetDefaultEndpoint`, `SetDeviceFormat`, `SetPropertyValue`, `SetEndpointVisibility`) + VB-CABLE detection / elevated `--setup`.
2. **Native OSD, Tray & Settings/Setup Wizard UI (`dualmix-ui`)**:
   - Direct2D/Win32 click-through layered OSD (`WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_LAYERED`) hidden cleanly via `ShowWindow(SW_HIDE)` on expiry and `WM_DISPLAYCHANGE` (no ghost gray frame).
   - Native Win32 Tray Icon (`Shell_NotifyIconW`) + context menu.
   - Lightweight native Win32/WebView2 Settings window + 3-step guided Setup Wizard (`--wizard`) + `--snapshot <dir>` support.
3. **Packaging & Release CI**:
   - Update `packaging/setup/setup.ps1` & `installer.nsi` so no .NET 10 Desktop runtime download is required for Rust builds (single self-contained binary ~3–5 MB!).
   - Hardware verification on Marco's PC or Windows test box + GitHub Actions Rust build workflow.
