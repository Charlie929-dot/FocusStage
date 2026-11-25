using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Text;
using System.Runtime.InteropServices;
using static FocusStageTray.NativeHostInterop;

namespace FocusStageTray;

internal sealed record AuthBeginRequest(
    string Cookie,
    string ActionId,
    string Message,
    string? IconPng,
    string? ProgramName,
    string? CommandLine,
    string User,
    IReadOnlyList<string> Users,
    string Prompt,
    bool EchoOn,
    string? Info,
    string? Error);

internal sealed record AuthPromptRequest(
    string Cookie,
    string Prompt,
    bool EchoOn,
    string? Info,
    string? Error,
    bool Retry);

internal sealed record AuthEndRequest(
    string Cookie,
    bool Succeeded,
    string? Message);

internal sealed class ElevationDialogHost : IDisposable
{
    private readonly Action<string, string, string> _onSubmit;
    private readonly Action<string> _onCancel;
    private readonly object _lock = new();
    private readonly Dictionary<string, NativeElevationDialog> _dialogs = new(StringComparer.Ordinal);
    private Thread? _uiThread;
    private Control? _marshaler;
    private ApplicationContext? _appContext;
    private ManualResetEventSlim? _ready;
    private bool _disposed;

    public ElevationDialogHost(
        Action<string, string, string> onSubmit,
        Action<string> onCancel)
    {
        _onSubmit = onSubmit;
        _onCancel = onCancel;
    }

    public void Begin(AuthBeginRequest request)
    {
        PostToUi(() =>
        {
            if (_dialogs.TryGetValue(request.Cookie, out var existing))
            {
                _dialogs.Remove(request.Cookie);
                existing.CloseWithoutCancel();
                existing.Dispose();
            }

            var dialog = new NativeElevationDialog(
                request,
                onSubmit: (user, response) => _onSubmit(request.Cookie, user, response),
                onCancel: () =>
                {
                    _dialogs.Remove(request.Cookie);
                    _onCancel(request.Cookie);
                },
                onClosed: () => _dialogs.Remove(request.Cookie));
            _dialogs[request.Cookie] = dialog;
            dialog.ShowAndFocus();
        });
    }

    public void Prompt(AuthPromptRequest request)
    {
        PostToUi(() =>
        {
            if (_dialogs.TryGetValue(request.Cookie, out var dialog) && !dialog.IsDisposed)
            {
                dialog.ApplyPrompt(request);
            }
        });
    }

    public void End(AuthEndRequest request)
    {
        PostToUi(() =>
        {
            if (_dialogs.TryGetValue(request.Cookie, out var dialog))
            {
                _dialogs.Remove(request.Cookie);
                if (!dialog.IsDisposed)
                {
                    dialog.CloseWithoutCancel();
                    dialog.Dispose();
                }
            }
        });
    }

    private void PostToUi(Action action)
    {
        Control? marshaler;
        lock (_lock)
        {
            if (_disposed) return;
            EnsureUiThreadLocked();
            marshaler = _marshaler;
        }
        if (marshaler is null || marshaler.IsDisposed) return;
        try
        {
            marshaler.BeginInvoke(() =>
            {
                if (_disposed) return;
                try
                {
                    action();
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine($"FocusStage elevation dialog action error: {ex}");
                }
            });
        }
        catch (ObjectDisposedException)
        {
        }
        catch (InvalidOperationException)
        {
        }
    }

    private void EnsureUiThreadLocked()
    {
        if (_uiThread is not null) return;
        _ready = new ManualResetEventSlim(false);
        _uiThread = new Thread(() =>
        {
            try
            {
                var marshaler = new Control();
                marshaler.CreateControl();
                _ = marshaler.Handle;
                var ctx = new ApplicationContext();
                _marshaler = marshaler;
                _appContext = ctx;
                _ready.Set();
                Application.Run(ctx);
                marshaler.Dispose();
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"FocusStage elevation UI thread error: {ex}");
                _ready.Set();
            }
        })
        {
            IsBackground = true,
            Name = "FocusStage-ElevationUI",
        };
        _uiThread.SetApartmentState(ApartmentState.STA);
        _uiThread.Start();
        _ready.Wait();
    }

    public void Dispose()
    {
        Thread? thread;
        ApplicationContext? ctx;
        Control? marshaler;
        lock (_lock)
        {
            if (_disposed) return;
            _disposed = true;
            thread = _uiThread;
            ctx = _appContext;
            marshaler = _marshaler;
        }

        if (marshaler is not null && !marshaler.IsDisposed && marshaler.IsHandleCreated)
        {
            try
            {
                marshaler.BeginInvoke(() =>
                {
                    foreach (var dialog in _dialogs.Values.ToList())
                    {
                        if (!dialog.IsDisposed)
                        {
                            dialog.CloseWithoutCancel();
                            dialog.Dispose();
                        }
                    }
                    _dialogs.Clear();
                    ctx?.ExitThread();
                });
            }
            catch
            {
                ctx?.ExitThread();
            }
        }
        if (thread is not null && thread.IsAlive)
        {
            thread.Join(1000);
        }
        _ready?.Dispose();
    }
}

internal sealed class NativeElevationDialog : Form
{
    private const int DwmwaUseImmersiveDarkMode = 20;
    private const int WmNcLButtonDown = 0x00A1;
    private const int HtCaption = 2;

    [DllImport("user32.dll")]
    private static extern bool DestroyIcon(IntPtr hIcon);

    private readonly Action<string, string> _onSubmit;
    private readonly Action _onCancel;
    private readonly Action _onClosed;

    private readonly ThemePalette _palette;
    private readonly string _actionId;
    private readonly string _message;
    private readonly string? _programName;
    private readonly string? _commandLine;
    private readonly IReadOnlyList<string> _users;
    private readonly Bitmap? _appIconBitmap;
    private readonly Icon? _windowIcon;
    private IntPtr _windowHicon;
    private Font? _ownedInputFont;

    private string _selectedUser;
    private string _promptLabel;
    private bool _echoOn;
    private string? _bannerMessage;
    private bool _bannerIsError;
    private bool _revealPassword;
    private bool _detailsExpanded;
    private bool _verifying;
    private bool _capsLockOn;
    private bool _suppressCancel;
    private bool _hasSubmittedOrCanceled;
    private bool _positioned;

    private readonly TextBox _inputBox;
    private readonly System.Windows.Forms.Timer _shakeTimer;
    private int _shakeTick;
    private int _shakeBaseX;
    private int _shakeOffset;

    private double _scale = 1.0;

    private Rectangle _closeBtnRect;
    private Rectangle _userPillRect;
    private Rectangle _detailsToggleRect;
    private Rectangle _detailsBodyRect;
    private Rectangle _inputFrameRect;
    private Rectangle _eyeBtnRect;
    private Rectangle _cancelBtnRect;
    private Rectangle _submitBtnRect;
    private Rectangle _headerDragRect;

    private HitZone _hoverZone = HitZone.None;
    private HitZone _pressedZone = HitZone.None;

    private enum HitZone
    {
        None,
        Close,
        UserPill,
        DetailsToggle,
        Eye,
        Cancel,
        Submit,
    }

    private sealed record ThemePalette(
        bool IsDark,
        Color Surface,
        Color Subtle,
        Color InputBg,
        Color HoverBg,
        Color PressedBg,
        Color Border,
        Color BorderStrong,
        Color Ink,
        Color InkSecondary,
        Color InkMuted,
        Color Accent,
        Color AccentHover,
        Color AccentPressed,
        Color AccentSoft,
        Color Danger,
        Color DangerSoft,
        Color Amber,
        Color AmberSoft);

    public NativeElevationDialog(
        AuthBeginRequest request,
        Action<string, string> onSubmit,
        Action onCancel,
        Action onClosed)
    {
        _onSubmit = onSubmit;
        _onCancel = onCancel;
        _onClosed = onClosed;

        _palette = BuildPalette();
        _actionId = request.ActionId;
        _message = string.IsNullOrWhiteSpace(request.Message)
            ? "此操作需要验证您的 Linux 用户密码以继续。"
            : request.Message.Trim();
        _programName = request.ProgramName;
        _commandLine = request.CommandLine;
        _users = request.Users.Count > 0 ? request.Users : [request.User];
        _selectedUser = string.IsNullOrWhiteSpace(request.User) ? _users[0] : request.User;
        _promptLabel = FormatPromptLabel(request.Prompt, request.EchoOn);
        _echoOn = request.EchoOn;
        SetBanner(request.Error, request.Info);

        _appIconBitmap = DecodePngBitmap(request.IconPng);
        using (var brand = ShortcutManager.RenderBrandBitmap(64))
        {
            _windowHicon = brand.GetHicon();
            _windowIcon = Icon.FromHandle(_windowHicon);
        }

        Text = _actionId == "io.focusstage.launcher.verify"
            ? "FocusStage 身份验证"
            : "FocusStage 权限验证";
        Icon = _windowIcon;
        FormBorderStyle = FormBorderStyle.None;
        StartPosition = FormStartPosition.Manual;
        ShowInTaskbar = true;
        TopMost = true;
        KeyPreview = true;
        AutoScaleMode = AutoScaleMode.None;
        DoubleBuffered = true;
        BackColor = _palette.Surface;
        ForeColor = _palette.Ink;

        _inputBox = new TextBox
        {
            BorderStyle = BorderStyle.None,
            BackColor = _palette.InputBg,
            ForeColor = _palette.Ink,
            UseSystemPasswordChar = !_echoOn,
            ShortcutsEnabled = true,
            TabIndex = 0,
        };
        _inputBox.GotFocus += (_, _) =>
        {
            UpdateCapsLockState();
            Invalidate(_inputFrameRect);
        };
        _inputBox.LostFocus += (_, _) => Invalidate(_inputFrameRect);
        _inputBox.KeyDown += OnInputKeyDown;
        _inputBox.KeyUp += (_, _) => UpdateCapsLockState();
        Controls.Add(_inputBox);

        _shakeTimer = new System.Windows.Forms.Timer { Interval = 16 };
        _shakeTimer.Tick += OnShakeTick;

        SetStyle(
            ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.ResizeRedraw,
            true);
    }

    protected override CreateParams CreateParams
    {
        get
        {
            var cp = base.CreateParams;
            cp.Style |= Win11WindowHelper.WS_THICKFRAME | Win11WindowHelper.WS_SYSMENU;
            return cp;
        }
    }

    protected override void OnHandleCreated(EventArgs e)
    {
        base.OnHandleCreated(e);
        _scale = (GetDpiForWindow(Handle) is uint dpi and > 0 ? dpi : 96u) / 96.0;
        int dark = _palette.IsDark ? 1 : 0;
        _ = DwmSetWindowAttribute(Handle, DwmwaUseImmersiveDarkMode, in dark, sizeof(int));
        Win11WindowHelper.ApplyWin11Chrome(Handle);
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == Win11WindowHelper.WM_NCCALCSIZE && Win11WindowHelper.HandleNcCalcSize(Handle, ref m))
        {
            return;
        }
        if (m.Msg == 0x0084) // WM_NCHITTEST: keep modal card fixed-size while preserving DWM shadow
        {
            m.Result = (IntPtr)HtClient;
            return;
        }
        if (m.Msg == 0x02E0) // WM_DPICHANGED
        {
            _scale = (LowWord(m.WParam) is int dpi and > 0 ? dpi : 96) / 96.0;
            UpdateInputFont();
            RecalculateLayout(recenter: false);
        }
        base.WndProc(ref m);
    }

    public void ShowAndFocus()
    {
        UpdateCapsLockState();
        if (!IsHandleCreated) CreateHandle();
        _scale = (GetDpiForWindow(Handle) is uint dpi and > 0 ? dpi : 96u) / 96.0;
        UpdateInputFont();
        RecalculateLayout(recenter: true);
        Show();
        BringToFront();
        SetForegroundWindow(Handle);
        Activate();
        _inputBox.Focus();
        _inputBox.SelectAll();
    }

    public void ApplyPrompt(AuthPromptRequest request)
    {
        _verifying = false;
        _echoOn = request.EchoOn;
        _promptLabel = FormatPromptLabel(request.Prompt, request.EchoOn);
        SetBanner(request.Error, request.Info);
        _inputBox.Enabled = true;
        _inputBox.UseSystemPasswordChar = !_echoOn && !_revealPassword;
        if (request.Retry)
        {
            _inputBox.Text = string.Empty;
        }
        RecalculateLayout(recenter: false);
        Invalidate();
        if (request.Retry)
        {
            StartShake();
        }
        _inputBox.Focus();
    }

    public void CloseWithoutCancel()
    {
        _suppressCancel = true;
        _shakeTimer.Stop();
        _inputBox.Text = string.Empty;
        if (!IsDisposed)
        {
            Close();
        }
    }

    protected override void OnActivated(EventArgs e)
    {
        base.OnActivated(e);
        UpdateCapsLockState();
    }

    protected override void OnKeyDown(KeyEventArgs e)
    {
        UpdateCapsLockState();
        if (e.KeyCode == Keys.Escape)
        {
            e.Handled = true;
            e.SuppressKeyPress = true;
            TriggerCancel();
            return;
        }
        if (e.KeyCode == Keys.Enter && !_verifying)
        {
            e.Handled = true;
            e.SuppressKeyPress = true;
            TriggerSubmit();
            return;
        }
        base.OnKeyDown(e);
    }

    private void OnInputKeyDown(object? sender, KeyEventArgs e)
    {
        UpdateCapsLockState();
        if (e.KeyCode == Keys.Enter)
        {
            e.Handled = true;
            e.SuppressKeyPress = true;
            TriggerSubmit();
        }
        else if (e.KeyCode == Keys.Escape)
        {
            e.Handled = true;
            e.SuppressKeyPress = true;
            TriggerCancel();
        }
    }

    protected override void OnFormClosing(FormClosingEventArgs e)
    {
        _shakeTimer.Stop();
        _inputBox.Text = string.Empty;
        if (!_suppressCancel && !_hasSubmittedOrCanceled)
        {
            _hasSubmittedOrCanceled = true;
            _onCancel();
        }
        base.OnFormClosing(e);
    }

    protected override void OnFormClosed(FormClosedEventArgs e)
    {
        Icon = null;
        _appIconBitmap?.Dispose();
        _windowIcon?.Dispose();
        if (_windowHicon != IntPtr.Zero)
        {
            DestroyIcon(_windowHicon);
            _windowHicon = IntPtr.Zero;
        }
        _inputBox.Font = Font;
        _ownedInputFont?.Dispose();
        _ownedInputFont = null;
        _shakeTimer.Dispose();
        _onClosed();
        base.OnFormClosed(e);
    }

    protected override void OnMouseMove(MouseEventArgs e)
    {
        base.OnMouseMove(e);
        var zone = HitTest(e.Location);
        if (zone != _hoverZone)
        {
            _hoverZone = zone;
            Cursor = zone != HitZone.None ? Cursors.Hand : Cursors.Default;
            Invalidate();
        }
    }

    protected override void OnMouseLeave(EventArgs e)
    {
        base.OnMouseLeave(e);
        if (_hoverZone != HitZone.None || _pressedZone != HitZone.None)
        {
            _hoverZone = HitZone.None;
            _pressedZone = HitZone.None;
            Cursor = Cursors.Default;
            Invalidate();
        }
    }

    protected override void OnMouseDown(MouseEventArgs e)
    {
        base.OnMouseDown(e);
        if (e.Button != MouseButtons.Left) return;

        var zone = HitTest(e.Location);
        if (zone != HitZone.None)
        {
            _pressedZone = zone;
            Invalidate();
            return;
        }

        if (_headerDragRect.Contains(e.Location) || !_inputFrameRect.Contains(e.Location))
        {
            Win11WindowHelper.ReleaseCapture();
            Win11WindowHelper.SendMessage(Handle, WmNcLButtonDown, HtCaption, 0);
        }
    }

    protected override void OnMouseUp(MouseEventArgs e)
    {
        base.OnMouseUp(e);
        if (e.Button != MouseButtons.Left) return;
        var pressed = _pressedZone;
        _pressedZone = HitZone.None;
        Invalidate();

        if (pressed == HitZone.None || pressed != HitTest(e.Location)) return;
        switch (pressed)
        {
            case HitZone.Close:
            case HitZone.Cancel:
                TriggerCancel();
                break;
            case HitZone.Submit:
                TriggerSubmit();
                break;
            case HitZone.Eye:
                if (!_echoOn)
                {
                    _revealPassword = !_revealPassword;
                    _inputBox.UseSystemPasswordChar = !_revealPassword;
                    _inputBox.Focus();
                    _inputBox.SelectionStart = _inputBox.TextLength;
                    Invalidate();
                }
                break;
            case HitZone.DetailsToggle:
                _detailsExpanded = !_detailsExpanded;
                RecalculateLayout(recenter: false);
                Invalidate();
                break;
            case HitZone.UserPill:
                if (_users.Count > 1 && !_verifying)
                {
                    int idx = 0;
                    for (int i = 0; i < _users.Count; i++)
                    {
                        if (_users[i] == _selectedUser)
                        {
                            idx = (i + 1) % _users.Count;
                            break;
                        }
                    }
                    _selectedUser = _users[idx];
                    Invalidate(_userPillRect);
                }
                break;
        }
    }

    private HitZone HitTest(Point pt)
    {
        if (_closeBtnRect.Contains(pt)) return HitZone.Close;
        if (_cancelBtnRect.Contains(pt)) return HitZone.Cancel;
        if (_submitBtnRect.Contains(pt) && !_verifying) return HitZone.Submit;
        if (!_echoOn && _eyeBtnRect.Contains(pt)) return HitZone.Eye;
        if (!_detailsToggleRect.IsEmpty && _detailsToggleRect.Contains(pt)) return HitZone.DetailsToggle;
        if (_users.Count > 1 && _userPillRect.Contains(pt)) return HitZone.UserPill;
        return HitZone.None;
    }

    private void TriggerSubmit()
    {
        if (_verifying || _hasSubmittedOrCanceled) return;
        _verifying = true;
        _bannerMessage = null;
        _bannerIsError = false;
        _inputBox.Enabled = false;
        RecalculateLayout(recenter: false);
        Invalidate();
        _onSubmit(_selectedUser, _inputBox.Text);
    }

    private void TriggerCancel()
    {
        if (_hasSubmittedOrCanceled) return;
        _hasSubmittedOrCanceled = true;
        _suppressCancel = true;
        _shakeTimer.Stop();
        _inputBox.Text = string.Empty;
        _onCancel();
        if (!IsDisposed)
        {
            Close();
        }
    }

    private void StartShake()
    {
        _shakeTimer.Stop();
        _shakeBaseX = Left - _shakeOffset;
        _shakeOffset = 0;
        _shakeTick = 0;
        _shakeTimer.Start();
    }

    private void OnShakeTick(object? sender, EventArgs e)
    {
        _shakeTick++;
        const int totalTicks = 15;
        if (_shakeTick >= totalTicks)
        {
            _shakeTimer.Stop();
            _shakeOffset = 0;
            Left = _shakeBaseX;
            return;
        }
        double t = _shakeTick / (double)totalTicks;
        double damping = 1.0 - t;
        int amplitude = S(8);
        _shakeOffset = (int)Math.Round(Math.Sin(t * Math.PI * 4.0) * amplitude * damping);
        Left = _shakeBaseX + _shakeOffset;
    }

    private void UpdateCapsLockState()
    {
        bool caps = IsKeyLocked(Keys.CapsLock);
        if (caps != _capsLockOn)
        {
            _capsLockOn = caps;
            RecalculateLayout(recenter: false);
            Invalidate();
        }
    }

    private void SetBanner(string? error, string? info)
    {
        if (!string.IsNullOrWhiteSpace(error))
        {
            _bannerMessage = error.Trim();
            _bannerIsError = true;
        }
        else if (!string.IsNullOrWhiteSpace(info))
        {
            _bannerMessage = info.Trim();
            _bannerIsError = false;
        }
        else
        {
            _bannerMessage = null;
            _bannerIsError = false;
        }
    }

    private bool HasDetails =>
        !string.IsNullOrWhiteSpace(_actionId)
        || !string.IsNullOrWhiteSpace(_commandLine)
        || !string.IsNullOrWhiteSpace(_programName);

    private int S(float logicalPx) => Math.Max(1, (int)Math.Round(logicalPx * _scale));
    private float Sf(float logicalPx) => (float)(logicalPx * _scale);

    private static readonly string UiFontFamily = ResolveFontFamily("Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI");
    private static readonly string HeadingFontFamily = ResolveFontFamily("Segoe UI Variable Display", "Segoe UI", "Microsoft YaHei UI");
    private static readonly string MonoFontFamily = ResolveFontFamily("Cascadia Mono", "Consolas", "Courier New");

    private static string ResolveFontFamily(params string[] candidates)
    {
        using var fonts = new InstalledFontCollection();
        foreach (var candidate in candidates)
        {
            if (fonts.Families.Any(f => string.Equals(f.Name, candidate, StringComparison.OrdinalIgnoreCase)))
                return candidate;
        }
        return "Microsoft YaHei UI";
    }

    private Font MakeFont(float pt, FontStyle style = FontStyle.Regular, bool heading = false, bool mono = false)
    {
        string family = mono ? MonoFontFamily : (heading ? HeadingFontFamily : UiFontFamily);
        return new Font(family, pt, style, GraphicsUnit.Point);
    }

    private void UpdateInputFont()
    {
        var next = MakeFont(10.5f);
        if (_inputBox.Font is { } current && next.Equals(current))
        {
            next.Dispose();
            return;
        }
        var previous = _ownedInputFont;
        _inputBox.Font = next;
        _ownedInputFont = next;
        previous?.Dispose();
    }

    private int _yBadgeTop;
    private int _yMessageTop;
    private int _messageHeight;
    private Rectangle _contextCardRect;
    private int _yPromptTop;
    private int _yBannerTop;
    private int _bannerHeight;
    private int _yCapsTop;

    private static Size MeasureText(Graphics? g, string text, Font font, Size proposedSize, TextFormatFlags flags) =>
        g is not null
            ? TextRenderer.MeasureText(g, text, font, proposedSize, flags)
            : TextRenderer.MeasureText(text, font, proposedSize, flags);

    private void RecalculateLayout(bool recenter)
    {
        using var measureDc = IsHandleCreated ? CreateGraphics() : null;
        int width = S(440);
        int padX = S(24);
        int contentW = width - padX * 2;
        int y = S(22);

        _headerDragRect = new Rectangle(0, 0, width, S(76));
        _closeBtnRect = new Rectangle(width - S(14) - S(30), S(14), S(30), S(30));

        _yBadgeTop = y;
        y += S(46) + S(14);

        using (var msgFont = MakeFont(10.5f))
        {
            var measured = MeasureText(
                measureDc,
                _message,
                msgFont,
                new Size(contentW, 0),
                TextFormatFlags.WordBreak | TextFormatFlags.TextBoxControl);
            _yMessageTop = y;
            _messageHeight = Math.Max(S(20), measured.Height);
            y += _messageHeight + S(14);
        }

        // Identity + optional details card
        int cardHeaderH = S(44);
        int detailsBodyH = 0;
        if (_detailsExpanded && HasDetails)
        {
            detailsBodyH = S(10);
            using var monoFont = MakeFont(8.75f, mono: true);
            int valW = contentW - S(72);
            if (!string.IsNullOrWhiteSpace(_actionId))
            {
                var sz = MeasureText(measureDc, _actionId, monoFont, new Size(valW, 0), TextFormatFlags.WordBreak);
                detailsBodyH += Math.Max(S(18), sz.Height) + S(6);
            }
            string? cmdText = !string.IsNullOrWhiteSpace(_commandLine) ? _commandLine : _programName;
            if (!string.IsNullOrWhiteSpace(cmdText))
            {
                var sz = MeasureText(measureDc, cmdText, monoFont, new Size(valW, 0), TextFormatFlags.WordBreak);
                detailsBodyH += Math.Max(S(18), sz.Height) + S(6);
            }
            detailsBodyH += S(6);
        }

        _contextCardRect = new Rectangle(padX, y, contentW, cardHeaderH + detailsBodyH);
        _userPillRect = new Rectangle(padX + S(10), y + S(7), contentW - S(96), S(30));
        _detailsToggleRect = HasDetails
            ? new Rectangle(_contextCardRect.Right - S(76), y + S(8), S(68), S(28))
            : Rectangle.Empty;
        _detailsBodyRect = detailsBodyH > 0
            ? new Rectangle(padX + S(12), y + cardHeaderH, contentW - S(24), detailsBodyH)
            : Rectangle.Empty;
        y += _contextCardRect.Height + S(16);

        // Prompt label
        _yPromptTop = y;
        y += S(20) + S(6);

        // Input box
        int inputH = S(40);
        _inputFrameRect = new Rectangle(padX, y, contentW, inputH);
        int eyeW = _echoOn ? 0 : S(34);
        _eyeBtnRect = _echoOn
            ? Rectangle.Empty
            : new Rectangle(_inputFrameRect.Right - S(36), y + S(3), S(32), inputH - S(6));

        int textLeft = _inputFrameRect.Left + S(38);
        int textRight = _inputFrameRect.Right - S(10) - eyeW;
        int textH = _inputBox.PreferredHeight;
        int textTop = y + Math.Max(S(4), (inputH - textH) / 2);
        _inputBox.SetBounds(textLeft, textTop, Math.Max(S(80), textRight - textLeft), textH);
        y += inputH;

        // Caps Lock warning
        if (_capsLockOn && !_echoOn)
        {
            y += S(8);
            _yCapsTop = y;
            y += S(24);
        }
        else
        {
            _yCapsTop = 0;
        }

        // Error / PAM Info banner
        if (!string.IsNullOrWhiteSpace(_bannerMessage))
        {
            y += S(8);
            _yBannerTop = y;
            using var bannerFont = MakeFont(9.25f);
            var sz = MeasureText(
                measureDc,
                _bannerMessage,
                bannerFont,
                new Size(contentW - S(36), 0),
                TextFormatFlags.WordBreak);
            _bannerHeight = Math.Max(S(32), sz.Height + S(14));
            y += _bannerHeight;
        }
        else
        {
            _yBannerTop = 0;
            _bannerHeight = 0;
        }

        y += S(20);

        // Footer buttons
        int btnH = S(36);
        int submitW = S(104);
        int cancelW = S(86);
        int gap = S(10);
        _submitBtnRect = new Rectangle(width - padX - submitW, y, submitW, btnH);
        _cancelBtnRect = new Rectangle(_submitBtnRect.Left - gap - cancelW, y, cancelW, btnH);
        int totalH = y + btnH + S(22);

        Size = new Size(width, totalH);
        if (recenter || !_positioned)
        {
            CenterOnActiveMonitor();
            _positioned = true;
        }
    }

    private void CenterOnActiveMonitor()
    {
        Rectangle area;
        try
        {
            nint fg = GetForegroundWindow();
            nint monitor = fg != 0
                ? MonitorFromWindow(fg, MonitorDefaultToNearest)
                : MonitorFromPoint(default, MonitorDefaultToPrimary);
            var work = WorkAreaForMonitor(monitor).Work;
            area = new Rectangle(work.Left, work.Top, work.Width, work.Height);
        }
        catch
        {
            area = Screen.PrimaryScreen?.WorkingArea ?? new Rectangle(0, 0, 1280, 800);
        }
        int x = area.Left + Math.Max(0, (area.Width - Width) / 2);
        int y = area.Top + Math.Max(0, (area.Height - Height) / 2);
        Location = new Point(x, y);
        _shakeBaseX = x;
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;
        g.TextRenderingHint = TextRenderingHint.ClearTypeGridFit;
        g.Clear(_palette.Surface);

        // Subtle outer border for definition on light/dark backgrounds
        using (var borderPen = new Pen(_palette.Border, 1f))
        using (var borderPath = RoundedRect(new RectangleF(0.5f, 0.5f, Width - 1.5f, Height - 1.5f), Sf(8)))
        {
            g.DrawPath(borderPen, borderPath);
        }

        // Top close button
        PaintCloseButton(g);

        // Badge + Title
        int padX = S(24);
        var badgeRect = new Rectangle(padX, _yBadgeTop, S(46), S(46));
        PaintBadge(g, badgeRect);

        string titleText = _actionId == "io.focusstage.launcher.verify"
            ? "验证身份以解锁"
            : "需要管理员权限";
        string subText = _actionId == "io.focusstage.launcher.verify"
            ? "FocusStage 启动器安全验证"
            : "Linux 提权认证 · FocusStage";

        int titleX = badgeRect.Right + S(14);
        int titleW = _closeBtnRect.Left - titleX - S(8);
        using (var titleFont = MakeFont(13.5f, FontStyle.Bold, heading: true))
        using (var subFont = MakeFont(9.25f))
        {
            TextRenderer.DrawText(
                g,
                titleText,
                titleFont,
                new Rectangle(titleX, _yBadgeTop + S(2), titleW, S(24)),
                _palette.Ink,
                TextFormatFlags.Left | TextFormatFlags.EndEllipsis | TextFormatFlags.NoPadding);
            TextRenderer.DrawText(
                g,
                subText,
                subFont,
                new Rectangle(titleX, _yBadgeTop + S(26), titleW, S(18)),
                _palette.InkMuted,
                TextFormatFlags.Left | TextFormatFlags.EndEllipsis | TextFormatFlags.NoPadding);
        }

        // Message body
        using (var msgFont = MakeFont(10.5f))
        {
            TextRenderer.DrawText(
                g,
                _message,
                msgFont,
                new Rectangle(padX, _yMessageTop, Width - padX * 2, _messageHeight + S(4)),
                _palette.InkSecondary,
                TextFormatFlags.WordBreak | TextFormatFlags.TextBoxControl | TextFormatFlags.NoPadding);
        }

        // Context card (User + Details)
        PaintContextCard(g);

        // Prompt label
        using (var labelFont = MakeFont(9.5f, FontStyle.Bold))
        {
            TextRenderer.DrawText(
                g,
                _promptLabel,
                labelFont,
                new Rectangle(padX, _yPromptTop, Width - padX * 2, S(20)),
                _palette.Ink,
                TextFormatFlags.Left | TextFormatFlags.NoPadding);
        }

        // Input container
        PaintInputContainer(g);

        // Caps Lock hint
        if (_capsLockOn && !_echoOn && _yCapsTop > 0)
        {
            PaintCapsLockHint(g, padX, _yCapsTop);
        }

        // Error / Info banner
        if (!string.IsNullOrWhiteSpace(_bannerMessage) && _yBannerTop > 0)
        {
            PaintBanner(g, padX, _yBannerTop, Width - padX * 2, _bannerHeight);
        }

        // Footer buttons
        PaintButton(
            g,
            _cancelBtnRect,
            "取消",
            primary: false,
            hovered: _hoverZone == HitZone.Cancel,
            pressed: _pressedZone == HitZone.Cancel,
            disabled: false);
        PaintButton(
            g,
            _submitBtnRect,
            _verifying ? "验证中…" : "授权",
            primary: true,
            hovered: _hoverZone == HitZone.Submit,
            pressed: _pressedZone == HitZone.Submit,
            disabled: _verifying);
    }

    private void PaintCloseButton(Graphics g)
    {
        if (_hoverZone == HitZone.Close || _pressedZone == HitZone.Close)
        {
            Color fill = _pressedZone == HitZone.Close ? _palette.PressedBg : _palette.HoverBg;
            using var bg = new SolidBrush(fill);
            using var path = RoundedRect(_closeBtnRect, Sf(8));
            g.FillPath(bg, path);
        }

        float cx = _closeBtnRect.Left + _closeBtnRect.Width / 2f;
        float cy = _closeBtnRect.Top + _closeBtnRect.Height / 2f;
        float r = Sf(4.5f);
        using var pen = new Pen(_hoverZone == HitZone.Close ? _palette.Ink : _palette.InkMuted, Sf(1.5f))
        {
            StartCap = LineCap.Round,
            EndCap = LineCap.Round,
        };
        g.DrawLine(pen, cx - r, cy - r, cx + r, cy + r);
        g.DrawLine(pen, cx + r, cy - r, cx - r, cy + r);
    }

    private void PaintBadge(Graphics g, Rectangle rect)
    {
        using (var bg = new SolidBrush(_palette.AccentSoft))
        using (var border = new Pen(Color.FromArgb(72, _palette.Accent), 1f))
        using (var path = RoundedRect(rect, Sf(12)))
        {
            g.FillPath(bg, path);
            g.DrawPath(border, path);
        }

        if (_appIconBitmap is not null)
        {
            int iconPad = S(8);
            var iconRect = new Rectangle(rect.X + iconPad, rect.Y + iconPad, rect.Width - iconPad * 2, rect.Height - iconPad * 2);
            g.InterpolationMode = InterpolationMode.HighQualityBicubic;
            g.DrawImage(_appIconBitmap, iconRect);

            // Small security shield overlay at bottom-right
            var shieldRect = new RectangleF(rect.Right - Sf(17), rect.Bottom - Sf(17), Sf(18), Sf(18));
            DrawShieldVector(g, shieldRect, _palette.Accent, Color.White, drawCheck: true);
        }
        else
        {
            var shieldRect = new RectangleF(rect.X + Sf(11), rect.Y + Sf(10), rect.Width - Sf(22), rect.Height - Sf(20));
            DrawShieldVector(g, shieldRect, _palette.Accent, Color.White, drawCheck: false);
        }
    }

    private void DrawShieldVector(Graphics g, RectangleF bounds, Color fill, Color detail, bool drawCheck)
    {
        float x = bounds.X;
        float y = bounds.Y;
        float w = bounds.Width;
        float h = bounds.Height;

        using var path = new GraphicsPath();
        path.AddLine(x + w * 0.5f, y, x + w * 0.94f, y + h * 0.16f);
        path.AddLine(x + w * 0.94f, y + h * 0.50f, x + w * 0.5f, y + h * 0.98f);
        path.AddLine(x + w * 0.5f, y + h * 0.98f, x + w * 0.06f, y + h * 0.50f);
        path.AddLine(x + w * 0.06f, y + h * 0.16f, x + w * 0.5f, y);
        path.CloseFigure();

        using (var brush = new SolidBrush(fill))
        {
            g.FillPath(brush, path);
        }
        using var pen = new Pen(detail, Math.Max(1.4f, w * 0.11f))
        {
            StartCap = LineCap.Round,
            EndCap = LineCap.Round,
            LineJoin = LineJoin.Round,
        };
        if (drawCheck)
        {
            g.DrawLines(pen, new[]
            {
                new PointF(x + w * 0.30f, y + h * 0.50f),
                new PointF(x + w * 0.45f, y + h * 0.65f),
                new PointF(x + w * 0.72f, y + h * 0.36f),
            });
        }
        else
        {
            // Minimal keyhole/dot inside shield
            float r = w * 0.13f;
            using var dot = new SolidBrush(detail);
            g.FillEllipse(dot, x + w * 0.5f - r, y + h * 0.36f - r, r * 2, r * 2);
            g.DrawLine(pen, x + w * 0.5f, y + h * 0.42f, x + w * 0.5f, y + h * 0.64f);
        }
    }

    private void PaintContextCard(Graphics g)
    {
        using (var bg = new SolidBrush(_palette.Subtle))
        using (var border = new Pen(_palette.Border, 1f))
        using (var path = RoundedRect(_contextCardRect, Sf(10)))
        {
            g.FillPath(bg, path);
            g.DrawPath(border, path);
        }

        // User avatar circle
        int avatarSize = S(24);
        int avatarX = _contextCardRect.Left + S(12);
        int avatarY = _contextCardRect.Top + S(10);
        var avatarRect = new Rectangle(avatarX, avatarY, avatarSize, avatarSize);
        using (var avatarBg = new SolidBrush(_palette.AccentSoft))
        {
            g.FillEllipse(avatarBg, avatarRect);
        }
        string initial = string.IsNullOrEmpty(_selectedUser)
            ? "U"
            : _selectedUser[..1].ToUpperInvariant();
        using (var initFont = MakeFont(9.5f, FontStyle.Bold))
        {
            TextRenderer.DrawText(
                g,
                initial,
                initFont,
                avatarRect,
                _palette.Accent,
                TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
        }

        // Username + role chip
        int nameX = avatarRect.Right + S(9);
        string userDisplay = _users.Count > 1 ? $"{_selectedUser} ⇄" : _selectedUser;
        using (var userFont = MakeFont(9.75f, FontStyle.Bold))
        using (var chipFont = MakeFont(8f))
        {
            const string chipText = "Linux 账户";
            var chipSize = TextRenderer.MeasureText(g, chipText, chipFont, Size.Empty, TextFormatFlags.NoPadding);
            int chipW = Math.Max(S(62), chipSize.Width + S(14));
            var userSize = TextRenderer.MeasureText(g, userDisplay, userFont, Size.Empty, TextFormatFlags.NoPadding);
            int maxUserW = (_detailsToggleRect.IsEmpty ? _contextCardRect.Right : _detailsToggleRect.Left) - nameX - chipW - S(14);
            int userW = Math.Min(userSize.Width + S(2), Math.Max(S(40), maxUserW));
            TextRenderer.DrawText(
                g,
                userDisplay,
                userFont,
                new Rectangle(nameX, _contextCardRect.Top + S(12), userW, S(20)),
                _palette.Ink,
                TextFormatFlags.Left | TextFormatFlags.EndEllipsis | TextFormatFlags.NoPadding);

            int chipX = nameX + userW + S(8);
            var chipRect = new Rectangle(chipX, _contextCardRect.Top + S(12), chipW, S(20));
            using (var chipBg = new SolidBrush(_palette.Surface))
            using (var chipBorder = new Pen(_palette.Border, 1f))
            using (var chipPath = RoundedRect(chipRect, Sf(9)))
            {
                g.FillPath(chipBg, chipPath);
                g.DrawPath(chipBorder, chipPath);
            }
            TextRenderer.DrawText(
                g,
                chipText,
                chipFont,
                chipRect,
                _palette.InkMuted,
                TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
        }

        // Details toggle
        if (!_detailsToggleRect.IsEmpty)
        {
            if (_hoverZone == HitZone.DetailsToggle || _pressedZone == HitZone.DetailsToggle)
            {
                using var hoverBg = new SolidBrush(_palette.HoverBg);
                using var path = RoundedRect(_detailsToggleRect, Sf(7));
                g.FillPath(hoverBg, path);
            }
            using var toggleFont = MakeFont(8.75f, FontStyle.Bold);
            TextRenderer.DrawText(
                g,
                _detailsExpanded ? "收起 ▴" : "详情 ▾",
                toggleFont,
                _detailsToggleRect,
                _palette.Accent,
                TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
        }

        // Expanded details body
        if (_detailsExpanded && !_detailsBodyRect.IsEmpty)
        {
            using var sepPen = new Pen(_palette.Border, 1f);
            g.DrawLine(sepPen, _contextCardRect.Left + S(12), _detailsBodyRect.Top, _contextCardRect.Right - S(12), _detailsBodyRect.Top);

            int rowY = _detailsBodyRect.Top + S(8);
            using var keyFont = MakeFont(8.5f, FontStyle.Bold);
            using var valFont = MakeFont(8.75f, mono: true);
            int keyW = S(40);
            int valX = _detailsBodyRect.Left + keyW + S(6);
            int valW = _detailsBodyRect.Right - valX;

            if (!string.IsNullOrWhiteSpace(_actionId))
            {
                TextRenderer.DrawText(g, "操作", keyFont, new Rectangle(_detailsBodyRect.Left, rowY, keyW, S(18)), _palette.InkMuted, TextFormatFlags.Left | TextFormatFlags.NoPadding);
                var sz = TextRenderer.MeasureText(_actionId, valFont, new Size(valW, 0), TextFormatFlags.WordBreak);
                int h = Math.Max(S(18), sz.Height);
                TextRenderer.DrawText(g, _actionId, valFont, new Rectangle(valX, rowY, valW, h), _palette.InkSecondary, TextFormatFlags.WordBreak | TextFormatFlags.NoPadding);
                rowY += h + S(6);
            }

            string? cmdText = !string.IsNullOrWhiteSpace(_commandLine) ? _commandLine : _programName;
            if (!string.IsNullOrWhiteSpace(cmdText))
            {
                TextRenderer.DrawText(g, "命令", keyFont, new Rectangle(_detailsBodyRect.Left, rowY, keyW, S(18)), _palette.InkMuted, TextFormatFlags.Left | TextFormatFlags.NoPadding);
                var sz = TextRenderer.MeasureText(cmdText, valFont, new Size(valW, 0), TextFormatFlags.WordBreak);
                int h = Math.Max(S(18), sz.Height);
                TextRenderer.DrawText(g, cmdText, valFont, new Rectangle(valX, rowY, valW, h), _palette.InkSecondary, TextFormatFlags.WordBreak | TextFormatFlags.NoPadding);
            }
        }
    }

    private void PaintInputContainer(Graphics g)
    {
        bool focused = _inputBox.Focused;
        Color borderColor = _bannerIsError && !focused
            ? _palette.Danger
            : (focused ? _palette.Accent : _palette.BorderStrong);
        float borderWidth = focused ? Sf(1.8f) : 1f;

        using (var bg = new SolidBrush(_palette.InputBg))
        using (var border = new Pen(borderColor, borderWidth))
        using (var path = RoundedRect(_inputFrameRect, Sf(8)))
        {
            g.FillPath(bg, path);
            g.DrawPath(border, path);
        }

        // Left lock icon
        float lockCx = _inputFrameRect.Left + S(19);
        float lockCy = _inputFrameRect.Top + _inputFrameRect.Height / 2f;
        DrawLockVector(g, lockCx, lockCy, focused ? _palette.Accent : _palette.InkMuted);

        // Eye toggle button
        if (!_echoOn && !_eyeBtnRect.IsEmpty)
        {
            if (_hoverZone == HitZone.Eye || _pressedZone == HitZone.Eye)
            {
                using var hoverBg = new SolidBrush(_palette.HoverBg);
                using var path = RoundedRect(_eyeBtnRect, Sf(6));
                g.FillPath(hoverBg, path);
            }
            float ex = _eyeBtnRect.Left + _eyeBtnRect.Width / 2f;
            float ey = _eyeBtnRect.Top + _eyeBtnRect.Height / 2f;
            DrawEyeVector(g, ex, ey, _revealPassword, _hoverZone == HitZone.Eye ? _palette.Ink : _palette.InkMuted);
        }
    }

    private void DrawLockVector(Graphics g, float cx, float cy, Color color)
    {
        float w = Sf(11);
        float h = Sf(8.5f);
        using var pen = new Pen(color, Sf(1.45f))
        {
            StartCap = LineCap.Round,
            EndCap = LineCap.Round,
            LineJoin = LineJoin.Round,
        };
        var body = new RectangleF(cx - w / 2f, cy - Sf(1), w, h);
        using (var path = RoundedRect(body, Sf(2.2f)))
        {
            g.DrawPath(pen, path);
        }
        float shackleW = Sf(6.8f);
        float shackleH = Sf(6.5f);
        g.DrawArc(pen, cx - shackleW / 2f, body.Top - shackleH * 0.72f, shackleW, shackleH, 180, 180);
    }

    private void DrawEyeVector(Graphics g, float cx, float cy, bool revealed, Color color)
    {
        using var pen = new Pen(color, Sf(1.4f))
        {
            StartCap = LineCap.Round,
            EndCap = LineCap.Round,
        };
        float rx = Sf(7.5f);
        float ry = Sf(4.6f);
        using (var eyePath = new GraphicsPath())
        {
            eyePath.AddBezier(cx - rx, cy, cx - rx * 0.45f, cy - ry, cx + rx * 0.45f, cy - ry, cx + rx, cy);
            eyePath.AddBezier(cx + rx, cy, cx + rx * 0.45f, cy + ry, cx - rx * 0.45f, cy + ry, cx - rx, cy);
            g.DrawPath(pen, eyePath);
        }
        float pr = Sf(2.1f);
        using (var pupil = new SolidBrush(color))
        {
            g.FillEllipse(pupil, cx - pr, cy - pr, pr * 2, pr * 2);
        }
        if (revealed)
        {
            g.DrawLine(pen, cx - Sf(6.2f), cy + Sf(5.2f), cx + Sf(6.2f), cy - Sf(5.2f));
        }
    }

    private void PaintCapsLockHint(Graphics g, int x, int y)
    {
        const string capsText = "⚠ 大写锁定已开启";
        using var font = MakeFont(8.5f, FontStyle.Bold);
        var sz = TextRenderer.MeasureText(g, capsText, font, Size.Empty, TextFormatFlags.NoPadding);
        var pill = new Rectangle(x, y, Math.Max(S(128), sz.Width + S(18)), S(24));
        using (var bg = new SolidBrush(_palette.AmberSoft))
        using (var border = new Pen(Color.FromArgb(80, _palette.Amber), 1f))
        using (var path = RoundedRect(pill, Sf(7)))
        {
            g.FillPath(bg, path);
            g.DrawPath(border, path);
        }
        TextRenderer.DrawText(
            g,
            capsText,
            font,
            pill,
            _palette.Amber,
            TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
    }

    private void PaintBanner(Graphics g, int x, int y, int w, int h)
    {
        var rect = new Rectangle(x, y, w, h);
        Color fg = _bannerIsError ? _palette.Danger : _palette.InkSecondary;
        Color bgColor = _bannerIsError ? _palette.DangerSoft : _palette.Subtle;
        Color borderColor = _bannerIsError ? Color.FromArgb(72, _palette.Danger) : _palette.Border;

        using (var bg = new SolidBrush(bgColor))
        using (var border = new Pen(borderColor, 1f))
        using (var path = RoundedRect(rect, Sf(8)))
        {
            g.FillPath(bg, path);
            g.DrawPath(border, path);
        }

        using var font = MakeFont(9.25f);
        var textRect = new Rectangle(rect.X + S(12), rect.Y + S(6), rect.Width - S(24), rect.Height - S(12));
        TextRenderer.DrawText(
            g,
            _bannerMessage ?? "",
            font,
            textRect,
            fg,
            TextFormatFlags.WordBreak | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
    }

    private void PaintButton(
        Graphics g,
        Rectangle rect,
        string label,
        bool primary,
        bool hovered,
        bool pressed,
        bool disabled)
    {
        Color bgColor;
        Color fgColor;
        Color? borderColor = null;

        if (primary)
        {
            bgColor = disabled
                ? Color.FromArgb(140, _palette.Accent)
                : (pressed ? _palette.AccentPressed : (hovered ? _palette.AccentHover : _palette.Accent));
            fgColor = Color.White;
        }
        else
        {
            bgColor = pressed ? _palette.PressedBg : (hovered ? _palette.HoverBg : _palette.Subtle);
            fgColor = _palette.Ink;
            borderColor = _palette.BorderStrong;
        }

        using (var bg = new SolidBrush(bgColor))
        using (var path = RoundedRect(rect, Sf(8)))
        {
            g.FillPath(bg, path);
            if (borderColor is Color bc)
            {
                using var pen = new Pen(bc, 1f);
                g.DrawPath(pen, path);
            }
        }

        using var font = MakeFont(9.75f, primary ? FontStyle.Bold : FontStyle.Regular);
        TextRenderer.DrawText(
            g,
            label,
            font,
            rect,
            fgColor,
            TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter | TextFormatFlags.NoPadding);
    }

    private static GraphicsPath RoundedRect(RectangleF bounds, float radius)
    {
        float d = Math.Min(radius * 2f, Math.Min(bounds.Width, bounds.Height));
        var path = new GraphicsPath();
        if (d <= 0.5f)
        {
            path.AddRectangle(bounds);
            return path;
        }
        path.AddArc(bounds.X, bounds.Y, d, d, 180, 90);
        path.AddArc(bounds.Right - d, bounds.Y, d, d, 270, 90);
        path.AddArc(bounds.Right - d, bounds.Bottom - d, d, d, 0, 90);
        path.AddArc(bounds.X, bounds.Bottom - d, d, d, 90, 90);
        path.CloseFigure();
        return path;
    }

    private static Bitmap? DecodePngBitmap(string? base64Png)
    {
        if (string.IsNullOrWhiteSpace(base64Png)) return null;
        try
        {
            byte[] bytes = Convert.FromBase64String(base64Png);
            using var ms = new MemoryStream(bytes, writable: false);
            using var loaded = new Bitmap(ms);
            return new Bitmap(loaded);
        }
        catch
        {
            return null;
        }
    }

    private static string FormatPromptLabel(string? prompt, bool echoOn)
    {
        string trimmed = (prompt ?? "").Trim().TrimEnd(':', '：').Trim();
        if (string.IsNullOrEmpty(trimmed) || trimmed.Equals("Password", StringComparison.OrdinalIgnoreCase))
        {
            return echoOn ? "输入验证信息" : "输入 Linux 密码";
        }
        if (trimmed.Equals("密码", StringComparison.Ordinal))
        {
            return "输入 Linux 密码";
        }
        return trimmed;
    }

    private static ThemePalette BuildPalette()
    {
        var appearance = LauncherLayout.LoadExistingReadOnly().Appearance;
        appearance.Normalize();

        Color surface = ParseHexColor(appearance.Surface, Color.FromArgb(249, 251, 253));
        Color ink = ParseHexColor(appearance.Ink, Color.FromArgb(27, 38, 59));
        Color accent = ParseHexColor(appearance.Accent, Color.FromArgb(61, 102, 213));

        double luminance = (0.299 * surface.R + 0.587 * surface.G + 0.114 * surface.B) / 255.0;
        bool isDark = luminance < 0.5;

        Color subtle = Blend(surface, ink, isDark ? 0.08f : 0.045f);
        Color inputBg = isDark ? Blend(surface, Color.Black, 0.24f) : Color.White;
        Color hoverBg = Blend(surface, ink, isDark ? 0.12f : 0.06f);
        Color pressedBg = Blend(surface, ink, isDark ? 0.18f : 0.11f);
        Color accentHover = Blend(accent, isDark ? Color.White : Color.Black, 0.12f);
        Color accentPressed = Blend(accent, isDark ? Color.White : Color.Black, 0.22f);

        return new ThemePalette(
            IsDark: isDark,
            Surface: surface,
            Subtle: subtle,
            InputBg: inputBg,
            HoverBg: hoverBg,
            PressedBg: pressedBg,
            Border: Color.FromArgb(isDark ? 42 : 28, ink),
            BorderStrong: Color.FromArgb(isDark ? 72 : 52, ink),
            Ink: ink,
            InkSecondary: Color.FromArgb(215, ink),
            InkMuted: Color.FromArgb(145, ink),
            Accent: accent,
            AccentHover: accentHover,
            AccentPressed: accentPressed,
            AccentSoft: Color.FromArgb(isDark ? 48 : 26, accent),
            Danger: isDark ? Color.FromArgb(251, 113, 133) : Color.FromArgb(220, 38, 38),
            DangerSoft: isDark ? Color.FromArgb(46, 251, 113, 133) : Color.FromArgb(253, 236, 236),
            Amber: isDark ? Color.FromArgb(251, 191, 36) : Color.FromArgb(180, 83, 9),
            AmberSoft: isDark ? Color.FromArgb(42, 251, 191, 36) : Color.FromArgb(255, 245, 223));
    }

    private static Color ParseHexColor(string hex, Color fallback)
    {
        try
        {
            return ColorTranslator.FromHtml(hex);
        }
        catch
        {
            return fallback;
        }
    }

    private static Color Blend(Color a, Color b, float t)
    {
        t = Math.Clamp(t, 0f, 1f);
        int r = (int)Math.Round(a.R + (b.R - a.R) * t);
        int g = (int)Math.Round(a.G + (b.G - a.G) * t);
        int bl = (int)Math.Round(a.B + (b.B - a.B) * t);
        return Color.FromArgb(255, Math.Clamp(r, 0, 255), Math.Clamp(g, 0, 255), Math.Clamp(bl, 0, 255));
    }
}
