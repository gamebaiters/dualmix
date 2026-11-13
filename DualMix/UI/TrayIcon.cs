using System;
using System.ComponentModel;
using System.Drawing;
using DualMix.Core;
using WinForms = System.Windows.Forms;

namespace DualMix.UI;

/// <summary>Notification-area icon with a dark context menu.</summary>
public sealed class TrayIcon : IDisposable
{
    private readonly WinForms.NotifyIcon _icon;
    private readonly Controller _controller;
    private readonly WinForms.ToolStripMenuItem _autostartItem;
    private readonly WinForms.ToolStripMenuItem _mixLabel;
    private readonly Icon _appIcon;
    private readonly WinForms.ToolStripMenuItem _knobMenu;
    private readonly WinForms.ContextMenuStrip _menu;
    private readonly Font _menuFont = new("Segoe UI", 9.5f);
    private readonly Font _boldFont = new("Segoe UI", 9.5f, FontStyle.Bold);
    private bool _disposed;

    public TrayIcon(Controller controller, Action open, Action exit)
    {
        _controller = controller;
        _appIcon = LoadIcon();

        var menu = new WinForms.ContextMenuStrip
        {
            Renderer = new WinForms.ToolStripProfessionalRenderer(new MenuColors(true)) { RoundedEdges = false },
            ShowImageMargin = false,
            ShowCheckMargin = true,
            Font = _menuFont,
        };

        var openItem = new WinForms.ToolStripMenuItem("Apri DualMix", null, (_, _) => open()) { Font = _boldFont };
        _mixLabel = new WinForms.ToolStripMenuItem("") { Enabled = false };
        var allGame = new WinForms.ToolStripMenuItem("Tutto Game", null, (_, _) => _controller.SetMix(-1, false));
        var center = new WinForms.ToolStripMenuItem("Bilanciato", null, (_, _) => _controller.ResetMix());
        var allChat = new WinForms.ToolStripMenuItem("Tutto Chat", null, (_, _) => _controller.SetMix(1, false));
        _autostartItem = new WinForms.ToolStripMenuItem("Avvia con Windows", null, (_, _) =>
        {
            var s = _controller.Settings;
            s.StartWithWindows = !s.StartWithWindows;
            AutoStart.Set(s.StartWithWindows);
            _controller.ScheduleSave();
        });
        _knobMenu = new WinForms.ToolStripMenuItem("Funzione rotella");
        foreach (var t in new[] { KnobTarget.Mix, KnobTarget.Volume, KnobTarget.Game, KnobTarget.Chat })
        {
            var target = t;
            var item = new WinForms.ToolStripMenuItem(Controller.TargetName(t), null, (_, _) => _controller.SetKnobTarget(target, true)) { Tag = t };
            _knobMenu.DropDownItems.Add(item);
        }
        var exitItem = new WinForms.ToolStripMenuItem("Esci", null, (_, _) => exit());

        menu.Items.AddRange(new WinForms.ToolStripItem[]
        {
            openItem,
            new WinForms.ToolStripSeparator(),
            _mixLabel,
            allGame,
            center,
            allChat,
            new WinForms.ToolStripSeparator(),
            _knobMenu,
            _autostartItem,
            new WinForms.ToolStripSeparator(),
            exitItem,
        });
        _menu = menu;

        menu.Opening += (_, _) =>
        {
            ApplyTheme();
            _autostartItem.Checked = AutoStart.IsEnabled;
            _mixLabel.Text = $"Game {_controller.GameLevelText} · Chat {_controller.ChatLevelText}";
            foreach (WinForms.ToolStripMenuItem item in _knobMenu.DropDownItems)
                item.Checked = item.Tag is KnobTarget t && t == _controller.Settings.KnobTarget;
        };

        _icon = new WinForms.NotifyIcon
        {
            Icon = _appIcon,
            Visible = true,
            ContextMenuStrip = menu,
            Text = "DualMix",
        };
        _icon.MouseClick += (_, e) =>
        {
            if (e.Button == WinForms.MouseButtons.Left) open();
        };

        _controller.PropertyChanged += OnControllerChanged;
        UpdateTooltip();
    }

    private void OnControllerChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(Controller.Gains) or nameof(Controller.StatusTitle) or nameof(Controller.KnobTargetText)) UpdateTooltip();
    }

    private void UpdateTooltip()
    {
        var problem = _controller.Severity is StatusSeverity.Error or StatusSeverity.Warning ? " · ⚠" : "";
        var text = $"DualMix · Game {_controller.GameLevelText} · Chat {_controller.ChatLevelText}{problem}";
        _icon.Text = text.Length > 63 ? text[..63] : text;
    }

    public void ShowBalloon(string title, string text)
    {
        try
        {
            _icon.ShowBalloonTip(5000, title, text, WinForms.ToolTipIcon.None);
        }
        catch
        {
            // ignore
        }
    }

    private static Icon LoadIcon()
    {
        try
        {
            var res = System.Windows.Application.GetResourceStream(new Uri("pack://application:,,,/Assets/dualmix.ico"));
            if (res != null)
            {
                using var s = res.Stream;
                return new Icon(s, WinForms.SystemInformation.SmallIconSize);
            }
        }
        catch (Exception ex)
        {
            Log.Warn("Tray icon resource not found", ex);
        }
        return SystemIcons.Application;
    }

    public void Dispose()
    {
        if (_disposed) return; // called on exit and again when the application object goes away
        _disposed = true;
        _controller.PropertyChanged -= OnControllerChanged;
        _icon.Visible = false;
        _icon.Dispose();
        _menu.Dispose();
        _menuFont.Dispose();
        _boldFont.Dispose();
        if (!ReferenceEquals(_appIcon, SystemIcons.Application)) _appIcon.Dispose();
    }

    private void ApplyTheme()
    {
        bool dark = ThemeService.IsDark;
        var renderer = new WinForms.ToolStripProfessionalRenderer(new MenuColors(dark)) { RoundedEdges = false };
        _menu.Renderer = renderer;
        var fg = dark ? Color.FromArgb(240, 240, 240) : Color.FromArgb(26, 26, 26);
        var dim = dark ? Color.FromArgb(160, 160, 160) : Color.FromArgb(110, 110, 110);
        foreach (WinForms.ToolStripItem item in _menu.Items) item.ForeColor = fg;
        foreach (WinForms.ToolStripItem item in _knobMenu.DropDownItems) item.ForeColor = fg;
        _knobMenu.DropDown.BackColor = dark ? Color.FromArgb(44, 44, 44) : Color.FromArgb(249, 249, 249);
        _mixLabel.ForeColor = dim;
    }

    /// <summary>Windows 11 menu colours (dark and light).</summary>
    private sealed class MenuColors : WinForms.ProfessionalColorTable
    {
        private readonly Color _bg, _hover, _line;

        public MenuColors(bool dark)
        {
            _bg = dark ? Color.FromArgb(44, 44, 44) : Color.FromArgb(249, 249, 249);
            _hover = dark ? Color.FromArgb(61, 61, 61) : Color.FromArgb(235, 235, 235);
            _line = dark ? Color.FromArgb(64, 64, 64) : Color.FromArgb(222, 222, 222);
        }

        public override Color ToolStripDropDownBackground => _bg;
        public override Color MenuBorder => _line;
        public override Color MenuItemBorder => _hover;
        public override Color MenuItemSelected => _hover;
        public override Color MenuItemSelectedGradientBegin => _hover;
        public override Color MenuItemSelectedGradientEnd => _hover;
        public override Color MenuItemPressedGradientBegin => _hover;
        public override Color MenuItemPressedGradientEnd => _hover;
        public override Color ImageMarginGradientBegin => _bg;
        public override Color ImageMarginGradientMiddle => _bg;
        public override Color ImageMarginGradientEnd => _bg;
        public override Color SeparatorDark => _line;
        public override Color SeparatorLight => _bg;
        public override Color CheckBackground => _hover;
        public override Color CheckSelectedBackground => _hover;
        public override Color CheckPressedBackground => _hover;
    }
}
