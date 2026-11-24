using System.Buffers;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Drawing.Text;
using System.Runtime.InteropServices;

namespace FocusStageTray;

// Minimal Direct2D 1.0 bindings for a UI-thread-owned HWND render target, guest BGRA frame,
// and GPU-composited premultiplied-BGRA AI takeover overlay sprites (inner glow, pill banner,
// and animated virtual cursor).
internal sealed class NativeHostRenderer(NativeHostRenderer.Factory factory) : IDisposable
{
    private const int RecreateTarget = unchecked((int)0x8899000C);
    private const long ClickRippleMs = 420;
    private const long ScrollPulseMs = 380;
    private const long DragPulseMs = 320;
    private const long MovePulseMs = 260;

    private static readonly D2dPixelFormat BgraOpaque = new(87, 3); // B8G8R8A8_UNORM, ALPHA_MODE_IGNORE
    private static readonly D2dPixelFormat BgraPremultiplied = new(87, 1); // B8G8R8A8_UNORM, ALPHA_MODE_PREMULTIPLIED
    private static readonly D2dColor Black = new(0, 0, 0, 1);

    private sealed record Frame(PixelSize Size, byte[] Pixels);
    private readonly record struct BannerCacheKey(string Label, bool Hovered, int ScaleKey, int MaxPillWidth);

    private Frame? _frame;
    private bool _upload;
    private nint _target;
    private nint _bitmap;
    private PixelSize _targetSize;
    private PixelSize _bitmapSize;
    private CreateBitmap? _createBitmap;
    private DrawBitmap? _drawBitmap;
    private Clear? _clear;
    private BeginDraw? _beginDraw;
    private EndDraw? _endDraw;
    private Resize? _resize;
    private CopyFromMemory? _copyFromMemory;

    // AI takeover overlay state & cached D2D1 bitmaps
    private NativeHost.AgentOverlayState _overlay = new(false, null, 0, 0, false, "idle");
    private bool _bannerHovered;
    private NativeHostInterop.NativeRect _bannerHitRect;
    private readonly List<long> _clickRippleTicks = new(4);
    private long _scrollPulseTick;
    private long _dragPulseTick;
    private long _movePulseTick;

    private int _edgeScaleKey;
    private int _edgeThickness;
    private nint _edgeTop, _edgeBottom, _edgeLeft, _edgeRight;
    private PixelSize _edgeTopSize, _edgeBottomSize, _edgeLeftSize, _edgeRightSize;

    private BannerCacheKey _bannerCacheKey;
    private nint _bannerBitmap;
    private PixelSize _bannerBitmapSize;
    private int _bannerPillWidth, _bannerPillHeight, _bannerGlowPad, _bannerTopMargin;

    private int _cursorScaleKey;
    private nint _cursorBitmap;
    private PixelSize _cursorBitmapSize;
    private float _cursorHotspot;

    private nint _effectBitmap;
    private PixelSize _effectBitmapSize;

    internal bool IsAgentOverlayActive => _overlay.Active;

    internal void SetFrame(int width, int height, byte[] pixels)
    {
        ReturnFrame();
        _frame = new(new PixelSize((uint)width, (uint)height), pixels);
        _upload = true;
    }

    internal void SetAgentOverlay(NativeHost.AgentOverlayState state)
    {
        long now = Environment.TickCount64;
        if (!state.Active)
        {
            _clickRippleTicks.Clear();
            _scrollPulseTick = 0;
            _dragPulseTick = 0;
            _movePulseTick = 0;
            _bannerHitRect = default;
        }
        else if (state.PointerVisible)
        {
            switch (state.PointerState)
            {
                case "click":
                    if (_clickRippleTicks.Count >= 4)
                        _clickRippleTicks.RemoveAt(0);
                    _clickRippleTicks.Add(now);
                    break;
                case "scroll":
                    _scrollPulseTick = now;
                    break;
                case "drag":
                    _dragPulseTick = now;
                    break;
                case "move":
                    _movePulseTick = now;
                    break;
            }
        }
        _overlay = state;
    }

    internal void SetBannerHovered(bool hovered) => _bannerHovered = hovered;

    internal bool HitTestBanner(int clientX, int clientY) =>
        _overlay.Active
        && _bannerHitRect.Width > 0
        && _bannerHitRect.Height > 0
        && clientX >= _bannerHitRect.Left
        && clientX < _bannerHitRect.Right
        && clientY >= _bannerHitRect.Top
        && clientY < _bannerHitRect.Bottom;

    internal bool HasActiveAnimation()
    {
        if (!_overlay.Active || !_overlay.PointerVisible)
            return false;
        long now = Environment.TickCount64;
        PruneExpiredRipples(now);
        return _clickRippleTicks.Count > 0
            || (_scrollPulseTick != 0 && now - _scrollPulseTick < ScrollPulseMs)
            || (_dragPulseTick != 0 && now - _dragPulseTick < DragPulseMs)
            || (_movePulseTick != 0 && now - _movePulseTick < MovePulseMs);
    }

    internal void Draw(nint hwnd, int width, int height, double scale = 1.0)
    {
        var size = new PixelSize((uint)width, (uint)height);
        double effectiveScale = scale > 0.1 ? scale : 1.0;
        int hr = DrawFrame(hwnd, size, effectiveScale);
        if (hr == RecreateTarget)
        {
            ReleaseTarget();
            hr = DrawFrame(hwnd, size, effectiveScale);
        }
        if (hr < 0)
        {
            ReleaseTarget();
            Console.Error.WriteLine($"FocusStage window host Direct2D: draw failed (0x{hr:X8}).");
        }
    }

    private int DrawFrame(nint hwnd, PixelSize size, double scale)
    {
        if (_target == 0)
        {
            int hr = factory.CreateTarget(hwnd, size, out _target);
            if (hr < 0)
                return hr;
            // Slots include IUnknown and the inherited ID2D1Resource and ID2D1RenderTarget methods.
            _createBitmap = Method<CreateBitmap>(_target, 4);
            _drawBitmap = Method<DrawBitmap>(_target, 26);
            _clear = Method<Clear>(_target, 47);
            _beginDraw = Method<BeginDraw>(_target, 48);
            _endDraw = Method<EndDraw>(_target, 49);
            _resize = Method<Resize>(_target, 58);
            _targetSize = size;
        }
        else if (_targetSize != size)
        {
            int hr = _resize!(_target, in size);
            if (hr < 0)
                return hr;
            _targetSize = size;
        }

        if (_frame is { } frame && _upload)
        {
            int hr = Upload(frame);
            if (hr < 0)
                return hr;
            _upload = false;
        }

        bool drawEffect = false;
        long now = Environment.TickCount64;
        if (_overlay.Active)
        {
            int hr = EnsureEdgeGlowBitmaps(scale);
            if (hr < 0)
                return hr;
            hr = EnsureBannerBitmap((int)size.Width, scale);
            if (hr < 0)
                return hr;
            if (_overlay.PointerVisible)
            {
                hr = EnsureCursorBitmap(scale);
                if (hr < 0)
                    return hr;
                hr = UpdateEffectBitmap(scale, now, out drawEffect);
                if (hr < 0)
                    return hr;
            }
        }

        _beginDraw!(_target);
        if (_bitmap != 0)
        {
            var destination = new FloatRect(0, 0, size.Width, size.Height);
            _drawBitmap!(_target, _bitmap, in destination, 1, 1, 0); // Linear interpolation
        }
        else
        {
            _clear!(_target, in Black);
        }

        if (_overlay.Active)
        {
            DrawAgentOverlay(size, scale, drawEffect);
        }

        return _endDraw!(_target, 0, 0);
    }

    private void DrawAgentOverlay(PixelSize size, double scale, bool drawEffect)
    {
        float w = size.Width;
        float h = size.Height;
        float g = Math.Min(_edgeThickness, Math.Min(w, h) * 0.5f);

        // 1. Soft multi-layer luminous blue inner border along all 4 client edges.
        if (_edgeTop != 0 && g > 0)
        {
            var topRect = new FloatRect(0, 0, w, g);
            _drawBitmap!(_target, _edgeTop, in topRect, 1, 1, 0);
            var bottomRect = new FloatRect(0, h - g, w, h);
            _drawBitmap!(_target, _edgeBottom, in bottomRect, 1, 1, 0);
            var leftRect = new FloatRect(0, 0, g, h);
            _drawBitmap!(_target, _edgeLeft, in leftRect, 1, 1, 0);
            var rightRect = new FloatRect(w - g, 0, w, h);
            _drawBitmap!(_target, _edgeRight, in rightRect, 1, 1, 0);
        }

        // 2. AI virtual cursor & mouse action effects (drawn beneath the top banner so the banner stays readable).
        if (_overlay.PointerVisible && _cursorBitmap != 0)
        {
            float px = (float)(_overlay.PointerX * scale);
            float py = (float)(_overlay.PointerY * scale);

            if (drawEffect && _effectBitmap != 0)
            {
                float half = _effectBitmapSize.Width * 0.5f;
                var effRect = new FloatRect(px - half, py - half, px + half, py + half);
                _drawBitmap!(_target, _effectBitmap, in effRect, 1, 1, 0);
            }

            float left = px - _cursorHotspot;
            float top = py - _cursorHotspot;
            var curRect = new FloatRect(left, top, left + _cursorBitmapSize.Width, top + _cursorBitmapSize.Height);
            _drawBitmap!(_target, _cursorBitmap, in curRect, 1, 1, 0);
        }

        // 3. Top-center glowing blue pill banner.
        if (_bannerBitmap != 0 && _bannerPillWidth > 0)
        {
            int pillLeft = ((int)size.Width - _bannerPillWidth) / 2;
            int pillTop = _bannerTopMargin;
            _bannerHitRect = new NativeHostInterop.NativeRect(
                pillLeft, pillTop, pillLeft + _bannerPillWidth, pillTop + _bannerPillHeight);

            float bmpLeft = pillLeft - _bannerGlowPad;
            float bmpTop = pillTop - _bannerGlowPad;
            var bannerRect = new FloatRect(
                bmpLeft, bmpTop, bmpLeft + _bannerBitmapSize.Width, bmpTop + _bannerBitmapSize.Height);
            _drawBitmap!(_target, _bannerBitmap, in bannerRect, 1, 1, 0);
        }
    }

    private int EnsureEdgeGlowBitmaps(double scale)
    {
        int scaleKey = ScaleKey(scale);
        if (_edgeTop != 0 && _edgeScaleKey == scaleKey)
            return 0;

        int g = Math.Max(18, Round(26 * scale));
        int rim = Math.Max(2, Round(2.5 * scale));
        const int StripSpan = 4;

        var topPixels = new byte[StripSpan * g * 4];
        var bottomPixels = new byte[StripSpan * g * 4];
        var leftPixels = new byte[g * StripSpan * 4];
        var rightPixels = new byte[g * StripSpan * 4];

        for (int d = 0; d < g; d++)
        {
            ComputeEdgeGlowPixel(d, g, rim, out byte pb, out byte pg, out byte pr, out byte pa);

            // Top: d=0 at y=0; Bottom: d=0 at y=g-1
            int topRow = d * StripSpan * 4;
            int bottomRow = (g - 1 - d) * StripSpan * 4;
            for (int x = 0; x < StripSpan; x++)
            {
                WritePixel(topPixels, topRow + x * 4, pb, pg, pr, pa);
                WritePixel(bottomPixels, bottomRow + x * 4, pb, pg, pr, pa);
            }

            // Left: d=0 at x=0; Right: d=0 at x=g-1
            for (int y = 0; y < StripSpan; y++)
            {
                WritePixel(leftPixels, (y * g + d) * 4, pb, pg, pr, pa);
                WritePixel(rightPixels, (y * g + (g - 1 - d)) * 4, pb, pg, pr, pa);
            }
        }

        int hr = UploadPbgraBytes(ref _edgeTop, ref _edgeTopSize, StripSpan, g, topPixels);
        if (hr < 0) return hr;
        hr = UploadPbgraBytes(ref _edgeBottom, ref _edgeBottomSize, StripSpan, g, bottomPixels);
        if (hr < 0) return hr;
        hr = UploadPbgraBytes(ref _edgeLeft, ref _edgeLeftSize, g, StripSpan, leftPixels);
        if (hr < 0) return hr;
        hr = UploadPbgraBytes(ref _edgeRight, ref _edgeRightSize, g, StripSpan, rightPixels);
        if (hr < 0) return hr;

        _edgeThickness = g;
        _edgeScaleKey = scaleKey;
        return 0;
    }

    private static void ComputeEdgeGlowPixel(
        int d, int thickness, int rim, out byte pb, out byte pg, out byte pr, out byte pa)
    {
        double alpha;
        double r, g, b;
        if (d < rim)
        {
            double t = rim > 1 ? d / (double)(rim - 1) : 0;
            alpha = 0.94 - 0.16 * t;
            r = 104 - 36 * t;
            g = 205 - 38 * t;
            b = 255;
        }
        else
        {
            double span = Math.Max(1, thickness - rim);
            double u = Math.Clamp(1.0 - (d - rim) / span, 0.0, 1.0);
            alpha = 0.52 * u * u * u + 0.14 * u * u;
            r = 29 + 36 * u;
            g = 112 + 56 * u;
            b = 246 + 9 * u;
        }
        pa = (byte)Math.Clamp(Math.Round(alpha * 255.0), 0, 255);
        pr = (byte)Math.Clamp(Math.Round(r * alpha), 0, 255);
        pg = (byte)Math.Clamp(Math.Round(g * alpha), 0, 255);
        pb = (byte)Math.Clamp(Math.Round(b * alpha), 0, 255);
    }

    private static void WritePixel(byte[] buffer, int offset, byte b, byte g, byte r, byte a)
    {
        buffer[offset] = b;
        buffer[offset + 1] = g;
        buffer[offset + 2] = r;
        buffer[offset + 3] = a;
    }

    private int EnsureBannerBitmap(int clientWidth, double scale)
    {
        string? agent = string.IsNullOrWhiteSpace(_overlay.AgentName) ? null : _overlay.AgentName.Trim();
        string label = agent is not null
            ? $"AI 正在接管舞台 ({agent}) · 按 Esc 退出"
            : "AI 正在接管舞台 · 按 Esc 退出";

        int scaleKey = ScaleKey(scale);
        int marginX = Math.Max(20, Round(28 * scale));
        int maxPillW = Math.Max(140, clientWidth - marginX * 2);
        var key = new BannerCacheKey(label, _bannerHovered, scaleKey, maxPillW);
        if (_bannerBitmap != 0 && _bannerCacheKey == key)
            return 0;

        float s = (float)scale;
        int glowPad = Math.Max(10, Round(14 * scale));
        int pillH = Math.Max(28, Round(34 * scale));
        int topMargin = Math.Max(10, Round(12 * scale));
        float padLeft = 14f * s;
        float dotDiameter = 8f * s;
        float dotGap = 9f * s;
        float closeGap = 10f * s;
        float closeSize = 8f * s;
        float padRight = 13f * s;

        using var font = CreateUiFont(12.5f * s);
        float textWidth;
        using (var measureBmp = new Bitmap(1, 1, System.Drawing.Imaging.PixelFormat.Format32bppPArgb))
        using (var measureG = Graphics.FromImage(measureBmp))
        {
            measureG.TextRenderingHint = TextRenderingHint.AntiAliasGridFit;
            using var format = new StringFormat(StringFormat.GenericTypographic)
            {
                FormatFlags = StringFormatFlags.NoWrap | StringFormatFlags.MeasureTrailingSpaces,
            };
            textWidth = measureG.MeasureString(label, font, int.MaxValue, format).Width;
        }

        float chromeWidth = padLeft + dotDiameter + dotGap + closeGap + closeSize + padRight;
        int pillW = Math.Clamp((int)Math.Ceiling(textWidth + chromeWidth + 4f * s), Math.Min(140, maxPillW), maxPillW);
        int bmpW = pillW + glowPad * 2;
        int bmpH = pillH + glowPad * 2;

        using var bmp = new Bitmap(bmpW, bmpH, System.Drawing.Imaging.PixelFormat.Format32bppPArgb);
        using (var g = Graphics.FromImage(bmp))
        {
            g.Clear(System.Drawing.Color.Transparent);
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;
            g.TextRenderingHint = TextRenderingHint.AntiAliasGridFit;

            var pillRect = new RectangleF(glowPad, glowPad, pillW, pillH);

            // Soft layered outer luminous blue halo around the capsule.
            for (int layer = 5; layer >= 1; layer--)
            {
                float expand = layer * 2.3f * s;
                int alpha = (int)Math.Round((_bannerHovered ? 38 : 28) * Math.Pow(1.0 - (layer - 1) / 5.5, 1.5));
                var haloRect = RectangleF.Inflate(pillRect, expand, expand);
                using var haloPath = CreateCapsulePath(haloRect);
                using var haloBrush = new SolidBrush(System.Drawing.Color.FromArgb(alpha, 56, 152, 255));
                g.FillPath(haloBrush, haloPath);
            }

            // Deep translucent midnight-to-royal blue pill gradient fill.
            using var pillPath = CreateCapsulePath(pillRect);
            var gradTop = _bannerHovered
                ? System.Drawing.Color.FromArgb(242, 24, 56, 130)
                : System.Drawing.Color.FromArgb(234, 15, 34, 78);
            var gradBottom = _bannerHovered
                ? System.Drawing.Color.FromArgb(246, 37, 99, 235)
                : System.Drawing.Color.FromArgb(240, 22, 60, 146);
            using (var fillBrush = new LinearGradientBrush(pillRect, gradTop, gradBottom, LinearGradientMode.Vertical))
            {
                g.FillPath(fillBrush, pillPath);
            }

            // Subtle upper specular highlight inside the capsule.
            var glossRect = new RectangleF(pillRect.X + 1.5f * s, pillRect.Y + 1f * s, pillRect.Width - 3f * s, pillRect.Height * 0.52f);
            using (var glossPath = CreateCapsulePath(glossRect))
            using (var glossBrush = new LinearGradientBrush(
                glossRect,
                System.Drawing.Color.FromArgb(_bannerHovered ? 58 : 42, 186, 230, 253),
                System.Drawing.Color.FromArgb(0, 186, 230, 253),
                LinearGradientMode.Vertical))
            {
                g.FillPath(glossBrush, glossPath);
            }

            // Crisp luminous cyan-blue border stroke.
            using (var borderPen = new Pen(
                System.Drawing.Color.FromArgb(_bannerHovered ? 250 : 218, 110, 200, 255),
                Math.Max(1f, 1.35f * s)))
            {
                g.DrawPath(borderPen, pillPath);
            }

            // Glowing left status indicator dot.
            float dotCx = pillRect.X + padLeft + dotDiameter * 0.5f;
            float dotCy = pillRect.Y + pillRect.Height * 0.5f;
            float haloR = dotDiameter * 1.2f;
            using (var dotGlowPath = new GraphicsPath())
            {
                dotGlowPath.AddEllipse(dotCx - haloR, dotCy - haloR, haloR * 2, haloR * 2);
                using var dotGlow = new PathGradientBrush(dotGlowPath)
                {
                    CenterColor = System.Drawing.Color.FromArgb(190, 56, 189, 248),
                    SurroundColors = [System.Drawing.Color.FromArgb(0, 56, 189, 248)],
                };
                g.FillEllipse(dotGlow, dotCx - haloR, dotCy - haloR, haloR * 2, haloR * 2);
            }
            float dotR = dotDiameter * 0.5f;
            using (var dotBrush = new SolidBrush(System.Drawing.Color.FromArgb(255, 125, 224, 255)))
            {
                g.FillEllipse(dotBrush, dotCx - dotR, dotCy - dotR, dotDiameter, dotDiameter);
            }

            // Crisp white/ice-white banner label.
            float textLeft = pillRect.X + padLeft + dotDiameter + dotGap;
            float textRight = pillRect.Right - padRight - closeSize - closeGap;
            var textRect = new RectangleF(textLeft, pillRect.Y, Math.Max(10f, textRight - textLeft), pillRect.Height);
            using (var textBrush = new SolidBrush(System.Drawing.Color.FromArgb(250, 244, 249, 255)))
            using (var sf = new StringFormat(StringFormat.GenericTypographic)
            {
                Alignment = StringAlignment.Near,
                LineAlignment = StringAlignment.Center,
                Trimming = StringTrimming.EllipsisCharacter,
                FormatFlags = StringFormatFlags.NoWrap,
            })
            {
                g.DrawString(label, font, textBrush, textRect, sf);
            }

            // Subtle clickable dismiss '×' icon on the right.
            float closeCx = pillRect.Right - padRight - closeSize * 0.5f;
            float closeCy = dotCy;
            float halfX = closeSize * 0.42f;
            using var closePen = new Pen(
                System.Drawing.Color.FromArgb(_bannerHovered ? 245 : 185, 210, 236, 255),
                Math.Max(1.2f, 1.5f * s))
            {
                StartCap = LineCap.Round,
                EndCap = LineCap.Round,
            };
            g.DrawLine(closePen, closeCx - halfX, closeCy - halfX, closeCx + halfX, closeCy + halfX);
            g.DrawLine(closePen, closeCx + halfX, closeCy - halfX, closeCx - halfX, closeCy + halfX);
        }

        int hr = UploadGdiBitmap(ref _bannerBitmap, ref _bannerBitmapSize, bmp);
        if (hr < 0)
            return hr;

        _bannerCacheKey = key;
        _bannerPillWidth = pillW;
        _bannerPillHeight = pillH;
        _bannerGlowPad = glowPad;
        _bannerTopMargin = topMargin;
        return 0;
    }

    private int EnsureCursorBitmap(double scale)
    {
        int scaleKey = ScaleKey(scale);
        if (_cursorBitmap != 0 && _cursorScaleKey == scaleKey)
            return 0;

        float s = (float)scale;
        int canvas = Math.Max(68, Round(88 * scale));
        float hot = canvas * 0.5f;

        using var bmp = new Bitmap(canvas, canvas, System.Drawing.Imaging.PixelFormat.Format32bppPArgb);
        using (var g = Graphics.FromImage(bmp))
        {
            g.Clear(System.Drawing.Color.Transparent);
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;

            // Soft blue radial glow halo around the AI virtual cursor.
            float glowCx = hot + 4.5f * s;
            float glowCy = hot + 7.5f * s;
            float glowR = 27f * s;
            using (var glowPath = new GraphicsPath())
            {
                glowPath.AddEllipse(glowCx - glowR, glowCy - glowR, glowR * 2, glowR * 2);
                using var glowBrush = new PathGradientBrush(glowPath)
                {
                    CenterPoint = new PointF(glowCx, glowCy),
                    CenterColor = System.Drawing.Color.FromArgb(148, 56, 189, 248),
                    SurroundColors = [System.Drawing.Color.FromArgb(0, 37, 99, 235)],
                };
                glowBrush.InterpolationColors = new ColorBlend
                {
                    Colors =
                    [
                        System.Drawing.Color.FromArgb(0, 37, 99, 235),
                        System.Drawing.Color.FromArgb(62, 37, 99, 235),
                        System.Drawing.Color.FromArgb(148, 56, 189, 248),
                    ],
                    Positions = [0f, 0.52f, 1f],
                };
                g.FillEllipse(glowBrush, glowCx - glowR, glowCy - glowR, glowR * 2, glowR * 2);
            }

            PointF[] arrow =
            [
                new(hot, hot),
                new(hot, hot + 19.5f * s),
                new(hot + 4.8f * s, hot + 15.0f * s),
                new(hot + 8.3f * s, hot + 22.4f * s),
                new(hot + 11.4f * s, hot + 20.9f * s),
                new(hot + 7.9f * s, hot + 13.7f * s),
                new(hot + 14.4f * s, hot + 13.4f * s),
            ];

            // Subtle dark drop shadow for crisp visibility on light surfaces.
            PointF[] shadowArrow = new PointF[arrow.Length];
            for (int i = 0; i < arrow.Length; i++)
                shadowArrow[i] = new PointF(arrow[i].X + 1.2f * s, arrow[i].Y + 1.8f * s);
            using (var shadowBrush = new SolidBrush(System.Drawing.Color.FromArgb(92, 8, 18, 42)))
            {
                g.FillPolygon(shadowBrush, shadowArrow);
            }

            // Crisp pointer arrow with electric-blue outer stroke and luminous white fill.
            using (var outerPen = new Pen(System.Drawing.Color.FromArgb(245, 29, 78, 216), Math.Max(2f, 2.4f * s))
            {
                LineJoin = LineJoin.Round,
            })
            {
                g.DrawPolygon(outerPen, arrow);
            }
            using (var fillBrush = new SolidBrush(System.Drawing.Color.FromArgb(255, 255, 255, 255)))
            {
                g.FillPolygon(fillBrush, arrow);
            }
            using (var accentPen = new Pen(System.Drawing.Color.FromArgb(220, 56, 189, 248), Math.Max(0.9f, 1.0f * s))
            {
                LineJoin = LineJoin.Round,
            })
            {
                g.DrawPolygon(accentPen, arrow);
            }
        }

        int hr = UploadGdiBitmap(ref _cursorBitmap, ref _cursorBitmapSize, bmp);
        if (hr < 0)
            return hr;

        _cursorHotspot = hot;
        _cursorScaleKey = scaleKey;
        return 0;
    }

    private int UpdateEffectBitmap(double scale, long now, out bool hasEffect)
    {
        PruneExpiredRipples(now);

        bool isDown = _overlay.PointerState == "down";
        bool isDrag = _overlay.PointerState == "drag" || (_dragPulseTick != 0 && now - _dragPulseTick < DragPulseMs);
        bool isScroll = _scrollPulseTick != 0 && now - _scrollPulseTick < ScrollPulseMs;
        bool isMovePulse = _movePulseTick != 0 && now - _movePulseTick < MovePulseMs;
        bool hasRipples = _clickRippleTicks.Count > 0;

        hasEffect = isDown || isDrag || isScroll || isMovePulse || hasRipples;
        if (!hasEffect)
            return 0;

        float s = (float)scale;
        int canvas = Math.Max(80, Round(104 * scale));
        float cx = canvas * 0.5f;
        float cy = canvas * 0.5f;

        using var bmp = new Bitmap(canvas, canvas, System.Drawing.Imaging.PixelFormat.Format32bppPArgb);
        using (var g = Graphics.FromImage(bmp))
        {
            g.Clear(System.Drawing.Color.Transparent);
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;

            // Gentle luminous pulse halo on pointer move.
            if (isMovePulse)
            {
                double t = Math.Clamp((now - _movePulseTick) / (double)MovePulseMs, 0.0, 1.0);
                double fade = 1.0 - t;
                float r = (float)((11.0 + 11.0 * t) * s);
                int alpha = (int)Math.Round(105 * fade * fade);
                if (alpha > 1)
                {
                    using var movePen = new Pen(System.Drawing.Color.FromArgb(alpha, 96, 185, 255), Math.Max(1.2f, 1.6f * s));
                    g.DrawEllipse(movePen, cx - r, cy - r, r * 2, r * 2);
                }
            }

            // Solid pressed ring on mouse down.
            if (isDown)
            {
                float r = 11.5f * s;
                using var fill = new SolidBrush(System.Drawing.Color.FromArgb(62, 56, 189, 248));
                using var ring = new Pen(System.Drawing.Color.FromArgb(230, 56, 189, 248), Math.Max(1.8f, 2.2f * s));
                g.FillEllipse(fill, cx - r, cy - r, r * 2, r * 2);
                g.DrawEllipse(ring, cx - r, cy - r, r * 2, r * 2);
            }

            // Glowing drag indicator ring.
            if (isDrag)
            {
                double fade = _overlay.PointerState == "drag"
                    ? 1.0
                    : Math.Clamp(1.0 - (now - _dragPulseTick) / (double)DragPulseMs, 0.0, 1.0);
                float r = 16.5f * s;
                using var glowFill = new SolidBrush(System.Drawing.Color.FromArgb((int)Math.Round(48 * fade), 37, 99, 235));
                using var outerHalo = new Pen(System.Drawing.Color.FromArgb((int)Math.Round(110 * fade), 56, 189, 248), Math.Max(3.2f, 4.0f * s));
                using var innerRing = new Pen(System.Drawing.Color.FromArgb((int)Math.Round(235 * fade), 125, 224, 255), Math.Max(1.6f, 2.0f * s))
                {
                    DashStyle = DashStyle.Dash,
                };
                g.FillEllipse(glowFill, cx - r, cy - r, r * 2, r * 2);
                g.DrawEllipse(outerHalo, cx - r, cy - r, r * 2, r * 2);
                g.DrawEllipse(innerRing, cx - r, cy - r, r * 2, r * 2);
            }

            // Expanding click ripple ring(s).
            foreach (long startTick in _clickRippleTicks)
            {
                double t = Math.Clamp((now - startTick) / (double)ClickRippleMs, 0.0, 1.0);
                double easeOut = 1.0 - Math.Pow(1.0 - t, 3.0);
                double fade = Math.Pow(1.0 - t, 1.35);

                if (t < 0.28)
                {
                    float flashR = (float)((7.0 + 9.0 * (t / 0.28)) * s);
                    int flashAlpha = (int)Math.Round(120 * (1.0 - t / 0.28));
                    using var flashBrush = new SolidBrush(System.Drawing.Color.FromArgb(flashAlpha, 56, 189, 248));
                    g.FillEllipse(flashBrush, cx - flashR, cy - flashR, flashR * 2, flashR * 2);
                }

                float outerR = (float)((5.0 + 31.0 * easeOut) * s);
                int haloAlpha = (int)Math.Round(110 * fade);
                int ringAlpha = (int)Math.Round(240 * fade);
                if (haloAlpha > 1)
                {
                    using var haloPen = new Pen(System.Drawing.Color.FromArgb(haloAlpha, 37, 99, 235), Math.Max(3.5f, 4.8f * s));
                    g.DrawEllipse(haloPen, cx - outerR, cy - outerR, outerR * 2, outerR * 2);
                }
                if (ringAlpha > 1)
                {
                    using var ringPen = new Pen(System.Drawing.Color.FromArgb(ringAlpha, 125, 224, 255), Math.Max(1.6f, 2.1f * s));
                    g.DrawEllipse(ringPen, cx - outerR, cy - outerR, outerR * 2, outerR * 2);
                }

                float innerR = (float)((3.0 + 17.0 * easeOut) * s);
                int innerAlpha = (int)Math.Round(165 * fade * fade);
                if (innerAlpha > 1)
                {
                    using var innerPen = new Pen(System.Drawing.Color.FromArgb(innerAlpha, 56, 189, 248), Math.Max(1.1f, 1.4f * s));
                    g.DrawEllipse(innerPen, cx - innerR, cy - innerR, innerR * 2, innerR * 2);
                }
            }

            // Scroll indicator: glowing blue ring + crisp up/down chevrons.
            if (isScroll)
            {
                double fade = _scrollPulseTick == 0
                    ? 1.0
                    : Math.Clamp(1.0 - (now - _scrollPulseTick) / (double)ScrollPulseMs, 0.0, 1.0);
                if (fade > 0.02)
                {
                    float r = 14.5f * s;
                    using var scrollRing = new Pen(System.Drawing.Color.FromArgb((int)Math.Round(210 * fade), 56, 189, 248), Math.Max(1.6f, 2.0f * s));
                    g.DrawEllipse(scrollRing, cx - r, cy - r, r * 2, r * 2);

                    float chevX = cx + 21f * s;
                    float chevW = 4.2f * s;
                    float chevH = 3.2f * s;
                    using var chevPen = new Pen(System.Drawing.Color.FromArgb((int)Math.Round(240 * fade), 186, 230, 253), Math.Max(1.6f, 2.0f * s))
                    {
                        StartCap = LineCap.Round,
                        EndCap = LineCap.Round,
                        LineJoin = LineJoin.Round,
                    };
                    // Up chevron
                    g.DrawLines(chevPen, [
                        new PointF(chevX - chevW, cy - 3.5f * s),
                        new PointF(chevX, cy - 3.5f * s - chevH),
                        new PointF(chevX + chevW, cy - 3.5f * s),
                    ]);
                    // Down chevron
                    g.DrawLines(chevPen, [
                        new PointF(chevX - chevW, cy + 3.5f * s),
                        new PointF(chevX, cy + 3.5f * s + chevH),
                        new PointF(chevX + chevW, cy + 3.5f * s),
                    ]);
                }
            }
        }

        return UploadGdiBitmap(ref _effectBitmap, ref _effectBitmapSize, bmp);
    }

    private void PruneExpiredRipples(long now)
    {
        for (int i = _clickRippleTicks.Count - 1; i >= 0; i--)
        {
            if (now - _clickRippleTicks[i] >= ClickRippleMs)
                _clickRippleTicks.RemoveAt(i);
        }
    }

    private static GraphicsPath CreateCapsulePath(RectangleF rect)
    {
        float diameter = Math.Min(rect.Width, rect.Height);
        var path = new GraphicsPath();
        if (diameter <= 0.5f)
        {
            path.AddRectangle(rect);
            return path;
        }
        path.AddArc(rect.X, rect.Y, diameter, diameter, 90, 180);
        path.AddArc(rect.Right - diameter, rect.Y, diameter, diameter, 270, 180);
        path.CloseFigure();
        return path;
    }

    private static Font CreateUiFont(float pixelSize)
    {
        try
        {
            return new Font("Microsoft YaHei UI", pixelSize, FontStyle.Regular, GraphicsUnit.Pixel);
        }
        catch
        {
            return new Font("Segoe UI", pixelSize, FontStyle.Regular, GraphicsUnit.Pixel);
        }
    }

    private int UploadGdiBitmap(ref nint d2dBitmap, ref PixelSize currentSize, Bitmap bitmap)
    {
        var rect = new Rectangle(0, 0, bitmap.Width, bitmap.Height);
        BitmapData data = bitmap.LockBits(rect, ImageLockMode.ReadOnly, System.Drawing.Imaging.PixelFormat.Format32bppPArgb);
        try
        {
            var pixelSize = new PixelSize((uint)bitmap.Width, (uint)bitmap.Height);
            uint pitch = (uint)data.Stride;
            if (d2dBitmap != 0 && currentSize == pixelSize)
            {
                _copyFromMemory ??= Method<CopyFromMemory>(d2dBitmap, 10);
                return _copyFromMemory(d2dBitmap, 0, data.Scan0, pitch);
            }

            Release(ref d2dBitmap);
            var properties = new BitmapProperties(BgraPremultiplied, 96, 96);
            int hr = _createBitmap!(_target, pixelSize, data.Scan0, pitch, in properties, out d2dBitmap);
            if (hr >= 0)
            {
                _copyFromMemory ??= Method<CopyFromMemory>(d2dBitmap, 10);
                currentSize = pixelSize;
            }
            return hr;
        }
        finally
        {
            bitmap.UnlockBits(data);
        }
    }

    private int UploadPbgraBytes(ref nint d2dBitmap, ref PixelSize currentSize, int width, int height, byte[] pixels)
    {
        uint pitch = (uint)(width * 4);
        var pixelSize = new PixelSize((uint)width, (uint)height);
        var pin = GCHandle.Alloc(pixels, GCHandleType.Pinned);
        try
        {
            nint source = pin.AddrOfPinnedObject();
            if (d2dBitmap != 0 && currentSize == pixelSize)
            {
                _copyFromMemory ??= Method<CopyFromMemory>(d2dBitmap, 10);
                return _copyFromMemory(d2dBitmap, 0, source, pitch);
            }

            Release(ref d2dBitmap);
            var properties = new BitmapProperties(BgraPremultiplied, 96, 96);
            int hr = _createBitmap!(_target, pixelSize, source, pitch, in properties, out d2dBitmap);
            if (hr >= 0)
            {
                _copyFromMemory ??= Method<CopyFromMemory>(d2dBitmap, 10);
                currentSize = pixelSize;
            }
            return hr;
        }
        finally
        {
            pin.Free();
        }
    }

    private int Upload(Frame frame)
    {
        uint pitch = frame.Size.Width * 4;
        var pin = GCHandle.Alloc(frame.Pixels, GCHandleType.Pinned);
        try
        {
            nint source = pin.AddrOfPinnedObject();
            if (_bitmap != 0 && frame.Size == _bitmapSize)
                return _copyFromMemory!(_bitmap, 0, source, pitch);

            Release(ref _bitmap);
            var properties = new BitmapProperties(BgraOpaque, 96, 96);
            int hr = _createBitmap!(_target, frame.Size, source, pitch, in properties, out _bitmap);
            if (hr >= 0)
            {
                _copyFromMemory = Method<CopyFromMemory>(_bitmap, 10);
                _bitmapSize = frame.Size;
            }
            return hr;
        }
        finally
        {
            pin.Free();
        }
    }

    private void ReleaseTarget()
    {
        Release(ref _bitmap);
        Release(ref _edgeTop);
        Release(ref _edgeBottom);
        Release(ref _edgeLeft);
        Release(ref _edgeRight);
        Release(ref _bannerBitmap);
        Release(ref _cursorBitmap);
        Release(ref _effectBitmap);
        Release(ref _target);
        _copyFromMemory = null;
        _edgeScaleKey = 0;
        _cursorScaleKey = 0;
        _bannerCacheKey = default;
        _effectBitmapSize = default;
        _upload = _frame is not null;
    }

    private void ReturnFrame()
    {
        if (_frame is { } frame)
        {
            _frame = null;
            ArrayPool<byte>.Shared.Return(frame.Pixels);
        }
    }

    public void Dispose()
    {
        ReturnFrame();
        ReleaseTarget();
    }

    private static int ScaleKey(double scale) => Math.Max(10, (int)Math.Round(scale * 100.0));
    private static int Round(double value) => (int)Math.Round(value, MidpointRounding.AwayFromZero);

    internal sealed class Factory : IDisposable
    {
        private static readonly Guid FactoryId = new("06152247-6F50-465A-9245-118BFD3B6007");
        private readonly CreateHwndRenderTarget _createTarget;
        private nint _factory;

        internal Factory()
        {
            Marshal.ThrowExceptionForHR(D2D1CreateFactory(0, in FactoryId, 0, out _factory));
            _createTarget = Method<CreateHwndRenderTarget>(_factory, 14);
        }

        internal int CreateTarget(nint hwnd, PixelSize size, out nint target)
        {
            // Use 96 DPI so draw coordinates and target/bitmap pixel sizes use the same units.
            var properties = new RenderTargetProperties(0, BgraOpaque, 96, 96, 0, 0);
            // Let DWM pace desktop composition without blocking the window's input thread.
            const int PresentImmediately = 2; // D2D1_PRESENT_OPTIONS_IMMEDIATELY
            var hwndProperties = new HwndTargetProperties(hwnd, size, PresentImmediately);
            return _createTarget(_factory, in properties, in hwndProperties, out target);
        }

        public void Dispose() => Release(ref _factory);
    }

    private static T Method<T>(nint instance, int slot) where T : Delegate =>
        Marshal.GetDelegateForFunctionPointer<T>(Marshal.ReadIntPtr(Marshal.ReadIntPtr(instance), slot * nint.Size));

    private static void Release(ref nint instance)
    {
        if (instance != 0)
        {
            Marshal.Release(instance);
            instance = 0;
        }
    }

    [StructLayout(LayoutKind.Sequential)] internal readonly record struct PixelSize(uint Width, uint Height);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct D2dPixelFormat(int Format, int AlphaMode);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct FloatRect(float Left, float Top, float Right, float Bottom);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct D2dColor(float R, float G, float B, float A);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct BitmapProperties(D2dPixelFormat Format, float DpiX, float DpiY);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct RenderTargetProperties(int Type, D2dPixelFormat Format,
        float DpiX, float DpiY, int Usage, int MinimumFeatureLevel);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct HwndTargetProperties(nint Hwnd, PixelSize Size, int PresentOptions);

    [DllImport("d2d1.dll", ExactSpelling = true)]
    private static extern int D2D1CreateFactory(int type, in Guid iid, nint options, out nint factory);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int CreateHwndRenderTarget(nint self, in RenderTargetProperties properties,
        in HwndTargetProperties hwndProperties, out nint target);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int CreateBitmap(nint self, PixelSize size, nint source, uint pitch, in BitmapProperties bitmapProperties, out nint bitmap);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate void DrawBitmap(nint self, nint bitmap, in FloatRect destination, float opacity, int interpolation, nint source);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate void Clear(nint self, in D2dColor color);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate void BeginDraw(nint self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int EndDraw(nint self, nint tag1, nint tag2);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int Resize(nint self, in PixelSize size);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int CopyFromMemory(nint self, nint destination, nint source, uint pitch);
}
