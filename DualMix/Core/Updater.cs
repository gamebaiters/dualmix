using System;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Net.Http;
using System.Security.Cryptography;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace DualMix.Core;

/// <summary>A version newer than the running one, as described by the version document.</summary>
/// <param name="Location">Where the package is: an https address or a file path.</param>
/// <param name="Sha256">SHA-256 of the package (hex); the package is refused when it does not match.</param>
public sealed record UpdateInfo(Version Version, string Location, string Sha256, string? Notes);

public enum UpdateState
{
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Installing,
    Failed,
}

/// <summary>
/// Looks for a newer DualMix, downloads it and hands it to its own setup.
///
/// The scheme is the one of RP Soundboard's update checker: a small version document at a fixed address (there
/// <c>version.xml</c> on a branch of the GitHub repository, here <c>version.json</c>) names the latest version and
/// the address of its package among the GitHub releases; the program compares it with its own version, downloads
/// the package to the temporary folder and starts an installer that waits for the program to close, replaces it and
/// starts it again. Checks are spaced out: a day after a check, three days after "not now".
///
/// The version document:
/// <code>
/// { "product": "DualMix", "version": "2.3.0",
///   "url": "https://github.com/gamebaiters/DualMix/releases/download/v2.3.0/DualMix-2.3.0-win-x64.zip",
///   "sha256": "…", "notes": "what is new" }
/// </code>
/// "url" may be relative to the document. The feed may also be a folder (or a file) instead of an https address:
/// a shared or synchronised folder that holds <c>version.json</c> next to the package works the same way.
/// The package is the release zip: <c>setup\setup.ps1</c> plus the program in <c>app\</c>.
/// </summary>
public sealed class Updater
{
    /// <summary>Version document of the public repository (same layout RP Soundboard uses for its own).</summary>
    public const string DefaultFeed = "https://raw.githubusercontent.com/gamebaiters/DualMix/main/version.json";

    private const long MaxPackageBytes = 400L * 1024 * 1024;
    private const int MaxDocumentBytes = 256 * 1024;

    private static readonly HttpClient Http = CreateClient();
    private static readonly JsonSerializerOptions Json = new() { PropertyNameCaseInsensitive = true };

    private readonly Settings _settings;
    private readonly Action _settingsChanged;
    private readonly SemaphoreSlim _gate = new(1, 1);

    public Updater(Settings settings, Action settingsChanged)
    {
        _settings = settings;
        _settingsChanged = settingsChanged;
    }

    /// <summary>Version of the running program (major.minor.build).</summary>
    public static Version Current { get; } = Normalize(typeof(Updater).Assembly.GetName().Version ?? new Version(0, 0, 0));

    public UpdateState State { get; private set; } = UpdateState.Idle;

    /// <summary>The update found by the last check, if any.</summary>
    public UpdateInfo? Available { get; private set; }

    /// <summary>One line for the user about the last thing that happened (Italian).</summary>
    public string StatusText { get; private set; } = "";

    /// <summary>Download progress, 0..1.</summary>
    public double Progress { get; private set; }

    /// <summary>Raised (on any thread) when <see cref="State"/>, <see cref="StatusText"/> or the progress change.</summary>
    public event Action? Changed;

    public string Feed => string.IsNullOrWhiteSpace(_settings.UpdateFeed) ? DefaultFeed : _settings.UpdateFeed!.Trim();

    public bool Busy => State is UpdateState.Checking or UpdateState.Downloading or UpdateState.Installing;

    /// <summary>
    /// Reads the version document. Returns the update when there is one, null when DualMix is up to date or the
    /// document could not be read (see <see cref="State"/>).
    /// </summary>
    public async Task<UpdateInfo?> CheckAsync()
    {
        if (!await _gate.WaitAsync(0).ConfigureAwait(false)) return Available;
        try
        {
            Set(UpdateState.Checking, Loc.Get("Updater.Checking"));
            var feed = Feed;
            var info = Parse(await ReadDocumentAsync(feed).ConfigureAwait(false), feed);
            _settings.UpdateNextCheck = DateTime.Now.AddDays(1);
            _settingsChanged();
            if (info.Version > Current)
            {
                Available = info;
                Log.Info($"Update: version {info.Version} is available (running {Current})");
                Set(UpdateState.Available, Loc.Format("Updater.AvailableLine", info.Version));
                return info;
            }
            Available = null;
            Log.Info($"Update: up to date ({Current}, latest {info.Version})");
            Set(UpdateState.UpToDate, Loc.Format("Updater.UpToDateTime", DateTime.Now));
            return null;
        }
        catch (Exception ex)
        {
            Log.Warn("Update: check failed", ex);
            _settings.UpdateNextCheck = DateTime.Now.AddDays(1);
            _settingsChanged();
            Set(UpdateState.Failed, Loc.Format("Updater.CheckFailedPrefix", Describe(ex)));
            return null;
        }
        finally
        {
            _gate.Release();
        }
    }

    /// <summary>The user does not want the update now: do not ask again for three days.</summary>
    public void Postpone()
    {
        _settings.UpdateNextCheck = DateTime.Now.AddDays(3);
        _settingsChanged();
    }

    /// <summary>
    /// Downloads the package, checks it and starts its setup, which closes DualMix, replaces it and starts it
    /// again. Returns false when something went wrong before the setup started (DualMix keeps running untouched).
    /// </summary>
    public async Task<bool> InstallAsync(UpdateInfo info)
    {
        if (!await _gate.WaitAsync(0).ConfigureAwait(false)) return false;
        try
        {
            Progress = 0;
            Set(UpdateState.Downloading, Loc.Format("Updater.Downloading", info.Version));
            var work = Path.Combine(Path.GetTempPath(), "DualMix-update", info.Version.ToString());
            if (Directory.Exists(work)) Directory.Delete(work, true);
            Directory.CreateDirectory(work);

            var zip = Path.Combine(work, "package.zip");
            var hash = await FetchPackageAsync(info.Location, zip).ConfigureAwait(false);
            if (!hash.Equals(info.Sha256.Trim(), StringComparison.OrdinalIgnoreCase))
                throw new InvalidDataException(Loc.Format("Updater.InvalidPackage", hash));

            var pkg = Path.Combine(work, "pkg");
            ZipFile.ExtractToDirectory(zip, pkg, overwriteFiles: true);
            File.Delete(zip);
            var script = Path.Combine(pkg, "setup", "setup.ps1");
            if (!File.Exists(script) || !File.Exists(Path.Combine(pkg, "app", "DualMix.dll")))
                throw new InvalidDataException(Loc.Get("Updater.MissingSetup"));

            Set(UpdateState.Installing, Loc.Format("Updater.StartingInstall", info.Version));
            Log.Info($"Update: starting the setup of {info.Version} ({script})");
            StartSetup(script);
        }
        catch (Exception ex)
        {
            Log.Error("Update: installation failed", ex);
            Set(UpdateState.Failed, Loc.Format("Updater.UpdateFailedPrefix", Describe(ex)));
            return false;
        }
        finally
        {
            _gate.Release();
        }

        // The setup closes this process within seconds. Still here after a while: it did not work.
        _ = Task.Delay(TimeSpan.FromMinutes(3)).ContinueWith(_ =>
        {
            if (State != UpdateState.Installing) return;
            Log.Warn("Update: the setup did not replace DualMix (see setup.log)");
            Set(UpdateState.Failed, Loc.Get("Updater.SetupStalled"));
        }, TaskScheduler.Default);
        return true;
    }

    // ------------------------------------------------------------------ version document

    private sealed class Document
    {
        public string? Product { get; set; }
        public string? Version { get; set; }
        public string? Url { get; set; }
        public string? Sha256 { get; set; }
        public string? Notes { get; set; }
    }

    private static UpdateInfo Parse(string json, string feed)
    {
        var doc = JsonSerializer.Deserialize<Document>(json, Json) ?? throw new InvalidDataException(Loc.Get("Updater.EmptyDoc"));
        if (!string.Equals(doc.Product, "DualMix", StringComparison.OrdinalIgnoreCase))
            throw new InvalidDataException(Loc.Get("Updater.WrongDoc"));
        if (!Version.TryParse(doc.Version, out var version)) throw new InvalidDataException(Loc.Format("Updater.InvalidVer", doc.Version));
        if (string.IsNullOrWhiteSpace(doc.Url)) throw new InvalidDataException(Loc.Get("Updater.MissingUrl"));
        var sha = doc.Sha256?.Trim() ?? "";
        if (sha.Length != 64 || !Uri.IsHexDigit(sha[0]) || sha.AsSpan().IndexOfAnyExcept("0123456789abcdefABCDEF") >= 0)
            throw new InvalidDataException(Loc.Get("Updater.MissingSha"));
        return new UpdateInfo(Normalize(version), Resolve(doc.Url.Trim(), feed), sha, string.IsNullOrWhiteSpace(doc.Notes) ? null : doc.Notes.Trim());
    }

    /// <summary>Package address, absolute or relative to the version document.</summary>
    private static string Resolve(string url, string feed)
    {
        if (IsWeb(url)) return RequireHttps(url);
        if (IsWeb(feed)) return RequireHttps(new Uri(new Uri(feed), url).AbsoluteUri);
        if (Path.IsPathFullyQualified(url)) return url;
        return Path.GetFullPath(Path.Combine(Path.GetDirectoryName(DocumentPath(feed)) ?? ".", url));
    }

    private static bool IsWeb(string s) =>
        s.StartsWith("https://", StringComparison.OrdinalIgnoreCase) || s.StartsWith("http://", StringComparison.OrdinalIgnoreCase);

    /// <summary>Program code is never fetched over a connection that anyone on the way could alter.</summary>
    private static string RequireHttps(string url) =>
        url.StartsWith("https://", StringComparison.OrdinalIgnoreCase) ? url : throw new InvalidDataException("è ammesso solo https: " + url);

    private static string DocumentPath(string feed) => Directory.Exists(feed) ? Path.Combine(feed, "version.json") : feed;

    private static async Task<string> ReadDocumentAsync(string feed)
    {
        if (!IsWeb(feed))
        {
            var path = DocumentPath(feed);
            if (new FileInfo(path).Length > MaxDocumentBytes) throw new InvalidDataException("documento di versione troppo grande");
            return await File.ReadAllTextAsync(path).ConfigureAwait(false);
        }
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        using var response = await Http.GetAsync(RequireHttps(feed), HttpCompletionOption.ResponseHeadersRead, cts.Token).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        if (response.Content.Headers.ContentLength > MaxDocumentBytes) throw new InvalidDataException("documento di versione troppo grande");
        var text = await response.Content.ReadAsStringAsync(cts.Token).ConfigureAwait(false);
        return text.Length <= MaxDocumentBytes ? text : throw new InvalidDataException("documento di versione troppo grande");
    }

    // ------------------------------------------------------------------ package

    /// <summary>Copies the package to <paramref name="target"/> and returns its SHA-256 (hex).</summary>
    private async Task<string> FetchPackageAsync(string location, string target)
    {
        using var cts = new CancellationTokenSource(TimeSpan.FromMinutes(15));
        Stream source;
        long? length;
        HttpResponseMessage? response = null;
        if (IsWeb(location))
        {
            response = await Http.GetAsync(RequireHttps(location), HttpCompletionOption.ResponseHeadersRead, cts.Token).ConfigureAwait(false);
            response.EnsureSuccessStatusCode();
            length = response.Content.Headers.ContentLength;
            source = await response.Content.ReadAsStreamAsync(cts.Token).ConfigureAwait(false);
        }
        else
        {
            var file = new FileStream(location, FileMode.Open, FileAccess.Read, FileShare.Read, 1 << 16, useAsync: true);
            length = file.Length;
            source = file;
        }

        try
        {
            if (length > MaxPackageBytes) throw new InvalidDataException(Loc.Get("Updater.TooLarge"));
            using var sha = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
            await using var output = new FileStream(target, FileMode.Create, FileAccess.Write, FileShare.None, 1 << 16, useAsync: true);
            var buffer = new byte[1 << 16];
            long total = 0;
            long nextReport = 0;
            int read;
            while ((read = await source.ReadAsync(buffer, cts.Token).ConfigureAwait(false)) > 0)
            {
                total += read;
                if (total > MaxPackageBytes) throw new InvalidDataException(Loc.Get("Updater.TooLarge"));
                sha.AppendData(buffer, 0, read);
                await output.WriteAsync(buffer.AsMemory(0, read), cts.Token).ConfigureAwait(false);
                if (length > 0 && Environment.TickCount64 >= nextReport)
                {
                    nextReport = Environment.TickCount64 + 200;
                    Progress = Math.Min(1, (double)total / length.Value);
                    Changed?.Invoke();
                }
            }
            Progress = 1;
            return Convert.ToHexString(sha.GetHashAndReset());
        }
        finally
        {
            await source.DisposeAsync().ConfigureAwait(false);
            response?.Dispose();
        }
    }

    /// <summary>
    /// Runs the setup of the package on its own (not as part of DualMix, which it is about to close), with no window:
    /// Windows PowerShell inside the headless console host, like DualMix itself.
    /// </summary>
    private static void StartSetup(string script)
    {
        var system = Environment.SystemDirectory;
        var powershell = Path.Combine(system, "WindowsPowerShell", "v1.0", "powershell.exe");
        var conhost = Path.Combine(system, "conhost.exe");
        var arguments = $"-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{script}\" -Update";
        var psi = File.Exists(conhost)
            ? new ProcessStartInfo(conhost, $"--headless \"{powershell}\" {arguments}")
            : new ProcessStartInfo(powershell, arguments);
        psi.UseShellExecute = false;
        psi.CreateNoWindow = true;
        psi.WorkingDirectory = Path.GetTempPath();
        using var p = Process.Start(psi) ?? throw new InvalidOperationException(Loc.Get("Updater.CantLaunchSetup"));
    }

    // ------------------------------------------------------------------ helpers

    private void Set(UpdateState state, string text)
    {
        State = state;
        StatusText = text;
        try { Changed?.Invoke(); } catch { /* a listener that is closing */ }
    }

    private static Version Normalize(Version v) => new(v.Major, Math.Max(0, v.Minor), Math.Max(0, v.Build));

    private static string Describe(Exception ex) => ex switch
    {
        HttpRequestException { StatusCode: System.Net.HttpStatusCode.NotFound } => Loc.Get("Updater.Err404"),
        HttpRequestException => Loc.Format("Updater.ErrHttp", ex.Message),
        OperationCanceledException => Loc.Get("Updater.ErrTimeout"),
        FileNotFoundException or DirectoryNotFoundException => Loc.Get("Updater.ErrNotFound"),
        _ => ex.Message,
    };

    private static HttpClient CreateClient()
    {
        var client = new HttpClient(new SocketsHttpHandler
        {
            AllowAutoRedirect = true,
            MaxAutomaticRedirections = 5,
            ConnectTimeout = TimeSpan.FromSeconds(15),
        })
        {
            Timeout = Timeout.InfiniteTimeSpan, // each request carries its own limit
        };
        client.DefaultRequestHeaders.UserAgent.ParseAdd("DualMix-Updater");
        return client;
    }
}
