using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace DualMix.Core;

/// <summary>Logical audio channel an application belongs to.</summary>
public enum Channel
{
    Game,
    Chat,

    /// <summary>"Diretto": the app is not part of the mix and plays straight to the real output.</summary>
    Ignore,
}

/// <summary>How DualMix separates the two channels.</summary>
public enum EngineMode
{
    /// <summary>No drivers: per-application session volumes are driven by the mix (fallback).</summary>
    PerApp,

    /// <summary>Two virtual playback devices ("DualMix Game" / "DualMix Chat") are mixed into the real output.</summary>
    VirtualDevices,
}

/// <summary>What the knob (Volume +/- media keys) controls.</summary>
public enum KnobTarget
{
    Mix,
    Volume,
    Game,
    Chat,
}

[Flags]
public enum HotkeyModifiers
{
    None = 0,
    Ctrl = 1,
    Alt = 2,
    Shift = 4,
    Win = 8,
}

/// <summary>Function the knob has every time DualMix starts.</summary>
public enum KnobStartFunction
{
    /// <summary>The one it had when DualMix was closed.</summary>
    Last,
    Mix,
    Volume,
    Game,
    Chat,
}

/// <summary>What DualMix does about new versions.</summary>
public enum UpdateMode
{
    /// <summary>Never looks for updates.</summary>
    Off,

    /// <summary>Looks for updates and tells the user; installs only when asked.</summary>
    Notify,

    /// <summary>Installs an update found at start-up by itself; one found later is announced.</summary>
    Auto,
}

public enum LatencyMode
{
    /// <summary>Smallest buffers, for gaming.</summary>
    Minima,
    Bilanciata,

    /// <summary>Largest buffers, for slow or busy systems.</summary>
    Sicura,
}

public enum ThemePreference
{
    System,
    Light,
    Dark,
}

public enum LanguagePreference
{
    /// <summary>Italian when Windows display language is Italian, English everywhere else.</summary>
    Auto,
    English,
    Italian,
}

public sealed class AppRule
{
    /// <summary>Explicit channel, or null for automatic classification.</summary>
    public Channel? Channel { get; set; }

    /// <summary>Relative volume of this app inside its channel (0..1).</summary>
    public double Volume { get; set; } = 1.0;

    /// <summary>Friendly name, cached for the UI.</summary>
    public string? DisplayName { get; set; }

    /// <summary>Full path of the executable, cached for icons.</summary>
    public string? ExePath { get; set; }

    public DateTime LastSeen { get; set; } = DateTime.Now;
}

public sealed class DefaultDevicesBackup
{
    public string? Console { get; set; }
    public string? Multimedia { get; set; }
    public string? Communications { get; set; }
}

public sealed class Settings
{
    public const int CurrentVersion = 2;

    public int Version { get; set; } = CurrentVersion;

    // ---- Engine -------------------------------------------------------------
    public EngineMode Mode { get; set; } = EngineMode.VirtualDevices;

    // ---- Mix ----------------------------------------------------------------
    /// <summary>-1 = all Game, 0 = center, +1 = all Chat.</summary>
    public double Mix { get; set; }

    /// <summary>Channel volumes as shown on the sliders (0..1, perceptual scale).</summary>
    public double GameVolume { get; set; } = 1.0;
    public double ChatVolume { get; set; } = 1.0;
    public bool GameMuted { get; set; }
    public bool ChatMuted { get; set; }

    /// <summary>Level (0..1) the attenuated channel keeps at the extreme of the balance.</summary>
    public double MixFloor { get; set; }

    // ---- Applications -------------------------------------------------------
    public Dictionary<string, AppRule> Apps { get; set; } = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>Channel for apps not in the chat catalog and without explicit rule.</summary>
    public Channel DefaultChannel { get; set; } = Channel.Game;

    /// <summary>Channel for the Windows "System sounds" session.</summary>
    public Channel SystemSoundsChannel { get; set; } = Channel.Game;

    /// <summary>Extra process names (without .exe) to treat as chat apps.</summary>
    public List<string> ExtraChatApps { get; set; } = new();

    // ---- Devices --------------------------------------------------------------
    public string? GameDeviceId { get; set; }
    public string? ChatDeviceId { get; set; }

    /// <summary>Real output. When <see cref="OutputChosenByUser"/> is false DualMix picks the active Windows default or best analog output.</summary>
    public string? OutputDeviceId { get; set; }
    public bool OutputChosenByUser { get; set; }

    /// <summary>
    /// Games play straight on the real output (untouched path, no added latency) even when a virtual Game
    /// device exists. Always the case with a single virtual cable.
    /// </summary>
    public bool GameDirect { get; set; }

    /// <summary>Make Game the Windows default device and Chat the default communication device.</summary>
    public bool SetWindowsDefaults { get; set; } = true;

    /// <summary>Automatically route each app to the Game/Chat virtual device according to its channel.</summary>
    public bool RouteAppsToDevices { get; set; } = true;

    public LatencyMode Latency { get; set; } = LatencyMode.Bilanciata;

    /// <summary>Extra buffering (ms) the audio engine found necessary on this PC after dropouts; applied at every start.</summary>
    public double LearnedQueueMs { get; set; }
    public double LearnedSourceMs { get; set; }

    public DefaultDevicesBackup? DefaultsBackup { get; set; }

    /// <summary>Process names whose per-app output device was changed by DualMix and still needs to be reset.</summary>
    public List<string> RoutedApps { get; set; } = new();

    /// <summary>Last automatic request for administrator rights to finish the endpoint configuration.</summary>
    public DateTime? ElevatedConfigAt { get; set; }

    /// <summary>Last time the user declined the driver installation (UAC "No"); avoids asking at every start.</summary>
    public DateTime? DriverSetupDeclinedAt { get; set; }

    // ---- Knob / keyboard ----------------------------------------------------
    /// <summary>When false DualMix never touches the volume keys (Windows handles them).</summary>
    public bool KnobEnabled { get; set; } = true;

    public KnobTarget KnobTarget { get; set; } = KnobTarget.Volume;

    /// <summary>Function the knob goes back to at every start of DualMix (default: the general volume).</summary>
    public KnobStartFunction KnobStartFunction { get; set; } = KnobStartFunction.Volume;

    /// <summary>
    /// On the function "Mix" a press of the knob mutes/unmutes everything (the real output). Off: it sets the mix
    /// back to balanced, as DualMix did up to 2.1.
    /// </summary>
    public bool KnobMixPressMutes { get; set; } = true;

    /// <summary>Functions the "change knob function" shortcut cycles through.</summary>
    public List<KnobTarget> KnobCycle { get; set; } = new() { KnobTarget.Mix, KnobTarget.Volume };

    /// <summary>Shortcut that switches the knob function (virtual-key + modifiers). Default Ctrl+Alt+M.</summary>
    public int KnobHotkeyKey { get; set; } = 0x4D;
    public HotkeyModifiers KnobHotkeyModifiers { get; set; } = HotkeyModifiers.Ctrl | HotkeyModifiers.Alt;

    /// <summary>True once the user picked the shortcut; until then DualMix may replace a default that another program already uses.</summary>
    public bool KnobHotkeyCustom { get; set; }

    /// <summary>Holding Ctrl while turning uses the alternate function (Mix ↔ Volume).</summary>
    public bool KnobCtrlAlternate { get; set; } = true;

    /// <summary>Ctrl + press of the knob switches to the next function.</summary>
    public bool KnobCtrlPressCycles { get; set; } = true;

    /// <summary>Pressing the knob twice quickly switches to the next function.</summary>
    public bool KnobDoublePressCycles { get; set; } = true;

    public double KnobStep { get; set; } = 0.05;
    public bool KnobInvert { get; set; }

    /// <summary>Also react to keys sent by other programs (AutoHotkey, macro tools). Off: hardware keys only.</summary>
    public bool AcceptInjectedKeys { get; set; }

    /// <summary>Optional dedicated keys (virtual-key codes, 0 = none), e.g. F13/F14 mapped on the knob via VIA.</summary>
    public int KeyMixGame { get; set; }
    public int KeyMixChat { get; set; }
    public int KeyMixReset { get; set; }

    // ---- General --------------------------------------------------------------
    public bool StartWithWindows { get; set; } = true;
    public bool StartMinimized { get; set; } = true;
    public bool ShowOsd { get; set; } = true;
    public bool RestoreVolumesOnExit { get; set; } = true;
    public ThemePreference Theme { get; set; } = ThemePreference.System;
    public LanguagePreference Language { get; set; } = LanguagePreference.Auto;
    public bool FirstRunDone { get; set; }
    public bool TrayHintShown { get; set; }

    // ---- Updates --------------------------------------------------------------
    public UpdateMode UpdateMode { get; set; } = UpdateMode.Auto;

    /// <summary>
    /// Where the version document is: an https address or a folder/file path. Null = the default
    /// (<see cref="Updater.DefaultFeed"/>).
    /// </summary>
    public string? UpdateFeed { get; set; }

    /// <summary>No automatic check before this moment (a day after a check, three days after "not now").</summary>
    public DateTime? UpdateNextCheck { get; set; }

    /// <summary>The function the knob starts with, or null to keep the last one.</summary>
    public KnobTarget? StartTarget => KnobStartFunction switch
    {
        KnobStartFunction.Mix => KnobTarget.Mix,
        KnobStartFunction.Volume => KnobTarget.Volume,
        KnobStartFunction.Game => KnobTarget.Game,
        KnobStartFunction.Chat => KnobTarget.Chat,
        _ => null,
    };

    public Settings Normalize()
    {
        Apps ??= new(StringComparer.OrdinalIgnoreCase);
        if (Apps.Comparer != StringComparer.OrdinalIgnoreCase)
            Apps = new Dictionary<string, AppRule>(Apps, StringComparer.OrdinalIgnoreCase);
        ExtraChatApps ??= new();
        RoutedApps ??= new();
        KnobCycle ??= new();
        KnobCycle = KnobCycle.Distinct().ToList();
        if (KnobCycle.Count == 0) KnobCycle.AddRange(new[] { KnobTarget.Mix, KnobTarget.Volume });
        Mix = Clamp(Mix, -1, 1);
        GameVolume = Clamp(GameVolume, 0, 1);
        ChatVolume = Clamp(ChatVolume, 0, 1);
        MixFloor = Clamp(MixFloor, 0, 0.9);
        KnobStep = Clamp(KnobStep, 0.01, 0.25);
        if (string.IsNullOrWhiteSpace(UpdateFeed)) UpdateFeed = null;
        foreach (var r in Apps.Values) r.Volume = Clamp(r.Volume, 0, 1);
        return this;
    }

    /// <summary>Upgrades a settings file written by an older DualMix.</summary>
    public Settings Migrate()
    {
        if (Version < 2)
        {
            // 1.x: per-app mode by default and a knob that was easy to leave switched off.
            Mode = EngineMode.VirtualDevices;
            KnobEnabled = true;
            KnobTarget = KnobTarget.Mix;
            KnobCycle = new() { KnobTarget.Mix, KnobTarget.Volume };
            KnobHotkeyKey = 0x4D;
            KnobHotkeyModifiers = HotkeyModifiers.Ctrl | HotkeyModifiers.Alt;
            SystemSoundsChannel = Channel.Game;
            OutputChosenByUser = false;
            Version = 2;
            Log.Info("Settings migrated to version 2");
        }
        return this;
    }

    private static double Clamp(double v, double lo, double hi) => double.IsNaN(v) ? lo : Math.Min(hi, Math.Max(lo, v));
}

public static class SettingsStore
{
    private static readonly JsonSerializerOptions Options = new()
    {
        WriteIndented = true,
        Converters = { new JsonStringEnumConverter() },
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Skip,
    };

    private static readonly object Gate = new();

    /// <summary>When true nothing is written (used by the screenshot mode, which must leave no trace).</summary>
    public static bool ReadOnly { get; set; }

    public static Settings Load()
    {
        var path = AppPaths.SettingsFile;
        try
        {
            if (File.Exists(path))
            {
                var json = File.ReadAllText(path);
                var s = JsonSerializer.Deserialize<Settings>(json, Options);
                if (s != null)
                {
                    // Files without "Version" are 1.x files.
                    if (!json.Contains("\"Version\"", StringComparison.Ordinal)) s.Version = 1;
                    return s.Migrate().Normalize();
                }
            }
        }
        catch (Exception ex)
        {
            Log.Error("Settings file unreadable, starting from defaults", ex);
            try { File.Copy(path, path + ".bad", true); } catch { /* ignore */ }
        }
        return new Settings().Normalize();
    }

    public static void Save(Settings settings)
    {
        if (ReadOnly) return;
        lock (Gate)
        {
            try
            {
                var path = AppPaths.SettingsFile;
                var tmp = path + ".tmp";
                File.WriteAllText(tmp, JsonSerializer.Serialize(settings, Options));
                if (File.Exists(path)) File.Replace(tmp, path, null);
                else File.Move(tmp, path);
            }
            catch (Exception ex)
            {
                Log.Error("Could not save settings", ex);
            }
        }
    }
}
