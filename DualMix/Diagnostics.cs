using System;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using DualMix.Audio;
using DualMix.Core;
using DualMix.Input;
using NAudio.CoreAudioApi;
using NAudio.Wave;

namespace DualMix;

/// <summary>
/// "DualMix.exe --diag": health check of the audio stack, written to
/// %APPDATA%\DualMix\diag.txt (and to the console when started from one). It opens no audio streams
/// and never changes what you hear.
/// </summary>
public static partial class Diagnostics
{
    private static readonly StringBuilder Out = new();
    private static int _failures;

    public static int Run(string[] args)
    {
        // Run on an MTA thread, like the real engines (avoids cross-apartment marshaling to the idle UI thread).
        int result = 1;
        var t = new Thread(() => result = RunCore(args)) { IsBackground = true };
        t.SetApartmentState(ApartmentState.MTA);
        t.Start();
        t.Join();
        return result;
    }

    private static int RunCore(string[] args)
    {
        AttachConsole(-1);
        try { File.Delete(Path.Combine(AppPaths.DataDir, "diag.live.txt")); } catch { /* ignore */ }
        W($"DualMix diagnostics {DateTime.Now:yyyy-MM-dd HH:mm:ss}  (Windows {Environment.OSVersion.Version})");
        W("");

        Step("Mix math", MixMathTest);
        Step("Rule set / catalog", RulesTest);
        Step("Knob controller", KnobTest);
        Step("Resampler fidelity", ResamplerTest);
        Step("Channel mapping", ChannelMapTest);
        Step("Playback devices", DevicesTest);
        Step("Per-app routing API", RoutingTest);
        Step("Default device API (no-op)", DefaultDeviceTest);
        Step("Session engine (live, no audible change)", EngineTest);
        Step("Audio path end to end (tones at -60 dBFS through the virtual devices)", RouterTest);
        if (args.Contains("--stress", StringComparer.OrdinalIgnoreCase) || args.Contains("--stress-gc", StringComparer.OrdinalIgnoreCase))
            Step("Audio engine under stress (streams starting and stopping, every CPU core busy)", RouterStressTest);

        W("");
        W(_failures == 0 ? "RESULT: OK" : $"RESULT: {_failures} FAILURE(S)");
        var path = Path.Combine(AppPaths.DataDir, "diag.txt");
        File.WriteAllText(path, Out.ToString());
        Console.WriteLine($"(saved to {path})");
        return _failures == 0 ? 0 : 1;
    }

    private static void Step(string name, Action a)
    {
        W($"== {name}");
        try
        {
            a();
        }
        catch (Exception ex)
        {
            _failures++;
            W($"   FAIL: {ex.GetType().Name}: {ex.Message}");
            Log.Error("Diagnostics step failed: " + name, ex);
        }
        W("");
    }

    private static void Check(bool ok, string what)
    {
        if (!ok) _failures++;
        W($"   {(ok ? "ok  " : "FAIL")} {what}");
    }

    private static void W(string s)
    {
        Out.AppendLine(s);
        try { File.AppendAllText(Path.Combine(AppPaths.DataDir, "diag.live.txt"), s + Environment.NewLine); } catch { /* ignore */ }
        try { Console.WriteLine(s); } catch { /* no console */ }
    }

    // ------------------------------------------------------------------------------------

    private static void MixMathTest()
    {
        bool Near(double a, double b) => Math.Abs(a - b) < 1e-6;
        var c = MixMath.Balance(0);
        Check(Near(c.game, 1) && Near(c.chat, 1), "center = 100% / 100%");
        var g = MixMath.Balance(-1);
        Check(Near(g.game, 1) && Near(g.chat, 0), "full left = Game 100%, Chat 0%");
        var h = MixMath.Balance(0.5);
        Check(Near(h.game, 0.5) && Near(h.chat, 1), "half right = Game 50% (slider scale), Chat 100%");
        var f = MixMath.Balance(1, 0.2);
        Check(Near(f.game, 0.2) && Near(f.chat, 1), "floor 20% keeps attenuated channel at 20%");
        var unity = MixMath.Compute(new Settings());
        Check(unity.Game == 1f && unity.Chat == 1f, "defaults are an exact x1.0 (bit-transparent)");
        var s = new Settings { Mix = -0.3, GameVolume = 0.8, ChatVolume = 0.5 };
        var gains = MixMath.Compute(s);
        Check(Math.Abs(gains.Game - 0.64f) < 1e-6 && Math.Abs(gains.Chat - 0.1225f) < 1e-6, $"perceptual taper -> {gains}");
        s.ChatMuted = true;
        Check(MixMath.Compute(s).Chat == 0, "mute zeroes the channel");
    }

    private static void RulesTest()
    {
        var s = new Settings();
        s.Apps["vlc"] = new AppRule { Channel = Channel.Chat, Volume = 0.7 };
        s.ExtraChatApps.Add("myvoip.exe");
        var r = new RuleSet(s);
        Check(r.Resolve("Discord", false) == Channel.Chat, "Discord -> Chat (catalog)");
        Check(r.Resolve("ts3client_win64", false) == Channel.Chat, "TeamSpeak 3 -> Chat (catalog)");
        Check(r.Resolve("TeamSpeak", false) == Channel.Chat, "TeamSpeak 5/6 -> Chat (catalog)");
        Check(r.Resolve("cs2", false) == Channel.Game, "unknown game -> Game (default)");
        Check(r.Resolve("VLC", false) == Channel.Chat && Math.Abs(r.VolumeOf("vlc") - 0.7) < 1e-9, "explicit rule wins, case-insensitive");
        Check(r.Resolve("myvoip", false) == Channel.Chat, "extra chat app from settings");
        Check(r.Resolve(RuleSet.SystemSoundsKey, true) == Channel.Game, "system sounds follow Game by default");
    }

    private static void KnobTest()
    {
        var actions = new System.Collections.Generic.List<KnobAction>();
        var defaults = new Settings();
        Check(defaults.KnobTarget == KnobTarget.Volume && defaults.KnobStartFunction == KnobStartFunction.Volume,
              "default startup function is Master Volume");
        var s = new Settings { KnobTarget = KnobTarget.Mix };
        var k = new KnobController(KnobConfig.From(s), a => actions.Add(a)); // no keyboard thread: the hook path handles everything

        bool Bound(int vk, HotkeyModifiers mods, KnobActionKind kind, KnobTarget? target = null) =>
            k.Bindings.Any(b => b.Vk == vk && b.Mods == mods && b.Action.Kind == kind && (target == null || b.Action.Target == target));

        // function Mix
        Check(k.HandleKey(KeyboardHook.VK_VOLUME_DOWN, true, false) && actions[^1] == new KnobAction(KnobActionKind.Rotate, -1, KnobTarget.Mix),
              "Mix: knob left -> Rotate(-1, Mix), key swallowed");
        Check(k.HandleKey(KeyboardHook.VK_VOLUME_DOWN, false, false), "matching key-up swallowed too");
        Check(k.HandleKey(KeyboardHook.VK_VOLUME_UP, true, false) && actions[^1].Direction == 1, "Mix: knob right -> Rotate(+1)");
        k.HandleKey(KeyboardHook.VK_VOLUME_UP, false, false);
        Check(!k.HandleKey(KeyboardHook.VK_VOLUME_UP, true, true), "keys injected by other programs ignored by the hook");
        Check(k.HandleKey(KeyboardHook.VK_VOLUME_MUTE, true, false) && actions[^1].Kind == KnobActionKind.Press, "knob press -> Press");
        k.HandleKey(KeyboardHook.VK_VOLUME_MUTE, false, false);
        Check(!k.HandleKey(0x4D, true, false), "M without modifiers untouched");
        Check(Bound(0x4D, HotkeyModifiers.Ctrl | HotkeyModifiers.Alt, KnobActionKind.CycleFunction), "Ctrl + Alt + M switches the knob function");
        Check(Bound(KeyboardHook.VK_VOLUME_MUTE, HotkeyModifiers.Ctrl, KnobActionKind.CycleFunction), "Ctrl + knob press switches the knob function");
        Check(!k.Bindings.Any(b => b.Vk == KeyboardHook.VK_VOLUME_UP && b.Mods == HotkeyModifiers.Ctrl),
              "Mix: Ctrl + knob is left to Windows (= master volume, with the Windows flyout)");

        // function Volume
        s.KnobTarget = KnobTarget.Volume;
        k.UpdateConfig(KnobConfig.From(s));
        Check(!k.HandleKey(KeyboardHook.VK_VOLUME_UP, true, false), "Volume: rotation left to Windows (native volume flyout)");
        k.HandleKey(KeyboardHook.VK_VOLUME_UP, false, false);
        Check(Bound(KeyboardHook.VK_VOLUME_UP, HotkeyModifiers.Ctrl, KnobActionKind.Rotate, KnobTarget.Mix), "Volume: Ctrl + knob drives the mix");
        Check(!k.Bindings.Any(b => b.Vk == KeyboardHook.VK_VOLUME_MUTE && b.Mods == HotkeyModifiers.None), "Volume: press left to Windows (native mute)");

        // system hot key path
        int id = Array.FindIndex(k.Bindings.ToArray(), b => b.Action.Kind == KnobActionKind.CycleFunction) + 1;
        int before = actions.Count;
        k.HandleHotkey(id);
        Check(actions.Count == before + 1 && actions[^1].Kind == KnobActionKind.CycleFunction, "a registered hot key dispatches its action");

        // disabled / dedicated keys
        s.KnobEnabled = false;
        s.KeyMixGame = 0x7C; // F13
        k.UpdateConfig(KnobConfig.From(s));
        Check(!k.HandleKey(KeyboardHook.VK_VOLUME_MUTE, true, false), "disabled: every knob key goes to Windows");
        k.HandleKey(KeyboardHook.VK_VOLUME_MUTE, false, false);
        Check(k.HandleKey(0x7C, true, false) && actions[^1].Kind == KnobActionKind.MixTowardsGame, "dedicated key F13 -> towards Game (also when the knob is off)");
        k.HandleKey(0x7C, false, false);
        Check(!k.HandleKey(0x41, true, false), "normal keys untouched");

        var (file, args) = AutoStart.Command;
        W($"   autostart: {(AutoStart.IsEnabled ? (AutoStart.IsCurrent ? "task registered" : "task registered for another location") : "off")}  ->  {System.IO.Path.GetFileName(file)} {args}");
    }

    private static void ResamplerTest()
    {
        const int rate = 48000;
        foreach (var (freq, step) in new[] { (997.0, 1.00002), (997.0, 0.9998), (10000.0, 1.00005), (15000.0, 1.0002), (20000.0, 0.9997) })
        {
            var r = new SincResampler(1, 1.01);
            int total = rate * 2;
            var input = new float[total];
            for (int i = 0; i < total; i++) input[i] = (float)(0.5 * Math.Sin(2 * Math.PI * freq * i / rate));
            var outBuf = new float[1][] { new float[512] };
            double errSq = 0, sigSq = 0;
            int n = 0, fed = 0;
            while (fed < total)
            {
                int chunk = Math.Min(480, total - fed);
                r.Write(input.AsSpan(fed, chunk), chunk);
                fed += chunk;
                int got = r.Read(outBuf, 480, step);
                for (int j = 0; j < got; j++, n++)
                {
                    if (n < 2000 || n * step > total - 200) continue;
                    double ideal = 0.5 * Math.Sin(2 * Math.PI * freq * (n * step) / rate);
                    double e = outBuf[0][j] - ideal;
                    errSq += e * e;
                    sigSq += ideal * ideal;
                }
            }
            double snr = 10 * Math.Log10(sigSq / Math.Max(errSq, 1e-30));
            Check(snr > 120, $"{freq / 1000:0.###} kHz sine, drift ratio {step}: error {-snr:0.0} dB below signal");
        }

        // Ratio exactly 1: samples must come out bit-identical.
        var exact = new SincResampler(2, 1.01);
        var data = new float[2 * 1000];
        var rnd = new Random(1);
        for (int i = 0; i < data.Length; i++) data[i] = (float)(rnd.NextDouble() * 2 - 1);
        exact.Write(data, 1000);
        var o = new float[2][] { new float[1000], new float[1000] };
        int produced = exact.Read(o, 1000, 1.0);
        bool same = produced > 800;
        for (int i = 0; i < produced && same; i++) same = o[0][i] == data[2 * i] && o[1][i] == data[2 * i + 1];
        Check(same, $"ratio 1:1 is bit-exact ({produced} frames compared)");
    }

    private static void ChannelMapTest()
    {
        var s51 = new AudioFormat(6, 48000, 32, 32, true, 0x3F);
        var st = new AudioFormat(2, 48000, 32, 32, true, 0x3);
        var s71 = new AudioFormat(8, 48000, 32, 32, true, 0x63F);
        Check(ChannelMap.Create(s51, s51).IsIdentity, "5.1 -> 5.1 is a straight copy");
        Check(ChannelMap.Create(st, st).IsIdentity, "stereo -> stereo is a straight copy");

        var planar = new float[2][] { new[] { 0.25f }, new[] { -0.5f } };
        var mix = new float[6];
        ChannelMap.Create(st, s51).MixInto(planar, mix, 1, 6, 1f, 1f);
        Check(mix[0] == 0.25f && mix[1] == -0.5f && mix[2] == 0 && mix[3] == 0 && mix[4] == 0 && mix[5] == 0,
              "stereo on 5.1: front L/R only, untouched values");

        var p71 = new float[8][];
        for (int i = 0; i < 8; i++) p71[i] = new[] { 0.1f * (i + 1) };
        var mix51 = new float[6];
        ChannelMap.Create(s71, s51).MixInto(p71, mix51, 1, 6, 1f, 1f);
        Check(mix51[4] > 0.5f && mix51[5] > 0.6f, $"7.1 -> 5.1 folds side+back into surround ({string.Join(", ", mix51.Select(v => v.ToString("0.00")))})");
    }

    private static void DevicesTest()
    {
        var list = Devices.ListRender();
        Check(list.Count > 0, $"{list.Count} active playback device(s)");
        foreach (var d in list)
        {
            var fmt = Devices.GetDeviceFormat(d.Id)?.Describe() ?? "?";
            var probe = Devices.ProbeOpen(d.Id);
            W($"   - {d.Name}{(d.IsDefault ? "  [default]" : "")}{(d.IsVbCable ? "  [VB-CABLE]" : "")}{(d.IsAe5 ? "  [AE-5]" : "")}  {fmt}  " +
              (probe == null ? "opens OK" : $"OPEN FAILS 0x{probe:X8}"));
        }
        var preferred = Devices.FindPreferredOutput(list);
        W($"   preferred output: {preferred?.Name ?? "(none found)"}");
        var (game, chat) = Devices.FindCables(list, null, null);
        W($"   cables: game = {game?.Name ?? "-"}, chat = {chat?.Name ?? "-"}");
        W($"   default console: {Devices.NameOf(Devices.GetDefaultId(Role.Console))}");
        W($"   default communications: {Devices.NameOf(Devices.GetDefaultId(Role.Communications))}");
    }

    private static void RoutingTest()
    {
        var available = AppDeviceRouting.IsAvailable;
        W($"   AudioPolicyConfig factory: {(available ? "available" : "NOT available")}");
        if (!available) return;
        var pid = (uint)Environment.ProcessId;
        var own = AppDeviceRouting.GetAppDevice(pid);
        Check(own != null, $"GetPersistedDefaultAudioEndpoint(own pid) -> '{own}' (empty = default)");

        // Round trip on DualMix's own entry only: route it to a device, read it back, reset it.
        var target = Devices.ListRender().FirstOrDefault();
        if (target == null) return;
        Check(AppDeviceRouting.SetAppDevice(pid, target.Id), $"SetPersistedDefaultAudioEndpoint(own pid -> {target.Name})");
        var back = AppDeviceRouting.GetAppDevice(pid);
        Check(string.Equals(back, target.Id, StringComparison.OrdinalIgnoreCase), $"read back '{back}'");
        Check(AppDeviceRouting.SetAppDevice(pid, null), "reset own pid to default");
        Check(AppDeviceRouting.GetAppDevice(pid) == string.Empty, "own pid back on default");
    }

    private static void DefaultDeviceTest()
    {
        // No-op: re-apply the current default device for each role (validates the IPolicyConfig vtable).
        foreach (var role in new[] { Role.Console, Role.Multimedia, Role.Communications })
        {
            var id = Devices.GetDefaultId(role);
            if (id == null) continue;
            Check(Devices.SetDefault(id, role), $"SetDefaultEndpoint({role}) to current default (no change)");
            Check(Devices.GetDefaultId(role) == id, $"default {role} unchanged");
        }
    }

    private static void EngineTest()
    {
        // Runs the real session engine for a few seconds at mix center: every app keeps the volume it already has
        // (DualMix inherits the current Windows mixer level), so this never changes what you hear.
        var settings = SettingsStore.Load();
        var p = new SessionParams(new ChannelGains(1, 1), new RuleSet(settings), RouteDevices: false,
                                  null, null, null, null, null, MetersEnabled: true);
        var engine = new SessionEngine(p);
        engine.Start();
        Thread.Sleep(2500);
        var snap = engine.Snapshot;
        foreach (var a in snap)
            W($"   {a.DisplayName} ({a.Key}) -> {a.Channel}{(a.IsExplicit ? " [manual]" : "")}, app volume {a.RelativeVolume:P0}, " +
              $"{(a.Active ? "active" : "idle")}, peak {a.Peak:0.00}, on {a.Devices}");
        int discovered = 0;
        while (engine.TryDequeueEvent(out var ev)) if (ev is AppDiscovered) discovered++;
        Check(snap.Count > 0, $"{snap.Count} app(s) in snapshot, {discovered} discovered");
        var sw = System.Diagnostics.Stopwatch.StartNew();
        engine.Stop(true);
        Check(sw.ElapsedMilliseconds < 3000, $"engine stopped and restored in {sw.ElapsedMilliseconds} ms");
    }
    [DllImport("kernel32.dll")]
    private static extern bool AttachConsole(int pid);
}
