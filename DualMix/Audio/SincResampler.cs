using System;
using System.Numerics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace DualMix.Audio;

/// <summary>
/// High-quality streaming resampler: polyphase Kaiser-windowed sinc, 160 taps, 2048 interpolated phases.
/// Kaiser β = 14: stop-band and pass-band ripple ≈ −135 dB, pass-band flat to 0.444·fs (21.3 kHz at 48 kHz),
/// linear phase. It is used for clock-drift compensation, i.e. ratios within a few hundred ppm of 1:1, where
/// its total error stays below the noise floor of a 24-bit converter.
/// Planar float storage, SIMD dot products. Not thread safe: one audio thread owns it.
/// </summary>
public sealed class SincResampler
{
    public const int Taps = 160;
    private const int Half = Taps / 2;
    private const int Phases = 2048;
    private const double KaiserBeta = 14.0;

    /// <summary>Centre of the transition band (cycles per input sample): the stop-band starts exactly at Nyquist.</summary>
    private const double BaseCutoff = 0.472;

    private readonly int _channels;
    private readonly float[] _table;   // (Phases + 1) rows of Taps coefficients
    private readonly float[] _coef = new float[Taps];
    private readonly float[][] _buf;   // per-channel input history
    private int _len;                  // valid samples per channel in _buf
    private double _pos;               // position (in _buf samples) of the next output sample

    public int Channels => _channels;
    public double Cutoff { get; }

    /// <param name="channels">Interleaved channel count.</param>
    /// <param name="maxStep">Largest input/output ratio expected (for the anti-alias cutoff).</param>
    public SincResampler(int channels, double maxStep = 1.0)
    {
        _channels = channels;
        Cutoff = BaseCutoff * Math.Min(1.0, 1.0 / Math.Max(1e-6, maxStep));
        _table = BuildTable(Cutoff);
        _buf = new float[channels][];
        for (int c = 0; c < channels; c++) _buf[c] = new float[65536];
        Reset();
    }

    /// <summary>Input frames buffered and not yet consumed (fractional).</summary>
    public double Pending => _len - (Taps - 1) - _pos;

    /// <summary>Samples held per channel, history included.</summary>
    public int Buffered => _len;

    /// <summary>Delay introduced by the filter itself, in input samples.</summary>
    public static int FilterDelay => Half;

    public void Reset()
    {
        // Prime with Half - 1 zeros so the first output sample has a full (silent) history.
        _len = Half - 1;
        for (int c = 0; c < _channels; c++) Array.Clear(_buf[c], 0, Taps);
        _pos = 0;
    }

    /// <summary>Drops <paramref name="frames"/> pending input frames (used to recover from large backlogs).</summary>
    public void Skip(int frames)
    {
        frames = Math.Max(0, Math.Min(frames, (int)Pending));
        _pos += frames;
        Compact();
    }

    /// <summary>Appends interleaved input frames.</summary>
    public void Write(ReadOnlySpan<float> interleaved, int frames)
    {
        EnsureCapacity(_len + frames);
        int ch = _channels;
        for (int c = 0; c < ch; c++)
        {
            var dst = _buf[c];
            int w = _len;
            for (int f = 0, i = c; f < frames; f++, i += ch) dst[w + f] = interleaved[i];
        }
        _len += frames;
    }

    /// <summary>Appends silence.</summary>
    public void WriteSilence(int frames)
    {
        EnsureCapacity(_len + frames);
        for (int c = 0; c < _channels; c++) Array.Clear(_buf[c], _len, frames);
        _len += frames;
    }

    /// <summary>
    /// Produces up to <paramref name="frames"/> planar output frames into <paramref name="output"/>[channel][index],
    /// advancing by <paramref name="step"/> input samples per output sample. Returns frames produced.
    /// </summary>
    public int Read(float[][] output, int frames, double step)
    {
        int produced = 0;
        int ch = _channels;
        var table = _table;
        var coef = _coef;
        while (produced < frames)
        {
            double p = _pos;
            int start = (int)p; // index in _buf of tap 0 (the buffer is pre-offset by Half - 1)
            if (start + Taps > _len) break;

            double frac = p - start;
            if (frac == 0.0)
            {
                // Exactly on an input sample: the kernel is a unit impulse, copy bit-exact.
                for (int c = 0; c < ch; c++) output[c][produced] = _buf[c][start + Half - 1];
            }
            else
            {
                double ph = frac * Phases;
                int p0 = (int)ph;
                InterpolateRow(table, p0 * Taps, (float)(ph - p0), coef);
                for (int c = 0; c < ch; c++) output[c][produced] = Dot(_buf[c], start, coef);
            }
            produced++;
            _pos = p + step;
        }
        Compact();
        return produced;
    }

    /// <summary>Like <see cref="Read"/> but only advances (used while the whole buffer is digital silence).</summary>
    public int Advance(int frames, double step)
    {
        int produced = 0;
        while (produced < frames)
        {
            if ((int)_pos + Taps > _len) break;
            produced++;
            _pos += step;
        }
        Compact();
        return produced;
    }

    private void Compact()
    {
        int drop = (int)_pos;
        if (drop < 16384) return;
        int keep = _len - drop;
        for (int c = 0; c < _channels; c++) Array.Copy(_buf[c], drop, _buf[c], 0, keep);
        _len = keep;
        _pos -= drop;
    }

    private void EnsureCapacity(int needed)
    {
        if (_buf[0].Length >= needed) return;
        int size = Math.Max(needed, _buf[0].Length * 2);
        for (int c = 0; c < _channels; c++) Array.Resize(ref _buf[c], size);
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static void InterpolateRow(float[] table, int row, float t, float[] dst)
    {
        var a = MemoryMarshal.Cast<float, Vector<float>>(table.AsSpan(row, Taps));
        var b = MemoryMarshal.Cast<float, Vector<float>>(table.AsSpan(row + Taps, Taps));
        var d = MemoryMarshal.Cast<float, Vector<float>>(dst.AsSpan());
        var vt = new Vector<float>(t);
        for (int i = 0; i < d.Length; i++) d[i] = a[i] + (b[i] - a[i]) * vt;
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static float Dot(float[] x, int start, float[] coef)
    {
        var xs = MemoryMarshal.Cast<float, Vector<float>>(x.AsSpan(start, Taps));
        var cs = MemoryMarshal.Cast<float, Vector<float>>(coef.AsSpan());
        var acc = Vector<float>.Zero;
        for (int i = 0; i < cs.Length; i++) acc += xs[i] * cs[i];
        return Vector.Sum(acc);
    }

    private static float[] BuildTable(double fc)
    {
        var table = new float[(Phases + 1) * Taps];
        double i0Beta = BesselI0(KaiserBeta);
        var row = new double[Taps];
        for (int p = 0; p <= Phases; p++)
        {
            double frac = (double)p / Phases;
            double sum = 0;
            for (int k = 0; k < Taps; k++)
            {
                // Tap k multiplies input sample (ip - Half + 1 + k); the output time is ip + frac.
                double u = k - (Half - 1) - frac;
                double sinc = u == 0 ? 2 * fc : Math.Sin(2 * Math.PI * fc * u) / (Math.PI * u);
                double r = u / Half;
                double w = Math.Abs(r) >= 1 ? 0 : BesselI0(KaiserBeta * Math.Sqrt(1 - r * r)) / i0Beta;
                row[k] = sinc * w;
                sum += row[k];
            }
            // Normalise every phase to exactly unity DC gain.
            for (int k = 0; k < Taps; k++) table[p * Taps + k] = (float)(row[k] / sum);
        }
        return table;
    }

    private static double BesselI0(double x)
    {
        double sum = 1, term = 1, half = x / 2;
        for (int k = 1; k < 200; k++)
        {
            term *= half / k;
            double t2 = term * term;
            sum += t2;
            if (t2 < sum * 1e-18) break;
        }
        return sum;
    }
}
