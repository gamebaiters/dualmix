using System;
using System.IO;
using System.Text;

namespace DualMix.Core;

public static class AppPaths
{
    private static readonly Lazy<string> DataDirLazy = new(() =>
    {
        var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "DualMix");
        Directory.CreateDirectory(dir);
        return dir;
    });

    /// <summary>%APPDATA%\DualMix (created on first use; every log line asks for it, so it is worked out once).</summary>
    public static string DataDir => DataDirLazy.Value;

    public static string SettingsFile => Path.Combine(DataDir, "settings.json");
    public static string LogFile => Path.Combine(DataDir, "dualmix.log");

    /// <summary>Image of the running process (DualMix.exe, or dotnet.exe when hosted).</summary>
    public static string ExePath => Environment.ProcessPath ?? Path.Combine(AppContext.BaseDirectory, "DualMix.exe");

    public static string AppDir => AppContext.BaseDirectory;
    public static string AppExe => Path.Combine(AppDir, "DualMix.exe");
    public static string AppDll => Path.Combine(AppDir, "DualMix.dll");

    /// <summary>True when DualMix runs inside the .NET host (dotnet.exe DualMix.dll).</summary>
    public static bool IsHosted => Path.GetFileNameWithoutExtension(ExePath).Equals("dotnet", StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// Microsoft's .NET host with the desktop runtime DualMix needs, if installed.
    /// DualMix prefers to run inside it: security products (seen with Kaspersky) hold the audio-capture calls of an
    /// unknown, unsigned executable, which freezes the audio engine start; the signed Microsoft host is trusted.
    /// </summary>
    public static string? FindDotnetHost()
    {
        var major = Environment.Version.Major.ToString();
        var candidates = new[]
        {
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "dotnet"),
            Environment.GetEnvironmentVariable("DOTNET_ROOT") ?? "",
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".dotnet"),
        };
        foreach (var dir in candidates)
        {
            try
            {
                if (dir.Length == 0) continue;
                var exe = Path.Combine(dir, "dotnet.exe");
                var desktop = Path.Combine(dir, "shared", "Microsoft.WindowsDesktop.App");
                if (File.Exists(exe) && Directory.Exists(desktop) && Directory.GetDirectories(desktop, major + ".*").Length > 0)
                    return exe;
            }
            catch
            {
                // try the next one
            }
        }
        return null;
    }

    /// <summary>
    /// How to start the long-running DualMix: windowless and under the supervisor.
    /// dotnet.exe is a console program, so starting it directly would leave a terminal window open for as long as
    /// DualMix runs; Windows' own console host in headless mode (conhost --headless) runs it with no window at all,
    /// and keeps the whole chain made of Microsoft-signed programs.
    /// </summary>
    public static (string File, string Arguments) BackgroundCommand(bool tray)
    {
        var host = FindDotnetHost();
        var conhost = Path.Combine(Environment.SystemDirectory, "conhost.exe");
        string tail = "--supervise" + (tray ? " --tray" : "");
        if (host != null && File.Exists(AppDll) && File.Exists(conhost))
            return (conhost, $"--headless \"{host}\" \"{AppDll}\" {tail}");
        return (AppExe, tail); // no .NET host found: the program's own (windowless) launcher
    }

    /// <summary>How to start DualMix: program and the argument prefix that goes before DualMix's own arguments.</summary>
    public static (string File, string Prefix) Launcher
    {
        get
        {
            if (IsHosted) return (ExePath, $"\"{AppDll}\" ");
            var host = FindDotnetHost();
            return host != null && File.Exists(AppDll) ? (host, $"\"{AppDll}\" ") : (AppExe, "");
        }
    }
}

/// <summary>Tiny thread-safe file logger with size based rotation.</summary>
public static class Log
{
    private static readonly object Gate = new();
    private const long MaxSize = 1024 * 1024;

    public static void Info(string message) => Write("INFO ", message, null);
    public static void Warn(string message, Exception? ex = null) => Write("WARN ", message, ex);
    public static void Error(string message, Exception? ex = null) => Write("ERROR", message, ex);

    private static void Write(string level, string message, Exception? ex)
    {
        try
        {
            var sb = new StringBuilder();
            sb.Append(DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss.fff"));
            sb.Append(' ').Append(level).Append(' ');
            sb.Append('[').Append(Environment.CurrentManagedThreadId).Append("] ");
            sb.Append(message);
            if (ex != null) sb.Append(" | ").Append(ex.GetType().Name).Append(": ").Append(ex.Message);
            sb.AppendLine();
            if (ex != null && level == "ERROR") sb.AppendLine(ex.ToString());

            lock (Gate)
            {
                var path = AppPaths.LogFile;
                var fi = new FileInfo(path);
                if (fi.Exists && fi.Length > MaxSize)
                {
                    var old = path + ".old";
                    if (File.Exists(old)) File.Delete(old);
                    File.Move(path, old);
                }
                File.AppendAllText(path, sb.ToString(), Encoding.UTF8);
            }
#if DEBUG
            System.Diagnostics.Debug.Write(sb.ToString());
#endif
        }
        catch
        {
            // logging must never crash the app
        }
    }
}
