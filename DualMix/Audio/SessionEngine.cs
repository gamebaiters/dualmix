using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using DualMix.Audio.Interop;
using DualMix.Core;

namespace DualMix.Audio;

/// <summary>Parameters the session engine works with (immutable, swapped atomically).</summary>
/// <param name="Gains">Linear gain of each channel.</param>
/// <param name="RouteDevices">Send each app to the device of its channel.</param>
/// <param name="GameRouteId">Device for Game apps; null = leave them on the Windows default.</param>
/// <param name="ChatRouteId">Device for Chat apps; null = leave them on the Windows default.</param>
/// <param name="DirectRouteId">Device for "Diretto" apps; null = leave them on the Windows default.</param>
/// <param name="GameBusId">Virtual Game device mixed by the router (no session gain is applied there).</param>
/// <param name="ChatBusId">Virtual Chat device mixed by the router (no session gain is applied there).</param>
public sealed record SessionParams(
    ChannelGains Gains,
    RuleSet Rules,
    bool RouteDevices,
    string? GameRouteId,
    string? ChatRouteId,
    string? DirectRouteId,
    string? GameBusId,
    string? ChatBusId,
    bool MetersEnabled);

/// <summary>One application with audio, aggregated over all its sessions.</summary>
public sealed class AudioAppInfo
{
    public required string Key { get; init; }
    public required string DisplayName { get; init; }
    public string? ExePath { get; init; }
    public bool IsSystemSounds { get; init; }
    public Channel Channel { get; init; }
    public bool IsExplicit { get; init; }
    public Channel AutoChannel { get; init; }
    public double RelativeVolume { get; init; }
    public float Peak { get; init; }
    public bool Active { get; init; }
    public string Devices { get; init; } = "";

    /// <summary>
    /// The app is playing on a real device although its channel has a virtual one: its output device is fixed in its
    /// own settings. It is still balanced, through its Windows volume.
    /// </summary>
    public bool FixedOutput { get; init; }
}

public abstract record EngineEvent;

/// <summary>A process was seen for the first time; <see cref="Volume"/> is its current Windows volume.</summary>
public sealed record AppDiscovered(string Key, string DisplayName, string? ExePath, double Volume) : EngineEvent;

/// <summary>The user changed an app volume from the Windows volume mixer; adopt it as relative volume.</summary>
public sealed record AppVolumeAdopted(string Key, double Volume) : EngineEvent;

/// <summary>A process was routed to a virtual device.</summary>
public sealed record AppRouted(string Key) : EngineEvent;

/// <summary>
/// Watches every audio session on every active playback device and classifies it as Game/Chat.
/// Sessions playing on a real device get their channel gain through the Windows session volume (this is how Game is
/// balanced when games play straight on the sound card, and the fallback for everything when no virtual device
/// exists). Sessions playing on a DualMix virtual device only get their own per-app volume: the router applies the
/// channel gain. It also sends each app to the device of its channel.
/// All COM work happens on one dedicated MTA thread. Session notifications are registered once per device;
/// a slow periodic rescan is only a safety net.
/// </summary>
public sealed class SessionEngine : IDisposable
{
    private const int RescanIntervalMs = 3000;
    private const int VerifyIntervalMs = 1000;

    private sealed class Tracked
    {
        public required string InstanceId;
        public required IAudioSessionControl2 Control;
        public required ISimpleAudioVolume Volume;
        public required uint Pid;
        public required string Key;
        public required string DisplayName;
        public string? ExePath;
        public bool IsSystem;
        public required string DeviceId;
        public int State;
        public float LastSet = -1;
        public bool Touched;
        public float Peak;
        public bool Seen;
    }

    private sealed class DeviceHolder
    {
        public required string Id;
        public required string Name;
        public required IMMDevice Device;
        public required IAudioSessionManager2 Manager;
        public bool Registered;
    }

    private readonly Thread _thread;
    private readonly AutoResetEvent _wake = new(false);
    private readonly ConcurrentQueue<(Action action, TaskCompletionSource tcs)> _commands = new();
    private readonly ConcurrentQueue<EngineEvent> _events = new();
    private readonly Dictionary<string, Tracked> _tracked = new();
    private readonly Dictionary<string, DeviceHolder> _devices = new();
    private readonly Dictionary<string, string> _deviceNames = new();
    private readonly Dictionary<string, double> _learned = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<uint, string> _routedPids = new();
    private readonly HashSet<string> _routedKeys = new(StringComparer.OrdinalIgnoreCase);
    private readonly uint _ownPid = (uint)Environment.ProcessId;
    private readonly SessionNotificationSink _sink;

    private volatile SessionParams _params;
    private SessionParams? _appliedParams;
    private volatile bool _stop;
    private volatile bool _rescan = true;
    private volatile bool _restoreOnStop = true;
    private volatile bool _routeReset;
    private SessionParams? _routeBlockedFor;
    private IMMDeviceEnumerator? _enumerator;

    private volatile IReadOnlyList<AudioAppInfo> _snapshot = Array.Empty<AudioAppInfo>();
    private long _lastPublish;
    private bool _dirty = true;

    public IReadOnlyList<AudioAppInfo> Snapshot => _snapshot;
    public float GamePeak { get; private set; }
    public float ChatPeak { get; private set; }

    public SessionEngine(SessionParams initial)
    {
        _params = initial;
        _sink = new SessionNotificationSink(OnSessionCreated);
        _thread = new Thread(Run) { IsBackground = true, Name = "DualMix.Sessions" };
        _thread.SetApartmentState(ApartmentState.MTA);
    }

    public void Start() => _thread.Start();

    public void Update(SessionParams p)
    {
        var old = _params;
        _params = p;
        if (!ReferenceEquals(old.Rules, p.Rules) || old.RouteDevices != p.RouteDevices
            || old.GameBusId != p.GameBusId || old.ChatBusId != p.ChatBusId) _dirty = true;
        if (old.GameRouteId != p.GameRouteId || old.ChatRouteId != p.ChatRouteId || old.DirectRouteId != p.DirectRouteId
            || old.RouteDevices != p.RouteDevices || !ReferenceEquals(old.Rules, p.Rules))
            _routeReset = true;
        _wake.Set();
    }

    public void RequestRescan()
    {
        _rescan = true;
        _wake.Set();
    }

    public bool TryDequeueEvent(out EngineEvent? ev)
    {
        var ok = _events.TryDequeue(out var e);
        ev = e;
        return ok;
    }

    /// <summary>Runs an action on the engine thread.</summary>
    public Task Invoke(Action action)
    {
        var tcs = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        if (_stop || !_thread.IsAlive)
        {
            tcs.SetException(new InvalidOperationException("Session engine not running"));
            return tcs.Task;
        }
        _commands.Enqueue((action, tcs));
        _wake.Set();
        return tcs.Task;
    }

    /// <summary>Resets the per-app output device of every app DualMix routed (plus the given process names).</summary>
    public Task UnrouteAll(IEnumerable<string> extraKeys)
    {
        var extra = extraKeys.ToList();
        return Invoke(() =>
        {
            var keys = new HashSet<string>(_routedKeys, StringComparer.OrdinalIgnoreCase);
            foreach (var k in extra) keys.Add(k);
            var done = new HashSet<uint>();
            foreach (var (pid, dev) in _routedPids)
            {
                if (dev.Length > 0 && done.Add(pid)) AppDeviceRouting.SetAppDevice(pid, null);
            }
            foreach (var key in keys)
            {
                foreach (var pid in ProcessInfo.PidsByName(key))
                {
                    if (done.Add(pid)) AppDeviceRouting.SetAppDevice(pid, null);
                }
            }
            _routedPids.Clear();
            _routedKeys.Clear();
            _routeBlockedFor = _params;
            Log.Info($"Unrouted {done.Count} processes ({string.Join(", ", keys)})");
        });
    }

    public void Stop(bool restoreVolumes)
    {
        _restoreOnStop = restoreVolumes;
        _stop = true;
        try { _wake.Set(); } catch { /* disposed */ }
        if (_thread.IsAlive && !_thread.Join(3000)) Log.Warn("Session engine thread did not stop in time");
    }

    public void Dispose()
    {
        if (!_stop) Stop(true);
    }

    // ------------------------------------------------------------------------------------------

    private void Run()
    {
        Log.Info("Session engine started");
        long nextScan = 0, nextVerify = 0;
        int failures = 0;
        while (!_stop)
        {
            var p = _params;
            try
            {
                _enumerator ??= AudioCom.CreateDeviceEnumerator();
                DrainCommands();
                p = _params; // a command (e.g. "unroute everything") refers to the newest parameters

                var now = Environment.TickCount64;
                bool scanned = false;
                if (_rescan || now >= nextScan)
                {
                    _rescan = false;
                    Scan(p);
                    nextScan = now + RescanIntervalMs;
                    scanned = true;
                }

                // Apply immediately when something changed, otherwise verify once per second
                // (catches volume changes made in the Windows mixer).
                if (scanned || !ReferenceEquals(p, _appliedParams) || now >= nextVerify)
                {
                    Apply(p);
                    _appliedParams = p;
                    nextVerify = now + VerifyIntervalMs;
                    if (_routeReset)
                    {
                        _routeReset = false;
                        // Re-check every app against the new targets (keeps the "routed" bookkeeping).
                        foreach (var k in _routedPids.Keys.ToList()) _routedPids[k] = _routedPids[k].Length > 0 ? "?" : "";
                    }
                    // After "unroute everything" nothing is routed again until the controller sends new parameters.
                    if (p.RouteDevices && !ReferenceEquals(p, _routeBlockedFor)) Route(p);
                }

                if (p.MetersEnabled) SampleMeters(p);
                else if (GamePeak != 0 || ChatPeak != 0) GamePeak = ChatPeak = 0;

                Publish(p, now);
                failures = 0;
            }
            catch (Exception ex)
            {
                failures++;
                Log.Error($"Session engine loop error (#{failures})", ex);
                ResetAll();
                _rescan = true;
                Thread.Sleep(Math.Min(10000, 500 * failures));
            }

            _wake.WaitOne(p.MetersEnabled ? 50 : 250);
        }

        try
        {
            DrainCommands();
            if (_restoreOnStop) RestoreAll();
        }
        catch (Exception ex)
        {
            Log.Error("Restore on stop failed", ex);
        }
        ResetAll();
        AudioCom.FinalRelease(_enumerator);
        _enumerator = null;
        Log.Info("Session engine stopped");
    }

    private void DrainCommands()
    {
        while (_commands.TryDequeue(out var c))
        {
            try
            {
                c.action();
                c.tcs.TrySetResult();
            }
            catch (Exception ex)
            {
                c.tcs.TrySetException(ex);
            }
        }
    }

    private void OnSessionCreated()
    {
        _rescan = true;
        try { _wake.Set(); } catch { /* disposed */ }
    }

    // ------------------------------------------------------------------------------------------ scan

    private void Scan(SessionParams p)
    {
        var en = _enumerator!;
        var seenDevices = new HashSet<string>();
        foreach (var t in _tracked.Values) t.Seen = false;

        if (en.EnumAudioEndpoints(EDataFlow.Render, DeviceStateMask.Active, out var collection) != 0 || collection == null)
            throw new InvalidOperationException("EnumAudioEndpoints failed");
        try
        {
            collection.GetCount(out int count);
            for (int d = 0; d < count; d++)
            {
                if (collection.Item(d, out var dev) != 0 || dev == null) continue;
                string? id = null;
                bool keep = false;
                try
                {
                    if (dev.GetId(out id) != 0 || id == null) continue;
                    seenDevices.Add(id);
                    if (!_devices.ContainsKey(id))
                    {
                        var holder = OpenDevice(id, dev);
                        if (holder != null)
                        {
                            _devices[id] = holder;
                            keep = true;
                        }
                    }
                }
                finally
                {
                    if (!keep) AudioCom.Release(dev);
                }
            }
        }
        finally
        {
            AudioCom.Release(collection);
        }

        // Devices that disappeared
        foreach (var id in _devices.Keys.Where(k => !seenDevices.Contains(k)).ToList())
        {
            CloseDevice(_devices[id]);
            _devices.Remove(id);
            _dirty = true;
        }

        foreach (var holder in _devices.Values.ToList())
        {
            try
            {
                EnumerateSessions(p, holder);
            }
            catch (Exception ex)
            {
                Log.Warn($"Session enumeration failed on {holder.Name}", ex);
                CloseDevice(holder);
                _devices.Remove(holder.Id);
            }
        }

        // Sessions that disappeared / expired
        foreach (var key in _tracked.Where(kv => !kv.Value.Seen || kv.Value.State == AudioSessionStates.Expired)
                                    .Select(kv => kv.Key).ToList())
        {
            ReleaseTracked(_tracked[key]);
            _tracked.Remove(key);
            _dirty = true;
        }

        var alivePids = new HashSet<uint>(_tracked.Values.Select(t => t.Pid));
        ProcessInfo.Prune(alivePids);
        foreach (var pid in _routedPids.Keys.Where(pid => !alivePids.Contains(pid)).ToList())
        {
            _routedPids.Remove(pid);
        }
    }

    private DeviceHolder? OpenDevice(string id, IMMDevice dev)
    {
        var iid = AudioCom.IID_IAudioSessionManager2;
        if (dev.Activate(ref iid, AudioCom.CLSCTX_ALL, IntPtr.Zero, out var obj) != 0 || obj is not IAudioSessionManager2 mgr)
        {
            Log.Warn($"Cannot activate session manager on {id}");
            return null;
        }

        if (!_deviceNames.TryGetValue(id, out var name))
        {
            name = Devices.NameOf(id);
            _deviceNames[id] = name;
        }

        var holder = new DeviceHolder { Id = id, Name = name, Device = dev, Manager = mgr };

        // The session manager only starts sending notifications after the first GetSessionEnumerator call.
        if (mgr.GetSessionEnumerator(out var first) == 0) AudioCom.Release(first);
        holder.Registered = mgr.RegisterSessionNotification(_sink) == 0;
        if (!holder.Registered) Log.Warn($"RegisterSessionNotification failed on {name}");
        Log.Info($"Watching sessions on '{name}'");
        return holder;
    }

    private void CloseDevice(DeviceHolder holder)
    {
        try
        {
            if (holder.Registered) holder.Manager.UnregisterSessionNotification(_sink);
        }
        catch
        {
            // device already gone
        }
        AudioCom.Release(holder.Manager);
        AudioCom.Release(holder.Device);
        Log.Info($"Stopped watching '{holder.Name}'");
    }

    private void EnumerateSessions(SessionParams p, DeviceHolder holder)
    {
        if (holder.Manager.GetSessionEnumerator(out var sessions) != 0 || sessions == null) return;
        try
        {
            sessions.GetCount(out int count);
            for (int i = 0; i < count; i++)
            {
                if (sessions.GetSession(i, out var s) != 0 || s == null) continue;
                bool keep = false;
                try
                {
                    keep = TrackSession(p, s, holder);
                }
                catch
                {
                    keep = false;
                }
                finally
                {
                    // Every marshaled reference is released once; the tracked one keeps its own reference.
                    if (!keep) AudioCom.Release(s);
                }
            }
        }
        finally
        {
            AudioCom.Release(sessions);
        }
    }

    /// <returns>true when the session object is now owned by <see cref="_tracked"/>.</returns>
    private bool TrackSession(SessionParams p, IAudioSessionControl2 s, DeviceHolder holder)
    {
        if (s.GetSessionInstanceIdentifier(out var instanceId) != 0 || string.IsNullOrEmpty(instanceId)) return false;
        s.GetState(out int state);
        if (state == AudioSessionStates.Expired) return false;

        if (_tracked.TryGetValue(instanceId, out var existing))
        {
            existing.Seen = true;
            if (existing.State != state) _dirty = true;
            existing.State = state;
            return false; // duplicate reference: released by the caller
        }

        s.GetProcessId(out uint pid);
        bool isSystem = s.IsSystemSoundsSession() == 0;
        if (pid == _ownPid && !isSystem) return false;
        if (!isSystem && IsAuxiliaryProcess(pid)) return false;

        string key, display;
        string? path = null;
        if (isSystem)
        {
            key = RuleSet.SystemSoundsKey;
            display = Loc.Get("Apps.SystemSounds");
        }
        else
        {
            var info = ProcessInfo.Get(pid);
            key = info.Name;
            path = info.Path;
            display = ProcessInfo.FriendlyName(path, info.Name);
            if (path == null && s.GetDisplayName(out var dn) == 0 && !string.IsNullOrWhiteSpace(dn) && !dn.StartsWith('@'))
                display = dn;
        }

        var volume = (ISimpleAudioVolume)s;
        var t = new Tracked
        {
            InstanceId = instanceId,
            Control = s,
            Volume = volume,
            Pid = pid,
            Key = key,
            DisplayName = display,
            ExePath = path,
            IsSystem = isSystem,
            DeviceId = holder.Id,
            State = state,
            Seen = true,
        };
        _tracked[instanceId] = t;
        _dirty = true;

        if (!p.Rules.HasRule(key) && !_learned.ContainsKey(key))
        {
            double current = 1.0;
            if (volume.GetMasterVolume(out var v) == 0) current = v;
            // Inherit whatever level the user had set in the Windows mixer, even if very low:
            // starting DualMix must never make anything suddenly louder.
            current = Math.Round(Math.Clamp(current, 0, 1), 3);
            _learned[key] = current;
            _events.Enqueue(new AppDiscovered(key, display, path, current));
            Log.Info($"Discovered app '{key}' ({display}) on {holder.Name}, volume {current:P0}");
        }
        return true;
    }

    /// <summary>Name of the mutex a DualMix helper process (diagnostics) holds so that its test tones are left alone.</summary>
    public static string AuxiliaryMutexName(uint pid) => $@"Local\DualMix.Aux.{pid}";

    private static bool IsAuxiliaryProcess(uint pid)
    {
        if (!Mutex.TryOpenExisting(AuxiliaryMutexName(pid), out var m)) return false;
        m.Dispose();
        return true;
    }

    private void ReleaseTracked(Tracked t)
    {
        // Volume is the same RCW as Control (QueryInterface on one object): release once.
        AudioCom.Release(t.Control);
    }

    // ------------------------------------------------------------------------------------------ apply

    private double RelativeVolume(SessionParams p, string key)
    {
        if (p.Rules.HasRule(key)) return p.Rules.VolumeOf(key);
        return _learned.TryGetValue(key, out var v) ? v : 1.0;
    }

    private void Apply(SessionParams p)
    {
        foreach (var t in _tracked.Values)
        {
            var channel = p.Rules.Resolve(t.Key, t.IsSystem);
            var relative = RelativeVolume(p, t.Key);

            if (channel == Channel.Ignore)
            {
                if (t.Touched)
                {
                    TrySet(t, (float)relative);
                    t.Touched = false;
                    t.LastSet = -1;
                }
                continue;
            }

            if (t.Volume.GetMasterVolume(out float current) != 0) continue;

            // On a real device: app volume = channel gain x relative volume.
            // On a DualMix virtual device the router applies the channel gain: here only the relative volume.
            bool onBus = t.DeviceId == p.ChatBusId || t.DeviceId == p.GameBusId;
            var gain = onBus ? 1f : p.Gains.For(channel);

            // Someone (the user, through the Windows volume mixer) changed the app volume behind our back:
            // adopt it as the new relative volume instead of fighting it.
            if (t.Touched && t.LastSet >= 0 && Math.Abs(current - t.LastSet) > 0.015f && gain > 0.05f)
            {
                var adopted = Math.Round(Math.Clamp(current / gain, 0, 1), 2);
                if (Math.Abs(adopted - relative) > 0.01)
                {
                    relative = adopted;
                    _learned[t.Key] = adopted;
                    _events.Enqueue(new AppVolumeAdopted(t.Key, adopted));
                    Log.Info($"Adopted Windows mixer volume for '{t.Key}': {adopted:P0}");
                }
            }

            var target = (float)Math.Clamp(gain * relative, 0, 1);
            if (!t.Touched || Math.Abs(current - target) > 0.002f)
            {
                if (TrySet(t, target))
                {
                    t.LastSet = target;
                    t.Touched = true;
                }
            }
            else
            {
                t.LastSet = target;
            }
        }
    }

    private static bool TrySet(Tracked t, float value)
    {
        try
        {
            var ctx = Guid.Empty;
            return t.Volume.SetMasterVolume(Math.Clamp(value, 0f, 1f), ref ctx) == 0;
        }
        catch
        {
            return false;
        }
    }

    private void Route(SessionParams p)
    {
        var handled = new HashSet<uint>();
        foreach (var t in _tracked.Values)
        {
            if (t.IsSystem || t.Pid == 0 || !handled.Add(t.Pid)) continue;
            var channel = p.Rules.Resolve(t.Key, false);
            // null = the app follows the Windows default device (Windows keeps as few per-app overrides as possible).
            string? target = channel switch
            {
                Channel.Game => p.GameRouteId,
                Channel.Chat => p.ChatRouteId,
                _ => p.DirectRouteId,
            };

            _routedPids.TryGetValue(t.Pid, out var routedTo);
            if (target == null)
            {
                if (routedTo != null && routedTo.Length > 0)
                {
                    AppDeviceRouting.SetAppDevice(t.Pid, null);
                    Log.Info($"Unrouted '{t.Key}' (pid {t.Pid}): follows the Windows default device");
                    _rescan = true;
                }
                _routedPids[t.Pid] = "";
                continue;
            }

            if (routedTo == target) continue;
            var current = AppDeviceRouting.GetAppDevice(t.Pid);
            if (current == null) continue; // API unavailable
            if (!string.Equals(current, target, StringComparison.OrdinalIgnoreCase))
            {
                if (AppDeviceRouting.SetAppDevice(t.Pid, target))
                {
                    var label = channel switch { Channel.Game => "Game", Channel.Chat => "Chat", _ => "uscita diretta" };
                    Log.Info($"Routed '{t.Key}' (pid {t.Pid}) to {label}");
                    if (_routedKeys.Add(t.Key)) _events.Enqueue(new AppRouted(t.Key));
                    _rescan = true;
                }
            }
            else if (_routedKeys.Add(t.Key))
            {
                _events.Enqueue(new AppRouted(t.Key));
            }
            _routedPids[t.Pid] = target;
        }
    }

    private void SampleMeters(SessionParams p)
    {
        float game = 0, chat = 0;
        foreach (var t in _tracked.Values)
        {
            float peak = 0;
            if (t.State == AudioSessionStates.Active)
            {
                try
                {
                    if (((IAudioMeterInformation)t.Control).GetPeakValue(out var v) == 0) peak = v;
                }
                catch
                {
                    peak = 0;
                }
            }
            var ch = p.Rules.Resolve(t.Key, t.IsSystem);
            // An app "playing" on the virtual device of the other channel is really capturing it (echo cancellation,
            // stream audio): that is not its sound.
            if ((ch == Channel.Chat && t.DeviceId == p.GameBusId) || (ch == Channel.Game && t.DeviceId == p.ChatBusId)) peak = 0;
            t.Peak = peak;
            if (ch == Channel.Game) game = Math.Max(game, peak);
            else if (ch == Channel.Chat) chat = Math.Max(chat, peak);
        }
        GamePeak = game;
        ChatPeak = chat;
    }

    // ------------------------------------------------------------------------------------------ snapshot

    private void Publish(SessionParams p, long now)
    {
        if (!_dirty && (!p.MetersEnabled || now - _lastPublish < 50)) return;
        _dirty = false;
        _lastPublish = now;

        var list = new List<AudioAppInfo>();
        foreach (var g in _tracked.Values.GroupBy(t => t.Key, StringComparer.OrdinalIgnoreCase))
        {
            var first = g.First();
            var channel = p.Rules.Resolve(first.Key, first.IsSystem);
            var expected = channel switch { Channel.Game => p.GameBusId, Channel.Chat => p.ChatBusId, _ => null };
            bool fixedOutput = p.RouteDevices && expected != null && !first.IsSystem
                               && !g.Any(x => x.State == AudioSessionStates.Active && x.DeviceId == expected)
                               && g.Any(x => x.State == AudioSessionStates.Active && x.DeviceId != p.GameBusId && x.DeviceId != p.ChatBusId);
            list.Add(new AudioAppInfo
            {
                FixedOutput = fixedOutput,
                Key = first.Key,
                DisplayName = first.DisplayName,
                ExePath = g.Select(x => x.ExePath).FirstOrDefault(x => x != null),
                IsSystemSounds = first.IsSystem,
                Channel = p.Rules.Resolve(first.Key, first.IsSystem),
                AutoChannel = p.Rules.AutoChannel(first.Key, first.IsSystem),
                IsExplicit = p.Rules.ExplicitChannel(first.Key).HasValue,
                RelativeVolume = RelativeVolume(p, first.Key),
                Peak = g.Max(x => x.Peak),
                Active = g.Any(x => x.State == AudioSessionStates.Active),
                Devices = string.Join(", ", g.Select(x => _deviceNames.TryGetValue(x.DeviceId, out var n) ? n : "?").Distinct()),
            });
        }

        list.Sort((a, b) =>
        {
            if (a.IsSystemSounds != b.IsSystemSounds) return a.IsSystemSounds ? 1 : -1;
            return string.Compare(a.DisplayName, b.DisplayName, StringComparison.CurrentCultureIgnoreCase);
        });
        _snapshot = list;
    }

    // ------------------------------------------------------------------------------------------ teardown

    private void RestoreAll()
    {
        var p = _params;
        int n = 0;
        foreach (var t in _tracked.Values)
        {
            if (!t.Touched) continue;
            if (TrySet(t, (float)RelativeVolume(p, t.Key))) n++;
            t.Touched = false;
        }
        if (n > 0) Log.Info($"Restored {n} session volumes");
    }

    private void ResetAll()
    {
        foreach (var t in _tracked.Values) ReleaseTracked(t);
        _tracked.Clear();
        foreach (var d in _devices.Values) CloseDevice(d);
        _devices.Clear();
        _dirty = true;
    }
}
