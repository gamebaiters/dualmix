using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Threading;
using DualMix.Core;

namespace DualMix;

/// <summary>
/// Keeps DualMix running, the way a service manager would: it starts the application as a child process and starts
/// it again if it ends abnormally (crash, killed). A clean exit ("Esci" in the tray menu, sign-out) ends the
/// supervisor too. It runs windowless (see <see cref="AppPaths.BackgroundCommand"/>) and uses a few megabytes.
/// </summary>
public static class Supervisor
{
    public const string InstanceMutex = @"Local\DualMix.SingleInstance.7f3c2a";
    public const string ShowEvent = @"Local\DualMix.ShowWindow.7f3c2a";
    private const string SupervisorMutex = @"Local\DualMix.Supervisor.7f3c2a";

    private const int MaxRestarts = 5;
    private static readonly TimeSpan RestartWindow = TimeSpan.FromMinutes(10);

    public static int Run(string[] args)
    {
        bool tray = args.Contains("--tray", StringComparer.OrdinalIgnoreCase);

        using var mine = new Mutex(true, SupervisorMutex, out bool first);
        if (!first || AppRunning())
        {
            // DualMix is already up: at most bring its window to the front.
            if (!tray) SignalShow();
            return 0;
        }

        var restarts = new Queue<DateTime>();
        bool firstStart = true;
        while (true)
        {
            int code;
            try
            {
                var (file, prefix) = AppPaths.Launcher;
                // After a restart DualMix comes back quietly in the notification area.
                var arguments = prefix + "--child" + (tray || !firstStart ? " --tray" : "");
                using var child = Process.Start(new ProcessStartInfo(file, arguments)
                {
                    UseShellExecute = false,
                    CreateNoWindow = true,
                    WorkingDirectory = AppPaths.AppDir,
                });
                if (child == null) return 1;
                child.WaitForExit();
                code = child.ExitCode;
            }
            catch (Exception ex)
            {
                Log.Error("Supervisor: cannot start DualMix", ex);
                return 1;
            }
            firstStart = false;

            if (code == 0) return 0; // closed on purpose

            var now = DateTime.UtcNow;
            restarts.Enqueue(now);
            while (restarts.Count > 0 && now - restarts.Peek() > RestartWindow) restarts.Dequeue();
            if (restarts.Count > MaxRestarts)
            {
                Log.Error($"Supervisor: DualMix ended abnormally {restarts.Count} times in {RestartWindow.TotalMinutes:0} minutes (last code 0x{code:X8}): giving up");
                return code;
            }
            Log.Warn($"Supervisor: DualMix ended abnormally (code 0x{code:X8}): restarting ({restarts.Count}/{MaxRestarts})");
            Thread.Sleep(2000);
        }
    }

    public static bool AppRunning()
    {
        if (!Mutex.TryOpenExisting(InstanceMutex, out var m)) return false;
        m.Dispose();
        return true;
    }

    public static void SignalShow()
    {
        try
        {
            using var ev = EventWaitHandle.OpenExisting(ShowEvent);
            ev.Set();
        }
        catch
        {
            // not running (yet)
        }
    }
}
