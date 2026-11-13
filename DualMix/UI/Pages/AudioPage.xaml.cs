using System;
using System.ComponentModel;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Threading;
using DualMix.Audio;
using DualMix.Core;
using Microsoft.Win32;
using Wpf.Ui.Controls;

namespace DualMix.UI.Pages;

public partial class AudioPage : Page
{
    private sealed record OutputChoice(string? Id, string Label);

    private readonly Controller _c;
    private readonly DispatcherTimer _timer;
    private bool _loading = true; // controls raise change events while the page is being built

    public AudioPage()
    {
        _c = App.Current.Controller;
        InitializeComponent();
        _timer = new DispatcherTimer { Interval = TimeSpan.FromSeconds(1) };
        _timer.Tick += (_, _) => RefreshLive();
        Loaded += (_, _) =>
        {
            _c.EngineChanged += Load;
            _c.PropertyChanged += OnChanged;
            Load();
            _timer.Start();
        };
        Unloaded += (_, _) =>
        {
            _c.EngineChanged -= Load;
            _c.PropertyChanged -= OnChanged;
            _timer.Stop();
        };
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(Controller.SetupBusy) or nameof(Controller.NeedsDriver) or nameof(Controller.NeedsRepair)) Load();
    }

    private static string Short(string deviceName)
    {
        int i = deviceName.IndexOf(" (", StringComparison.Ordinal);
        return i > 0 ? deviceName[..i] : deviceName;
    }

    private void Load()
    {
        _loading = true;
        var s = _c.Settings;

        var render = Devices.ListRender().Where(d => !d.IsVbCable).ToList();
        var auto = Devices.FindPreferredOutput(render);
        var choices = new System.Collections.Generic.List<OutputChoice>
        {
            new(null, auto != null ? $"Automatico ({auto.Name})" : "Automatico (Sound Blaster AE-5)"),
        };
        choices.AddRange(render.Select(d => new OutputChoice(d.Id, d.Name)));
        OutputCombo.ItemsSource = choices;
        OutputCombo.SelectedItem = s.OutputChosenByUser
            ? choices.FirstOrDefault(c => c.Id == s.OutputDeviceId) ?? choices[0]
            : choices[0];

        bool virtualMode = s.Mode == EngineMode.VirtualDevices;
        UseVirtual.IsChecked = virtualMode;
        SetDefaults.IsChecked = s.SetWindowsDefaults;
        RouteApps.IsChecked = s.RouteAppsToDevices;
        GameDirect.IsChecked = s.GameDirect;
        SetDefaults.IsEnabled = RouteApps.IsEnabled = GameDirect.IsEnabled = virtualMode;
        GameDirectCard.Visibility = _c.VirtualCables >= 2 ? Visibility.Visible : Visibility.Collapsed;
        LMin.IsChecked = s.Latency == LatencyMode.Minima;
        LBal.IsChecked = s.Latency == LatencyMode.Bilanciata;
        LSafe.IsChecked = s.Latency == LatencyMode.Sicura;
        NoDucking.IsChecked = WindowsDucking.IsDisabled;

        string playback = _c.GameViaBus ? "«DualMix Game»" : "la scheda audio reale";
        DefaultsText.Text = $"Riproduzione → {playback}, comunicazioni → «DualMix Chat». Chiudendo DualMix torna tutto com'era.";

        switch (_c.VirtualCables)
        {
            case 0:
                DriverText.Text = "Non installato. DualMix scarica il driver VB-CABLE dal sito ufficiale (vb-audio.com) e ne verifica la firma.";
                DriverButton.Content = "Installa";
                DriverButton.Appearance = ControlAppearance.Primary;
                break;
            case 1:
                DriverText.Text = $"Un cavo installato: «{Short(_c.ChatDeviceName)}». I giochi suonano direttamente sulla scheda audio. " +
                                  "Per avere anche «DualMix Game» aggiungi il pacchetto VB-CABLE A+B (file .zip ricevuto da VB-Audio).";
                DriverButton.Content = "Aggiungi A+B…";
                DriverButton.Appearance = ControlAppearance.Primary;
                break;
            default:
                DriverText.Text = $"Installato · {_c.GameDeviceName} · {_c.ChatDeviceName}";
                DriverButton.Content = "Riconfigura";
                DriverButton.Appearance = ControlAppearance.Secondary;
                break;
        }
        DriverButton.IsEnabled = RepairButton.IsEnabled = !_c.SetupBusy;
        RefreshLive();
        _loading = false;
    }

    private void RefreshLive()
    {
        QualitySummary.Text = _c.RouterRunning
            ? $"{_c.FormatText} · latenza aggiunta: {_c.LatencyText}"
            : _c.EngineText;
        Technical.Text = _c.TechnicalDetails;
    }

    private void Output_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || OutputCombo.SelectedItem is not OutputChoice choice) return;
        var s = _c.Settings;
        s.OutputChosenByUser = choice.Id != null;
        if (choice.Id != null) s.OutputDeviceId = choice.Id;
        _c.EngineSettingsChanged(restartRouter: true);
    }

    private void UseVirtual_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        _c.Settings.Mode = UseVirtual.IsChecked == true ? EngineMode.VirtualDevices : EngineMode.PerApp;
        _c.EngineSettingsChanged();
        Load();
    }

    private void Option_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var s = _c.Settings;
        bool gameDirect = GameDirect.IsChecked == true;
        bool restart = gameDirect != s.GameDirect;
        s.SetWindowsDefaults = SetDefaults.IsChecked == true;
        s.RouteAppsToDevices = RouteApps.IsChecked == true;
        s.GameDirect = gameDirect;
        _c.EngineSettingsChanged(restartRouter: restart);
    }

    private void Latency_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag } || !Enum.TryParse<LatencyMode>(tag, out var mode)) return;
        _c.Settings.Latency = mode;
        _c.Settings.LearnedQueueMs = 0;   // what was learned belonged to the previous choice
        _c.Settings.LearnedSourceMs = 0;
        _c.EngineSettingsChanged(restartRouter: true);
    }

    private void Ducking_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        WindowsDucking.SetDisabled(NoDucking.IsChecked == true);
    }

    private async void Driver_Click(object sender, RoutedEventArgs e)
    {
        SetupResult result;
        switch (_c.VirtualCables)
        {
            case 0:
                result = await _c.RunSetupAsync(install: true, repair: _c.NeedsRepair);
                break;
            case 1:
                var dlg = new OpenFileDialog
                {
                    Title = "Scegli il pacchetto VB-CABLE A+B",
                    Filter = "Pacchetto VB-CABLE (*.zip)|*.zip",
                    CheckFileExists = true,
                };
                if (dlg.ShowDialog(Window.GetWindow(this)) != true) return;
                result = await _c.RunSetupAsync(install: false, repair: false, packPath: dlg.FileName);
                break;
            default:
                result = await _c.RunSetupAsync(install: false, repair: false);
                break;
        }
        ShowResult(result);
        Load();
    }

    private async void Repair_Click(object sender, RoutedEventArgs e)
    {
        var result = await _c.RunSetupAsync(install: _c.NeedsDriver && _c.Settings.Mode == EngineMode.VirtualDevices, repair: true);
        ShowResult(result);
        Load();
    }

    private void ShowResult(SetupResult result)
    {
        if (result.Declined)
        {
            SetupInfo.Severity = InfoBarSeverity.Warning;
            SetupInfo.Title = "Operazione annullata";
            SetupInfo.Message = "Serve l'autorizzazione di amministratore (richiesta di Windows «Controllo account utente»).";
        }
        else
        {
            SetupInfo.Severity = !result.Ok || result.PackRejected ? InfoBarSeverity.Error
                : result.RebootRecommended ? InfoBarSeverity.Warning : InfoBarSeverity.Success;
            SetupInfo.Title = !result.Ok || result.PackRejected ? "Non riuscito" : result.RebootRecommended ? "Riavvio necessario" : "Fatto";
            SetupInfo.Message = string.IsNullOrWhiteSpace(result.Message) ? "Configurazione verificata: era già tutto a posto." : result.Message;
        }
        SetupInfo.IsOpen = true;
    }
}
