using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal sealed class LinuxAppDto
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("stem")]
    public string Stem { get; set; } = "";

    [JsonPropertyName("source")]
    public string Source { get; set; } = "desktop";

    [JsonPropertyName("file_path")]
    public string? FilePath { get; set; }

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("name_zh_cn")]
    public string? NameZhCn { get; set; }

    [JsonPropertyName("generic_name")]
    public string? GenericName { get; set; }

    [JsonPropertyName("cmd")]
    public string Cmd { get; set; } = "";

    [JsonPropertyName("args")]
    public List<string> Args { get; set; } = new();

    [JsonPropertyName("wm_class")]
    public string WmClass { get; set; } = "";

    [JsonPropertyName("icon")]
    public string? Icon { get; set; }

    [JsonPropertyName("icon_path")]
    public string? IconPath { get; set; }

    [JsonPropertyName("categories")]
    public string? Categories { get; set; }

    public string DisplayName => !string.IsNullOrWhiteSpace(NameZhCn) ? NameZhCn! : Name;
}

internal static class ShortcutManager
{
    private const uint SHCNE_ASSOCCHANGED = 0x08000000;
    private const uint SHCNF_IDLIST = 0x0000;

    [DllImport("shell32.dll")]
    private static extern void SHChangeNotify(uint wEventId, uint uFlags, IntPtr dwItem1, IntPtr dwItem2);

    public static string AppInstallDir =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "FocusStage");

    public static string IconsDir => Path.Combine(AppInstallDir, "icons");

    public static string StartMenuFocusStageDir =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.StartMenu), "Programs", "FocusStage");

    public static string StartMenuProgramsDir =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.StartMenu), "Programs");

    public static string DesktopDir =>
        Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory);

    public static string StartupDir =>
        Environment.GetFolderPath(Environment.SpecialFolder.Startup);

    public static string GetFocusStageExePath()
    {
        var installed = Path.Combine(AppInstallDir, "FocusStage.exe");
        if (File.Exists(installed)) return installed;
        return Environment.ProcessPath ?? Application.ExecutablePath;
    }

    public static string EnsureAppBrandIcon()
    {
        Directory.CreateDirectory(IconsDir);
        var icoPath = Path.Combine(IconsDir, "focusstage.ico");
        using var bmp = RenderBrandBitmap(64);
        SaveBitmapAsIco(bmp, icoPath);
        return icoPath;
    }

    public static Bitmap RenderBrandBitmap(int size)
    {
        var bmp = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using var g = Graphics.FromImage(bmp);
        g.SmoothingMode = SmoothingMode.AntiAlias;
        g.Clear(Color.Transparent);

        var pad = Math.Max(2, size / 16);
        using var bgBrush = new LinearGradientBrush(
            new Rectangle(0, 0, size, size),
            Color.FromArgb(26, 36, 58),
            Color.FromArgb(16, 22, 36),
            45f);
        g.FillEllipse(bgBrush, pad, pad, size - pad * 2, size - pad * 2);

        using var ringPen = new Pen(Color.FromArgb(88, 178, 255), Math.Max(2f, size / 12f));
        var rPad = Math.Max(4, size / 8);
        g.DrawEllipse(ringPen, rPad, rPad, size - rPad * 2, size - rPad * 2);

        using var dotBrush = new SolidBrush(Color.FromArgb(115, 232, 176));
        var dPad = size / 3;
        g.FillEllipse(dotBrush, dPad, dPad, size - dPad * 2, size - dPad * 2);
        return bmp;
    }

    public static Bitmap? TryLoadLinuxAppBitmap(string distro, LinuxAppDto app, int size = 48)
    {
        try
        {
            if (!string.IsNullOrWhiteSpace(app.IconPath))
            {
                var unc = ToWslUncPath(distro, app.IconPath!);
                if (File.Exists(unc))
                {
                    var ext = Path.GetExtension(unc).ToLowerInvariant();
                    if (ext is ".png" or ".bmp" or ".jpg" or ".jpeg")
                    {
                        using var src = new Bitmap(unc);
                        var resized = new Bitmap(size, size, PixelFormat.Format32bppArgb);
                        using var g = Graphics.FromImage(resized);
                        g.InterpolationMode = InterpolationMode.HighQualityBicubic;
                        g.SmoothingMode = SmoothingMode.AntiAlias;
                        g.DrawImage(src, 0, 0, size, size);
                        return resized;
                    }
                    if (ext == ".ico")
                    {
                        using var ico = new Icon(unc, size, size);
                        return ico.ToBitmap();
                    }
                }
            }
        }
        catch
        {
            // Fall through to generated badge icon.
        }
        return RenderAppFallbackBitmap(app.DisplayName, size);
    }

    public static Bitmap RenderAppFallbackBitmap(string name, int size = 48)
    {
        var bmp = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using var g = Graphics.FromImage(bmp);
        g.SmoothingMode = SmoothingMode.AntiAlias;
        g.Clear(Color.Transparent);

        var hash = Math.Abs(name.GetHashCode());
        var palette = new[]
        {
            Color.FromArgb(52, 120, 246),
            Color.FromArgb(16, 163, 127),
            Color.FromArgb(124, 92, 235),
            Color.FromArgb(219, 107, 45),
            Color.FromArgb(46, 149, 200),
        };
        var baseColor = palette[hash % palette.Length];
        using var bg = new SolidBrush(baseColor);
        using var path = RoundedRect(new Rectangle(2, 2, size - 4, size - 4), size / 4);
        g.FillPath(bg, path);

        var letter = string.IsNullOrWhiteSpace(name) ? "L" : name.Trim()[0].ToString().ToUpperInvariant();
        using var font = new Font("Segoe UI", size * 0.42f, FontStyle.Bold, GraphicsUnit.Pixel);
        using var sf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
        using var textBrush = new SolidBrush(Color.White);
        g.DrawString(letter, font, textBrush, new RectangleF(0, 0, size, size), sf);
        return bmp;
    }

    private static GraphicsPath RoundedRect(Rectangle bounds, int radius)
    {
        int d = radius * 2;
        var path = new GraphicsPath();
        path.AddArc(bounds.X, bounds.Y, d, d, 180, 90);
        path.AddArc(bounds.Right - d, bounds.Y, d, d, 270, 90);
        path.AddArc(bounds.Right - d, bounds.Bottom - d, d, d, 0, 90);
        path.AddArc(bounds.X, bounds.Bottom - d, d, d, 90, 90);
        path.CloseFigure();
        return path;
    }

    public static string ToWslUncPath(string distro, string linuxPath)
    {
        var trimmed = linuxPath.TrimStart('/');
        return $@"\\wsl.localhost\{distro}\{trimmed.Replace('/', '\\')}";
    }

    internal static string AppIconPath(string appId) =>
        Path.Combine(IconsDir, $"{SanitizeFileName(appId)}.ico");

    public static string EnsureAppIco(string distro, LinuxAppDto app)
    {
        Directory.CreateDirectory(IconsDir);
        var icoPath = AppIconPath(string.IsNullOrWhiteSpace(app.Id) ? app.Stem : app.Id);
        using var bmp = TryLoadLinuxAppBitmap(distro, app, 48) ?? RenderAppFallbackBitmap(app.DisplayName, 48);
        SaveBitmapAsIco(bmp, icoPath);
        return icoPath;
    }

    public static void SaveBitmapAsIco(Bitmap bmp, string icoPath)
    {
        using var output = File.Create(icoPath);
        WriteBitmapAsIco(bmp, output);
    }

    internal static Icon IconFromPng(string image)
    {
        using var input = new MemoryStream(Convert.FromBase64String(image));
        using var bitmap = new Bitmap(input);
        using var ico = new MemoryStream();
        WriteBitmapAsIco(bitmap, ico);
        ico.Position = 0;
        return new Icon(ico);
    }

    private static void WriteBitmapAsIco(Bitmap bmp, Stream output)
    {
        using var pngStream = new MemoryStream();
        bmp.Save(pngStream, ImageFormat.Png);
        var pngBytes = pngStream.ToArray();

        using var bw = new BinaryWriter(output, System.Text.Encoding.UTF8, leaveOpen: true);
        // ICONDIR (6 bytes)
        bw.Write((ushort)0);
        bw.Write((ushort)1); // 1 = ICO
        bw.Write((ushort)1); // 1 image
        // ICONDIRENTRY (16 bytes)
        bw.Write((byte)(bmp.Width >= 256 ? 0 : bmp.Width));
        bw.Write((byte)(bmp.Height >= 256 ? 0 : bmp.Height));
        bw.Write((byte)0); // color count
        bw.Write((byte)0); // reserved
        bw.Write((ushort)1); // planes
        bw.Write((ushort)32); // bpp
        bw.Write((uint)pngBytes.Length);
        bw.Write((uint)22); // offset
        bw.Write(pngBytes);
    }

    public static async Task<(bool Ok, string Message, int Count)> SyncAllAsync(string distro, bool createDesktopForLauncher = true, bool createStartupTray = true)
    {
        // 1. Sync the Linux desktop entries used for app identity and icon lookup.
        var (syncCode, syncOut, syncErr) = await RunWslAsync(distro, true, "/usr/local/bin/focusstage", "--json", "sync-apps");
        if (syncCode != 0)
        {
            // Fallback to user dir if root isn't available
            await RunWslAsync(distro, false, "focusstage", "--json", "sync-apps");
        }

        // 2. Query discovered apps with resolved icon paths
        var (appsOk, apps, appsErr) = await FetchLinuxAppsAsync(distro);
        if (!appsOk)
        {
            return (false, string.IsNullOrWhiteSpace(appsErr) ? syncErr : appsErr, 0);
        }

        var brandIco = EnsureAppBrandIcon();
        var exePath = GetFocusStageExePath();

        // 3. Create FocusStage Launcher shortcuts (Start Menu top-level, Start Menu\FocusStage, Desktop, optional Startup)
        Directory.CreateDirectory(StartMenuFocusStageDir);
        CreateShortcut(
            Path.Combine(StartMenuProgramsDir, "FocusStage.lnk"),
            exePath,
            $"--distro \"{distro}\"",
            brandIco,
            "FocusStage Linux App Launcher & AI Stage Manager");
        CreateShortcut(
            Path.Combine(StartMenuFocusStageDir, "FocusStage Launcher.lnk"),
            exePath,
            $"--distro \"{distro}\"",
            brandIco,
            "FocusStage Linux App Launcher & AI Stage Manager");

        if (createDesktopForLauncher)
        {
            CreateShortcut(
                Path.Combine(DesktopDir, "FocusStage.lnk"),
                exePath,
                $"--distro \"{distro}\"",
                brandIco,
                "FocusStage Linux App Launcher");
        }

        if (createStartupTray)
        {
            CreateShortcut(
                Path.Combine(StartupDir, "FocusStage.lnk"),
                exePath,
                $"--tray --distro \"{distro}\"",
                brandIco,
                "FocusStage Background Tray & Clipboard Sync");
        }

        // 4. Refresh per-app Windows Start Menu shortcuts under Start Menu\Programs\FocusStage\
        var keepFiles = new HashSet<string>(StringComparer.OrdinalIgnoreCase) { "FocusStage Launcher.lnk" };
        var wslgExe = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "WSL", "wslg.exe");
        if (!File.Exists(wslgExe))
        {
            wslgExe = "wslg.exe";
        }

        int count = 0;
        foreach (var app in apps)
        {
            if (string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }
            var lnkName = $"{SanitizeFileName(app.DisplayName)} (FocusStage).lnk";
            keepFiles.Add(lnkName);
            var icoPath = EnsureAppIco(distro, app);
            var args = BuildWslgLaunchArgs(distro, app);
            CreateShortcut(
                Path.Combine(StartMenuFocusStageDir, lnkName),
                wslgExe,
                args,
                icoPath,
                $"Launch {app.DisplayName} via FocusStage ({distro})");
            count++;
        }

        foreach (var file in Directory.GetFiles(StartMenuFocusStageDir, "*.lnk"))
        {
            var fn = Path.GetFileName(file);
            if (!keepFiles.Contains(fn))
            {
                try { File.Delete(file); } catch { }
            }
        }

        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, IntPtr.Zero, IntPtr.Zero);
        return (true, $"Synced {count} Linux apps to Windows Start Menu (Programs\\FocusStage).", count);
    }

    public static string CreateStartMenuAppShortcut(string distro, LinuxAppDto app, bool alsoTopLevel = true)
    {
        Directory.CreateDirectory(StartMenuFocusStageDir);
        var icoPath = EnsureAppIco(distro, app);
        var wslgExe = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "WSL", "wslg.exe");
        if (!File.Exists(wslgExe)) wslgExe = "wslg.exe";

        var fileName = $"{SanitizeFileName(app.DisplayName)} (FocusStage).lnk";
        var lnkPath = Path.Combine(StartMenuFocusStageDir, fileName);
        var args = BuildWslgLaunchArgs(distro, app);
        CreateShortcut(
            lnkPath,
            wslgExe,
            args,
            icoPath,
            $"Launch {app.DisplayName} via FocusStage ({distro})");

        if (alsoTopLevel)
        {
            var topPath = Path.Combine(StartMenuProgramsDir, fileName);
            CreateShortcut(
                topPath,
                wslgExe,
                args,
                icoPath,
                $"Launch {app.DisplayName} via FocusStage ({distro})");
        }

        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, IntPtr.Zero, IntPtr.Zero);
        return lnkPath;
    }

    public static async Task<(bool Ok, string Message)> InstallDebPackageAsync(string distro, string debLinuxPath)
    {
        var escaped = debLinuxPath.Replace("'", "'\"'\"'");
        var script = $"DEBIAN_FRONTEND=noninteractive apt-get install -y '{escaped}' || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y '{escaped}')";
        var (code, stdout, stderr) = await RunWslAsync(distro, asRoot: true, 180, "bash", "-lc", script);
        if (code == 0)
        {
            return (true, "安装包安装完成，已自动刷新应用列表。");
        }
        var err = string.IsNullOrWhiteSpace(stderr) ? stdout : stderr;
        return (false, string.IsNullOrWhiteSpace(err) ? $"apt-get exited {code}" : err);
    }

    public static async Task<string?> FindInstalledAppCenterCommandAsync(string distro)
    {
        var (code, stdout, _) = await RunWslAsync(
            distro,
            false,
            "bash",
            "-lc",
            "for c in snap-store gnome-software ubuntu-software plasma-discover; do if command -v \"$c\" >/dev/null 2>&1 || [ -x \"/snap/bin/$c\" ]; then echo \"$c\"; break; fi; done");
        if (code == 0 && !string.IsNullOrWhiteSpace(stdout))
        {
            return stdout.Split('\n', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault()?.Trim();
        }
        return null;
    }

    public static async Task<(bool Ok, string Command, string Message)> InstallAppCenterAsync(string distro)
    {
        var script = "if command -v snap >/dev/null 2>&1; then snap install snap-store --channel=2/stable && echo snap-store; else apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y gnome-software && echo gnome-software; fi";
        var (code, stdout, stderr) = await RunWslAsync(distro, asRoot: true, 240, "bash", "-lc", script);
        if (code == 0)
        {
            var cmd = stdout.Split('\n', StringSplitOptions.RemoveEmptyEntries).LastOrDefault()?.Trim() ?? "snap-store";
            return (true, cmd, $"Ubuntu 应用中心 ({cmd}) 安装完成。");
        }
        var err = string.IsNullOrWhiteSpace(stderr) ? stdout : stderr;
        return (false, "", string.IsNullOrWhiteSpace(err) ? $"安装失败 (exit {code})" : err);
    }

    public static string CreateDesktopAppShortcut(string distro, LinuxAppDto app)
    {
        var icoPath = EnsureAppIco(distro, app);
        var wslgExe = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "WSL", "wslg.exe");
        if (!File.Exists(wslgExe)) wslgExe = "wslg.exe";

        var lnkPath = Path.Combine(DesktopDir, $"{SanitizeFileName(app.DisplayName)} (FocusStage).lnk");
        CreateShortcut(
            lnkPath,
            wslgExe,
            BuildWslgLaunchArgs(distro, app),
            icoPath,
            $"Launch {app.DisplayName} via FocusStage ({distro})");
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, IntPtr.Zero, IntPtr.Zero);
        return lnkPath;
    }

    public static bool IsStartupEnabled() =>
        File.Exists(Path.Combine(StartupDir, "FocusStage.lnk"));

    public static void SetStartupEnabled(string distro, bool enabled)
    {
        var lnkPath = Path.Combine(StartupDir, "FocusStage.lnk");
        if (enabled)
        {
            var brandIco = EnsureAppBrandIcon();
            CreateShortcut(
                lnkPath,
                GetFocusStageExePath(),
                $"--tray --distro \"{distro}\"",
                brandIco,
                "FocusStage Background Tray & Clipboard Sync");
        }
        else if (File.Exists(lnkPath))
        {
            File.Delete(lnkPath);
        }
    }

    private static string BuildWslgLaunchArgs(string distro, LinuxAppDto app)
    {
        var sb = new StringBuilder();
        sb.Append($"-d {QuoteArg(distro)} --cd \"~\" -- /usr/local/bin/focusstage-launch --wm-class {QuoteArg(app.WmClass)} {QuoteArg(app.Cmd)}");
        if (app.Args.Count > 0)
        {
            sb.Append(" --");
            foreach (var a in app.Args)
            {
                sb.Append(' ');
                sb.Append(QuoteArg(a));
            }
        }
        return sb.ToString();
    }

    private static string QuoteArg(string s)
    {
        if (!string.IsNullOrEmpty(s) && !s.Any(c => char.IsWhiteSpace(c) || c == '"' || c == '\\'))
        {
            return s;
        }
        return "\"" + s.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"";
    }

    private static void CreateShortcut(string shortcutPath, string targetPath, string arguments, string iconPath, string description)
    {
        var shellType = Type.GetTypeFromProgID("WScript.Shell")
            ?? throw new InvalidOperationException("WScript.Shell COM object unavailable.");
        dynamic shell = Activator.CreateInstance(shellType)!;
        dynamic sc = shell.CreateShortcut(shortcutPath);
        sc.TargetPath = targetPath;
        sc.Arguments = arguments;
        sc.Description = description;
        if (!string.IsNullOrWhiteSpace(iconPath) && File.Exists(iconPath))
        {
            sc.IconLocation = $"{iconPath},0";
        }
        var workDir = Path.GetDirectoryName(targetPath);
        if (!string.IsNullOrWhiteSpace(workDir) && Directory.Exists(workDir))
        {
            sc.WorkingDirectory = workDir;
        }
        sc.Save();
    }

    public static async Task<(bool Ok, List<LinuxAppDto> Apps, string Error)> FetchLinuxAppsAsync(string distro)
    {
        var (code, stdout, stderr) = await RunWslAsync(distro, false, "focusstage", "--json", "list-apps");
        if (code != 0 || string.IsNullOrWhiteSpace(stdout))
        {
            return (false, new(), string.IsNullOrWhiteSpace(stderr) ? $"focusstage list-apps exited {code}" : stderr);
        }
        try
        {
            using var doc = JsonDocument.Parse(stdout);
            if (doc.RootElement.TryGetProperty("apps", out var arr))
            {
                var apps = JsonSerializer.Deserialize<List<LinuxAppDto>>(arr.GetRawText()) ?? new();
                return (true, apps, "");
            }
            return (false, new(), "Unexpected response from focusstage list-apps");
        }
        catch (Exception ex)
        {
            return (false, new(), ex.Message);
        }
    }

    public static Task<(int ExitCode, string Stdout, string Stderr)> RunWslAsync(string distro, bool asRoot, params string[] args)
        => RunWslAsync(distro, asRoot, 15, args);

    public static async Task<(int ExitCode, string Stdout, string Stderr)> RunWslAsync(string distro, bool asRoot, int timeoutSeconds, params string[] args)
    {
        try
        {
            var psi = new ProcessStartInfo("wsl.exe")
            {
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true,
                StandardOutputEncoding = Encoding.UTF8,
                StandardErrorEncoding = Encoding.UTF8
            };
            psi.ArgumentList.Add("-d");
            psi.ArgumentList.Add(distro);
            if (asRoot)
            {
                psi.ArgumentList.Add("-u");
                psi.ArgumentList.Add("root");
            }
            psi.ArgumentList.Add("-e");
            foreach (var a in args)
            {
                psi.ArgumentList.Add(a);
            }

            using var proc = Process.Start(psi);
            if (proc is null) return (-1, "", "Failed to start wsl.exe");

            var outTask = proc.StandardOutput.ReadToEndAsync();
            var errTask = proc.StandardError.ReadToEndAsync();
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(timeoutSeconds));
            await proc.WaitForExitAsync(cts.Token);
            return (proc.ExitCode, (await outTask).Trim(), (await errTask).Trim());
        }
        catch (Exception ex)
        {
            return (-1, "", ex.Message);
        }
    }

    private static string SanitizeFileName(string name)
    {
        var invalid = Path.GetInvalidFileNameChars();
        var sb = new StringBuilder(name.Length);
        foreach (var ch in name)
        {
            sb.Append(invalid.Contains(ch) ? '_' : ch);
        }
        return sb.ToString().Trim();
    }
}
