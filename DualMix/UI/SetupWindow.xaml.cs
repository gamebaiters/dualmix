using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using DualMix.Audio;
using DualMix.Core;
using Wpf.Ui.Controls;

namespace DualMix.UI;

public partial class SetupWindow : FluentWindow
{
    private sealed record OutputChoice(string? Id, string Label);

    private readonly Controller _controller;
    private readonly bool _snapshotMode;
    private int _step = 1;
    private bool _loading = true;
    private bool _finished;

    public bool InstallVirtualDriverRequested { get; private set; }

    public SetupWindow(Controller controller, bool snapshotMode = false)
    {
        _controller = controller;
        _snapshotMode = snapshotMode;
        InitializeComponent();
        ThemeService.Watch(this, controller.Settings.Theme);
        LoadState();
        ShowStep(1);
    }

    private void LoadState()
    {
        _loading = true;
        var s = _controller.Settings;

        LangAuto.IsChecked = s.Language == LanguagePreference.Auto;
        LangEn.IsChecked = s.Language == LanguagePreference.English;
        LangIt.IsChecked = s.Language == LanguagePreference.Italian;

        RefreshDevicesAndStatus();

        if (!s.FirstRunDone)
        {
            // Keep existing defaults, or pick VirtualDevices when cables exist / PerApp if not specified.
            ModeVirtual.IsChecked = s.Mode == EngineMode.VirtualDevices;
            ModePerApp.IsChecked = s.Mode == EngineMode.PerApp;
        }
        else
        {
            ModeVirtual.IsChecked = s.Mode == EngineMode.VirtualDevices;
            ModePerApp.IsChecked = s.Mode == EngineMode.PerApp;
        }

        StartWithWindowsCheck.IsChecked = s.FirstRunDone ? AutoStart.IsEnabled : s.StartWithWindows;
        DisableDuckingCheck.IsChecked = !s.FirstRunDone || WindowsDucking.IsDisabled;
        _loading = false;
    }

    private void RefreshDevicesAndStatus()
    {
        var s = _controller.Settings;
        var render = Devices.ListRender().Where(d => !d.IsVbCable).ToList();
        var auto = Devices.FindPreferredOutput(render, s.DefaultsBackup?.Multimedia ?? Devices.GetDefaultId(NAudio.CoreAudioApi.Role.Multimedia));
        var choices = new List<OutputChoice>
        {
            new(null, auto != null ? Loc.Format("Audio.AutoOption", auto.Name) : Loc.Get("Audio.AutoOptionEmpty")),
        };
        choices.AddRange(render.Select(d => new OutputChoice(d.Id, d.Name)));
        OutputCombo.ItemsSource = choices;
        OutputCombo.SelectedItem = s.OutputChosenByUser
            ? choices.FirstOrDefault(c => c.Id == s.OutputDeviceId) ?? choices[0]
            : choices[0];

        bool hasCables = DriverSetup.CablesPresent();
        VbStatusText.Text = hasCables ? Loc.Get("Wizard.S2.VbReady") : Loc.Get("Wizard.S2.VbInstallNeeded");
    }

    public void ShowStep(int step)
    {
        _step = Math.Clamp(step, 1, 3);
        Step1Panel.Visibility = _step == 1 ? Visibility.Visible : Visibility.Collapsed;
        Step2Panel.Visibility = _step == 2 ? Visibility.Visible : Visibility.Collapsed;
        Step3Panel.Visibility = _step == 3 ? Visibility.Visible : Visibility.Collapsed;

        StepLabel.Text = Loc.Format("Wizard.StepIndicator", _step);
        BackButton.Content = _step == 1 ? Loc.Get("Wizard.Skip") : Loc.Get("Wizard.Back");
        NextButton.Content = _step == 3 ? Loc.Get("Wizard.Finish") : Loc.Get("Wizard.Next");
    }

    private void Language_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag } || !Enum.TryParse<LanguagePreference>(tag, out var pref)) return;
        _controller.Settings.Language = pref;
        if (!_snapshotMode) _controller.ScheduleSave();
        Loc.SetLanguage(pref);
        RefreshDevicesAndStatus();
        ShowStep(_step);
    }

    private void Mode_Changed(object sender, RoutedEventArgs e)
    {
        // Visual state already reflects RadioButton; applied on Finish.
    }

    private void Back_Click(object sender, RoutedEventArgs e)
    {
        if (_step == 1)
        {
            ApplyAndClose(skipDriverInstall: true);
            return;
        }
        ShowStep(_step - 1);
    }

    private void Next_Click(object sender, RoutedEventArgs e)
    {
        if (_step < 3)
        {
            ShowStep(_step + 1);
            return;
        }
        ApplyAndClose(skipDriverInstall: false);
    }

    private void ApplyAndClose(bool skipDriverInstall)
    {
        if (_finished) return;
        _finished = true;

        var s = _controller.Settings;
        s.FirstRunDone = true;

        if (!_snapshotMode)
        {
            if (OutputCombo.SelectedItem is OutputChoice choice)
            {
                s.OutputChosenByUser = choice.Id != null;
                if (choice.Id != null) s.OutputDeviceId = choice.Id;
            }
            bool wantVirtual = ModeVirtual.IsChecked == true;
            s.Mode = wantVirtual ? EngineMode.VirtualDevices : EngineMode.PerApp;
            s.StartWithWindows = StartWithWindowsCheck.IsChecked == true;
            AutoStart.Set(s.StartWithWindows);

            if (DisableDuckingCheck.IsChecked == true && !WindowsDucking.IsDisabled)
                WindowsDucking.SetDisabled(true);

            SettingsStore.Save(s);

            if (!skipDriverInstall && wantVirtual && !DriverSetup.CablesPresent())
                InstallVirtualDriverRequested = true;
            else
                _controller.MarkAutoSetupHandled();

            _controller.EngineSettingsChanged(restartRouter: true);
        }

        if (!_snapshotMode) Close();
    }

    protected override void OnClosed(EventArgs e)
    {
        if (!_finished && !_snapshotMode)
        {
            // User closed the window via X: mark first run done so we don't nag endlessly, avoid unsolicited UAC.
            _controller.Settings.FirstRunDone = true;
            _controller.MarkAutoSetupHandled();
            SettingsStore.Save(_controller.Settings);
        }
        base.OnClosed(e);
    }

    public async Task SaveSnapshotAsync(string path)
    {
        await Dispatcher.InvokeAsync(() => { }, System.Windows.Threading.DispatcherPriority.ApplicationIdle);
        await Task.Delay(250);
        if (Content is not FrameworkElement root) return;
        var dpi = VisualTreeHelper.GetDpi(this);
        int w = (int)Math.Ceiling(root.ActualWidth * dpi.DpiScaleX);
        int h = (int)Math.Ceiling(root.ActualHeight * dpi.DpiScaleY);
        if (w <= 0 || h <= 0) return;
        var rtb = new RenderTargetBitmap(w, h, dpi.PixelsPerInchX, dpi.PixelsPerInchY, PixelFormats.Pbgra32);
        var bg = new DrawingVisual();
        using (var dc = bg.RenderOpen())
        {
            var brush = TryFindResource("ApplicationBackgroundBrush") as Brush ?? Brushes.Black;
            dc.DrawRectangle(brush, null, new Rect(0, 0, root.ActualWidth, root.ActualHeight));
        }
        rtb.Render(bg);
        rtb.Render(root);
        var enc = new PngBitmapEncoder();
        enc.Frames.Add(BitmapFrame.Create(rtb));
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        await using var fs = File.Create(path);
        enc.Save(fs);
    }
}
