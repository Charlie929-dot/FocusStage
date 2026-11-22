using System.Drawing;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;

namespace FocusStageTray;

internal static class Win11WindowHelper
{
    private const int DWMWA_WINDOW_CORNER_PREFERENCE = 33;
    private const int DWMWCP_ROUND = 2;

    public const int WM_NCHITTEST = 0x0084;
    public const int WM_NCLBUTTONDOWN = 0x00A1;
    public const int HTCAPTION = 2;
    public const int HTLEFT = 10;
    public const int HTRIGHT = 11;
    public const int HTTOP = 12;
    public const int HTTOPLEFT = 13;
    public const int HTTOPRIGHT = 14;
    public const int HTBOTTOM = 15;
    public const int HTBOTTOMLEFT = 16;
    public const int HTBOTTOMRIGHT = 17;

    [StructLayout(LayoutKind.Sequential)]
    private struct MARGINS
    {
        public int Left;
        public int Right;
        public int Top;
        public int Bottom;
    }

    [DllImport("dwmapi.dll", PreserveSig = true)]
    private static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int attrValue, int attrSize);

    [DllImport("dwmapi.dll", PreserveSig = true)]
    private static extern int DwmExtendFrameIntoClientArea(IntPtr hwnd, ref MARGINS pMarInset);

    [DllImport("user32.dll")]
    public static extern bool ReleaseCapture();

    [DllImport("user32.dll")]
    public static extern IntPtr SendMessage(IntPtr hWnd, int msg, int wParam, int lParam);

    public static void ApplyWin11Chrome(IntPtr handle)
    {
        try
        {
            int pref = DWMWCP_ROUND;
            DwmSetWindowAttribute(handle, DWMWA_WINDOW_CORNER_PREFERENCE, ref pref, sizeof(int));

            var margins = new MARGINS { Left = 0, Right = 0, Top = 0, Bottom = 1 };
            DwmExtendFrameIntoClientArea(handle, ref margins);
        }
        catch
        {
            // Older Windows builds without DWM corner attributes gracefully ignore this.
        }
    }

    public static void BeginDragWindow(Form form)
    {
        ReleaseCapture();
        SendMessage(form.Handle, WM_NCLBUTTONDOWN, HTCAPTION, 0);
    }

    public static bool TryHitTestResizeBorder(Form form, ref Message m, int grip = 7)
    {
        if (form.WindowState == FormWindowState.Maximized) return false;

        int lp = m.LParam.ToInt32();
        int x = (short)(lp & 0xFFFF);
        int y = (short)((lp >> 16) & 0xFFFF);
        var pt = form.PointToClient(new Point(x, y));

        bool left = pt.X < grip;
        bool right = pt.X >= form.ClientSize.Width - grip;
        bool top = pt.Y < grip;
        bool bottom = pt.Y >= form.ClientSize.Height - grip;

        if (top && left) { m.Result = (IntPtr)HTTOPLEFT; return true; }
        if (top && right) { m.Result = (IntPtr)HTTOPRIGHT; return true; }
        if (bottom && left) { m.Result = (IntPtr)HTBOTTOMLEFT; return true; }
        if (bottom && right) { m.Result = (IntPtr)HTBOTTOMRIGHT; return true; }
        if (left) { m.Result = (IntPtr)HTLEFT; return true; }
        if (right) { m.Result = (IntPtr)HTRIGHT; return true; }
        if (top) { m.Result = (IntPtr)HTTOP; return true; }
        if (bottom) { m.Result = (IntPtr)HTBOTTOM; return true; }

        return false;
    }
}

internal static class UiGraphics
{
    public static GraphicsPath BuildRoundPath(Rectangle rect, int radius)
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

    public static GraphicsPath BuildRoundPath(RectangleF rect, float radius)
    {
        float d = Math.Min(radius * 2f, Math.Min(rect.Width, rect.Height));
        var path = new GraphicsPath();
        if (d <= 0.1f)
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

    public static Bitmap RenderWallpaper(int width, int height)
    {
        int w = Math.Max(16, width);
        int h = Math.Max(16, height);
        var bmp = new Bitmap(w, h);
        using var g = Graphics.FromImage(bmp);
        g.SmoothingMode = SmoothingMode.AntiAlias;

        // Soft serene daylight homescreen gradient
        using (var baseBrush = new LinearGradientBrush(
            new Rectangle(0, 0, w, h),
            Color.FromArgb(234, 242, 252),
            Color.FromArgb(238, 244, 249),
            128f))
        {
            var cb = new ColorBlend
            {
                Colors = new[]
                {
                    Color.FromArgb(232, 241, 252),
                    Color.FromArgb(242, 239, 250),
                    Color.FromArgb(233, 245, 247)
                },
                Positions = new[] { 0f, 0.52f, 1f }
            };
            baseBrush.InterpolationColors = cb;
            g.FillRectangle(baseBrush, 0, 0, w, h);
        }

        // Subtle ambient glow blobs for depth
        DrawRadialGlow(g, new RectangleF(w * 0.55f, -h * 0.25f, w * 0.65f, h * 0.78f), Color.FromArgb(62, 147, 197, 253));
        DrawRadialGlow(g, new RectangleF(-w * 0.18f, h * 0.45f, w * 0.60f, h * 0.75f), Color.FromArgb(52, 196, 181, 253));
        DrawRadialGlow(g, new RectangleF(w * 0.35f, h * 0.55f, w * 0.55f, h * 0.65f), Color.FromArgb(40, 153, 246, 228));

        return bmp;
    }

    private static void DrawRadialGlow(Graphics g, RectangleF bounds, Color centerColor)
    {
        using var path = new GraphicsPath();
        path.AddEllipse(bounds);
        using var brush = new PathGradientBrush(path)
        {
            CenterColor = centerColor,
            SurroundColors = new[] { Color.FromArgb(0, centerColor) }
        };
        g.FillPath(brush, path);
    }
}

internal sealed class BufferedFlowPanel : FlowLayoutPanel
{
    public Func<Bitmap?>? WallpaperProvider { get; set; }

    public BufferedFlowPanel()
    {
        DoubleBuffered = true;
        SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint | ControlStyles.ResizeRedraw, true);
    }

    protected override void OnPaintBackground(PaintEventArgs e)
    {
        var wp = WallpaperProvider?.Invoke();
        if (wp is not null)
        {
            var origin = PointToScreen(Point.Empty);
            var form = FindForm();
            if (form is not null)
            {
                var formOrigin = form.PointToScreen(Point.Empty);
                int ox = origin.X - formOrigin.X;
                int oy = origin.Y - formOrigin.Y;
                e.Graphics.DrawImage(wp, new Rectangle(0, 0, Width, Height), new Rectangle(ox, oy, Width, Height), GraphicsUnit.Pixel);
                return;
            }
        }
        base.OnPaintBackground(e);
    }
}

internal sealed class RoundedPanel : Panel
{
    public int CornerRadius { get; set; } = 18;
    public Color FillColor { get; set; } = Color.White;
    public Color BorderColor { get; set; } = Color.FromArgb(216, 225, 236);
    public bool DrawShadow { get; set; }

    public RoundedPanel()
    {
        DoubleBuffered = true;
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor
            | ControlStyles.ResizeRedraw,
            true);
        BackColor = Color.Transparent;
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        int inset = DrawShadow ? 4 : 0;
        var rect = new Rectangle(inset, inset, Width - 1 - inset * 2, Height - 1 - inset * 2);
        if (rect.Width <= 0 || rect.Height <= 0) return;

        if (DrawShadow)
        {
            for (int i = 3; i >= 1; i--)
            {
                var sr = new Rectangle(rect.X - i + 1, rect.Y + i, rect.Width + (i - 1) * 2, rect.Height);
                using var sp = UiGraphics.BuildRoundPath(sr, CornerRadius + 1);
                using var sb = new SolidBrush(Color.FromArgb(10 * (4 - i), 15, 23, 42));
                g.FillPath(sb, sp);
            }
        }

        using var path = UiGraphics.BuildRoundPath(rect, CornerRadius);
        using var fill = new SolidBrush(FillColor);
        using var pen = new Pen(BorderColor, 1f);
        g.FillPath(fill, path);
        if (BorderColor.A > 0)
        {
            g.DrawPath(pen, path);
        }
        base.OnPaint(e);
    }
}

internal sealed class DimOverlayPanel : Panel
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
            e.Graphics.Clear(Color.FromArgb(224, 232, 242));
        }
        using var dim = new SolidBrush(Color.FromArgb(108, 15, 23, 42));
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

internal sealed class PillSearchBox : Panel
{
    private readonly TextBox _input;
    private readonly Label _clearBtn;
    private bool _focused;

    public string SearchText
    {
        get => _input.Text;
        set => _input.Text = value;
    }

    public event EventHandler? SearchTextChanged;

    public PillSearchBox(string placeholder = "搜索应用...")
    {
        DoubleBuffered = true;
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
        Height = 36;
        Width = 280;

        _input = new TextBox
        {
            BorderStyle = BorderStyle.None,
            BackColor = Color.FromArgb(250, 252, 255),
            ForeColor = Color.FromArgb(30, 41, 59),
            Font = new Font("Microsoft YaHei UI", 9.2f),
            PlaceholderText = placeholder
        };

        _clearBtn = new Label
        {
            Text = "✕",
            Visible = false,
            AutoSize = false,
            Size = new Size(22, 22),
            TextAlign = ContentAlignment.MiddleCenter,
            ForeColor = Color.FromArgb(148, 163, 184),
            BackColor = Color.FromArgb(250, 252, 255),
            Font = new Font("Microsoft YaHei UI", 8.0f),
            Cursor = Cursors.Hand
        };

        _input.TextChanged += (_, _) =>
        {
            _clearBtn.Visible = _input.Text.Length > 0;
            SearchTextChanged?.Invoke(this, EventArgs.Empty);
        };
        _input.GotFocus += (_, _) => { _focused = true; Invalidate(); };
        _input.LostFocus += (_, _) => { _focused = false; Invalidate(); };
        _clearBtn.Click += (_, _) =>
        {
            _input.Clear();
            _input.Focus();
        };
        _clearBtn.MouseEnter += (_, _) => _clearBtn.ForeColor = Color.FromArgb(51, 65, 85);
        _clearBtn.MouseLeave += (_, _) => _clearBtn.ForeColor = Color.FromArgb(148, 163, 184);

        Controls.Add(_input);
        Controls.Add(_clearBtn);
        Click += (_, _) => _input.Focus();
        Resize += (_, _) => LayoutChildren();
        LayoutChildren();
    }

    public void FocusInput() => _input.Focus();

    public void ClearSearch() => _input.Clear();

    private void LayoutChildren()
    {
        int clearW = 24;
        int left = 34;
        int right = Width - clearW - 10;
        int textH = _input.PreferredHeight;
        _input.Location = new Point(left, Math.Max(4, (Height - textH) / 2));
        _input.Width = Math.Max(40, right - left);
        _clearBtn.Location = new Point(Width - clearW - 8, (Height - _clearBtn.Height) / 2);
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        var rect = new Rectangle(1, 1, Width - 3, Height - 3);
        using var path = UiGraphics.BuildRoundPath(rect, Height / 2 - 1);
        using var fill = new SolidBrush(Color.FromArgb(250, 252, 255));
        using var border = new Pen(_focused ? Color.FromArgb(59, 130, 246) : Color.FromArgb(212, 222, 236), _focused ? 1.5f : 1.0f);
        g.FillPath(fill, path);
        g.DrawPath(border, path);

        // Draw crisp vector magnifying glass
        int cy = Height / 2;
        using var iconPen = new Pen(_focused ? Color.FromArgb(59, 130, 246) : Color.FromArgb(148, 163, 184), 1.6f)
        {
            StartCap = LineCap.Round,
            EndCap = LineCap.Round
        };
        g.DrawEllipse(iconPen, 13, cy - 6, 10, 10);
        g.DrawLine(iconPen, 21.5f, cy + 2.5f, 25f, cy + 6f);
    }
}

internal enum WindowControlKind
{
    Minimize,
    Close
}

internal sealed class WindowControlButton : Control
{
    private readonly WindowControlKind _kind;
    private bool _hovered;

    public WindowControlButton(WindowControlKind kind)
    {
        _kind = kind;
        Size = new Size(32, 32);
        Cursor = Cursors.Hand;
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
    }

    protected override void OnMouseEnter(EventArgs e) { _hovered = true; Invalidate(); base.OnMouseEnter(e); }
    protected override void OnMouseLeave(EventArgs e) { _hovered = false; Invalidate(); base.OnMouseLeave(e); }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        if (_hovered)
        {
            var bg = _kind == WindowControlKind.Close
                ? Color.FromArgb(239, 68, 68)
                : Color.FromArgb(28, 15, 23, 42);
            using var b = new SolidBrush(bg);
            g.FillEllipse(b, 2, 2, Width - 5, Height - 5);
        }

        var fg = _hovered && _kind == WindowControlKind.Close
            ? Color.White
            : Color.FromArgb(71, 85, 105);

        using var pen = new Pen(fg, 1.5f) { StartCap = LineCap.Round, EndCap = LineCap.Round };
        int cx = Width / 2;
        int cy = Height / 2;

        if (_kind == WindowControlKind.Minimize)
        {
            g.DrawLine(pen, cx - 5, cy, cx + 5, cy);
        }
        else
        {
            g.DrawLine(pen, cx - 4.5f, cy - 4.5f, cx + 4.5f, cy + 4.5f);
            g.DrawLine(pen, cx + 4.5f, cy - 4.5f, cx - 4.5f, cy + 4.5f);
        }
    }
}

internal sealed class PillButton : Control
{
    private bool _hovered;
    private bool _pressed;

    public Color FillColor { get; set; } = Color.FromArgb(241, 245, 249);
    public Color HoverColor { get; set; } = Color.FromArgb(226, 234, 244);
    public Color BorderColor { get; set; } = Color.FromArgb(214, 224, 236);
    public int CornerRadius { get; set; } = 10;

    public PillButton(string text)
    {
        Text = text;
        Height = 32;
        ForeColor = Color.FromArgb(30, 41, 59);
        Font = new Font("Microsoft YaHei UI", 8.8f, FontStyle.Regular);
        Cursor = Cursors.Hand;
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
        UpdateAutoWidth();
    }

    public void UpdateAutoWidth(int horizontalPadding = 28)
    {
        using var bmp = new Bitmap(1, 1);
        using var g = Graphics.FromImage(bmp);
        var sz = g.MeasureString(Text, Font);
        Width = Math.Max(64, (int)Math.Ceiling(sz.Width) + horizontalPadding);
    }

    protected override void OnTextChanged(EventArgs e)
    {
        base.OnTextChanged(e);
        Invalidate();
    }

    protected override void OnEnabledChanged(EventArgs e)
    {
        base.OnEnabledChanged(e);
        Cursor = Enabled ? Cursors.Hand : Cursors.Default;
        Invalidate();
    }

    protected override void OnMouseEnter(EventArgs e) { _hovered = true; Invalidate(); base.OnMouseEnter(e); }
    protected override void OnMouseLeave(EventArgs e) { _hovered = false; _pressed = false; Invalidate(); base.OnMouseLeave(e); }
    protected override void OnMouseDown(MouseEventArgs e) { if (e.Button == MouseButtons.Left) { _pressed = true; Invalidate(); } base.OnMouseDown(e); }
    protected override void OnMouseUp(MouseEventArgs e) { _pressed = false; Invalidate(); base.OnMouseUp(e); }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        var rect = new Rectangle(1, 1, Width - 3, Height - 3);
        using var path = UiGraphics.BuildRoundPath(rect, CornerRadius);

        var bg = !Enabled
            ? Color.FromArgb(140, FillColor)
            : _pressed
                ? ControlPaint.Dark(HoverColor, 0.04f)
                : _hovered
                    ? HoverColor
                    : FillColor;

        using var brush = new SolidBrush(bg);
        g.FillPath(brush, path);

        if (BorderColor.A > 0)
        {
            using var pen = new Pen(BorderColor, 1f);
            g.DrawPath(pen, path);
        }

        var fg = Enabled ? ForeColor : Color.FromArgb(148, 163, 184);
        using var textBrush = new SolidBrush(fg);
        using var sf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
        g.DrawString(Text, Font, textBrush, rect, sf);
    }
}

internal sealed class ToggleSwitch : Control
{
    private bool _checked;

    public bool Checked
    {
        get => _checked;
        set
        {
            if (_checked == value) return;
            _checked = value;
            Invalidate();
            CheckedChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    public event EventHandler? CheckedChanged;

    public ToggleSwitch()
    {
        Size = new Size(44, 24);
        Cursor = Cursors.Hand;
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
    }

    protected override void OnClick(EventArgs e)
    {
        Checked = !Checked;
        base.OnClick(e);
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        var trackRect = new Rectangle(1, 1, Width - 3, Height - 3);
        using var trackPath = UiGraphics.BuildRoundPath(trackRect, trackRect.Height / 2);
        var trackColor = _checked ? Color.FromArgb(37, 99, 235) : Color.FromArgb(203, 213, 225);
        using var trackBrush = new SolidBrush(trackColor);
        g.FillPath(trackBrush, trackPath);

        int knobSize = trackRect.Height - 4;
        int knobX = _checked ? trackRect.Right - knobSize - 2 : trackRect.X + 2;
        int knobY = trackRect.Y + 2;

        using var shadow = new SolidBrush(Color.FromArgb(36, 15, 23, 42));
        g.FillEllipse(shadow, knobX, knobY + 1, knobSize, knobSize);

        using var knobBrush = new SolidBrush(Color.White);
        g.FillEllipse(knobBrush, knobX, knobY, knobSize, knobSize);
    }
}

internal enum DockIconKind
{
    AppCenter,
    NewFolder,
    RunCommand,
    Settings
}

internal sealed class DockItemButton : Control
{
    private bool _hovered;
    private readonly DockIconKind _iconKind;
    private readonly Color _accent;
    private readonly Color _tint;

    public DockItemButton(string label, DockIconKind iconKind, Color accent, Color tint)
    {
        Text = label;
        _iconKind = iconKind;
        _accent = accent;
        _tint = tint;
        Size = new Size(88, 54);
        Margin = new Padding(4, 0, 4, 0);
        Cursor = Cursors.Hand;
        Font = new Font("Microsoft YaHei UI", 8.0f, FontStyle.Regular);
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
    }

    protected override void OnMouseEnter(EventArgs e) { _hovered = true; Invalidate(); base.OnMouseEnter(e); }
    protected override void OnMouseLeave(EventArgs e) { _hovered = false; Invalidate(); base.OnMouseLeave(e); }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        if (_hovered)
        {
            using var hoverPath = UiGraphics.BuildRoundPath(new Rectangle(2, 2, Width - 5, Height - 5), 14);
            using var hoverBrush = new SolidBrush(Color.FromArgb(20, 15, 23, 42));
            g.FillPath(hoverBrush, hoverPath);
        }

        // Icon badge (30x30 squircle)
        int iconBox = 30;
        int ix = (Width - iconBox) / 2;
        int iy = _hovered ? 4 : 5;
        var boxRect = new Rectangle(ix, iy, iconBox, iconBox);
        using var boxPath = UiGraphics.BuildRoundPath(boxRect, 9);
        using var boxBrush = new SolidBrush(_tint);
        g.FillPath(boxBrush, boxPath);

        DrawVectorIcon(g, boxRect, _iconKind, _accent);

        // Label below
        var labelRect = new Rectangle(0, iy + iconBox + 2, Width, Height - (iy + iconBox + 2));
        using var fg = new SolidBrush(Enabled ? Color.FromArgb(51, 65, 85) : Color.FromArgb(148, 163, 184));
        using var sf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
        g.DrawString(Text, Font, fg, labelRect, sf);
    }

    private static void DrawVectorIcon(Graphics g, Rectangle box, DockIconKind kind, Color color)
    {
        float cx = box.X + box.Width / 2f;
        float cy = box.Y + box.Height / 2f;
        using var pen = new Pen(color, 1.7f) { StartCap = LineCap.Round, EndCap = LineCap.Round, LineJoin = LineJoin.Round };

        switch (kind)
        {
            case DockIconKind.AppCenter:
            {
                // Shopping / app store bag
                var bag = new RectangleF(cx - 6f, cy - 3.5f, 12f, 10f);
                using var bp = UiGraphics.BuildRoundPath(bag, 2.2f);
                g.DrawPath(pen, bp);
                g.DrawArc(pen, cx - 3.2f, cy - 7.5f, 6.4f, 6.4f, 180, 180);
                break;
            }
            case DockIconKind.NewFolder:
            {
                // Folder outline with plus
                var pts = new[]
                {
                    new PointF(cx - 7f, cy - 4.5f),
                    new PointF(cx - 2f, cy - 4.5f),
                    new PointF(cx - 0.5f, cy - 2.5f),
                    new PointF(cx + 7f, cy - 2.5f),
                    new PointF(cx + 7f, cy + 5.5f),
                    new PointF(cx - 7f, cy + 5.5f)
                };
                g.DrawPolygon(pen, pts);
                g.DrawLine(pen, cx, cy - 0.5f, cx, cy + 3.5f);
                g.DrawLine(pen, cx - 2f, cy + 1.5f, cx + 2f, cy + 1.5f);
                break;
            }
            case DockIconKind.RunCommand:
            {
                // Terminal >_
                g.DrawLine(pen, cx - 5.5f, cy - 3.5f, cx - 1.5f, cy + 0.5f);
                g.DrawLine(pen, cx - 5.5f, cy + 4.5f, cx - 1.5f, cy + 0.5f);
                g.DrawLine(pen, cx + 0.5f, cy + 4.5f, cx + 5.5f, cy + 4.5f);
                break;
            }
            case DockIconKind.Settings:
            {
                // Clean gear
                g.DrawEllipse(pen, cx - 3.2f, cy - 3.2f, 6.4f, 6.4f);
                for (int i = 0; i < 6; i++)
                {
                    double ang = i * Math.PI / 3.0;
                    float x1 = cx + (float)(Math.Cos(ang) * 5.0);
                    float y1 = cy + (float)(Math.Sin(ang) * 5.0);
                    float x2 = cx + (float)(Math.Cos(ang) * 7.2);
                    float y2 = cy + (float)(Math.Sin(ang) * 7.2);
                    g.DrawLine(pen, x1, y1, x2, y2);
                }
                break;
            }
        }
    }
}

internal sealed class ToastPill : Control
{
    public ToastPill()
    {
        Visible = false;
        Height = 32;
        Font = new Font("Microsoft YaHei UI", 8.6f);
        SetStyle(
            ControlStyles.OptimizedDoubleBuffer
            | ControlStyles.AllPaintingInWmPaint
            | ControlStyles.UserPaint
            | ControlStyles.SupportsTransparentBackColor,
            true);
        BackColor = Color.Transparent;
    }

    public void SetMessage(string text, int maxWidth)
    {
        Text = text;
        using var bmp = new Bitmap(1, 1);
        using var g = Graphics.FromImage(bmp);
        var sz = g.MeasureString(text, Font);
        Width = Math.Min(maxWidth, Math.Max(120, (int)Math.Ceiling(sz.Width) + 32));
        Invalidate();
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        var g = e.Graphics;
        g.SmoothingMode = SmoothingMode.AntiAlias;

        var rect = new Rectangle(1, 1, Width - 3, Height - 3);
        using var path = UiGraphics.BuildRoundPath(rect, Height / 2 - 1);
        using var bg = new SolidBrush(Color.FromArgb(224, 15, 23, 42));
        g.FillPath(bg, path);

        using var fg = new SolidBrush(Color.White);
        using var sf = new StringFormat
        {
            Alignment = StringAlignment.Center,
            LineAlignment = StringAlignment.Center,
            Trimming = StringTrimming.EllipsisCharacter,
            FormatFlags = StringFormatFlags.NoWrap
        };
        g.DrawString(Text, Font, fg, new Rectangle(12, 0, Width - 24, Height), sf);
    }
}

internal sealed class DesktopIconTile : Control
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
        Size = new Size(104, 112);
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

        // Subtle frosted hover / selection halo
        if (_isSelected || _hovered || _isDropTarget)
        {
            var tileRect = new Rectangle(2, 2, Width - 5, Height - 5);
            using var tilePath = UiGraphics.BuildRoundPath(tileRect, 16);
            var bg = _isDropTarget
                ? Color.FromArgb(165, 219, 234, 254)
                : _isSelected
                    ? Color.FromArgb(150, 224, 238, 255)
                    : Color.FromArgb(120, 255, 255, 255);
            using var bgBrush = new SolidBrush(bg);
            g.FillPath(bgBrush, tilePath);

            if (_isSelected || _isDropTarget)
            {
                using var borderPen = new Pen(Color.FromArgb(180, 59, 130, 246), 1.4f);
                g.DrawPath(borderPen, tilePath);
            }
        }

        // Squircle icon (62x62 centered) with soft layered drop shadow
        var boxRect = new Rectangle((Width - 62) / 2, 8, 62, 62);
        for (int i = 2; i >= 1; i--)
        {
            var shadowRect = new Rectangle(boxRect.X, boxRect.Y + i + 1, boxRect.Width, boxRect.Height);
            using var sp = UiGraphics.BuildRoundPath(shadowRect, 16);
            using var sb = new SolidBrush(Color.FromArgb(i == 1 ? 22 : 11, 15, 23, 42));
            g.FillPath(sb, sp);
        }

        using var boxPath = UiGraphics.BuildRoundPath(boxRect, 16);

        if (IsFolder)
        {
            using var folderBg = new SolidBrush(Color.FromArgb(185, 255, 255, 255));
            using var folderBorder = new Pen(Color.FromArgb(200, 255, 255, 255), 1.2f);
            g.FillPath(folderBg, boxPath);
            g.DrawPath(folderBorder, boxPath);

            int miniSize = 21;
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
                    using var slotPath = UiGraphics.BuildRoundPath(mr, 5);
                    using var slotBrush = new SolidBrush(Color.FromArgb(80, 148, 163, 184));
                    g.FillPath(slotBrush, slotPath);
                }
            }
        }
        else
        {
            using var appBg = new SolidBrush(Color.FromArgb(248, 255, 255, 255));
            using var appBorder = new Pen(Color.FromArgb(140, 226, 232, 240), 1.0f);
            g.FillPath(appBg, boxPath);
            g.DrawPath(appBorder, boxPath);

            if (AppIcon is not null)
            {
                var iconRect = new Rectangle(boxRect.X + 9, boxRect.Y + 9, 44, 44);
                g.DrawImage(AppIcon, iconRect);
            }
        }

        // Optional mini badge (only for DEB/APP installers)
        if (!string.IsNullOrWhiteSpace(SubBadgeText))
        {
            using var badgeFont = new Font("Microsoft YaHei UI", 6.8f, FontStyle.Bold);
            var sz = g.MeasureString(SubBadgeText, badgeFont);
            int bw = Math.Max(18, (int)Math.Ceiling(sz.Width) + 8);
            int bh = 15;
            var br = new Rectangle(boxRect.Right - bw + 4, boxRect.Top - 2, bw, bh);
            using var bp = UiGraphics.BuildRoundPath(br, 7);
            var badgeColor = SubBadgeText == "DEB"
                ? Color.FromArgb(217, 119, 6)
                : Color.FromArgb(16, 163, 127);
            using var bb = new SolidBrush(badgeColor);
            using var bf = new SolidBrush(Color.White);
            using var bsf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center };
            g.FillPath(bb, bp);
            g.DrawString(SubBadgeText, badgeFont, bf, br, bsf);
        }

        // App / Folder label underneath
        var textRect = new Rectangle(4, boxRect.Bottom + 6, Width - 8, Height - boxRect.Bottom - 8);
        using var labelFont = new Font("Microsoft YaHei UI", 8.4f, FontStyle.Regular);
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
