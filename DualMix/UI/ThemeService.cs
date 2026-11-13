using System;
using System.Windows;
using System.Windows.Media;
using DualMix.Core;
using Wpf.Ui.Appearance;
using Wpf.Ui.Controls;

namespace DualMix.UI;

/// <summary>Light / dark / follow-Windows theme, plus DualMix's own channel colours for each theme.</summary>
public static class ThemeService
{
    private static bool _hooked;

    public static bool IsDark => ApplicationThemeManager.GetAppTheme() != ApplicationTheme.Light;

    public static void Apply(ThemePreference pref)
    {
        if (!_hooked)
        {
            _hooked = true;
            ApplicationThemeManager.Changed += (theme, _) => UpdateChannelBrushes(theme);
        }

        var theme = pref switch
        {
            ThemePreference.Light => ApplicationTheme.Light,
            ThemePreference.Dark => ApplicationTheme.Dark,
            _ => ApplicationThemeManager.GetSystemTheme() == SystemTheme.Light ? ApplicationTheme.Light : ApplicationTheme.Dark,
        };
        ApplicationThemeManager.Apply(theme, WindowBackdropType.Mica, true);
        UpdateChannelBrushes(theme);
    }

    /// <summary>Follows Windows' light/dark switch while the window is open (only for the "System" preference).</summary>
    public static void Watch(Window window, ThemePreference pref)
    {
        try
        {
            if (pref == ThemePreference.System) SystemThemeWatcher.Watch(window, WindowBackdropType.Mica, true);
            else SystemThemeWatcher.UnWatch(window);
        }
        catch (Exception ex)
        {
            Log.Warn("Theme watcher unavailable", ex);
        }
    }

    private static void UpdateChannelBrushes(ApplicationTheme theme)
    {
        var res = Application.Current.Resources;
        bool light = theme == ApplicationTheme.Light;
        // Light theme colours are darker so they keep ≥ 3:1 contrast on light surfaces (WCAG 2.1 non-text contrast).
        Color game = light ? Color.FromRgb(0xC2, 0x4E, 0x00) : Color.FromRgb(0xFF, 0x9A, 0x52);
        Color chat = light ? Color.FromRgb(0x00, 0x67, 0xC0) : Color.FromRgb(0x5C, 0xC8, 0xFF);
        res["GameColor"] = game;
        res["ChatColor"] = chat;
        res["GameBrush"] = Frozen(game);
        res["ChatBrush"] = Frozen(chat);
        res["GameSoftBrush"] = Frozen(Color.FromArgb(0x33, game.R, game.G, game.B));
        res["ChatSoftBrush"] = Frozen(Color.FromArgb(0x33, chat.R, chat.G, chat.B));
        res["MeterTrackBrush"] = Frozen(light ? Color.FromArgb(0x24, 0, 0, 0) : Color.FromArgb(0x1F, 0xFF, 0xFF, 0xFF));
    }

    private static SolidColorBrush Frozen(Color c)
    {
        var b = new SolidColorBrush(c);
        b.Freeze();
        return b;
    }
}
