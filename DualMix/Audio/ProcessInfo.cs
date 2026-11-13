using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

namespace DualMix.Audio;

/// <summary>Resolves process ids to names / executable paths / friendly names (cached).</summary>
public static class ProcessInfo
{
    private static readonly ConcurrentDictionary<uint, (string Name, string? Path)> ByPid = new();
    private static readonly ConcurrentDictionary<string, string> FriendlyByPath = new(StringComparer.OrdinalIgnoreCase);

    public static (string Name, string? Path) Get(uint pid)
    {
        if (ByPid.TryGetValue(pid, out var cached)) return cached;

        string? path = QueryPath(pid);
        string name;
        if (path != null)
        {
            name = System.IO.Path.GetFileNameWithoutExtension(path);
        }
        else
        {
            try
            {
                using var p = Process.GetProcessById((int)pid);
                name = p.ProcessName;
            }
            catch
            {
                name = "pid" + pid.ToString(CultureInfo.InvariantCulture);
            }
        }

        var result = (name, path);
        ByPid[pid] = result;
        return result;
    }

    /// <summary>Drops cache entries of processes that no longer own audio sessions.</summary>
    public static void Prune(HashSet<uint> alive)
    {
        foreach (var pid in ByPid.Keys)
        {
            if (!alive.Contains(pid)) ByPid.TryRemove(pid, out _);
        }
    }

    public static string FriendlyName(string? path, string processName)
    {
        if (string.IsNullOrEmpty(path)) return Prettify(processName);
        return FriendlyByPath.GetOrAdd(path, p =>
        {
            try
            {
                var vi = FileVersionInfo.GetVersionInfo(p);
                var d = vi.FileDescription?.Trim();
                if (!string.IsNullOrEmpty(d) && d.Length <= 48 && !d.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) return d;
                var prod = vi.ProductName?.Trim();
                if (!string.IsNullOrEmpty(prod) && prod.Length <= 48) return prod;
            }
            catch
            {
                // fall through
            }
            return Prettify(processName);
        });
    }

    private static string Prettify(string processName)
    {
        if (string.IsNullOrEmpty(processName)) return "?";
        return char.ToUpper(processName[0], CultureInfo.CurrentCulture) + processName[1..];
    }

    private static string? QueryPath(uint pid)
    {
        if (pid == 0) return null;
        var h = OpenProcess(0x1000 /* PROCESS_QUERY_LIMITED_INFORMATION */, false, pid);
        if (h == IntPtr.Zero) return null;
        try
        {
            var sb = new StringBuilder(1024);
            var size = sb.Capacity;
            return QueryFullProcessImageName(h, 0, sb, ref size) ? sb.ToString(0, size) : null;
        }
        finally
        {
            CloseHandle(h);
        }
    }

    public static IEnumerable<uint> PidsByName(string processName)
    {
        Process[] procs;
        try { procs = Process.GetProcessesByName(processName); }
        catch { yield break; }
        foreach (var p in procs)
        {
            uint id;
            try { id = (uint)p.Id; }
            catch { continue; }
            finally { p.Dispose(); }
            yield return id;
        }
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool QueryFullProcessImageName(IntPtr hProcess, int flags, StringBuilder exeName, ref int size);

    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr h);
}
