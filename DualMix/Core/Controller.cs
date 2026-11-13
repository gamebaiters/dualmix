using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using System.Windows.Threading;
using DualMix.Audio;
using DualMix.Input;
using NAudio.CoreAudioApi;
using NAudio.CoreAudioApi.Interfaces;

namespace DualMix.Core;

public enum OsdKind
{
    Mix,
    Volume,
    Game,
    Chat,
    Function,
}

/// <summary>What the on-screen indicator shows.</summary>
/// <param name="Value">Mix: −1..1; levels: 0..1.</param>
public sealed record OsdInfo(OsdKind Kind, string Title, string ValueText, double Value, bool Muted = false);

public enum StatusSeverity
{
    Success,
    Info,
    Warning,
    Error,
}

/// <summary>
/// Central application state. Lives on the UI thread; owns settings, the session engine, the audio router,
/// the keyboard hook, the knob logic and the endpoint volumes.
///
/// Audio layout while the virtual devices are active:
/// <list type="bullet">
/// <item>Chat apps play on the virtual device "DualMix Chat"; the router captures it and mixes it into the real
/// output with the Chat gain.</item>
/// <item>Game apps play straight on the real output (untouched signal path, no added latency) and are balanced
/// through their Windows session volume. When a second, independent virtual cable exists they play on
/// "DualMix Game" instead and are mixed by the router as well.</item>
/// </list>
/// If anything is wrong the controller falls back to plain per-app volumes with every Windows setting restored.
/// </summary>
public sealed class Controller : INotifyPropertyChanged, IDisposable
{
    private static readonly int ClassNotRegistered = unchecked((int)0x80040154);

    private readonly Dispatcher _ui;
    private readonly SessionEngine _sessions;
    private readonly AudioRouter _router = new();
    private readonly KeyboardHook _hook;
    private readonly DispatcherTimer _tick;
    private readonly EndpointVolume _outputVolume = new();
    private readonly EndpointVolume _masterProxy = new();
    private readonly EndpointVolume _chatVolume = new();
    private double _gameAmp = 1, _chatAmp = 1;
    private MMDeviceEnumerator? _notifyEnumerator;
    private DeviceNotifications? _notifications;
    private RuleSet _rules;
    private bool _metersEnabled;
    private bool _saveScheduled;
    private DateTime _saveDue;
    private bool _virtualActive;
    private bool _dual;
    private long _lastPressAt;
    private Action? _undoPress;
    private long _nextVolumeNote;
    private bool _shutdown;
    private bool _configuredEndpoints;
    private bool _autoSetupTried;
    private bool _elevatedConfigTried;
    private readonly bool _dryRun;
    private bool _packTried;
    private int _routerFailures;
    private long _nextEvaluate;
    private long _nextStatsLog;
    private long _devicesChangedAt = -1;
    private bool _defaultsDrifted;
    private bool _formatChanged;

    // status
    private StatusSeverity _severity = StatusSeverity.Info;
    private string _statusTitle = Loc.Get("Status.Starting");
    private string _statusMessage = "";
    private bool _needsDriver;
    private bool _needsRepair;
    private bool _setupBusy;
    private string? _outputName;
    private AudioFormat? _outputFormat;
    private long _underrunsSeen;
    private long _nextUnderrunLog;
    private string? _gameName;
    private string? _chatName;

    public Settings Settings { get; }
    public KnobController Knob { get; }

    public event PropertyChangedEventHandler? PropertyChanged;
    public event Action<OsdInfo>? OsdRequested;

    /// <summary>Raised when rules/apps change (UI refreshes the app list).</summary>
    public event Action? AppsChanged;

    /// <summary>Raised after devices / engine state were re-evaluated.</summary>
    public event Action? EngineChanged;

    /// <param name="dryRun">
    /// Look but do not touch: no keyboard hook, no change of Windows default devices, no per-app routing, no driver
    /// setup. Used to render screenshots of the interface without disturbing the audio of the PC.
    /// </param>
    public Controller(Settings settings, Dispatcher ui, bool dryRun = false)
    {
        Settings = settings;
        _ui = ui;
        _dryRun = dryRun;
        // Every start begins on the same knob function (by default the general volume), whatever was left selected.
        if (!dryRun && settings.StartTarget is { } start && settings.KnobTarget != start)
        {
            Log.Info($"Knob function at start: {start} (was {settings.KnobTarget})");
            settings.KnobTarget = start;
        }
        _rules = new RuleSet(settings);
        _gameAmp = MixMath.Taper(settings.GameVolume);
        _chatAmp = MixMath.Taper(settings.ChatVolume);
        _sessions = new SessionEngine(BuildParams());
        Knob = new KnobController(KnobConfig.From(settings), a => _ui.BeginInvoke(() => OnKnobAction(a)));
        _hook = new KeyboardHook(Knob.HandleKey, Knob.HandleHotkey) { AcceptInjected = settings.AcceptInjectedKeys };
        _hook.HotkeysApplied += () => _ui.BeginInvoke(() =>
        {
            EnsureShortcutUsable();
            OnPropertyChanged(nameof(KeyboardStatus));
        });
        if (!dryRun) Knob.Attach(_hook);
        _tick = new DispatcherTimer(TimeSpan.FromMilliseconds(250), DispatcherPriority.Background, (_, _) => Tick(), ui);
        _outputVolume.Changed += () => _ui.BeginInvoke(OnOutputVolumeChanged);
        _masterProxy.Changed += () => _ui.BeginInvoke(OnFrontVolumeChanged);
        _chatVolume.Changed += () => _ui.BeginInvoke(PullCableVolumes);
        Loc.LanguageChanged += OnLanguageChanged;
    }

    private void OnLanguageChanged()
    {
        _ui.BeginInvoke(() =>
        {
            if (_shutdown) return;
            Evaluate();
            AppsChanged?.Invoke();
            OnPropertyChanged(nameof(MixText));
            OnPropertyChanged(nameof(KnobTargetText));
            OnPropertyChanged(nameof(KnobHotkeyText));
            OnPropertyChanged(nameof(KeyboardStatus));
            OnPropertyChanged(nameof(TechnicalDetails));
        });
    }

    public void MarkAutoSetupHandled() => _autoSetupTried = true;

    // ===================================================================== lifecycle

    public void Start()
    {
        _sessions.Start();
        if (!_dryRun)
        {
            RecoverFromPreviousRun();
            _hook.Start();
        }
        try
        {
            _notifyEnumerator = new MMDeviceEnumerator();
            _notifications = new DeviceNotifications(this);
            _notifyEnumerator.RegisterEndpointNotificationCallback(_notifications);
        }
        catch (Exception ex)
        {
            Log.Warn("Device notifications unavailable", ex);
        }
        Evaluate();
        _tick.Start();
        Log.Info($"Controller started, mode {Settings.Mode}, mix {Settings.Mix:0.00}, knob {Settings.KnobTarget}");
    }

    /// <summary>Undo leftovers of a crash/forced shutdown while the virtual devices were active.</summary>
    private void RecoverFromPreviousRun()
    {
        if (Settings.DefaultsBackup != null)
        {
            Log.Info("Restoring default devices left over by a previous run");
            RestoreWindowsDefaults();
        }
        if (Settings.RoutedApps.Count > 0)
        {
            Log.Info("Resetting per-app devices left over by a previous run");
            UnrouteApps();
        }
    }

    public async Task ShutdownAsync()
    {
        if (_shutdown) return;
        _shutdown = true;
        Loc.LanguageChanged -= OnLanguageChanged;
        _tick.Stop();
        _hook.Dispose();
        try
        {
            if (_notifications != null) _notifyEnumerator?.UnregisterEndpointNotificationCallback(_notifications);
        }
        catch { /* ignore */ }

        PushParams(); // routing off from now on
        if (_virtualActive || Settings.RoutedApps.Count > 0)
        {
            try
            {
                await Task.WhenAny(_sessions.UnrouteAll(Settings.RoutedApps.ToList()), Task.Delay(2500)).ConfigureAwait(false);
                Settings.RoutedApps.Clear();
            }
            catch (Exception ex)
            {
                Log.Error("Unroute on exit failed", ex);
            }
        }
        RestoreWindowsDefaults();
        // The mixer stops only after the apps are back on the real device.
        await Task.Delay(150).ConfigureAwait(false);
        _router.Stop();

        await Task.Run(() => _sessions.Stop(Settings.RestoreVolumesOnExit)).ConfigureAwait(false);
        _outputVolume.Dispose();
        _masterProxy.Dispose();
        _chatVolume.Dispose();
        SettingsStore.Save(Settings);
        Log.Info("Controller shut down");
    }

    public void Dispose()
    {
        if (!_shutdown) ShutdownAsync().GetAwaiter().GetResult();
        _sessions.Dispose();
        _router.Dispose();
    }

    // ===================================================================== mix properties

    public double Mix => Settings.Mix;

    /// <summary>-100..100 for sliders.</summary>
    public double MixPercent
    {
        get => Math.Round(Settings.Mix * 100);
        // Gentle magnet around the centre so "balanced" is easy to hit with the mouse.
        set => SetMix(Math.Abs(value) < 3 ? 0 : value / 100.0, false);
    }

    public double GameVolumePercent
    {
        get => Math.Round(Settings.GameVolume * 100);
        set => SetChannelVolume(Channel.Game, value / 100.0, false);
    }

    public double ChatVolumePercent
    {
        get => Math.Round(Settings.ChatVolume * 100);
        set => SetChannelVolume(Channel.Chat, value / 100.0, false);
    }

    public bool GameMuted
    {
        get => Settings.GameMuted;
        set
        {
            Settings.GameMuted = value;
            MixStateChanged();
        }
    }

    public bool ChatMuted
    {
        get => Settings.ChatMuted;
        set
        {
            Settings.ChatMuted = value;
            if (_virtualActive && _chatVolume.IsAvailable) _chatVolume.Muted = value;
            MixStateChanged();
        }
    }

    /// <summary>
    /// Linear gains of the two channels: channel volume × balance, exactly ×1 at 100% / centre.
    /// A channel that has a virtual device uses the Windows volume of that device (its own dB curve); otherwise the
    /// perceptual taper of DualMix.
    /// </summary>
    public ChannelGains Gains
    {
        get
        {
            if (!_virtualActive) return MixMath.Compute(Settings);
            var (g, c) = MixMath.Balance(Settings.Mix, Settings.MixFloor);
            var game = Settings.GameMuted ? 0 : _gameAmp * MixMath.Taper(g);
            var chat = Settings.ChatMuted ? 0 : _chatAmp * MixMath.Taper(c);
            return new ChannelGains((float)game, (float)chat);
        }
    }

    public double GameLevel => MixMath.Levels(Settings).game;
    public double ChatLevel => MixMath.Levels(Settings).chat;
    public string GameLevelText => MixMath.Percent(GameLevel);
    public string ChatLevelText => MixMath.Percent(ChatLevel);

    public string MixText
    {
        get
        {
            var m = Settings.Mix;
            if (Math.Abs(m) < 0.001) return Loc.Get("Common.Balanced");
            return m < 0
                ? Loc.Format("Mixer.MixText.GameBias", ChatLevelText)
                : Loc.Format("Mixer.MixText.ChatBias", GameLevelText);
        }
    }

    // ---- Windows volume of the virtual devices
    //
    // "DualMix Chat": its Windows volume is the Chat channel volume.
    // "DualMix Game": it is the Windows default playback device, so its volume is what the Windows volume keys,
    // the volume flyout and the taskbar slider move. DualMix makes that the true master volume by keeping it in
    // step with the real output (the sound card): the standard Windows 11 volume interface then does exactly what
    // it shows. The volume of a cable never touches the audio DualMix captures from it, so nothing is attenuated twice.

    /// <summary>Windows changed the volume/mute of "DualMix Chat" (volume mixer, another app): adopt it.</summary>
    private void PullCableVolumes()
    {
        if (!_virtualActive || _shutdown) return;
        if (_chatVolume.IsAvailable)
        {
            Settings.ChatVolume = Math.Round(_chatVolume.Level, 3);
            Settings.ChatMuted = _chatVolume.Muted;
            _chatAmp = _chatVolume.Amplitude;
        }
        MixStateChanged();
    }

    /// <summary>DualMix is the authority when the virtual devices come up: write its state to the devices.</summary>
    private void PushCableVolumes()
    {
        _gameAmp = MixMath.Taper(Settings.GameVolume);
        if (_chatVolume.IsAvailable)
        {
            _chatVolume.Level = Settings.ChatVolume;
            _chatVolume.Muted = Settings.ChatMuted;
            _chatAmp = _chatVolume.Amplitude;
        }
        else
        {
            _chatAmp = MixMath.Taper(Settings.ChatVolume);
        }
        MirrorOutputToWindows();
    }

    /// <summary>The endpoint the Windows volume interface acts on: "DualMix Game" while it is the default, else the real output.</summary>
    private EndpointVolume Front => _masterProxy.IsAvailable ? _masterProxy : _outputVolume;

    private double _knownLevel = -1;
    private bool _knownMuted;
    private long _lastMuteToggleAt;

    /// <summary>The Windows default device shows the volume of the real output.</summary>
    private void MirrorOutputToWindows()
    {
        if (!_outputVolume.IsAvailable) return;
        if (_masterProxy.IsAvailable)
        {
            _masterProxy.Level = _outputVolume.Level;
            _masterProxy.Muted = _outputVolume.Muted;
        }
        _knownLevel = _outputVolume.Level;
        _knownMuted = _outputVolume.Muted;
    }

    /// <summary>
    /// The Windows volume changed without DualMix doing it: volume keys, flyout, taskbar, or the knob when its
    /// keys could not be intercepted (some games keep them from reaching other programs). If the knob is on another
    /// function, a key-sized step is the knob: the Windows volume is put back and the step goes where it belongs.
    /// Otherwise it is a real volume change and is applied to the sound card.
    /// </summary>
    private void OnFrontVolumeChanged()
    {
        if (_shutdown || !_outputVolume.IsAvailable) return;
        var front = Front;
        double level = front.Level;
        bool muted = front.Muted;
        if (_knownLevel < 0)
        {
            _knownLevel = _outputVolume.Level;
            _knownMuted = _outputVolume.Muted;
        }
        double prevLevel = _knownLevel;
        bool prevMuted = _knownMuted;
        bool muteToggled = muted != prevMuted;
        bool levelMoved = Math.Abs(level - prevLevel) > 0.004;
        if (!muteToggled && !levelMoved) return;

        var s = Settings;
        long tick = Environment.TickCount64;
        bool mouse = KeyboardHook.IsDown(0x01); // a volume slider is being dragged
        bool ctrl = KeyboardHook.IsDown(0x11);
        void Revert()
        {
            front.Level = prevLevel;
            front.Muted = prevMuted;
        }

        if (s.KnobEnabled && !mouse)
        {
            var fn = s.KnobTarget;
            if (ctrl && s.KnobCtrlAlternate) fn = fn == KnobTarget.Mix ? KnobTarget.Volume : KnobTarget.Mix;

            if (muteToggled && !levelMoved)
            {
                if (ctrl && s.KnobCtrlPressCycles)
                {
                    Revert();
                    CycleKnobTarget();
                    return;
                }
                if (fn != KnobTarget.Volume)
                {
                    Revert();
                    Log.Info("Knob press reached Windows instead of DualMix: corrected");
                    OnKnobPress(fn);
                    return;
                }
                // Function "Volume": the press is the normal Windows mute. Two in a row switch function instead,
                // and the sound is left unmuted, as it was before the first press.
                if (s.KnobDoublePressCycles)
                {
                    if (tick - _lastMuteToggleAt <= 600)
                    {
                        _lastMuteToggleAt = 0;
                        if (muted) front.Muted = false;
                        ApplyFront(level, false);
                        CycleKnobTarget();
                        return;
                    }
                    _lastMuteToggleAt = tick;
                }
            }
            else if (!muteToggled && fn != KnobTarget.Volume)
            {
                double steps = (level - prevLevel) / 0.02; // one key press = 2%
                int k = (int)Math.Round(steps);
                if (k != 0 && Math.Abs(k) <= 6 && Math.Abs(steps - k) < 0.26)
                {
                    Revert();
                    if (tick >= _nextVolumeNote)
                    {
                        _nextVolumeNote = tick + 5000;
                        Log.Info($"Knob turn reached Windows instead of DualMix: corrected (function {fn})");
                    }
                    for (int i = 0; i < Math.Abs(k); i++) OnKnobAction(new KnobAction(KnobActionKind.Rotate, Math.Sign(k), fn));
                    return;
                }
            }
        }
        ApplyFront(level, muted);
    }

    /// <summary>A real change of the Windows volume: the sound card follows.</summary>
    private void ApplyFront(double level, bool muted)
    {
        if (_masterProxy.IsAvailable)
        {
            _outputVolume.Level = level;
            _outputVolume.Muted = muted;
        }
        _knownLevel = level;
        _knownMuted = muted;
        OnPropertyChanged(nameof(OutputVolumePercent));
        OnPropertyChanged(nameof(OutputMuted));
    }

    /// <summary>The real output changed (Sound Blaster software, or Windows when it is the default device).</summary>
    private void OnOutputVolumeChanged()
    {
        if (_shutdown) return;
        if (_masterProxy.IsAvailable)
        {
            MirrorOutputToWindows();
            OnPropertyChanged(nameof(OutputVolumePercent));
            OnPropertyChanged(nameof(OutputMuted));
        }
        else
        {
            OnFrontVolumeChanged();
        }
    }

    // ---- master (real output) volume

    public bool OutputVolumeAvailable => _outputVolume.IsAvailable;

    public double OutputVolumePercent
    {
        get => Math.Round(_outputVolume.Level * 100);
        set
        {
            _outputVolume.Level = value / 100.0;
            if (value > 0 && _outputVolume.Muted) _outputVolume.Muted = false;
            MirrorOutputToWindows();
            OnPropertyChanged();
            OnPropertyChanged(nameof(OutputMuted));
        }
    }

    public bool OutputMuted
    {
        get => _outputVolume.Muted;
        set
        {
            _outputVolume.Muted = value;
            MirrorOutputToWindows();
            OnPropertyChanged();
        }
    }

    // ---- meters

    public float GamePeak => GameViaBus ? Math.Max(_router.GamePeak, _sessions.GamePeak) : _sessions.GamePeak;
    public float ChatPeak => _virtualActive ? _router.ChatPeak : _sessions.ChatPeak;
    public float OutputPeak => Math.Max(_virtualActive ? _router.OutputPeak : 0f, Math.Max(GamePeak, ChatPeak));

    public IReadOnlyList<AudioAppInfo> Apps => _sessions.Snapshot;

    public bool MetersEnabled
    {
        get => _metersEnabled;
        set
        {
            if (_metersEnabled == value) return;
            _metersEnabled = value;
            PushParams();
        }
    }

    // ---- status

    public bool VirtualActive => _virtualActive;

    /// <summary>Number of usable virtual cables (0 = driver missing, 1 = Chat only, 2+ = Game and Chat).</summary>
    public int VirtualCables { get; private set; }

    /// <summary>True when a second, independent cable exists ("DualMix Game" is available).</summary>
    public bool HasGameBus => _virtualActive && _dual;

    /// <summary>True when games play on "DualMix Game" and are mixed by DualMix (false: straight on the real output).</summary>
    public bool GameViaBus => _virtualActive && _dual && !Settings.GameDirect;

    public AudioRouter Router => _router;
    public StatusSeverity Severity => _severity;
    public string StatusTitle => _statusTitle;
    public string StatusMessage => _statusMessage;
    public bool NeedsDriver => _needsDriver;
    public bool NeedsRepair => _needsRepair;
    public bool SetupBusy => _setupBusy;
    public string OutputName => _outputName ?? Loc.Get("Common.NoOutput");
    public string GameDeviceName => _gameName ?? "—";
    public string ChatDeviceName => _chatName ?? "—";
    public bool DriverInstalled => !_needsDriver;
    public bool RouterRunning => _virtualActive && _router.State == RouterState.Running;
    public bool RouterIdle => RouterRunning && _router.Suspended;

    /// <summary>Format of the real output device (what the sound card receives).</summary>
    public string FormatText => _outputFormat?.Describe() ?? "—";

    public string LatencyText => RouterRunning
        ? (GameViaBus ? Loc.Format("Latency.Both", _router.LatencyMs) : Loc.Format("Latency.ChatOnly", _router.LatencyMs))
        : "—";

    public string EngineText => _virtualActive
        ? (GameViaBus ? Loc.Get("Engine.BothVirtual") : Loc.Get("Engine.ChatVirtual"))
        : Loc.Get("Engine.PerApp");

    public string TechnicalDetails
    {
        get
        {
            if (!RouterRunning) return Loc.Get("Status.Tech.NotRunning");
            var game = GameViaBus
                ? Loc.Format("Status.Tech.GameBus", _router.GameFormat?.Describe(), _router.GameDriftPpm)
                : Loc.Get("Status.Tech.GameDirect");
            var dropoutsExtra = _router.Underruns > 0 ? $" ({_router.DropoutDetail})" : "";
            var idle = _router.Suspended ? Loc.Get("Status.Tech.Idle") : "";
            var rt = _router.RealTimeScheduling ? "" : Loc.Get("Status.Tech.NoRt");
            return Loc.Format("Status.Tech.Summary",
                _router.OutputFormat?.Describe(),
                game,
                _router.ChatFormat?.Describe(),
                _router.ChatDriftPpm,
                _router.Underruns,
                dropoutsExtra,
                idle,
                rt);
        }
    }

    // ===================================================================== mix actions

    public void SetMix(double value, bool fromKnob)
    {
        var v = MixMath.SnapCenter(Math.Clamp(value, -1, 1));
        if (Math.Abs(v - Settings.Mix) < 1e-6 && !fromKnob) return;
        Settings.Mix = v;
        MixStateChanged();
        if (fromKnob)
        {
            Log.Info($"Knob: mix {v:+0.00;-0.00;0.00} (Game {GameLevelText}, Chat {ChatLevelText})");
            ShowMixOsd();
        }
    }

    public void Nudge(double delta, bool fromKnob = true)
    {
        var current = Settings.Mix;
        var next = current + delta;
        // The knob stops at the center for one notch when crossing it, so "balanced" is easy to find.
        if (current != 0 && Math.Sign(next) != Math.Sign(current) && next != 0) next = 0;
        SetMix(next, fromKnob);
    }

    public void ResetMix(bool fromKnob = false) => SetMix(0, fromKnob);

    public void SetChannelVolume(Channel ch, double value, bool fromKnob)
    {
        value = Math.Round(Math.Clamp(value, 0, 1), 3);
        if (ch == Channel.Game)
        {
            Settings.GameVolume = value;
            if (Settings.GameMuted && value > 0 && fromKnob) Settings.GameMuted = false;
            _gameAmp = MixMath.Taper(value);
        }
        else
        {
            Settings.ChatVolume = value;
            if (Settings.ChatMuted && value > 0 && fromKnob) Settings.ChatMuted = false;
            if (_virtualActive && _chatVolume.IsAvailable)
            {
                _chatVolume.Level = value;
                _chatVolume.Muted = Settings.ChatMuted;
                _chatAmp = _chatVolume.Amplitude;
            }
            else
            {
                _chatAmp = MixMath.Taper(value);
            }
        }
        MixStateChanged();
        if (fromKnob)
        {
            Log.Info($"Knob: {ch} volume {value:P0}");
            ShowChannelOsd(ch);
        }
    }

    /// <summary>Recomputes and pushes gains after a setting that affects them (e.g. mix floor) changed.</summary>
    public void RefreshGains() => MixStateChanged();

    private void MixStateChanged()
    {
        PushGains();
        OnPropertyChanged(nameof(Mix));
        OnPropertyChanged(nameof(MixPercent));
        OnPropertyChanged(nameof(GameVolumePercent));
        OnPropertyChanged(nameof(ChatVolumePercent));
        OnPropertyChanged(nameof(GameMuted));
        OnPropertyChanged(nameof(ChatMuted));
        OnPropertyChanged(nameof(Gains));
        OnPropertyChanged(nameof(GameLevel));
        OnPropertyChanged(nameof(ChatLevel));
        OnPropertyChanged(nameof(GameLevelText));
        OnPropertyChanged(nameof(ChatLevelText));
        OnPropertyChanged(nameof(MixText));
        ScheduleSave();
    }

    private void PushGains()
    {
        _router.SetGains(Gains);
        PushParams();
    }

    // ===================================================================== knob

    public static string TargetName(KnobTarget t) => t switch
    {
        KnobTarget.Mix => Loc.Get("Target.Mix"),
        KnobTarget.Volume => Loc.Get("Target.Volume"),
        KnobTarget.Game => Loc.Get("Target.Game"),
        KnobTarget.Chat => Loc.Get("Target.Chat"),
        _ => t.ToString(),
    };

    public string KnobTargetText => TargetName(Settings.KnobTarget);
    public string KnobHotkeyText => KnobController.ChordName(Settings.KnobHotkeyKey, Settings.KnobHotkeyModifiers);
    public KnobTarget KnobTarget => Settings.KnobTarget;

    /// <summary>Default shortcuts for "next knob function", in order of preference.</summary>
    public static readonly (int Vk, HotkeyModifiers Mods)[] ShortcutDefaults =
    {
        (0x4D, HotkeyModifiers.Ctrl | HotkeyModifiers.Alt),   // Ctrl + Alt + M
        (0x4B, HotkeyModifiers.Ctrl | HotkeyModifiers.Alt),   // Ctrl + Alt + K
        (0x52, HotkeyModifiers.Ctrl | HotkeyModifiers.Alt),   // Ctrl + Alt + R
        (0x7B, HotkeyModifiers.Ctrl | HotkeyModifiers.Shift), // Ctrl + Maiusc + F12
        (0x7B, HotkeyModifiers.Ctrl | HotkeyModifiers.Alt),   // Ctrl + Alt + F12
    };

    /// <summary>
    /// A default shortcut that another program has already registered would only half work: move on to the next
    /// free default (a shortcut chosen by the user is never changed).
    /// </summary>
    private void EnsureShortcutUsable()
    {
        var s = Settings;
        if (_shutdown || s.KnobHotkeyCustom || !s.KnobEnabled || s.KnobHotkeyKey == 0) return;
        if (_hook.IsHotkeyActive(s.KnobHotkeyKey, s.KnobHotkeyModifiers)) return;
        int i = Array.FindIndex(ShortcutDefaults, c => c.Vk == s.KnobHotkeyKey && c.Mods == s.KnobHotkeyModifiers);
        if (i < 0 || i + 1 >= ShortcutDefaults.Length) return;
        var old = KnobHotkeyText;
        (s.KnobHotkeyKey, s.KnobHotkeyModifiers) = ShortcutDefaults[i + 1];
        Log.Info($"Shortcut {old} is already used by another program: using {KnobHotkeyText} instead");
        KnobSettingsChanged(); // registers again, and this check runs once more with the result
    }

    /// <summary>How the keys of the knob reach DualMix, for the settings page.</summary>
    public string KeyboardStatus
    {
        get
        {
            if (!Settings.KnobEnabled) return Loc.Get("Knob.Status.Disabled");
            int total = Knob.Bindings.Count;
            var failed = _hook.FailedHotkeys;
            if (failed.Count == 0) return Loc.Format("Knob.Status.AllActive", _hook.ActiveHotkeys);
            return Loc.Format("Knob.Status.SomeFailed", _hook.ActiveHotkeys, total, string.Join(", ", failed));
        }
    }

    private void OnKnobAction(KnobAction a)
    {
        var step = Settings.KnobStep;
        switch (a.Kind)
        {
            case KnobActionKind.Rotate:
            {
                int dir = Settings.KnobInvert ? -a.Direction : a.Direction;
                switch (a.Target)
                {
                    case KnobTarget.Mix:
                        // clockwise = towards Chat, like the GameDAC dial
                        Nudge(dir * step);
                        break;
                    case KnobTarget.Volume:
                        AdjustOutputVolume(dir * step);
                        break;
                    case KnobTarget.Game:
                        SetChannelVolume(Channel.Game, Settings.GameVolume + dir * step, true);
                        break;
                    case KnobTarget.Chat:
                        SetChannelVolume(Channel.Chat, Settings.ChatVolume + dir * step, true);
                        break;
                }
                break;
            }
            case KnobActionKind.Press:
                OnKnobPress(a.Target);
                break;
            case KnobActionKind.CycleFunction:
                _undoPress = null;
                CycleKnobTarget();
                break;
            case KnobActionKind.MixTowardsGame:
                Nudge(-step);
                break;
            case KnobActionKind.MixTowardsChat:
                Nudge(step);
                break;
            case KnobActionKind.MixReset:
                ResetMix(true);
                break;
        }
    }

    /// <summary>
    /// A press of the knob: it acts at once (mute; on the mix: mute everything, or balanced mix when
    /// <see cref="Settings.KnobMixPressMutes"/> is off). A second press right after it means "double press": the
    /// first one is taken back and the knob switches to its next function.
    /// </summary>
    private void OnKnobPress(KnobTarget target)
    {
        long now = Environment.TickCount64;
        if (Settings.KnobDoublePressCycles && _undoPress != null && now - _lastPressAt <= 450)
        {
            var undo = _undoPress;
            _undoPress = null;
            _lastPressAt = 0;
            undo();
            CycleKnobTarget();
            return;
        }
        _lastPressAt = now;

        switch (target)
        {
            case KnobTarget.Mix when !Settings.KnobMixPressMutes:
            {
                var before = Settings.Mix;
                ResetMix(true);
                _undoPress = () => SetMix(before, false);
                break;
            }
            case KnobTarget.Mix: // one press mutes everything, exactly like on "Volume generale"
            case KnobTarget.Volume:
                if (!_outputVolume.IsAvailable) { _undoPress = null; break; }
                OutputMuted = !OutputMuted;
                Log.Info($"Knob: output {(OutputMuted ? "muted" : "unmuted")}");
                ShowVolumeOsd();
                _undoPress = () => OutputMuted = !OutputMuted;
                break;
            case KnobTarget.Game:
                GameMuted = !GameMuted;
                ShowChannelOsd(Channel.Game);
                _undoPress = () => GameMuted = !GameMuted;
                break;
            case KnobTarget.Chat:
                ChatMuted = !ChatMuted;
                ShowChannelOsd(Channel.Chat);
                _undoPress = () => ChatMuted = !ChatMuted;
                break;
        }
    }

    public void CycleKnobTarget()
    {
        var cycle = Settings.KnobCycle.Count > 0 ? Settings.KnobCycle : new List<KnobTarget> { KnobTarget.Mix, KnobTarget.Volume };
        int i = cycle.IndexOf(Settings.KnobTarget);
        var next = cycle[(i + 1) % cycle.Count];
        SetKnobTarget(next, true);
    }

    public void SetKnobTarget(KnobTarget target, bool showOsd)
    {
        Settings.KnobTarget = target;
        Knob.UpdateConfig(KnobConfig.From(Settings));
        OnPropertyChanged(nameof(KnobTargetText));
        OnPropertyChanged(nameof(KnobTarget));
        ScheduleSave();
        Log.Info("Knob function: " + target);
        if (showOsd) OsdRequested?.Invoke(new OsdInfo(OsdKind.Function, Loc.Get("Common.Wheel"), TargetName(target), 0));
    }

    /// <summary>Master volume from DualMix itself (the function "Volume" normally leaves the keys to Windows).</summary>
    private void AdjustOutputVolume(double delta)
    {
        if (!_outputVolume.IsAvailable) return;
        var v = Math.Clamp(Math.Round((_outputVolume.Level + delta) * 100) / 100, 0, 1);
        _outputVolume.Level = v;
        if (delta > 0 && _outputVolume.Muted) _outputVolume.Muted = false;
        MirrorOutputToWindows();
        OnPropertyChanged(nameof(OutputVolumePercent));
        OnPropertyChanged(nameof(OutputMuted));
        Log.Info($"Knob: output volume {v:P0}");
        ShowVolumeOsd();
    }

    private void ShowMixOsd()
    {
        var text = Math.Abs(Settings.Mix) < 0.001
            ? Loc.Get("Common.Balanced")
            : $"Game {GameLevelText} · Chat {ChatLevelText}";
        OsdRequested?.Invoke(new OsdInfo(OsdKind.Mix, Loc.Get("Common.Mix"), text, Settings.Mix));
    }

    private void ShowVolumeOsd()
    {
        var muted = _outputVolume.Muted;
        OsdRequested?.Invoke(new OsdInfo(OsdKind.Volume, Loc.Get("Common.Volume"), muted ? Loc.Get("Common.Muted") : MixMath.Percent(_outputVolume.Level), _outputVolume.Level, muted));
    }

    private void ShowChannelOsd(Channel ch)
    {
        bool game = ch == Channel.Game;
        var vol = game ? Settings.GameVolume : Settings.ChatVolume;
        var muted = game ? Settings.GameMuted : Settings.ChatMuted;
        OsdRequested?.Invoke(new OsdInfo(game ? OsdKind.Game : OsdKind.Chat, game ? Loc.Get("Common.Game") : Loc.Get("Common.Chat"),
            muted ? Loc.Get("Common.Muted") : MixMath.Percent(vol), vol, muted));
    }

    /// <summary>React to keys sent by other programs as well (see <see cref="Settings.AcceptInjectedKeys"/>).</summary>
    public bool AcceptInjectedKeys
    {
        get => _hook.AcceptInjected;
        set => _hook.AcceptInjected = value;
    }

    public void KnobSettingsChanged()
    {
        Knob.UpdateConfig(KnobConfig.From(Settings));
        OnPropertyChanged(nameof(KnobHotkeyText));
        OnPropertyChanged(nameof(KnobTargetText));
        OnPropertyChanged(nameof(KeyboardStatus));
        ScheduleSave();
    }

    // ===================================================================== apps / rules

    public void SetAppChannel(string key, Channel? channel)
    {
        if (!Settings.Apps.TryGetValue(key, out var rule))
        {
            rule = new AppRule();
            Settings.Apps[key] = rule;
        }
        rule.Channel = channel;
        RulesChanged();
    }

    public void SetAppVolume(string key, double volume)
    {
        if (!Settings.Apps.TryGetValue(key, out var rule))
        {
            rule = new AppRule();
            Settings.Apps[key] = rule;
        }
        rule.Volume = Math.Clamp(volume, 0, 1);
        RulesChanged(raiseApps: false);
    }

    public void ForgetApp(string key)
    {
        if (Settings.Apps.Remove(key)) RulesChanged();
    }

    public void RulesChanged(bool raiseApps = true)
    {
        _rules = new RuleSet(Settings);
        PushParams();
        ScheduleSave();
        if (raiseApps) AppsChanged?.Invoke();
    }

    // ===================================================================== engine

    /// <summary>Called by the UI after any device/engine setting changed.</summary>
    public void EngineSettingsChanged(bool restartRouter = false)
    {
        if (restartRouter) _router.Stop();
        _configuredEndpoints = false;
        _routerFailures = 0;
        Evaluate();
        ScheduleSave();
    }

    /// <summary>
    /// Looks at the devices and decides how to run: virtual devices when a cable and the real output are healthy,
    /// otherwise per-app volumes with every Windows setting restored (fail-open: sound never stops because of
    /// DualMix).
    /// </summary>
    public void Evaluate()
    {
        if (_shutdown) return;
        _devicesChangedAt = -1;
        var s = Settings;
        var render = Devices.List(DataFlow.Render);

        // Real output: user choice, or Windows active playback / best analog device on any PC.
        DeviceInfo? output = null;
        if (s.OutputChosenByUser) output = render.FirstOrDefault(d => d.Id == s.OutputDeviceId && !d.IsVbCable);
        var defaultMultimedia = s.DefaultsBackup?.Multimedia ?? Devices.GetDefaultId(Role.Multimedia);
        output ??= Devices.FindPreferredOutput(render, defaultMultimedia);
        output ??= render.FirstOrDefault(d => d.Id == s.OutputDeviceId && !d.IsVbCable);
        if (output != null && output.Id != s.OutputDeviceId && !s.OutputChosenByUser)
        {
            s.OutputDeviceId = output.Id;
            ScheduleSave();
        }
        _outputName = output?.Name;
        _outputFormat = output != null ? Devices.GetDeviceFormat(output.Id) : null;
        _outputVolume.Attach(output?.Id);

        int? probe = output != null ? Devices.ProbeOpen(output.Id) : null;
        _needsRepair = probe == ClassNotRegistered;

        bool wantVirtual = s.Mode == EngineMode.VirtualDevices;
        var (game, chat) = Devices.FindCables(render, s.GameDeviceId, s.ChatDeviceId);
        bool driverPresent = chat != null || DriverSetup.CablesPresent();

        if (!_dryRun && wantVirtual && driverPresent && output != null && probe == null && (!_configuredEndpoints || _formatChanged))
        {
            _formatChanged = false;
            _configuredEndpoints = true;
            var result = DriverSetup.Configure(s, elevated: false);
            if (result.Changed)
            {
                _router.Stop();
                render = Devices.List(DataFlow.Render);
                (game, chat) = Devices.FindCables(render, s.GameDeviceId, s.ChatDeviceId);
            }
            bool askedRecently = s.ElevatedConfigAt != null && DateTime.Now - s.ElevatedConfigAt.Value < TimeSpan.FromHours(24);
            if (result.Failed && !_elevatedConfigTried && !_setupBusy && !askedRecently)
            {
                // Some endpoint settings need administrator rights: finish the configuration elevated
                // (at most one request per day, never a prompt at every start).
                _elevatedConfigTried = true;
                s.ElevatedConfigAt = DateTime.Now;
                ScheduleSave();
                Log.Info("Endpoint configuration needs elevation");
                _ = _ui.BeginInvoke(async () => await RunSetupAsync(install: false, repair: false));
            }
        }

        bool chatOk = chat != null;
        VirtualCables = Devices.FindBuses(render).Count;
        _needsDriver = !chatOk;
        _gameName = game?.Name;
        _chatName = chat?.Name;

        if (wantVirtual && chatOk && output != null && probe == null && !_router.Blocked)
        {
            s.ChatDeviceId = chat!.Id;
            s.GameDeviceId = game?.Id;
            // The Windows volume of a virtual device is the channel volume, but only while apps really play on it.
            // "DualMix Game" stands in for the real output in the Windows volume interface while it is the default device.
            _masterProxy.Attach(game != null && !s.GameDirect && s.SetWindowsDefaults ? game.Id : null);
            _chatVolume.Attach(chat.Id);
            ActivateVirtual();
        }
        else
        {
            DeactivateVirtual();
        }

        UpdateStatus(wantVirtual, chatOk, output, probe);
        PushGains();
        RaiseEngineProperties();
        EngineChanged?.Invoke();

        if (_router.Blocked)
        {
            SetStatus(StatusSeverity.Error, Loc.Get("Status.Blocked.Title"), Loc.Get("Status.Blocked.Body"));
            _nextEvaluate = Environment.TickCount64 + 60000;
        }
        if (_dryRun) return;

        // A second cable was installed before from a purchased pack and is gone: put it back automatically.
        if (s.FirstRunDone && wantVirtual && chatOk && game == null && !s.GameDirect && !_packTried && !_setupBusy && probe == null)
        {
            _packTried = true;
            var cached = DriverSetup.CachedPack();
            var declined = s.DriverSetupDeclinedAt;
            if (cached != null && (declined == null || DateTime.Now - declined.Value > TimeSpan.FromHours(12)))
                _ = _ui.BeginInvoke(async () => await RunSetupAsync(install: false, repair: false, packPath: cached));
        }

        // Install the driver / repair the audio service automatically, once per start (only after initial setup wizard finishes).
        if (s.FirstRunDone && !_autoSetupTried && !_setupBusy && ((wantVirtual && !chatOk) || _needsRepair))
        {
            _autoSetupTried = true;
            var declined = s.DriverSetupDeclinedAt;
            if (declined == null || DateTime.Now - declined.Value > TimeSpan.FromHours(12))
                _ = RunSetupAsync(install: wantVirtual && !chatOk, repair: _needsRepair);
        }
    }

    private void ActivateVirtual()
    {
        var s = Settings;
        bool wasActive = _virtualActive;
        _virtualActive = true;
        _dual = s.GameDeviceId != null;

        if (!_dryRun) PushCableVolumes();
        _router.SetGains(Gains);

        // The mixer must be running before any audio is sent to the virtual devices, otherwise the first moments
        // after the switch would fall into silence.
        var routerGame = GameViaBus ? s.GameDeviceId : null;
        bool sameDevices = _router.GameId == routerGame && _router.ChatId == s.ChatDeviceId && _router.OutputId == s.OutputDeviceId;
        if (!_router.IsRunning || !sameDevices)
        {
            StartRouter();
            var deadline = Environment.TickCount64 + 1500;
            while (_router.State == RouterState.Starting && Environment.TickCount64 < deadline) System.Threading.Thread.Sleep(15);
        }

        if (!_dryRun)
        {
            if (s.SetWindowsDefaults) ApplyWindowsDefaults();
            else RestoreWindowsDefaults();
            if (!s.RouteAppsToDevices && s.RoutedApps.Count > 0) UnrouteApps();
        }
        PushParams();
        if (!wasActive) Log.Info($"Virtual devices active ({(GameViaBus ? "Game + Chat" : "Chat; Game direct")}) → {OutputName}");
    }

    private void DeactivateVirtual()
    {
        bool wasActive = _virtualActive;
        _virtualActive = false;
        _dual = false;
        _gameAmp = MixMath.Taper(Settings.GameVolume);
        _chatAmp = MixMath.Taper(Settings.ChatVolume);
        PushParams();
        // Give the audio back to the real device first; the mixer stops last, so nothing falls into silence.
        if (wasActive || Settings.RoutedApps.Count > 0)
        {
            try { UnrouteApps().Wait(500); } catch { /* engine stopping */ }
        }
        RestoreWindowsDefaults();
        _router.Stop();
        if (wasActive) Log.Info("Virtual devices inactive (per-app volumes)");
    }

    private void StartRouter()
    {
        var s = Settings;
        try
        {
            _router.SetGains(Gains);
            var profile = LatencyProfile.For(s.Latency) with
            {
                ExtraQueueMs = Math.Clamp(s.LearnedQueueMs, 0, 40),
                ExtraSourceMs = Math.Clamp(s.LearnedSourceMs, 0, 20),
            };
            _router.Start(GameViaBus ? s.GameDeviceId : null, s.ChatDeviceId!, s.OutputDeviceId!, profile);
        }
        catch (Exception ex)
        {
            Log.Error("Router start failed", ex);
        }
    }

    private void UpdateStatus(bool wantVirtual, bool chatOk, DeviceInfo? output, int? probe)
    {
        if (_setupBusy)
        {
            SetStatus(StatusSeverity.Info, Loc.Get("Status.SetupBusy.Title"), Loc.Get("Status.SetupBusy.Body"));
        }
        else if (output == null)
        {
            SetStatus(StatusSeverity.Error, Loc.Get("Status.NoOutput.Title"), Loc.Get("Status.NoOutput.Body"));
        }
        else if (probe == ClassNotRegistered)
        {
            SetStatus(StatusSeverity.Error, Loc.Get("Status.AudioLost.Title"), Loc.Get("Status.CardNoPlay.Body"));
        }
        else if (probe != null)
        {
            SetStatus(StatusSeverity.Error, Loc.Get("Status.OutputUnusable.Title"), Loc.Format("Status.OutputUnusable.Body", output.Name, probe.Value));
        }
        else if (wantVirtual && !chatOk)
        {
            SetStatus(StatusSeverity.Warning, Loc.Get("Status.NoDriver.Title"), Loc.Get("Status.NoDriver.Body"));
        }
        else if (!wantVirtual)
        {
            SetStatus(StatusSeverity.Info, Loc.Get("Status.PerAppMode.Title"), Loc.Format("Status.PerAppMode.Body", output.Name));
        }
        else if (GameViaBus)
        {
            SetStatus(StatusSeverity.Success, Loc.Get("Status.ReadyDual.Title"), Loc.Format("Status.ReadyDual.Body", output.Name));
        }
        else
        {
            SetStatus(StatusSeverity.Success, Loc.Get("Status.ReadySingle.Title"), Loc.Format("Status.ReadySingle.Body", output.Name));
        }
    }

    private void SetStatus(StatusSeverity severity, string title, string message)
    {
        _severity = severity;
        _statusTitle = title;
        _statusMessage = message;
        OnPropertyChanged(nameof(Severity));
        OnPropertyChanged(nameof(StatusTitle));
        OnPropertyChanged(nameof(StatusMessage));
    }

    private void RaiseEngineProperties()
    {
        OnPropertyChanged(nameof(VirtualActive));
        OnPropertyChanged(nameof(VirtualCables));
        OnPropertyChanged(nameof(HasGameBus));
        OnPropertyChanged(nameof(GameViaBus));
        OnPropertyChanged(nameof(NeedsDriver));
        OnPropertyChanged(nameof(NeedsRepair));
        OnPropertyChanged(nameof(DriverInstalled));
        OnPropertyChanged(nameof(SetupBusy));
        OnPropertyChanged(nameof(OutputName));
        OnPropertyChanged(nameof(GameDeviceName));
        OnPropertyChanged(nameof(ChatDeviceName));
        OnPropertyChanged(nameof(FormatText));
        OnPropertyChanged(nameof(LatencyText));
        OnPropertyChanged(nameof(EngineText));
        OnPropertyChanged(nameof(OutputVolumeAvailable));
        OnPropertyChanged(nameof(OutputVolumePercent));
        OnPropertyChanged(nameof(OutputMuted));
    }

    /// <summary>
    /// Runs the elevated helper (UAC prompt): installs the virtual-cable driver and/or restarts the Windows audio
    /// service, then configures the endpoints.
    /// </summary>
    /// <param name="packPath">Optional VB-CABLE A+B pack (zip) to install as the second cable.</param>
    public async Task<SetupResult> RunSetupAsync(bool install, bool repair, string? packPath = null)
    {
        if (_setupBusy) return new SetupResult { Message = Loc.Get("Status.AlreadyRunning") };
        _setupBusy = true;
        UpdateStatus(true, true, null, null);
        RaiseEngineProperties();
        if (repair)
        {
            // Restarting the audio service kills every stream: put Windows back to normal first.
            DeactivateVirtual();
        }
        else
        {
            // The endpoints are about to be reconfigured: release them.
            _router.Stop();
        }

        SetupResult result;
        try
        {
            result = await DriverSetup.RunElevatedAsync(install, repair, packPath);
        }
        finally
        {
            _setupBusy = false;
        }

        if (result.Declined) Settings.DriverSetupDeclinedAt = DateTime.Now;
        else Settings.DriverSetupDeclinedAt = null;
        ScheduleSave();
        _configuredEndpoints = false;
        _routerFailures = 0;
        // Give Windows a moment to publish new endpoints / restart its engine.
        await Task.Delay(1500);
        Evaluate();
        return result;
    }

    // ---- Windows defaults

    /// <summary>
    /// Playback default: the real output (or "DualMix Game" when it exists). Communications default: "DualMix Chat".
    /// The previous defaults are saved and put back when DualMix exits.
    /// </summary>
    private void ApplyWindowsDefaults()
    {
        var s = Settings;
        if (s.ChatDeviceId == null || s.OutputDeviceId == null) return;
        var playback = GameViaBus ? s.GameDeviceId! : s.OutputDeviceId;
        if (s.DefaultsBackup == null)
        {
            var backup = new DefaultDevicesBackup
            {
                Console = Devices.GetDefaultId(Role.Console),
                Multimedia = Devices.GetDefaultId(Role.Multimedia),
                Communications = Devices.GetDefaultId(Role.Communications),
            };
            // Never back up our own virtual devices as "the real default".
            bool IsOurs(string? id) => id != null && (id == s.GameDeviceId || id == s.ChatDeviceId);
            if (IsOurs(backup.Console)) backup.Console = s.OutputDeviceId;
            if (IsOurs(backup.Multimedia)) backup.Multimedia = s.OutputDeviceId;
            if (IsOurs(backup.Communications)) backup.Communications = s.OutputDeviceId;
            s.DefaultsBackup = backup;
            SettingsStore.Save(s);
        }

        bool changed = false;
        if (Devices.GetDefaultId(Role.Multimedia) != playback || Devices.GetDefaultId(Role.Console) != playback)
        {
            Devices.SetDefault(playback, Role.Console);
            Devices.SetDefault(playback, Role.Multimedia);
            changed = true;
        }
        if (Devices.GetDefaultId(Role.Communications) != s.ChatDeviceId)
        {
            Devices.SetDefault(s.ChatDeviceId, Role.Communications);
            changed = true;
        }
        if (changed) Log.Info($"Windows defaults: playback → {Devices.NameOf(playback)}, communications → {Devices.NameOf(s.ChatDeviceId)}");
    }

    private void RestoreWindowsDefaults()
    {
        var b = Settings.DefaultsBackup;
        if (b == null) return;
        // Fall back to the real output DualMix was using when a saved device is gone.
        string? Pick(string? saved) => Devices.IsActive(saved) ? saved : Devices.IsActive(Settings.OutputDeviceId) ? Settings.OutputDeviceId : null;
        var c = Pick(b.Console);
        var m = Pick(b.Multimedia);
        var k = Pick(b.Communications);
        if (c != null && Devices.GetDefaultId(Role.Console) != c) Devices.SetDefault(c, Role.Console);
        if (m != null && Devices.GetDefaultId(Role.Multimedia) != m) Devices.SetDefault(m, Role.Multimedia);
        if (k != null && Devices.GetDefaultId(Role.Communications) != k) Devices.SetDefault(k, Role.Communications);
        Settings.DefaultsBackup = null;
        SettingsStore.Save(Settings);
        Log.Info("Windows default devices restored");
    }

    private Task UnrouteApps()
    {
        var keys = Settings.RoutedApps.ToList();
        var task = _sessions.UnrouteAll(keys);
        _ = task.ContinueWith(t =>
        {
            if (t.IsCompletedSuccessfully) _ui.BeginInvoke(() =>
            {
                foreach (var k in keys) Settings.RoutedApps.Remove(k);
                ScheduleSave();
            });
        }, TaskScheduler.Default);
        return task;
    }

    private SessionParams BuildParams()
    {
        var s = Settings;
        bool dual = _virtualActive && _dual;
        bool viaBus = GameViaBus;
        // With "DualMix Game" as the Windows default, Game apps land there by themselves and "Diretto" apps must be
        // sent to the real output explicitly; otherwise the real output is the default and nothing needs routing
        // except Chat.
        bool gameIsDefault = viaBus && s.SetWindowsDefaults;
        return new SessionParams(
            Gains,
            _rules,
            RouteDevices: _virtualActive && s.RouteAppsToDevices && !_dryRun && !_shutdown,
            GameRouteId: viaBus && !gameIsDefault ? s.GameDeviceId : null,
            ChatRouteId: _virtualActive ? s.ChatDeviceId : null,
            DirectRouteId: gameIsDefault ? s.OutputDeviceId : null,
            GameBusId: dual ? s.GameDeviceId : null,
            ChatBusId: _virtualActive ? s.ChatDeviceId : null,
            MetersEnabled: _metersEnabled);
    }

    private void PushParams() => _sessions.Update(BuildParams());

    // ===================================================================== periodic work

    private void Tick()
    {
        if (_shutdown) return;
        var now = Environment.TickCount64;

        // Engine events (new apps, adopted volumes, routed apps)
        bool rulesChanged = false;
        while (_sessions.TryDequeueEvent(out var ev))
        {
            switch (ev)
            {
                case AppDiscovered d:
                    if (!Settings.Apps.TryGetValue(d.Key, out var rule))
                    {
                        Settings.Apps[d.Key] = new AppRule { Volume = d.Volume, DisplayName = d.DisplayName, ExePath = d.ExePath, LastSeen = DateTime.Now };
                        rulesChanged = true;
                    }
                    else
                    {
                        rule.DisplayName = d.DisplayName;
                        rule.ExePath = d.ExePath ?? rule.ExePath;
                        rule.LastSeen = DateTime.Now;
                    }
                    break;
                case AppVolumeAdopted a:
                    if (!Settings.Apps.TryGetValue(a.Key, out var r2))
                    {
                        r2 = new AppRule();
                        Settings.Apps[a.Key] = r2;
                    }
                    r2.Volume = a.Volume;
                    rulesChanged = true;
                    break;
                case AppRouted rt:
                    if (!Settings.RoutedApps.Contains(rt.Key, StringComparer.OrdinalIgnoreCase))
                    {
                        Settings.RoutedApps.Add(rt.Key);
                        SettingsStore.Save(Settings); // persist right away: needed for crash recovery
                    }
                    break;
            }
        }
        if (rulesChanged) RulesChanged();

        // Device changes (debounced)
        if (_devicesChangedAt >= 0 && now - _devicesChangedAt > 800 && !_setupBusy)
        {
            Log.Info("Audio devices changed: re-evaluating");
            Evaluate();
        }

        // Someone moved the Windows defaults (e.g. a headset was plugged in): the output must stay where it is.
        if (_defaultsDrifted && !_setupBusy)
        {
            _defaultsDrifted = false;
            if (_virtualActive && Settings.SetWindowsDefaults) ApplyWindowsDefaults();
        }

        // Router supervision
        if (_virtualActive && !_setupBusy)
        {
            bool hung = _router.State == RouterState.Starting && now - _router.StartedAt > 6000;
            if (_router.State == RouterState.Error || hung || (!_router.IsRunning && _router.State != RouterState.Starting))
            {
                _routerFailures++;
                var reason = hung ? Loc.Get("Status.RouterHungReason") : _router.LastError ?? Loc.Get("Status.RouterStoppedReason");
                Log.Warn($"Router failure #{_routerFailures}: {reason}");
                if (hung || _router.LastErrorCode == ClassNotRegistered || _routerFailures >= 3)
                {
                    // Fail-open: give the apps back to the real device while we wait.
                    if (hung) Log.Error($"Router start is hanging at '{_router.Stage}'");
                    DeactivateVirtual();
                    if (_router.Blocked)
                    {
                        Evaluate(); // shows the "blocked" status and schedules a re-check
                    }
                    else
                    {
                        SetStatus(StatusSeverity.Warning, Loc.Get("Status.VirtualPaused.Title"),
                            Loc.Format("Status.VirtualPaused.Problem", reason));
                        _nextEvaluate = now + Math.Min(60000, 5000L << Math.Min(4, _routerFailures - 1));
                        RaiseEngineProperties();
                        EngineChanged?.Invoke();
                    }
                }
                else
                {
                    StartRouter();
                }
            }
            else if (_router.State == RouterState.Running && now - _router.StartedAt > 30000)
            {
                _routerFailures = 0;
            }
        }
        else if (_nextEvaluate > 0 && now >= _nextEvaluate && !_setupBusy)
        {
            _nextEvaluate = 0;
            Evaluate();
        }

        // Dropouts are worth knowing about (at most one line every 10 s), with their cause
        if (RouterRunning)
        {
            long u = _router.Underruns;
            if (u < _underrunsSeen) _underrunsSeen = u; // router restarted
            if (u > _underrunsSeen && now >= _nextUnderrunLog)
            {
                Log.Warn($"Router: {u - _underrunsSeen} dropout(s) (total {u}: {_router.DropoutDetail})");
                _underrunsSeen = u;
                _nextUnderrunLog = now + 10_000;
            }
            var note = _router.TakeNote();
            if (note != null)
            {
                Log.Info("Router: " + note);
                // Remember what this PC needs, so the next start begins with it.
                if (_router.LearnedQueueMs > Settings.LearnedQueueMs + 0.5 || _router.LearnedSourceMs > Settings.LearnedSourceMs + 0.5)
                {
                    Settings.LearnedQueueMs = Math.Max(Settings.LearnedQueueMs, Math.Round(_router.LearnedQueueMs, 1));
                    Settings.LearnedSourceMs = Math.Max(Settings.LearnedSourceMs, Math.Round(_router.LearnedSourceMs, 1));
                    ScheduleSave();
                    OnPropertyChanged(nameof(LatencyText));
                }
            }
        }

        // Health line in the log every 10 minutes while audio is flowing
        if (RouterRunning && !_router.Suspended && now >= _nextStatsLog)
        {
            _nextStatsLog = now + 600_000;
            Log.Info($"Router ok: clock correction chat {_router.ChatDriftPpm:+0.0;-0.0} ppm" +
                     (GameViaBus ? $", game {_router.GameDriftPpm:+0.0;-0.0} ppm" : "") +
                     $", underruns {_router.Underruns}, added latency {_router.LatencyMs:0} ms");
        }

        // Debounced save
        if (_saveScheduled && DateTime.Now >= _saveDue)
        {
            _saveScheduled = false;
            SettingsStore.Save(Settings);
        }
    }

    public void ScheduleSave()
    {
        _saveScheduled = true;
        _saveDue = DateTime.Now.AddSeconds(1.5);
    }

    public void RescanNow() => _sessions.RequestRescan();

    /// <summary>Formats/latency text refresh for the UI (router state changes asynchronously).</summary>
    public void RefreshEngineText()
    {
        OnPropertyChanged(nameof(FormatText));
        OnPropertyChanged(nameof(LatencyText));
        OnPropertyChanged(nameof(EngineText));
        OnPropertyChanged(nameof(TechnicalDetails));
    }

    private void OnPropertyChanged([CallerMemberName] string? name = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));

    // ===================================================================== device notifications

    private sealed class DeviceNotifications : IMMNotificationClient
    {
        private static readonly Guid DeviceFormatKey = new("f19f064d-082c-4e27-bc73-6882a1bb8e4c");
        private readonly Controller _c;

        public DeviceNotifications(Controller c) => _c = c;

        private void Changed() => _c._ui.BeginInvoke(() => { if (_c._devicesChangedAt < 0) _c._devicesChangedAt = Environment.TickCount64; });

        public void OnDeviceStateChanged(string deviceId, DeviceState newState) => Changed();
        public void OnDeviceAdded(string pwstrDeviceId) => Changed();
        public void OnDeviceRemoved(string deviceId) => Changed();

        public void OnDefaultDeviceChanged(DataFlow flow, Role role, string defaultDeviceId)
        {
            if (flow != DataFlow.Render) return;
            _c._ui.BeginInvoke(() =>
            {
                var s = _c.Settings;
                if (!_c._virtualActive) return;
                var expected = role == Role.Communications ? s.ChatDeviceId : _c.GameViaBus ? s.GameDeviceId : s.OutputDeviceId;
                if (!string.Equals(defaultDeviceId, expected, StringComparison.OrdinalIgnoreCase)) _c._defaultsDrifted = true;
            });
        }

        public void OnPropertyValueChanged(string pwstrDeviceId, PropertyKey key)
        {
            if (key.formatId != DeviceFormatKey) return;
            _c._ui.BeginInvoke(() =>
            {
                if (pwstrDeviceId == _c.Settings.OutputDeviceId)
                {
                    _c._formatChanged = true;
                    Changed();
                }
            });
        }
    }
}
