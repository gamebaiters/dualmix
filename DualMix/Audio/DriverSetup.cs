using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Linq;
using System.Net.Http;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using DualMix.Core;
using NAudio.CoreAudioApi;

namespace DualMix.Audio;

public sealed class SetupResult
{
    /// <summary>A cable pack was given but its driver could not be loaded by Windows (and was removed again).</summary>
    public bool PackRejected { get; set; }

    public bool Ok { get; set; }
    public bool Declined { get; set; }
    public bool Installed { get; set; }
    public bool Repaired { get; set; }
    public bool RebootRecommended { get; set; }
    public string Message { get; set; } = "";
}

/// <summary>
/// Virtual-cable driver management. DualMix uses the free VB-CABLE driver from VB-Audio (donationware,
/// https://vb-audio.com/Cable/): its two playback endpoints ("CABLE Input" and "CABLE In 16 Ch") become
/// "DualMix Game" and "DualMix Chat". The package is always downloaded from the official site and its signatures
/// are verified before the official setup runs.
/// Also hosts the "repair Windows audio" action (restart of the audio services).
/// </summary>
public static class DriverSetup
{
    public const string PackageUrl = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
    public const string HomePage = "https://vb-audio.com/Cable/";

    /// <summary>At least one usable virtual cable (enough for the Chat output).</summary>
    public static bool CablesPresent()
    {
        var all = Devices.List(DataFlow.Render, DeviceState.Active | DeviceState.Disabled);
        return all.Any(d => d.IsVbCable);
    }

    public static bool DriverInstalled()
    {
        var all = Devices.List(DataFlow.Render, DeviceState.Active | DeviceState.Disabled | DeviceState.Unplugged);
        return all.Any(d => d.IsVbCable);
    }

    // ================================================================== unprivileged side

    /// <summary>Runs DualMix elevated (UAC prompt) to install the driver and/or repair the audio service.</summary>
    /// <param name="packPath">Optional VB-CABLE A+B pack (zip) to install as the second cable.</param>
    public static async Task<SetupResult> RunElevatedAsync(bool install, bool repair, string? packPath = null, CancellationToken ct = default)
    {
        var resultFile = Path.Combine(Path.GetTempPath(), $"dualmix-setup-{Guid.NewGuid():N}.json");
        var args = $"--setup \"{resultFile}\"" + (install ? " --install" : "") + (repair ? " --repair" : "")
                   + (packPath != null ? $" --pack \"{packPath}\"" : "");
        var (file, prefix) = AppPaths.Launcher;
        var psi = new ProcessStartInfo(file, prefix + args)
        {
            UseShellExecute = true,
            Verb = "runas",
            WindowStyle = ProcessWindowStyle.Hidden,
        };
        Log.Info($"Elevated setup requested (install={install}, repair={repair}, pack={packPath ?? "-"})");
        try
        {
            using var p = Process.Start(psi);
            if (p == null) return new SetupResult { Message = Loc.Get("Setup.CantStart") };
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(TimeSpan.FromMinutes(6));
            await p.WaitForExitAsync(timeout.Token).ConfigureAwait(false);
        }
        catch (Win32Exception ex) when (ex.NativeErrorCode == 1223)
        {
            Log.Warn("Elevated setup declined by the user (UAC)");
            return new SetupResult { Declined = true, Message = Loc.Get("Setup.UacDeclined") };
        }
        catch (OperationCanceledException)
        {
            return new SetupResult { Message = Loc.Get("Setup.Timeout") };
        }
        catch (Exception ex)
        {
            Log.Error("Elevated setup failed to start", ex);
            return new SetupResult { Message = ex.Message };
        }

        try
        {
            if (File.Exists(resultFile))
            {
                var r = JsonSerializer.Deserialize<SetupResult>(File.ReadAllText(resultFile)) ?? new SetupResult();
                File.Delete(resultFile);
                Log.Info($"Elevated setup result: ok={r.Ok} installed={r.Installed} repaired={r.Repaired} '{r.Message}'");
                return r;
            }
        }
        catch (Exception ex)
        {
            Log.Error("Cannot read setup result", ex);
        }
        return new SetupResult { Message = Loc.Get("Setup.NoResult") };
    }

    // ================================================================== elevated side

    /// <summary>Entry point of "DualMix.exe --setup &lt;result&gt; [--install] [--repair]" (runs as administrator).</summary>
    public static int RunElevated(string[] args)
    {
        Loc.Initialize(SettingsStore.Load().Language);
        var idx = Array.FindIndex(args, a => a.Equals("--setup", StringComparison.OrdinalIgnoreCase));
        var resultFile = idx >= 0 && idx + 1 < args.Length ? args[idx + 1] : Path.Combine(Path.GetTempPath(), "dualmix-setup.json");
        bool install = args.Contains("--install", StringComparer.OrdinalIgnoreCase);
        bool repair = args.Contains("--repair", StringComparer.OrdinalIgnoreCase);
        bool unconfigure = args.Contains("--unconfigure", StringComparer.OrdinalIgnoreCase);
        bool removeDrivers = args.Contains("--remove-drivers", StringComparer.OrdinalIgnoreCase);
        var packIdx = Array.FindIndex(args, a => a.Equals("--pack", StringComparison.OrdinalIgnoreCase));
        var pack = packIdx >= 0 && packIdx + 1 < args.Length ? args[packIdx + 1] : null;
        var result = new SetupResult();
        var messages = new System.Collections.Generic.List<string>();
        Log.Info($"Setup (elevated) started: install={install}, repair={repair}");

        try
        {
            if (unconfigure || removeDrivers)
            {
                // Uninstallation: give the cables their factory names back, or remove them altogether.
                if (removeDrivers)
                {
                    int n = RemoveCables(all: true);
                    messages.Add(Loc.Format("Setup.CablesRemoved", n));
                }
                else if (Unconfigure())
                {
                    messages.Add(Loc.Get("Setup.CablesRestored"));
                }
                result.Ok = true;
                result.Message = string.Join(" ", messages);
                File.WriteAllText(resultFile, JsonSerializer.Serialize(result));
                Log.Info("Setup (elevated) finished: " + result.Message);
                return 0;
            }

            if (repair)
            {
                RepairAudioServices();
                result.Repaired = true;
                messages.Add(Loc.Get("Setup.AudioRepaired"));
            }

            if (install && !CablesPresent())
            {
                var before = DefaultsSnapshot.Take();
                InstallVbCable(messages);
                result.Installed = true;
                if (!WaitFor(CablesPresent, TimeSpan.FromSeconds(40)))
                {
                    // The driver is in place but Windows has not created the endpoints yet: a restart of the audio
                    // services usually makes them appear, otherwise a reboot does.
                    RepairAudioServices();
                    if (!WaitFor(CablesPresent, TimeSpan.FromSeconds(20)))
                    {
                        result.RebootRecommended = true;
                        messages.Add(Loc.Get("Setup.RebootNeeded"));
                    }
                }

                // Windows likes to make a freshly installed device the default one: put everything back
                // (DualMix itself decides later what the defaults are while it is running).
                Thread.Sleep(1500);
                before.Restore();
            }

            if (pack != null) InstallCablePack(pack, messages, result);

            if (CablesPresent())
            {
                var s = SettingsStore.Load();
                Configure(s, elevated: true, messages);
                // A second pass picks up the state Windows publishes after renames / format changes.
                Thread.Sleep(500);
                Configure(s, elevated: true, null);
            }
            else if (install && !result.RebootRecommended)
            {
                result.RebootRecommended = true;
                messages.Add(Loc.Get("Setup.RebootPending"));
            }

            result.Ok = true;
        }
        catch (Exception ex)
        {
            Log.Error("Elevated setup failed", ex);
            messages.Add(ex.Message);
            result.Ok = false;
        }

        result.Message = string.Join(" ", messages);
        try
        {
            File.WriteAllText(resultFile, JsonSerializer.Serialize(result));
        }
        catch (Exception ex)
        {
            Log.Error("Cannot write setup result", ex);
        }
        Log.Info("Setup (elevated) finished: " + result.Message);
        return result.Ok ? 0 : 1;
    }

    private static bool WaitFor(Func<bool> condition, TimeSpan timeout)
    {
        var end = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < end)
        {
            if (condition()) return true;
            Thread.Sleep(1000);
        }
        return condition();
    }

    private static void InstallVbCable(System.Collections.Generic.List<string> messages)
    {
        var work = Path.Combine(Path.GetTempPath(), "DualMix-driver-" + Guid.NewGuid().ToString("N")[..8]);
        Directory.CreateDirectory(work);
        try
        {
            var zip = Path.Combine(work, "vbcable.zip");
            Log.Info("Downloading " + PackageUrl);
            using (var http = new HttpClient { Timeout = TimeSpan.FromMinutes(3) })
            {
                http.DefaultRequestHeaders.UserAgent.ParseAdd("DualMix/2.0");
                var bytes = http.GetByteArrayAsync(PackageUrl).GetAwaiter().GetResult();
                File.WriteAllBytes(zip, bytes);
            }
            var dir = Path.Combine(work, "pkg");
            ZipFile.ExtractToDirectory(zip, dir);

            var setup = Path.Combine(dir, Environment.Is64BitOperatingSystem ? "VBCABLE_Setup_x64.exe" : "VBCABLE_Setup.exe");
            if (!File.Exists(setup)) throw new FileNotFoundException("Pacchetto VB-CABLE incompleto (setup mancante).");

            // Never run something we did not verify: valid Authenticode signature from VB-Audio's author,
            // and the driver catalog signed by Microsoft (Windows Hardware Compatibility Publisher).
            var signer = VerifySignature(setup) ?? throw new InvalidOperationException(Loc.Get("Setup.VbSigInvalid"));
            if (!signer.Contains("BUREL", StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException(Loc.Format("Setup.VbUnexpectedSigner", signer));
            var cat = Path.Combine(dir, "vbaudio_cable64_win10.cat");
            if (File.Exists(cat))
            {
                var catSigner = VerifySignature(cat);
                if (catSigner == null || !catSigner.Contains("Microsoft", StringComparison.OrdinalIgnoreCase))
                    throw new InvalidOperationException(Loc.Get("Setup.VbCatInvalid"));
            }
            Log.Info("VB-CABLE package verified, signer: " + signer);

            var psi = new ProcessStartInfo(setup, "-i -h")
            {
                WorkingDirectory = dir,
                UseShellExecute = false,
                CreateNoWindow = true,
            };
            using var p = Process.Start(psi) ?? throw new InvalidOperationException(Loc.Get("Setup.VbCantLaunch"));
            if (!p.WaitForExit(TimeSpan.FromMinutes(4)))
            {
                try { p.Kill(); } catch { /* ignore */ }
                throw new TimeoutException(Loc.Get("Setup.VbTimeout"));
            }
            Log.Info($"VB-CABLE setup exit code {p.ExitCode}");
            messages.Add(Loc.Get("Setup.VbInstalled"));
        }
        finally
        {
            try { Directory.Delete(work, true); } catch { /* ignore */ }
        }
    }

    /// <summary>Folder where a purchased cable pack is kept, so it can be reinstalled automatically.</summary>
    public static string PackCacheDir => Path.Combine(AppPaths.DataDir, "drivers");

    /// <summary>A cable pack saved by a previous installation, if any.</summary>
    public static string? CachedPack()
    {
        try
        {
            return Directory.Exists(PackCacheDir)
                ? Directory.GetFiles(PackCacheDir, "*.zip").OrderByDescending(File.GetLastWriteTimeUtc).FirstOrDefault()
                : null;
        }
        catch
        {
            return null;
        }
    }

    private static int CableDeviceCount() =>
        Devices.List(DataFlow.Render, DeviceState.Active | DeviceState.Disabled).Where(d => d.IsVbCable)
               .Select(d => d.DeviceKey).Distinct(StringComparer.OrdinalIgnoreCase).Count();

    /// <summary>
    /// Installs one more virtual cable from a VB-CABLE A+B pack (zip, as downloaded from VB-Audio after the donation).
    /// Only as many cables as DualMix needs are installed (two in total). Every setup program is verified before it
    /// runs, and a driver that Windows refuses to load is removed again.
    /// </summary>
    private static void InstallCablePack(string packPath, System.Collections.Generic.List<string> messages, SetupResult result)
    {
        if (!File.Exists(packPath)) throw new FileNotFoundException(Loc.Format("Setup.PackNotFound", packPath));

        var work = Path.Combine(Path.GetTempPath(), "DualMix-pack-" + Guid.NewGuid().ToString("N")[..8]);
        Directory.CreateDirectory(work);
        try
        {
            ZipFile.ExtractToDirectory(packPath, work);
            // The A+B pack contains one zip per cable.
            foreach (var inner in Directory.GetFiles(work, "*.zip", SearchOption.AllDirectories))
            {
                var dir = Path.Combine(Path.GetDirectoryName(inner)!, Path.GetFileNameWithoutExtension(inner));
                ZipFile.ExtractToDirectory(inner, dir);
            }

            var setups = Directory.GetFiles(work, "VBCABLE_Setup_x64.exe", SearchOption.AllDirectories)
                                  .OrderBy(f => f, StringComparer.OrdinalIgnoreCase).ToList();
            if (setups.Count == 0) throw new InvalidOperationException(Loc.Get("Setup.PackEmpty"));

            foreach (var setup in setups)
            {
                var dir = Path.GetDirectoryName(setup)!;
                var deviceName = ReadInfString(dir, "DeviceName") ?? "VB-Audio Cable";
                if (Devices.List(DataFlow.Render, DeviceState.Active | DeviceState.Disabled).Any(d => d.InterfaceName.Equals(deviceName, StringComparison.OrdinalIgnoreCase)))
                    continue; // this cable is already there

                var signer = VerifySignature(setup) ?? throw new InvalidOperationException(Loc.Format("Setup.PackSigInvalid", Path.GetFileName(dir)));
                if (!signer.Contains("Burel", StringComparison.OrdinalIgnoreCase))
                    throw new InvalidOperationException(Loc.Format("Setup.VbUnexpectedSigner", signer));

                int before = CableDeviceCount();
                var defaults = DefaultsSnapshot.Take();
                Log.Info($"Installing '{deviceName}' from {Path.GetFileName(packPath)} (signer: {signer})");
                int exit = RunSetupProgram(setup, "-i -h");
                Log.Info($"'{deviceName}' setup exit code {exit}");

                bool appeared = WaitFor(() => CableDeviceCount() > before, TimeSpan.FromSeconds(30));
                Thread.Sleep(1500);
                defaults.Restore();

                int problem = DeviceProblemCode(deviceName);
                if (!appeared || problem > 0)
                {
                    // Windows did not start the driver (typically: an old driver blocked by "Memory integrity").
                    Log.Warn($"'{deviceName}' did not load (endpoint appeared: {appeared}, problem code {problem}): removing it");
                    RunSetupProgram(setup, "-u -h");
                    result.PackRejected = true;
                    messages.Add(Loc.Format("Setup.PackRejected", deviceName, problem != 0 ? $" ({problem})" : ""));
                    return;
                }

                messages.Add(Loc.Format("Setup.CableInstalled", deviceName));
                result.Installed = true;
            }

            // With two bit-transparent cables in place the free VB-CABLE (whose captured audio is requantised)
            // is not needed any more: remove it so that only the two DualMix devices remain.
            var transparent = Devices.FindBuses(Devices.List(DataFlow.Render)).Count(d => !d.IsLossyCable);
            if (transparent >= 2 && RemoveFreeCable()) messages.Add(Loc.Get("Setup.FreeRemoved"));

            // Keep a copy: DualMix can then reinstall the cable by itself.
            try
            {
                Directory.CreateDirectory(PackCacheDir);
                var cached = Path.Combine(PackCacheDir, Path.GetFileName(packPath));
                if (!string.Equals(Path.GetFullPath(cached), Path.GetFullPath(packPath), StringComparison.OrdinalIgnoreCase))
                    File.Copy(packPath, cached, true);
            }
            catch (Exception ex)
            {
                Log.Warn("Cannot cache the cable pack", ex);
            }
        }
        finally
        {
            try { Directory.Delete(work, true); } catch { /* ignore */ }
        }
    }

    /// <summary>Uninstalls the free VB-CABLE device and its driver package (plain Windows tools, nothing to download).</summary>
    private static bool RemoveFreeCable() => RemoveCables(all: false) > 0;

    /// <summary>
    /// Uninstalls VB-Audio cable devices together with their driver packages.
    /// <paramref name="all"/> false: only the free "VB-Audio Virtual Cable"; true: every VB-Audio cable.
    /// Returns the number of devices removed.
    /// </summary>
    private static int RemoveCables(bool all)
    {
        string filter = all
            ? "$_.FriendlyName -like 'VB-Audio*Cable*' -and $_.FriendlyName -notlike '*Voicemeeter*'"
            : "$_.FriendlyName -eq 'VB-Audio Virtual Cable'";
        string script =
            "$ErrorActionPreference='SilentlyContinue';" +
            "$devs = @(Get-PnpDevice -Class MEDIA | Where-Object { " + filter + " });" +
            "$n = 0;" +
            "foreach ($d in $devs) {" +
            "  $inf = (Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceID -eq $d.InstanceId } | Select-Object -First 1).InfName;" +
            "  pnputil /remove-device $d.InstanceId | Out-Null;" +
            "  if ($inf -like 'oem*.inf') { pnputil /delete-driver $inf /uninstall /force | Out-Null };" +
            "  $n++ };" +
            "$n";
        try
        {
            var defaults = DefaultsSnapshot.Take();
            var psi = new ProcessStartInfo("powershell.exe", $"-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{script}\"")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
            };
            using var p = Process.Start(psi);
            if (p == null) return 0;
            var output = p.StandardOutput.ReadToEnd().Trim();
            p.WaitForExit(90000);
            Thread.Sleep(1500);
            defaults.Restore();
            Log.Info($"Cable removal (all={all}): {output} device(s)");
            return int.TryParse(output.Split('\n').Last().Trim(), out var n) ? n : 0;
        }
        catch (Exception ex)
        {
            Log.Warn("Cannot remove the virtual cables", ex);
            return 0;
        }
    }

    /// <summary>Undoes <see cref="Configure"/>: factory names back, every endpoint of the cables enabled again.</summary>
    public static bool Unconfigure()
    {
        bool changed = false;
        foreach (var flow in new[] { DataFlow.Render, DataFlow.Capture })
        {
            foreach (var d in Devices.List(flow, DeviceState.Active | DeviceState.Disabled).Where(d => d.IsVbCable))
            {
                if (d.State == DeviceState.Disabled && Devices.SetVisible(d.Id, true)) changed = true;
                if (flow == DataFlow.Render && d.Description.StartsWith("DualMix", StringComparison.OrdinalIgnoreCase))
                {
                    var name = d.IsMainJack ? d.FactoryName : Devices.SecondaryCableName;
                    if (Devices.Rename(d.Id, name)) changed = true;
                }
            }
        }
        if (changed) Log.Info("Cable endpoints restored to their factory names");
        return changed;
    }

    private static int RunSetupProgram(string setup, string arguments)
    {
        var psi = new ProcessStartInfo(setup, arguments)
        {
            WorkingDirectory = Path.GetDirectoryName(setup)!,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        using var p = Process.Start(psi) ?? throw new InvalidOperationException(Loc.Format("Setup.CantLaunchSetup", Path.GetFileName(setup)));
        if (!p.WaitForExit(TimeSpan.FromMinutes(4)))
        {
            try { p.Kill(); } catch { /* ignore */ }
            throw new TimeoutException(Loc.Get("Setup.DriverTimeout"));
        }
        return p.ExitCode;
    }

    /// <summary>Reads a value of the [Strings] section from the 64-bit INF in a folder (e.g. DeviceName).</summary>
    private static string? ReadInfString(string dir, string key)
    {
        try
        {
            var inf = Directory.GetFiles(dir, "*64_win10.inf").Concat(Directory.GetFiles(dir, "*64_win7.inf")).FirstOrDefault();
            if (inf == null) return null;
            foreach (var line in File.ReadLines(inf))
            {
                var t = line.Trim();
                if (!t.StartsWith(key, StringComparison.OrdinalIgnoreCase)) continue;
                int eq = t.IndexOf('=');
                if (eq < 0 || t[..eq].Trim().Length != key.Length) continue;
                return t[(eq + 1)..].Trim().Trim('"');
            }
        }
        catch
        {
            // fall through
        }
        return null;
    }

    /// <summary>Device Manager problem code of a device by name (0 = working, 39 = driver could not be loaded, -1 = not found).</summary>
    private static int DeviceProblemCode(string deviceName)
    {
        try
        {
            var script = "$d = Get-CimInstance Win32_PnPEntity | Where-Object { $_.Name -eq '" + deviceName.Replace("'", "''") + "' } | Select-Object -First 1; " +
                         "if ($d) { [int]$d.ConfigManagerErrorCode } else { -1 }";
            var psi = new ProcessStartInfo("powershell.exe", $"-NoProfile -NonInteractive -Command \"{script}\"")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
            };
            using var p = Process.Start(psi);
            if (p == null) return -1;
            var text = p.StandardOutput.ReadToEnd().Trim();
            p.WaitForExit(30000);
            return int.TryParse(text, out var code) ? code : -1;
        }
        catch
        {
            return -1;
        }
    }

    /// <summary>Restarts the Windows audio services (and Creative's, which depend on them).</summary>
    public static void RepairAudioServices()
    {
        const string script =
            "$ErrorActionPreference='SilentlyContinue';" +
            "$v = Get-Service CtxSvc32,CTAudSvcService | Where-Object Status -eq 'Running';" +
            "$v | Stop-Service -Force;" +
            "Restart-Service AudioEndpointBuilder -Force;" +
            "Start-Service Audiosrv;" +
            "Start-Sleep -Seconds 2;" +
            "$v | Start-Service;" +
            "Get-Service AudioEndpointBuilder,Audiosrv | ForEach-Object { \"$($_.Name)=$($_.Status)\" }";
        var psi = new ProcessStartInfo("powershell.exe", $"-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{script}\"")
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        using var p = Process.Start(psi) ?? throw new InvalidOperationException(Loc.Get("Setup.PowerShellFailed"));
        var output = p.StandardOutput.ReadToEnd();
        p.WaitForExit(60000);
        Log.Info("Audio services restarted: " + output.Replace(Environment.NewLine, " ").Trim());
    }

    // ================================================================== endpoint configuration

    /// <summary>What <see cref="Configure"/> did.</summary>
    public readonly record struct ConfigureResult(bool Changed, bool Failed);

    /// <summary>
    /// Prepares the cable endpoints:
    /// names ("DualMix Chat", and "DualMix Game" when a second cable exists), the secondary input of each cable
    /// disabled (it shares the stream of the main one: if anything opened it, the main one would stop working),
    /// the recording side of the cable hidden (it must never be picked as a microphone) and the device format matching
    /// the real output (same sample rate: no resampling; Game also the same speaker layout).
    /// Safe to call repeatedly: it only changes what differs.
    /// </summary>
    public static ConfigureResult Configure(Settings s, bool elevated, System.Collections.Generic.List<string>? messages = null)
    {
        bool changed = false, failed = false;

        // 1. One usable jack per cable device.
        var all = Devices.List(DataFlow.Render, DeviceState.Active | DeviceState.Disabled).Where(d => d.IsVbCable).ToList();
        foreach (var group in all.GroupBy(d => d.DeviceKey, StringComparer.OrdinalIgnoreCase))
        {
            var main = group.OrderBy(d => d.IsMainJack ? 0 : 1).First();
            if (main.State == DeviceState.Disabled)
            {
                if (Devices.SetVisible(main.Id, true)) { changed = true; Log.Info($"Enabled '{main.Name}'"); }
                else failed = true;
            }
            foreach (var secondary in group.Where(d => d.Id != main.Id))
            {
                if (secondary.Description.StartsWith("DualMix", StringComparison.OrdinalIgnoreCase))
                {
                    if (Devices.Rename(secondary.Id, Devices.SecondaryCableName)) changed = true;
                    else failed = true;
                }
                if (secondary.State == DeviceState.Active)
                {
                    if (Devices.SetVisible(secondary.Id, false)) { changed = true; Log.Info($"Disabled secondary cable input '{secondary.Name}'"); }
                    else failed = true;
                }
            }
        }
        if (changed) System.Threading.Thread.Sleep(400); // let Windows publish the new endpoint states

        // 2. Names and formats.
        var render = Devices.List(DataFlow.Render);
        var (game, chat) = Devices.FindCables(render, s.GameDeviceId, s.ChatDeviceId);
        if (chat == null) return new ConfigureResult(changed, true);

        var output = s.OutputChosenByUser && render.Any(d => d.Id == s.OutputDeviceId)
            ? render.First(d => d.Id == s.OutputDeviceId)
            : Devices.FindPreferredOutput(render, s.DefaultsBackup?.Multimedia ?? Devices.GetDefaultId(Role.Multimedia));

        if (game != null && !game.Description.Equals(Devices.GameEndpointName, StringComparison.Ordinal))
        {
            if (Devices.Rename(game.Id, Devices.GameEndpointName)) changed = true;
            else failed = true;
        }
        if (!chat.Description.Equals(Devices.ChatEndpointName, StringComparison.Ordinal))
        {
            if (Devices.Rename(chat.Id, Devices.ChatEndpointName)) changed = true;
            else failed = true;
        }

        foreach (var cap in Devices.List(DataFlow.Capture).Where(d => d.IsVbCable))
        {
            if (Devices.SetVisible(cap.Id, false))
            {
                changed = true;
                Log.Info($"Hidden cable recording endpoint '{cap.Name}'");
            }
            else failed = true;
        }

        foreach (var unused in Devices.FindBuses(render).Where(d => d.Id != chat.Id && d.Id != game?.Id))
        {
            if (unused.Description.StartsWith("DualMix", StringComparison.OrdinalIgnoreCase) && Devices.Rename(unused.Id, unused.FactoryName))
                changed = true;
        }

        var outFmt = output != null ? Devices.GetDeviceFormat(output.Id) : null;
        if (outFmt != null)
        {
            // Chat is voice: stereo, which every chat app handles. Game (when it has its own cable) follows the
            // real output exactly: same rate and speaker layout, so surround stays surround.
            changed |= AlignFormat(chat.Id, "Chat", outFmt.SampleRate, new[] { (2, 0x3u) });
            if (game != null) changed |= AlignFormat(game.Id, "Game", outFmt.SampleRate, Layouts(outFmt.Channels, outFmt.ChannelMask));
        }

        if (changed)
        {
            Log.Info($"Cable endpoints configured (elevated={elevated}, failed={failed})");
            messages?.Add(game != null
                ? Loc.Get("Setup.ConfiguredBoth")
                : Loc.Get("Setup.ConfiguredChat"));
        }
        return new ConfigureResult(changed, failed);
    }

    /// <summary>Speaker layouts to try for a channel count, the output's own first, stereo last.</summary>
    private static (int channels, uint mask)[] Layouts(int channels, uint mask)
    {
        var list = new System.Collections.Generic.List<(int, uint)> { (channels, mask) };
        void Add(int c, uint m) { if (!list.Contains((c, m))) list.Add((c, m)); }
        if (channels == 6) { Add(6, 0x3F); Add(6, 0x60F); }
        if (channels == 8) { Add(8, 0x63F); Add(8, 0xFF); Add(6, 0x3F); }
        if (channels == 4) { Add(4, 0x33); Add(4, 0x603); }
        Add(2, 0x3);
        return list.ToArray();
    }

    private static bool AlignFormat(string id, string label, int rate, (int channels, uint mask)[] layouts)
    {
        var current = Devices.GetDeviceFormat(id);
        var (wantCh, wantMask) = layouts[0];
        if (current != null && current.SampleRate == rate && current.Channels == wantCh &&
            (current.ChannelMask == wantMask || wantCh <= 2))
            return false;

        // The engine mixes in 32-bit float and DualMix captures that float mix (loopback), so the cable's own
        // device format only has to match rate and layout; the deepest format the driver accepts is used.
        foreach (var (ch, mask) in layouts)
        {
            if (current != null && current.SampleRate == rate && current.Channels == ch && (current.ChannelMask == mask || ch <= 2))
            {
                Log.Info($"{label} cable keeps {current.Describe()} (preferred layout not accepted by the driver)");
                return false;
            }
            AudioFormat[] candidates =
            {
                new(ch, rate, 32, 24, false, mask),
                new(ch, rate, 24, 24, false, mask),
                new(ch, rate, 32, 32, false, mask),
                new(ch, rate, 32, 32, true, mask),
                new(ch, rate, 16, 16, false, mask),
            };
            foreach (var f in candidates)
            {
                if (!Devices.SetDeviceFormat(id, f)) continue;
                var after = Devices.GetDeviceFormat(id);
                if (after != null && after.Channels == ch && after.SampleRate == rate)
                {
                    Log.Info($"{label} cable format set to {after.Describe()}");
                    return true;
                }
            }
        }

        // Layout not changeable: at least match the sample rate with the channels it already has.
        if (current != null && current.SampleRate != rate)
        {
            foreach (var bits in new[] { (32, 24), (24, 24), (16, 16) })
            {
                var f = current with { SampleRate = rate, Bits = bits.Item1, ValidBits = bits.Item2, IsFloat = false };
                if (Devices.SetDeviceFormat(id, f) && Devices.GetDeviceFormat(id)?.SampleRate == rate)
                {
                    Log.Info($"{label} cable sample rate set to {rate} Hz ({current.Channels} ch kept)");
                    return true;
                }
            }
        }
        Log.Warn($"{label} cable format could not be aligned to {rate} Hz (current: {current?.Describe() ?? "?"})");
        return false;
    }

    /// <summary>Default playback/recording devices before a driver installation.</summary>
    private sealed class DefaultsSnapshot
    {
        private readonly (DataFlow flow, Role role, string? id)[] _items;

        private DefaultsSnapshot((DataFlow, Role, string?)[] items) => _items = items;

        public static DefaultsSnapshot Take()
        {
            var list = new System.Collections.Generic.List<(DataFlow, Role, string?)>();
            foreach (var flow in new[] { DataFlow.Render, DataFlow.Capture })
            foreach (var role in new[] { Role.Console, Role.Multimedia, Role.Communications })
                list.Add((flow, role, Devices.GetDefaultId(role, flow)));
            return new DefaultsSnapshot(list.ToArray());
        }

        public void Restore()
        {
            foreach (var (flow, role, id) in _items)
            {
                if (id == null) continue;
                var now = Devices.GetDefaultId(role, flow);
                if (now == id || !Devices.IsActive(id)) continue;
                if (Devices.SetDefault(id, role)) Log.Info($"Default {flow}/{role} restored after driver installation");
            }
        }
    }

    // ================================================================== Authenticode

    /// <summary>Returns the signer subject of a validly signed file, or null.</summary>
    public static string? VerifySignature(string path)
    {
        var fileInfo = new WINTRUST_FILE_INFO
        {
            cbStruct = (uint)Marshal.SizeOf<WINTRUST_FILE_INFO>(),
            pcwszFilePath = path,
        };
        var pFile = Marshal.AllocHGlobal(Marshal.SizeOf<WINTRUST_FILE_INFO>());
        try
        {
            Marshal.StructureToPtr(fileInfo, pFile, false);
            var data = new WINTRUST_DATA
            {
                cbStruct = (uint)Marshal.SizeOf<WINTRUST_DATA>(),
                dwUIChoice = 2,          // WTD_UI_NONE
                fdwRevocationChecks = 0, // WTD_REVOKE_NONE
                dwUnionChoice = 1,       // WTD_CHOICE_FILE
                pFile = pFile,
                dwStateAction = 0,
                dwProvFlags = 0x10,      // WTD_REVOCATION_CHECK_NONE: no network round trip
            };
            var action = new Guid("00AAC56B-CD44-11d0-8CC2-00C04FC295EE"); // WINTRUST_ACTION_GENERIC_VERIFY_V2
            int hr = WinVerifyTrust(IntPtr.Zero, ref action, ref data);
            if (hr != 0)
            {
                Log.Warn($"WinVerifyTrust({Path.GetFileName(path)}) = 0x{hr:X8}");
                return null;
            }
#pragma warning disable SYSLIB0057
            using var cert = System.Security.Cryptography.X509Certificates.X509Certificate.CreateFromSignedFile(path);
#pragma warning restore SYSLIB0057
            return cert.Subject;
        }
        catch (Exception ex)
        {
            Log.Warn("Signature check failed for " + path, ex);
            return null;
        }
        finally
        {
            Marshal.FreeHGlobal(pFile);
        }
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct WINTRUST_FILE_INFO
    {
        public uint cbStruct;
        [MarshalAs(UnmanagedType.LPWStr)] public string pcwszFilePath;
        public IntPtr hFile;
        public IntPtr pgKnownSubject;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct WINTRUST_DATA
    {
        public uint cbStruct;
        public IntPtr pPolicyCallbackData;
        public IntPtr pSIPClientData;
        public uint dwUIChoice;
        public uint fdwRevocationChecks;
        public uint dwUnionChoice;
        public IntPtr pFile;
        public uint dwStateAction;
        public IntPtr hWVTStateData;
        public IntPtr pwszURLReference;
        public uint dwProvFlags;
        public uint dwUIContext;
        public IntPtr pSignatureSettings;
    }

    [DllImport("wintrust.dll", CharSet = CharSet.Unicode)]
    private static extern int WinVerifyTrust(IntPtr hwnd, ref Guid action, ref WINTRUST_DATA data);
}
