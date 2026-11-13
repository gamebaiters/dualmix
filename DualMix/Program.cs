using System;
using System.Linq;
using System.Runtime.CompilerServices;

namespace DualMix;

/// <summary>
/// Entry point. "--supervise" runs the small watchdog that keeps DualMix alive (no user interface is loaded in that
/// process); everything else is the application itself.
/// </summary>
public static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        if (args.Contains("--supervise", StringComparer.OrdinalIgnoreCase)) return Supervisor.Run(args);
        return RunApp();
    }

    // Kept out of Main so that the supervisor process never loads WPF.
    [MethodImpl(MethodImplOptions.NoInlining)]
    private static int RunApp()
    {
        var app = new App();
        app.InitializeComponent();
        return app.Run();
    }
}
