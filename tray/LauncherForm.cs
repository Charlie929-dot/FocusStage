using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;

namespace FocusStageTray;

internal sealed class LauncherForm : Form
{
    private readonly FocusStageTrayContext _ctx;
    private readonly string _distro;

    // Top bar
    private readonly Panel _topBar;
    private readonly Label _titleLbl;
    private readonly Panel _statusDot;
    private readonly PillSearchBox _searchBox;
    private readonly WindowControlButton _btnMinimize;
    private readonly WindowControlButton _btnClose;
    private readonly ToolTip _toolTip = new();

    // Main homescreen canvas
    private readonly BufferedFlowPanel _homeGrid;

    // Floating dock & transient toast
    private readonly RoundedPanel _dockCard;
    private readonly DockItemButton _dockAppCenter;
    private readonly DockItemButton _dockNewFolder;
    private readonly DockItemButton _dockRunCmd;
    private readonly DockItemButton _dockSettings;
    private readonly ToastPill _toastPill;
    private readonly System.Windows.Forms.Timer _toastTimer;

    // Folder overlay modal
    private readonly DimOverlayPanel _folderOverlay;
    private readonly RoundedPanel _folderCard;
    private readonly Label _folderTitleLabel;
    private readonly BufferedFlowPanel _folderGrid;

    // Settings overlay modal (contains daemon controls, startup toggle, sync, and zero-flicker stages list)
    private readonly DimOverlayPanel _settingsOverlay;
    private readonly RoundedPanel _settingsCard;
    private readonly Panel _settingsScrollBody;
    private readonly ToggleSwitch _toggleStartup;
    private readonly PillButton _btnSyncStartMenu;
    private readonly Panel _settingsDaemonDot;
    private readonly Label _settingsDaemonLabel;
    private readonly PillButton _btnStartStopDaemon;
    private readonly PillButton _btnTakeoverAll;
    private readonly Label _stagesHeaderLabel;
    private readonly BufferedFlowPanel _stagesPanel;

    private Bitmap? _wallpaper;
    private List<LinuxAppDto> _apps = new();
    private Dictionary<string, LinuxAppDto> _appById = new(StringComparer.Ordinal);
    private LauncherLayout _layout = new();
    private List<StageDto> _stages = new();
    private bool _daemonAlive;
    private bool _daemonStateInitialized;
    private int _lastStageCount = -1;
    private string _lastStagesSignature = "";
    private bool _loadingApps;
    private bool _settingsVisible;
    private string? _openFolderId;
    private string? _selectedItemKey;
    private string _lastAppsSignature = "";

    private readonly Dictionary<string, Image> _iconCache = new(StringComparer.Ordinal);
    private FileSystemWatcher? _downloadsWatcher;
    private readonly System.Windows.Forms.Timer _debounceTimer;
    private readonly System.Windows.Forms.Timer _autoDiscoverTimer;

    public LauncherForm(FocusStageTrayContext ctx, string distro)
    {
        _ctx = ctx;
        _distro = distro;

        Text = $"FocusStage ({distro})";
        FormBorderStyle = FormBorderStyle.None;
        Width = 960;
        Height = 660;
        MinimumSize = new Size(680, 500);
        StartPosition = FormStartPosition.CenterScreen;
        BackColor = Color.FromArgb(234, 242, 252);
        Font = new Font("Microsoft YaHei UI", 9.0f, FontStyle.Regular);
        AutoScaleMode = AutoScaleMode.Dpi;
        DoubleBuffered = true;
        SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint | ControlStyles.ResizeRedraw, true);

        using (var bmp = ShortcutManager.RenderBrandBitmap(64))
        {
            Icon = Icon.FromHandle(bmp.GetHicon());
        }

        // 1. Minimal Top Bar (drag handle + subtle brand/status + centered search pill + window controls)
        _topBar = new TransparentPanel
        {
            Dock = DockStyle.Top,
            Height = 58,
            BackColor = Color.Transparent
        };
        _topBar.MouseDown += (_, e) =>
        {
            if (e.Button == MouseButtons.Left) Win11WindowHelper.BeginDragWindow(this);
        };

        var brandIcon = new PictureBox
        {
            Image = ShortcutManager.RenderBrandBitmap(24),
            Size = new Size(22, 22),
            SizeMode = PictureBoxSizeMode.Zoom,
            Location = new Point(20, 18),
            BackColor = Color.Transparent
        };
        brandIcon.MouseDown += (_, e) =>
        {
            if (e.Button == MouseButtons.Left) Win11WindowHelper.BeginDragWindow(this);
        };

        _titleLbl = new Label
        {
            Text = "FocusStage",
            ForeColor = Color.FromArgb(30, 41, 59),
            Font = new Font("Segoe UI", 10.8f, FontStyle.Bold),
            AutoSize = true,
            Location = new Point(48, 19),
            BackColor = Color.Transparent
        };
        _titleLbl.MouseDown += (_, e) =>
        {
            if (e.Button == MouseButtons.Left) Win11WindowHelper.BeginDragWindow(this);
        };

        _statusDot = new TransparentPanel
        {
            Size = new Size(9, 9),
            Location = new Point(150, 26),
            BackColor = Color.Transparent
        };
        _statusDot.Paint += (_, e) =>
        {
            e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
            var color = _daemonAlive ? Color.FromArgb(16, 185, 129) : Color.FromArgb(148, 163, 184);
            using var brush = new SolidBrush(color);
            e.Graphics.FillEllipse(brush, 0, 0, 8, 8);
        };
        _toolTip.SetToolTip(_statusDot, "focusstaged 状态检测中...");

        _searchBox = new PillSearchBox("搜索应用...");
        _searchBox.SearchTextChanged += (_, _) => RenderHomeGrid();

        _btnMinimize = new WindowControlButton(WindowControlKind.Minimize);
        _btnMinimize.Click += (_, _) => WindowState = FormWindowState.Minimized;
        _toolTip.SetToolTip(_btnMinimize, "最小化");

        _btnClose = new WindowControlButton(WindowControlKind.Close);
        _btnClose.Click += (_, _) =>
        {
            CloseAllOverlays();
            Hide();
        };
        _toolTip.SetToolTip(_btnClose, "隐藏到托盘");

        _topBar.Controls.AddRange(new Control[] { brandIcon, _titleLbl, _statusDot, _searchBox, _btnMinimize, _btnClose });

        // 2. Main Phone-Style App Grid
        _homeGrid = new BufferedFlowPanel
        {
            Dock = DockStyle.Fill,
            AutoScroll = true,
            BackColor = Color.Transparent,
            Padding = new Padding(32, 10, 32, 96),
            AllowDrop = true,
            WallpaperProvider = () => _wallpaper
        };
        _homeGrid.MouseUp += (_, e) =>
        {
            if (e.Button == MouseButtons.Right)
            {
                ShowEmptyAreaMenu(_homeGrid, e.Location);
            }
            else if (e.Button == MouseButtons.Left)
            {
                _selectedItemKey = null;
                RefreshSelectionVisuals(_homeGrid);
            }
        };
        _homeGrid.DragEnter += (_, e) =>
        {
            if (e.Data?.GetDataPresent(typeof(DragAppPayload)) == true)
            {
                e.Effect = DragDropEffects.Move;
            }
        };
        _homeGrid.DragDrop += (_, e) =>
        {
            if (e.Data?.GetData(typeof(DragAppPayload)) is DragAppPayload payload)
            {
                _layout.MoveAppToRoot(payload.AppId);
                RenderHomeGrid();
                if (_openFolderId is not null) RenderFolderModal();
            }
        };

        // 3. Floating Bottom Dock
        _dockCard = new RoundedPanel
        {
            Height = 70,
            Width = 420,
            CornerRadius = 22,
            FillColor = Color.FromArgb(236, 255, 255, 255),
            BorderColor = Color.FromArgb(190, 226, 232, 240),
            DrawShadow = true,
            Padding = new Padding(14, 7, 14, 7)
        };

        var dockFlow = new TransparentFlowPanel
        {
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            FlowDirection = FlowDirection.LeftToRight,
            WrapContents = false,
            BackColor = Color.Transparent,
            Padding = new Padding(0),
            Margin = new Padding(0)
        };

        _dockAppCenter = new DockItemButton("应用中心", DockIconKind.AppCenter, Color.FromArgb(234, 88, 12), Color.FromArgb(255, 237, 213));
        _dockAppCenter.Click += async (_, _) => await OpenOrInstallAppCenterAsync();

        _dockNewFolder = new DockItemButton("新建文件夹", DockIconKind.NewFolder, Color.FromArgb(37, 99, 235), Color.FromArgb(219, 234, 254));
        _dockNewFolder.Click += (_, _) => PromptCreateFolder();

        _dockRunCmd = new DockItemButton("运行命令", DockIconKind.RunCommand, Color.FromArgb(79, 70, 229), Color.FromArgb(224, 231, 255));
        _dockRunCmd.Click += (_, _) => _ctx.PromptLaunchCustom(this);

        _dockSettings = new DockItemButton("设置", DockIconKind.Settings, Color.FromArgb(71, 85, 105), Color.FromArgb(226, 232, 240));
        _dockSettings.Click += (_, _) => OpenSettingsOverlay();

        dockFlow.Controls.AddRange(new Control[] { _dockAppCenter, _dockNewFolder, _dockRunCmd, _dockSettings });
        _dockCard.Controls.Add(dockFlow);
        _dockCard.Resize += (_, _) =>
        {
            dockFlow.Location = new Point(
                Math.Max(0, (_dockCard.Width - dockFlow.Width) / 2),
                Math.Max(0, (_dockCard.Height - dockFlow.Height) / 2 - 1));
        };

        // 4. Transient Floating Toast Pill
        _toastPill = new ToastPill();
        _toastTimer = new System.Windows.Forms.Timer { Interval = 3200 };
        _toastTimer.Tick += (_, _) =>
        {
            _toastTimer.Stop();
            _toastPill.Visible = false;
        };

        // 5. Phone-Style Folder Overlay Modal (positioned over full ClientRectangle, not DockStyle.Fill)
        _folderOverlay = new DimOverlayPanel
        {
            Visible = false
        };
        _folderOverlay.BackdropClicked += (_, _) => CloseFolderOverlay();

        _folderCard = new RoundedPanel
        {
            Width = 540,
            Height = 400,
            CornerRadius = 24,
            FillColor = Color.FromArgb(250, 252, 255),
            BorderColor = Color.FromArgb(216, 226, 238),
            DrawShadow = true,
            Padding = new Padding(22, 18, 22, 18)
        };

        var folderHeader = new TransparentPanel
        {
            Dock = DockStyle.Top,
            Height = 42,
            BackColor = Color.Transparent
        };

        _folderTitleLabel = new Label
        {
            Dock = DockStyle.Fill,
            Font = new Font("Microsoft YaHei UI", 12.5f, FontStyle.Bold),
            ForeColor = Color.FromArgb(15, 23, 42),
            TextAlign = ContentAlignment.MiddleLeft,
            Cursor = Cursors.Hand
        };
        _folderTitleLabel.Click += (_, _) => PromptRenameCurrentFolder();

        var folderHeaderBtns = new TransparentFlowPanel
        {
            Dock = DockStyle.Right,
            AutoSize = true,
            FlowDirection = FlowDirection.LeftToRight,
            WrapContents = false,
            Padding = new Padding(0, 5, 0, 0),
            BackColor = Color.Transparent
        };

        var btnRenameFolder = new PillButton("重命名") { Height = 30, Margin = new Padding(0, 0, 6, 0) };
        btnRenameFolder.Click += (_, _) => PromptRenameCurrentFolder();

        var btnCloseFolder = new PillButton("✕") { Width = 34, Height = 30, Margin = new Padding(0) };
        btnCloseFolder.Click += (_, _) => CloseFolderOverlay();

        folderHeaderBtns.Controls.Add(btnRenameFolder);
        folderHeaderBtns.Controls.Add(btnCloseFolder);
        folderHeader.Controls.Add(_folderTitleLabel);
        folderHeader.Controls.Add(folderHeaderBtns);

        _folderGrid = new BufferedFlowPanel
        {
            Dock = DockStyle.Fill,
            AutoScroll = true,
            BackColor = Color.Transparent,
            Padding = new Padding(4, 10, 4, 6)
        };
        _folderGrid.MouseUp += (_, e) =>
        {
            if (e.Button == MouseButtons.Left)
            {
                _selectedItemKey = null;
                RefreshSelectionVisuals(_folderGrid);
            }
        };

        _folderCard.Controls.Add(_folderGrid);
        _folderCard.Controls.Add(folderHeader);
        _folderOverlay.Controls.Add(_folderCard);
        _folderOverlay.Resize += (_, _) => CenterFolderCard();

        // 6. Settings Overlay Modal (All settings + Running Stages consolidated here)
        _settingsOverlay = new DimOverlayPanel
        {
            Visible = false
        };
        _settingsOverlay.BackdropClicked += (_, _) => CloseSettingsOverlay();

        _settingsCard = new RoundedPanel
        {
            Width = 540,
            Height = 520,
            CornerRadius = 24,
            FillColor = Color.FromArgb(246, 249, 253),
            BorderColor = Color.FromArgb(214, 224, 236),
            DrawShadow = true,
            Padding = new Padding(22, 18, 22, 18)
        };

        var settingsHeader = new TransparentPanel
        {
            Dock = DockStyle.Top,
            Height = 42,
            BackColor = Color.Transparent
        };

        var settingsTitle = new Label
        {
            Text = "设置",
            Font = new Font("Microsoft YaHei UI", 13f, FontStyle.Bold),
            ForeColor = Color.FromArgb(15, 23, 42),
            AutoSize = true,
            Location = new Point(2, 7)
        };

        var distroBadge = new Label
        {
            Text = $"WSL · {_distro}",
            Font = new Font("Segoe UI", 8.2f, FontStyle.Regular),
            ForeColor = Color.FromArgb(100, 116, 139),
            AutoSize = true,
            Location = new Point(56, 13)
        };

        var btnCloseSettings = new PillButton("✕") { Width = 34, Height = 30 };
        btnCloseSettings.Anchor = AnchorStyles.Top | AnchorStyles.Right;
        btnCloseSettings.Click += (_, _) => CloseSettingsOverlay();
        settingsHeader.Resize += (_, _) =>
        {
            distroBadge.Location = new Point(settingsTitle.Right + 10, settingsTitle.Top + (settingsTitle.Height - distroBadge.Height) / 2 + 1);
            btnCloseSettings.Location = new Point(settingsHeader.Width - btnCloseSettings.Width, 5);
        };
        settingsHeader.Controls.AddRange(new Control[] { settingsTitle, distroBadge, btnCloseSettings });

        _settingsScrollBody = new TransparentPanel
        {
            Dock = DockStyle.Fill,
            AutoScroll = true,
            BackColor = Color.Transparent,
            Padding = new Padding(0, 6, 4, 4)
        };

        var settingsStack = new TransparentFlowPanel
        {
            FlowDirection = FlowDirection.TopDown,
            WrapContents = false,
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            BackColor = Color.Transparent,
            Location = new Point(0, 4)
        };

        // --- Section 1: General & Desktop ---
        var generalCard = CreateSettingsGroupCard("桌面与启动");
        var startupRow = new TransparentPanel { Height = 42, Dock = DockStyle.Top, BackColor = Color.Transparent };
        var startupTitle = new Label
        {
            Text = "开机自启",
            Font = new Font("Microsoft YaHei UI", 9.2f, FontStyle.Bold),
            ForeColor = Color.FromArgb(30, 41, 59),
            AutoSize = true,
            Location = new Point(0, 2)
        };
        var startupSub = new Label
        {
            Text = "登录 Windows 时自动在托盘运行 FocusStage",
            Font = new Font("Microsoft YaHei UI", 8.0f),
            ForeColor = Color.FromArgb(100, 116, 139),
            AutoSize = true,
            Location = new Point(0, 22)
        };
        _toggleStartup = new ToggleSwitch { Checked = ShortcutManager.IsStartupEnabled() };
        _toggleStartup.CheckedChanged += (_, _) =>
        {
            try
            {
                ShortcutManager.SetStartupEnabled(_distro, _toggleStartup.Checked);
                ShowToast(_toggleStartup.Checked ? "已开启开机自启" : "已关闭开机自启");
            }
            catch (Exception ex)
            {
                MessageBox.Show(this, $"设置开机自启失败: {ex.Message}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
        };
        startupRow.Resize += (_, _) => _toggleStartup.Location = new Point(startupRow.Width - _toggleStartup.Width - 2, 9);
        startupRow.Controls.AddRange(new Control[] { startupTitle, startupSub, _toggleStartup });

        var desktopActions = new TransparentFlowPanel
        {
            Dock = DockStyle.Top,
            Height = 42,
            Padding = new Padding(0, 8, 0, 0),
            WrapContents = false,
            BackColor = Color.Transparent
        };

        var btnRefreshApps = new PillButton("刷新应用") { Margin = new Padding(0, 0, 8, 0) };
        btnRefreshApps.Click += async (_, _) =>
        {
            await RefreshAppsAsync(forceRender: true);
            ShowToast($"已刷新，共 {_apps.Count} 个应用");
        };

        var btnAutoOrganize = new PillButton("整理分类") { Margin = new Padding(0, 0, 8, 0) };
        btnAutoOrganize.Click += (_, _) =>
        {
            _layout.AutoOrganize(_apps);
            _openFolderId = null;
            RenderHomeGrid();
            ShowToast("已按应用类型整理桌面文件夹");
        };

        _btnSyncStartMenu = new PillButton("同步开始菜单")
        {
            FillColor = Color.FromArgb(236, 253, 245),
            HoverColor = Color.FromArgb(209, 250, 229),
            BorderColor = Color.FromArgb(167, 243, 208),
            ForeColor = Color.FromArgb(6, 95, 70),
            Margin = new Padding(0)
        };
        _btnSyncStartMenu.Click += async (_, _) => await HandleSyncStartMenuAsync();

        desktopActions.Controls.AddRange(new Control[] { btnRefreshApps, btnAutoOrganize, _btnSyncStartMenu });
        generalCard.Controls.Add(desktopActions);
        generalCard.Controls.Add(startupRow);
        generalCard.Height = 132;

        // --- Section 2: Daemon & Takeover ---
        var daemonCard = CreateSettingsGroupCard("守护进程与接管");
        var daemonRow = new TransparentPanel { Height = 36, Dock = DockStyle.Top, BackColor = Color.Transparent };
        _settingsDaemonDot = new TransparentPanel { Size = new Size(9, 9), Location = new Point(2, 13), BackColor = Color.Transparent };
        _settingsDaemonDot.Paint += (_, e) =>
        {
            e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
            using var b = new SolidBrush(_daemonAlive ? Color.FromArgb(16, 185, 129) : Color.FromArgb(148, 163, 184));
            e.Graphics.FillEllipse(b, 0, 0, 8, 8);
        };
        _settingsDaemonLabel = new Label
        {
            Text = "正在检测 focusstaged 状态...",
            Font = new Font("Microsoft YaHei UI", 8.8f),
            ForeColor = Color.FromArgb(51, 65, 85),
            AutoSize = true,
            Location = new Point(18, 9)
        };
        _btnStartStopDaemon = new PillButton("启动守护") { Height = 30 };
        _btnStartStopDaemon.Click += async (_, _) =>
        {
            _btnStartStopDaemon.Enabled = false;
            try
            {
                if (_daemonAlive) await _ctx.RunControlAndRefreshAsync("shutdown");
                else await _ctx.StartDaemonAsync();
            }
            finally
            {
                _btnStartStopDaemon.Enabled = true;
            }
        };
        daemonRow.Resize += (_, _) => _btnStartStopDaemon.Location = new Point(daemonRow.Width - _btnStartStopDaemon.Width - 2, 3);
        daemonRow.Controls.AddRange(new Control[] { _settingsDaemonDot, _settingsDaemonLabel, _btnStartStopDaemon });

        var daemonActions = new TransparentFlowPanel
        {
            Dock = DockStyle.Top,
            Height = 40,
            Padding = new Padding(0, 6, 0, 0),
            WrapContents = false,
            BackColor = Color.Transparent
        };
        _btnTakeoverAll = new PillButton("接管全部 (Ctrl+Alt+Space)")
        {
            FillColor = Color.FromArgb(239, 246, 255),
            HoverColor = Color.FromArgb(219, 234, 254),
            BorderColor = Color.FromArgb(191, 219, 254),
            ForeColor = Color.FromArgb(29, 78, 216),
            Margin = new Padding(0)
        };
        _btnTakeoverAll.UpdateAutoWidth(28);
        _btnTakeoverAll.Click += async (_, _) => await _ctx.TakeoverAllAsync();
        daemonActions.Controls.Add(_btnTakeoverAll);

        daemonCard.Controls.Add(daemonActions);
        daemonCard.Controls.Add(daemonRow);
        daemonCard.Height = 122;

        // --- Section 3: Running Stages (Zero-flicker, tucked inside Settings) ---
        var stagesCard = new RoundedPanel
        {
            CornerRadius = 16,
            FillColor = Color.White,
            BorderColor = Color.FromArgb(226, 232, 240),
            Padding = new Padding(16, 12, 16, 14),
            Margin = new Padding(0, 0, 0, 6),
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink
        };

        var stagesHeaderRow = new TransparentPanel
        {
            Height = 30,
            Dock = DockStyle.Top,
            BackColor = Color.Transparent
        };
        _stagesHeaderLabel = new Label
        {
            Text = "运行中的 Stages (0)",
            Font = new Font("Microsoft YaHei UI", 9.0f, FontStyle.Bold),
            ForeColor = Color.FromArgb(71, 85, 105),
            AutoSize = true,
            Location = new Point(0, 5)
        };
        var btnRefreshStages = new PillButton("刷新") { Height = 26, Width = 58, Font = new Font("Microsoft YaHei UI", 8.0f) };
        btnRefreshStages.Click += async (_, _) =>
        {
            await _ctx.RefreshAndRebuildMenuAsync();
            RenderStages(force: true);
        };
        stagesHeaderRow.Resize += (_, _) => btnRefreshStages.Location = new Point(stagesHeaderRow.Width - btnRefreshStages.Width, 1);
        stagesHeaderRow.Controls.AddRange(new Control[] { _stagesHeaderLabel, btnRefreshStages });

        _stagesPanel = new BufferedFlowPanel
        {
            Dock = DockStyle.Top,
            FlowDirection = FlowDirection.TopDown,
            WrapContents = false,
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            BackColor = Color.Transparent,
            Padding = new Padding(0, 6, 0, 0)
        };

        stagesCard.Controls.Add(_stagesPanel);
        stagesCard.Controls.Add(stagesHeaderRow);

        settingsStack.Controls.AddRange(new Control[] { generalCard, daemonCard, stagesCard });
        _settingsScrollBody.Controls.Add(settingsStack);

        int lastSettingsContentWidth = -1;
        void SyncSettingsCardWidths()
        {
            int contentW = Math.Max(320, _settingsScrollBody.ClientSize.Width - 18);
            if (contentW == lastSettingsContentWidth) return;
            lastSettingsContentWidth = contentW;

            _settingsScrollBody.SuspendLayout();
            settingsStack.Width = contentW;
            generalCard.Width = contentW;
            daemonCard.Width = contentW;
            stagesCard.MinimumSize = new Size(contentW, 90);
            stagesCard.MaximumSize = new Size(contentW, 0);
            stagesCard.Width = contentW;
            UpdateStageCardWidths();
            _settingsScrollBody.ResumeLayout(performLayout: true);
        }
        _settingsScrollBody.Resize += (_, _) => SyncSettingsCardWidths();

        _settingsCard.Controls.Add(_settingsScrollBody);
        _settingsCard.Controls.Add(settingsHeader);
        _settingsOverlay.Controls.Add(_settingsCard);
        _settingsOverlay.Resize += (_, _) => CenterSettingsCard();

        // Assemble root hierarchy
        Controls.Add(_homeGrid);
        Controls.Add(_topBar);
        Controls.Add(_dockCard);
        Controls.Add(_toastPill);
        Controls.Add(_folderOverlay);
        Controls.Add(_settingsOverlay);

        _dockCard.BringToFront();
        _toastPill.BringToFront();
        _folderOverlay.BringToFront();
        _settingsOverlay.BringToFront();

        Resize += (_, _) =>
        {
            RebuildWallpaper();
            LayoutChrome();
            RefreshActiveOverlayBackdrop();
            Invalidate();
        };

        // Hide to tray instead of exiting when user closes the window
        FormClosing += (_, e) =>
        {
            if (e.CloseReason == CloseReason.UserClosing)
            {
                e.Cancel = true;
                CloseAllOverlays();
                Hide();
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
            RebuildWallpaper();
            LayoutChrome();
            CenterFolderCard();
            CenterSettingsCard();
            SyncSettingsCardWidths();
            await RefreshAppsAsync(forceRender: true);
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

    protected override void OnHandleCreated(EventArgs e)
    {
        base.OnHandleCreated(e);
        Win11WindowHelper.ApplyWin11Chrome(Handle);
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == Win11WindowHelper.WM_NCHITTEST)
        {
            if (Win11WindowHelper.TryHitTestResizeBorder(this, ref m))
            {
                return;
            }
        }
        base.WndProc(ref m);
    }

    protected override bool ProcessCmdKey(ref Message msg, Keys keyData)
    {
        if (keyData == Keys.Escape)
        {
            if (_settingsVisible)
            {
                CloseSettingsOverlay();
                return true;
            }
            if (_openFolderId is not null)
            {
                CloseFolderOverlay();
                return true;
            }
            if (!string.IsNullOrEmpty(_searchBox.SearchText))
            {
                _searchBox.ClearSearch();
                return true;
            }
        }
        else if (keyData == (Keys.Control | Keys.F))
        {
            CloseAllOverlays();
            _searchBox.FocusInput();
            return true;
        }
        return base.ProcessCmdKey(ref msg, keyData);
    }

    protected override void OnPaintBackground(PaintEventArgs e)
    {
        if (_wallpaper is null || _wallpaper.Width != ClientSize.Width || _wallpaper.Height != ClientSize.Height)
        {
            RebuildWallpaper();
        }
        if (_wallpaper is not null)
        {
            e.Graphics.DrawImageUnscaled(_wallpaper, 0, 0);
        }
        else
        {
            base.OnPaintBackground(e);
        }

        // Crisp subtle window border
        using var borderPen = new Pen(Color.FromArgb(65, 148, 163, 184), 1f);
        e.Graphics.DrawRectangle(borderPen, 0, 0, ClientSize.Width - 1, ClientSize.Height - 1);
    }

    private void RebuildWallpaper()
    {
        if (ClientSize.Width <= 0 || ClientSize.Height <= 0) return;
        _wallpaper?.Dispose();
        _wallpaper = UiGraphics.RenderWallpaper(ClientSize.Width, ClientSize.Height);
    }

    private void LayoutChrome()
    {
        // Position status dot cleanly to the right of FocusStage title at any DPI
        _statusDot.Location = new Point(_titleLbl.Right + 8, _titleLbl.Top + (_titleLbl.Height - _statusDot.Height) / 2 + 1);

        // Top bar layout: centered search pill + right-aligned window buttons
        int searchW = Math.Min(320, Math.Max(180, ClientSize.Width - 360));
        _searchBox.Width = searchW;
        _searchBox.Location = new Point((ClientSize.Width - searchW) / 2, (_topBar.Height - _searchBox.Height) / 2);

        _btnClose.Location = new Point(ClientSize.Width - _btnClose.Width - 14, (_topBar.Height - _btnClose.Height) / 2);
        _btnMinimize.Location = new Point(_btnClose.Left - _btnMinimize.Width - 4, (_topBar.Height - _btnMinimize.Height) / 2);

        // Floating bottom dock sized to fit all items symmetrically and centered
        if (_dockCard.Controls.Count > 0)
        {
            var flow = _dockCard.Controls[0];
            flow.PerformLayout();
            int neededW = flow.PreferredSize.Width + 32;
            _dockCard.Width = Math.Max(360, neededW);
            _dockCard.Height = Math.Max(68, flow.PreferredSize.Height + 16);
            flow.Location = new Point(
                Math.Max(0, (_dockCard.Width - flow.Width) / 2),
                Math.Max(0, (_dockCard.Height - flow.Height) / 2 - 1));
        }
        _dockCard.Location = new Point(
            Math.Max(12, (ClientSize.Width - _dockCard.Width) / 2),
            ClientSize.Height - _dockCard.Height - 14);

        PositionToast();

        _folderOverlay.Bounds = ClientRectangle;
        _settingsOverlay.Bounds = ClientRectangle;
    }

    private void PositionToast()
    {
        _toastPill.Location = new Point(
            Math.Max(16, (ClientSize.Width - _toastPill.Width) / 2),
            _dockCard.Top - _toastPill.Height - 10);
    }

    private void ShowToast(string message)
    {
        if (string.IsNullOrWhiteSpace(message)) return;
        _toastPill.SetMessage(message, Math.Max(220, ClientSize.Width - 80));
        PositionToast();
        _toastPill.Visible = true;
        _toastPill.BringToFront();
        if (_folderOverlay.Visible) _folderOverlay.BringToFront();
        if (_settingsOverlay.Visible) _settingsOverlay.BringToFront();
        _toastTimer.Stop();
        _toastTimer.Start();
    }

    private static RoundedPanel CreateSettingsGroupCard(string sectionTitle)
    {
        var card = new RoundedPanel
        {
            CornerRadius = 16,
            FillColor = Color.White,
            BorderColor = Color.FromArgb(226, 232, 240),
            Padding = new Padding(16, 12, 16, 12),
            Margin = new Padding(0, 0, 0, 12)
        };
        var title = new Label
        {
            Text = sectionTitle,
            Dock = DockStyle.Top,
            Height = 24,
            Font = new Font("Microsoft YaHei UI", 8.6f, FontStyle.Bold),
            ForeColor = Color.FromArgb(100, 116, 139)
        };
        card.Controls.Add(title);
        return card;
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

    public void UpdateDaemonAndStages(bool daemonAlive, List<StageDto> stages)
    {
        if (InvokeRequired)
        {
            BeginInvoke(() => UpdateDaemonAndStages(daemonAlive, stages));
            return;
        }

        bool daemonChanged = !_daemonStateInitialized || _daemonAlive != daemonAlive || _lastStageCount != stages.Count;
        _daemonStateInitialized = true;
        _daemonAlive = daemonAlive;
        _lastStageCount = stages.Count;
        _stages = stages;

        if (daemonChanged)
        {
            var tip = daemonAlive
                ? $"focusstaged 运行中 · {_stages.Count} 个活跃舞台"
                : "focusstaged 待机中（启动应用时自动拉起）";
            _toolTip.SetToolTip(_statusDot, tip);
            _statusDot.Invalidate();

            _settingsDaemonLabel.Text = daemonAlive
                ? $"focusstaged 运行中 · {_stages.Count} 个活跃舞台"
                : "focusstaged 未运行（启动应用时自动拉起）";
            _settingsDaemonDot.Invalidate();
            _btnStartStopDaemon.Text = daemonAlive ? "停止守护" : "启动守护";
            _btnTakeoverAll.Enabled = daemonAlive && _stages.Count > 0;
            _stagesHeaderLabel.Text = $"运行中的 Stages ({_stages.Count})";
        }

        // Only re-render stages inside Settings when Settings is open AND the stage list actually changed (no flickering!)
        if (_settingsVisible)
        {
            RenderStages(force: false);
        }
    }

    private static string BuildStagesSignature(List<StageDto> stages)
    {
        if (stages.Count == 0) return "";
        return string.Join("|", stages.Select(s =>
            $"{s.StageId}:{s.App}:{s.Title}:{s.Display}:{s.Kind}:{s.InputOwner.Type}:{s.InputOwner.AgentName}"));
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
                if (forceRender)
                {
                    ShowToast($"读取 Linux 应用失败: {err}");
                }
                return;
            }

            var sig = string.Join("|", apps.Select(a => $"{a.Id}:{a.DisplayName}:{a.Source}"));
            bool listChanged = sig != _lastAppsSignature;
            _lastAppsSignature = sig;

            _apps = apps;
            _appById = apps.ToDictionary(a => a.Id, StringComparer.Ordinal);
            _layout = LauncherLayout.LoadOrDefault(_apps);

            if (forceRender || listChanged)
            {
                RenderHomeGrid();
                if (_openFolderId is not null)
                {
                    RenderFolderModal();
                }
                RefreshActiveOverlayBackdrop();
                if (listChanged && !forceRender)
                {
                    ShowToast($"已自动检测到应用更新，当前共 {_apps.Count} 个应用");
                }
            }
        }
        finally
        {
            _loadingApps = false;
        }
    }

    private void RenderHomeGrid()
    {
        var filter = _searchBox.SearchText.Trim();
        _homeGrid.SuspendLayout();
        _homeGrid.Controls.Clear();

        if (!string.IsNullOrWhiteSpace(filter))
        {
            var matched = _apps
                .Where(a =>
                    a.DisplayName.Contains(filter, StringComparison.OrdinalIgnoreCase) ||
                    a.Name.Contains(filter, StringComparison.OrdinalIgnoreCase) ||
                    a.Cmd.Contains(filter, StringComparison.OrdinalIgnoreCase))
                .ToList();

            if (matched.Count == 0)
            {
                _homeGrid.Controls.Add(MakeHintLabel($"未找到匹配 “{filter}” 的应用"));
            }
            else
            {
                foreach (var app in matched)
                {
                    _homeGrid.Controls.Add(BuildAppTile(app, inFolderId: null));
                }
            }
            _homeGrid.ResumeLayout();
            return;
        }

        if (_apps.Count == 0)
        {
            _homeGrid.Controls.Add(MakeHintLabel("暂未发现 Linux 应用，可点击底部“应用中心”安装，或直接下载 .AppImage / .deb"));
            _homeGrid.ResumeLayout();
            return;
        }

        foreach (var token in _layout.RootOrder)
        {
            if (LauncherLayout.IsFolderToken(token, out var fid))
            {
                var folder = _layout.FindFolder(fid);
                if (folder is not null)
                {
                    _homeGrid.Controls.Add(BuildFolderTile(folder));
                }
            }
            else if (_appById.TryGetValue(token, out var app))
            {
                _homeGrid.Controls.Add(BuildAppTile(app, inFolderId: null));
            }
        }

        _homeGrid.ResumeLayout();
    }

    private static Label MakeHintLabel(string text) => new()
    {
        Text = text,
        AutoSize = true,
        Padding = new Padding(8, 12, 8, 12),
        ForeColor = Color.FromArgb(100, 116, 139),
        BackColor = Color.Transparent
    };

    private Control BuildFolderTile(LauncherFolder folder)
    {
        var tileKey = "folder:" + folder.Id;
        var tile = new DesktopIconTile
        {
            TileKey = tileKey,
            TitleText = folder.Name,
            IsSelected = _selectedItemKey == tileKey,
            IsFolder = true,
            AllowDrop = true
        };

        var previewIcons = new List<Image>();
        foreach (var aid in folder.AppIds.Take(4))
        {
            if (_appById.TryGetValue(aid, out var app))
            {
                previewIcons.Add(GetCachedAppImage(app));
            }
        }
        tile.FolderPreviewIcons = previewIcons;

        tile.Click += (_, _) =>
        {
            _selectedItemKey = tileKey;
            RefreshSelectionVisuals(_homeGrid);
            OpenFolderOverlay(folder.Id);
        };
        tile.MouseUp += (_, e) =>
        {
            if (e.Button == MouseButtons.Right)
            {
                _selectedItemKey = tileKey;
                RefreshSelectionVisuals(_homeGrid);
                ShowFolderMenu(folder, tile, e.Location);
            }
        };

        tile.DragEnter += (_, e) =>
        {
            if (e.Data?.GetDataPresent(typeof(DragAppPayload)) == true)
            {
                e.Effect = DragDropEffects.Move;
                tile.IsDropTarget = true;
            }
        };
        tile.DragLeave += (_, _) => tile.IsDropTarget = false;
        tile.DragDrop += (_, e) =>
        {
            tile.IsDropTarget = false;
            if (e.Data?.GetData(typeof(DragAppPayload)) is DragAppPayload payload)
            {
                _layout.MoveAppToFolder(payload.AppId, folder.Id);
                RenderHomeGrid();
                if (_openFolderId is not null) RenderFolderModal();
                ShowToast($"已移入文件夹 “{folder.Name}”");
            }
        };

        return tile;
    }

    private Control BuildAppTile(LinuxAppDto app, string? inFolderId)
    {
        var tile = new DesktopIconTile
        {
            TileKey = app.Id,
            TitleText = app.DisplayName,
            AppIcon = GetCachedAppImage(app),
            SubBadgeText = string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase)
                ? "DEB"
                : string.Equals(app.Source, "appimage", StringComparison.OrdinalIgnoreCase)
                    ? "APP"
                    : null,
            IsSelected = _selectedItemKey == app.Id,
            AllowDrop = true
        };

        Point? dragStart = null;
        tile.MouseDown += (_, e) =>
        {
            if (e.Button == MouseButtons.Left)
            {
                dragStart = e.Location;
                _selectedItemKey = app.Id;
                RefreshSelectionVisuals(inFolderId is null ? _homeGrid : _folderGrid);
            }
        };
        tile.MouseMove += (_, e) =>
        {
            if (e.Button == MouseButtons.Left && dragStart.HasValue)
            {
                if (Math.Abs(e.X - dragStart.Value.X) > 8 || Math.Abs(e.Y - dragStart.Value.Y) > 8)
                {
                    dragStart = null;
                    tile.DoDragDrop(new DragAppPayload(app.Id, inFolderId), DragDropEffects.Move);
                }
            }
        };
        tile.MouseUp += (_, e) =>
        {
            dragStart = null;
            if (e.Button == MouseButtons.Right)
            {
                _selectedItemKey = app.Id;
                RefreshSelectionVisuals(inFolderId is null ? _homeGrid : _folderGrid);
                ShowAppMenu(app, inFolderId, tile, e.Location);
            }
        };
        tile.DoubleClick += async (_, _) => await ActivateAppAsync(app);

        if (inFolderId is null)
        {
            tile.DragEnter += (_, e) =>
            {
                if (e.Data?.GetData(typeof(DragAppPayload)) is DragAppPayload payload && payload.AppId != app.Id)
                {
                    e.Effect = DragDropEffects.Move;
                    tile.IsDropTarget = true;
                }
            };
            tile.DragLeave += (_, _) => tile.IsDropTarget = false;
            tile.DragDrop += (_, e) =>
            {
                tile.IsDropTarget = false;
                if (e.Data?.GetData(typeof(DragAppPayload)) is DragAppPayload payload && payload.AppId != app.Id)
                {
                    var folder = _layout.MergeAppsIntoNewFolder(payload.AppId, app.Id, "新建文件夹");
                    RenderHomeGrid();
                    OpenFolderOverlay(folder.Id);
                    ShowToast($"已创建文件夹 “{folder.Name}”");
                }
            };
        }

        return tile;
    }

    private void RefreshSelectionVisuals(FlowLayoutPanel container)
    {
        foreach (Control c in container.Controls)
        {
            if (c is DesktopIconTile t)
            {
                t.IsSelected = t.TileKey == _selectedItemKey;
            }
        }
    }

    private void CloseAllOverlays()
    {
        CloseFolderOverlay();
        CloseSettingsOverlay();
    }

    private void OpenFolderOverlay(string folderId)
    {
        CloseSettingsOverlay();
        _openFolderId = folderId;
        _folderOverlay.Bounds = ClientRectangle;
        _folderOverlay.SuspendLayout();
        CenterFolderCard();
        RenderFolderModal();
        _folderOverlay.ResumeLayout(performLayout: true);

        CaptureOverlayBackdrop(_folderOverlay);
        _folderOverlay.BringToFront();
        _folderOverlay.Visible = true;
    }

    private void CloseFolderOverlay()
    {
        if (_openFolderId is null && !_folderOverlay.Visible) return;
        _openFolderId = null;
        _folderOverlay.Visible = false;
        _folderOverlay.ClearBackdrop();
    }

    private void CenterFolderCard()
    {
        if (_folderOverlay.Width <= 0 || _folderOverlay.Height <= 0) return;
        int w = Math.Min(580, Math.Max(360, _folderOverlay.Width - 100));
        int h = Math.Min(430, Math.Max(280, _folderOverlay.Height - 100));
        _folderCard.SetBounds(
            (_folderOverlay.Width - w) / 2,
            (_folderOverlay.Height - h) / 2,
            w,
            h);
    }

    private void OpenSettingsOverlay()
    {
        CloseFolderOverlay();
        _settingsVisible = true;
        _toggleStartup.SetCheckedSilent(ShortcutManager.IsStartupEnabled());

        _settingsOverlay.Bounds = ClientRectangle;
        _settingsOverlay.SuspendLayout();
        CenterSettingsCard();
        RenderStages(force: true);
        _settingsOverlay.ResumeLayout(performLayout: true);

        CaptureOverlayBackdrop(_settingsOverlay);
        _settingsOverlay.BringToFront();
        _settingsOverlay.Visible = true;
    }

    private void CloseSettingsOverlay()
    {
        if (!_settingsVisible && !_settingsOverlay.Visible) return;
        _settingsVisible = false;
        _settingsOverlay.Visible = false;
        _settingsOverlay.ClearBackdrop();
    }

    private void CenterSettingsCard()
    {
        if (_settingsOverlay.Width <= 0 || _settingsOverlay.Height <= 0) return;
        int w = Math.Min(540, Math.Max(380, _settingsOverlay.Width - 96));
        int h = Math.Min(540, Math.Max(340, _settingsOverlay.Height - 80));
        _settingsCard.SetBounds(
            (_settingsOverlay.Width - w) / 2,
            (_settingsOverlay.Height - h) / 2,
            w,
            h);
    }

    private void RefreshActiveOverlayBackdrop()
    {
        if (_folderOverlay.Visible)
        {
            CaptureOverlayBackdrop(_folderOverlay);
            _folderOverlay.Invalidate();
        }
        if (_settingsOverlay.Visible)
        {
            CaptureOverlayBackdrop(_settingsOverlay);
            _settingsOverlay.Invalidate();
        }
    }

    private void CaptureOverlayBackdrop(DimOverlayPanel overlay)
    {
        if (ClientSize.Width <= 0 || ClientSize.Height <= 0) return;
        overlay.CaptureBackdrop(ClientSize, bmp =>
        {
            using (var g = Graphics.FromImage(bmp))
            {
                if (_wallpaper is null || _wallpaper.Width != ClientSize.Width || _wallpaper.Height != ClientSize.Height)
                {
                    RebuildWallpaper();
                }
                if (_wallpaper is not null)
                {
                    g.DrawImageUnscaled(_wallpaper, 0, 0);
                }
                else
                {
                    g.Clear(BackColor);
                }

                using var borderPen = new Pen(Color.FromArgb(65, 148, 163, 184), 1f);
                g.DrawRectangle(borderPen, 0, 0, ClientSize.Width - 1, ClientSize.Height - 1);
            }

            DrawSiblingToBitmap(_homeGrid, bmp);
            DrawSiblingToBitmap(_topBar, bmp);
            DrawSiblingToBitmap(_dockCard, bmp);
            if (_toastPill.Visible)
            {
                DrawSiblingToBitmap(_toastPill, bmp);
            }
        });
    }

    private static void DrawSiblingToBitmap(Control control, Bitmap target)
    {
        if (!control.Visible || control.Width <= 0 || control.Height <= 0) return;
        var bounds = control.Bounds;
        if (bounds.Right <= 0 || bounds.Bottom <= 0 || bounds.Left >= target.Width || bounds.Top >= target.Height) return;
        control.DrawToBitmap(target, bounds);
    }

    private void RenderFolderModal()
    {
        if (_openFolderId is null) return;
        var folder = _layout.FindFolder(_openFolderId);
        if (folder is null)
        {
            CloseFolderOverlay();
            return;
        }

        _folderTitleLabel.Text = $"{folder.Name}  ({folder.AppIds.Count})";
        _folderGrid.SuspendLayout();
        _folderGrid.Controls.Clear();

        if (folder.AppIds.Count == 0)
        {
            _folderGrid.Controls.Add(MakeHintLabel("文件夹为空，可拖拽应用到此文件夹或右键应用“移动到文件夹”"));
        }
        else
        {
            foreach (var aid in folder.AppIds)
            {
                if (_appById.TryGetValue(aid, out var app))
                {
                    _folderGrid.Controls.Add(BuildAppTile(app, inFolderId: folder.Id));
                }
            }
        }
        _folderGrid.ResumeLayout();
    }

    private int GetStageCardWidth() =>
        Math.Max(260, _stagesPanel.Parent is not null ? _stagesPanel.Parent.ClientSize.Width - 36 : 420);

    private void UpdateStageCardWidths()
    {
        int cardWidth = GetStageCardWidth();
        bool anyUpdated = false;
        foreach (Control c in _stagesPanel.Controls)
        {
            if (c is RoundedPanel card && card.Width != cardWidth)
            {
                if (!anyUpdated)
                {
                    _stagesPanel.SuspendLayout();
                    anyUpdated = true;
                }
                card.Width = cardWidth;
            }
        }
        if (anyUpdated)
        {
            _stagesPanel.ResumeLayout(performLayout: true);
        }
    }

    private void RenderStages(bool force)
    {
        var sig = BuildStagesSignature(_stages);
        if (!force && sig == _lastStagesSignature)
        {
            return;
        }
        _lastStagesSignature = sig;
        _stagesHeaderLabel.Text = $"运行中的 Stages ({_stages.Count})";

        _stagesPanel.SuspendLayout();
        _stagesPanel.Controls.Clear();

        int cardWidth = GetStageCardWidth();

        if (_stages.Count == 0)
        {
            _stagesPanel.Controls.Add(new Label
            {
                Text = "当前没有运行中的 Stage",
                AutoSize = true,
                Padding = new Padding(2, 6, 2, 4),
                ForeColor = Color.FromArgb(148, 163, 184),
                Font = new Font("Microsoft YaHei UI", 8.6f)
            });
            _stagesPanel.ResumeLayout();
            return;
        }

        foreach (var stage in _stages)
        {
            var card = new RoundedPanel
            {
                Width = cardWidth,
                Height = 76,
                CornerRadius = 12,
                FillColor = Color.FromArgb(248, 250, 252),
                BorderColor = Color.FromArgb(226, 232, 240),
                Margin = new Padding(0, 0, 0, 8),
                Padding = new Padding(12, 8, 12, 8)
            };

            var title = string.IsNullOrWhiteSpace(stage.Title) ? stage.App : stage.Title;
            var ownerTag = stage.InputOwner.Type switch
            {
                "agent" => $"AI:{stage.InputOwner.AgentName ?? "agent"}",
                "frozen" => "已冻结",
                _ => "人类"
            };
            var dispTag = stage.Display == "desktop" ? "桌面显示" : "后台运行";

            var lblTitle = new Label
            {
                Text = $"[{stage.StageId}] {title}",
                Font = new Font("Microsoft YaHei UI", 8.8f, FontStyle.Bold),
                ForeColor = Color.FromArgb(30, 41, 59),
                Location = new Point(12, 8),
                Size = new Size(cardWidth - 24, 20),
                Anchor = AnchorStyles.Top | AnchorStyles.Left | AnchorStyles.Right,
                AutoEllipsis = true,
                BackColor = Color.Transparent
            };
            var lblMeta = new Label
            {
                Text = $"{dispTag} · {ownerTag} · {stage.Kind}",
                Font = new Font("Microsoft YaHei UI", 7.9f),
                ForeColor = Color.FromArgb(100, 116, 139),
                Location = new Point(12, 27),
                Size = new Size(cardWidth - 24, 16),
                Anchor = AnchorStyles.Top | AnchorStyles.Left | AnchorStyles.Right,
                AutoEllipsis = true,
                BackColor = Color.Transparent
            };

            var actions = new TransparentFlowPanel
            {
                Location = new Point(10, 45),
                Size = new Size(cardWidth - 20, 26),
                Anchor = AnchorStyles.Top | AnchorStyles.Left | AnchorStyles.Right,
                WrapContents = false,
                BackColor = Color.Transparent
            };

            var stageId = stage.StageId;
            var isDesktop = stage.Display == "desktop";

            var btnTake = MakeStageActionBtn("接管");
            btnTake.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync("takeover", stageId);

            var btnVis = MakeStageActionBtn(isDesktop ? "隐藏" : "显示");
            btnVis.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync(isDesktop ? "hide" : "show", stageId);

            var btnClose = MakeStageActionBtn("关闭");
            btnClose.ForeColor = Color.FromArgb(185, 28, 28);
            btnClose.FillColor = Color.FromArgb(254, 242, 242);
            btnClose.HoverColor = Color.FromArgb(254, 226, 226);
            btnClose.BorderColor = Color.FromArgb(254, 202, 202);
            btnClose.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync("close", stageId);

            actions.Controls.AddRange(new Control[] { btnTake, btnVis, btnClose });
            card.Controls.AddRange(new Control[] { lblTitle, lblMeta, actions });
            _stagesPanel.Controls.Add(card);
        }

        _stagesPanel.ResumeLayout();
    }

    private static PillButton MakeStageActionBtn(string text)
    {
        var btn = new PillButton(text)
        {
            Height = 24,
            CornerRadius = 7,
            Font = new Font("Microsoft YaHei UI", 7.9f),
            Margin = new Padding(0, 0, 6, 0)
        };
        btn.UpdateAutoWidth(18);
        return btn;
    }

    private void ShowAppMenu(LinuxAppDto app, string? inFolderId, Control anchor, Point loc)
    {
        var menu = new ContextMenuStrip();

        bool isDeb = string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase);
        var openItem = new ToolStripMenuItem(isDeb ? "安装此 .deb 安装包" : "打开应用");
        openItem.Font = new Font(openItem.Font, FontStyle.Bold);
        openItem.Click += async (_, _) => await ActivateAppAsync(app);
        menu.Items.Add(openItem);

        if (!isDeb)
        {
            var startMenuItem = new ToolStripMenuItem("添加到 Windows 开始菜单");
            startMenuItem.Click += (_, _) =>
            {
                try
                {
                    ShortcutManager.CreateStartMenuAppShortcut(_distro, app, alsoTopLevel: true);
                    ShowToast($"已将 “{app.DisplayName}” 添加到开始菜单");
                }
                catch (Exception ex)
                {
                    MessageBox.Show(this, $"添加到开始菜单失败: {ex.Message}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                }
            };
            menu.Items.Add(startMenuItem);

            var desktopItem = new ToolStripMenuItem("发送到 Windows 桌面");
            desktopItem.Click += (_, _) =>
            {
                try
                {
                    var path = ShortcutManager.CreateDesktopAppShortcut(_distro, app);
                    ShowToast($"已创建桌面快捷方式：{Path.GetFileName(path)}");
                }
                catch (Exception ex)
                {
                    MessageBox.Show(this, $"创建桌面快捷方式失败: {ex.Message}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                }
            };
            menu.Items.Add(desktopItem);
        }

        menu.Items.Add(new ToolStripSeparator());

        var moveSub = new ToolStripMenuItem("移动到文件夹");
        foreach (var f in _layout.Folders)
        {
            if (f.Id == inFolderId) continue;
            var targetFolder = f;
            var mi = new ToolStripMenuItem(targetFolder.Name);
            mi.Click += (_, _) =>
            {
                _layout.MoveAppToFolder(app.Id, targetFolder.Id);
                RenderHomeGrid();
                if (_openFolderId is not null) RenderFolderModal();
                ShowToast($"已将 “{app.DisplayName}” 移入 “{targetFolder.Name}”");
            };
            moveSub.DropDownItems.Add(mi);
        }
        if (moveSub.DropDownItems.Count > 0)
        {
            moveSub.DropDownItems.Add(new ToolStripSeparator());
        }
        var newFolderMove = new ToolStripMenuItem("+ 新建文件夹并移入...");
        newFolderMove.Click += (_, _) =>
        {
            var name = PromptDialog.Show(this, "新建文件夹", "请输入文件夹名称：", "新建文件夹");
            if (name is null) return;
            var folder = _layout.CreateFolder(name, app.Id);
            RenderHomeGrid();
            if (_openFolderId is not null) RenderFolderModal();
            ShowToast($"已创建文件夹 “{folder.Name}” 并移入 “{app.DisplayName}”");
        };
        moveSub.DropDownItems.Add(newFolderMove);
        menu.Items.Add(moveSub);

        if (inFolderId is not null)
        {
            var moveOut = new ToolStripMenuItem("移出到主桌面");
            moveOut.Click += (_, _) =>
            {
                _layout.MoveAppToRoot(app.Id);
                RenderHomeGrid();
                RenderFolderModal();
                ShowToast($"已将 “{app.DisplayName}” 移回主桌面");
            };
            menu.Items.Add(moveOut);
        }

        menu.Items.Add(new ToolStripSeparator());
        var infoItem = new ToolStripMenuItem("查看命令 / 来源");
        infoItem.Click += (_, _) =>
        {
            var cmdDesc = app.Args.Count > 0 ? $"{app.Cmd} {string.Join(" ", app.Args)}" : app.Cmd;
            MessageBox.Show(
                this,
                $"名称：{app.DisplayName}\n来源：{app.Source}\n命令：{cmdDesc}\nWM_CLASS：{app.WmClass}",
                "应用属性",
                MessageBoxButtons.OK,
                MessageBoxIcon.Information);
        };
        menu.Items.Add(infoItem);

        menu.Show(anchor, loc);
    }

    private void ShowFolderMenu(LauncherFolder folder, Control anchor, Point loc)
    {
        var menu = new ContextMenuStrip();

        var openItem = new ToolStripMenuItem("打开文件夹")
        {
            Font = new Font(Font, FontStyle.Bold)
        };
        openItem.Click += (_, _) => OpenFolderOverlay(folder.Id);

        var renameItem = new ToolStripMenuItem("重命名文件夹");
        renameItem.Click += (_, _) =>
        {
            var next = PromptDialog.Show(this, "重命名文件夹", "文件夹名称：", folder.Name);
            if (next is not null)
            {
                _layout.RenameFolder(folder.Id, next);
                RenderHomeGrid();
                if (_openFolderId == folder.Id) RenderFolderModal();
            }
        };

        var syncFolderItem = new ToolStripMenuItem("将文件夹内应用添加到开始菜单");
        syncFolderItem.Click += (_, _) =>
        {
            int added = 0;
            foreach (var aid in folder.AppIds)
            {
                if (_appById.TryGetValue(aid, out var app) && !string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase))
                {
                    ShortcutManager.CreateStartMenuAppShortcut(_distro, app, alsoTopLevel: true);
                    added++;
                }
            }
            ShowToast($"已将文件夹 “{folder.Name}” 内 {added} 个应用添加到开始菜单");
        };

        var dissolveItem = new ToolStripMenuItem("解散文件夹（应用移回主桌面）");
        dissolveItem.Click += (_, _) =>
        {
            _layout.DissolveFolder(folder.Id);
            if (_openFolderId == folder.Id) CloseFolderOverlay();
            RenderHomeGrid();
            ShowToast($"已解散文件夹 “{folder.Name}”");
        };

        menu.Items.AddRange(new ToolStripItem[]
        {
            openItem, renameItem, syncFolderItem, new ToolStripSeparator(), dissolveItem
        });
        menu.Show(anchor, loc);
    }

    private void ShowEmptyAreaMenu(Control anchor, Point loc)
    {
        var menu = new ContextMenuStrip();

        var newFolderItem = new ToolStripMenuItem("新建文件夹");
        newFolderItem.Click += (_, _) => PromptCreateFolder();

        var organizeItem = new ToolStripMenuItem("按类别自动整理文件夹");
        organizeItem.Click += (_, _) =>
        {
            _layout.AutoOrganize(_apps);
            RenderHomeGrid();
            ShowToast("已按类别自动整理桌面文件夹");
        };

        var appCenterItem = new ToolStripMenuItem("打开 Ubuntu 应用中心");
        appCenterItem.Click += async (_, _) => await OpenOrInstallAppCenterAsync();

        var customCmdItem = new ToolStripMenuItem("运行自定义命令...");
        customCmdItem.Click += (_, _) => _ctx.PromptLaunchCustom(this);

        var refreshItem = new ToolStripMenuItem("刷新应用列表");
        refreshItem.Click += async (_, _) => await RefreshAppsAsync(forceRender: true);

        var settingsItem = new ToolStripMenuItem("设置...");
        settingsItem.Click += (_, _) => OpenSettingsOverlay();

        menu.Items.AddRange(new ToolStripItem[]
        {
            newFolderItem, organizeItem, new ToolStripSeparator(), appCenterItem, customCmdItem, refreshItem, new ToolStripSeparator(), settingsItem
        });
        menu.Show(anchor, loc);
    }

    private void PromptCreateFolder()
    {
        var name = PromptDialog.Show(this, "新建文件夹", "请输入文件夹名称：", "新建文件夹");
        if (name is null) return;
        _layout.CreateFolder(name);
        RenderHomeGrid();
        ShowToast($"已创建文件夹 “{name}”");
    }

    private void PromptRenameCurrentFolder()
    {
        if (_openFolderId is null) return;
        var folder = _layout.FindFolder(_openFolderId);
        if (folder is null) return;
        var next = PromptDialog.Show(this, "重命名文件夹", "文件夹名称：", folder.Name);
        if (next is null) return;
        _layout.RenameFolder(folder.Id, next);
        RenderHomeGrid();
        RenderFolderModal();
    }

    private Image GetCachedAppImage(LinuxAppDto app)
    {
        if (_iconCache.TryGetValue(app.Id, out var cached))
        {
            return cached;
        }
        var bmp = ShortcutManager.TryLoadLinuxAppBitmap(_distro, app, 48)
                  ?? ShortcutManager.RenderAppFallbackBitmap(app.DisplayName, 48);
        _iconCache[app.Id] = bmp;
        return bmp;
    }

    private async Task ActivateAppAsync(LinuxAppDto app)
    {
        if (string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase))
        {
            var confirm = MessageBox.Show(
                this,
                $"检测到下载的 Linux 安装包：\n{app.DisplayName}\n\n是否立即安装到 WSL ({_distro})？安装完成后会自动出现在启动器中。",
                "安装 .deb 应用",
                MessageBoxButtons.YesNo,
                MessageBoxIcon.Question);
            if (confirm != DialogResult.Yes) return;

            ShowToast($"正在安装 {app.DisplayName}...");
            var (ok, msg) = await ShortcutManager.InstallDebPackageAsync(_distro, app.Cmd);
            ShowToast(ok ? msg : $"安装失败: {msg}");
            if (ok)
            {
                await RefreshAppsAsync(forceRender: true);
                MessageBox.Show(this, msg, "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Information);
            }
            else
            {
                MessageBox.Show(this, $"安装失败：\n{msg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
            return;
        }

        ShowToast($"正在启动 {app.DisplayName}...");
        var args = new List<string> { "launch", "--ensure-daemon", "--wm-class", app.WmClass, app.Cmd };
        if (app.Args.Count > 0)
        {
            args.Add("--");
            args.AddRange(app.Args);
        }
        var (launchOk, launchMsg) = await _ctx.RunControlWithFeedbackAsync(args.ToArray());
        if (launchOk)
        {
            ShowToast($"已启动 {app.DisplayName}");
        }
        else
        {
            ShowToast($"启动 {app.DisplayName} 失败");
            MessageBox.Show(this, $"启动 {app.DisplayName} 失败：\n{launchMsg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
    }

    private async Task OpenOrInstallAppCenterAsync()
    {
        _dockAppCenter.Enabled = false;
        try
        {
            ShowToast("正在查找 Ubuntu 应用中心...");
            var cmd = await ShortcutManager.FindInstalledAppCenterCommandAsync(_distro);
            if (string.IsNullOrWhiteSpace(cmd))
            {
                var confirm = MessageBox.Show(
                    this,
                    $"当前 WSL ({_distro}) 尚未安装 Ubuntu 应用中心 (snap-store / gnome-software)。\n\n是否立即自动安装并打开？安装的应用会自动出现在 FocusStage 启动器中。",
                    "安装 Ubuntu 应用中心",
                    MessageBoxButtons.YesNo,
                    MessageBoxIcon.Question);
                if (confirm != DialogResult.Yes)
                {
                    ShowToast("已取消安装应用中心");
                    return;
                }

                ShowToast("正在安装 Ubuntu 应用中心（首次约需 1-2 分钟）...");
                var (instOk, instCmd, instMsg) = await ShortcutManager.InstallAppCenterAsync(_distro);
                if (!instOk)
                {
                    ShowToast("安装应用中心失败");
                    MessageBox.Show(this, $"安装应用中心失败：\n{instMsg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
                    return;
                }
                cmd = instCmd;
                await RefreshAppsAsync(forceRender: true);
            }

            ShowToast($"正在启动应用中心 ({cmd})...");
            var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("launch", "--ensure-daemon", cmd);
            ShowToast(ok ? $"已打开应用中心 ({cmd})" : $"启动应用中心失败");
            if (!ok)
            {
                MessageBox.Show(this, $"启动应用中心失败：\n{msg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
        }
        finally
        {
            _dockAppCenter.Enabled = true;
        }
    }

    private async Task HandleSyncStartMenuAsync()
    {
        _btnSyncStartMenu.Enabled = false;
        ShowToast("正在同步所有 Linux 应用到 Windows 开始菜单...");
        try
        {
            var (ok, msg, _) = await ShortcutManager.SyncAllAsync(_distro, createDesktopForLauncher: true, createStartupTray: _toggleStartup.Checked);
            ShowToast(msg);
            MessageBox.Show(
                this,
                ok
                    ? $"{msg}\n\n你可以在“开始 -> 所有应用 -> FocusStage”看到全部应用，也可以右键任意应用“固定到开始屏幕”或“固定到任务栏”。"
                    : $"同步失败：{msg}",
                "FocusStage",
                MessageBoxButtons.OK,
                ok ? MessageBoxIcon.Information : MessageBoxIcon.Warning);
        }
        finally
        {
            _btnSyncStartMenu.Enabled = true;
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing)
        {
            _toastTimer.Stop();
            _toastTimer.Dispose();
            _autoDiscoverTimer.Stop();
            _autoDiscoverTimer.Dispose();
            _debounceTimer.Stop();
            _debounceTimer.Dispose();
            _downloadsWatcher?.Dispose();
            _toolTip.Dispose();
            _wallpaper?.Dispose();
            foreach (var img in _iconCache.Values) img.Dispose();
            _iconCache.Clear();
        }
        base.Dispose(disposing);
    }

    private sealed record DragAppPayload(string AppId, string? FromFolderId);

    private static class PromptDialog
    {
        public static string? Show(IWin32Window owner, string title, string prompt, string defaultValue)
        {
            using var dlg = new Form
            {
                Text = title,
                AutoScaleMode = AutoScaleMode.Dpi,
                FormBorderStyle = FormBorderStyle.FixedDialog,
                StartPosition = FormStartPosition.CenterParent,
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

            var lbl = new Label { Text = prompt, AutoSize = true };
            var tb = new TextBox { Text = defaultValue, Dock = DockStyle.Fill };
            tb.SelectAll();

            var buttons = new TransparentFlowPanel
            {
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                FlowDirection = FlowDirection.RightToLeft,
                Dock = DockStyle.Fill,
                WrapContents = false,
                Margin = new Padding(0)
            };
            var cancel = new Button { Text = "取消", AutoSize = true, DialogResult = DialogResult.Cancel };
            var ok = new Button { Text = "确定", AutoSize = true, DialogResult = DialogResult.OK };
            buttons.Controls.Add(cancel);
            buttons.Controls.Add(ok);

            layout.Controls.Add(lbl, 0, 0);
            layout.Controls.Add(tb, 0, 1);
            layout.Controls.Add(buttons, 0, 2);
            dlg.Controls.Add(layout);
            dlg.AcceptButton = ok;
            dlg.CancelButton = cancel;

            void ApplyDpiLayout(int dpi)
            {
                int S(int px) => (int)Math.Round(px * (dpi / 96.0));
                int pad = S(16);
                lbl.MaximumSize = new Size(S(380), 0);
                lbl.Margin = new Padding(0, 0, 0, S(8));
                tb.MinimumSize = new Size(S(360), 0);
                tb.Margin = new Padding(0, 0, 0, S(12));
                cancel.MinimumSize = new Size(S(78), S(30));
                cancel.Padding = new Padding(S(10), S(3), S(10), S(3));
                cancel.Margin = new Padding(0);
                ok.MinimumSize = new Size(S(78), S(30));
                ok.Padding = new Padding(S(10), S(3), S(10), S(3));
                ok.Margin = new Padding(0, 0, S(8), 0);

                layout.Location = new Point(pad, pad);
                layout.PerformLayout();
                var pref = layout.GetPreferredSize(Size.Empty);
                dlg.ClientSize = new Size(pref.Width + pad * 2, pref.Height + pad * 2);
            }

            ApplyDpiLayout((owner as Control)?.DeviceDpi ?? dlg.DeviceDpi);
            dlg.Load += (_, _) => ApplyDpiLayout(dlg.DeviceDpi);

            return dlg.ShowDialog(owner) == DialogResult.OK && !string.IsNullOrWhiteSpace(tb.Text)
                ? tb.Text.Trim()
                : null;
        }
    }
}
