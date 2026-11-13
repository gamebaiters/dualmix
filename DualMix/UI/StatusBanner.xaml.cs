using System.ComponentModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using DualMix.Core;
using Wpf.Ui.Controls;

namespace DualMix.UI;

/// <summary>Shows the engine status when it needs attention, with the action that fixes it.</summary>
public partial class StatusBanner : UserControl
{
    private Controller? _controller;

    /// <summary>When true the banner is also shown for "all good" (used on the Audio page).</summary>
    public bool ShowSuccess { get; set; }

    public StatusBanner()
    {
        InitializeComponent();
        Loaded += (_, _) =>
        {
            _controller = App.Current.Controller;
            _controller.PropertyChanged += OnChanged;
            Refresh();
        };
        Unloaded += (_, _) =>
        {
            if (_controller != null) _controller.PropertyChanged -= OnChanged;
        };
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(Controller.Severity) or nameof(Controller.StatusTitle) or nameof(Controller.StatusMessage)
            or nameof(Controller.NeedsDriver) or nameof(Controller.NeedsRepair) or nameof(Controller.SetupBusy))
            Refresh();
    }

    private void Refresh()
    {
        var c = _controller;
        if (c == null) return;
        bool show = c.Severity != StatusSeverity.Success || ShowSuccess || c.SetupBusy;
        Root.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        TitleText.Text = c.StatusTitle;
        MessageText.Text = c.StatusMessage;
        MessageText.Visibility = string.IsNullOrEmpty(c.StatusMessage) ? Visibility.Collapsed : Visibility.Visible;

        var (symbol, bgKey, fgKey) = c.Severity switch
        {
            StatusSeverity.Success => (SymbolRegular.CheckmarkCircle24, "SystemFillColorSuccessBackgroundBrush", "SystemFillColorSuccessBrush"),
            StatusSeverity.Warning => (SymbolRegular.Warning24, "SystemFillColorCautionBackgroundBrush", "SystemFillColorCautionBrush"),
            StatusSeverity.Error => (SymbolRegular.DismissCircle24, "SystemFillColorCriticalBackgroundBrush", "SystemFillColorCriticalBrush"),
            _ => (SymbolRegular.Info24, "SystemFillColorAttentionBackgroundBrush", "AccentTextFillColorPrimaryBrush"),
        };
        Icon.Symbol = symbol;
        Root.SetResourceReference(Border.BackgroundProperty, bgKey);
        Icon.SetResourceReference(ForegroundProperty, fgKey);

        Busy.Visibility = c.SetupBusy ? Visibility.Visible : Visibility.Collapsed;
        RepairButton.Visibility = c.NeedsRepair && !c.SetupBusy ? Visibility.Visible : Visibility.Collapsed;
        InstallButton.Visibility = c.NeedsDriver && !c.SetupBusy && c.Settings.Mode == EngineMode.VirtualDevices ? Visibility.Visible : Visibility.Collapsed;
    }

    private async void Repair_Click(object sender, RoutedEventArgs e)
    {
        if (_controller == null) return;
        await _controller.RunSetupAsync(install: _controller.NeedsDriver && _controller.Settings.Mode == EngineMode.VirtualDevices, repair: true);
    }

    private async void Install_Click(object sender, RoutedEventArgs e)
    {
        if (_controller == null) return;
        await _controller.RunSetupAsync(install: true, repair: _controller.NeedsRepair);
    }
}
