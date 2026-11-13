using System;
using System.ComponentModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Threading;
using DualMix.Core;
using Wpf.Ui.Controls;

namespace DualMix.UI.Pages;

public partial class MixerPage : Page
{
    private readonly Controller _c;
    private readonly DispatcherTimer _meterTimer;
    private double _game, _chat, _out;

    public MixerPage()
    {
        _c = App.Current.Controller;
        DataContext = _c;
        InitializeComponent();

        _meterTimer = new DispatcherTimer(DispatcherPriority.Render) { Interval = TimeSpan.FromMilliseconds(33) };
        _meterTimer.Tick += (_, _) => UpdateMeters();

        Loaded += (_, _) =>
        {
            _c.PropertyChanged += OnChanged;
            _c.EngineChanged += RefreshEngine;
            _meterTimer.Start();
            RefreshEngine();
            RefreshMuteIcons();
        };
        Unloaded += (_, _) =>
        {
            _c.PropertyChanged -= OnChanged;
            _c.EngineChanged -= RefreshEngine;
            _meterTimer.Stop();
        };
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(Controller.GameMuted):
            case nameof(Controller.ChatMuted):
            case nameof(Controller.OutputMuted):
                RefreshMuteIcons();
                break;
            case nameof(Controller.FormatText):
            case nameof(Controller.LatencyText):
            case nameof(Controller.VirtualActive):
            case nameof(Controller.Severity):
                RefreshEngine();
                break;
        }
    }

    private void RefreshMuteIcons()
    {
        GameMuteIcon.Symbol = _c.GameMuted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Speaker224;
        ChatMuteIcon.Symbol = _c.ChatMuted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Speaker224;
        OutMuteIcon.Symbol = _c.OutputMuted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Speaker224;
    }

    private int _engineRefreshCountdown;

    private static string Short(string deviceName)
    {
        int i = deviceName.IndexOf(" (", StringComparison.Ordinal);
        return i > 0 ? deviceName[..i] : deviceName;
    }

    private void RefreshEngine()
    {
        EngineChip.Text = _c.RouterRunning
            ? (_c.RouterIdle ? "Attivo · in attesa di audio" : $"Attivo · {_c.LatencyText}")
            : _c.VirtualActive ? "Avvio…" : _c.EngineText;
        EngineDot.SetResourceReference(System.Windows.Shapes.Shape.FillProperty, _c.Severity switch
        {
            StatusSeverity.Success => "SystemFillColorSuccessBrush",
            StatusSeverity.Warning => "SystemFillColorCautionBrush",
            StatusSeverity.Error => "SystemFillColorCriticalBrush",
            _ => "SystemFillColorAttentionBrush",
        });
        FormatLine.Text = _c.FormatText;
        GameDevice.Text = _c.GameViaBus ? $"Uscita per le app: {Short(_c.GameDeviceName)}"
            : _c.VirtualActive ? "Diretto sulla scheda audio · latenza zero"
            : "Volume per applicazione";
        ChatDevice.Text = _c.VirtualActive ? $"Uscita per le app: {Short(_c.ChatDeviceName)}" : "Volume per applicazione";
    }

    private void UpdateMeters()
    {
        // Peak meters with fast attack / smooth release (≈ 300 ms), on a perceptual (square-root) scale.
        static double Smooth(double current, double target) => target > current ? target : current + (target - current) * 0.18;
        _game = Smooth(_game, Math.Sqrt(Math.Min(1, _c.GamePeak)));
        _chat = Smooth(_chat, Math.Sqrt(Math.Min(1, _c.ChatPeak)));
        _out = Smooth(_out, Math.Sqrt(Math.Min(1, _c.OutputPeak)));
        GameMeterScale.ScaleX = _game;
        ChatMeterScale.ScaleX = _chat;
        OutMeterScale.ScaleX = _out;

        // The router finishes starting asynchronously: refresh the status texts a few times per second.
        if (++_engineRefreshCountdown >= 15)
        {
            _engineRefreshCountdown = 0;
            _c.RefreshEngineText();
        }
    }

    private void AllGame_Click(object sender, RoutedEventArgs e) => _c.SetMix(-1, false);
    private void Center_Click(object sender, RoutedEventArgs e) => _c.ResetMix();
    private void AllChat_Click(object sender, RoutedEventArgs e) => _c.SetMix(1, false);

    private void MixSlider_DoubleClick(object sender, MouseButtonEventArgs e)
    {
        _c.ResetMix();
        e.Handled = true;
    }

    private void KnobSettings_Click(object sender, RoutedEventArgs e)
    {
        if (Window.GetWindow(this) is MainWindow w) w.Navigate(2);
    }
}
