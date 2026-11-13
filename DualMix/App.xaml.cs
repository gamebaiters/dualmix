using System;
using System.Diagnostics;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Threading;
using DualMix.Audio;
using DualMix.Core;
using DualMix.UI;

namespace DualMix;

public partial class App : Application
{
    private const string MutexName = Supervisor.InstanceMutex;
    private const string ShowEventName = Supervisor.ShowEvent;
    private const string ExitEventName = @"Local\DualMix.Exit.7f3c2a";

    private Mutex? _mutex;
    private EventWaitHandle? _showEvent;
    private RegisteredWaitHandle? _showWait;
    private EventWaitHandle? _exitEvent;
    private RegisteredWaitHandle? _exitWait;
    private Controller? _controller;
    private TrayIcon? _tray;
    private MainWindow? _window;
    private OsdWindow? _osd;
    private bool _exiting;

    public static new App Current => (App)Application.Current;
    public Controller Controller => _controller!;

    protected override void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        bool Has(string arg) => e.Args.Contains(arg, StringComparer.OrdinalIgnoreCase);

        // Elevated helper (driver installation / audio service repair): no UI, no single-instance lock.
        if (Has("--setup"))
        {
            Shutdown(DriverSetup.RunElevated(e.Args));
            return;
        }

        if (Has("--diag"))
        {
            bool hostDiag = !AppPaths.IsHosted && !Has("--native") && AppPaths.Launcher.File != AppPaths.AppExe;
            Shutdown(hostDiag ? RelaunchHosted(e.Args) : Diagnostics.Run(e.Args));
            return;
        }

        if (Has("--exit"))
        {
            // Ask the running instance to quit (used by the install/uninstall scripts).
            try
            {
                using var ev = EventWaitHandle.OpenExisting(ExitEventName);
                ev.Set();
            }
            catch { /* not running */ }
            Shutdown(0);
            return;
        }

        bool tray = Has("--tray");
        var snapIndex = Array.FindIndex(e.Args, a => a.Equals("--snapshot", StringComparison.OrdinalIgnoreCase));
        bool snapshot = snapIndex >= 0 && snapIndex + 1 < e.Args.Length;

        // "--child": started by the supervisor. Anything else (a double click, the Start menu) is handed over to the
        // windowless, supervised way of running, so there is exactly one way DualMix runs.
        if (!Has("--child") && !Has("--native") && !snapshot)
        {
            if (Supervisor.AppRunning())
            {
                if (!tray) Supervisor.SignalShow();
                Shutdown(0);
                return;
            }
            if (StartBackground(tray))
            {
                Shutdown(0);
                return;
            }
            // could not start it that way: run right here
        }

        if (!snapshot)
        {
            _mutex = new Mutex(true, MutexName, out bool first);
            if (!first)
            {
                if (!tray) Supervisor.SignalShow();
                Shutdown(0);
                return;
            }
        }

        try { SetCurrentProcessExplicitAppUserModelID("Marco.DualMix"); } catch { /* cosmetic */ }
        if (!snapshot) ProcessTuning.Apply();
        // The audio thread must not be paused by full blocking collections.
        System.Runtime.GCSettings.LatencyMode = System.Runtime.GCLatencyMode.SustainedLowLatency;

        DispatcherUnhandledException += (_, args) =>
        {
            Log.Error("Unhandled UI exception", args.Exception);
            args.Handled = true;
        };
        AppDomain.CurrentDomain.UnhandledException += (_, args) => Log.Error("Unhandled exception", args.ExceptionObject as Exception);
        TaskScheduler.UnobservedTaskException += (_, args) =>
        {
            Log.Error("Unobserved task exception", args.Exception);
            args.SetObserved();
        };

        Log.Info($"DualMix {typeof(App).Assembly.GetName().Version} starting ({string.Join(' ', e.Args)})");

        if (snapshot) SettingsStore.ReadOnly = true; // screenshots leave no trace
        var settings = SettingsStore.Load();
        ThemeService.Apply(settings.Theme);

        if (!snapshot)
        {
            // Keep the autostart task pointing at the current installation (e.g. after an update/move).
            if (settings.StartWithWindows && !AutoStart.IsCurrent) AutoStart.Set(true);
            else if (!settings.StartWithWindows && AutoStart.IsEnabled) AutoStart.Set(false);
        }

        if (!settings.FirstRunDone && !snapshot)
        {
            settings.FirstRunDone = true;
            // Windows lowers every other sound by 80% when it detects a call: that defeats the whole point
            // of a Game/Chat mix, so switch it off once (it can be turned back on in "Audio e dispositivi").
            if (!WindowsDucking.IsDisabled) WindowsDucking.SetDisabled(true);
            SettingsStore.Save(settings);
        }

        _controller = new Controller(settings, Dispatcher, dryRun: snapshot);
        if (Has("--accept-injected")) _controller.AcceptInjectedKeys = true;
        _controller.OsdRequested += info => ShowOsd(info);
        _controller.Start();

        if (snapshot)
        {
            _ = SnapshotAsync(e.Args[snapIndex + 1]);
            return;
        }

        _tray = new TrayIcon(_controller, ShowMainWindow, () => _ = ExitAsync());

        _showEvent = new EventWaitHandle(false, EventResetMode.AutoReset, ShowEventName);
        _showWait = ThreadPool.RegisterWaitForSingleObject(_showEvent, (_, _) => Dispatcher.BeginInvoke(ShowMainWindow), null, -1, false);
        _exitEvent = new EventWaitHandle(false, EventResetMode.AutoReset, ExitEventName);
        _exitWait = ThreadPool.RegisterWaitForSingleObject(_exitEvent, (_, _) => Dispatcher.BeginInvoke(() => _ = ExitAsync()), null, -1, true);

        if (!tray || !settings.StartMinimized) ShowMainWindow();

        SessionEnding += (_, _) =>
        {
            Log.Info("Windows session ending");
            try { _controller?.ShutdownAsync().Wait(4000); } catch { /* ignore */ }
        };
    }

    /// <summary>Starts the windowless, supervised DualMix (see <see cref="AppPaths.BackgroundCommand"/>).</summary>
    private static bool StartBackground(bool tray)
    {
        try
        {
            var (file, arguments) = AppPaths.BackgroundCommand(tray);
            using var p = Process.Start(new ProcessStartInfo(file, arguments)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                WorkingDirectory = AppPaths.AppDir,
            });
            return p != null;
        }
        catch (Exception ex)
        {
            Log.Error("Cannot start DualMix in the background", ex);
            return false;
        }
    }

    /// <summary>Developer aid: renders every page to PNG files (off-screen) and exits.</summary>
    private async Task SnapshotAsync(string dir)
    {
        _window = new MainWindow(_controller!) { WindowStartupLocation = WindowStartupLocation.Manual, Left = -20000, Top = 0, ShowActivated = false };
        _window.Show();
        await Task.Delay(3000);
        // Once at the normal size, once tall enough to see every page down to its last card.
        foreach (var (height, suffix) in new[] { (740.0, ""), (2100.0, "_full") })
        {
            _window.Height = height;
            for (int i = 0; i < 5; i++)
            {
                _window.Navigate(i);
                await Task.Delay(900);
                await _window.SaveSnapshotAsync(System.IO.Path.Combine(dir, $"page{i}{suffix}.png"));
                if (suffix.Length > 0 && _window.ScrollPageToEnd())
                {
                    await Task.Delay(500);
                    await _window.SaveSnapshotAsync(System.IO.Path.Combine(dir, $"page{i}_end.png"));
                }
            }
        }
        await ExitAsync();
    }

    /// <summary>Runs the same command line inside the .NET host and returns its exit code.</summary>
    private static int RelaunchHosted(string[] args)
    {
        var (file, prefix) = AppPaths.Launcher;
        var arguments = prefix + string.Join(" ", args.Select(a => a.Contains(' ') || a.Length == 0 ? "\"" + a + "\"" : a));
        try
        {
            using var p = Process.Start(new ProcessStartInfo(file, arguments)
            {
                UseShellExecute = false,
                WorkingDirectory = AppPaths.AppDir,
            });
            if (p == null) return 1;
            p.WaitForExit();
            return p.ExitCode;
        }
        catch (Exception ex)
        {
            Log.Error("Cannot start inside the .NET host", ex);
            return 1;
        }
    }

    [System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    private static extern int SetCurrentProcessExplicitAppUserModelID(string appId);

    public void ShowMainWindow()
    {
        if (_exiting) return;
        if (_window == null)
        {
            _window = new MainWindow(_controller!);
            _window.Closed += (_, _) => _window = null;
        }
        if (!_window.IsVisible) _window.Show();
        if (_window.WindowState == WindowState.Minimized) _window.WindowState = WindowState.Normal;
        _window.Activate();
        _window.Topmost = true;
        _window.Topmost = false;
        _window.Focus();
    }

    public void OnMainWindowHidden()
    {
        var s = _controller!.Settings;
        if (!s.TrayHintShown)
        {
            s.TrayHintShown = true;
            _controller.ScheduleSave();
            _tray?.ShowBalloon("DualMix è ancora attivo", "Continua a gestire l'audio dall'area di notifica. Clic sull'icona per riaprirlo, tasto destro per uscire.");
        }
    }

    public void ShowOsd(OsdInfo info, bool force = false)
    {
        if (_controller == null || (!_controller.Settings.ShowOsd && !force)) return;
        _osd ??= new OsdWindow();
        _osd.ShowInfo(info);
    }

    public async Task ExitAsync()
    {
        if (_exiting) return;
        _exiting = true;
        Log.Info("Exit requested");
        try
        {
            _window?.CloseForReal();
            _osd?.Close();
            if (_controller != null) await _controller.ShutdownAsync();
        }
        catch (Exception ex)
        {
            Log.Error("Error during exit", ex);
        }
        finally
        {
            _tray?.Dispose();
            _showWait?.Unregister(null);
            _showEvent?.Dispose();
            _exitWait?.Unregister(null);
            _exitEvent?.Dispose();
            try { _mutex?.ReleaseMutex(); } catch { /* ignore */ }
            await Dispatcher.InvokeAsync(() => Shutdown(0));
        }
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _tray?.Dispose();
        base.OnExit(e);
    }
}
