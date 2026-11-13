using System;

namespace DualMix.Core;

/// <summary>Effective linear (amplitude) gains for the two channels.</summary>
public readonly record struct ChannelGains(float Game, float Chat)
{
    public float For(Channel channel) => channel switch
    {
        Channel.Game => Game,
        Channel.Chat => Chat,
        _ => 1f,
    };
}

public static class MixMath
{
    /// <summary>
    /// Perceptual taper: slider position (0..1) to amplitude. A square law follows loudness perception
    /// closely (50% ≈ −12 dB) and, unlike a pure dB scale, still reaches true silence at 0.
    /// 100% is exactly 1.0, so the signal passes untouched.
    /// </summary>
    public static double Taper(double position)
    {
        position = Math.Clamp(position, 0, 1);
        return position * position;
    }

    /// <summary>
    /// Balance curve like SteelSeries ChatMix: at the center both channels play at 100%;
    /// moving towards one side keeps that side at 100% and fades the other one (perceptually) down to
    /// <paramref name="floor"/>. Returns slider-scale positions (apply <see cref="Taper"/> for amplitude).
    /// </summary>
    public static (double game, double chat) Balance(double mix, double floor = 0)
    {
        mix = Math.Clamp(mix, -1, 1);
        floor = Math.Clamp(floor, 0, 1);
        var game = mix <= 0 ? 1.0 : 1.0 - mix * (1.0 - floor);
        var chat = mix >= 0 ? 1.0 : 1.0 + mix * (1.0 - floor);
        return (game, chat);
    }

    /// <summary>Final amplitude gains: volume × balance, both on the perceptual scale.</summary>
    public static ChannelGains Compute(Settings s)
    {
        var (g, c) = Balance(s.Mix, s.MixFloor);
        var game = s.GameMuted ? 0 : Taper(s.GameVolume * g);
        var chat = s.ChatMuted ? 0 : Taper(s.ChatVolume * c);
        return new ChannelGains((float)game, (float)chat);
    }

    /// <summary>Perceived level (0..1, slider scale) of each channel, for display.</summary>
    public static (double game, double chat) Levels(Settings s)
    {
        var (g, c) = Balance(s.Mix, s.MixFloor);
        return (s.GameMuted ? 0 : s.GameVolume * g, s.ChatMuted ? 0 : s.ChatVolume * c);
    }

    /// <summary>Snaps values very close to the center to exactly 0 (makes the knob "click" into the center).</summary>
    public static double SnapCenter(double mix) => Math.Abs(mix) < 0.005 ? 0 : Math.Round(mix, 3);

    public static string Percent(double v) => $"{Math.Round(v * 100):0}%";

    public static string Decibels(double amplitude)
    {
        if (amplitude <= 0.00001) return "−∞ dB";
        var db = 20 * Math.Log10(amplitude);
        return Math.Abs(db) < 0.05 ? "0 dB" : $"{db:+0.0;−0.0} dB";
    }
}
