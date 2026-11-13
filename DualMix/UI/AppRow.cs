using System;
using System.Collections.Concurrent;
using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using DualMix.Audio;
using DualMix.Core;

namespace DualMix.UI;

/// <summary>One application in the App page list.</summary>
public sealed class AppRow : INotifyPropertyChanged
{
    private static readonly ConcurrentDictionary<string, ImageSource?> IconCache = new(StringComparer.OrdinalIgnoreCase);

    private readonly Controller _controller;
    private Channel _channel;
    private Channel _autoChannel;
    private bool _explicit;
    private double _volume;
    private bool _active;
    private bool _fixedOutput;
    private double _peak;
    private string _displayName;

    public string Key { get; }
    public string? ExePath { get; private set; }
    public bool IsSystem { get; }

    public AppRow(Controller controller, string key, string displayName, string? exePath, bool isSystem)
    {
        _controller = controller;
        Key = key;
        _displayName = displayName;
        ExePath = exePath;
        IsSystem = isSystem;
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    public string DisplayName
    {
        get => IsSystem ? Loc.Get("Apps.SystemSounds") : _displayName;
        private set => Set(ref _displayName, value);
    }

    public ImageSource? Icon => ExePath == null ? null : IconCache.GetOrAdd(ExePath, LoadIcon);
    public bool HasIcon => Icon != null;
    public string Initial => string.IsNullOrEmpty(DisplayName) ? "?" : DisplayName[..1].ToUpperInvariant();

    public string Subtitle
    {
        get
        {
            var where = _channel switch
            {
                Channel.Game => Loc.Get("Common.Game"),
                Channel.Chat => Loc.Get("Common.Chat"),
                _ => Loc.Get("AppRow.DirectSub"),
            };
            var how = _explicit ? Loc.Get("AppRow.Manual") : Loc.Get("AppRow.Auto");
            var state = !_active ? Loc.Get("AppRow.Inactive") : _fixedOutput ? Loc.Get("AppRow.FixedOutput") : "";
            return IsSystem ? $"{where}{state}" : $"{where} · {how}{state}";
        }
    }

    public bool IsGame
    {
        get => _channel == Channel.Game;
        set { if (value) Choose(Channel.Game); }
    }

    public bool IsChat
    {
        get => _channel == Channel.Chat;
        set { if (value) Choose(Channel.Chat); }
    }

    public bool IsDirect
    {
        get => _channel == Channel.Ignore;
        set { if (value) Choose(Channel.Ignore); }
    }

    public bool IsExplicit => _explicit;

    /// <summary>Explains the state of the row (shown as a tooltip).</summary>
    public string Hint => _fixedOutput ? Loc.Get("AppRow.FixedHint") : Key;
    public bool Active => _active;
    public double Opacity => _active ? 1.0 : 0.6;

    /// <summary>App volume inside its channel, 0..100.</summary>
    public double VolumePercent
    {
        get => Math.Round(_volume * 100);
        set
        {
            var v = Math.Clamp(value / 100.0, 0, 1);
            if (Math.Abs(v - _volume) < 0.0005) return;
            _volume = v;
            _controller.SetAppVolume(Key, v);
            OnChanged();
        }
    }

    public double Peak
    {
        get => _peak;
        private set => Set(ref _peak, value);
    }

    private void Choose(Channel channel)
    {
        // Choosing the channel the app would get anyway keeps it "automatic".
        Channel? rule = IsSystem ? channel : channel == _autoChannel ? null : channel;
        if (IsSystem)
        {
            _controller.Settings.SystemSoundsChannel = channel;
            _controller.RulesChanged();
        }
        else
        {
            _controller.SetAppChannel(Key, rule);
        }
        _channel = channel;
        _explicit = rule != null;
        RaiseChannel();
    }

    public void ResetToAutomatic()
    {
        if (IsSystem) return;
        _controller.SetAppChannel(Key, null);
        _channel = _autoChannel;
        _explicit = false;
        RaiseChannel();
    }

    /// <summary>Refresh from a live engine snapshot.</summary>
    public void Update(AudioAppInfo info)
    {
        DisplayName = info.DisplayName;
        if (info.ExePath != null && ExePath == null)
        {
            ExePath = info.ExePath;
            OnChanged(nameof(Icon));
            OnChanged(nameof(HasIcon));
        }
        bool channelChanged = _channel != info.Channel || _explicit != info.IsExplicit || _autoChannel != info.AutoChannel;
        _channel = info.Channel;
        _autoChannel = info.AutoChannel;
        _explicit = info.IsExplicit;
        if (Math.Abs(_volume - info.RelativeVolume) > 0.0005)
        {
            _volume = info.RelativeVolume;
            OnChanged(nameof(VolumePercent));
        }
        if (_fixedOutput != info.FixedOutput)
        {
            _fixedOutput = info.FixedOutput;
            OnChanged(nameof(Subtitle));
            OnChanged(nameof(Hint));
        }
        if (_active != info.Active)
        {
            _active = info.Active;
            OnChanged(nameof(Active));
            OnChanged(nameof(Opacity));
            OnChanged(nameof(Subtitle));
        }
        Peak = info.Peak;
        if (channelChanged) RaiseChannel();
    }

    /// <summary>Refresh from a saved rule (app not currently playing).</summary>
    public void UpdateFromRule(AppRule rule, RuleSet rules)
    {
        _autoChannel = rules.AutoChannel(Key, IsSystem);
        _channel = rule.Channel ?? _autoChannel;
        _explicit = rule.Channel != null;
        _volume = rule.Volume;
        _active = false;
        _fixedOutput = false;
        Peak = 0;
        OnChanged(nameof(VolumePercent));
        OnChanged(nameof(Opacity));
        RaiseChannel();
    }

    private void RaiseChannel()
    {
        OnChanged(nameof(IsGame));
        OnChanged(nameof(IsChat));
        OnChanged(nameof(IsDirect));
        OnChanged(nameof(IsExplicit));
        OnChanged(nameof(Subtitle));
    }

    private static ImageSource? LoadIcon(string path)
    {
        try
        {
            using var icon = System.Drawing.Icon.ExtractAssociatedIcon(path);
            if (icon == null) return null;
            var src = Imaging.CreateBitmapSourceFromHIcon(icon.Handle, Int32Rect.Empty, BitmapSizeOptions.FromWidthAndHeight(32, 32));
            src.Freeze();
            return src;
        }
        catch
        {
            return null;
        }
    }

    private void Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (Equals(field, value)) return;
        field = value;
        OnChanged(name);
    }

    private void OnChanged([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}
