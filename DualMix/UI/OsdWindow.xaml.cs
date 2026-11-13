using System;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Animation;
using System.Windows.Threading;
using DualMix.Core;
using Wpf.Ui.Controls;

namespace DualMix.UI;

/// <summary>
/// Glanceable click-through overlay shown when the knob or the shortcut changes something.
/// It never takes focus (WS_EX_NOACTIVATE) and ignores the mouse (WS_EX_TRANSPARENT), so it is safe over games.
/// </summary>
public partial class OsdWindow : Window
{
    private readonly DispatcherTimer _hideTimer;
    private bool _shown;
    private OsdInfo? _last;

    public OsdWindow()
    {
        InitializeComponent();
        _hideTimer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(1300) };
        _hideTimer.Tick += (_, _) =>
        {
            _hideTimer.Stop();
            FadeTo(0, 300);
        };
        SourceInitialized += (_, _) =>
        {
            var hwnd = new WindowInteropHelper(this).Handle;
            var ex = GetWindowLong(hwnd, GWL_EXSTYLE);
            SetWindowLong(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_LAYERED);
        };
        BarArea.SizeChanged += (_, _) => { if (_last != null) Layout(_last); };
    }

    public void ShowInfo(OsdInfo info)
    {
        _last = info;
        ApplyTheme();
        Render(info);

        if (!_shown)
        {
            Show();
            _shown = true;
        }
        PositionOnActiveScreen();
        UpdateLayout();
        Layout(info);

        BeginAnimation(OpacityProperty, null);
        if (Opacity < 1) FadeTo(1, 90);
        Topmost = false;
        Topmost = true;
        _hideTimer.Stop();
        _hideTimer.Start();
    }

    private void Render(OsdInfo info)
    {
        TitleText.Text = info.Title;
        ValueText.Text = info.ValueText;
        OsdIcon.Symbol = info.Kind switch
        {
            OsdKind.Mix => SymbolRegular.Options24,
            OsdKind.Volume => info.Muted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Speaker224,
            OsdKind.Game => info.Muted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Games24,
            OsdKind.Chat => info.Muted ? SymbolRegular.SpeakerMute24 : SymbolRegular.Chat24,
            _ => SymbolRegular.Keyboard24,
        };
        bool bar = info.Kind != OsdKind.Function;
        BarArea.Visibility = bar ? Visibility.Visible : Visibility.Collapsed;
        bool mix = info.Kind == OsdKind.Mix;
        CenterMark.Visibility = mix ? Visibility.Visible : Visibility.Collapsed;
        Thumb.Visibility = mix ? Visibility.Visible : Visibility.Collapsed;

        var res = Application.Current.Resources;
        if (mix)
        {
            var g = (Color)res["GameColor"];
            var c = (Color)res["ChatColor"];
            Track.Background = new LinearGradientBrush(
                new GradientStopCollection { new(g, 0), new(Color.FromArgb(0x80, 0x8A, 0x8A, 0x8A), 0.5), new(c, 1) }, 0);
            Fill.Background = Brushes.Transparent;
        }
        else
        {
            Track.SetResourceReference(Border.BackgroundProperty, "MeterTrackBrush");
            var key = info.Kind switch { OsdKind.Game => "GameBrush", OsdKind.Chat => "ChatBrush", _ => "AccentFillColorDefaultBrush" };
            Fill.SetResourceReference(Border.BackgroundProperty, key);
            Fill.Opacity = info.Muted ? 0.35 : 1;
        }
    }

    private void Layout(OsdInfo info)
    {
        var w = BarArea.ActualWidth;
        if (w <= 0) return;
        if (info.Kind == OsdKind.Mix)
        {
            var x = (Math.Clamp(info.Value, -1, 1) + 1) / 2 * (w - Thumb.Width);
            Canvas.SetLeft(Thumb, x);
            Canvas.SetTop(Thumb, (BarArea.ActualHeight - Thumb.Height) / 2);
            Fill.Width = 0;
        }
        else
        {
            Fill.Width = Math.Clamp(info.Value, 0, 1) * w;
        }
    }

    private void ApplyTheme()
    {
        bool dark = ThemeService.IsDark;
        Card.Background = new SolidColorBrush(dark ? Color.FromArgb(0xF0, 0x2C, 0x2C, 0x2C) : Color.FromArgb(0xF5, 0xFA, 0xFA, 0xFA));
        Card.BorderBrush = new SolidColorBrush(dark ? Color.FromArgb(0x30, 0xFF, 0xFF, 0xFF) : Color.FromArgb(0x1A, 0, 0, 0));
        var primary = dark ? Colors.White : Color.FromRgb(0x1A, 0x1A, 0x1A);
        var secondary = dark ? Color.FromArgb(0xC8, 0xFF, 0xFF, 0xFF) : Color.FromArgb(0x9E, 0, 0, 0);
        TitleText.Foreground = new SolidColorBrush(secondary);
        ValueText.Foreground = new SolidColorBrush(primary);
        OsdIcon.Foreground = new SolidColorBrush(primary);
        CenterMark.Fill = new SolidColorBrush(secondary);
        Thumb.Fill = new SolidColorBrush(primary);
        Thumb.Stroke = new SolidColorBrush(dark ? Color.FromRgb(0x2C, 0x2C, 0x2C) : Colors.White);
        Thumb.StrokeThickness = 2;
    }

    private void PositionOnActiveScreen()
    {
        // Bottom-centre of the monitor that holds the foreground window (the game), like the Windows volume flyout.
        var fg = GetForegroundWindow();
        var mon = MonitorFromWindow(fg, 2 /* MONITOR_DEFAULTTONEAREST */);
        var mi = new MONITORINFO { cbSize = Marshal.SizeOf<MONITORINFO>() };
        if (!GetMonitorInfo(mon, ref mi)) return;
        double scale = 1;
        if (GetDpiForMonitor(mon, 0, out uint dpiX, out _) == 0) scale = dpiX / 96.0;
        var area = mi.rcWork;
        var hwnd = new WindowInteropHelper(this).Handle;
        int wPx = (int)(Width * scale), hPx = (int)(Height * scale);
        int x = area.Left + (area.Right - area.Left - wPx) / 2;
        int y = area.Bottom - hPx - (int)(56 * scale);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, wPx, hPx, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }

    private void FadeTo(double to, int ms)
    {
        var reduceMotion = !SystemParameters.ClientAreaAnimation;
        var anim = new DoubleAnimation(to, TimeSpan.FromMilliseconds(reduceMotion ? 1 : ms)) { FillBehavior = FillBehavior.HoldEnd };
        BeginAnimation(OpacityProperty, anim);
    }

    private const int GWL_EXSTYLE = -20;
    private const int WS_EX_TRANSPARENT = 0x20;
    private const int WS_EX_TOOLWINDOW = 0x80;
    private const int WS_EX_LAYERED = 0x80000;
    private const int WS_EX_NOACTIVATE = 0x08000000;
    private static readonly IntPtr HWND_TOPMOST = new(-1);
    private const uint SWP_NOACTIVATE = 0x0010;
    private const uint SWP_SHOWWINDOW = 0x0040;

    [StructLayout(LayoutKind.Sequential)]
    private struct RECT
    {
        public int Left, Top, Right, Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MONITORINFO
    {
        public int cbSize;
        public RECT rcMonitor;
        public RECT rcWork;
        public uint dwFlags;
    }

    [DllImport("user32.dll")]
    private static extern int GetWindowLong(IntPtr hwnd, int index);

    [DllImport("user32.dll")]
    private static extern int SetWindowLong(IntPtr hwnd, int index, int value);

    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    private static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);

    [DllImport("user32.dll")]
    private static extern bool GetMonitorInfo(IntPtr monitor, ref MONITORINFO info);

    [DllImport("shcore.dll")]
    private static extern int GetDpiForMonitor(IntPtr monitor, int type, out uint dpiX, out uint dpiY);

    [DllImport("user32.dll")]
    private static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
}
