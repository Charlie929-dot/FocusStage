using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        ApplicationConfiguration.Initialize();
        var distro = args.Length > 0 ? args[0] : "Ubuntu";
        using var ctx = new FocusStageTrayContext(distro);
        Application.Run(ctx);
    }
}

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

    private bool _daemonAlive;
    private bool _clipboardSyncEnabled = true;
    private string _lastWinClipboard = "";
    private string _lastWslClipboard = "";
    private List<StageDto> _stages = new();
    private bool _busy;

    public FocusStageTrayContext(string distro)
    {
        _distro = distro;
        _trayIcon = CreateTrayIcon();
        _menu = new ContextMenuStrip();
        _menu.Opening += (_, _) => _ = RefreshAndRebuildMenuAsync();

        _notifyIcon = new NotifyIcon
        {
            Icon = _trayIcon,
            Text = "FocusStage (Ctrl+Alt+Space: Takeover)",
            Visible = true,
            ContextMenuStrip = _menu
        };
        _notifyIcon.DoubleClick += (_, _) => _ = TakeoverAllAsync();

        _hotkeyWindow = new HotkeyWindow();
        _hotkeyWindow.TakeoverHotkeyPressed += () => _ = TakeoverAllAsync();

        if (!_hotkeyWindow.Registered)
        {
            _notifyIcon.ShowBalloonTip(
                3000,
                "FocusStage",
                "Could not register Ctrl+Alt+Space hotkey; use tray menu or double-click icon to take over.",
                ToolTipIcon.Warning);
        }

        _syncTimer = new System.Windows.Forms.Timer { Interval = 1200 };
        _syncTimer.Tick += async (_, _) => await TickBackgroundSyncAsync();
        _syncTimer.Start();

        BuildMenu();
        _ = RefreshAndRebuildMenuAsync();
    }

    private static Icon CreateTrayIcon()
    {
        using var bmp = new Bitmap(32, 32);
        using var g = Graphics.FromImage(bmp);
        g.SmoothingMode = SmoothingMode.AntiAlias;
        g.Clear(Color.Transparent);

        using var bgBrush = new SolidBrush(Color.FromArgb(24, 30, 44));
        g.FillEllipse(bgBrush, 1, 1, 30, 30);

        using var ringPen = new Pen(Color.FromArgb(90, 178, 255), 2.5f);
        g.DrawEllipse(ringPen, 3, 3, 26, 26);

        using var dotBrush = new SolidBrush(Color.FromArgb(120, 230, 175));
        g.FillEllipse(dotBrush, 10, 10, 12, 12);

        return Icon.FromHandle(bmp.GetHicon());
    }

    private void BuildMenu()
    {
        _menu.Items.Clear();

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

        var launchMenu = new ToolStripMenuItem("Launch Stage");
        launchMenu.DropDownItems.Add("foot (Wayland Terminal)", null, (_, _) => _ = RunControlAndRefreshAsync("launch", "foot"));
        launchMenu.DropDownItems.Add("xterm (X11 Terminal)", null, (_, _) => _ = RunControlAndRefreshAsync("launch", "--kind", "x11", "xterm"));
        launchMenu.DropDownItems.Add("chromium (Wayland Browser)", null, (_, _) => _ = RunControlAndRefreshAsync("launch", "--kind", "chromium", "chromium"));
        launchMenu.DropDownItems.Add("Custom command...", null, (_, _) => PromptLaunchCustom());
        _menu.Items.Add(launchMenu);

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

    private void PromptLaunchCustom()
    {
        using var form = new Form
        {
            Text = "Launch FocusStage App",
            Width = 420,
            Height = 155,
            FormBorderStyle = FormBorderStyle.FixedDialog,
            StartPosition = FormStartPosition.CenterScreen,
            MaximizeBox = false,
            MinimizeBox = false
        };
        var label = new Label { Left = 12, Top = 12, Width = 380, Text = "Linux command (e.g. code --no-sandbox, gedit, xeyes):" };
        var box = new TextBox { Left = 12, Top = 36, Width = 380 };
        var ok = new Button { Text = "Launch", Left = 230, Top = 72, Width = 75, DialogResult = DialogResult.OK };
        var cancel = new Button { Text = "Cancel", Left = 317, Top = 72, Width = 75, DialogResult = DialogResult.Cancel };
        form.Controls.AddRange(new Control[] { label, box, ok, cancel });
        form.AcceptButton = ok;
        form.CancelButton = cancel;

        if (form.ShowDialog() == DialogResult.OK && !string.IsNullOrWhiteSpace(box.Text))
        {
            var parts = box.Text.Trim().Split(' ', StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length > 0)
            {
                var args = new List<string> { "launch", parts[0] };
                if (parts.Length > 1)
                {
                    args.Add("--");
                    args.AddRange(parts.Skip(1));
                }
                _ = RunControlAndRefreshAsync(args.ToArray());
            }
        }
    }

    private async Task TakeoverAllAsync()
    {
        await RunControlAndRefreshAsync("takeover");
        _notifyIcon.ShowBalloonTip(1500, "FocusStage", "Human takeover activated.", ToolTipIcon.Info);
    }

    private async Task RunControlAndRefreshAsync(params string[] focusstageArgs)
    {
        await RunFocusStageAsync(focusstageArgs);
        await RefreshAndRebuildMenuAsync();
    }

    private async Task StartDaemonAsync()
    {
        try
        {
            var psi = new ProcessStartInfo("wsl.exe")
            {
                UseShellExecute = false,
                CreateNoWindow = true
            };
            psi.ArgumentList.Add("-d");
            psi.ArgumentList.Add(_distro);
            psi.ArgumentList.Add("-e");
            psi.ArgumentList.Add("bash");
            psi.ArgumentList.Add("-lc");
            psi.ArgumentList.Add("nohup focusstaged >/tmp/focusstaged.log 2>&1 </dev/null &");
            Process.Start(psi);
            await Task.Delay(900);
        }
        catch
        {
            // Ignore startup error; refresh will reflect daemon status.
        }
        await RefreshAndRebuildMenuAsync();
    }

    private async Task RefreshAndRebuildMenuAsync()
    {
        await RefreshStateAsync();
        BuildMenu();
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
            if (_daemonAlive && _clipboardSyncEnabled)
            {
                // 1. Windows -> Human-owned WSL stages
                if (Clipboard.ContainsText())
                {
                    var currentWin = Clipboard.GetText();
                    if (!string.IsNullOrEmpty(currentWin) && currentWin != _lastWinClipboard)
                    {
                        _lastWinClipboard = currentWin;
                        await RunFocusStageAsync("--json", "host-clipboard-sync", currentWin);
                    }
                }

                // 2. Human-owned WSL stages -> Windows
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
            if (proc is null) return (-1, "");

            var outTask = proc.StandardOutput.ReadToEndAsync();
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(4));
            await proc.WaitForExitAsync(cts.Token);
            var stdout = await outTask;
            return (proc.ExitCode, stdout.Trim());
        }
        catch
        {
            return (-1, "");
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing)
        {
            _syncTimer.Stop();
            _syncTimer.Dispose();
            _notifyIcon.Visible = false;
            _notifyIcon.Dispose();
            _hotkeyWindow.Dispose();
            _trayIcon.Dispose();
            _menu.Dispose();
        }
        base.Dispose(disposing);
    }
}
