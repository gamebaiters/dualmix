using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.IO;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Threading;
using DualMix.Core;
using Microsoft.Win32;

namespace DualMix.UI.Pages;

public partial class AppsPage : Page
{
    private readonly Controller _c;
    private readonly ObservableCollection<AppRow> _rows = new();
    private readonly Dictionary<string, AppRow> _byKey = new(StringComparer.OrdinalIgnoreCase);
    private readonly DispatcherTimer _timer;
    private string _filter = "";

    public AppsPage()
    {
        _c = App.Current.Controller;
        InitializeComponent();
        List.ItemsSource = _rows;
        _timer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(100) };
        _timer.Tick += (_, _) => Refresh();
        Loaded += (_, _) =>
        {
            _c.AppsChanged += Refresh;
            _c.RescanNow();
            Refresh();
            _timer.Start();
        };
        Unloaded += (_, _) =>
        {
            _c.AppsChanged -= Refresh;
            _timer.Stop();
        };
    }

    private void Refresh()
    {
        var live = _c.Apps;
        var rules = new RuleSet(_c.Settings);
        var wanted = new List<AppRow>();
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);

        foreach (var info in live)
        {
            if (!Matches(info.DisplayName, info.Key)) continue;
            if (!_byKey.TryGetValue(info.Key, out var row))
            {
                row = new AppRow(_c, info.Key, info.DisplayName, info.ExePath, info.IsSystemSounds);
                _byKey[info.Key] = row;
            }
            row.Update(info);
            wanted.Add(row);
            seen.Add(info.Key);
        }

        if (ShowInactive.IsChecked == true)
        {
            foreach (var (key, rule) in _c.Settings.Apps.OrderBy(kv => kv.Value.DisplayName ?? kv.Key, StringComparer.CurrentCultureIgnoreCase))
            {
                if (seen.Contains(key)) continue;
                bool isSystem = key == RuleSet.SystemSoundsKey;
                var name = rule.DisplayName ?? key;
                if (!Matches(name, key)) continue;
                if (!_byKey.TryGetValue(key, out var row))
                {
                    row = new AppRow(_c, key, name, rule.ExePath, isSystem);
                    _byKey[key] = row;
                }
                row.UpdateFromRule(rule, rules);
                wanted.Add(row);
            }
        }

        // Minimal diff so sliders being dragged are not recreated.
        for (int i = _rows.Count - 1; i >= 0; i--)
        {
            if (!wanted.Contains(_rows[i])) _rows.RemoveAt(i);
        }
        for (int i = 0; i < wanted.Count; i++)
        {
            int at = _rows.IndexOf(wanted[i]);
            if (at < 0) _rows.Insert(i, wanted[i]);
            else if (at != i) _rows.Move(at, i);
        }

        EmptyState.Visibility = _rows.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private bool Matches(string name, string key) =>
        _filter.Length == 0 ||
        name.Contains(_filter, StringComparison.CurrentCultureIgnoreCase) ||
        key.Contains(_filter, StringComparison.OrdinalIgnoreCase);

    private void Search_TextChanged(object sender, TextChangedEventArgs e)
    {
        _filter = SearchBox.Text?.Trim() ?? "";
        Refresh();
    }

    private void ShowInactive_Changed(object sender, RoutedEventArgs e) => Refresh();

    private void Auto_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.Tag is AppRow row) row.ResetToAutomatic();
    }

    private void Add_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new OpenFileDialog
        {
            Title = "Scegli il programma da configurare",
            Filter = "Programmi (*.exe)|*.exe",
            CheckFileExists = true,
        };
        if (dlg.ShowDialog(Window.GetWindow(this)) != true) return;
        var key = Path.GetFileNameWithoutExtension(dlg.FileName);
        if (!_c.Settings.Apps.TryGetValue(key, out var rule))
        {
            rule = new AppRule { DisplayName = Audio.ProcessInfo.FriendlyName(dlg.FileName, key), ExePath = dlg.FileName };
            _c.Settings.Apps[key] = rule;
        }
        _c.RulesChanged();
        ShowInactive.IsChecked = true;
        Refresh();
    }
}
