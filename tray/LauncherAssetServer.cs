using System.Collections.Concurrent;
using System.Drawing;
using System.Drawing.Imaging;
using System.Reflection;
using System.Security;
using System.Text;

namespace FocusStageTray;

internal sealed class LauncherAssetServer
{
    public const string VirtualOrigin = "https://app.focusstage.local";
    private const string ResourcePrefix = "FocusStageTray.webui.";

    private readonly string _distro;
    private readonly Assembly _assembly = typeof(LauncherAssetServer).Assembly;
    private readonly ConcurrentDictionary<string, (byte[] Bytes, string ContentType)> _iconCache = new(StringComparer.Ordinal);
    private volatile IReadOnlyDictionary<string, LinuxAppDto> _appLookup = new Dictionary<string, LinuxAppDto>(StringComparer.Ordinal);

    private static readonly (string From, string To)[] FallbackGradients =
    [
        ("#2563EB", "#1D4ED8"),
        ("#059669", "#047857"),
        ("#7C3AED", "#6D28D9"),
        ("#EA580C", "#C2410C"),
        ("#0284C7", "#0369A1"),
        ("#4F46E5", "#4338CA"),
    ];

    public LauncherAssetServer(string distro)
    {
        _distro = distro;
    }

    public void UpdateApps(IReadOnlyDictionary<string, LinuxAppDto> apps)
    {
        _appLookup = apps;
        // Evict cached icons whose app no longer exists or whose icon_path changed.
        foreach (var key in _iconCache.Keys)
        {
            if (!apps.ContainsKey(key))
            {
                _iconCache.TryRemove(key, out _);
            }
        }
    }

    public void ClearIconCache() => _iconCache.Clear();

    public bool TryResolve(string uriString, out byte[] bytes, out string contentType, out string cacheControl)
    {
        bytes = [];
        contentType = "application/octet-stream";
        cacheControl = "no-cache";

        if (!Uri.TryCreate(uriString, UriKind.Absolute, out var uri)) return false;
        if (!string.Equals(uri.Host, "app.focusstage.local", StringComparison.OrdinalIgnoreCase)) return false;

        var path = uri.AbsolutePath.TrimStart('/');
        if (string.IsNullOrEmpty(path)) path = "index.html";

        if (path.Equals("icons/brand.svg", StringComparison.OrdinalIgnoreCase))
        {
            bytes = Encoding.UTF8.GetBytes(BrandSvg);
            contentType = "image/svg+xml; charset=utf-8";
            cacheControl = "public, max-age=86400";
            return true;
        }

        if (path.StartsWith("icons/", StringComparison.OrdinalIgnoreCase))
        {
            var encodedId = path["icons/".Length..];
            var appId = Uri.UnescapeDataString(encodedId);
            if (_appLookup.TryGetValue(appId, out var app))
            {
                var resolved = _iconCache.GetOrAdd(appId, _ => LoadAppIcon(app));
                bytes = resolved.Bytes;
                contentType = resolved.ContentType;
                cacheControl = "public, max-age=3600";
                return true;
            }
            bytes = RenderFallbackSvg("?");
            contentType = "image/svg+xml; charset=utf-8";
            cacheControl = "no-cache";
            return true;
        }

        // Static webui embedded resource
        var resourceName = ResourcePrefix + path.Replace('/', '.');
        using var stream = _assembly.GetManifestResourceStream(resourceName);
        if (stream is null) return false;

        using var ms = new MemoryStream();
        stream.CopyTo(ms);
        bytes = ms.ToArray();
        contentType = GetMimeType(path);
        cacheControl = "no-cache";
        return true;
    }

    private (byte[] Bytes, string ContentType) LoadAppIcon(LinuxAppDto app)
    {
        try
        {
            if (!string.IsNullOrWhiteSpace(app.IconPath))
            {
                var unc = ShortcutManager.ToWslUncPath(_distro, app.IconPath!);
                if (File.Exists(unc))
                {
                    var ext = Path.GetExtension(unc).ToLowerInvariant();
                    switch (ext)
                    {
                        case ".svg":
                            return (File.ReadAllBytes(unc), "image/svg+xml");
                        case ".png":
                            return (File.ReadAllBytes(unc), "image/png");
                        case ".jpg":
                        case ".jpeg":
                            return (File.ReadAllBytes(unc), "image/jpeg");
                        case ".webp":
                            return (File.ReadAllBytes(unc), "image/webp");
                        case ".bmp":
                        {
                            using var bmp = new Bitmap(unc);
                            using var ms = new MemoryStream();
                            bmp.Save(ms, ImageFormat.Png);
                            return (ms.ToArray(), "image/png");
                        }
                        case ".ico":
                        {
                            using var ico = new Icon(unc, 64, 64);
                            using var bmp = ico.ToBitmap();
                            using var ms = new MemoryStream();
                            bmp.Save(ms, ImageFormat.Png);
                            return (ms.ToArray(), "image/png");
                        }
                    }
                }
            }
        }
        catch
        {
            // Fall through to clean SVG badge fallback.
        }

        return (RenderFallbackSvg(app.DisplayName), "image/svg+xml; charset=utf-8");
    }

    private static byte[] RenderFallbackSvg(string name)
    {
        int hash = 0;
        foreach (char c in name)
        {
            hash = unchecked(hash * 31 + c);
        }
        var (from, to) = FallbackGradients[Math.Abs(hash) % FallbackGradients.Length];
        string rawLetter = string.IsNullOrWhiteSpace(name) ? "L" : name.Trim()[0].ToString().ToUpperInvariant();
        string letter = SecurityElement.Escape(rawLetter) ?? "L";

        var svg = $"""
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
              <defs>
                <linearGradient id="g" x1="0%" y1="0%" x2="100%" y2="100%">
                  <stop offset="0%" stop-color="{from}"/>
                  <stop offset="100%" stop-color="{to}"/>
                </linearGradient>
              </defs>
              <rect x="2" y="2" width="60" height="60" rx="15" fill="url(#g)"/>
              <text x="32" y="34" text-anchor="middle" dominant-baseline="central" fill="#FFFFFF" font-family="'Segoe UI Variable Text','Segoe UI','Microsoft YaHei UI',sans-serif" font-size="27" font-weight="600">{letter}</text>
            </svg>
            """;
        return Encoding.UTF8.GetBytes(svg);
    }

    private static string GetMimeType(string path) => Path.GetExtension(path).ToLowerInvariant() switch
    {
        ".html" => "text/html; charset=utf-8",
        ".css" => "text/css; charset=utf-8",
        ".js" => "application/javascript; charset=utf-8",
        ".json" => "application/json; charset=utf-8",
        ".svg" => "image/svg+xml; charset=utf-8",
        ".png" => "image/png",
        ".ico" => "image/x-icon",
        _ => "application/octet-stream"
    };

    private const string BrandSvg = """
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
          <defs>
            <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
              <stop offset="0%" stop-color="#1A2642"/>
              <stop offset="100%" stop-color="#0F172A"/>
            </linearGradient>
            <linearGradient id="ring" x1="0%" y1="0%" x2="100%" y2="100%">
              <stop offset="0%" stop-color="#60A5FA"/>
              <stop offset="100%" stop-color="#38BDF8"/>
            </linearGradient>
          </defs>
          <circle cx="32" cy="32" r="29" fill="url(#bg)"/>
          <circle cx="32" cy="32" r="21" fill="none" stroke="url(#ring)" stroke-width="5"/>
          <circle cx="32" cy="32" r="9.5" fill="#6EE7B7"/>
        </svg>
        """;
}
