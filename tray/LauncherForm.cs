using System.Diagnostics;
using System.Drawing;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using Microsoft.Web.WebView2.Core;
using Microsoft.Web.WebView2.WinForms;

namespace FocusStageTray;

internal sealed class LauncherForm : Form
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull
    };

    private static readonly JsonSerializerOptions RpcReadOptions = new()
    {
        PropertyNameCaseInsensitive = true
    };

    private readonly FocusStageTrayContext _ctx;
    private readonly string _distro;
    private readonly LauncherAssetServer _assetServer;
    private readonly WebView2 _webView;

    private List<LinuxAppDto> _apps = new();
    private Dictionary<string, LinuxAppDto> _appById = new(StringComparer.Ordinal);
    private LauncherLayout _layout = LauncherLayout.LoadExistingReadOnly();
    private List<StageDto> _stages = new();
    private bool _daemonAlive;

    private bool _webViewReady;
    private bool _appsLoaded;
    private bool _loadingApps;
    private string _lastAppsSignature = "";
    private string _lastStateSignature = "";

    private FileSystemWatcher? _downloadsWatcher;
    private readonly System.Windows.Forms.Timer _debounceTimer;
    private readonly System.Windows.Forms.Timer _autoDiscoverTimer;

    public LauncherForm(FocusStageTrayContext ctx, string distro)
    {
        _ctx = ctx;
        _distro = distro;
        _assetServer = new LauncherAssetServer(distro);

        Text = $"FocusStage ({distro})";
        FormBorderStyle = FormBorderStyle.None;
        Width = 1040;
        Height = 700;
        MinimumSize = new Size(760, 520);
        StartPosition = FormStartPosition.CenterScreen;
        BackColor = Color.FromArgb(238, 242, 247);
        AutoScaleMode = AutoScaleMode.Dpi;

        using (var bmp = ShortcutManager.RenderBrandBitmap(64))
        {
            Icon = Icon.FromHandle(bmp.GetHicon());
        }

        _webView = new WebView2
        {
            Dock = DockStyle.Fill,
            DefaultBackgroundColor = Color.FromArgb(255, 238, 242, 247)
        };
        Controls.Add(_webView);

        // Hide to tray instead of exiting when user closes the window.
        FormClosing += (_, e) =>
        {
            if (e.CloseReason == CloseReason.UserClosing)
            {
                e.Cancel = true;
                Hide();
            }
        };

        SizeChanged += (_, _) =>
        {
            if (_webViewReady)
            {
                PushEvent("windowState", new { maximized = WindowState == FormWindowState.Maximized });
            }
        };

        _debounceTimer = new System.Windows.Forms.Timer { Interval = 800 };
        _debounceTimer.Tick += async (_, _) =>
        {
            _debounceTimer.Stop();
            await RefreshAppsAsync(forceRender: false);
        };

        _autoDiscoverTimer = new System.Windows.Forms.Timer { Interval = 5000 };
        _autoDiscoverTimer.Tick += async (_, _) =>
        {
            if (Visible && !_loadingApps)
            {
                await RefreshAppsAsync(forceRender: false);
            }
        };

        SetupDownloadsWatcher();

        Load += async (_, _) =>
        {
            _daemonAlive = _ctx.DaemonAlive;
            _stages = _ctx.CurrentStages;
            _layout = LauncherLayout.LoadExistingReadOnly();
            await InitializeWebViewAsync();
            _ = RefreshAppsAsync(forceRender: true);
            _autoDiscoverTimer.Start();
        };

        Activated += async (_, _) =>
        {
            if (IsHandleCreated && !_loadingApps)
            {
                await RefreshAppsAsync(forceRender: false);
            }
        };
    }

    protected override CreateParams CreateParams
    {
        get
        {
            var cp = base.CreateParams;
            cp.Style |= Win11WindowHelper.WS_THICKFRAME
                      | Win11WindowHelper.WS_MINIMIZEBOX
                      | Win11WindowHelper.WS_MAXIMIZEBOX
                      | Win11WindowHelper.WS_CAPTION
                      | Win11WindowHelper.WS_SYSMENU;
            return cp;
        }
    }

    protected override void OnHandleCreated(EventArgs e)
    {
        base.OnHandleCreated(e);
        Win11WindowHelper.ApplyWin11Chrome(Handle);
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == Win11WindowHelper.WM_NCCALCSIZE && Win11WindowHelper.HandleNcCalcSize(Handle, ref m))
        {
            return;
        }
        base.WndProc(ref m);
    }

    private async Task InitializeWebViewAsync()
    {
        try
        {
            var userDataDir = Path.Combine(ShortcutManager.AppInstallDir, "WebView2Data");
            Directory.CreateDirectory(userDataDir);

            var env = await CoreWebView2Environment.CreateAsync(null, userDataDir);
            await _webView.EnsureCoreWebView2Async(env);

            var core = _webView.CoreWebView2;
            core.Settings.AreDefaultContextMenusEnabled = false;
            core.Settings.AreBrowserAcceleratorKeysEnabled = false;
            core.Settings.IsStatusBarEnabled = false;
            core.Settings.IsZoomControlEnabled = false;
            core.Settings.IsPinchZoomEnabled = false;
#if DEBUG
            core.Settings.AreDevToolsEnabled = true;
#else
            core.Settings.AreDevToolsEnabled = false;
#endif

            core.AddWebResourceRequestedFilter($"{LauncherAssetServer.VirtualOrigin}/*", CoreWebView2WebResourceContext.All);
            core.WebResourceRequested += OnWebResourceRequested;

            core.NavigationStarting += (_, e) =>
            {
                if (!e.Uri.StartsWith(LauncherAssetServer.VirtualOrigin, StringComparison.OrdinalIgnoreCase)
                    && !string.Equals(e.Uri, "about:blank", StringComparison.OrdinalIgnoreCase))
                {
                    e.Cancel = true;
                }
            };

            core.NewWindowRequested += (_, e) => e.Handled = true;
            core.WebMessageReceived += OnWebMessageReceived;

            core.NavigationCompleted += (_, e) =>
            {
                if (!e.IsSuccess) return;
                _webViewReady = true;
                PushEvent("windowState", new { maximized = WindowState == FormWindowState.Maximized });
                PushState(force: true);
                if (_appsLoaded)
                {
                    PushApps("init");
                }
                _ = _ctx.RefreshAndRebuildMenuAsync();
            };

            core.Navigate($"{LauncherAssetServer.VirtualOrigin}/index.html");
        }
        catch (Exception ex)
        {
            ShowWebViewFallback(ex.Message);
        }
    }

    private void OnWebResourceRequested(object? sender, CoreWebView2WebResourceRequestedEventArgs e)
    {
        if (_webView.CoreWebView2 is null) return;
        if (_assetServer.TryResolve(e.Request.Uri, out var bytes, out var contentType, out var cacheControl))
        {
            var stream = new MemoryStream(bytes, writable: false);
            var headers = $"Content-Type: {contentType}\r\nCache-Control: {cacheControl}";
            e.Response = _webView.CoreWebView2.Environment.CreateWebResourceResponse(stream, 200, "OK", headers);
        }
        else
        {
            var stream = new MemoryStream("Not Found"u8.ToArray(), writable: false);
            e.Response = _webView.CoreWebView2.Environment.CreateWebResourceResponse(stream, 404, "Not Found", "Content-Type: text/plain; charset=utf-8");
        }
    }

    private void OnWebMessageReceived(object? sender, CoreWebView2WebMessageReceivedEventArgs e)
    {
        _ = HandleWebMessageAsync(e.WebMessageAsJson);
    }

    private async Task HandleWebMessageAsync(string rawJson)
    {
        string? requestId = null;
        try
        {
            using var doc = JsonDocument.Parse(rawJson);
            var root = doc.RootElement;
            if (root.TryGetProperty("id", out var idEl))
            {
                requestId = idEl.ValueKind == JsonValueKind.String ? idEl.GetString() : idEl.GetRawText();
            }

            if (!root.TryGetProperty("method", out var methodEl) || methodEl.ValueKind != JsonValueKind.String)
            {
                if (requestId is not null) SendRpcError(requestId, "Missing method");
                return;
            }

            var method = methodEl.GetString()!;
            var hasParams = root.TryGetProperty("params", out var paramsEl) && paramsEl.ValueKind == JsonValueKind.Object;
            JsonElement p = hasParams ? paramsEl : default;

            object? result = await DispatchRpcAsync(method, hasParams, p);
            if (requestId is not null)
            {
                SendRpcSuccess(requestId, result);
            }
        }
        catch (Exception ex)
        {
            if (requestId is not null)
            {
                SendRpcError(requestId, ex.Message);
            }
        }
    }

    private async Task<object?> DispatchRpcAsync(string method, bool hasParams, JsonElement p)
    {
        string Str(string key, string fallback = "") =>
            hasParams && p.TryGetProperty(key, out var el) && el.ValueKind == JsonValueKind.String
                ? (el.GetString() ?? fallback)
                : fallback;

        bool Bool(string key, bool fallback = false) =>
            hasParams && p.TryGetProperty(key, out var el) && (el.ValueKind == JsonValueKind.True || el.ValueKind == JsonValueKind.False)
                ? el.GetBoolean()
                : fallback;

        List<string> StrList(string key)
        {
            var list = new List<string>();
            if (hasParams && p.TryGetProperty(key, out var el) && el.ValueKind == JsonValueKind.Array)
            {
                foreach (var item in el.EnumerateArray())
                {
                    if (item.ValueKind == JsonValueKind.String && item.GetString() is string s)
                    {
                        list.Add(s);
                    }
                }
            }
            return list;
        }

        int Int(string key, int fallback = 0) =>
            hasParams && p.TryGetProperty(key, out var el) && el.TryGetInt32(out var value)
                ? value
                : fallback;

        T? Obj<T>(string key) where T : class =>
            hasParams && p.TryGetProperty(key, out var el) && el.ValueKind == JsonValueKind.Object
                ? JsonSerializer.Deserialize<T>(el.GetRawText(), RpcReadOptions)
                : null;

        List<LauncherDockItem> DockList(string key)
        {
            if (!hasParams || !p.TryGetProperty(key, out var el) || el.ValueKind != JsonValueKind.Array)
            {
                return new List<LauncherDockItem>();
            }
            return JsonSerializer.Deserialize<List<LauncherDockItem>>(el.GetRawText(), RpcReadOptions)
                   ?? new List<LauncherDockItem>();
        }

        switch (method)
        {
            case "app.init":
                return new
                {
                    distro = _distro,
                    daemonAlive = _daemonAlive,
                    stages = BuildStagePayloads(_stages),
                    apps = BuildAppPayloads(_apps),
                    appsLoaded = _appsLoaded,
                    layout = BuildLayoutPayload(_layout),
                    startupEnabled = ShortcutManager.IsStartupEnabled(),
                    clipboardSyncEnabled = _ctx.ClipboardSyncEnabled,
                    hotkeyRegistered = _ctx.HotkeyRegistered,
                    maximized = WindowState == FormWindowState.Maximized
                };

            case "window.drag":
                Win11WindowHelper.BeginDragWindow(this);
                return null;

            case "window.minimize":
                WindowState = FormWindowState.Minimized;
                return null;

            case "window.toggleMaximize":
                WindowState = WindowState == FormWindowState.Maximized
                    ? FormWindowState.Normal
                    : FormWindowState.Maximized;
                return new { maximized = WindowState == FormWindowState.Maximized };

            case "window.hide":
                Hide();
                return null;

            case "apps.refresh":
                _assetServer.ClearIconCache();
                await RefreshAppsAsync(forceRender: true);
                return new
                {
                    apps = BuildAppPayloads(_apps),
                    appsLoaded = _appsLoaded,
                    layout = BuildLayoutPayload(_layout),
                    count = _apps.Count
                };

            case "stages.refresh":
                await _ctx.RefreshAndRebuildMenuAsync();
                PushState(force: true);
                return new
                {
                    daemonAlive = _daemonAlive,
                    stages = BuildStagePayloads(_stages)
                };

            case "app.activate":
            {
                var appId = Str("appId");
                if (!_appById.TryGetValue(appId, out var app))
                {
                    throw new InvalidOperationException("找不到指定的应用。");
                }
                if (string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase))
                {
                    var (ok, msg) = await ShortcutManager.InstallDebPackageAsync(_distro, app.Cmd);
                    if (ok)
                    {
                        await RefreshAppsAsync(forceRender: true);
                    }
                    return new { ok, message = msg, installedDeb = true };
                }

                var args = LaunchCommandLine.ForStage(app.Cmd, app.Args, app.WmClass);
                var (launchOk, launchMsg) = await _ctx.RunControlWithFeedbackAsync(args);
                return new { ok = launchOk, message = launchOk ? $"已启动 {app.DisplayName}" : launchMsg };
            }

            case "app.launchCustom":
            {
                var commandLine = Str("commandLine");
                var parsed = LaunchCommandLine.Parse(commandLine);
                var stageArgs = LaunchCommandLine.ForStage(parsed[0], parsed[1..]);
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync(stageArgs);
                return new { ok, message = ok ? $"已启动 {parsed[0]}" : msg };
            }

            case "app.openAppCenter":
            {
                var cmd = await ShortcutManager.FindInstalledAppCenterCommandAsync(_distro);
                if (string.IsNullOrWhiteSpace(cmd))
                {
                    return new { status = "needs_install", distro = _distro };
                }
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("launch", "--ensure-daemon", cmd);
                return new
                {
                    status = ok ? "launched" : "error",
                    command = cmd,
                    message = ok ? $"已打开应用中心 ({cmd})" : msg
                };
            }

            case "app.installAndOpenAppCenter":
            {
                var (instOk, instCmd, instMsg) = await ShortcutManager.InstallAppCenterAsync(_distro);
                if (!instOk)
                {
                    return new { ok = false, message = instMsg };
                }
                await RefreshAppsAsync(forceRender: true);
                var (launchOk, launchMsg) = await _ctx.RunControlWithFeedbackAsync("launch", "--ensure-daemon", instCmd);
                return new
                {
                    ok = launchOk,
                    command = instCmd,
                    message = launchOk ? $"已安装并打开应用中心 ({instCmd})" : launchMsg
                };
            }

            case "app.uninstall":
            {
                var app = RequireApp(Str("appId"));
                var (ok, message) = await ShortcutManager.UninstallAppAsync(_distro, app.Id);
                if (!ok)
                {
                    return new { ok = false, message };
                }

                _layout.RemoveUninstalledApp(app.Id);
                _assetServer.ClearIconCache();
                await RefreshAppsAsync(forceRender: true);
                var (syncOk, syncMessage, _) = await ShortcutManager.SyncAllAsync(
                    _distro,
                    createDesktopForLauncher: true,
                    createStartupTray: ShortcutManager.IsStartupEnabled());

                var finalMessage = string.IsNullOrWhiteSpace(message) ? $"已卸载 {app.DisplayName}" : message;
                if (!syncOk && !string.IsNullOrWhiteSpace(syncMessage))
                {
                    finalMessage = $"{finalMessage}（开始菜单同步失败：{syncMessage}）";
                }

                return new
                {
                    ok = true,
                    message = finalMessage,
                    apps = BuildAppPayloads(_apps),
                    layout = BuildLayoutPayload(_layout),
                    count = _apps.Count,
                    shortcutsSynced = syncOk,
                    syncMessage
                };
            }

            case "shortcut.addToStartMenu":
            {
                var app = RequireApp(Str("appId"));
                ShortcutManager.CreateStartMenuAppShortcut(_distro, app, alsoTopLevel: true);
                return new { ok = true, message = $"已将 “{app.DisplayName}” 添加到开始菜单" };
            }

            case "shortcut.addToDesktop":
            {
                var app = RequireApp(Str("appId"));
                var path = ShortcutManager.CreateDesktopAppShortcut(_distro, app);
                return new { ok = true, message = $"已创建桌面快捷方式：{Path.GetFileName(path)}" };
            }

            case "shortcut.addFolderToStartMenu":
            {
                var folder = _layout.FindFolder(Str("folderId"))
                    ?? throw new InvalidOperationException("找不到指定的文件夹。");
                int added = 0;
                foreach (var aid in folder.AppIds)
                {
                    if (_appById.TryGetValue(aid, out var app)
                        && !string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase))
                    {
                        ShortcutManager.CreateStartMenuAppShortcut(_distro, app, alsoTopLevel: true);
                        added++;
                    }
                }
                return new { ok = true, added, message = $"已将文件夹 “{folder.Name}” 内 {added} 个应用添加到开始菜单" };
            }

            case "shortcut.syncAll":
            {
                var (ok, msg, count) = await ShortcutManager.SyncAllAsync(
                    _distro,
                    createDesktopForLauncher: true,
                    createStartupTray: ShortcutManager.IsStartupEnabled());
                await RefreshAppsAsync(forceRender: true);
                return new { ok, count, message = msg };
            }

            case "layout.createFolder":
            {
                var name = Str("name", "新建文件夹");
                var initialAppId = Str("initialAppId");
                var folder = _layout.CreateFolder(name, string.IsNullOrWhiteSpace(initialAppId) ? null : initialAppId);
                return new { folder = BuildFolderPayload(folder), layout = BuildLayoutPayload(_layout) };
            }

            case "layout.renameFolder":
            {
                _layout.RenameFolder(Str("folderId"), Str("name"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.dissolveFolder":
            {
                _layout.DissolveFolder(Str("folderId"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.moveAppToFolder":
            {
                _layout.MoveAppToFolder(Str("appId"), Str("folderId"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.moveAppToRoot":
            {
                _layout.MoveAppToRoot(Str("appId"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.mergeAppsIntoFolder":
            {
                var name = Str("name", "新建文件夹");
                var folder = _layout.MergeAppsIntoNewFolder(Str("sourceAppId"), Str("targetAppId"), name);
                return new { folder = BuildFolderPayload(folder), layout = BuildLayoutPayload(_layout) };
            }

            case "layout.reorderRoot":
            {
                _layout.ReorderRoot(StrList("rootOrder"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.reorderFolder":
            {
                _layout.ReorderFolder(Str("folderId"), StrList("appIds"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.setRootPosition":
            {
                _layout.SetRootPosition(Str("token"), Int("row"), Int("col"), Bool("repel", false));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.moveAppToRootAt":
            {
                _layout.MoveAppToRootAt(Str("appId"), Int("row"), Int("col"), Bool("repel", false));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.setDockItems":
            {
                _layout.SetDockItems(DockList("items"), _appById);
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "appearance.set":
            {
                _layout.SetAppearance(Obj<LauncherAppearance>("appearance"));
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "layout.autoOrganize":
            {
                _layout.AutoOrganize(_apps);
                return new { layout = BuildLayoutPayload(_layout) };
            }

            case "daemon.start":
                await _ctx.StartDaemonAsync();
                PushState(force: true);
                return new { ok = _daemonAlive, daemonAlive = _daemonAlive };

            case "daemon.stop":
            {
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("shutdown");
                PushState(force: true);
                return new { ok, message = ok ? "focusstaged 已停止" : msg, daemonAlive = _daemonAlive };
            }

            case "stage.takeoverAll":
                await _ctx.TakeoverAllAsync();
                PushState(force: true);
                return new { ok = true };

            case "stage.takeover":
            {
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("takeover", Str("stageId"));
                PushState(force: true);
                return new { ok, message = ok ? "已接管舞台" : msg };
            }

            case "stage.show":
            {
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("show", Str("stageId"));
                PushState(force: true);
                return new { ok, message = ok ? "已恢复到桌面" : msg };
            }

            case "stage.hide":
            {
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("hide", Str("stageId"));
                PushState(force: true);
                return new { ok, message = ok ? "已收入后台" : msg };
            }

            case "stage.freeze":
            {
                var stageId = Str("stageId");
                var freeze = Bool("freeze", true);
                var (ok, msg) = freeze
                    ? await _ctx.RunControlWithFeedbackAsync("freeze", stageId)
                    : await _ctx.RunControlWithFeedbackAsync("freeze", "--unfreeze", stageId);
                PushState(force: true);
                return new { ok, message = ok ? (freeze ? "已冻结舞台输入" : "已恢复舞台输入") : msg };
            }

            case "stage.close":
            {
                var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("close", Str("stageId"));
                PushState(force: true);
                return new { ok, message = ok ? "已关闭舞台" : msg };
            }

            case "settings.setStartup":
            {
                var enabled = Bool("enabled");
                ShortcutManager.SetStartupEnabled(_distro, enabled);
                PushState(force: true);
                return new { ok = true, enabled };
            }

            case "settings.setClipboardSync":
            {
                var enabled = Bool("enabled");
                _ctx.ClipboardSyncEnabled = enabled;
                PushState(force: true);
                return new { ok = true, enabled };
            }

            case "linuxSettings.get":
                return BuildLinuxSettingsPayload(await _ctx.LinuxSettingsAsync("get"));

            case "linuxSettings.setBrowser":
                return BuildLinuxSettingsPayload(await _ctx.LinuxSettingsAsync("set-browser", Str("id")));

            case "linuxSettings.setTerminal":
                return BuildLinuxSettingsPayload(await _ctx.LinuxSettingsAsync("set-terminal", Str("id")));

            case "linuxSettings.syncHandlers":
                return BuildLinuxSettingsPayload(await _ctx.LinuxSettingsAsync("sync-handlers"));

            default:
                throw new InvalidOperationException($"Unknown RPC method: {method}");
        }
    }

    private LinuxAppDto RequireApp(string appId)
    {
        if (!_appById.TryGetValue(appId, out var app))
        {
            throw new InvalidOperationException("找不到指定的应用。");
        }
        return app;
    }

    public void UpdateDaemonAndStages(bool daemonAlive, List<StageDto> stages)
    {
        if (InvokeRequired)
        {
            BeginInvoke(() => UpdateDaemonAndStages(daemonAlive, stages));
            return;
        }

        _daemonAlive = daemonAlive;
        _stages = stages;
        PushState(force: false);
    }

    public async Task RefreshAppsAsync(bool forceRender = true)
    {
        if (_loadingApps) return;
        _loadingApps = true;
        try
        {
            var (ok, apps, err) = await ShortcutManager.FetchLinuxAppsAsync(_distro);
            if (!ok)
            {
                if (forceRender && _webViewReady)
                {
                    PushEvent("toast", new { message = $"读取 Linux 应用失败: {err}", tone = "error" });
                }
                return;
            }

            var sig = string.Join("|", apps.Select(a => $"{a.Id}:{a.DisplayName}:{a.Source}:{a.IconPath}"));
            bool listChanged = sig != _lastAppsSignature;
            _lastAppsSignature = sig;
            bool wasLoaded = _appsLoaded;
            _appsLoaded = true;

            _apps = apps;
            _appById = apps.ToDictionary(a => a.Id, StringComparer.Ordinal);
            _assetServer.UpdateApps(_appById);
            if (_apps.Count > 0)
            {
                _layout = LauncherLayout.LoadOrDefault(_apps);
            }

            if ((!wasLoaded || forceRender || listChanged) && _webViewReady)
            {
                PushApps(listChanged && !forceRender && wasLoaded ? "auto" : "refresh");
            }
        }
        finally
        {
            _loadingApps = false;
        }
    }

    private void PushState(bool force)
    {
        if (!_webViewReady) return;
        bool startup = ShortcutManager.IsStartupEnabled();
        bool clip = _ctx.ClipboardSyncEnabled;
        bool hotkey = _ctx.HotkeyRegistered;
        var sig = BuildStateSignature(_daemonAlive, startup, clip, hotkey, _stages);
        if (!force && sig == _lastStateSignature) return;
        _lastStateSignature = sig;

        PushEvent("state", new
        {
            distro = _distro,
            daemonAlive = _daemonAlive,
            startupEnabled = startup,
            clipboardSyncEnabled = clip,
            hotkeyRegistered = hotkey,
            stages = BuildStagePayloads(_stages)
        });
    }

    private void PushApps(string reason)
    {
        if (!_webViewReady) return;
        PushEvent("apps", new
        {
            apps = BuildAppPayloads(_apps),
            appsLoaded = _appsLoaded,
            layout = BuildLayoutPayload(_layout),
            reason
        });
    }

    private static string BuildStateSignature(bool daemonAlive, bool startup, bool clip, bool hotkey, List<StageDto> stages)
    {
        var head = $"{daemonAlive}:{startup}:{clip}:{hotkey}:{stages.Count}";
        if (stages.Count == 0) return head;
        var body = string.Join("|", stages.Select(s =>
            $"{s.StageId}:{s.App}:{s.Title}:{s.Display}:{s.Kind}:{s.Pid}:{s.InputOwner.Type}:{s.InputOwner.AgentName}:{s.InputOwner.LeaseId}:{s.HasSurface}:{(s.Size is { Length: >= 2 } ? $"{s.Size[0]}x{s.Size[1]}" : "")}:{s.Scale:F2}"));
        return head + "|" + body;
    }

    private static object[] BuildAppPayloads(List<LinuxAppDto> apps) =>
        apps.Select(a => (object)new
        {
            id = a.Id,
            stem = a.Stem,
            source = a.Source,
            filePath = a.FilePath,
            name = a.Name,
            nameZhCn = a.NameZhCn,
            displayName = a.DisplayName,
            genericName = a.GenericName,
            cmd = a.Cmd,
            args = a.Args,
            wmClass = a.WmClass,
            categories = a.Categories,
            iconUrl = $"/icons/{Uri.EscapeDataString(a.Id)}"
        }).ToArray();

    private static object BuildFolderPayload(LauncherFolder f) => new
    {
        id = f.Id,
        name = f.Name,
        appIds = f.AppIds
    };

    private static object BuildLayoutPayload(LauncherLayout layout) => new
    {
        folders = layout.Folders.Select(BuildFolderPayload).ToArray(),
        rootOrder = layout.RootOrder,
        positions = layout.Positions.ToDictionary(
            pair => pair.Key,
            pair => new { row = pair.Value.Row, col = pair.Value.Col },
            StringComparer.Ordinal),
        dockItems = layout.DockItems.Select(d => new
        {
            id = d.Id,
            kind = d.Kind,
            target = d.Target,
            label = d.Label
        }).ToArray(),
        appearance = layout.Appearance
    };

    private static object[] BuildStagePayloads(List<StageDto> stages) =>
        stages.Select(s => (object)new
        {
            stageId = s.StageId,
            app = s.App,
            title = string.IsNullOrWhiteSpace(s.Title) ? s.App : s.Title,
            kind = s.Kind,
            pid = s.Pid,
            display = s.Display,
            inputOwner = new
            {
                type = s.InputOwner.Type,
                leaseId = s.InputOwner.LeaseId,
                agentName = s.InputOwner.AgentName
            },
            size = s.Size,
            frameSeq = s.FrameSeq,
            hasSurface = s.HasSurface,
            scale = s.Scale,
            wmClass = s.WmClass
        }).ToArray();

    private static object BuildLinuxSettingsPayload(LinuxDesktopSettings s) => new
    {
        browser = s.Browser,
        browsers = s.Browsers.Select(b => new { id = b.Id, name = b.Name }).ToArray(),
        terminal = s.Terminal,
        terminals = s.Terminals.Select(t => new { id = t.Id, name = t.Name }).ToArray(),
        protocols = s.Protocols,
        xdgOpenInstalled = s.XdgOpenInstalled,
        portalActive = s.PortalActive,
        mimeappsSynced = s.MimeappsSynced
    };

    private void SendRpcSuccess(string id, object? result)
    {
        PostJson(new { type = "response", id, ok = true, result });
    }

    private void SendRpcError(string id, string error)
    {
        PostJson(new { type = "response", id, ok = false, error });
    }

    private void PushEvent(string eventName, object payload)
    {
        PostJson(new { type = "event", @event = eventName, payload });
    }

    private void PostJson(object payload)
    {
        if (IsDisposed || _webView.IsDisposed || _webView.CoreWebView2 is null) return;
        var json = JsonSerializer.Serialize(payload, JsonOptions);
        if (InvokeRequired)
        {
            BeginInvoke(() =>
            {
                if (!IsDisposed && !_webView.IsDisposed && _webView.CoreWebView2 is not null)
                {
                    _webView.CoreWebView2.PostWebMessageAsJson(json);
                }
            });
        }
        else
        {
            _webView.CoreWebView2.PostWebMessageAsJson(json);
        }
    }

    private void SetupDownloadsWatcher()
    {
        try
        {
            var downloads = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                "Downloads");
            if (!Directory.Exists(downloads)) return;
            _downloadsWatcher = new FileSystemWatcher(downloads)
            {
                IncludeSubdirectories = false,
                EnableRaisingEvents = true
            };
            FileSystemEventHandler onChanged = (_, e) =>
            {
                var ext = Path.GetExtension(e.Name ?? "").ToLowerInvariant();
                if (ext is ".deb" or ".appimage" or ".desktop")
                {
                    BeginInvoke(() =>
                    {
                        _debounceTimer.Stop();
                        _debounceTimer.Start();
                    });
                }
            };
            _downloadsWatcher.Created += onChanged;
            _downloadsWatcher.Renamed += (_, e) => onChanged(this, e);
            _downloadsWatcher.Deleted += onChanged;
        }
        catch
        {
            // Ignore watcher errors.
        }
    }

    private void ShowWebViewFallback(string detail)
    {
        Controls.Remove(_webView);
        FormBorderStyle = FormBorderStyle.Sizable;
        var panel = new Panel { Dock = DockStyle.Fill, Padding = new Padding(36), BackColor = Color.FromArgb(248, 250, 252) };
        var title = new Label
        {
            Text = "无法初始化 WebView2 启动器界面",
            Font = new Font("Microsoft YaHei UI", 14f, FontStyle.Bold),
            ForeColor = Color.FromArgb(15, 23, 42),
            AutoSize = true,
            Location = new Point(36, 36)
        };
        var body = new Label
        {
            Text = $"FocusStage 启动器需要 Microsoft Edge WebView2 Runtime。\n\n错误详情：{detail}\n\n你仍然可以通过系统托盘图标右键菜单启动和管理 Stage。",
            Font = new Font("Microsoft YaHei UI", 9.5f),
            ForeColor = Color.FromArgb(71, 85, 105),
            MaximumSize = new Size(640, 0),
            AutoSize = true,
            Location = new Point(36, 78)
        };
        var btnDownload = new Button
        {
            Text = "下载 WebView2 Runtime",
            AutoSize = true,
            Padding = new Padding(12, 6, 12, 6),
            Location = new Point(36, 190)
        };
        btnDownload.Click += (_, _) =>
        {
            Process.Start(new ProcessStartInfo("https://developer.microsoft.com/microsoft-edge/webview2/") { UseShellExecute = true });
        };
        panel.Controls.AddRange([title, body, btnDownload]);
        Controls.Add(panel);
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing)
        {
            _autoDiscoverTimer.Stop();
            _autoDiscoverTimer.Dispose();
            _debounceTimer.Stop();
            _debounceTimer.Dispose();
            _downloadsWatcher?.Dispose();
            _webView.Dispose();
        }
        base.Dispose(disposing);
    }
}
