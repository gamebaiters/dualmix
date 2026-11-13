using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using DualMix.Core;
using DualMix.Input;

namespace DualMix.UI.Pages;

public partial class KnobPage : Page
{
    private readonly Controller _c;
    private bool _loading = true; // controls raise change events while the page is being built
    private string? _capturing;

    public KnobPage()
    {
        _c = App.Current.Controller;
        InitializeComponent();
        Loaded += (_, _) =>
        {
            _c.PropertyChanged += OnChanged;
            _c.Knob.KeySeen += OnKeySeen;
            Load();
        };
        Unloaded += (_, _) =>
        {
            _c.PropertyChanged -= OnChanged;
            _c.Knob.KeySeen -= OnKeySeen;
            _c.Knob.CaptureNextKey = null;
        };
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(Controller.KnobTarget) or nameof(Controller.KnobTargetText) or nameof(Controller.KnobHotkeyText)) Load();
        else if (e.PropertyName == nameof(Controller.KeyboardStatus)) KeyboardStatusText.Text = _c.KeyboardStatus;
    }

    private void OnKeySeen(string text) => Dispatcher.BeginInvoke(() => TestText.Text = $"{DateTime.Now:HH:mm:ss}  {text}");

    private void Load()
    {
        _loading = true;
        var s = _c.Settings;
        TMix.IsChecked = s.KnobTarget == KnobTarget.Mix;
        TVolume.IsChecked = s.KnobTarget == KnobTarget.Volume;
        TGame.IsChecked = s.KnobTarget == KnobTarget.Game;
        TChat.IsChecked = s.KnobTarget == KnobTarget.Chat;
        CMix.IsChecked = s.KnobCycle.Contains(KnobTarget.Mix);
        CVolume.IsChecked = s.KnobCycle.Contains(KnobTarget.Volume);
        CGame.IsChecked = s.KnobCycle.Contains(KnobTarget.Game);
        CChat.IsChecked = s.KnobCycle.Contains(KnobTarget.Chat);
        DoublePress.IsChecked = s.KnobDoublePressCycles;
        CtrlPress.IsChecked = s.KnobCtrlPressCycles;
        CtrlAlt.IsChecked = s.KnobCtrlAlternate;
        Enabled.IsChecked = s.KnobEnabled;
        Invert.IsChecked = s.KnobInvert;
        MixPressMutes.IsChecked = s.KnobMixPressMutes;
        foreach (ComboBoxItem item in StartFunction.Items)
            if (item.Tag is string tag && tag == s.KnobStartFunction.ToString()) StartFunction.SelectedItem = item;
        Step.Value = Math.Round(s.KnobStep * 100);
        StepText.Text = $"{Step.Value:0}%";
        if (_capturing == null)
        {
            HotkeyButton.Content = _c.KnobHotkeyText;
            KeyGame.Content = KnobController.KeyName(s.KeyMixGame);
            KeyChat.Content = KnobController.KeyName(s.KeyMixChat);
            KeyReset.Content = KnobController.KeyName(s.KeyMixReset);
        }
        CycleSummary.Text = "Ciclo: " + string.Join(" → ", s.KnobCycle.Select(Controller.TargetName));
        KeyboardStatusText.Text = _c.KeyboardStatus;
        Describe(s.KnobTarget);
        _loading = false;
    }

    private void Describe(KnobTarget t)
    {
        bool inv = _c.Settings.KnobInvert;
        string left, right, press, note;
        switch (t)
        {
            case KnobTarget.Mix:
                left = "più Game, meno Chat";
                right = "più Chat, meno Game";
                press = _c.Settings.KnobMixPressMutes ? "silenzia / riattiva tutto l'audio" : "torna a «Bilanciato» (Game e Chat al 100%)";
                note = "Compare l'indicatore di DualMix; il volume di Windows non cambia.";
                break;
            case KnobTarget.Volume:
                left = "abbassa il volume generale";
                right = "alza il volume generale";
                press = "silenzia / riattiva l'uscita";
                note = "È il normale volume di Windows 11, con il suo indicatore: DualMix lo applica alla scheda audio reale, quindi ciò che Windows mostra è il volume vero.";
                inv = false; // the rotation is handled by Windows itself
                break;
            case KnobTarget.Game:
                left = "abbassa il volume Game";
                right = "alza il volume Game";
                press = "silenzia / riattiva Game";
                note = "Compare l'indicatore di DualMix; il volume di Windows non cambia.";
                break;
            default:
                left = "abbassa il volume Chat";
                right = "alza il volume Chat";
                press = "silenzia / riattiva Chat";
                note = "Compare l'indicatore di DualMix; il volume di Windows non cambia.";
                break;
        }
        if (inv) (left, right) = (right, left);
        LeftText.Text = left;
        RightText.Text = right;
        PressText.Text = press;
        FunctionNote.Text = note;
    }

    private void Target_Checked(object sender, RoutedEventArgs e)
    {
        if (_loading || sender is not RadioButton { Tag: string tag }) return;
        if (Enum.TryParse<KnobTarget>(tag, out var t)) _c.SetKnobTarget(t, false);
        Describe(t);
    }

    private void Cycle_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var list = new List<KnobTarget>();
        if (CMix.IsChecked == true) list.Add(KnobTarget.Mix);
        if (CVolume.IsChecked == true) list.Add(KnobTarget.Volume);
        if (CGame.IsChecked == true) list.Add(KnobTarget.Game);
        if (CChat.IsChecked == true) list.Add(KnobTarget.Chat);
        if (list.Count == 0)
        {
            // At least one function must stay in the cycle.
            (sender as CheckBox)!.IsChecked = true;
            return;
        }
        _c.Settings.KnobCycle = list;
        _c.KnobSettingsChanged();
        CycleSummary.Text = "Ciclo: " + string.Join(" → ", list.Select(Controller.TargetName));
    }

    private void Option_Changed(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var s = _c.Settings;
        s.KnobDoublePressCycles = DoublePress.IsChecked == true;
        s.KnobCtrlPressCycles = CtrlPress.IsChecked == true;
        s.KnobCtrlAlternate = CtrlAlt.IsChecked == true;
        s.KnobEnabled = Enabled.IsChecked == true;
        s.KnobInvert = Invert.IsChecked == true;
        s.KnobMixPressMutes = MixPressMutes.IsChecked == true;
        _c.KnobSettingsChanged();
        Describe(s.KnobTarget);
    }

    private void StartFunction_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || StartFunction.SelectedItem is not ComboBoxItem { Tag: string tag }) return;
        if (!Enum.TryParse<KnobStartFunction>(tag, out var start)) return;
        _c.Settings.KnobStartFunction = start;
        _c.ScheduleSave();
    }

    private void Step_Changed(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (StepText != null) StepText.Text = $"{Step.Value:0}%";
        if (_loading) return;
        _c.Settings.KnobStep = Step.Value / 100.0;
        _c.KnobSettingsChanged();
    }

    private void Hotkey_Click(object sender, RoutedEventArgs e)
    {
        _capturing = "hotkey";
        HotkeyButton.Content = "Premi la combinazione…";
        _c.Knob.CaptureNextKey = chord => Dispatcher.BeginInvoke(() =>
        {
            _capturing = null;
            if (chord.Key == 0x1B)
            {
                Load(); // Esc: cancel
                return;
            }
            if (chord.Modifiers is HotkeyModifiers.None or HotkeyModifiers.Shift && !KnobController.UsableAlone(chord.Key))
            {
                // A plain letter or digit would hijack normal typing.
                HotkeyButton.Content = "Aggiungi Ctrl, Alt o Win";
                return;
            }
            _c.Settings.KnobHotkeyKey = chord.Key;
            _c.Settings.KnobHotkeyModifiers = chord.Modifiers;
            _c.Settings.KnobHotkeyCustom = true;
            _c.KnobSettingsChanged();
            Load();
        });
    }

    private void HotkeyReset_Click(object sender, RoutedEventArgs e)
    {
        _c.Knob.CaptureNextKey = null;
        _capturing = null;
        (_c.Settings.KnobHotkeyKey, _c.Settings.KnobHotkeyModifiers) = Controller.ShortcutDefaults[0];
        _c.Settings.KnobHotkeyCustom = false; // DualMix picks the first default no other program is using
        _c.KnobSettingsChanged();
        Load();
    }

    private void KeyAssign_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Wpf.Ui.Controls.Button { Tag: string which } button) return;
        _capturing = which;
        button.Content = "Premi un tasto… (Esc = nessuno)";
        _c.Knob.CaptureNextKey = chord => Dispatcher.BeginInvoke(() =>
        {
            _capturing = null;
            int vk = chord.Key == 0x1B ? 0 : chord.Key;
            var s = _c.Settings;
            switch (which)
            {
                case "Game": s.KeyMixGame = vk; break;
                case "Chat": s.KeyMixChat = vk; break;
                default: s.KeyMixReset = vk; break;
            }
            _c.KnobSettingsChanged();
            Load();
        });
    }
}
