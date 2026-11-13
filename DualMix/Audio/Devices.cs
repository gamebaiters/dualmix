using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using DualMix.Core;
using NAudio.CoreAudioApi;

namespace DualMix.Audio;

public sealed record DeviceInfo(string Id, string Name, string Description, string InterfaceName, DataFlow Flow, DeviceState State, bool IsDefault,
                                string Adapter = "", string JackSubType = "")
{
    private const string SpeakerJack = "{DFF21CE1-F70F-11D0-B917-00A0C9223196}";

    /// <summary>A VB-Audio virtual cable (VB-CABLE, VB-CABLE A/B/C/D, Hi-Fi Cable). Voicemeeter devices need its app and are not used.</summary>
    public bool IsVbCable => InterfaceName.StartsWith("VB-Audio", StringComparison.OrdinalIgnoreCase)
                             && InterfaceName.Contains("Cable", StringComparison.OrdinalIgnoreCase)
                             && !InterfaceName.Contains("Voicemeeter", StringComparison.OrdinalIgnoreCase);

    /// <summary>The main playback jack of its device (VB-CABLE also exposes a secondary "16 ch" input that shares the same stream).</summary>
    public bool IsMainJack => JackSubType.Equals(SpeakerJack, StringComparison.OrdinalIgnoreCase)
                              || (JackSubType.Length == 0 && !Description.Contains("16", StringComparison.Ordinal));

    /// <summary>
    /// The current free VB-CABLE driver (3.x). Measured: what Windows captures from it is requantised (19 bit in the
    /// 24-bit formats), while the classic VB-CABLE A/B/C/D and Hi-Fi drivers return the samples bit for bit.
    /// It is therefore used only when nothing better is installed, and never for Game if a transparent cable exists.
    /// </summary>
    public bool IsLossyCable => IsVbCable && InterfaceName.Contains("Virtual Cable", StringComparison.OrdinalIgnoreCase);

    /// <summary>Factory name of the main playback endpoint of this cable.</summary>
    public string FactoryName
    {
        get
        {
            foreach (var letter in new[] { "A", "B", "C", "D" })
            {
                if (InterfaceName.EndsWith("Cable " + letter, StringComparison.OrdinalIgnoreCase) ||
                    InterfaceName.EndsWith("CABLE-" + letter, StringComparison.OrdinalIgnoreCase))
                    return $"CABLE-{letter} Input";
            }
            return InterfaceName.Contains("Hi-Fi", StringComparison.OrdinalIgnoreCase) ? "Hi-Fi Cable Input" : "CABLE Input";
        }
    }

    /// <summary>Key identifying the physical/virtual device an endpoint belongs to.</summary>
    public string DeviceKey => Adapter.Length > 0 ? Adapter : InterfaceName;
    public bool IsAe5 => Name.Contains("AE-5", StringComparison.OrdinalIgnoreCase) || InterfaceName.Contains("AE-5", StringComparison.OrdinalIgnoreCase);
    public override string ToString() => Name;
}

/// <summary>Plain description of a PCM / float wave format (WAVEFORMATEXTENSIBLE).</summary>
public sealed record AudioFormat(int Channels, int SampleRate, int Bits, int ValidBits, bool IsFloat, uint ChannelMask)
{
    private static readonly Guid SubtypePcm = new("00000001-0000-0010-8000-00aa00389b71");
    private static readonly Guid SubtypeFloat = new("00000003-0000-0010-8000-00aa00389b71");

    public string Describe()
    {
        var bits = IsFloat ? "32 bit float" : $"{ValidBits} bit";
        return $"{SampleRate / 1000.0:0.#} kHz · {bits} · {LayoutName(Channels, ChannelMask)}";
    }

    public static string LayoutName(int channels, uint mask) => (channels, mask) switch
    {
        (1, _) => "mono",
        (2, _) => "stereo",
        (4, _) => "quadrifonico",
        (6, _) => "5.1",
        (8, _) => "7.1",
        _ => $"{channels} canali",
    };

    public static uint DefaultMask(int channels) => channels switch
    {
        1 => 0x4,
        2 => 0x3,
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3F,
        7 => 0x13F,
        8 => 0x63F,
        _ => 0,
    };

    public AudioFormat AsFloatMix() => this with { Bits = 32, ValidBits = 32, IsFloat = true };

    /// <summary>Serialises to a WAVEFORMATEXTENSIBLE (40 bytes).</summary>
    public byte[] ToBytes()
    {
        var b = new byte[40];
        int block = Channels * Bits / 8;
        BitConverter.TryWriteBytes(b.AsSpan(0), (ushort)0xFFFE);
        BitConverter.TryWriteBytes(b.AsSpan(2), (ushort)Channels);
        BitConverter.TryWriteBytes(b.AsSpan(4), (uint)SampleRate);
        BitConverter.TryWriteBytes(b.AsSpan(8), (uint)(SampleRate * block));
        BitConverter.TryWriteBytes(b.AsSpan(12), (ushort)block);
        BitConverter.TryWriteBytes(b.AsSpan(14), (ushort)Bits);
        BitConverter.TryWriteBytes(b.AsSpan(16), (ushort)22);
        BitConverter.TryWriteBytes(b.AsSpan(18), (ushort)ValidBits);
        BitConverter.TryWriteBytes(b.AsSpan(20), ChannelMask == 0 ? DefaultMask(Channels) : ChannelMask);
        (IsFloat ? SubtypeFloat : SubtypePcm).TryWriteBytes(b.AsSpan(24));
        return b;
    }

    public static AudioFormat? FromBytes(ReadOnlySpan<byte> b)
    {
        if (b.Length < 16) return null;
        int tag = BitConverter.ToUInt16(b[0..]);
        int ch = BitConverter.ToUInt16(b[2..]);
        int rate = (int)BitConverter.ToUInt32(b[4..]);
        int bits = BitConverter.ToUInt16(b[14..]);
        if (tag == 0xFFFE && b.Length >= 40)
        {
            int valid = BitConverter.ToUInt16(b[18..]);
            uint mask = BitConverter.ToUInt32(b[20..]);
            var sub = new Guid(b.Slice(24, 16));
            return new AudioFormat(ch, rate, bits, valid == 0 ? bits : valid, sub == SubtypeFloat, mask == 0 ? DefaultMask(ch) : mask);
        }
        return new AudioFormat(ch, rate, bits, bits, tag == 3, DefaultMask(ch));
    }

    public static AudioFormat? FromPtr(IntPtr p)
    {
        if (p == IntPtr.Zero) return null;
        int cb = Marshal.ReadInt16(p, 16);
        int size = Marshal.ReadInt16(p) == unchecked((short)0xFFFE) ? 18 + Math.Max(cb, 22) : 18;
        var bytes = new byte[size];
        Marshal.Copy(p, bytes, 0, size);
        return FromBytes(bytes);
    }

    public static AudioFormat? FromNAudio(NAudio.Wave.WaveFormat wf)
    {
        using var ms = new System.IO.MemoryStream();
        using (var w = new System.IO.BinaryWriter(ms)) wf.Serialize(w);
        // NAudio writes a 4-byte length prefix before the WAVEFORMATEX itself.
        var bytes = ms.ToArray();
        return bytes.Length > 4 ? FromBytes(bytes.AsSpan(4)) : null;
    }

    public bool SameLayout(AudioFormat o) => Channels == o.Channels && SampleRate == o.SampleRate && ChannelMask == o.ChannelMask;
}

/// <summary>Enumerates audio endpoints, finds the devices DualMix cares about, changes Windows audio policy.</summary>
public static class Devices
{
    private static readonly PropertyKey PKEY_DeviceDesc = new(new Guid("a45c254e-df1c-4efd-8020-67d146a850e0"), 2);
    private static readonly PropertyKey PKEY_DeviceInterfaceName = new(new Guid("026e516e-b814-414b-83cd-856d6fef4822"), 2);
    private static readonly PropertyKey PKEY_DeviceFormat = new(new Guid("f19f064d-082c-4e27-bc73-6882a1bb8e4c"), 0);
    private static readonly PropertyKey PKEY_FriendlyName = new(new Guid("a45c254e-df1c-4efd-8020-67d146a850e0"), 14);
    private static readonly PropertyKey PKEY_Adapter = new(new Guid("b3f8fa53-0004-438e-9003-51a46e139bfc"), 2);
    private static readonly PropertyKey PKEY_JackSubType = new(new Guid("1da5d803-d492-4edd-8c23-e0c0ffee7f0e"), 8);

    /// <summary>Factory name of the secondary VB-CABLE input (restored when DualMix stops using it).</summary>
    public const string SecondaryCableName = "CABLE In 16 Ch";

    public const string GameEndpointName = "DualMix Game";
    public const string ChatEndpointName = "DualMix Chat";

    public static List<DeviceInfo> List(DataFlow flow, DeviceState states = DeviceState.Active)
    {
        var list = new List<DeviceInfo>();
        try
        {
            using var en = new MMDeviceEnumerator();
            string? defId = null;
            try
            {
                if (en.HasDefaultAudioEndpoint(flow, Role.Multimedia))
                {
                    using var d = en.GetDefaultAudioEndpoint(flow, Role.Multimedia);
                    defId = d.ID;
                }
            }
            catch { /* no default */ }

            foreach (var dev in en.EnumerateAudioEndPoints(flow, states))
            {
                using (dev)
                {
                    string id = dev.ID;
                    string name = ReadString(dev, PKEY_FriendlyName) ?? id;
                    string desc = ReadString(dev, PKEY_DeviceDesc) ?? name;
                    string iface = ReadString(dev, PKEY_DeviceInterfaceName) ?? "";
                    DeviceState st;
                    try { st = dev.State; } catch { st = DeviceState.NotPresent; }
                    string adapter = ReadString(dev, PKEY_Adapter) ?? "";
                    string jack = ReadString(dev, PKEY_JackSubType) ?? "";
                    list.Add(new DeviceInfo(id, name, desc, iface, flow, st, id == defId, adapter, jack));
                }
            }
        }
        catch (Exception ex)
        {
            Log.Error("Device enumeration failed", ex);
        }
        list.Sort((a, b) => string.Compare(a.Name, b.Name, StringComparison.CurrentCultureIgnoreCase));
        return list;
    }

    public static List<DeviceInfo> ListRender() => List(DataFlow.Render);

    private static string? ReadString(MMDevice dev, PropertyKey key)
    {
        try
        {
            var store = dev.Properties;
            if (!store.Contains(key)) return null;
            var value = store[key].Value;
            return value as string ?? (value is Guid g ? g.ToString("B") : null);
        }
        catch
        {
            return null;
        }
    }

    /// <summary>The real output DualMix should use: the Sound Blaster AE-5 analog output when present.</summary>
    public static DeviceInfo? FindPreferredOutput(IReadOnlyList<DeviceInfo> render)
    {
        static int Score(DeviceInfo d)
        {
            if (d.IsVbCable) return -1;
            int s = 0;
            if (d.IsAe5) s += 100;
            var n = d.Name.ToLowerInvariant();
            if (n.Contains("spdif") || n.Contains("digital") || n.Contains("digitale")) s -= 50;
            if (n.Contains("altoparlanti") || n.Contains("speakers") || n.Contains("cuffie") || n.Contains("headphones")) s += 10;
            return s;
        }
        return render.Where(d => d.IsAe5 && Score(d) > 0).OrderByDescending(Score).FirstOrDefault();
    }

    /// <summary>
    /// The usable virtual buses: one per cable device (the two inputs of the free VB-CABLE share one stream and cannot
    /// be used together, so only its main jack counts). Bit-transparent cables come first.
    /// </summary>
    public static List<DeviceInfo> FindBuses(IReadOnlyList<DeviceInfo> render) =>
        render.Where(d => d.IsVbCable && d.State == DeviceState.Active)
              .GroupBy(d => d.DeviceKey, StringComparer.OrdinalIgnoreCase)
              .Select(g => g.OrderBy(d => d.IsMainJack ? 0 : 1).First())
              .OrderBy(d => d.IsLossyCable ? 1 : 0)
              .ThenBy(d => d.InterfaceName, StringComparer.OrdinalIgnoreCase)
              .ToList();

    /// <summary>
    /// Picks the virtual devices. With one cable it carries Chat (games then play straight on the real output).
    /// With two or more, Game gets the first bit-transparent cable and Chat the next one.
    /// </summary>
    public static (DeviceInfo? game, DeviceInfo? chat) FindCables(IReadOnlyList<DeviceInfo> render, string? savedGame, string? savedChat)
    {
        var buses = FindBuses(render);
        if (buses.Count == 0) return (null, null);
        if (buses.Count == 1) return (null, buses[0]);
        return (buses[0], buses[1]);
    }

    public static string? GetDefaultId(Role role, DataFlow flow = DataFlow.Render)
    {
        try
        {
            using var en = new MMDeviceEnumerator();
            if (!en.HasDefaultAudioEndpoint(flow, role)) return null;
            using var d = en.GetDefaultAudioEndpoint(flow, role);
            return d.ID;
        }
        catch
        {
            return null;
        }
    }

    public static bool IsActive(string? id)
    {
        if (string.IsNullOrEmpty(id)) return false;
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            return d.State == DeviceState.Active;
        }
        catch
        {
            return false;
        }
    }

    public static string NameOf(string? id)
    {
        if (string.IsNullOrEmpty(id)) return "(nessuno)";
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            return ReadString(d, PKEY_FriendlyName) ?? id;
        }
        catch (Exception ex)
        {
            Log.Warn($"Cannot read name of device {id}", ex);
            return "(non disponibile)";
        }
    }

    /// <summary>The endpoint's configured device format (Sound control panel → Advanced), read from its property store.</summary>
    public static AudioFormat? GetDeviceFormat(string id)
    {
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            var store = d.Properties;
            if (!store.Contains(PKEY_DeviceFormat)) return null;
            return store[PKEY_DeviceFormat].Value is byte[] blob ? AudioFormat.FromBytes(blob) : null;
        }
        catch (Exception ex)
        {
            Log.Warn($"Cannot read device format of {id}", ex);
            return null;
        }
    }

    /// <summary>
    /// Checks that the endpoint can actually be opened by applications. Returns null when fine,
    /// otherwise the failing HRESULT (0x80040154 = the Windows audio service lost the device: needs a restart).
    /// </summary>
    public static int? ProbeOpen(string id)
    {
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            var client = d.AudioClient;
            _ = client.MixFormat;
            client.Dispose();
            return null;
        }
        catch (COMException ex)
        {
            return ex.HResult;
        }
        catch (Exception ex)
        {
            return ex.HResult == 0 ? -1 : ex.HResult;
        }
    }

    // ------------------------------------------------------------------ policy (undocumented, stable since Vista)

    /// <summary>Sets the Windows default playback device for a role.</summary>
    public static bool SetDefault(string id, Role role) => WithPolicy(p =>
    {
        var hr = p.SetDefaultEndpoint(id, role);
        if (hr != 0) Log.Warn($"SetDefaultEndpoint({role}) hr=0x{hr:X8}");
        return hr == 0;
    });

    /// <summary>Changes the device format (what the Sound control panel "Advanced" tab does).</summary>
    public static bool SetDeviceFormat(string id, AudioFormat format)
    {
        var dev = format.ToBytes();
        var mix = format.AsFloatMix().ToBytes();
        var pDev = Marshal.AllocCoTaskMem(dev.Length);
        var pMix = Marshal.AllocCoTaskMem(mix.Length);
        try
        {
            Marshal.Copy(dev, 0, pDev, dev.Length);
            Marshal.Copy(mix, 0, pMix, mix.Length);
            return WithPolicy(p =>
            {
                var hr = p.SetDeviceFormat(id, pDev, pMix);
                if (hr != 0) Log.Warn($"SetDeviceFormat({format.Describe()}) on {id} hr=0x{hr:X8}");
                return hr == 0;
            });
        }
        finally
        {
            Marshal.FreeCoTaskMem(pDev);
            Marshal.FreeCoTaskMem(pMix);
        }
    }

    /// <summary>Renames an endpoint (its "device description", the first part of the friendly name).</summary>
    public static bool Rename(string id, string description)
    {
        if (RenameViaPolicy(id, description)) return true;
        // Fallback: write the property store directly (only works elevated).
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            d.GetPropertyInformation(NAudio.CoreAudioApi.Interfaces.StorageAccessMode.ReadWrite);
            var store = d.Properties;
            var pv = new NAudio.CoreAudioApi.Interfaces.PropVariant { vt = (short)VarEnum.VT_LPWSTR, pointerValue = Marshal.StringToCoTaskMemUni(description) };
            try
            {
                store.SetValue(PKEY_DeviceDesc, pv);
                store.Commit();
            }
            finally
            {
                Marshal.FreeCoTaskMem(pv.pointerValue);
            }
            return true;
        }
        catch (Exception ex)
        {
            Log.Warn($"Rename endpoint {id} failed", ex);
            return false;
        }
    }

    private static bool RenameViaPolicy(string id, string description)
    {
        var key = new PROPERTYKEY { fmtid = PKEY_DeviceDesc.formatId, pid = PKEY_DeviceDesc.propertyId };
        var pv = Marshal.AllocCoTaskMem(24);
        var str = Marshal.StringToCoTaskMemUni(description);
        try
        {
            for (int i = 0; i < 24; i++) Marshal.WriteByte(pv, i, 0);
            Marshal.WriteInt16(pv, 31 /* VT_LPWSTR */);
            Marshal.WriteIntPtr(pv, 8, str);
            return WithPolicy(p =>
            {
                var hr = p.SetPropertyValue(id, 0, ref key, pv);
                if (hr != 0) Log.Warn($"Rename endpoint {id} hr=0x{hr:X8}");
                return hr == 0;
            });
        }
        finally
        {
            Marshal.FreeCoTaskMem(str);
            Marshal.FreeCoTaskMem(pv);
        }
    }

    /// <summary>Enables/disables an endpoint (hidden endpoints do not appear in app device lists).</summary>
    public static bool SetVisible(string id, bool visible) => WithPolicy(p =>
    {
        var hr = p.SetEndpointVisibility(id, visible ? 1 : 0);
        if (hr != 0) Log.Warn($"SetEndpointVisibility({visible}) on {id} hr=0x{hr:X8}");
        return hr == 0;
    });

    /// <summary>Endpoint volume to 100% and unmuted (bit-transparent path).</summary>
    public static void SetUnityVolume(string id)
    {
        try
        {
            using var en = new MMDeviceEnumerator();
            using var d = en.GetDevice(id);
            var v = d.AudioEndpointVolume;
            if (Math.Abs(v.MasterVolumeLevelScalar - 1f) > 0.0001f) v.MasterVolumeLevelScalar = 1f;
            if (v.Mute) v.Mute = false;
        }
        catch (Exception ex)
        {
            Log.Warn($"Cannot set unity volume on {id}", ex);
        }
    }

    private static bool WithPolicy(Func<IPolicyConfig, bool> action)
    {
        IPolicyConfig? policy = null;
        try
        {
            policy = (IPolicyConfig)new PolicyConfigClient();
            return action(policy);
        }
        catch (Exception ex)
        {
            Log.Error("IPolicyConfig call failed", ex);
            return false;
        }
        finally
        {
            if (policy != null) Marshal.ReleaseComObject(policy);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct PROPERTYKEY
    {
        public Guid fmtid;
        public int pid;
    }

    [ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")]
    private class PolicyConfigClient
    {
    }

    [ComImport, Guid("f8679f50-850a-41cf-9c72-430f290290c8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IPolicyConfig
    {
        [PreserveSig] int GetMixFormat([MarshalAs(UnmanagedType.LPWStr)] string id, out IntPtr format);
        [PreserveSig] int GetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id, int useDefault, out IntPtr format);
        [PreserveSig] int ResetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id);
        [PreserveSig] int SetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr endpointFormat, IntPtr mixFormat);
        [PreserveSig] int GetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string id, int useDefault, out long defaultPeriod, out long minimumPeriod);
        [PreserveSig] int SetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr period);
        [PreserveSig] int GetShareMode([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr mode);
        [PreserveSig] int SetShareMode([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr mode);
        [PreserveSig] int GetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, ref PROPERTYKEY key, IntPtr value);
        [PreserveSig] int SetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, ref PROPERTYKEY key, IntPtr value);
        [PreserveSig] int SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string id, Role role);
        [PreserveSig] int SetEndpointVisibility([MarshalAs(UnmanagedType.LPWStr)] string id, int visible);
    }
}

/// <summary>
/// Per-application output device ("App volume and device preferences" in Windows settings),
/// through the internal Windows.Media.Internal.AudioPolicyConfig factory (same API used by EarTrumpet).
/// Called through the raw vtable because .NET 5+ has no built-in WinRT interop.
/// </summary>
public static unsafe class AppDeviceRouting
{
    private const string RenderInterface = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";
    private const string DevicePrefix = @"\\?\SWD#MMDEVAPI#";

    // Windows 10 21H2+/Windows 11 and the older down-level variant share the same vtable layout.
    private static readonly Guid[] FactoryIids =
    {
        new("ab3d4648-e242-459f-b02f-541c70306324"),
        new("2a59116d-6c4f-45e0-a74f-707e3fef9258"),
    };

    private const int SlotSetPersisted = 25;
    private const int SlotGetPersisted = 26;

    private static IntPtr _factory;
    private static bool _initialized;
    private static readonly object Gate = new();

    public static bool IsAvailable
    {
        get
        {
            EnsureFactory();
            return _factory != IntPtr.Zero;
        }
    }

    private static void EnsureFactory()
    {
        lock (Gate)
        {
            if (_initialized) return;
            _initialized = true;
            try
            {
                RoInitialize(1); // multithreaded; S_FALSE / RPC_E_CHANGED_MODE are fine
                const string className = "Windows.Media.Internal.AudioPolicyConfig";
                if (WindowsCreateString(className, className.Length, out var hClass) != 0) return;
                try
                {
                    foreach (var iid in FactoryIids)
                    {
                        var g = iid;
                        if (RoGetActivationFactory(hClass, ref g, out var f) == 0 && f != IntPtr.Zero)
                        {
                            _factory = f;
                            Log.Info($"AudioPolicyConfig factory available ({iid})");
                            return;
                        }
                    }
                    Log.Warn("AudioPolicyConfig factory not available: per-app routing disabled");
                }
                finally
                {
                    WindowsDeleteString(hClass);
                }
            }
            catch (Exception ex)
            {
                Log.Error("AudioPolicyConfig init failed", ex);
            }
        }
    }

    /// <summary>Routes a process to a playback device. <paramref name="deviceId"/> null resets to the Windows default.</summary>
    public static bool SetAppDevice(uint processId, string? deviceId)
    {
        EnsureFactory();
        if (_factory == IntPtr.Zero) return false;
        IntPtr h = IntPtr.Zero;
        try
        {
            if (!string.IsNullOrEmpty(deviceId))
            {
                var full = DevicePrefix + deviceId + "#" + RenderInterface;
                if (WindowsCreateString(full, full.Length, out h) != 0) return false;
            }

            var fn = (delegate* unmanaged[Stdcall]<IntPtr, uint, int, int, IntPtr, int>)(*(*(void***)_factory + SlotSetPersisted));
            var hr1 = fn(_factory, processId, 0 /*eRender*/, 0 /*eConsole*/, h);
            var hr2 = fn(_factory, processId, 0 /*eRender*/, 1 /*eMultimedia*/, h);
            fn(_factory, processId, 0 /*eRender*/, 2 /*eCommunications*/, h); // best effort
            const int InvalidArg = unchecked((int)0x80070057); // process without an audio policy entry (helper processes)
            if ((hr1 != 0 || hr2 != 0) && !(h == IntPtr.Zero && hr1 == InvalidArg))
                Log.Warn($"SetPersistedDefaultAudioEndpoint pid={processId} hr=0x{hr1:X8}/0x{hr2:X8}");
            return hr1 == 0 && hr2 == 0;
        }
        catch (Exception ex)
        {
            Log.Error("SetAppDevice failed", ex);
            return false;
        }
        finally
        {
            if (h != IntPtr.Zero) WindowsDeleteString(h);
        }
    }

    /// <summary>Returns the MMDevice id a process is routed to, empty string for "default", null on error.</summary>
    public static string? GetAppDevice(uint processId)
    {
        EnsureFactory();
        if (_factory == IntPtr.Zero) return null;
        try
        {
            var fn = (delegate* unmanaged[Stdcall]<IntPtr, uint, int, int, IntPtr*, int>)(*(*(void***)_factory + SlotGetPersisted));
            IntPtr h = IntPtr.Zero;
            var hr = fn(_factory, processId, 0, 1, &h);
            if (hr != 0) return null;
            if (h == IntPtr.Zero) return string.Empty;
            try
            {
                var p = WindowsGetStringRawBuffer(h, out var len);
                var s = Marshal.PtrToStringUni(p, (int)len) ?? string.Empty;
                if (s.StartsWith(DevicePrefix, StringComparison.OrdinalIgnoreCase)) s = s[DevicePrefix.Length..];
                var hash = s.LastIndexOf('#');
                if (hash > 0) s = s[..hash];
                return s;
            }
            finally
            {
                WindowsDeleteString(h);
            }
        }
        catch (Exception ex)
        {
            Log.Error("GetAppDevice failed", ex);
            return null;
        }
    }

    [DllImport("combase.dll")]
    private static extern int RoInitialize(int initType);

    [DllImport("combase.dll")]
    private static extern int RoGetActivationFactory(IntPtr activatableClassId, ref Guid iid, out IntPtr factory);

    [DllImport("combase.dll", CharSet = CharSet.Unicode)]
    private static extern int WindowsCreateString(string sourceString, int length, out IntPtr hstring);

    [DllImport("combase.dll")]
    private static extern int WindowsDeleteString(IntPtr hstring);

    [DllImport("combase.dll")]
    private static extern IntPtr WindowsGetStringRawBuffer(IntPtr hstring, out uint length);
}
