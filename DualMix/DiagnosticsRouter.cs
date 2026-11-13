using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using DualMix.Audio;
using DualMix.Core;
using NAudio.CoreAudioApi;
using NAudio.Wave;

namespace DualMix;

public static partial class Diagnostics
{
    private const string InstanceMutex = @"Local\DualMix.SingleInstance.7f3c2a";

    /// <summary>
    /// End-to-end measurement of the real audio path: test tones are played into the two virtual devices,
    /// the router runs exactly as in normal use, and the mix it hands to the real output is analysed.
    /// The tones are at −60 dBFS (barely audible).
    /// </summary>
    private static void RouterTest()
    {
        // The running DualMix must leave the test tones alone (no routing, no volume changes on them).
        using var auxiliary = new Mutex(false, SessionEngine.AuxiliaryMutexName((uint)Environment.ProcessId));
        bool appRunning = Mutex.TryOpenExisting(InstanceMutex, out var running);
        running?.Dispose();

        var render = Devices.ListRender();
        var (game, chat) = Devices.FindCables(render, null, null);
        var output = Devices.FindPreferredOutput(render) ?? render.FirstOrDefault(d => !d.IsVbCable && d.IsDefault)
                     ?? render.FirstOrDefault(d => !d.IsVbCable);
        if (chat == null || output == null)
        {
            W("   skipped: virtual device (or a real output) not available");
            return;
        }
        // The measurement needs the virtual devices to itself: anything else playing there would be measured as "noise".
        foreach (var bus in new[] { game, chat })
        {
            if (bus == null) continue;
            float busy = ListenFor(bus.Id, 500);
            if (busy > 1e-6f)
            {
                W($"   skipped: something is playing on '{bus.Name}' (peak {busy:0.0000}). Pause it and run the diagnostics again.");
                return;
            }
        }
        if (appRunning) W("   DualMix is running: its own mixer also plays the test tones (still far below audibility)");
        W(game != null
            ? "   layout: Game and Chat both through virtual devices"
            : "   layout: Chat through 'DualMix Chat'; Game plays directly on the output (nothing to measure: untouched path)");

        const double amp = 0.001; // −60 dBFS
        const double fGameFront = 997, fGameBack = 2003, fChat = 1503;
        int quick = Environment.GetCommandLineArgs().Contains("--quick", StringComparer.OrdinalIgnoreCase) ? 1 : 0;
        double tSettleA = 10, tEndA = 25, tStartB = quick == 1 ? 25 : 46, tEndB = quick == 1 ? 40 : 61, tGain = tEndB + 0.5, tEnd = tEndB + 4.5;

        using var en = new MMDeviceEnumerator();
        using var gameDev = game != null ? en.GetDevice(game.Id) : null;
        using var chatDev = en.GetDevice(chat.Id);
        var gameFmt = gameDev?.AudioClient.MixFormat;
        var chatFmt = chatDev.AudioClient.MixFormat;
        var outFmt = Devices.GetDeviceFormat(output.Id);
        bool surround = gameFmt != null && gameFmt.Channels >= 6 && outFmt != null && outFmt.Channels >= 6;

        (double f, double a)[]? gameTones = null;
        if (gameFmt != null)
        {
            gameTones = new (double f, double a)[gameFmt.Channels];
            gameTones[0] = (fGameFront, amp);
            if (surround) gameTones[4] = (fGameBack, amp);
        }
        var chatTones = new (double f, double a)[chatFmt.Channels];
        // Chat on the right speaker only when Game shares the left one; on both otherwise.
        if (gameFmt == null || surround) chatTones[0] = (fChat, amp);
        if (chatFmt.Channels > 1) chatTones[1] = (fChat, amp);

        var router = new AudioRouter();
        bool trace = Environment.GetCommandLineArgs().Contains("--trace", StringComparer.OrdinalIgnoreCase);
        if (trace)
        {
            router.GameTrace = new List<double[]>();
            router.ChatTrace = new List<double[]>();
        }
        float[][]? rec = null;
        int recPos = 0, outCh = 0, outRate = 48000;
        float strayPeak = 0;
        int[] watch = surround ? new[] { 0, 1, 4 } : new[] { 0, 1 };
        router.Tap = (mix, frames, channels) =>
        {
            if (rec == null)
            {
                outCh = channels;
                rec = new float[watch.Length][];
                for (int i = 0; i < rec.Length; i++) rec[i] = new float[(int)(outRate * (tEnd + 3))];
            }
            int room = Math.Min(frames, rec[0].Length - recPos);
            for (int f = 0; f < room; f++)
            {
                int b = f * channels;
                for (int i = 0; i < watch.Length; i++)
                {
                    if (watch[i] < channels) rec[i][recPos + f] = mix[b + watch[i]];
                }
                for (int c = 0; c < channels; c++)
                {
                    if (Array.IndexOf(watch, c) >= 0) continue;
                    float a = Math.Abs(mix[b + c]);
                    if (a > strayPeak) strayPeak = a;
                }
            }
            recPos += room;
        };

        using var gamePlayer = gameDev != null ? new WasapiOut(gameDev, AudioClientShareMode.Shared, true, 60) : null;
        using var chatPlayer = new WasapiOut(chatDev, AudioClientShareMode.Shared, true, 60);
        gamePlayer?.Init(new ToneProvider(gameFmt!, gameTones!));
        chatPlayer.Init(new ToneProvider(chatFmt, chatTones));

        long underrunsAfterStart = -1;
        try
        {
            gamePlayer?.Play();
            chatPlayer.Play();
            router.SetGains(new ChannelGains(1f, 1f));
            router.Start(game?.Id, chat.Id, output.Id, LatencyProfile.For(SettingsStore.Load().Latency));

            var sw = System.Diagnostics.Stopwatch.StartNew();
            bool gainsChanged = false;
            while (sw.Elapsed.TotalSeconds < tEnd)
            {
                Thread.Sleep(100);
                if (router.State == RouterState.Error) throw new InvalidOperationException("router: " + router.LastError);
                if (router.State == RouterState.Starting && sw.Elapsed.TotalSeconds > 8)
                    throw new TimeoutException($"the router did not start: blocked during '{router.Stage}' " +
                                               "(a security product may be holding the audio capture of this program)");
                if (underrunsAfterStart < 0 && sw.Elapsed.TotalSeconds > 3) underrunsAfterStart = router.Underruns;
                if (!gainsChanged && sw.Elapsed.TotalSeconds >= tGain)
                {
                    router.SetGains(new ChannelGains(0.5f, 0.25f));
                    gainsChanged = true;
                }
            }
        }
        finally
        {
            router.Stop();
            try { gamePlayer?.Stop(); } catch { /* ignore */ }
            try { chatPlayer.Stop(); } catch { /* ignore */ }
        }

        outRate = router.OutputFormat?.SampleRate ?? outRate;
        if (trace)
        {
            foreach (var (name, list) in new[] { ("game", router.GameTrace), ("chat", router.ChatTrace) })
            {
                if (list == null || list.Count == 0) continue;
                var lines = new List<string> { "t,pending,accrued,v,vsmooth,err_us,adj_ppm,int_ppm,frames,packets,clock" };
                double t0 = list[0][0];
                foreach (var r in list)
                    lines.Add(string.Join(",", r.Select((x, i) => (i == 0 || i == 10 ? x - t0 : x).ToString("0.######", System.Globalization.CultureInfo.InvariantCulture))));
                System.IO.File.WriteAllLines(System.IO.Path.Combine(AppPaths.DataDir, $"trace_{name}.csv"), lines);
            }
            if (rec != null)
            {
                // Raw recording of the analysed channels (float32) for offline inspection.
                for (int i = 0; i < rec.Length; i++)
                {
                    var bytes = new byte[recPos * 4];
                    Buffer.BlockCopy(rec[i], 0, bytes, 0, bytes.Length);
                    System.IO.File.WriteAllBytes(System.IO.Path.Combine(AppPaths.DataDir, $"trace_rec{i}.f32"), bytes);
                }
            }
        }
        W($"   output '{output.Name}': {router.OutputFormat?.Describe()}; chat {router.ChatFormat?.Describe()}" +
          (game != null ? $"; game {router.GameFormat?.Describe()}" : ""));
        W($"   added latency (estimate): {router.LatencyMs:0} ms; recorded {recPos / (double)outRate:0.0} s of the output mix");
        if (rec == null || recPos < outRate * (tEndB + 1))
        {
            Check(false, "the router produced audio for the whole test");
            return;
        }

        Check(router.Underruns - Math.Max(0, underrunsAfterStart) == 0,
              $"no dropouts ({router.Underruns - Math.Max(0, underrunsAfterStart)} underruns after start-up, {Math.Max(0, underrunsAfterStart)} during start-up)");
        Check(strayPeak == 0f, $"channels without signal stay digital silence (peak {strayPeak:0.#########})");

        double gameRatio = (double)(router.GameFormat?.SampleRate ?? outRate) / outRate;
        double chatRatio = (double)(router.ChatFormat?.SampleRate ?? outRate) / outRate;
        int iBack = surround ? 2 : -1;

        foreach (var (label, t0, t1) in new[] { ("settling (10–25 s)", tSettleA, tEndA), ("steady state", tStartB, tEndB) })
        {
            W($"   -- {label}");
            // Chat chain: FR carries the chat tone only.
            var chatFit = Analyse(rec[1], (int)(t0 * outRate), (int)(t1 * outRate), fChat * chatRatio, outRate);
            Report("Chat", chatFit, amp, 1.0);
            if (game == null)
            {
                // Chat only: the left speaker carries the same tone.
                Report("Chat (left)", Analyse(rec[0], (int)(t0 * outRate), (int)(t1 * outRate), fChat * chatRatio, outRate), amp, 1.0);
                continue;
            }
            // Game chain: the back channel (surround) or FL (stereo) carries a game tone only.
            ToneStats gameFit;
            if (surround)
            {
                gameFit = Analyse(rec[iBack], (int)(t0 * outRate), (int)(t1 * outRate), fGameBack * gameRatio, outRate);
                Report("Game (surround channel)", gameFit, amp, 1.0);
                // FL = game tone + chat tone: the sum must be exactly the two tones (each window uses the pitch
                // measured in the same window on the channels that carry one tone only).
                var f1 = gameFit.Freqs.Select(f => f * fGameFront / fGameBack).ToArray();
                var f2 = chatFit.Freqs;
                var sum = AnalyseTwo(rec[0], (int)(t0 * outRate), (int)(t1 * outRate), f1, f2, outRate);
                Check(sum.residualDb < -95 && Math.Abs(Db(sum.a1 / amp)) < 0.002 && Math.Abs(Db(sum.a2 / amp)) < 0.002,
                      $"Game + Chat summed on the same speaker: levels {Db(sum.a1 / amp):+0.0000;-0.0000} / {Db(sum.a2 / amp):+0.0000;-0.0000} dB, " +
                      $"everything else {sum.residualDb:0.0} dB below the tones");
            }
            else
            {
                gameFit = Analyse(rec[0], (int)(t0 * outRate), (int)(t1 * outRate), fGameFront * gameRatio, outRate);
                Report("Game", gameFit, amp, 1.0);
            }
        }

        // Gains: Game x0.5 and Chat x0.25 must come out as -6.0206 dB and -12.0412 dB.
        int g0 = (int)((tGain + 1.0) * outRate), g1 = (int)((tEnd - 0.3) * outRate);
        var chatGain = Analyse(rec[1], g0, g1, fChat * chatRatio, outRate);
        double cdb = Db(chatGain.MeanAmp / amp);
        if (game == null)
        {
            Check(Math.Abs(cdb - Db(0.25)) < 0.002, $"gain accuracy: Chat x0.25 -> {cdb:0.0000} dB (ideal {Db(0.25):0.0000})");
        }
        else
        {
            var gameGain = Analyse(rec[surround ? iBack : 0], g0, g1, (surround ? fGameBack : fGameFront) * gameRatio, outRate);
            double gdb = Db(gameGain.MeanAmp / amp);
            Check(Math.Abs(gdb - Db(0.5)) < 0.002 && Math.Abs(cdb - Db(0.25)) < 0.002,
                  $"gain accuracy: Game x0.5 -> {gdb:0.0000} dB (ideal {Db(0.5):0.0000}), Chat x0.25 -> {cdb:0.0000} dB (ideal {Db(0.25):0.0000})");
        }
    }

    /// <summary>
    /// Torture test: for 30 seconds the test tones are started and stopped over and over on both virtual devices
    /// (what games and chat programs do all the time), while every CPU core is kept busy and the garbage collector is
    /// provoked. The engine must get through it without a single dropout.
    /// </summary>
    private static void RouterStressTest()
    {
        using var auxiliary = new Mutex(false, SessionEngine.AuxiliaryMutexName((uint)Environment.ProcessId) + ".stress");
        var render = Devices.ListRender();
        var (game, chat) = Devices.FindCables(render, null, null);
        var output = Devices.FindPreferredOutput(render) ?? render.FirstOrDefault(d => !d.IsVbCable);
        if (chat == null || output == null)
        {
            W("   skipped: virtual device (or a real output) not available");
            return;
        }

        const int seconds = 30;
        int burners = Math.Max(2, Environment.ProcessorCount - 2);
        // "--stress-gc" also floods the garbage collector from every core (far beyond anything DualMix itself does).
        bool garbage = Environment.GetCommandLineArgs().Contains("--stress-gc", StringComparer.OrdinalIgnoreCase);
        using var en = new MMDeviceEnumerator();
        using var gameDev = game != null ? en.GetDevice(game.Id) : null;
        using var chatDev = en.GetDevice(chat.Id);
        var router = new AudioRouter();
        var stop = new ManualResetEventSlim(false);
        long starts = 0;

        void Churn(MMDevice dev, double freq, int onMs, int offMs)
        {
            var fmt = dev.AudioClient.MixFormat;
            var tones = new (double f, double a)[fmt.Channels];
            tones[0] = (freq, 0.001);
            var rnd = new Random((int)freq);
            while (!stop.IsSet)
            {
                try
                {
                    using var player = new WasapiOut(dev, AudioClientShareMode.Shared, true, 60);
                    player.Init(new ToneProvider(fmt, tones));
                    player.Play();
                    Interlocked.Increment(ref starts);
                    stop.Wait(onMs + rnd.Next(onMs));
                    player.Stop();
                }
                catch (Exception ex)
                {
                    W("   stream error: " + ex.Message);
                    stop.Wait(500);
                }
                stop.Wait(offMs + rnd.Next(offMs));
            }
        }

        var threads = new List<Thread>();
        if (gameDev != null) threads.Add(new Thread(() => Churn(gameDev, 997, 500, 150)) { IsBackground = true });
        threads.Add(new Thread(() => Churn(chatDev, 1503, 250, 300)) { IsBackground = true });
        // CPU load: one busy thread per logical core at normal priority, plus garbage for the collector.
        for (int i = 0; i < burners; i++)
        {
            threads.Add(new Thread(() =>
            {
                var junk = new List<byte[]>();
                double x = 1;
                while (!stop.IsSet)
                {
                    for (int k = 0; k < 200_000; k++) x = Math.Sqrt(x + k);
                    if (!garbage) continue;
                    junk.Add(new byte[64 * 1024]);
                    if (junk.Count > 200) junk.Clear();
                }
            }) { IsBackground = true, Priority = ThreadPriority.Normal });
        }

        try
        {
            router.SetGains(new ChannelGains(1f, 1f));
            router.Start(game?.Id, chat.Id, output.Id, LatencyProfile.For(SettingsStore.Load().Latency));
            var sw = System.Diagnostics.Stopwatch.StartNew();
            while (router.State == RouterState.Starting && sw.Elapsed.TotalSeconds < 8) Thread.Sleep(50);
            if (router.State != RouterState.Running) throw new InvalidOperationException("router did not start: " + (router.LastError ?? router.Stage));
            Thread.Sleep(1500);
            foreach (var t in threads) t.Start();
            while (sw.Elapsed.TotalSeconds < seconds + 1.5)
            {
                Thread.Sleep(200);
                if (router.State == RouterState.Error) throw new InvalidOperationException("router: " + router.LastError);
            }
        }
        finally
        {
            stop.Set();
            foreach (var t in threads)
            {
                if (t.IsAlive) t.Join(3000);
            }
            Thread.Sleep(300);
            router.Stop();
        }

        W($"   load: CPU{(garbage ? " + garbage collector flood" : "")}");
        W($"   {seconds} s, {burners} busy cores of {Environment.ProcessorCount}, {Interlocked.Read(ref starts)} stream starts/stops; real-time scheduling: {(router.RealTimeScheduling ? "yes" : "NO")}");
        W($"   {router.DropoutDetail}");
        Check(router.Underruns == 0, $"no dropouts under stress ({router.Underruns})");
        Check(router.MaxLateMs < 8, $"audio thread always on time (worst delay {router.MaxLateMs:0.0} ms, limit before a dropout ≈ 10 ms)");
    }

    /// <summary>Peak level currently playing on a playback device (loopback), over a short time.</summary>
    private static float ListenFor(string deviceId, int milliseconds)
    {
        using var en = new MMDeviceEnumerator();
        using var dev = en.GetDevice(deviceId);
        float peak = 0;
        using var capture = new WasapiLoopbackCapture(dev);
        capture.DataAvailable += (_, e) =>
        {
            var wb = new WaveBuffer(e.Buffer);
            int n = e.BytesRecorded / 4;
            for (int i = 0; i < n; i++) peak = Math.Max(peak, Math.Abs(wb.FloatBuffer[i]));
        };
        capture.StartRecording();
        Thread.Sleep(milliseconds);
        capture.StopRecording();
        Thread.Sleep(100);
        return peak;
    }

    private static void Report(string label, ToneStats s, double amp, double gain)
    {
        double levelDb = Db(s.MeanAmp / (amp * gain));
        Check(Math.Abs(levelDb) < 0.002, $"{label}: level {levelDb:+0.0000;-0.0000} dB (unity = 0)");
        Check(s.ResidualDb < -95, $"{label}: noise + distortion {s.ResidualDb:0.0} dB below the tone");
        Check(s.WowPpm < 20 && Math.Abs(s.DriftPpm) < 1500,
              $"{label}: clock correction {s.DriftPpm:+0.0;-0.0} ppm, pitch stability ±{s.WowPpm:0.00} ppm rms");
    }

    private static double Db(double ratio) => 20 * Math.Log10(Math.Max(ratio, 1e-12));

    private readonly record struct ToneStats(double MeanAmp, double MeanFreq, double DriftPpm, double WowPpm, double ResidualDb, double[] Freqs);

    /// <summary>Sine fit (amplitude, phase, offset, frequency) on consecutive short windows.</summary>
    private static ToneStats Analyse(float[] x, int from, int to, double f0, int rate)
    {
        const int n = 2048;
        var amps = new List<double>();
        var freqs = new List<double>();
        double resPow = 0, sigPow = 0;
        for (int start = from; start + n <= to; start += n)
        {
            // Golden-section search of the frequency that minimises the residual of a 3-parameter fit.
            double lo = f0 * (1 - 0.002), hi = f0 * (1 + 0.002);
            const double phi = 0.6180339887498949;
            double c = hi - phi * (hi - lo), d = lo + phi * (hi - lo);
            var rc = Fit(x, start, n, c, rate);
            var rd = Fit(x, start, n, d, rate);
            for (int it = 0; it < 60 && hi - lo > f0 * 1e-10; it++)
            {
                if (rc.res < rd.res)
                {
                    hi = d; d = c; rd = rc;
                    c = hi - phi * (hi - lo);
                    rc = Fit(x, start, n, c, rate);
                }
                else
                {
                    lo = c; c = d; rc = rd;
                    d = lo + phi * (hi - lo);
                    rd = Fit(x, start, n, d, rate);
                }
            }
            var best = rc.res < rd.res ? rc : rd;
            double f = rc.res < rd.res ? c : d;
            amps.Add(best.amp);
            freqs.Add(f);
            // Residual computed sample by sample (the closed form loses precision 120 dB down).
            double w = 2 * Math.PI * f / rate, mid = (n - 1) / 2.0, r2 = 0;
            for (int i = 0; i < n; i++)
            {
                double ph = w * (i - mid);
                double e = x[start + i] - (best.a * Math.Cos(ph) + best.b * Math.Sin(ph) + best.c);
                r2 += e * e;
            }
            resPow += r2;
            sigPow += best.amp * best.amp / 2 * n;
        }
        if (amps.Count == 0) return new ToneStats(0, f0, 0, 0, 0, Array.Empty<double>());
        double meanF = freqs.Average();
        double wow = Math.Sqrt(freqs.Sum(f => (f - meanF) * (f - meanF)) / freqs.Count) / meanF * 1e6;
        return new ToneStats(amps.Average(), meanF, (meanF / f0 - 1) * 1e6, wow, 10 * Math.Log10(Math.Max(resPow, 1e-40) / Math.Max(sigPow, 1e-40)), freqs.ToArray());
    }

    /// <summary>Least-squares fit of A·cos + B·sin + C at a fixed frequency; returns amplitude and residual energy.</summary>
    private static (double amp, double res, double a, double b, double c) Fit(float[] x, int start, int n, double f, int rate)
    {
        double w = 2 * Math.PI * f / rate;
        double scc = 0, sss = 0, scs = 0, sc = 0, ss = 0, sxc = 0, sxs = 0, sx = 0, sxx = 0;
        double mid = (n - 1) / 2.0;
        for (int i = 0; i < n; i++)
        {
            double ph = w * (i - mid);
            double c = Math.Cos(ph), s = Math.Sin(ph), v = x[start + i];
            scc += c * c; sss += s * s; scs += c * s; sc += c; ss += s;
            sxc += v * c; sxs += v * s; sx += v; sxx += v * v;
        }
        // Normal equations for [A B C].
        var m = new double[3, 4]
        {
            { scc, scs, sc, sxc },
            { scs, sss, ss, sxs },
            { sc, ss, n, sx },
        };
        Solve(m, 3);
        double a = m[0, 3], b = m[1, 3], c0 = m[2, 3];
        double res = sxx - (a * sxc + b * sxs + c0 * sx);
        return (Math.Sqrt(a * a + b * b), Math.Max(res, 0), a, b, c0);
    }

    /// <summary>Two tones of known frequency in one channel: amplitudes and what is left over.</summary>
    private static (double a1, double a2, double residualDb) AnalyseTwo(float[] x, int from, int to, double[] f1, double[] f2, int rate)
    {
        const int n = 2048;
        double a1 = 0, a2 = 0, resPow = 0, sigPow = 0;
        int windows = 0;
        for (int start = from; start + n <= to && windows < f1.Length && windows < f2.Length; start += n)
        {
            double w1 = 2 * Math.PI * f1[windows] / rate, w2 = 2 * Math.PI * f2[windows] / rate, mid = (n - 1) / 2.0;
            var m = new double[5, 6];
            var basis = new double[5];
            for (int i = 0; i < n; i++)
            {
                double t = i - mid;
                basis[0] = Math.Cos(w1 * t); basis[1] = Math.Sin(w1 * t);
                basis[2] = Math.Cos(w2 * t); basis[3] = Math.Sin(w2 * t);
                basis[4] = 1;
                double v = x[start + i];
                for (int r = 0; r < 5; r++)
                {
                    for (int c = 0; c < 5; c++) m[r, c] += basis[r] * basis[c];
                    m[r, 5] += basis[r] * v;
                }
            }
            Solve(m, 5);
            double p1 = Math.Sqrt(m[0, 5] * m[0, 5] + m[1, 5] * m[1, 5]);
            double p2 = Math.Sqrt(m[2, 5] * m[2, 5] + m[3, 5] * m[3, 5]);
            a1 += p1; a2 += p2;
            double r2 = 0;
            for (int i = 0; i < n; i++)
            {
                double t = i - mid;
                double model = m[0, 5] * Math.Cos(w1 * t) + m[1, 5] * Math.Sin(w1 * t) + m[2, 5] * Math.Cos(w2 * t) + m[3, 5] * Math.Sin(w2 * t) + m[4, 5];
                double e = x[start + i] - model;
                r2 += e * e;
            }
            resPow += r2;
            sigPow += (p1 * p1 + p2 * p2) / 2 * n;
            windows++;
        }
        if (windows == 0) return (0, 0, 0);
        return (a1 / windows, a2 / windows, 10 * Math.Log10(Math.Max(resPow, 1e-40) / Math.Max(sigPow, 1e-40)));
    }

    /// <summary>Gauss-Jordan elimination with partial pivoting on an augmented matrix.</summary>
    private static void Solve(double[,] m, int n)
    {
        for (int col = 0; col < n; col++)
        {
            int pivot = col;
            for (int r = col + 1; r < n; r++)
            {
                if (Math.Abs(m[r, col]) > Math.Abs(m[pivot, col])) pivot = r;
            }
            if (pivot != col)
            {
                for (int c = 0; c <= n; c++) (m[col, c], m[pivot, c]) = (m[pivot, c], m[col, c]);
            }
            double p = m[col, col];
            if (Math.Abs(p) < 1e-300) continue;
            for (int c = 0; c <= n; c++) m[col, c] /= p;
            for (int r = 0; r < n; r++)
            {
                if (r == col) continue;
                double k = m[r, col];
                if (k == 0) continue;
                for (int c = 0; c <= n; c++) m[r, c] -= k * m[col, c];
            }
        }
    }

    /// <summary>Plays one sine per channel (frequency, amplitude) in the device mix format (32-bit float).</summary>
    private sealed class ToneProvider : IWaveProvider
    {
        private readonly (double f, double a)[] _tones;
        private readonly double[] _phase;

        public WaveFormat WaveFormat { get; }

        public ToneProvider(WaveFormat format, (double f, double a)[] tones)
        {
            WaveFormat = format;
            _tones = tones;
            _phase = new double[tones.Length];
        }

        public int Read(byte[] buffer, int offset, int count)
        {
            var wb = new WaveBuffer(buffer);
            int ch = WaveFormat.Channels;
            int frames = count / 4 / ch;
            int o = offset / 4;
            double rate = WaveFormat.SampleRate;
            for (int c = 0; c < ch; c++)
            {
                var (f, a) = c < _tones.Length ? _tones[c] : (0, 0);
                if (a == 0)
                {
                    for (int i = 0; i < frames; i++) wb.FloatBuffer[o + i * ch + c] = 0f;
                    continue;
                }
                double ph = _phase[c], inc = 2 * Math.PI * f / rate;
                for (int i = 0; i < frames; i++)
                {
                    wb.FloatBuffer[o + i * ch + c] = (float)(a * Math.Sin(ph));
                    ph += inc;
                    if (ph > 2 * Math.PI) ph -= 2 * Math.PI;
                }
                _phase[c] = ph;
            }
            return frames * ch * 4;
        }
    }
}
