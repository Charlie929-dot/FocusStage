using System.Diagnostics;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal static class Program
{
    private const string SingleInstanceMutexName = @"Local\FocusStage_SingleInstance_Mutex";
    private const string ShowLauncherEventName = @"Local\FocusStage_ShowLauncher_Event";

    [STAThread]
    private static int Main(string[] args)
    {
        ApplicationConfiguration.Initialize();
        if (args.Length == 1 && args[0] == "--window-host")
        {
            return NativeHost.Run();
        }

        string distro = "Ubuntu";
        bool trayOnly = false;
        bool syncShortcuts = false;

        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--tray":
                    trayOnly = true;
                    break;
                case "--sync-shortcuts":
                    syncShortcuts = true;
                    break;
                case "--distro" when i + 1 < args.Length:
                    distro = args[++i];
                    break;
                default:
                    if (!args[i].StartsWith('-'))
                    {
                        distro = args[i];
                    }
                    break;
            }
        }

        if (syncShortcuts)
        {
            var (ok, msg, _) = ShortcutManager
                .SyncAllAsync(distro, createDesktopForLauncher: true, createStartupTray: true)
                .GetAwaiter()
                .GetResult();
            Console.WriteLine(msg);
            return ok ? 0 : 1;
        }

        using var mutex = new Mutex(true, SingleInstanceMutexName, out bool createdNew);
        if (!createdNew)
        {
            if (!trayOnly)
            {
                try
                {
                    using var ev = EventWaitHandle.OpenExisting(ShowLauncherEventName);
                    ev.Set();
                }
                catch
                {
                    // Ignore if existing instance hasn't created event yet.
                }
            }
            return 0;
        }

        using var showEvent = new EventWaitHandle(false, EventResetMode.AutoReset, ShowLauncherEventName);
        using var ctx = new FocusStageTrayContext(distro, startMinimized: trayOnly, showEvent);
        Application.Run(ctx);
        return 0;
    }
}

internal sealed record LinuxAppChoice(string Id, string Name);

internal sealed record LinuxDesktopSettings(string Browser, List<LinuxAppChoice> Browsers,
    string? Terminal, List<LinuxAppChoice> Terminals, Dictionary<string, string> Protocols,
    bool XdgOpenInstalled, bool PortalActive, bool MimeappsSynced);

internal sealed class StageDto
{
    [JsonPropertyName("stage_id")]
    public string StageId { get; set; } = "";

    [JsonPropertyName("app")]
    public string App { get; set; } = "";

    [JsonPropertyName("title")]
    public string Title { get; set; } = "";

    [JsonPropertyName("kind")]
    public string Kind { get; set; } = "";

    [JsonPropertyName("pid")]
    public int? Pid { get; set; }

    [JsonPropertyName("display")]
    public string Display { get; set; } = "hidden";

    [JsonPropertyName("input_owner")]
    public InputOwnerDto InputOwner { get; set; } = new();

    [JsonPropertyName("frame_seq")]
    public ulong FrameSeq { get; set; }
}

internal sealed class InputOwnerDto
{
    [JsonPropertyName("type")]
    public string Type { get; set; } = "human";

    [JsonPropertyName("lease_id")]
    public string? LeaseId { get; set; }

    [JsonPropertyName("agent_name")]
    public string? AgentName { get; set; }
}

internal sealed class HotkeyWindow : NativeWindow, IDisposable
{
    private const int WmHotkey = 0x0312;
    private const uint ModAlt = 0x0001;
    private const uint ModControl = 0x0002;
    private const uint ModNorepeat = 0x4000;
    private const int HotkeyIdTakeover = 1;

    public event Action? TakeoverHotkeyPressed;
    public bool Registered { get; }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool UnregisterHotKey(IntPtr hWnd, int id);

    public HotkeyWindow()
    {
        CreateHandle(new CreateParams());
        Registered = RegisterHotKey(Handle, HotkeyIdTakeover, ModControl | ModAlt | ModNorepeat, (uint)Keys.Space);
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == WmHotkey && m.WParam.ToInt32() == HotkeyIdTakeover)
        {
            TakeoverHotkeyPressed?.Invoke();
        }
        base.WndProc(ref m);
    }

    public void Dispose()
    {
        if (Registered)
        {
            UnregisterHotKey(Handle, HotkeyIdTakeover);
        }
        DestroyHandle();
    }
}

internal sealed class FocusStageTrayContext : ApplicationContext
{
    private readonly string _distro;
    private readonly NotifyIcon _notifyIcon;
    private readonly ContextMenuStrip _menu;
    private readonly HotkeyWindow _hotkeyWindow;
    private readonly System.Windows.Forms.Timer _syncTimer;
    private readonly Icon _trayIcon;
    private readonly CancellationTokenSource _signalCts = new();
    private LauncherForm? _launcherForm;

    private bool _daemonAlive;
    private bool _clipboardSyncEnabled = true;
    private string _lastWinClipboard = "";
    private string _lastWslClipboard = "";
    private List<StageDto> _stages = new();
    private bool _busy;
    private bool _disposed;

    public bool DaemonAlive => _daemonAlive;
    public List<StageDto> CurrentStages => _stages;

    public FocusStageTrayContext(string distro, bool startMinimized, EventWaitHandle showEvent)
    {
        _distro = distro;
        _trayIcon = CreateTrayIcon();
        _menu = new ContextMenuStrip();
        _menu.Opening += (_, _) => _ = RefreshAndRebuildMenuAsync();

        _notifyIcon = new NotifyIcon
        {
            Icon = _trayIcon,
            Text = "FocusStage (点击打开启动器 | Ctrl+Alt+Space 接管)",
            Visible = true,
            ContextMenuStrip = _menu
        };
        _notifyIcon.MouseClick += (_, e) =>
        {
            if (e.Button == MouseButtons.Left)
            {
                ShowLauncher();
            }
        };
        _notifyIcon.DoubleClick += (_, _) => ShowLauncher();

        _hotkeyWindow = new HotkeyWindow();
        _hotkeyWindow.TakeoverHotkeyPressed += () => _ = TakeoverAllAsync();

        if (!_hotkeyWindow.Registered)
        {
            _notifyIcon.ShowBalloonTip(
                3000,
                "FocusStage",
                "无法注册 Ctrl+Alt+Space 全局热键，可通过托盘或启动器界面接管。",
                ToolTipIcon.Warning);
        }

        _syncTimer = new System.Windows.Forms.Timer { Interval = 1200 };
        _syncTimer.Tick += async (_, _) => await TickBackgroundSyncAsync();
        _syncTimer.Start();

        BuildMenu();
        _ = RefreshAndRebuildMenuAsync();
        StartShowEventListener(showEvent);

        if (!startMinimized)
        {
            ShowLauncher();
        }
    }

    private void StartShowEventListener(EventWaitHandle showEvent)
    {
        var syncCtx = SynchronizationContext.Current;
        Task.Run(() =>
        {
            var handles = new[] { showEvent, _signalCts.Token.WaitHandle };
            while (!_signalCts.IsCancellationRequested)
            {
                int idx = WaitHandle.WaitAny(handles);
                if (idx == 0)
                {
                    if (syncCtx is not null)
                    {
                        syncCtx.Post(_ => ShowLauncher(), null);
                    }
                    else
                    {
                        ShowLauncher();
                    }
                }
                else
                {
                    break;
                }
            }
        });
    }

    public void ShowLauncher()
    {
        if (_launcherForm is null || _launcherForm.IsDisposed)
        {
            _launcherForm = new LauncherForm(this, _distro);
        }
        if (!_launcherForm.Visible)
        {
            _launcherForm.Show();
        }
        if (_launcherForm.WindowState == FormWindowState.Minimized)
        {
            _launcherForm.WindowState = FormWindowState.Normal;
        }
        _launcherForm.Activate();
        _launcherForm.BringToFront();
        _launcherForm.UpdateDaemonAndStages(_daemonAlive, _stages);
    }

    private static Icon CreateTrayIcon()
    {
        using var bmp = ShortcutManager.RenderBrandBitmap(32);
        return Icon.FromHandle(bmp.GetHicon());
    }

    private void BuildMenu()
    {
        _menu.Items.Clear();

        var openLauncher = new ToolStripMenuItem("Open App Launcher (打开应用启动器)")
        {
            Font = new Font(_menu.Font, FontStyle.Bold)
        };
        openLauncher.Click += (_, _) => ShowLauncher();
        _menu.Items.Add(openLauncher);

        var statusItem = new ToolStripMenuItem(
            _daemonAlive
                ? $"focusstaged: Running ({_stages.Count} stages)"
                : "focusstaged: Stopped")
        {
            Enabled = false
        };
        _menu.Items.Add(statusItem);

        var takeoverAll = new ToolStripMenuItem("Takeover All (Ctrl+Alt+Space)");
        takeoverAll.Click += (_, _) => _ = TakeoverAllAsync();
        _menu.Items.Add(takeoverAll);

        _menu.Items.Add(new ToolStripSeparator());

        if (_stages.Count == 0)
        {
            _menu.Items.Add(new ToolStripMenuItem("(No active Stages)") { Enabled = false });
        }
        else
        {
            foreach (var stage in _stages)
            {
                var ownerTag = stage.InputOwner.Type switch
                {
                    "agent" => $"AI:{stage.InputOwner.AgentName ?? "agent"}",
                    "frozen" => "Frozen",
                    _ => "Human"
                };
                var dispTag = stage.Display == "desktop" ? "Desktop" : "Hidden";
                var title = string.IsNullOrWhiteSpace(stage.Title) ? stage.App : stage.Title;
                if (title.Length > 36) title = title[..36] + "...";

                var stageItem = new ToolStripMenuItem($"[{dispTag} | {ownerTag}] {title}");

                var takeItem = new ToolStripMenuItem("Takeover (Human + Show)");
                takeItem.Click += (_, _) => _ = RunControlAndRefreshAsync("takeover", stage.StageId);
                stageItem.DropDownItems.Add(takeItem);

                if (stage.Display == "desktop")
                {
                    var hideItem = new ToolStripMenuItem("Hide to Background");
                    hideItem.Click += (_, _) => _ = RunControlAndRefreshAsync("hide", stage.StageId);
                    stageItem.DropDownItems.Add(hideItem);
                }
                else
                {
                    var showItem = new ToolStripMenuItem("Show on Desktop");
                    showItem.Click += (_, _) => _ = RunControlAndRefreshAsync("show", stage.StageId);
                    stageItem.DropDownItems.Add(showItem);
                }

                if (stage.InputOwner.Type == "frozen")
                {
                    var unfreezeItem = new ToolStripMenuItem("Unfreeze Input");
                    unfreezeItem.Click += (_, _) => _ = RunControlAndRefreshAsync("freeze", "--unfreeze", stage.StageId);
                    stageItem.DropDownItems.Add(unfreezeItem);
                }
                else
                {
                    var freezeItem = new ToolStripMenuItem("Freeze Input");
                    freezeItem.Click += (_, _) => _ = RunControlAndRefreshAsync("freeze", stage.StageId);
                    stageItem.DropDownItems.Add(freezeItem);
                }

                stageItem.DropDownItems.Add(new ToolStripSeparator());
                var closeItem = new ToolStripMenuItem("Close Stage");
                closeItem.Click += (_, _) => _ = RunControlAndRefreshAsync("close", stage.StageId);
                stageItem.DropDownItems.Add(closeItem);

                _menu.Items.Add(stageItem);
            }
        }

        _menu.Items.Add(new ToolStripSeparator());

        var launchMenu = new ToolStripMenuItem("Quick Launch Stage");
        launchMenu.DropDownItems.Add("Chromium (Wayland Browser)", null, (_, _) => _ = RunControlAndRefreshAsync("launch", "--ensure-daemon", "--kind", "chromium", "--wm-class", "focusstage-chromium_chromium", "chromium"));
        launchMenu.DropDownItems.Add("Custom command...", null, (_, _) => PromptLaunchCustom());
        _menu.Items.Add(launchMenu);

        var syncAppsItem = new ToolStripMenuItem("Sync Linux Apps to Start Menu");
        syncAppsItem.Click += (_, _) => _ = SyncLinuxAppsAsync();
        _menu.Items.Add(syncAppsItem);

        var clipToggle = new ToolStripMenuItem("Sync Win11 Clipboard")
        {
            Checked = _clipboardSyncEnabled,
            CheckOnClick = true
        };
        clipToggle.CheckedChanged += (_, _) => _clipboardSyncEnabled = clipToggle.Checked;
        _menu.Items.Add(clipToggle);

        var daemonMenu = new ToolStripMenuItem("Daemon");
        daemonMenu.DropDownItems.Add("Start focusstaged", null, (_, _) => _ = StartDaemonAsync());
        daemonMenu.DropDownItems.Add("Stop focusstaged", null, (_, _) => _ = RunControlAndRefreshAsync("shutdown"));
        daemonMenu.DropDownItems.Add("Refresh", null, (_, _) => _ = RefreshAndRebuildMenuAsync());
        _menu.Items.Add(daemonMenu);

        _menu.Items.Add(new ToolStripSeparator());
        _menu.Items.Add("Exit", null, (_, _) => ExitThread());
    }

    public void PromptLaunchCustom(IWin32Window? owner = null)
    {
        using var form = new Form
        {
            Text = "Launch FocusStage App",
            AutoScaleMode = AutoScaleMode.Dpi,
            FormBorderStyle = FormBorderStyle.FixedDialog,
            StartPosition = owner is not null ? FormStartPosition.CenterParent : FormStartPosition.CenterScreen,
            MaximizeBox = false,
            MinimizeBox = false,
            Font = new Font("Microsoft YaHei UI", 9.2f)
        };

        var layout = new TableLayoutPanel
        {
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            ColumnCount = 1,
            RowCount = 3
        };
        layout.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100f));

        var label = new Label
        {
            AutoSize = true,
            Text = "Linux command (e.g. chromium, code --no-sandbox, nautilus):"
        };
        var box = new TextBox
        {
            Dock = DockStyle.Fill
        };

        var buttons = new FlowLayoutPanel
        {
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            FlowDirection = FlowDirection.RightToLeft,
            Dock = DockStyle.Fill,
            WrapContents = false,
            Margin = new Padding(0)
        };
        var cancel = new Button
        {
            Text = "Cancel",
            AutoSize = true,
            DialogResult = DialogResult.Cancel
        };
        var ok = new Button
        {
            Text = "Launch",
            AutoSize = true,
            DialogResult = DialogResult.OK
        };
        buttons.Controls.Add(cancel);
        buttons.Controls.Add(ok);

        layout.Controls.Add(label, 0, 0);
        layout.Controls.Add(box, 0, 1);
        layout.Controls.Add(buttons, 0, 2);
        form.Controls.Add(layout);
        form.AcceptButton = ok;
        form.CancelButton = cancel;

        void ApplyDpiLayout(int dpi)
        {
            int S(int px) => (int)Math.Round(px * (dpi / 96.0));
            int pad = S(16);
            label.MaximumSize = new Size(S(460), 0);
            label.Margin = new Padding(0, 0, 0, S(8));
            box.MinimumSize = new Size(S(440), 0);
            box.Margin = new Padding(0, 0, 0, S(12));
            cancel.MinimumSize = new Size(S(84), S(30));
            cancel.Padding = new Padding(S(10), S(3), S(10), S(3));
            cancel.Margin = new Padding(0);
            ok.MinimumSize = new Size(S(84), S(30));
            ok.Padding = new Padding(S(10), S(3), S(10), S(3));
            ok.Margin = new Padding(0, 0, S(8), 0);

            layout.Location = new Point(pad, pad);
            layout.PerformLayout();
            var pref = layout.GetPreferredSize(Size.Empty);
            form.ClientSize = new Size(pref.Width + pad * 2, pref.Height + pad * 2);
        }

        ApplyDpiLayout((owner as Control)?.DeviceDpi ?? form.DeviceDpi);
        form.Load += (_, _) => ApplyDpiLayout(form.DeviceDpi);

        var dialogResult = owner is not null ? form.ShowDialog(owner) : form.ShowDialog();
        if (dialogResult == DialogResult.OK && !string.IsNullOrWhiteSpace(box.Text))
        {
            var parts = box.Text.Trim().Split(' ', StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length > 0)
            {
                var args = new List<string> { "launch", "--ensure-daemon", parts[0] };
                if (parts.Length > 1)
                {
                    args.Add("--");
                    args.AddRange(parts.Skip(1));
                }
                _ = RunControlAndRefreshAsync(args.ToArray());
            }
        }
    }

    public async Task TakeoverAllAsync()
    {
        await RunControlAndRefreshAsync("takeover");
        _notifyIcon.ShowBalloonTip(1500, "FocusStage", "Human takeover activated.", ToolTipIcon.Info);
    }

    private async Task SyncLinuxAppsAsync()
    {
        var (ok, msg, _) = await ShortcutManager.SyncAllAsync(_distro, createDesktopForLauncher: true, createStartupTray: ShortcutManager.IsStartupEnabled());
        _notifyIcon.ShowBalloonTip(2500, "FocusStage", msg, ok ? ToolTipIcon.Info : ToolTipIcon.Warning);
        if (_launcherForm is not null && !_launcherForm.IsDisposed)
        {
            await _launcherForm.RefreshAppsAsync();
        }
    }

    internal async Task<LinuxDesktopSettings> LinuxSettingsAsync(params string[] settingsArgs)
    {
        var (code, stdout, stderr) = await RunFocusStageFullAsync(["--json", "settings", .. settingsArgs]);
        if (code != 0)
            throw new InvalidOperationException(string.IsNullOrWhiteSpace(stderr) ? "无法读取 Linux 设置" : stderr);
        return JsonSerializer.Deserialize<LinuxDesktopSettings>(stdout,
            new JsonSerializerOptions { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower })
            ?? throw new InvalidOperationException("Linux 设置响应为空");
    }

    public async Task<(bool Ok, string Message)> RunControlWithFeedbackAsync(params string[] focusstageArgs)
    {
        var fullArgs = new List<string> { "--json" };
        fullArgs.AddRange(focusstageArgs);
        var (code, stdout, stderr) = await RunFocusStageFullAsync(fullArgs.ToArray());
        await RefreshAndRebuildMenuAsync();

        if (code == 0)
        {
            return (true, stdout);
        }

        string errMsg = "";
        if (!string.IsNullOrWhiteSpace(stdout))
        {
            try
            {
                using var doc = JsonDocument.Parse(stdout);
                if (doc.RootElement.TryGetProperty("message", out var msgEl))
                {
                    errMsg = msgEl.GetString() ?? "";
                }
            }
            catch
            {
                errMsg = stdout;
            }
        }
        if (string.IsNullOrWhiteSpace(errMsg))
        {
            errMsg = string.IsNullOrWhiteSpace(stderr) ? $"Command failed (exit code {code})" : stderr;
        }
        return (false, errMsg);
    }

    public async Task RunControlAndRefreshAsync(params string[] focusstageArgs)
    {
        var (ok, msg) = await RunControlWithFeedbackAsync(focusstageArgs);
        if (!ok)
        {
            _notifyIcon.ShowBalloonTip(3500, "FocusStage Error", msg, ToolTipIcon.Error);
        }
    }

    public async Task StartDaemonAsync()
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
            psi.ArgumentList.Add(_distro);
            psi.ArgumentList.Add("-e");
            psi.ArgumentList.Add("bash");
            psi.ArgumentList.Add("-lc");
            psi.ArgumentList.Add("if ! focusstage --json ping >/dev/null 2>&1; then LOG=/run/user/$(id -u)/focusstaged.log; if command -v systemd-run >/dev/null 2>&1 && systemctl --user status >/dev/null 2>&1; then systemd-run --user --collect -E DISPLAY -E WAYLAND_DISPLAY -E PULSE_SERVER -E WSL_INTEROP -E XDG_CONFIG_HOME -E PATH -p \"StandardOutput=append:$LOG\" -p \"StandardError=append:$LOG\" focusstaged >/dev/null 2>&1; else setsid -f focusstaged >\"$LOG\" 2>&1 </dev/null; fi; fi; sleep 0.6; focusstage --json ping");
            using var proc = Process.Start(psi);
            if (proc is not null)
            {
                await proc.WaitForExitAsync();
            }
        }
        catch (Exception ex)
        {
            _notifyIcon.ShowBalloonTip(3000, "FocusStage", $"启动 focusstaged 失败: {ex.Message}", ToolTipIcon.Error);
        }
        await RefreshAndRebuildMenuAsync();
    }

    public async Task RefreshAndRebuildMenuAsync()
    {
        await RefreshStateAsync();
        BuildMenu();
        if (_launcherForm is not null && !_launcherForm.IsDisposed)
        {
            _launcherForm.UpdateDaemonAndStages(_daemonAlive, _stages);
        }
    }

    private async Task RefreshStateAsync()
    {
        var (code, stdout) = await RunFocusStageAsync("--json", "list");
        if (code == 0 && !string.IsNullOrWhiteSpace(stdout))
        {
            try
            {
                using var doc = JsonDocument.Parse(stdout);
                if (doc.RootElement.TryGetProperty("status", out var st) && st.GetString() == "stages" &&
                    doc.RootElement.TryGetProperty("stages", out var arr))
                {
                    _stages = JsonSerializer.Deserialize<List<StageDto>>(arr.GetRawText()) ?? new();
                    _daemonAlive = true;
                    return;
                }
            }
            catch
            {
                // Fall through to offline state.
            }
        }
        _daemonAlive = false;
        _stages = new();
    }

    private async Task TickBackgroundSyncAsync()
    {
        if (_busy) return;
        _busy = true;
        try
        {
            await RefreshStateAsync();
            if (_launcherForm is not null && !_launcherForm.IsDisposed && _launcherForm.Visible)
            {
                _launcherForm.UpdateDaemonAndStages(_daemonAlive, _stages);
            }
            if (_daemonAlive && _clipboardSyncEnabled)
            {
                if (Clipboard.ContainsText())
                {
                    var currentWin = Clipboard.GetText();
                    if (!string.IsNullOrEmpty(currentWin) && currentWin != _lastWinClipboard)
                    {
                        _lastWinClipboard = currentWin;
                        await RunFocusStageAsync("--json", "host-clipboard-sync", currentWin);
                    }
                }

                var (code, stdout) = await RunFocusStageAsync("--json", "host-clipboard-get");
                if (code == 0 && !string.IsNullOrWhiteSpace(stdout))
                {
                    using var doc = JsonDocument.Parse(stdout);
                    if (doc.RootElement.TryGetProperty("text", out var txtEl) && txtEl.ValueKind == JsonValueKind.String)
                    {
                        var text = txtEl.GetString() ?? "";
                        if (!string.IsNullOrEmpty(text) && text != _lastWslClipboard && text != _lastWinClipboard)
                        {
                            _lastWslClipboard = text;
                            _lastWinClipboard = text;
                            Clipboard.SetText(text);
                        }
                    }
                }
            }
        }
        catch
        {
            // Ignore transient clipboard or WSL errors during background polling.
        }
        finally
        {
            _busy = false;
        }
    }

    private async Task<(int ExitCode, string Stdout)> RunFocusStageAsync(params string[] args)
    {
        var (code, stdout, _) = await RunFocusStageFullAsync(args);
        return (code, stdout);
    }

    private async Task<(int ExitCode, string Stdout, string Stderr)> RunFocusStageFullAsync(params string[] args)
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
            psi.ArgumentList.Add(_distro);
            psi.ArgumentList.Add("-e");
            psi.ArgumentList.Add("focusstage");
            foreach (var a in args)
            {
                psi.ArgumentList.Add(a);
            }

            using var proc = Process.Start(psi);
            if (proc is null) return (-1, "", "Failed to start wsl.exe");

            var outTask = proc.StandardOutput.ReadToEndAsync();
            var errTask = proc.StandardError.ReadToEndAsync();
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(8));
            await proc.WaitForExitAsync(cts.Token);
            var stdout = await outTask;
            var stderr = await errTask;
            return (proc.ExitCode, stdout.Trim(), stderr.Trim());
        }
        catch (Exception ex)
        {
            return (-1, "", ex.Message);
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (!_disposed)
        {
            _disposed = true;
            if (disposing)
            {
                _signalCts.Cancel();
                _signalCts.Dispose();
                _syncTimer.Stop();
                _syncTimer.Dispose();
                _launcherForm?.Dispose();
                _notifyIcon.Visible = false;
                _notifyIcon.Dispose();
                _hotkeyWindow.Dispose();
                _trayIcon.Dispose();
                _menu.Dispose();
            }
        }
        base.Dispose(disposing);
    }
}
