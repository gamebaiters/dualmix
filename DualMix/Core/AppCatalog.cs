using System;
using System.Collections.Generic;
using System.Collections.Frozen;
using System.Linq;

namespace DualMix.Core;

/// <summary>Built-in knowledge about which processes are voice/chat applications.</summary>
public static class AppCatalog
{
    /// <summary>Process names (without ".exe", case-insensitive) classified as Chat by default.</summary>
    public static readonly FrozenSet<string> ChatProcesses = new[]
    {
        // Discord and alternative clients
        "discord", "discordptb", "discordcanary", "discorddevelopment", "vesktop", "webcord", "legcord", "armcord", "ripcord",
        // TeamSpeak 3 / 5 / 6
        "ts3client_win64", "ts3client_win32", "teamspeak", "teamspeak3", "teamspeak5", "teamspeak6",
        // Other voice apps
        "mumble", "skype", "skypeapp", "lync", "ms-teams", "teams", "zoom", "slack", "signal", "whatsapp", "whatsapp.root",
        "telegram", "element", "guilded", "ventrilo", "raidcall", "revolt", "webex", "ciscowebexstart", "jitsi meet",
    }.ToFrozenSet(StringComparer.OrdinalIgnoreCase);

    public static bool IsChatProcess(string processName, IEnumerable<string>? extra = null)
    {
        if (ChatProcesses.Contains(processName)) return true;
        if (extra != null)
        {
            foreach (var e in extra)
            {
                if (string.Equals(Normalize(e), processName, StringComparison.OrdinalIgnoreCase)) return true;
            }
        }
        return false;
    }

    public static string Normalize(string processName)
    {
        var n = processName.Trim();
        if (n.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) n = n[..^4];
        return n;
    }
}

/// <summary>Immutable snapshot of the classification rules, safe to read from audio threads.</summary>
public sealed class RuleSet
{
    public const string SystemSoundsKey = "#system";

    private readonly Dictionary<string, (Channel? channel, double volume)> _rules;
    private readonly HashSet<string> _extraChat;

    public Channel DefaultChannel { get; }
    public Channel SystemSoundsChannel { get; }

    public RuleSet(Settings s)
    {
        _rules = s.Apps.ToDictionary(kv => kv.Key, kv => (kv.Value.Channel, kv.Value.Volume), StringComparer.OrdinalIgnoreCase);
        _extraChat = new HashSet<string>(s.ExtraChatApps.Select(AppCatalog.Normalize), StringComparer.OrdinalIgnoreCase);
        DefaultChannel = s.DefaultChannel;
        SystemSoundsChannel = s.SystemSoundsChannel;
    }

    public bool HasRule(string key) => _rules.ContainsKey(key);

    /// <summary>Channel that applies when the user did not pick one explicitly.</summary>
    public Channel AutoChannel(string processName, bool isSystemSounds)
    {
        if (isSystemSounds) return SystemSoundsChannel;
        if (AppCatalog.ChatProcesses.Contains(processName) || _extraChat.Contains(processName)) return Channel.Chat;
        return DefaultChannel;
    }

    public Channel Resolve(string key, bool isSystemSounds)
    {
        if (_rules.TryGetValue(key, out var r) && r.channel.HasValue) return r.channel.Value;
        return AutoChannel(key, isSystemSounds);
    }

    public Channel? ExplicitChannel(string key) => _rules.TryGetValue(key, out var r) ? r.channel : null;

    public double VolumeOf(string key) => _rules.TryGetValue(key, out var r) ? r.volume : 1.0;
}
