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
        Loaded += (_, _) => Load();
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

    private void TestOsd_Click(object sender, RoutedEventArgs e) => App.Current.ShowOsd(new OsdInfo(OsdKind.Mix, "Mix",
        Math.Abs(_c.Mix) < 0.001 ? "Bilanciato" : $"Game {_c.GameLevelText} · Chat {_c.ChatLevelText}", _c.Mix), force: true);

    private async void Diag_Click(object sender, RoutedEventArgs e)
    {
        DiagButton.IsEnabled = false;
        DiagText.Text = "Diagnostica in corso… (circa un minuto)";
        try
        {
            var (host, prefix) = AppPaths.Launcher;
            var psi = new ProcessStartInfo(host, prefix + "--diag --quick") { UseShellExecute = false, CreateNoWindow = true };
            using var p = Process.Start(psi);
            if (p != null) await p.WaitForExitAsync();
            var file = Path.Combine(AppPaths.DataDir, "diag.txt");
            DiagText.Text = p?.ExitCode == 0 ? "Tutto a posto. Report aperto nel Blocco note." : "Trovati problemi: guarda il report aperto nel Blocco note.";
            if (File.Exists(file)) Process.Start(new ProcessStartInfo("notepad.exe", $"\"{file}\"") { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            DiagText.Text = "Impossibile eseguire la diagnostica: " + ex.Message;
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
