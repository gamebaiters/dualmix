using System;
using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Threading;
using DualMix.Core;
using NAudio.CoreAudioApi;
using NAudio.Wave;

namespace DualMix.Audio;

public enum RouterState
{
    Stopped,
    Starting,
    Running,
    Error,
}

/// <summary>Buffering used by the router for a latency preset.</summary>
/// <param name="SourcePackets">Capture packets kept in each source buffer on top of the block being played.</param>
/// <param name="SourceMarginMs">Extra safety margin in each source buffer.</param>
/// <param name="PaddingPeriods">Audio kept queued in the output buffer, in device periods (≥ 1).</param>
/// <param name="BufferMs">Output buffer capacity to ask Windows for (room for the queue to grow when needed).</param>
/// <param name="ExtraQueueMs">Output queue added on top, learned from past dropouts on this PC.</param>
/// <param name="ExtraSourceMs">Source reserve added on top, learned from past dropouts on this PC.</param>
public readonly record struct LatencyProfile(double SourcePackets, int SourceMarginMs, double PaddingPeriods, int BufferMs,
                                             double ExtraQueueMs = 0, double ExtraSourceMs = 0)
{
    public static LatencyProfile For(LatencyMode m) => m switch
    {
        LatencyMode.Minima => new(1.0, 2, 1.5, 100),
        LatencyMode.Sicura => new(3.0, 10, 4.0, 160),
        _ => new(2.0, 4, 2.0, 120),
    };
}

/// <summary>
/// Virtual-devices engine. Two goals, in this order: never stutter, and be transparent.
/// <list type="bullet">
/// <item>Each virtual playback device ("DualMix Chat", and "DualMix Game" when a second cable exists) is captured
/// with WASAPI loopback, i.e. the 32-bit float mix of the Windows engine, before any conversion to the device
/// format of the cable and independent of its volume.</item>
/// <item>Buffers are controlled on their <b>real</b> fill level, with deliberately generous margins: what is
/// buffered is always at least one block plus a safety reserve. If a dropout ever happens, the reserve of the part
/// that ran dry grows by itself (and stays larger), so a given PC converges to the smallest buffering that never
/// stutters on it.</item>
/// <item>Clock drift between the devices is absorbed by a 160-tap windowed-sinc resampler whose ratio follows the
/// averaged buffer level through a very slow, critically damped PI loop (a few ppm, changing over minutes).</item>
/// <item>Channels are mapped by speaker position (5.1/7.1 stays surround). Gains are applied in 32-bit float
/// with short linear ramps; 100% is an exact ×1. No limiter, no compressor: the mix is handed to Windows in its
/// native float format.</item>
/// <item>After ten minutes of digital silence the output stream is paused (so the PC can sleep) and restarted as
/// soon as something plays.</item>
/// </list>
/// Everything runs on one MMCSS "Pro Audio" thread woken by the output and capture events; nothing is allocated,
/// logged or locked on that thread while audio flows.
/// </summary>
public sealed class AudioRouter : IDisposable
{
    private const int IdleSuspendMs = 600_000;
    private const float SilenceThreshold = 1e-6f; // −120 dBFS

    private sealed class RunToken
    {
        public volatile bool Stop;
    }

    private Thread? _thread;
    private Thread? _stuck;
    private RunToken _run = new();
    private volatile float _gameGain = 1f, _chatGain = 1f;
    private long _underruns, _outputStarved, _gameEmpty, _chatEmpty;
    private volatile string? _note;

    public RouterState State { get; private set; } = RouterState.Stopped;
    public string? LastError { get; private set; }
    public int LastErrorCode { get; private set; }
    public float GamePeak { get; private set; }
    public float ChatPeak { get; private set; }
    public float OutputPeak { get; private set; }
    public double LatencyMs { get; private set; }
    public double GameDriftPpm { get; private set; }
    public double ChatDriftPpm { get; private set; }
    public bool Suspended { get; private set; }
    public AudioFormat? OutputFormat { get; private set; }
    public AudioFormat? GameFormat { get; private set; }
    public AudioFormat? ChatFormat { get; private set; }
    public string? GameId { get; private set; }
    public string? ChatId { get; private set; }
    public string? OutputId { get; private set; }
    public long StartedAt { get; private set; }

    /// <summary>Dropouts of any kind since the router started.</summary>
    public long Underruns => Interlocked.Read(ref _underruns);

    /// <summary>The output device found its queue empty (this thread was scheduled too late).</summary>
    public long OutputStarved => Interlocked.Read(ref _outputStarved);

    /// <summary>A source buffer ran dry while its device was still producing audio.</summary>
    public long GameEmpty => Interlocked.Read(ref _gameEmpty);
    public long ChatEmpty => Interlocked.Read(ref _chatEmpty);

    /// <summary>Longest delay (ms) between the moment the output asked for audio and this thread running, recently.</summary>
    public double MaxLateMs { get; private set; }

    /// <summary>Extra buffering this PC turned out to need (kept by the controller across restarts).</summary>
    public double LearnedQueueMs { get; private set; }
    public double LearnedSourceMs { get; private set; }

    /// <summary>Whether the thread got its real-time scheduling class from Windows (MMCSS "Pro Audio").</summary>
    public bool RealTimeScheduling { get; private set; }

    public string DropoutDetail =>
        $"uscita a secco {OutputStarved}, buffer Game vuoto {GameEmpty}, buffer Chat vuoto {ChatEmpty}, ritardo massimo del thread audio {MaxLateMs:0} ms";

    /// <summary>A message from the audio thread for the log (e.g. a reserve that was enlarged). Null when there is none.</summary>
    public string? TakeNote() => Interlocked.Exchange(ref _note, null);

    /// <summary>What the router thread is doing while it starts (shown when it does not come up).</summary>
    public string Stage { get; private set; } = "";

    private long _stageSince;

    /// <summary>Marks the next start-up step; a step that took long is written to the log with its duration.</summary>
    internal void Step(string stage)
    {
        long now = Stopwatch.GetTimestamp();
        if (Stage.Length > 0 && _stageSince != 0)
        {
            double ms = (now - _stageSince) * 1000.0 / Stopwatch.Frequency;
            if (ms > 300) Log.Warn($"Router: '{Stage}' took {ms:0} ms");
        }
        Stage = stage;
        _stageSince = now;
    }

    /// <summary>Diagnostics: when set before Start, every block appends the state of the clock loop of each source.</summary>
    public System.Collections.Generic.List<double[]>? GameTrace { get; set; }
    public System.Collections.Generic.List<double[]>? ChatTrace { get; set; }

    /// <summary>Diagnostics hook: called on the audio thread with every mixed block (interleaved, frames, channels).</summary>
    public Action<float[], int, int>? Tap { get; set; }

    public bool IsRunning => _thread is { IsAlive: true };

    /// <summary>
    /// A previous start never came back from Windows (the audio service, or a security product in between, is holding
    /// the calls). No new start is attempted while that thread is stuck: piling up blocked requests only makes it worse.
    /// </summary>
    public bool Blocked => _stuck is { IsAlive: true };

    public void SetGains(ChannelGains g)
    {
        _gameGain = g.Game;
        _chatGain = g.Chat;
    }

    /// <param name="gameId">Virtual Game device, or null when games play directly on the real output.</param>
    public void Start(string? gameId, string chatId, string outputId, LatencyProfile latency)
    {
        Stop();
        if (Blocked) throw new InvalidOperationException("Il motore audio è bloccato: una richiesta precedente a Windows non è mai tornata.");
        if (outputId == gameId || outputId == chatId)
            throw new ArgumentException("L'uscita reale non può essere uno dei dispositivi virtuali (creerebbe un loop).");
        if (gameId == chatId)
            throw new ArgumentException("Game e Chat devono essere due dispositivi virtuali diversi.");

        GameId = gameId;
        ChatId = chatId;
        OutputId = outputId;
        var run = new RunToken();
        _run = run;
        Interlocked.Exchange(ref _underruns, 0);
        Interlocked.Exchange(ref _outputStarved, 0);
        Interlocked.Exchange(ref _gameEmpty, 0);
        Interlocked.Exchange(ref _chatEmpty, 0);
        MaxLateMs = 0;
        State = RouterState.Starting;
        StartedAt = Environment.TickCount64;
        LastError = null;
        LastErrorCode = 0;
        _thread = new Thread(() => Run(run, gameId, chatId, outputId, latency))
        {
            IsBackground = true,
            Name = "DualMix.Router",
            Priority = ThreadPriority.Highest,
        };
        _thread.SetApartmentState(ApartmentState.MTA);
        _thread.Start();
    }

    public void Stop()
    {
        _run.Stop = true;
        var t = _thread;
        if (t != null && t.IsAlive && !t.Join(2000))
        {
            Log.Warn($"Router thread did not stop in time (stage: '{Stage}')");
            _stuck = t;
        }
        _thread = null;
        if (State != RouterState.Error) State = RouterState.Stopped;
        GamePeak = ChatPeak = OutputPeak = 0;
        Suspended = false;
    }

    public void Dispose() => Stop();

    // -----------------------------------------------------------------------------------------

    private void Run(RunToken run, string? gameId, string chatId, string outputId, LatencyProfile latency)
    {
        IntPtr mmcss = IntPtr.Zero;
        int taskIndex = 0;
        try
        {
            mmcss = AvSetMmThreadCharacteristics("Pro Audio", ref taskIndex);
            if (mmcss != IntPtr.Zero) AvSetMmThreadPriority(mmcss, 2 /* AVRT_PRIORITY_CRITICAL */);
        }
        catch { /* avrt not available */ }
        RealTimeScheduling = mmcss != IntPtr.Zero;
        if (mmcss == IntPtr.Zero) Log.Warn("Router: Windows did not grant real-time audio scheduling (MMCSS); running at high priority instead");

        MMDeviceEnumerator? en = null;
        MMDevice? outDev = null;
        AudioClient? outClient = null;
        Source? game = null, chat = null;
        EventWaitHandle? ev = null;

        try
        {
            Step("uscita: apertura");
            en = new MMDeviceEnumerator();
            outDev = en.GetDevice(outputId);
            outClient = outDev.AudioClient;
            var outWave = outClient.MixFormat;
            var outFmt = AudioFormat.FromNAudio(outWave) ?? throw new InvalidOperationException("Formato uscita illeggibile");
            var outSample = SampleCodec.From(outFmt);
            long period100Ns = outClient.DefaultDevicePeriod;

            Step("uscita: Initialize");
            outClient.Initialize(AudioClientShareMode.Shared, AudioClientStreamFlags.EventCallback, latency.BufferMs * 10_000L, 0, outWave, Guid.Empty);
            Step("uscita: servizi");
            ev = new EventWaitHandle(false, EventResetMode.AutoReset);
            outClient.SetEventHandle(ev.SafeWaitHandle.DangerousGetHandle());
            var render = outClient.AudioRenderClient;
            int bufferFrames = outClient.BufferSize;
            int outCh = outFmt.Channels;
            int outRate = outFmt.SampleRate;
            int periodFrames = Math.Max(1, (int)Math.Round(period100Ns * outRate / 1e7));
            double periodMs = periodFrames * 1000.0 / outRate;
            int basePadding = Math.Clamp((int)Math.Round(periodFrames * latency.PaddingPeriods), Math.Min(bufferFrames, periodFrames + outRate / 1000), bufferFrames);
            int maxPadding = Math.Min(bufferFrames, Math.Max(basePadding, periodFrames * 6));
            int targetPadding = Math.Min(maxPadding, basePadding + (int)(latency.ExtraQueueMs * outRate / 1000.0));

            if (gameId != null) game = new Source(this, "Game", en, gameId, outFmt, periodFrames, latency);
            chat = new Source(this, "Chat", en, chatId, outFmt, periodFrames, latency);
            if (run.Stop) return; // stopped (or given up on) while Windows was still opening the devices
            Step("avvio dei flussi");
            if (game != null) game.Trace = GameTrace;
            chat.Trace = ChatTrace;
            OutputFormat = outFmt;
            GameFormat = game?.Format;
            ChatFormat = chat.Format;

            // Estimated latency added by DualMix: one cable engine period (loopback), the source buffer, the
            // delay of the resampling filter and the audio queued in the output buffer.
            double sourceLatency = chat.PacketMs + chat.AverageBufferMs + SincResampler.FilterDelay * 1000.0 / chat.Format.SampleRate;
            double Latency(int queued) => Math.Round(sourceLatency + (queued - periodFrames / 2.0) * 1000.0 / outRate, 0);
            LatencyMs = Latency(targetPadding);
            LearnedQueueMs = (targetPadding - basePadding) * 1000.0 / outRate;
            LearnedSourceMs = latency.ExtraSourceMs;
            Log.Info($"Router: '{outDev.FriendlyName}' {outFmt.Describe()} ({outSample}), buffer {bufferFrames}, period {periodFrames}, " +
                     $"queued {targetPadding} frames; " +
                     (game != null ? $"game '{game.Name}' {game.Format.Describe()}; " : "game direct on the output; ") +
                     $"chat '{chat.Name}' {chat.Format.Describe()}; added latency ≈ {LatencyMs} ms; real-time scheduling: {(RealTimeScheduling ? "yes" : "NO")}");

            // Pre-roll silence, then start everything.
            render.GetBuffer(targetPadding);
            render.ReleaseBuffer(targetPadding, AudioClientBufferFlags.Silent);
            game?.Start();
            chat.Start();
            outClient.Start();
            State = RouterState.Running;
            Step("");

            var handles = game != null ? new WaitHandle[] { ev, chat.Event, game.Event } : new WaitHandle[] { ev, chat.Event };
            var mix = new float[bufferFrames * outCh];
            float gameCur = _gameGain, chatCur = _chatGain;
            float gameHold = 0, chatHold = 0, outHold = 0;
            double ticksToMs = 1000.0 / Stopwatch.Frequency;
            long now = Stopwatch.GetTimestamp();
            long lastOutEvent = now, lastPublish = now, lastSignal = now, lastStarve = 0, lateWindowStart = now;
            double lateMax = 0;
            bool suspended = false;
            int warmup = 20; // blocks during which a late start is not counted as a dropout

            while (!run.Stop)
            {
                int idx = WaitHandle.WaitAny(handles, 100);
                if (run.Stop) break;
                now = Stopwatch.GetTimestamp();

                game?.Pump(now);
                chat.Pump(now);
                lastSignal = Math.Max(lastSignal, Math.Max(game?.LastSignal ?? 0, chat.LastSignal));

                if (suspended)
                {
                    if (lastSignal == now)
                    {
                        // Something started playing: bring the output back. The packet that woke us is kept.
                        outClient.Reset();
                        render.GetBuffer(targetPadding);
                        render.ReleaseBuffer(targetPadding, AudioClientBufferFlags.Silent);
                        game?.Prime();
                        chat.Prime();
                        outClient.Start();
                        suspended = false;
                        Suspended = false;
                        lastOutEvent = now;
                        warmup = 20;
                    }
                    else
                    {
                        game?.DiscardSilence();
                        chat.DiscardSilence();
                    }
                    continue;
                }

                if (idx != 0)
                {
                    // Woken by a capture event (or the timeout): nothing to render yet.
                    if ((now - lastOutEvent) * ticksToMs > 2000) throw new TimeoutException("L'uscita audio non risponde");
                    continue;
                }

                double sinceLast = (now - lastOutEvent) * ticksToMs;
                lastOutEvent = now;
                double late = sinceLast - periodMs;
                if (warmup == 0 && late > lateMax) lateMax = late;

                int padding = outClient.CurrentPadding;
                int frames = Math.Min(bufferFrames - padding, targetPadding - padding);
                if (frames <= 0) continue;
                if (warmup > 0)
                {
                    warmup--;
                }
                else if (padding == 0)
                {
                    // The device ran out of audio: this thread was kept waiting for too long.
                    Interlocked.Increment(ref _underruns);
                    Interlocked.Increment(ref _outputStarved);
                    if (lastStarve != 0 && (now - lastStarve) * ticksToMs < 120_000 && targetPadding < maxPadding)
                    {
                        // Twice within two minutes: this PC needs a longer queue. Grow it by half a period, for good.
                        targetPadding = Math.Min(maxPadding, targetPadding + periodFrames / 2);
                        frames = Math.Min(bufferFrames - padding, targetPadding - padding);
                        LatencyMs = Latency(targetPadding);
                        LearnedQueueMs = (targetPadding - basePadding) * 1000.0 / outRate;
                        _note = $"output queue enlarged to {targetPadding * 1000.0 / outRate:0} ms after repeated late wake-ups (latency now ≈ {LatencyMs} ms)";
                    }
                    lastStarve = now;
                }

                int n = frames * outCh;
                Array.Clear(mix, 0, n);
                double dt = (double)frames / outRate;

                float gameTarget = _gameGain, chatTarget = _chatGain;
                if (game != null) gameHold = Math.Max(gameHold, game.MixInto(mix, frames, gameCur, gameTarget, now, dt, ref _gameEmpty, ref _underruns));
                chatHold = Math.Max(chatHold, chat.MixInto(mix, frames, chatCur, chatTarget, now, dt, ref _chatEmpty, ref _underruns));
                gameCur = gameTarget;
                chatCur = chatTarget;
                outHold = Math.Max(outHold, Dsp.Peak(mix, n));

                Tap?.Invoke(mix, frames, outCh);

                var ptr = render.GetBuffer(frames);
                outSample.Write(mix, n, ptr);
                render.ReleaseBuffer(frames, AudioClientBufferFlags.None);

                if ((now - lastPublish) * ticksToMs >= 33)
                {
                    GamePeak = gameHold;
                    ChatPeak = chatHold;
                    OutputPeak = outHold;
                    gameHold = chatHold = outHold = 0;
                    GameDriftPpm = game?.DriftPpm ?? 0;
                    ChatDriftPpm = chat.DriftPpm;
                    lastPublish = now;
                    if (lateMax > MaxLateMs) MaxLateMs = lateMax;
                    if ((now - lateWindowStart) * ticksToMs > 600_000)
                    {
                        // "recently" = the last ten minutes or so
                        MaxLateMs = lateMax;
                        lateMax = 0;
                        lateWindowStart = now;
                    }
                }

                if ((now - lastSignal) * ticksToMs > IdleSuspendMs)
                {
                    // Nothing but digital silence for a long while: stop the output stream so Windows can idle/sleep.
                    outClient.Stop();
                    suspended = true;
                    Suspended = true;
                    GamePeak = ChatPeak = OutputPeak = 0;
                }
            }

            try { outClient.Stop(); } catch { /* already stopped */ }
            if (ReferenceEquals(_run, run)) State = RouterState.Stopped;
            Log.Info($"Router stopped (dropouts: {Underruns} [{DropoutDetail}], clock correction chat {chat.DriftPpm:+0.0;-0.0} ppm" +
                     (game != null ? $", game {game.DriftPpm:+0.0;-0.0} ppm)" : ")"));
        }
        catch (Exception ex)
        {
            if (ReferenceEquals(_run, run))
            {
                LastErrorCode = ex.HResult;
                LastError = Describe(ex);
                State = RouterState.Error;
            }
            Log.Error("Router stopped by error: " + Describe(ex), ex);
        }
        finally
        {
            game?.Dispose();
            chat?.Dispose();
            try { outClient?.Dispose(); } catch { /* ignore */ }
            try { outDev?.Dispose(); } catch { /* ignore */ }
            try { en?.Dispose(); } catch { /* ignore */ }
            ev?.Dispose();
            GamePeak = ChatPeak = OutputPeak = 0;
            Suspended = false;
            if (mmcss != IntPtr.Zero) AvRevertMmThreadCharacteristics(mmcss);
        }
    }

    public static string Describe(Exception ex)
    {
        if (ex is COMException ce)
        {
            return (uint)ce.HResult switch
            {
                0x88890004 => "dispositivo scollegato o formato cambiato",
                0x8889000A => "dispositivo in uso esclusivo da un'altra applicazione",
                0x88890008 => "formato audio non supportato",
                0x80070490 => "dispositivo non trovato",
                0x80040154 => "il servizio audio di Windows ha perso il dispositivo (serve un riavvio del servizio audio)",
                _ => $"errore audio 0x{ce.HResult:X8}",
            };
        }
        return ex.Message;
    }

    [DllImport("avrt.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr AvSetMmThreadCharacteristics(string taskName, ref int taskIndex);

    [DllImport("avrt.dll")]
    private static extern bool AvSetMmThreadPriority(IntPtr handle, int priority);

    [DllImport("avrt.dll")]
    private static extern bool AvRevertMmThreadCharacteristics(IntPtr handle);

    // ==========================================================================================

    /// <summary>One virtual device: event-driven loopback capture + drift-steered resampler + channel map.</summary>
    private sealed class Source : IDisposable
    {
        private readonly AudioRouter _owner;
        private readonly string _label;
        private readonly MMDevice _device;
        private readonly AudioClient _capture;
        private readonly AudioCaptureClient _captureClient;
        private readonly SampleCodec _codec;
        private readonly int _inCh, _inRate, _outCh;
        private readonly ChannelMap _map;
        private readonly SincResampler _resampler;
        private readonly double _nominalStep;
        private readonly double _packetFrames;
        private readonly double _baseTarget;
        private readonly double _ticksToMs;
        private readonly long _holdTicks;
        private float[] _interleaved = new float[16384];
        private readonly float[][] _planar;
        private bool _silent = true; // nothing but digital silence: the buffer is "parked" (see Pump)
        private bool _fresh;         // a sound has just started: take the fill level as it is
        private double _extra;      // reserve added after dropouts (frames)
        private double _avg;        // averaged fill level (frames)
        private double _integral;   // learned drift (survives pauses)
        private double _step;
        private long _holdUntil;
        private long _zeroRun;
        private long _dryAt;        // when the buffer was found short while packets were still arriving
        private bool _pendingDropout;

        public EventWaitHandle Event { get; } = new(false, EventResetMode.AutoReset);
        public System.Collections.Generic.List<double[]>? Trace;
        public string Name { get; }
        public AudioFormat Format { get; }
        public double DriftPpm => (_step / _nominalStep - 1) * 1e6;
        public double PacketMs => _packetFrames * 1000.0 / _inRate;

        /// <summary>Average time audio spends in this buffer (ms).</summary>
        public double AverageBufferMs { get; }

        /// <summary>Stopwatch timestamp of the last packet that carried signal (not digital silence).</summary>
        public long LastSignal { get; private set; }

        public Source(AudioRouter owner, string label, MMDeviceEnumerator en, string id, AudioFormat output, int outPeriodFrames, LatencyProfile latency)
        {
            _owner = owner;
            _label = label;
            owner.Step(label + ": apertura");
            _device = en.GetDevice(id);
            Name = _device.FriendlyName;
            var client = _device.AudioClient;
            owner.Step(label + ": formato");
            var wave = client.MixFormat;
            Format = AudioFormat.FromNAudio(wave) ?? throw new InvalidOperationException("Formato " + label + " illeggibile");
            _codec = SampleCodec.From(Format);
            _inCh = Format.Channels;
            _inRate = Format.SampleRate;
            _outCh = output.Channels;
            long period100Ns = client.DefaultDevicePeriod;
            owner.Step(label + ": Initialize (loopback)");
            try
            {
                client.Initialize(AudioClientShareMode.Shared, AudioClientStreamFlags.Loopback | AudioClientStreamFlags.EventCallback,
                                  2_000_000 /* 200 ms */, 0, wave, Guid.Empty);
                client.SetEventHandle(Event.SafeWaitHandle.DangerousGetHandle());
            }
            catch (COMException ex)
            {
                // Old Windows builds cannot signal events for loopback streams: fall back to polling
                // (the loop also wakes for the output event, every device period).
                Log.Warn($"Router: event-driven loopback unavailable on {label}, polling instead", ex);
                try { client.Dispose(); } catch { /* ignore */ }
                client = _device.AudioClient;
                client.Initialize(AudioClientShareMode.Shared, AudioClientStreamFlags.Loopback, 2_000_000, 0, wave, Guid.Empty);
            }
            owner.Step(label + ": servizi");
            _capture = client;
            _captureClient = client.AudioCaptureClient;

            _map = ChannelMap.Create(Format, output);
            _nominalStep = (double)_inRate / output.SampleRate;
            _step = _nominalStep;
            _resampler = new SincResampler(_inCh, _nominalStep * 1.003);
            _packetFrames = Math.Max(1, period100Ns * _inRate / 1e7);
            double margin = _inRate * latency.SourceMarginMs / 1000.0;
            // Level wanted right before a block is read: the block itself, plus whole capture packets (a packet may
            // be about to arrive but not there yet, and old drivers deliver them in bursts), plus the margin.
            _baseTarget = outPeriodFrames * _nominalStep + _packetFrames * latency.SourcePackets + margin;
            _extra = Math.Min(latency.ExtraSourceMs * _inRate / 1000.0, _packetFrames * 2);
            AverageBufferMs = ((latency.SourcePackets - 0.5) * _packetFrames + margin + _extra) * 1000.0 / _inRate;
            _ticksToMs = 1000.0 / Stopwatch.Frequency;
            _holdTicks = Stopwatch.Frequency * 2;
            _planar = new float[_inCh][];
            for (int c = 0; c < _inCh; c++) _planar[c] = new float[8192];
            LastSignal = Stopwatch.GetTimestamp();
        }

        public void Start() => _capture.Start();

        /// <summary>Fill level a sound starts from: the level wanted before a block is read, minus the packet that starts it.</summary>
        private double Park => _baseTarget + _extra - _packetFrames;

        /// <summary>
        /// Moves every captured packet into the resampler.
        /// Digital silence is the normal state of a device most of the time, and Windows delivers it irregularly
        /// (several packets at once every 30 ms or so, or none at all with classic cable drivers). So while there is
        /// nothing but silence it is not buffered at all: the buffer is "parked". The first packet that carries sound
        /// is put behind an exactly known reserve of silence, so every sound starts from the same, full buffer
        /// level, whatever happened before it.
        /// </summary>
        public void Pump(long now)
        {
            int packet = _captureClient.GetNextPacketSize();
            while (packet > 0)
            {
                var ptr = _captureClient.GetBuffer(out int frames, out var flags);
                if (frames > 0)
                {
                    bool zero = (flags & AudioClientBufferFlags.Silent) != 0;
                    if (!zero)
                    {
                        int n = frames * _inCh;
                        if (_interleaved.Length < n) _interleaved = new float[n * 2];
                        _codec.Read(ptr, _interleaved, n);
                        float peak = Dsp.Peak(_interleaved, n);
                        zero = peak == 0f;
                        if (peak >= SilenceThreshold) LastSignal = now;
                    }

                    if (zero)
                    {
                        // Parked: nothing to keep. Playing: a pause inside the sound, kept to preserve its timing.
                        if (!_silent)
                        {
                            _resampler.WriteSilence(frames);
                            _zeroRun += frames;
                        }
                    }
                    else
                    {
                        if (_silent)
                        {
                            _resampler.Reset();
                            _resampler.WriteSilence((int)Math.Round(Park));
                            _silent = false;
                            _fresh = true;
                            _holdUntil = now + _holdTicks;
                        }
                        if (_dryAt != 0)
                        {
                            // Sound kept coming right after the buffer ran out: the stream had not ended, so that
                            // was a real dropout on this source.
                            if ((now - _dryAt) * _ticksToMs < PacketMs * 4) _pendingDropout = true;
                            _dryAt = 0;
                        }
                        _resampler.Write(_interleaved, frames);
                        _zeroRun = 0;
                    }
                }
                _captureClient.ReleaseBuffer(frames);
                packet = _captureClient.GetNextPacketSize();
            }
            if (_dryAt != 0 && (now - _dryAt) * _ticksToMs > PacketMs * 6) _dryAt = 0; // nothing came: the sound simply ended
        }

        private void GoSilent()
        {
            _resampler.Reset();
            _zeroRun = 0;
            _silent = true;
        }

        /// <summary>While the output is paused: nothing is kept.</summary>
        public void DiscardSilence()
        {
            if (!_silent) GoSilent();
        }

        /// <summary>The output restarts: nothing to do, a sound always starts from a full reserve.</summary>
        public void Prime()
        {
        }

        /// <summary>
        /// Resamples <paramref name="frames"/> output frames, maps channels, applies the gain ramp and adds into
        /// <paramref name="mix"/>. Returns the peak of what was added.
        /// </summary>
        public float MixInto(float[] mix, int frames, float fromGain, float toGain, long now, double dt, ref long empty, ref long dropouts)
        {
            if (_pendingDropout)
            {
                _pendingDropout = false;
                Interlocked.Increment(ref empty);
                Interlocked.Increment(ref dropouts);
                if (_extra < _packetFrames * 2)
                {
                    // Keep a larger reserve from now on.
                    _extra = Math.Min(_extra + _packetFrames * 0.5, _packetFrames * 2);
                    _owner.LearnedSourceMs = Math.Max(_owner.LearnedSourceMs, _extra * 1000.0 / _inRate);
                    _owner._note = $"{_label} buffer reserve enlarged by {_extra * 1000.0 / _inRate:0} ms after a dropout";
                }
            }
            if (_silent) return 0;

            double pending = _resampler.Pending;
            double target = _baseTarget + _extra;
            if (_fresh)
            {
                _fresh = false;
                _avg = pending;
            }

            // Only digital silence left, for a while already: park (the next sound starts from a full reserve).
            if (_zeroRun >= _resampler.Buffered && _zeroRun > _inRate / 5)
            {
                GoSilent();
                return 0;
            }

            if (pending < frames * _step + 1)
            {
                // Not enough for a whole block: the sound ended (Windows stops delivering packets, or sends silence
                // late and in bursts) or, rarely, the buffer really ran dry. Play everything that is left, including
                // the tail still inside the resampling filter; Pump tells the two cases apart a moment later.
                float tail = 0;
                if (_zeroRun < _resampler.Buffered)
                {
                    _resampler.WriteSilence((int)(frames * _step) + SincResampler.Taps + 8);
                    EnsurePlanar(frames);
                    int n = _resampler.Read(_planar, frames, _step);
                    for (int c = 0; c < _inCh; c++) Array.Clear(_planar[c], n, frames - n);
                    tail = _map.MixInto(_planar, mix, frames, _outCh, fromGain, toGain);
                    _dryAt = now;
                }
                GoSilent();
                return tail;
            }

            if (pending > target + _inRate * 0.08)
            {
                // Large backlog (this thread was stalled): jump back to the target instead of lagging behind.
                _resampler.Skip((int)(pending - target));
                pending = _resampler.Pending;
                _avg = pending;
            }

            // Clock loop on the real, averaged fill level: natural frequency 0.014 rad/s, damping 0.7. The ratio
            // moves by a few ppm over minutes; for the first two seconds of a sound it only holds the drift
            // learned so far.
            _avg += (pending - _avg) * Math.Min(1.0, dt / 2.0);
            double err = (_avg - target) / _inRate; // seconds, positive = too much buffered
            double adj = _integral;
            if (now >= _holdUntil)
            {
                _integral = Math.Clamp(_integral + 0.0002 * err * dt, -0.001, 0.001);
                adj = Math.Clamp(0.02 * err + _integral, -0.002, 0.002);
            }
            _step = _nominalStep * (1 + adj);
            if (Trace != null && Trace.Count < 200000)
                Trace.Add(new[] { now * _ticksToMs / 1000.0, pending, 0, pending, _avg, err * 1e6, adj * 1e6, _integral * 1e6, frames, 0, 0 });

            // A short pause inside the sound: nothing to compute, just keep time.
            if (_zeroRun >= _resampler.Buffered)
            {
                if (_resampler.Advance(frames, _step) < frames) GoSilent();
                return 0;
            }

            EnsurePlanar(frames);
            int got = _resampler.Read(_planar, frames, _step);
            if (got < frames)
            {
                // Cannot happen after the check above; kept as a safety net.
                for (int c = 0; c < _inCh; c++) Array.Clear(_planar[c], got, frames - got);
                GoSilent();
            }
            return _map.MixInto(_planar, mix, frames, _outCh, fromGain, toGain);
        }

        private void EnsurePlanar(int frames)
        {
            for (int c = 0; c < _inCh; c++)
            {
                if (_planar[c].Length < frames) _planar[c] = new float[frames * 2];
            }
        }

        public void Dispose()
        {
            try { _capture.Stop(); } catch { /* ignore */ }
            try { _captureClient.Dispose(); } catch { /* ignore */ }
            try { _capture.Dispose(); } catch { /* ignore */ }
            try { _device.Dispose(); } catch { /* ignore */ }
            Event.Dispose();
        }
    }
}

internal static class Dsp
{
    /// <summary>Largest absolute sample value.</summary>
    public static float Peak(float[] data, int count)
    {
        int i = 0;
        float peak = 0;
        int w = Vector<float>.Count;
        if (count >= w)
        {
            var acc = Vector<float>.Zero;
            var span = MemoryMarshal.Cast<float, Vector<float>>(data.AsSpan(0, count - count % w));
            for (int k = 0; k < span.Length; k++) acc = Vector.Max(acc, Vector.Abs(span[k]));
            for (int k = 0; k < w; k++) peak = Math.Max(peak, acc[k]);
            i = span.Length * w;
        }
        for (; i < count; i++) peak = Math.Max(peak, Math.Abs(data[i]));
        return peak;
    }
}

/// <summary>Speaker-position based channel mapping (source → output), no normalisation of matched channels.</summary>
public sealed class ChannelMap
{
    private const uint FL = 0x1, FR = 0x2, FC = 0x4, LFE = 0x8, BL = 0x10, BR = 0x20, FLC = 0x40, FRC = 0x80,
                       BC = 0x100, SL = 0x200, SR = 0x400;

    private readonly (int outCh, int inCh, float w)[] _routes;
    public bool IsIdentity { get; }

    private ChannelMap((int, int, float)[] routes, bool identity)
    {
        _routes = routes;
        IsIdentity = identity;
    }

    public static ChannelMap Create(AudioFormat input, AudioFormat output)
    {
        var inPos = Positions(input);
        var outPos = Positions(output);
        var routes = new System.Collections.Generic.List<(int, int, float)>();

        int OutIndex(uint bit) => Array.IndexOf(outPos, bit);
        const float m3 = 0.70710678f;

        if (input.Channels == 1)
        {
            // Mono: centre if available, otherwise both front speakers at full level (like Windows).
            int c = OutIndex(FC);
            if (c >= 0 && output.Channels > 2) routes.Add((c, 0, 1f));
            else
            {
                int l = OutIndex(FL), r = OutIndex(FR);
                if (l >= 0) routes.Add((l, 0, 1f));
                if (r >= 0) routes.Add((r, 0, 1f));
                if (l < 0 && r < 0) routes.Add((0, 0, 1f));
            }
            return Build(routes, input, output);
        }

        for (int i = 0; i < inPos.Length; i++)
        {
            uint bit = inPos[i];
            if (bit == 0) continue;
            int o = OutIndex(bit);
            if (o >= 0)
            {
                routes.Add((o, i, 1f));
                continue;
            }
            // Fold-down rules (ITU-R BS.775 style) when the output lacks the speaker.
            void To(uint target, float w)
            {
                int t = OutIndex(target);
                if (t >= 0) routes.Add((t, i, w));
            }
            switch (bit)
            {
                case FC: To(FL, m3); To(FR, m3); break;
                case LFE: break; // dropped (as Windows does on outputs without a subwoofer)
                case BL: if (OutIndex(SL) >= 0) To(SL, 1f); else To(FL, m3); break;
                case BR: if (OutIndex(SR) >= 0) To(SR, 1f); else To(FR, m3); break;
                case SL: if (OutIndex(BL) >= 0) To(BL, 1f); else To(FL, m3); break;
                case SR: if (OutIndex(BR) >= 0) To(BR, 1f); else To(FR, m3); break;
                case FLC: To(FL, 1f); break;
                case FRC: To(FR, 1f); break;
                case BC:
                    if (OutIndex(BL) >= 0 && OutIndex(BR) >= 0) { To(BL, m3); To(BR, m3); }
                    else if (OutIndex(SL) >= 0 && OutIndex(SR) >= 0) { To(SL, m3); To(SR, m3); }
                    else { To(FL, 0.5f); To(FR, 0.5f); }
                    break;
                case FL: To(FC, 1f); break; // mono output
                case FR: To(FC, 1f); break;
                default: To(FL, 0.5f); To(FR, 0.5f); break; // top / unknown speakers
            }
        }
        // Positionless outputs (mask 0 / unknown): straight 1:1 copy.
        if (routes.Count == 0)
        {
            for (int i = 0; i < Math.Min(input.Channels, output.Channels); i++) routes.Add((i, i, 1f));
        }
        return Build(routes, input, output);
    }

    private static ChannelMap Build(System.Collections.Generic.List<(int, int, float)> routes, AudioFormat input, AudioFormat output)
    {
        bool identity = input.Channels == output.Channels && routes.Count == input.Channels;
        if (identity)
        {
            foreach (var (o, i, w) in routes)
            {
                if (o != i || w != 1f) { identity = false; break; }
            }
        }
        return new ChannelMap(routes.ToArray(), identity);
    }

    private static uint[] Positions(AudioFormat f)
    {
        // No mask (e.g. a 16-channel "direct out" device): the first channels follow the standard order
        // FL FR FC LFE BL BR SL SR, which is where Windows puts stereo/surround streams; the rest carry nothing.
        var mask = f.ChannelMask == 0 ? AudioFormat.DefaultMask(Math.Min(f.Channels, 8)) : f.ChannelMask;
        var list = new uint[f.Channels];
        int n = 0;
        for (int b = 0; b < 32 && n < f.Channels; b++)
        {
            uint bit = 1u << b;
            if ((mask & bit) != 0) list[n++] = bit;
        }
        return list; // unnamed channels stay 0 and are not routed
    }

    /// <summary>Adds the planar source (with a linear gain ramp) into the interleaved mix. Returns the post-gain peak.</summary>
    public float MixInto(float[][] planar, float[] mix, int frames, int outCh, float fromGain, float toGain)
    {
        float peak = 0;
        bool ramp = fromGain != toGain;
        if (!ramp && toGain == 0f) return 0;
        float step = ramp && frames > 0 ? (toGain - fromGain) / frames : 0;
        foreach (var (o, i, w) in _routes)
        {
            var src = planar[i];
            if (!ramp && toGain == 1f && w == 1f)
            {
                // Unity: exact copy-add, no arithmetic on the samples.
                for (int f = 0, idx = o; f < frames; f++, idx += outCh)
                {
                    float s = src[f];
                    mix[idx] += s;
                    float a = Math.Abs(s);
                    if (a > peak) peak = a;
                }
            }
            else
            {
                float g = fromGain;
                for (int f = 0, idx = o; f < frames; f++, idx += outCh)
                {
                    float s = src[f] * (ramp ? g : toGain) * w;
                    mix[idx] += s;
                    float a = Math.Abs(s);
                    if (a > peak) peak = a;
                    g += step;
                }
            }
        }
        return peak;
    }
}

/// <summary>Converts between a device sample format and float.</summary>
internal sealed class SampleCodec
{
    private readonly int _bits;
    private readonly int _validBits;
    private readonly bool _float;
    private uint _rng = 0x9E3779B9u;

    private SampleCodec(int bits, int validBits, bool isFloat)
    {
        _bits = bits;
        _validBits = validBits;
        _float = isFloat;
    }

    public static SampleCodec From(AudioFormat f)
    {
        if (f.IsFloat && f.Bits != 32) throw new NotSupportedException($"Formato float a {f.Bits} bit non supportato");
        if (!f.IsFloat && f.Bits is not (16 or 24 or 32)) throw new NotSupportedException($"Formato PCM a {f.Bits} bit non supportato");
        return new SampleCodec(f.Bits, f.ValidBits, f.IsFloat);
    }

    public override string ToString() => _float ? "float32" : $"pcm{_validBits}/{_bits}";

    public unsafe void Read(IntPtr src, float[] dst, int samples)
    {
        if (_float)
        {
            Marshal.Copy(src, dst, 0, samples);
            return;
        }
        byte* p = (byte*)src;
        switch (_bits)
        {
            case 16:
                for (int i = 0; i < samples; i++) dst[i] = ((short*)p)[i] / 32768f;
                break;
            case 24:
                for (int i = 0; i < samples; i++, p += 3)
                    dst[i] = ((p[0] << 8) | (p[1] << 16) | (p[2] << 24)) / 2147483648f;
                break;
            default:
                for (int i = 0; i < samples; i++) dst[i] = ((int*)p)[i] / 2147483648f;
                break;
        }
    }

    /// <summary>Float to device format. Float outputs are copied untouched; integer outputs get TPDF dither at their LSB.</summary>
    public unsafe void Write(float[] src, int samples, IntPtr dst)
    {
        if (_float)
        {
            Marshal.Copy(src, 0, dst, samples);
            return;
        }
        int bits = Math.Clamp(_validBits, 16, 32);
        double full = Math.Pow(2, bits - 1);
        double lsb = 1.0 / full;
        byte* p = (byte*)dst;
        for (int i = 0; i < samples; i++)
        {
            double x = src[i];
            if (bits < 32) x += (Rand() - Rand()) * lsb; // TPDF ±1 LSB
            double v = Math.Clamp(x * full, -full, full - 1);
            long q = (long)Math.Round(v);
            switch (_bits)
            {
                case 16:
                    ((short*)p)[i] = (short)q;
                    break;
                case 24:
                    {
                        int iv = (int)q;
                        byte* b = p + i * 3;
                        b[0] = (byte)iv; b[1] = (byte)(iv >> 8); b[2] = (byte)(iv >> 16);
                        break;
                    }
                default:
                    ((int*)p)[i] = (int)(q << (32 - bits));
                    break;
            }
        }
    }

    private double Rand()
    {
        _rng ^= _rng << 13;
        _rng ^= _rng >> 17;
        _rng ^= _rng << 5;
        return _rng / 4294967296.0;
    }
}
