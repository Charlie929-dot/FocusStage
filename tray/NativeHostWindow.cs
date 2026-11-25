using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using static FocusStageTray.NativeHostInterop;

namespace FocusStageTray;

internal sealed partial class NativeHostWindow : NativeWindow, IDisposable
{
    private const int WmActivate = 0x0006, WmKillFocus = 0x0008, WmPaint = 0x000F, WmClose = 0x0010,
        WmEraseBackground = 0x0014, WmShowWindow = 0x0018, WmSetCursor = 0x0020, WmSize = 0x0005,
        WmWindowPosChanged = 0x0047, WmSetIcon = 0x0080,
        WmNcCalcSize = 0x0083, WmNcHitTest = 0x0084, WmNcActivate = 0x0086, WmDisplayChange = 0x007E,
        WmTimer = 0x0113, WmSizing = 0x0214,
        WmEnterSizeMove = 0x0231, WmExitSizeMove = 0x0232, WmDropFiles = 0x0233, WmDpiChanged = 0x02E0,
        WmDwmCompositionChanged = 0x031E;
    private const nuint OverlayTimerId = 1;
    private const uint OverlayTimerIntervalMs = 16;
    private const int DwmExtendedFrameBounds = 9, DwmWindowCornerPreference = 33;
    private const int SmCxSizeFrame = 32, SmCySizeFrame = 33, SmCxPaddedBorder = 92;
    private static readonly int[] ResizeDirections = [4, 3, 5, 2, 8, 6, 7, 1]; // WMSZ_* in protocol order

    private readonly string _id;
    private readonly Action<object> _emit;
    private readonly NativeHostRenderer _renderer;
    private readonly Icon? _appIcon;
    private string? _stageId;
    private WindowState? _lastState;
    private (int Style, WindowPlacement Placement)? _beforeFullscreen;
    private bool IsFullscreen => _beforeFullscreen.HasValue;
    private bool _activated;
    private bool _placing;
    private bool _inSizeMove;
    private bool _sizeCommand;
    private bool _resizing;
    private bool _overlayTimerRunning;
    private bool _bannerHovered;
    private bool _swallowBannerLeftUp;

    internal string? ParentId { get; set; }
    internal nint Owner => GetWindow(Handle, GwOwner);
    private uint Dpi => GetDpiForWindow(Handle) is uint dpi and > 0 ? dpi : 96;
    private double Scale => Dpi / 96.0;
    private bool CanReportGeometry => IsWindowVisible(Handle) && !IsIconic(Handle);

    private readonly record struct WindowState(NativeHost.WindowRect Rect, NativeHost.WindowRect Monitor,
        int PixelWidth, int PixelHeight, double Scale, bool Activated, bool Maximized, bool Fullscreen,
        bool Resizing, int Tiled);

    internal NativeHostWindow(string id, NativeHostRenderer.Factory renderer, Action<object> emit,
        NativeHost.WindowRect initialRect, double initialScale, string wmClass, string? iconPng)
    {
        _id = id;
        _emit = emit;
        _renderer = new NativeHostRenderer(renderer);
        CreateHandle(new CreateParams
        {
            Style = WsOverlappedWindow,
            ClassStyle = 0x0008, // CS_DBLCLKS, so every click in client decorations reaches the app
            X = initialRect.X,
            Y = initialRect.Y,
            Width = ToPixels(initialRect.Width, initialScale),
            Height = ToPixels(initialRect.Height, initialScale),
        });
        // The Linux client handles input composition for this window.
        ImmAssociateContext(Handle, 0);
        _appIcon = LoadAppIcon(wmClass, iconPng);
        nint icon = (_appIcon ?? SystemIcons.Application).Handle;
        SendMessage(Handle, WmSetIcon, 0, icon); // ICON_SMALL
        SendMessage(Handle, WmSetIcon, 1, icon); // ICON_BIG
        ApplyDwmFrame();
        DragAcceptFiles(Handle, true);
    }

    private static Icon? LoadAppIcon(string wmClass, string? iconPng)
    {
        if (!string.IsNullOrEmpty(iconPng))
        {
            try { return ShortcutManager.IconFromPng(iconPng); }
            catch (Exception ex) when (ex is FormatException or ArgumentException or IOException)
            {
                Console.Error.WriteLine($"FocusStage window host: unable to decode desktop icon: {ex.Message}");
            }
        }

        string path = ShortcutManager.AppIconPath(wmClass);
        try
        {
            return File.Exists(path) ? new Icon(path) : null;
        }
        catch (Exception ex) when (ex is ArgumentException or IOException or UnauthorizedAccessException)
        {
            Console.Error.WriteLine($"FocusStage window host: unable to load application icon: {ex.Message}");
            return null;
        }
    }

    internal void SetTitle(string title) => SetWindowText(Handle, title);

    internal void SetStage(string stageId)
    {
        if (stageId == _stageId)
            return;
        SetStageAppId(Handle, stageId);
        _stageId = stageId;
    }

    internal void SetOwner(nint owner)
    {
        if (Owner == owner)
            return;
        for (nint link = owner; link != 0; link = GetWindow(link, GwOwner))
        {
            if (link == Handle)
                return;
        }
        if (SetWindowOwner(Handle, owner) == 0 && Marshal.GetLastWin32Error() is int error and not 0)
            Console.Error.WriteLine($"FocusStage window host: unable to update window owner ({error}).");
    }

    internal void Show(string title, NativeHost.WindowRect rect, bool maximized, bool fullscreen, bool firstShow)
    {
        _placing = true;
        try
        {
            SetTitle(title);
            if (IsFullscreen)
            {
                if (fullscreen)
                {
                    if (IsIconic(Handle))
                        ShowWindow(Handle, SwRestore);
                    if (!IsWindowVisible(Handle))
                        ShowWindow(Handle, SwShow);
                    PlaceFullscreen();
                }
                else
                {
                    ApplyFullscreen(false);
                }
            }
            else
            {
                if (IsIconic(Handle) || (!maximized && IsZoomed(Handle)))
                    ShowWindow(Handle, SwRestore);
                // Preserve the exact HWND placement (including a Snap zone) across Stage hide/show.
                if (!IsZoomed(Handle) && (firstShow || _lastState?.Rect != rect))
                {
                    NativeRect? work = firstShow ? InitialWorkArea() : null;
                    double scale = Scale;
                    Place(rect, scale, work);
                    if (Scale != scale)
                        Place(rect, Scale, work);
                }
                if (maximized && !IsZoomed(Handle))
                    ShowWindow(Handle, SwMaximize);
                else if (!IsWindowVisible(Handle))
                    ShowWindow(Handle, SwShow);
                if (fullscreen)
                    ApplyFullscreen(true);
            }
        }
        finally
        {
            _placing = false;
        }
        // A remapped Linux surface needs the actual client pixels again, even at the same size.
        _lastState = null;
        ReportState();
    }

    internal void Hide()
    {
        CancelInteraction();
        StopOverlayTimer();
        ShowWindow(Handle, SwHide);
        ReportState();
    }

    internal void Activate()
    {
        if (IsIconic(Handle))
            ShowWindow(Handle, SwRestore);
        SetForegroundWindow(Handle);
    }

    internal void Minimize()
    {
        if (!CanReportGeometry)
            return;
        CancelInteraction();
        ShowWindow(Handle, SwMinimize);
        ReportState();
    }

    internal void Maximize(bool maximized)
    {
        if (!IsWindowVisible(Handle) || IsFullscreen)
            return;
        if (IsIconic(Handle) || IsZoomed(Handle) != maximized)
        {
            CancelInteraction();
            ShowWindow(Handle, maximized ? SwMaximize : SwRestore);
            if (!maximized && IsZoomed(Handle))
                ShowWindow(Handle, SwRestore);
        }
        ReportState();
    }

    internal void SetFullscreen(bool fullscreen)
    {
        _placing = true;
        try
        {
            if (IsFullscreen != fullscreen)
                ApplyFullscreen(fullscreen);
        }
        finally
        {
            _placing = false;
        }
        ReportState();
    }

    private void ApplyFullscreen(bool fullscreen)
    {
        int style = GetWindowLong(Handle, GwlStyle);
        if (fullscreen)
        {
            var placement = new WindowPlacement { Length = Marshal.SizeOf<WindowPlacement>() };
            if (!GetWindowPlacement(Handle, ref placement))
                throw new Win32Exception(Marshal.GetLastWin32Error());

            // Restoring can move a maximized window back to a different monitor.
            var monitor = WorkAreaForMonitor(MonitorFromWindow(Handle, MonitorDefaultToNearest)).Monitor;
            if (IsZoomed(Handle) || IsIconic(Handle))
            {
                ShowWindow(Handle, SwRestore);
                style = GetWindowLong(Handle, GwlStyle);
            }
            _beforeFullscreen = (style & WsOverlappedWindow, placement);
            SetFrameStyle(style & ~WsOverlappedWindow);
            ApplyDwmFrame();
            PlaceFullscreen(monitor);
        }
        else if (_beforeFullscreen is { } saved)
        {
            _beforeFullscreen = null;
            SetFrameStyle(style | saved.Style);
            ApplyDwmFrame();
            if (!SetWindowPlacement(Handle, in saved.Placement))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            if (!SetWindowPos(Handle, 0, 0, 0, 0, 0,
                SwpNoMove | SwpNoSize | SwpNoZOrder | SwpNoActivate | SwpNoOwnerZOrder | SwpFrameChanged))
                throw new Win32Exception(Marshal.GetLastWin32Error());
        }
    }

    private void SetFrameStyle(int style)
    {
        if (SetWindowLong(Handle, GwlStyle, style) == 0 && Marshal.GetLastWin32Error() is int error and not 0)
            throw new Win32Exception(error);
    }

    private void PlaceFullscreen(NativeRect? bounds = null)
    {
        var monitor = bounds ?? WorkAreaForMonitor(MonitorFromWindow(Handle, MonitorDefaultToNearest)).Monitor;
        if (!SetWindowPos(Handle, 0, monitor.Left, monitor.Top, monitor.Width, monitor.Height,
            SwpNoZOrder | SwpNoActivate | SwpNoOwnerZOrder | SwpFrameChanged))
            throw new Win32Exception(Marshal.GetLastWin32Error());
    }

    internal void StartMove() => StartInteraction(ScMove | HtCaption);

    internal void StartResize(int direction)
    {
        if ((uint)direction < ResizeDirections.Length && !IsZoomed(Handle))
            StartInteraction(ScSize | ResizeDirections[direction]);
    }

    private void StartInteraction(int command)
    {
        if (!CanReportGeometry || IsFullscreen || _inSizeMove || !GetCursorPos(out var pointer))
            return;
        ReleaseInput();
        SendMessage(Handle, WmSysCommand, command, pointer.ToLParam());
    }

    internal void CancelInteraction()
    {
        ReleaseInput();
        SendMessage(Handle, WmCancelMode, 0, 0);
    }

    internal void SetFrame(int width, int height, byte[] pixels)
    {
        _renderer.SetFrame(width, height, pixels);
        InvalidateRect(Handle, 0, false);
    }

    internal void SetAgentOverlay(NativeHost.AgentOverlayState state)
    {
        _renderer.SetAgentOverlay(state);
        if (!state.Active)
        {
            _bannerHovered = false;
            _swallowBannerLeftUp = false;
            _renderer.SetBannerHovered(false);
        }
        SyncOverlayTimer();
        if (Handle != 0)
            InvalidateRect(Handle, 0, false);
    }

    private void SyncOverlayTimer()
    {
        if (Handle == 0)
            return;
        bool needTimer = IsWindowVisible(Handle) && _renderer.HasActiveAnimation();
        if (needTimer && !_overlayTimerRunning)
        {
            if (SetTimer(Handle, OverlayTimerId, OverlayTimerIntervalMs, 0) != 0)
                _overlayTimerRunning = true;
        }
        else if (!needTimer && _overlayTimerRunning)
        {
            StopOverlayTimer();
        }
    }

    private void StopOverlayTimer()
    {
        if (_overlayTimerRunning && Handle != 0)
        {
            KillTimer(Handle, OverlayTimerId);
            _overlayTimerRunning = false;
        }
    }

    protected override void WndProc(ref Message message)
    {
        switch (message.Msg)
        {
            case WmNcCalcSize:
                CalculateClientArea(message.HWnd, message.LParam);
                return;
            case WmNcHitTest:
                message.Result = HitTest(NativePoint.FromLParam(message.LParam));
                return;
            case WmSetCursor when LowWord(message.LParam) == HtClient:
                SetCursor((_bannerHovered ? Cursors.Hand : Cursors.Arrow).Handle);
                message.Result = 1;
                return;
            case WmNcActivate:
                // Keep normal activation semantics but leave caption painting to the client.
                message.LParam = -1;
                base.WndProc(ref message);
                return;
            case WmEraseBackground:
                message.Result = 1;
                return;
            case WmPaint:
                Paint(message.HWnd);
                return;
            case WmTimer when (nuint)message.WParam == OverlayTimerId:
                if (!_renderer.HasActiveAnimation())
                    StopOverlayTimer();
                InvalidateRect(Handle, 0, false);
                return;
            case WmClose:
                _emit(new { type = "close_requested", window_id = _id });
                return;
            case WmActivate:
                _activated = LowWord(message.WParam) != 0;
                if (!_activated)
                    ReleaseInput();
                base.WndProc(ref message);
                ReportState();
                return;
            case WmCancelMode:
            case WmKillFocus:
                ReleaseInput();
                break;
            case WmSysCommand when ((int)message.WParam & 0xFFF0) is ScMove or ScSize:
                if (_inSizeMove)
                    return;
                _sizeCommand = ((int)message.WParam & 0xFFF0) == ScSize;
                try
                {
                    base.WndProc(ref message);
                }
                finally
                {
                    _sizeCommand = false;
                }
                return;
            case WmEnterSizeMove:
                _inSizeMove = true;
                _resizing = _sizeCommand;
                ReleaseInput(releaseCapture: false); // The native modal loop may already own capture.
                ReportState();
                break;
            case WmSizing:
                _resizing = true;
                ReportState();
                break;
            case WmExitSizeMove:
                _inSizeMove = false;
                _resizing = false;
                ReportState();
                break;
            case WmWindowPosChanged:
            case WmShowWindow:
            case WmDisplayChange:
                base.WndProc(ref message);
                ReportState();
                return;
            case WmSize:
                InvalidateRect(Handle, 0, false);
                break;
            case WmDpiChanged:
                if (!_placing)
                {
                    var suggested = Marshal.PtrToStructure<NativeRect>(message.LParam);
                    SetWindowPos(Handle, 0, suggested.Left, suggested.Top, suggested.Width, suggested.Height,
                        SwpNoZOrder | SwpNoActivate | SwpNoOwnerZOrder);
                    ReportState();
                }
                return;
            case WmDwmCompositionChanged:
                ApplyDwmFrame();
                break;
            case WmDropFiles:
                DropFiles(message.WParam);
                return;
        }
        if (!HandleInput(ref message))
            base.WndProc(ref message);
    }

    private void ApplyDwmFrame()
    {
        var margins = IsFullscreen ? default : new Margins(1, 1, 1, 1);
        DwmExtendFrameIntoClientArea(Handle, in margins);
        const int DoNotRound = 1, RoundCorners = 2;
        DwmSetWindowAttribute(Handle, DwmWindowCornerPreference, IsFullscreen ? DoNotRound : RoundCorners, sizeof(int));
    }

    private static void CalculateClientArea(nint hwnd, nint lParam)
    {
        if (!IsZoomed(hwnd))
            return;

        // Both forms of WM_NCCALCSIZE start with the proposed window rectangle.
        // Keep the native maximized outer bounds for shell behavior and clip the client to the work area.
        var proposed = Marshal.PtrToStructure<NativeRect>(lParam);
        var work = WorkAreaForMonitor(MonitorFromRect(in proposed, MonitorDefaultToNearest)).Work;
        var clipped = new NativeRect(Math.Max(proposed.Left, work.Left), Math.Max(proposed.Top, work.Top),
            Math.Min(proposed.Right, work.Right), Math.Min(proposed.Bottom, work.Bottom));
        if (clipped.Width > 0 && clipped.Height > 0)
            Marshal.StructureToPtr(clipped, lParam, false);
    }

    private int HitTest(NativePoint pointer)
    {
        if (IsFullscreen || IsZoomed(Handle) || !GetWindowRect(Handle, out var rect))
            return HtClient;

        uint dpi = Dpi;
        int padding = GetSystemMetricsForDpi(SmCxPaddedBorder, dpi);
        int horizontal = GetSystemMetricsForDpi(SmCxSizeFrame, dpi) + padding;
        int vertical = GetSystemMetricsForDpi(SmCySizeFrame, dpi) + padding;
        bool left = pointer.X < rect.Left + horizontal;
        bool right = pointer.X >= rect.Right - horizontal;
        bool top = pointer.Y < rect.Top + vertical;
        bool bottom = pointer.Y >= rect.Bottom - vertical;
        if (top)
            return left ? HtTopLeft : right ? HtTopRight : HtTop;
        if (bottom)
            return left ? HtBottomLeft : right ? HtBottomRight : HtBottom;
        return left ? HtLeft : right ? HtRight : HtClient;
    }

    private NativeRect InitialWorkArea()
    {
        nint reference = Owner;
        if (reference == 0)
            reference = GetForegroundWindow();
        return WorkAreaForMonitor(MonitorFromWindow(reference != 0 ? reference : Handle, MonitorDefaultToNearest)).Work;
    }

    private void Place(NativeHost.WindowRect rect, double scale, NativeRect? workArea)
    {
        int x = rect.X, y = rect.Y;
        int width = ToPixels(rect.Width, scale), height = ToPixels(rect.Height, scale);
        if (workArea is { } work)
        {
            width = Math.Min(width, work.Width);
            height = Math.Min(height, work.Height);
            x = work.Left + (work.Width - width) / 2;
            y = work.Top + (work.Height - height) / 2;
        }
        if (!SetWindowPos(Handle, 0, x, y, width, height, SwpNoZOrder | SwpNoActivate | SwpNoOwnerZOrder))
            throw new Win32Exception(Marshal.GetLastWin32Error());
    }

    private static int ToPixels(int logical, double scale) => Math.Max(1, Round(logical * scale));

    private void ReportState()
    {
        if (_placing || Handle == 0)
            return;
        if (!CanReportGeometry)
        {
            ReportInactive();
            return;
        }
        if (!TryGetClientBounds(Handle, out var client, out var origin) || client.Width <= 0 || client.Height <= 0)
            return;

        double scale = Scale;
        var rect = new NativeHost.WindowRect(origin.X, origin.Y, Round(client.Width / scale), Round(client.Height / scale));
        bool maximized = IsZoomed(Handle);
        var area = WorkAreaForMonitor(MonitorFromWindow(Handle, MonitorDefaultToNearest));
        var monitor = new NativeHost.WindowRect(area.Monitor.Left, area.Monitor.Top, area.Monitor.Width, area.Monitor.Height);
        PublishState(new WindowState(rect, monitor, client.Width, client.Height, scale, _activated,
            maximized, IsFullscreen, _resizing, maximized || IsFullscreen ? 0 : TiledEdges(area.Work)));
    }

    private void ReportInactive()
    {
        if (_lastState is { Activated: true } state)
            PublishState(state with { Activated = false, Resizing = false });
    }

    private void PublishState(WindowState state)
    {
        if (_lastState == state)
            return;
        _lastState = state;
        _emit(new
        {
            type = "state", window_id = _id, rect = state.Rect, scale = state.Scale, monitor = state.Monitor,
            pixel_size = new[] { state.PixelWidth, state.PixelHeight },
            activated = state.Activated, maximized = state.Maximized, fullscreen = state.Fullscreen,
            resizing = state.Resizing,
            tiled = new[] { (state.Tiled & 1) != 0, (state.Tiled & 2) != 0, (state.Tiled & 4) != 0, (state.Tiled & 8) != 0 },
        });
    }

    private int TiledEdges(NativeRect work)
    {
        if (DwmGetWindowAttribute(Handle, DwmExtendedFrameBounds, out var frame, Marshal.SizeOf<NativeRect>()) < 0
            && !GetWindowRect(Handle, out frame))
            return 0;

        int tolerance = Math.Max(1, Round(Scale));
        return (Math.Abs(frame.Left - work.Left) <= tolerance ? 1 : 0)
            | (Math.Abs(frame.Right - work.Right) <= tolerance ? 2 : 0)
            | (Math.Abs(frame.Top - work.Top) <= tolerance ? 4 : 0)
            | (Math.Abs(frame.Bottom - work.Bottom) <= tolerance ? 8 : 0);
    }

    private void Paint(nint hwnd)
    {
        BeginPaint(hwnd, out var paint);
        try
        {
            if (GetClientRect(hwnd, out var client) && client.Width > 0 && client.Height > 0)
                _renderer.Draw(hwnd, client.Width, client.Height, Scale);
        }
        finally
        {
            EndPaint(hwnd, in paint);
        }
        SyncOverlayTimer();
    }

    private void DropFiles(nint drop)
    {
        try
        {
            DragQueryPoint(drop, out var point);
            uint count = DragQueryFile(drop, uint.MaxValue, null, 0);
            var paths = new string[count];
            for (uint index = 0; index < count; index++)
            {
                uint length = DragQueryFile(drop, index, null, 0);
                var path = new StringBuilder((int)length + 1);
                DragQueryFile(drop, index, path, (uint)path.Capacity);
                paths[index] = path.ToString();
            }
            var (x, y) = LogicalPoint(point);
            _emit(new { type = "files_dropped", window_id = _id, paths, x, y });
        }
        finally
        {
            DragFinish(drop);
        }
    }

    public void Dispose()
    {
        if (Handle == 0)
            return;
        CancelInteraction();
        StopOverlayTimer();
        ReportInactive();
        DragAcceptFiles(Handle, false);
        if (_stageId is not null)
            SetStageAppId(Handle, null);
        DestroyHandle();
        _appIcon?.Dispose();
        _renderer.Dispose();
    }
}
