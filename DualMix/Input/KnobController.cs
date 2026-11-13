using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using DualMix.Core;

namespace DualMix.Input;

public enum KnobActionKind
{
    /// <summary>Knob turned (Volume + / Volume −).</summary>
    Rotate,

    /// <summary>Knob pressed (Mute).</summary>
    Press,

    /// <summary>Switch the knob to its next function.</summary>
    CycleFunction,

    MixTowardsGame,
    MixTowardsChat,
    MixReset,
}

/// <param name="Direction">+1 clockwise (Volume +), −1 counter-clockwise.</param>
/// <param name="Target">Function the action applies to.</param>
public readonly record struct KnobAction(KnobActionKind Kind, int Direction = 0, KnobTarget Target = KnobTarget.Mix);

/// <summary>Immutable copy of the knob settings.</summary>
public sealed record KnobConfig(
    bool Enabled,
    KnobTarget Target,
    bool CtrlAlternate,
    bool CtrlPressCycles,
    bool DoublePressCycles,
    int HotkeyKey,
    HotkeyModifiers HotkeyModifiers,
    int KeyGame,
    int KeyChat,
    int KeyReset)
{
    public static KnobConfig From(Settings s) =>
        new(s.KnobEnabled, s.KnobTarget, s.KnobCtrlAlternate, s.KnobCtrlPressCycles, s.KnobDoublePressCycles, s.KnobHotkeyKey, s.KnobHotkeyModifiers,
            s.KeyMixGame, s.KeyMixChat, s.KeyMixReset);

    /// <summary>Function used while Ctrl is held: Volume when the knob is on the mix, the mix otherwise.</summary>
    public KnobTarget Alternate => Target == KnobTarget.Mix ? KnobTarget.Volume : KnobTarget.Mix;
}

/// <summary>A key plus modifiers, as captured from the keyboard.</summary>
public readonly record struct KeyChord(int Key, HotkeyModifiers Modifiers)
{
    public override string ToString() => KnobController.ChordName(Key, Modifiers);
}

/// <summary>One key combination and what it does.</summary>
public readonly record struct KnobBinding(int Vk, HotkeyModifiers Mods, KnobAction Action, bool NoRepeat);

/// <summary>
/// Decides what each key of the knob does. The NuPhy Kick75 knob sends the standard media keys Volume + / Volume − /
/// Mute; from the settings this class builds the list of key combinations DualMix takes over:
/// <list type="bullet">
/// <item>function "Volume": the rotation is left to Windows (its own volume flyout appears, and DualMix keeps the
/// real output in step with it);</item>
/// <item>any other function: the plain keys drive that function, and with Ctrl held the alternate one;</item>
/// <item>switching function: Ctrl + press of the knob, and a keyboard shortcut.</item>
/// </list>
/// The combinations are registered as system hot keys (see <see cref="KeyboardHook"/>); the low-level hook only
/// handles the ones Windows could not register.
/// </summary>
public sealed class KnobController
{
    private readonly Action<KnobAction> _dispatch;
    private readonly HashSet<int> _swallowedDown = new();
    private volatile KnobConfig _config;
    private volatile KnobBinding[] _bindings;
    private KeyboardHook? _keyboard;
    private int _logged;

    /// <summary>Time (Environment.TickCount64) a knob key was last handled here (hot key or hook).</summary>
    public long LastHandledTick => System.Threading.Interlocked.Read(ref _lastHandled);
    private long _lastHandled;

    /// <summary>When set, the next non-modifier key (with its modifiers) is reported here instead of being processed.</summary>
    public volatile Action<KeyChord>? CaptureNextKey;

    /// <summary>Raised (keyboard thread) for every knob key, used by the "Prova" panel.</summary>
    public event Action<string>? KeySeen;

    public KnobController(KnobConfig config, Action<KnobAction> dispatch)
    {
        _config = config;
        _dispatch = dispatch;
        _bindings = Build(config);
    }

    public IReadOnlyList<KnobBinding> Bindings => _bindings;

    /// <summary>Connects the keyboard thread that registers the hot keys.</summary>
    public void Attach(KeyboardHook keyboard)
    {
        _keyboard = keyboard;
        Apply();
    }

    public void UpdateConfig(KnobConfig config)
    {
        _config = config;
        _bindings = Build(config);
        Apply();
    }

    private void Apply()
    {
        var b = _bindings;
        var specs = new HotkeySpec[b.Length];
        for (int i = 0; i < b.Length; i++) specs[i] = new HotkeySpec(i + 1, b[i].Vk, b[i].Mods, b[i].NoRepeat);
        _keyboard?.SetHotkeys(specs);
    }

    /// <summary>The key combinations DualMix takes over for a configuration.</summary>
    public static KnobBinding[] Build(KnobConfig cfg)
    {
        var list = new List<KnobBinding>();
        void Add(int vk, HotkeyModifiers mods, KnobAction action, bool noRepeat = false)
        {
            if (vk == 0 || list.Any(x => x.Vk == vk && x.Mods == mods)) return;
            list.Add(new KnobBinding(vk, mods, action, noRepeat));
        }

        if (cfg.Enabled)
        {
            var main = cfg.Target;
            if (main != KnobTarget.Volume)
            {
                // Function "Volume" registers nothing: turning and pressing are plain Windows volume and mute,
                // with the Windows flyout (a double press is still recognised, see HandleKey).
                Add(KeyboardHook.VK_VOLUME_UP, HotkeyModifiers.None, new KnobAction(KnobActionKind.Rotate, +1, main));
                Add(KeyboardHook.VK_VOLUME_DOWN, HotkeyModifiers.None, new KnobAction(KnobActionKind.Rotate, -1, main));
                Add(KeyboardHook.VK_VOLUME_MUTE, HotkeyModifiers.None, new KnobAction(KnobActionKind.Press, 0, main), noRepeat: true);
            }

            if (cfg.CtrlAlternate && cfg.Alternate != KnobTarget.Volume)
            {
                // (When the alternate function is Volume nothing is registered: Ctrl + knob is then plain Windows volume.)
                Add(KeyboardHook.VK_VOLUME_UP, HotkeyModifiers.Ctrl, new KnobAction(KnobActionKind.Rotate, +1, cfg.Alternate));
                Add(KeyboardHook.VK_VOLUME_DOWN, HotkeyModifiers.Ctrl, new KnobAction(KnobActionKind.Rotate, -1, cfg.Alternate));
            }
            if (cfg.CtrlPressCycles)
                Add(KeyboardHook.VK_VOLUME_MUTE, HotkeyModifiers.Ctrl, new KnobAction(KnobActionKind.CycleFunction), noRepeat: true);
            Add(cfg.HotkeyKey, cfg.HotkeyModifiers, new KnobAction(KnobActionKind.CycleFunction), noRepeat: true);
        }

        // Dedicated keys (e.g. F13–F15 assigned with VIA) work whatever the knob does.
        Add(cfg.KeyGame, HotkeyModifiers.None, new KnobAction(KnobActionKind.MixTowardsGame));
        Add(cfg.KeyChat, HotkeyModifiers.None, new KnobAction(KnobActionKind.MixTowardsChat));
        Add(cfg.KeyReset, HotkeyModifiers.None, new KnobAction(KnobActionKind.MixReset), noRepeat: true);
        return list.ToArray();
    }

    /// <summary>A registered system hot key was pressed (keyboard thread).</summary>
    public void HandleHotkey(int id)
    {
        var b = _bindings;
        if (id < 1 || id > b.Length) return;
        var binding = b[id - 1];
        System.Threading.Interlocked.Exchange(ref _lastHandled, Environment.TickCount64);
        Note(binding.Vk, binding.Mods, "tasto di sistema");
        _dispatch(binding.Action);
    }

    /// <summary>Low-level hook (keyboard thread). Returns true to swallow the key.</summary>
    public bool HandleKey(int vk, bool down, bool injected)
    {
        // Ignore synthetic input (other tools) unless the user asked for it.
        if (injected) return false;

        var capture = CaptureNextKey;
        if (capture != null && down && !IsModifierKey(vk))
        {
            CaptureNextKey = null;
            lock (_swallowedDown) _swallowedDown.Add(vk);
            capture(new KeyChord(vk, CurrentModifiers()));
            return true;
        }

        // A key whose "down" we swallowed must have its "up" swallowed as well.
        if (!down)
        {
            lock (_swallowedDown)
            {
                if (_swallowedDown.Remove(vk)) return true;
            }
            return false;
        }

        bool knobKey = vk is KeyboardHook.VK_VOLUME_UP or KeyboardHook.VK_VOLUME_DOWN or KeyboardHook.VK_VOLUME_MUTE;
        var mods = knobKey || _bindings.Length > 0 ? CurrentModifiers() : HotkeyModifiers.None;

        KnobBinding? match = null;
        foreach (var b in _bindings)
        {
            if (b.Vk == vk && b.Mods == mods) { match = b; break; }
        }

        if (match == null)
        {
            if (knobKey) Note(vk, mods, Loc.Get("Knob.Key.LeftToWin"));
            return false;
        }

        // Windows delivers it to us as a hot key a moment later: nothing to do here.
        if (_keyboard != null && _keyboard.IsHotkeyActive(vk, mods)) return false;

        // Not registered (taken by another program, or no keyboard thread): handle it from the hook.
        System.Threading.Interlocked.Exchange(ref _lastHandled, Environment.TickCount64);
        Note(vk, mods, Loc.Get("Knob.Key.Hooked"));
        lock (_swallowedDown) _swallowedDown.Add(vk);
        _dispatch(match.Value.Action);
        return true;
    }

    private void Note(int vk, HotkeyModifiers mods, string how)
    {
        string what = vk switch
        {
            KeyboardHook.VK_VOLUME_UP => Loc.Get("Knob.Key.VolUp"),
            KeyboardHook.VK_VOLUME_DOWN => Loc.Get("Knob.Key.VolDown"),
            KeyboardHook.VK_VOLUME_MUTE => Loc.Get("Knob.Key.VolMute"),
            _ => KeyName(vk),
        };
        if (mods != HotkeyModifiers.None) what = ChordName(0, mods) + " + " + what;
        // The first events of every run go to the log: they show how the keyboard really reaches DualMix.
        if (_logged < 12)
        {
            _logged++;
            Log.Info($"Key: {what} ({how})");
        }
        try { KeySeen?.Invoke($"{what} · {how}"); } catch { /* UI closing */ }
    }

    public static HotkeyModifiers CurrentModifiers()
    {
        var m = HotkeyModifiers.None;
        if (KeyboardHook.IsDown(0x11)) m |= HotkeyModifiers.Ctrl;
        if (KeyboardHook.IsDown(0x12)) m |= HotkeyModifiers.Alt;
        if (KeyboardHook.IsDown(0x10)) m |= HotkeyModifiers.Shift;
        if (KeyboardHook.IsDown(0x5B) || KeyboardHook.IsDown(0x5C)) m |= HotkeyModifiers.Win;
        return m;
    }

    private static bool IsModifierKey(int vk) => vk is 0x10 or 0x11 or 0x12 or 0x5B or 0x5C or 0xA0 or 0xA1 or 0xA2 or 0xA3 or 0xA4 or 0xA5;

    /// <summary>Keys that make sense as a shortcut on their own (pressing them while typing does no harm).</summary>
    public static bool UsableAlone(int vk) =>
        (vk >= 0x70 && vk <= 0x87)      // F1–F24
        || vk is 0x13 or 0x91 or 0x2C   // Pause, Scroll Lock, Print Screen
        || vk is 0x2D                   // Insert
        || (vk >= 0xA6 && vk <= 0xB7);  // browser / media / launch keys

    public static string ChordName(int vk, HotkeyModifiers mods)
    {
        var sb = new StringBuilder();
        if (mods.HasFlag(HotkeyModifiers.Ctrl)) sb.Append("Ctrl + ");
        if (mods.HasFlag(HotkeyModifiers.Alt)) sb.Append("Alt + ");
        if (mods.HasFlag(HotkeyModifiers.Shift)) sb.Append(Loc.Get("Knob.Key.Shift") + " + ");
        if (mods.HasFlag(HotkeyModifiers.Win)) sb.Append("Win + ");
        if (vk == 0) return sb.Length == 0 ? Loc.Get("Knob.Key.NoneFem") : sb.ToString().TrimEnd(' ', '+');
        sb.Append(KeyName(vk));
        return sb.ToString();
    }

    public static string KeyName(int vk)
    {
        if (vk == 0) return Loc.Get("Knob.Key.None");
        if (vk >= 0x70 && vk <= 0x87) return "F" + (vk - 0x6F);
        if (vk >= 0x30 && vk <= 0x39) return ((char)vk).ToString();
        if (vk >= 0x41 && vk <= 0x5A) return ((char)vk).ToString();
        if (vk >= 0x60 && vk <= 0x69) return Loc.Format("Knob.Key.Num", vk - 0x60);
        return vk switch
        {
            KeyboardHook.VK_VOLUME_UP => "Volume +",
            KeyboardHook.VK_VOLUME_DOWN => "Volume −",
            KeyboardHook.VK_VOLUME_MUTE => Loc.Get("Knob.Key.PressMute"),
            0xB0 => Loc.Get("Knob.Key.NextTrack"),
            0xB1 => Loc.Get("Knob.Key.PrevTrack"),
            0xB2 => "Stop",
            0xB3 => Loc.Get("Knob.Key.PlayPause"),
            0x13 => Loc.Get("Knob.Key.Pause"),
            0x91 => Loc.Get("Knob.Key.ScrollLock"),
            0x2C => Loc.Get("Knob.Key.PrtSc"),
            0x2D => "Ins",
            0x2E => Loc.Get("Knob.Key.Del"),
            0x24 => "Home",
            0x23 => Loc.Get("Knob.Key.End"),
            0x21 => Loc.Get("Knob.Key.PgUp"),
            0x22 => Loc.Get("Knob.Key.PgDn"),
            0x20 => Loc.Get("Knob.Key.Space"),
            0x0D => Loc.Get("Knob.Key.Enter"),
            0x1B => "Esc",
            0x09 => "Tab",
            0x08 => "Backspace",
            0x25 => "←",
            0x26 => "↑",
            0x27 => "→",
            0x28 => "↓",
            0xDC => "\\",
            0xBC => ",",
            0xBE => ".",
            0xBD => "-",
            0xBB => "+",
            _ => System.Windows.Input.KeyInterop.KeyFromVirtualKey(vk).ToString(),
        };
    }
}
