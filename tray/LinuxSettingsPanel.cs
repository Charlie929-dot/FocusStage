namespace FocusStageTray;

internal sealed class LinuxSettingsPanel : TransparentPanel
{
    private readonly FocusStageTrayContext _context;
    private readonly Action<string> _toast;
    private readonly ComboBox _browser = ChoiceBox("默认浏览器");
    private readonly ComboBox _terminal = ChoiceBox("默认终端");
    private readonly Label _hint = new()
    {
        Dock = DockStyle.Top, Height = 34, ForeColor = Color.FromArgb(71, 85, 105),
        Text = "vscode:// 等回调需要使用 Linux 浏览器。"
    };
    private readonly Label _status = new()
    {
        Dock = DockStyle.Fill, TextAlign = ContentAlignment.MiddleLeft, AutoEllipsis = true,
        ForeColor = Color.FromArgb(71, 85, 105), Text = "正在读取协议关联…"
    };
    private readonly PillButton _sync = new("同步协议关联") { Enabled = false, Width = 112, Height = 29, Margin = new Padding(6, 1, 0, 0) };
    private readonly ToolTip _details = new();
    private bool _busy;

    internal LinuxSettingsPanel(FocusStageTrayContext context, Action<string> toast)
    {
        _context = context;
        _toast = toast;
        Dock = DockStyle.Fill;
        Font = new Font("Microsoft YaHei UI", 8.6f);
        var choices = new TableLayoutPanel
        {
            Dock = DockStyle.Top, Height = 62, ColumnCount = 2, RowCount = 2, BackColor = Color.Transparent,
            TabIndex = 0
        };
        choices.ColumnStyles.Add(new ColumnStyle(SizeType.Absolute, 91));
        choices.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
        choices.RowStyles.Add(new RowStyle(SizeType.Percent, 50));
        choices.RowStyles.Add(new RowStyle(SizeType.Percent, 50));
        choices.Controls.Add(ChoiceLabel("默认浏览器"), 0, 0);
        choices.Controls.Add(_browser, 1, 0);
        choices.Controls.Add(ChoiceLabel("默认终端"), 0, 1);
        choices.Controls.Add(_terminal, 1, 1);

        var association = new TableLayoutPanel
        {
            Dock = DockStyle.Top, Height = 34, ColumnCount = 2, RowCount = 1, BackColor = Color.Transparent,
            TabIndex = 1
        };
        association.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
        association.ColumnStyles.Add(new ColumnStyle(SizeType.Absolute, 118));
        association.Controls.Add(_status, 0, 0);
        association.Controls.Add(_sync, 1, 0);
        Controls.Add(association);
        Controls.Add(_hint);
        Controls.Add(choices);
        _hint.Visible = false;

        _browser.SelectionChangeCommitted += async (_, _) => await SetChoiceAsync(_browser, "set-browser", "已设置默认浏览器");
        _terminal.SelectionChangeCommitted += async (_, _) => await SetChoiceAsync(_terminal, "set-terminal", "已设置默认终端");
        _sync.Click += async (_, _) => await RefreshAsync(["sync-handlers"], "已同步协议关联");
    }

    internal Task RefreshAsync() => RefreshAsync(["get"], null);

    private static ComboBox ChoiceBox(string label) => new ScrollSafeChoiceBox
    {
        Dock = DockStyle.Top, Enabled = false, AccessibleName = label, DropDownStyle = ComboBoxStyle.DropDownList,
        DisplayMember = nameof(LinuxAppChoice.Name), Margin = new Padding(0, 2, 0, 0)
    };

    private sealed class ScrollSafeChoiceBox : ComboBox
    {
        private const int WmMouseWheel = 0x020A;

        protected override void WndProc(ref Message m)
        {
            if (m.Msg == WmMouseWheel && !DroppedDown)
            {
                // The native combo box commits a new value even when scrolled without focus.
                for (Control? parent = Parent; parent is not null; parent = parent.Parent)
                {
                    if (parent is ScrollableControl { AutoScroll: true })
                    {
                        m.Result = NativeHostInterop.SendMessage(parent.Handle, m.Msg, m.WParam, m.LParam);
                        break;
                    }
                }
                return;
            }
            base.WndProc(ref m);
        }
    }

    private static Label ChoiceLabel(string label) => new()
    {
        Dock = DockStyle.Fill, Text = label, TextAlign = ContentAlignment.MiddleLeft,
        Margin = Padding.Empty, ForeColor = Color.FromArgb(30, 41, 59)
    };

    private static void LoadChoices(ComboBox box, List<LinuxAppChoice> choices, string? selected)
    {
        box.BeginUpdate();
        try
        {
            box.Items.Clear();
            box.Items.AddRange(choices.ToArray());
            box.SelectedIndex = choices.FindIndex(choice => choice.Id == selected);
        }
        finally { box.EndUpdate(); }
    }

    private async Task SetChoiceAsync(ComboBox box, string command, string message)
    {
        if (box.SelectedItem is not LinuxAppChoice choice) return;
        if (!await RefreshAsync([command, choice.Id], message))
            await RefreshAsync();
    }

    private async Task<bool> RefreshAsync(string[] args, string? success)
    {
        if (_busy || IsDisposed) return false;
        _busy = true;
        _browser.Enabled = _terminal.Enabled = _sync.Enabled = false;
        try
        {
            var settings = await _context.LinuxSettingsAsync(args);
            if (IsDisposed) return false;
            LoadChoices(_browser, settings.Browsers, settings.Browser);
            LoadChoices(_terminal, settings.Terminals, settings.Terminal);
            _hint.Visible = settings.Browser == "windows-host";
            string[] protocols = settings.Protocols.Keys
                .OrderBy(scheme => scheme is "http" or "https" ? 0 : scheme == "vscode" ? 1 : 2)
                .ThenBy(scheme => scheme, StringComparer.Ordinal).ToArray();
            string status = !settings.XdgOpenInstalled ? "缺少 xdg-open" : !settings.MimeappsSynced ? "关联未同步" : "已就绪";
            _status.Text = protocols.Length == 0 ? status : $"{status}：{string.Join(" / ", protocols.Take(3))}";
            _details.SetToolTip(_status, $"已关联协议：{(protocols.Length == 0 ? "无" : string.Join(" / ", protocols))}\nxdg-open：{(settings.XdgOpenInstalled ? "已安装" : "未安装")}\n桌面门户：{(settings.PortalActive ? "运行中" : "未运行")}\n浏览器关联：{(settings.MimeappsSynced ? "已同步" : "未同步")}");
            _browser.Enabled = _browser.Items.Count != 0;
            _terminal.Enabled = _terminal.Items.Count != 0;
            _sync.Enabled = true;
            if (success is not null) _toast(success);
            return true;
        }
        catch (Exception ex)
        {
            if (!IsDisposed)
            {
                _status.Text = "无法读取 Linux 设置";
                _details.SetToolTip(_status, ex.Message);
                _sync.Enabled = true;
                if (success is not null) _toast($"Linux 设置未保存：{ex.Message}");
            }
            return false;
        }
        finally { _busy = false; }
    }

    protected override void Dispose(bool disposing)
    {
        if (disposing) _details.Dispose();
        base.Dispose(disposing);
    }
}
