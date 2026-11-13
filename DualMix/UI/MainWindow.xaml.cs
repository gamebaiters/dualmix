using System;
using System.ComponentModel;
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using DualMix.Core;
using DualMix.UI.Pages;
using Wpf.Ui.Controls;

namespace DualMix.UI;

public partial class MainWindow : FluentWindow
{
    private static readonly Type[] PageOrder = { typeof(MixerPage), typeof(AppsPage), typeof(KnobPage), typeof(AudioPage), typeof(GeneralPage) };

    private readonly Controller _controller;
    private bool _reallyClose;

    public MainWindow(Controller controller)
    {
        _controller = controller;
        InitializeComponent();
        ThemeService.Watch(this, controller.Settings.Theme);

        Loaded += (_, _) => Navigate(0);
        IsVisibleChanged += (_, _) => _controller.MetersEnabled = IsVisible && WindowState != WindowState.Minimized;
        StateChanged += (_, _) => _controller.MetersEnabled = IsVisible && WindowState != WindowState.Minimized;
        PreviewKeyDown += OnPreviewKeyDown;
    }

    public void Navigate(int index)
    {
        if (index < 0 || index >= PageOrder.Length) return;
        Nav.Navigate(PageOrder[index]);
    }

    private void OnPreviewKeyDown(object sender, KeyEventArgs e)
    {
        // Keyboard accelerators: Ctrl+1..5 switch section, Esc hides the window.
        if (Keyboard.Modifiers == ModifierKeys.Control && e.Key >= Key.D1 && e.Key <= Key.D5)
        {
            Navigate(e.Key - Key.D1);
            e.Handled = true;
        }
        else if (e.Key == Key.Escape && Keyboard.Modifiers == ModifierKeys.None && Keyboard.FocusedElement is not System.Windows.Controls.TextBox)
        {
            Hide();
            e.Handled = true;
        }
    }

    protected override void OnClosing(CancelEventArgs e)
    {
        if (!_reallyClose)
        {
            // Closing the window keeps DualMix running in the notification area.
            e.Cancel = true;
            Hide();
            App.Current.OnMainWindowHidden();
            return;
        }
        base.OnClosing(e);
    }

    public void CloseForReal()
    {
        _reallyClose = true;
        Close();
    }

    /// <summary>Developer aid (--snapshot): scrolls the current page to its end. Returns false when nothing scrolls.</summary>
    public bool ScrollPageToEnd()
    {
        DependencyObject? node = FindDescendant<System.Windows.Controls.Page>(this);
        while (node != null && node is not System.Windows.Controls.ScrollViewer) node = VisualTreeHelper.GetParent(node);
        if (node is not System.Windows.Controls.ScrollViewer sv || sv.ScrollableHeight < 1) return false;
        sv.ScrollToEnd();
        return true;
    }

    private static T? FindDescendant<T>(DependencyObject parent) where T : DependencyObject
    {
        int n = VisualTreeHelper.GetChildrenCount(parent);
        for (int i = 0; i < n; i++)
        {
            var child = VisualTreeHelper.GetChild(parent, i);
            if (child is T hit) return hit;
            var deeper = FindDescendant<T>(child);
            if (deeper != null) return deeper;
        }
        return null;
    }

    /// <summary>Developer aid (--snapshot): renders the window content to a PNG.</summary>
    public async Task SaveSnapshotAsync(string path)
    {
        await Dispatcher.InvokeAsync(() => { }, System.Windows.Threading.DispatcherPriority.ApplicationIdle);
        if (Content is not FrameworkElement root) return;
        var dpi = VisualTreeHelper.GetDpi(this);
        var w = (int)Math.Ceiling(root.ActualWidth * dpi.DpiScaleX);
        var h = (int)Math.Ceiling(root.ActualHeight * dpi.DpiScaleY);
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
