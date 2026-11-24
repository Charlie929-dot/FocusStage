using System.Buffers;
using System.Runtime.InteropServices;

namespace FocusStageTray;

// Minimal Direct2D 1.0 bindings for a UI-thread-owned HWND render target and one BGRA bitmap.
internal sealed class NativeHostRenderer(NativeHostRenderer.Factory factory) : IDisposable
{
    private const int RecreateTarget = unchecked((int)0x8899000C);
    private static readonly PixelFormat BgraOpaque = new(87, 3); // B8G8R8A8_UNORM, ALPHA_MODE_IGNORE
    private static readonly Color Black = new(0, 0, 0, 1);

    private sealed record Frame(PixelSize Size, byte[] Pixels);
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

    internal void SetFrame(int width, int height, byte[] pixels)
    {
        ReturnFrame();
        _frame = new(new PixelSize((uint)width, (uint)height), pixels);
        _upload = true;
    }

    internal void Draw(nint hwnd, int width, int height)
    {
        var size = new PixelSize((uint)width, (uint)height);
        int hr = DrawFrame(hwnd, size);
        if (hr == RecreateTarget)
        {
            ReleaseTarget();
            hr = DrawFrame(hwnd, size);
        }
        if (hr < 0)
        {
            ReleaseTarget();
            Console.Error.WriteLine($"FocusStage window host Direct2D: draw failed (0x{hr:X8}).");
        }
    }

    private int DrawFrame(nint hwnd, PixelSize size)
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
        return _endDraw!(_target, 0, 0);
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
        Release(ref _target);
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
    [StructLayout(LayoutKind.Sequential)] private readonly record struct PixelFormat(int Format, int AlphaMode);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct FloatRect(float Left, float Top, float Right, float Bottom);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct Color(float R, float G, float B, float A);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct BitmapProperties(PixelFormat Format, float DpiX, float DpiY);
    [StructLayout(LayoutKind.Sequential)] private readonly record struct RenderTargetProperties(int Type, PixelFormat Format,
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
    private delegate void Clear(nint self, in Color color);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate void BeginDraw(nint self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int EndDraw(nint self, nint tag1, nint tag2);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int Resize(nint self, in PixelSize size);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int CopyFromMemory(nint self, nint destination, nint source, uint pitch);
}
