using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;

namespace FocusStageTray;

internal sealed class LauncherForm : Form
{
    private readonly FocusStageTrayContext _ctx;
    private readonly string _distro;

    private readonly Label _statusLabel;
    private readonly Button _btnAppCenter;
    private readonly Button _btnStartStopDaemon;
    private readonly Button _btnTakeoverAll;
    private readonly Button _btnSyncStartMenu;
    private readonly Button _btnStagesToggle;
    private readonly CheckBox _chkStartup;
    private readonly TextBox _searchBox;
    private readonly Label _footerLabel;

    private readonly Panel _workspaceHost;
    private readonly Panel _stagesDrawer;
    private readonly FlowLayoutPanel _stagesPanel;
    private readonly BufferedFlowPanel _homeGrid;

    private readonly DimOverlayPanel _folderOverlay;
    private readonly RoundedPanel _folderCard;
    private readonly Label _folderTitleLabel;
    private readonly BufferedFlowPanel _folderGrid;

    private List<LinuxAppDto> _apps = new();
    private Dictionary<string, LinuxAppDto> _appById = new(StringComparer.Ordinal);
    private LauncherLayout _layout = new();
    private List<StageDto> _stages = new();
    private bool _daemonAlive;
    private bool _loadingApps;
    private bool _stagesVisible;
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

        Text = $"FocusStage — Linux 应用桌面 ({distro})";
        Width = 1080;
        Height = 700;
        MinimumSize = new Size(820, 540);
        StartPosition = FormStartPosition.CenterScreen;
        BackColor = Color.FromArgb(241, 245, 249);
        Font = new Font("Microsoft YaHei UI", 9.2f, FontStyle.Regular);
        AutoScaleMode = AutoScaleMode.Dpi;
        DoubleBuffered = true;

        using (var bmp = ShortcutManager.RenderBrandBitmap(64))
        {
            Icon = Icon.FromHandle(bmp.GetHicon());
        }

        // 1. Header bar
        var header = new Panel
        {
            Dock = DockStyle.Top,
            Height = 64,
            BackColor = Color.FromArgb(22, 29, 43),
            Padding = new Padding(16, 10, 16, 10)
        };

        var titleLbl = new Label
        {
            Text = "FocusStage 应用桌面",
            ForeColor = Color.White,
            Font = new Font("Microsoft YaHei UI", 13f, FontStyle.Bold),
            AutoSize = true,
            Location = new Point(16, 10)
        };

        _statusLabel = new Label
        {
            Text = "正在检测 focusstaged 状态...",
            ForeColor = Color.FromArgb(160, 196, 235),
            Font = new Font("Microsoft YaHei UI", 8.6f),
            AutoSize = true,
            Location = new Point(18, 37)
        };

        var headerRight = new FlowLayoutPanel
        {
            Dock = DockStyle.Right,
            FlowDirection = FlowDirection.RightToLeft,
            WrapContents = false,
            AutoSize = true,
            BackColor = Color.Transparent,
            Padding = new Padding(0, 6, 0, 0)
        };

        _btnTakeoverAll = MakeHeaderButton("接管全部 (Ctrl+Alt+Space)", Color.FromArgb(37, 99, 235));
        _btnTakeoverAll.Click += async (_, _) => await _ctx.TakeoverAllAsync();

        _btnStartStopDaemon = MakeHeaderButton("启动守护", Color.FromArgb(44, 62, 86));
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

        _btnSyncStartMenu = MakeHeaderButton("同步开始菜单", Color.FromArgb(22, 134, 96));
        _btnSyncStartMenu.Click += async (_, _) => await HandleSyncStartMenuAsync();

        _btnAppCenter = MakeHeaderButton("应用中心", Color.FromArgb(217, 119, 6));
        _btnAppCenter.Click += async (_, _) => await OpenOrInstallAppCenterAsync();

        headerRight.Controls.Add(_btnTakeoverAll);
        headerRight.Controls.Add(_btnStartStopDaemon);
        headerRight.Controls.Add(_btnSyncStartMenu);
        headerRight.Controls.Add(_btnAppCenter);

        header.Controls.Add(titleLbl);
        header.Controls.Add(_statusLabel);
        header.Controls.Add(headerRight);

        // 2. Toolbar
        var toolbar = new FlowLayoutPanel
        {
            Dock = DockStyle.Top,
            AutoSize = true,
            AutoSizeMode = AutoSizeMode.GrowAndShrink,
            MinimumSize = new Size(0, 46),
            BackColor = Color.White,
            Padding = new Padding(12, 8, 12, 8),
            WrapContents = true
        };
        toolbar.Paint += (_, e) =>
        {
            using var pen = new Pen(Color.FromArgb(224, 230, 238));
            e.Graphics.DrawLine(pen, 0, toolbar.Height - 1, toolbar.Width, toolbar.Height - 1);
        };

        _searchBox = new TextBox
        {
            PlaceholderText = "搜索应用或文件夹...",
            Width = 190,
            Margin = new Padding(0, 2, 8, 0)
        };
        _searchBox.TextChanged += (_, _) => RenderHomeGrid();

        var btnNewFolder = MakeToolButton("+ 新建文件夹");
        btnNewFolder.Click += (_, _) => PromptCreateFolder();

        var btnAutoGroup = MakeToolButton("整理分类");
        btnAutoGroup.Click += (_, _) =>
        {
            _layout.AutoOrganize(_apps);
            _openFolderId = null;
            CloseFolderOverlay();
            RenderHomeGrid();
            SetFooter("已按应用类型整理桌面文件夹。");
        };

        var btnRefresh = MakeToolButton("刷新");
        btnRefresh.Click += async (_, _) => await RefreshAppsAsync(forceRender: true);

        var btnCustomCmd = MakeToolButton("运行命令...");
        btnCustomCmd.Click += (_, _) => _ctx.PromptLaunchCustom(this);

        _btnStagesToggle = MakeToolButton("运行中舞台 (0)");
        _btnStagesToggle.Click += (_, _) => ToggleStagesDrawer();

        _chkStartup = new CheckBox
        {
            Text = "开机自启",
            AutoSize = true,
            Checked = ShortcutManager.IsStartupEnabled(),
            Margin = new Padding(6, 4, 0, 0)
        };
        _chkStartup.CheckedChanged += (_, _) =>
        {
            try
            {
                ShortcutManager.SetStartupEnabled(_distro, _chkStartup.Checked);
                SetFooter(_chkStartup.Checked ? "已开启开机自启 FocusStage 托盘。" : "已关闭开机自启。");
            }
            catch (Exception ex)
            {
                MessageBox.Show(this, $"设置开机自启失败: {ex.Message}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
        };

        toolbar.Controls.AddRange(new Control[]
        {
            _searchBox, btnNewFolder, btnAutoGroup, btnRefresh, btnCustomCmd, _btnStagesToggle, _chkStartup
        });

        // 3. Footer
        var footer = new Panel
        {
            Dock = DockStyle.Bottom,
            Height = 28,
            BackColor = Color.FromArgb(234, 239, 246),
            Padding = new Padding(14, 5, 14, 4)
        };
        _footerLabel = new Label
        {
            Dock = DockStyle.Fill,
            Text = "提示：点击文件夹展开应用；双击应用直接启动；右键可添加到开始菜单/桌面或移动到文件夹；可拖拽图标合并成文件夹。",
            ForeColor = Color.FromArgb(71, 85, 105),
            Font = new Font("Microsoft YaHei UI", 8.4f),
            AutoEllipsis = true
        };
        footer.Controls.Add(_footerLabel);

        // 4. Main workspace (Home grid + collapsible Stages drawer)
        _workspaceHost = new Panel
        {
            Dock = DockStyle.Fill,
            BackColor = Color.FromArgb(241, 245, 249)
        };

        _homeGrid = new BufferedFlowPanel
        {
            Dock = DockStyle.Fill,
            AutoScroll = true,
            BackColor = Color.FromArgb(241, 245, 249),
            Padding = new Padding(22, 18, 22, 22),
            AllowDrop = true
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

        _stagesDrawer = new Panel
        {
            Dock = DockStyle.Right,
            Width = 310,
            Visible = false,
            BackColor = Color.FromArgb(248, 250, 252),
            Padding = new Padding(10)
        };
        _stagesDrawer.Paint += (_, e) =>
        {
            using var pen = new Pen(Color.FromArgb(220, 227, 236));
            e.Graphics.DrawLine(pen, 0, 0, 0, _stagesDrawer.Height);
        };

        var stagesTitle = new Label
        {
            Dock = DockStyle.Top,
            Height = 30,
            Text = "运行中的 Stages",
            Font = new Font("Microsoft YaHei UI", 9.5f, FontStyle.Bold),
            ForeColor = Color.FromArgb(30, 41, 59)
        };

        _stagesPanel = new FlowLayoutPanel
        {
            Dock = DockStyle.Fill,
            AutoScroll = true,
            FlowDirection = FlowDirection.TopDown,
            WrapContents = false,
            BackColor = Color.Transparent
        };
        _stagesDrawer.Controls.Add(_stagesPanel);
        _stagesDrawer.Controls.Add(stagesTitle);

        // 5. Phone-style Folder Overlay Modal (hidden unless a folder is open)
        _folderOverlay = new DimOverlayPanel
        {
            Dock = DockStyle.Fill,
            Visible = false
        };
        _folderOverlay.BackdropClicked += (_, _) => CloseFolderOverlay();

        _folderCard = new RoundedPanel
        {
            Width = 560,
            Height = 420,
            CornerRadius = 22,
            FillColor = Color.FromArgb(250, 252, 255),
            BorderColor = Color.FromArgb(214, 222, 234),
            Padding = new Padding(18, 14, 18, 16)
        };

        var folderHeader = new Panel
        {
            Dock = DockStyle.Top,
            Height = 44,
            BackColor = Color.Transparent
        };

        _folderTitleLabel = new Label
        {
            Dock = DockStyle.Fill,
            Font = new Font("Microsoft YaHei UI", 13f, FontStyle.Bold),
            ForeColor = Color.FromArgb(15, 23, 42),
            TextAlign = ContentAlignment.MiddleLeft,
            Cursor = Cursors.Hand
        };
        _folderTitleLabel.Click += (_, _) => PromptRenameCurrentFolder();

        var folderHeaderBtns = new FlowLayoutPanel
        {
            Dock = DockStyle.Right,
            AutoSize = true,
            FlowDirection = FlowDirection.LeftToRight,
            WrapContents = false,
            Padding = new Padding(0, 6, 0, 0)
        };

        var btnRenameFolder = MakeToolButton("重命名");
        btnRenameFolder.Click += (_, _) => PromptRenameCurrentFolder();

        var btnCloseFolder = MakeToolButton("✕");
        btnCloseFolder.Width = 34;
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
            Padding = new Padding(6, 10, 6, 6)
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

        _workspaceHost.Controls.Add(_homeGrid);
        _workspaceHost.Controls.Add(_stagesDrawer);
        _workspaceHost.Controls.Add(_folderOverlay);
        _folderOverlay.BringToFront();

        Controls.Add(_workspaceHost);
        Controls.Add(toolbar);
        Controls.Add(header);
        Controls.Add(footer);

        // Hide to tray instead of exiting when user closes the window
        FormClosing += (_, e) =>
        {
            if (e.CloseReason == CloseReason.UserClosing)
            {
                e.Cancel = true;
                CloseFolderOverlay();
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
            CenterFolderCard();
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

    private static Button MakeHeaderButton(string text, Color bg)
    {
        var btn = new Button
        {
            Text = text,
            AutoSize = true,
            Height = 32,
            FlatStyle = FlatStyle.Flat,
            BackColor = bg,
            ForeColor = Color.White,
            Font = new Font("Microsoft YaHei UI", 8.8f, FontStyle.Bold),
            Padding = new Padding(10, 0, 10, 0),
            Margin = new Padding(8, 0, 0, 0),
            Cursor = Cursors.Hand
        };
        btn.FlatAppearance.BorderSize = 0;
        return btn;
    }

    private static Button MakeToolButton(string text) => new()
    {
        Text = text,
        AutoSize = true,
        Height = 28,
        FlatStyle = FlatStyle.System,
        Font = new Font("Microsoft YaHei UI", 8.8f),
        Margin = new Padding(0, 0, 6, 0),
        Cursor = Cursors.Hand
    };

    private void SetFooter(string text) => _footerLabel.Text = text;

    public void UpdateDaemonAndStages(bool daemonAlive, List<StageDto> stages)
    {
        if (InvokeRequired)
        {
            BeginInvoke(() => UpdateDaemonAndStages(daemonAlive, stages));
            return;
        }
        _daemonAlive = daemonAlive;
        _stages = stages;
        _statusLabel.Text = daemonAlive
            ? $"focusstaged 运行中 · 当前 {_stages.Count} 个后台/桌面舞台"
            : "focusstaged 未运行（双击任意应用时会自动拉起）";
        _btnStartStopDaemon.Text = daemonAlive ? "停止守护" : "启动守护";
        _btnTakeoverAll.Enabled = daemonAlive && _stages.Count > 0;
        _btnStagesToggle.Text = $"运行中舞台 ({_stages.Count})";
        if (_stagesVisible)
        {
            RenderStages();
        }
    }

    private void ToggleStagesDrawer()
    {
        _stagesVisible = !_stagesVisible;
        _stagesDrawer.Visible = _stagesVisible;
        if (_stagesVisible)
        {
            RenderStages();
        }
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
                    SetFooter($"读取 Linux 应用失败: {err}");
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
                if (listChanged && !forceRender)
                {
                    SetFooter($"已自动检测到应用更新，当前共 {_apps.Count} 个应用/安装包。");
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
        var filter = _searchBox.Text.Trim();
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
                _homeGrid.Controls.Add(MakeHintLabel($"未找到匹配 “{filter}” 的应用。"));
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
            _homeGrid.Controls.Add(MakeHintLabel("暂未发现 Linux 应用，可点击右上角“应用中心”安装，或直接在浏览器下载 .AppImage / .deb。"));
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
        Padding = new Padding(12),
        ForeColor = Color.FromArgb(100, 116, 139)
    };

    private Control BuildFolderTile(LauncherFolder folder)
    {
        var tileKey = "folder:" + folder.Id;
        var tile = new DesktopIconTile
        {
            TileKey = tileKey,
            TitleText = folder.Name,
            SubBadgeText = folder.AppIds.Count.ToString(),
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
                SetFooter($"已将应用移入文件夹 “{folder.Name}”。");
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
                    SetFooter($"已创建文件夹 “{folder.Name}”。");
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

    private void OpenFolderOverlay(string folderId)
    {
        _openFolderId = folderId;
        RenderFolderModal();
        _folderOverlay.CaptureBackdropFrom(_homeGrid);
        _folderOverlay.Visible = true;
        _folderOverlay.BringToFront();
        CenterFolderCard();
    }

    private void CloseFolderOverlay()
    {
        _openFolderId = null;
        _folderOverlay.Visible = false;
    }

    private void CenterFolderCard()
    {
        if (_folderOverlay.Width <= 0 || _folderOverlay.Height <= 0) return;
        int w = Math.Min(600, Math.Max(360, _folderOverlay.Width - 100));
        int h = Math.Min(450, Math.Max(280, _folderOverlay.Height - 90));
        _folderCard.Size = new Size(w, h);
        _folderCard.Location = new Point(
            (_folderOverlay.Width - w) / 2,
            (_folderOverlay.Height - h) / 2);
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
            _folderGrid.Controls.Add(MakeHintLabel("文件夹是空的，可拖拽应用到此文件夹或右键应用“移动到文件夹”。"));
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
                    SetFooter($"已将 “{app.DisplayName}” 添加到 Windows 开始菜单（可右键固定到开始屏幕或任务栏）。");
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
                    SetFooter($"已创建桌面快捷方式：{Path.GetFileName(path)}");
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
                SetFooter($"已将 “{app.DisplayName}” 移入 “{targetFolder.Name}”。");
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
            SetFooter($"已创建文件夹 “{folder.Name}” 并移入 “{app.DisplayName}”。");
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
                SetFooter($"已将 “{app.DisplayName}” 移回主桌面。");
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
            SetFooter($"已将文件夹 “{folder.Name}” 内 {added} 个应用添加到 Windows 开始菜单。");
        };

        var dissolveItem = new ToolStripMenuItem("解散文件夹（应用移回主桌面）");
        dissolveItem.Click += (_, _) =>
        {
            _layout.DissolveFolder(folder.Id);
            if (_openFolderId == folder.Id) CloseFolderOverlay();
            RenderHomeGrid();
            SetFooter($"已解散文件夹 “{folder.Name}”。");
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
            SetFooter("已按类别自动整理桌面文件夹。");
        };

        var appCenterItem = new ToolStripMenuItem("打开 Ubuntu 应用中心");
        appCenterItem.Click += async (_, _) => await OpenOrInstallAppCenterAsync();

        var customCmdItem = new ToolStripMenuItem("运行自定义命令...");
        customCmdItem.Click += (_, _) => _ctx.PromptLaunchCustom(this);

        var refreshItem = new ToolStripMenuItem("刷新应用列表");
        refreshItem.Click += async (_, _) => await RefreshAppsAsync(forceRender: true);

        menu.Items.AddRange(new ToolStripItem[]
        {
            newFolderItem, organizeItem, new ToolStripSeparator(), appCenterItem, customCmdItem, refreshItem
        });
        menu.Show(anchor, loc);
    }

    private void PromptCreateFolder()
    {
        var name = PromptDialog.Show(this, "新建文件夹", "请输入文件夹名称：", "新建文件夹");
        if (name is null) return;
        _layout.CreateFolder(name);
        RenderHomeGrid();
        SetFooter($"已创建文件夹 “{name}”，可将应用拖入或右键移入。");
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

            SetFooter($"正在安装 {app.DisplayName}...");
            var (ok, msg) = await ShortcutManager.InstallDebPackageAsync(_distro, app.Cmd);
            SetFooter(ok ? msg : $"安装失败: {msg}");
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

        SetFooter($"正在启动 {app.DisplayName}...");
        var args = new List<string> { "launch", "--ensure-daemon", "--wm-class", app.WmClass, app.Cmd };
        if (app.Args.Count > 0)
        {
            args.Add("--");
            args.AddRange(app.Args);
        }
        var (launchOk, launchMsg) = await _ctx.RunControlWithFeedbackAsync(args.ToArray());
        if (launchOk)
        {
            SetFooter($"已启动 {app.DisplayName}。");
        }
        else
        {
            SetFooter($"启动 {app.DisplayName} 失败: {launchMsg}");
            MessageBox.Show(this, $"启动 {app.DisplayName} 失败：\n{launchMsg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
    }

    private async Task OpenOrInstallAppCenterAsync()
    {
        _btnAppCenter.Enabled = false;
        try
        {
            SetFooter("正在查找 Ubuntu 应用中心...");
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
                    SetFooter("已取消安装应用中心。");
                    return;
                }

                SetFooter("正在安装 Ubuntu 应用中心（首次约需 1-2 分钟）...");
                var (instOk, instCmd, instMsg) = await ShortcutManager.InstallAppCenterAsync(_distro);
                if (!instOk)
                {
                    SetFooter($"安装应用中心失败: {instMsg}");
                    MessageBox.Show(this, $"安装应用中心失败：\n{instMsg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
                    return;
                }
                cmd = instCmd;
                await RefreshAppsAsync(forceRender: true);
            }

            SetFooter($"正在启动应用中心 ({cmd})...");
            var (ok, msg) = await _ctx.RunControlWithFeedbackAsync("launch", "--ensure-daemon", "--wm-class", $"focusstage-{cmd}", cmd);
            SetFooter(ok ? $"已打开应用中心 ({cmd})，在其中安装的应用会自动同步到这里。" : $"启动应用中心失败: {msg}");
            if (!ok)
            {
                MessageBox.Show(this, $"启动应用中心失败：\n{msg}", "FocusStage", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
        }
        finally
        {
            _btnAppCenter.Enabled = true;
        }
    }

    private async Task HandleSyncStartMenuAsync()
    {
        _btnSyncStartMenu.Enabled = false;
        SetFooter("正在同步所有 Linux 应用到 Windows 开始菜单...");
        try
        {
            var (ok, msg, _) = await ShortcutManager.SyncAllAsync(_distro, createDesktopForLauncher: true, createStartupTray: _chkStartup.Checked);
            SetFooter(msg);
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

    private void RenderStages()
    {
        _stagesPanel.SuspendLayout();
        _stagesPanel.Controls.Clear();

        if (_stages.Count == 0)
        {
            _stagesPanel.Controls.Add(MakeHintLabel("当前没有运行中的 Stage。"));
            _stagesPanel.ResumeLayout();
            return;
        }

        int cardWidth = Math.Max(240, _stagesPanel.ClientSize.Width - 16);
        foreach (var stage in _stages)
        {
            var card = new Panel
            {
                Width = cardWidth,
                Height = 84,
                BackColor = Color.White,
                Margin = new Padding(0, 0, 0, 8),
                Padding = new Padding(10)
            };
            card.Paint += (_, e) =>
            {
                using var pen = new Pen(Color.FromArgb(222, 228, 236));
                e.Graphics.DrawRectangle(pen, 0, 0, card.Width - 1, card.Height - 1);
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
                Font = new Font("Microsoft YaHei UI", 9.0f, FontStyle.Bold),
                Location = new Point(10, 8),
                Size = new Size(cardWidth - 20, 20),
                AutoEllipsis = true
            };
            var lblMeta = new Label
            {
                Text = $"{dispTag} · {ownerTag} · {stage.Kind}",
                Font = new Font("Microsoft YaHei UI", 8.1f),
                ForeColor = Color.FromArgb(100, 116, 139),
                Location = new Point(10, 29),
                Size = new Size(cardWidth - 20, 18),
                AutoEllipsis = true
            };

            var actions = new FlowLayoutPanel
            {
                Location = new Point(8, 50),
                Size = new Size(cardWidth - 16, 28),
                WrapContents = false
            };

            var btnTake = MakeMiniBtn("接管");
            btnTake.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync("takeover", stage.StageId);

            var btnVis = MakeMiniBtn(stage.Display == "desktop" ? "隐藏" : "显示");
            btnVis.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync(stage.Display == "desktop" ? "hide" : "show", stage.StageId);

            var btnClose = MakeMiniBtn("关闭");
            btnClose.ForeColor = Color.FromArgb(185, 28, 28);
            btnClose.Click += async (_, _) => await _ctx.RunControlAndRefreshAsync("close", stage.StageId);

            actions.Controls.AddRange(new Control[] { btnTake, btnVis, btnClose });
            card.Controls.AddRange(new Control[] { lblTitle, lblMeta, actions });
            _stagesPanel.Controls.Add(card);
        }
        _stagesPanel.ResumeLayout();
    }

    private static Button MakeMiniBtn(string text) => new()
    {
        Text = text,
        AutoSize = true,
        Height = 24,
        FlatStyle = FlatStyle.System,
        Font = new Font("Microsoft YaHei UI", 8.1f),
        Margin = new Padding(0, 0, 6, 0)
    };

    protected override void Dispose(bool disposing)
    {
        if (disposing)
        {
            _autoDiscoverTimer.Stop();
            _autoDiscoverTimer.Dispose();
            _debounceTimer.Stop();
            _debounceTimer.Dispose();
            _downloadsWatcher?.Dispose();
            foreach (var img in _iconCache.Values) img.Dispose();
            _iconCache.Clear();
        }
        base.Dispose(disposing);
    }

    private sealed record DragAppPayload(string AppId, string? FromFolderId);

    private sealed class BufferedFlowPanel : FlowLayoutPanel
    {
        public BufferedFlowPanel()
        {
            DoubleBuffered = true;
            SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint, true);
        }
    }

    private sealed class RoundedPanel : Panel
    {
        public int CornerRadius { get; set; } = 18;
        public Color FillColor { get; set; } = Color.White;
        public Color BorderColor { get; set; } = Color.FromArgb(215, 222, 232);

        public RoundedPanel()
        {
            DoubleBuffered = true;
            SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint | ControlStyles.UserPaint, true);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
            var rect = new Rectangle(1, 1, Width - 3, Height - 3);
            using var path = BuildRoundPath(rect, CornerRadius);
            using var fill = new SolidBrush(FillColor);
            using var pen = new Pen(BorderColor, 1.2f);
            e.Graphics.FillPath(fill, path);
            e.Graphics.DrawPath(pen, path);
            base.OnPaint(e);
        }
    }

    private sealed class DimOverlayPanel : Panel
    {
        private Bitmap? _backdrop;
        public event EventHandler? BackdropClicked;

        public DimOverlayPanel()
        {
            DoubleBuffered = true;
            SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint | ControlStyles.UserPaint, true);
            MouseDown += (_, _) => BackdropClicked?.Invoke(this, EventArgs.Empty);
        }

        public void CaptureBackdropFrom(Control source)
        {
            _backdrop?.Dispose();
            _backdrop = null;
            if (source.Width <= 0 || source.Height <= 0) return;
            try
            {
                var bmp = new Bitmap(source.Width, source.Height);
                source.DrawToBitmap(bmp, new Rectangle(0, 0, source.Width, source.Height));
                _backdrop = bmp;
            }
            catch
            {
                _backdrop = null;
            }
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            if (_backdrop is not null)
            {
                e.Graphics.DrawImage(_backdrop, new Rectangle(0, 0, Width, Height));
            }
            else
            {
                e.Graphics.Clear(Color.FromArgb(220, 228, 238));
            }
            using var dim = new SolidBrush(Color.FromArgb(115, 15, 23, 42));
            e.Graphics.FillRectangle(dim, ClientRectangle);
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                _backdrop?.Dispose();
                _backdrop = null;
            }
            base.Dispose(disposing);
        }
    }

    private sealed class DesktopIconTile : Control
    {
        private bool _hovered;
        private bool _isSelected;
        private bool _isDropTarget;

        public string TileKey { get; set; } = "";
        public string TitleText { get; set; } = "";
        public string? SubBadgeText { get; set; }
        public Image? AppIcon { get; set; }
        public bool IsFolder { get; set; }
        public List<Image> FolderPreviewIcons { get; set; } = new();

        public bool IsSelected
        {
            get => _isSelected;
            set { _isSelected = value; Invalidate(); }
        }

        public bool IsDropTarget
        {
            get => _isDropTarget;
            set { _isDropTarget = value; Invalidate(); }
        }

        public DesktopIconTile()
        {
            Size = new Size(108, 114);
            Margin = new Padding(8, 8, 8, 10);
            Cursor = Cursors.Hand;
            SetStyle(
                ControlStyles.OptimizedDoubleBuffer
                | ControlStyles.AllPaintingInWmPaint
                | ControlStyles.UserPaint
                | ControlStyles.SupportsTransparentBackColor
                | ControlStyles.StandardDoubleClick,
                true);
            BackColor = Color.Transparent;
        }

        protected override void OnMouseEnter(EventArgs e)
        {
            _hovered = true;
            Invalidate();
            base.OnMouseEnter(e);
        }

        protected override void OnMouseLeave(EventArgs e)
        {
            _hovered = false;
            Invalidate();
            base.OnMouseLeave(e);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            var g = e.Graphics;
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.InterpolationMode = InterpolationMode.HighQualityBicubic;

            // Outer hover / selection pill
            if (_isSelected || _hovered || _isDropTarget)
            {
                var tileRect = new Rectangle(2, 2, Width - 5, Height - 5);
                using var tilePath = BuildRoundPath(tileRect, 14);
                var bg = _isDropTarget
                    ? Color.FromArgb(219, 234, 254)
                    : _isSelected
                        ? Color.FromArgb(224, 236, 255)
                        : Color.FromArgb(232, 238, 247);
                using var bgBrush = new SolidBrush(bg);
                g.FillPath(bgBrush, tilePath);

                if (_isSelected || _isDropTarget)
                {
                    using var borderPen = new Pen(Color.FromArgb(59, 130, 246), 1.6f);
                    g.DrawPath(borderPen, tilePath);
                }
            }

            // Icon squircle (64x64 centered at top)
            var boxRect = new Rectangle((Width - 64) / 2, 8, 64, 64);
            using var boxPath = BuildRoundPath(boxRect, 16);

            if (IsFolder)
            {
                using var folderBg = new SolidBrush(Color.FromArgb(226, 234, 246));
                using var folderBorder = new Pen(Color.FromArgb(191, 206, 226), 1.2f);
                g.FillPath(folderBg, boxPath);
                g.DrawPath(folderBorder, boxPath);

                // Draw 2x2 mini icons inside folder squircle
                int miniSize = 22;
                int gap = 4;
                int startX = boxRect.X + (boxRect.Width - (miniSize * 2 + gap)) / 2;
                int startY = boxRect.Y + (boxRect.Height - (miniSize * 2 + gap)) / 2;
                for (int idx = 0; idx < 4; idx++)
                {
                    int col = idx % 2;
                    int row = idx / 2;
                    var mr = new Rectangle(startX + col * (miniSize + gap), startY + row * (miniSize + gap), miniSize, miniSize);
                    if (idx < FolderPreviewIcons.Count)
                    {
                        g.DrawImage(FolderPreviewIcons[idx], mr);
                    }
                    else
                    {
                        using var slotPath = BuildRoundPath(mr, 5);
                        using var slotBrush = new SolidBrush(Color.FromArgb(207, 218, 234));
                        g.FillPath(slotBrush, slotPath);
                    }
                }
            }
            else
            {
                using var appBg = new SolidBrush(Color.White);
                using var appBorder = new Pen(Color.FromArgb(218, 226, 237), 1.0f);
                g.FillPath(appBg, boxPath);
                g.DrawPath(appBorder, boxPath);

                if (AppIcon is not null)
                {
                    var iconRect = new Rectangle(boxRect.X + 10, boxRect.Y + 10, 44, 44);
                    g.DrawImage(AppIcon, iconRect);
                }
            }

            // Optional mini badge (count for folders, DEB/APP for downloads)
            if (!string.IsNullOrWhiteSpace(SubBadgeText))
            {
                using var badgeFont = new Font("Microsoft YaHei UI", 7.0f, FontStyle.Bold);
                var sz = g.MeasureString(SubBadgeText, badgeFont);
                int bw = Math.Max(18, (int)Math.Ceiling(sz.Width) + 8);
                int bh = 16;
                var br = new Rectangle(boxRect.Right - bw + 4, boxRect.Top - 2, bw, bh);
                using var bp = BuildRoundPath(br, 8);
                var badgeColor = IsFolder
                    ? Color.FromArgb(71, 85, 105)
                    : SubBadgeText == "DEB"
                        ? Color.FromArgb(217, 119, 6)
                        : Color.FromArgb(16, 163, 127);
                using var bb = new SolidBrush(badgeColor);
                using var bf = new SolidBrush(Color.White);
                using var bsf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
                g.FillPath(bb, bp);
                g.DrawString(SubBadgeText, badgeFont, bf, br, bsf);
            }

            // Label underneath icon
            var textRect = new Rectangle(4, boxRect.Bottom + 5, Width - 8, Height - boxRect.Bottom - 7);
            using var labelFont = new Font("Microsoft YaHei UI", 8.5f, FontStyle.Regular);
            using var labelBrush = new SolidBrush(Color.FromArgb(30, 41, 59));
            using var sf = new StringFormat
            {
                Alignment = StringAlignment.Center,
                LineAlignment = StringAlignment.Near,
                Trimming = StringTrimming.EllipsisCharacter
            };
            g.DrawString(TitleText, labelFont, labelBrush, textRect, sf);
        }
    }

    private static GraphicsPath BuildRoundPath(Rectangle rect, int radius)
    {
        int d = Math.Min(radius * 2, Math.Min(rect.Width, rect.Height));
        var path = new GraphicsPath();
        if (d <= 0)
        {
            path.AddRectangle(rect);
            return path;
        }
        path.AddArc(rect.X, rect.Y, d, d, 180, 90);
        path.AddArc(rect.Right - d, rect.Y, d, d, 270, 90);
        path.AddArc(rect.Right - d, rect.Bottom - d, d, d, 0, 90);
        path.AddArc(rect.X, rect.Bottom - d, d, d, 90, 90);
        path.CloseFigure();
        return path;
    }

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

            var lbl = new Label
            {
                Text = prompt,
                AutoSize = true
            };
            var tb = new TextBox
            {
                Text = defaultValue,
                Dock = DockStyle.Fill
            };
            tb.SelectAll();

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
                Text = "取消",
                AutoSize = true,
                DialogResult = DialogResult.Cancel
            };
            var ok = new Button
            {
                Text = "确定",
                AutoSize = true,
                DialogResult = DialogResult.OK
            };
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
