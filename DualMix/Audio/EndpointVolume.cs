using System;
using DualMix.Core;
using NAudio.CoreAudioApi;

namespace DualMix.Audio;

/// <summary>
/// Volume/mute of one playback endpoint, with change notifications.
/// Used for the master volume (the real output, e.g. the AE-5) and for the two virtual devices, whose Windows
/// volume sliders are the DualMix Game/Chat volumes (the volume of the cable does not touch the audio DualMix
/// captures, so DualMix applies it).
/// Lives on the UI thread; notifications arrive on a system thread and are marshalled by the caller.
/// </summary>
public sealed class EndpointVolume : IDisposable
{
    private static readonly Guid OwnContext = new("6d1f6c0a-2f0e-4b6e-9b56-d0a1f3c2e7a1");

    private MMDeviceEnumerator? _en;
    private MMDevice? _device;
    private AudioEndpointVolume? _volume;
    private float _minDb = -96f;

    public string? DeviceId { get; private set; }

    /// <summary>Raised on a system thread when someone else (Windows, another app) changes volume or mute.</summary>
    public event Action? Changed;

    public bool IsAvailable => _volume != null;

    public void Attach(string? deviceId)
    {
        if (deviceId == DeviceId && _volume != null) return;
        Detach();
        if (string.IsNullOrEmpty(deviceId)) return;
        try
        {
            _en = new MMDeviceEnumerator();
            _device = _en.GetDevice(deviceId);
            _volume = _device.AudioEndpointVolume;
            _volume.NotificationGuid = OwnContext;
            _minDb = _volume.VolumeRange.MinDecibels;
            _volume.OnVolumeNotification += OnNotify;
            DeviceId = deviceId;
        }
        catch (Exception ex)
        {
            Log.Warn("Endpoint volume unavailable", ex);
            Detach();
        }
    }

    private void OnNotify(AudioVolumeNotificationData data)
    {
        // Changes made by DualMix are applied synchronously by the caller: only report foreign ones.
        if (data.Guid == OwnContext) return;
        try { Changed?.Invoke(); } catch { /* never throw into the audio service */ }
    }

    /// <summary>Slider position 0..1 (the scale Windows shows).</summary>
    public double Level
    {
        get
        {
            try { return _volume?.MasterVolumeLevelScalar ?? 0; }
            catch { return 0; }
        }
        set
        {
            try { if (_volume != null) _volume.MasterVolumeLevelScalar = (float)Math.Clamp(value, 0, 1); }
            catch (Exception ex) { Log.Warn("Cannot set endpoint volume", ex); }
        }
    }

    public bool Muted
    {
        get
        {
            try { return _volume?.Mute ?? false; }
            catch { return false; }
        }
        set
        {
            try { if (_volume != null && _volume.Mute != value) _volume.Mute = value; }
            catch (Exception ex) { Log.Warn("Cannot set endpoint mute", ex); }
        }
    }

    /// <summary>Linear gain equivalent to the current level (the dB curve of the endpoint); exactly 1 at 100%, 0 at 0%.</summary>
    public double Amplitude
    {
        get
        {
            try
            {
                if (_volume == null) return 1;
                float db = _volume.MasterVolumeLevel;
                if (db >= -0.001f) return 1;
                if (db <= _minDb + 0.01f) return 0;
                return Math.Pow(10, db / 20.0);
            }
            catch
            {
                return 1;
            }
        }
    }

    private void Detach()
    {
        try { if (_volume != null) _volume.OnVolumeNotification -= OnNotify; } catch { /* ignore */ }
        try { _volume?.Dispose(); } catch { /* ignore */ }
        try { _device?.Dispose(); } catch { /* ignore */ }
        try { _en?.Dispose(); } catch { /* ignore */ }
        _volume = null;
        _device = null;
        _en = null;
        DeviceId = null;
    }

    public void Dispose() => Detach();
}
