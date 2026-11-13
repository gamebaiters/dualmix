using System;
using System.Diagnostics;
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Navigation;
using DualMix.Core;

namespace DualMix.UI.Pages;

public partial class GeneralPage : Page
{
    private readonly Controller _c;
    private bool _loading = true; // controls raise change events while the page is being built

    public GeneralPage()
    {
        _c = App.Current.Controller;
        InitializeComponent();
        Loaded += (_, _) =>
        {
            App.Current.Updater.Changed += OnUpdaterChanged;
            Loc.LanguageChanged += OnLanguageChanged;
            Load();
        };
        Unloaded += (_, _) =>
        {
            App.Current.Updater.Changed -= OnUpdaterChanged;
            Loc.LanguageChanged -= OnLanguageChanged;
        };
    }

    private void OnLanguageChanged() => Dispatcher.BeginInvoke(Load);
    private void OnUpdaterChanged() => Dispatcher.BeginInvoke(ShowUpdateState);

    private void ShowUpdateState()
    {
        var u = App.Current.Updater;
        UpdateTitle.Text = $"DualMix {Updater.Current}";
        UpdateText.Text = u.State switch
        {
            UpdateState.Idle => _c.Settings.UpdateMode == UpdateMode.Off
                ? Loc.Get("General.UpdateIdleOff")
                : Loc.Get("General.UpdateIdleOn"),
            UpdateState.Downloading => $"{u.StatusText} {u.Progress:P0}",
            UpdateState.Available when u.Available?.Notes is { } notes => $"{u.StatusText} {notes}",
            _ => u.StatusText,
        };
        UpdateButton.IsEnabled = !u.Busy;
        UpdateButton.Content = u.Available != null && u.State != UpdateState.UpToDate
            ? Loc.Format("General.UpdateInstall", u.Available.Version)
            : Loc.Get("General.UpdateCheck");
    }

    private async void Update_Click(object sender, RoutedEventArgs e)
    {
        var u = App.Current.Updater;
        if (u.Busy) return;
        try
        {
            if (u.Available is { } info && u.State != UpdateState.UpToDate) await u.InstallAsync(info);
            else await u.CheckAsync();
        }
        catch (Exception ex)
        {
            Log.Error("Update from the settings page failed", ex);
        }
    }

    private void UpdateMode_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag } || !Enum.TryParse<UpdateMode>(tag, out var mode)) return;
        _c.Settings.UpdateMode = mode;
        _c.ScheduleSave();
        ShowUpdateState();
    }

    private void Load()
    {
        _loading = true;
        var s = _c.Settings;
        Autostart.IsChecked = AutoStart.IsEnabled;
        StartHidden.IsChecked = s.StartMinimized;
        Osd.IsChecked = s.ShowOsd;
        RestoreVolumes.IsChecked = s.RestoreVolumesOnExit;
        ThSystem.IsChecked = s.Theme == ThemePreference.System;
        ThLight.IsChecked = s.Theme == ThemePreference.Light;
        ThDark.IsChecked = s.Theme == ThemePreference.Dark;
        LangAuto.IsChecked = s.Language == LanguagePreference.Auto;
        LangEn.IsChecked = s.Language == LanguagePreference.English;
        LangIt.IsChecked = s.Language == LanguagePreference.Italian;
        UpAuto.IsChecked = s.UpdateMode == UpdateMode.Auto;
        UpNotify.IsChecked = s.UpdateMode == UpdateMode.Notify;
        UpOff.IsChecked = s.UpdateMode == UpdateMode.Off;
        ShowUpdateState();
        DataPath.Text = AppPaths.DataDir;
        VersionText.Text = $"DualMix {typeof(App).Assembly.GetName().Version?.ToString(3)}";
        _loading = false;
    }

    private void Option_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var s = _c.Settings;
        bool autostart = Autostart.IsChecked == true;
        if (autostart != s.StartWithWindows || autostart != AutoStart.IsEnabled)
        {
            s.StartWithWindows = autostart;
            AutoStart.Set(autostart);
        }
        s.StartMinimized = StartHidden.IsChecked == true;
        s.ShowOsd = Osd.IsChecked == true;
        s.RestoreVolumesOnExit = RestoreVolumes.IsChecked == true;
        _c.ScheduleSave();
    }

    private void Theme_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag } || !Enum.TryParse<ThemePreference>(tag, out var pref)) return;
        _c.Settings.Theme = pref;
        _c.ScheduleSave();
        ThemeService.Apply(pref);
        if (Window.GetWindow(this) is Window w) ThemeService.Watch(w, pref);
    }

    private void Language_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag } || !Enum.TryParse<LanguagePreference>(tag, out var pref)) return;
        _c.Settings.Language = pref;
        _c.ScheduleSave();
        Loc.SetLanguage(pref);
    }

    private void Wizard_Click(object sender, RoutedEventArgs e) => App.Current.ShowSetupWizard();

    private void TestOsd_Click(object sender, RoutedEventArgs e) => App.Current.ShowOsd(new OsdInfo(OsdKind.Mix, Loc.Get("Common.Mix"),
        Math.Abs(_c.Mix) < 0.001 ? Loc.Get("Common.Balanced") : $"Game {_c.GameLevelText} · Chat {_c.ChatLevelText}", _c.Mix), force: true);

    private async void Diag_Click(object sender, RoutedEventArgs e)
    {
        DiagButton.IsEnabled = false;
        DiagText.Text = Loc.Get("General.DiagRunningText");
        try
        {
            var (host, prefix) = AppPaths.Launcher;
            var psi = new ProcessStartInfo(host, prefix + "--diag --quick") { UseShellExecute = false, CreateNoWindow = true };
            using var p = Process.Start(psi);
            if (p != null) await p.WaitForExitAsync();
            var file = Path.Combine(AppPaths.DataDir, "diag.txt");
            DiagText.Text = p?.ExitCode == 0 ? Loc.Get("General.DiagOk") : Loc.Get("General.DiagIssues");
            if (File.Exists(file)) Process.Start(new ProcessStartInfo("notepad.exe", $"\"{file}\"") { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            DiagText.Text = Loc.Format("General.DiagFail", ex.Message);
        }
        finally
        {
            DiagButton.IsEnabled = true;
        }
    }

    private void OpenFolder_Click(object sender, RoutedEventArgs e) =>
        Process.Start(new ProcessStartInfo("explorer.exe", $"\"{AppPaths.DataDir}\"") { UseShellExecute = true });

    private void Link_Navigate(object sender, RequestNavigateEventArgs e)
    {
        Process.Start(new ProcessStartInfo(e.Uri.AbsoluteUri) { UseShellExecute = true });
        e.Handled = true;
    }
}
