using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;

namespace FocusStageTray;

internal static class NativeHostInterop
{
    internal const int WsOverlappedWindow = 0x00CF0000;
    internal const int SwHide = 0, SwShow = 5, SwMinimize = 6, SwRestore = 9, SwMaximize = 3;
    internal const uint SwpNoZOrder = 0x0004, SwpNoActivate = 0x0010, SwpNoOwnerZOrder = 0x0200;
    internal const uint MonitorDefaultToPrimary = 1, MonitorDefaultToNearest = 2;
    internal const uint GwOwner = 4;

    internal const int WmCancelMode = 0x001F, WmSysCommand = 0x0112;
    internal const int ScSize = 0xF000, ScMove = 0xF010, ScClose = 0xF060, ScKeyMenu = 0xF100;
    internal const int HtClient = 1, HtCaption = 2, HtLeft = 10, HtRight = 11, HtTop = 12,
        HtTopLeft = 13, HtTopRight = 14, HtBottom = 15, HtBottomLeft = 16, HtBottomRight = 17;

    [StructLayout(LayoutKind.Sequential)]
    internal record struct NativePoint(int X, int Y)
    {
        internal static NativePoint FromLParam(nint value) => new(unchecked((short)value), SignedHighWord(value));
        internal nint ToLParam() => unchecked((int)((ushort)X | ((uint)(ushort)Y << 16)));
    }

    [StructLayout(LayoutKind.Sequential)]
    internal record struct NativeRect(int Left, int Top, int Right, int Bottom)
    {
        internal readonly int Width => Right - Left;
        internal readonly int Height => Bottom - Top;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct MonitorArea
    {
        internal int Size;
        internal NativeRect Monitor;
        internal NativeRect Work;
        internal uint Flags;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct NativeMessage
    {
        private nint _hwnd;
        private uint _message;
        private nuint _wParam;
        private nint _lParam;
        private uint _time;
        private NativePoint _point;
        private uint _reserved;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct PaintData
    {
        private nint _dc;
        private int _erase;
        private NativeRect _paint;
        private int _restore, _update;
        private int _reserved0, _reserved1, _reserved2, _reserved3, _reserved4, _reserved5, _reserved6, _reserved7;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct MouseTracking
    {
        internal int Size;
        internal uint Flags;
        internal nint Hwnd;
        private uint _hoverTime;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal record struct Margins(int Left, int Right, int Top, int Bottom);

    internal static int LowWord(nint value) => (ushort)value;
    internal static int HighWord(nint value) => (ushort)(value >> 16);
    internal static int SignedHighWord(nint value) => unchecked((short)(value >> 16));
    internal static int Round(double value) => (int)Math.Round(value, MidpointRounding.AwayFromZero);

    internal static MonitorArea WorkAreaForMonitor(nint monitor)
    {
        var info = new MonitorArea { Size = Marshal.SizeOf<MonitorArea>() };
        if (!GetMonitorInfo(monitor, ref info))
            throw new Win32Exception(Marshal.GetLastWin32Error());
        return info;
    }

    internal static bool TryGetClientBounds(nint hwnd, out NativeRect rect, out NativePoint origin)
    {
        origin = default;
        return GetClientRect(hwnd, out rect) && ClientToScreen(hwnd, ref origin);
    }

    internal static nint SetWindowOwner(nint hwnd, nint owner)
    {
        const int GwlpHwndParent = -8;
        return nint.Size == 8
            ? SetWindowLongPtr(hwnd, GwlpHwndParent, owner)
            : SetWindowLong(hwnd, GwlpHwndParent, (int)owner);
    }

    internal static void SetStageAppId(nint hwnd, string? stageId)
    {
        var iid = typeof(IPropertyStore).GUID;
        int hr = SHGetPropertyStoreForWindow(hwnd, in iid, out var store);
        if (hr < 0)
        {
            Console.Error.WriteLine($"FocusStage window host: property store unavailable (0x{hr:X8}).");
            return;
        }
        var key = new PropertyKey(new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), 5);
        var value = new PropertyVariant();
        try
        {
            if (stageId is not null)
            {
                // AppUserModelIDs are limited to 128 characters without whitespace.
                string id = $"FocusStage.Stage.{Environment.ProcessId}." + Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(stageId)));
                value.Type = 31; // VT_LPWSTR
                value.Pointer = Marshal.StringToCoTaskMemUni(id);
            }
            hr = store.SetValue(in key, in value);
            if (hr < 0)
                Console.Error.WriteLine($"FocusStage window host: AppUserModelID update failed (0x{hr:X8}).");
        }
        finally
        {
            Marshal.FreeCoTaskMem(value.Pointer);
            Marshal.ReleaseComObject(store);
        }
    }

    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    private readonly record struct PropertyKey(Guid Format, uint Id);

    [StructLayout(LayoutKind.Sequential)]
    private struct PropertyVariant
    {
        internal ushort Type;
        private ushort _reserved1, _reserved2, _reserved3;
        internal nint Pointer;
        private nint _unionPadding;
    }

    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IPropertyStore
    {
        [PreserveSig] int GetCount(out uint count);
        [PreserveSig] int GetAt(uint index, out PropertyKey key);
        [PreserveSig] int GetValue(in PropertyKey key, out PropertyVariant value);
        [PreserveSig] int SetValue(in PropertyKey key, in PropertyVariant value);
    }

    private const uint CfUnicodeText = 13;

    internal static bool TryReadClipboard(nint hwnd, out uint sequence, out string? text)
    {
        sequence = 0;
        text = null;
        if (!OpenClipboard(hwnd))
            return false;
        try
        {
            if (!IsClipboardFormatAvailable(CfUnicodeText))
            {
                if (CountClipboardFormats() == 0)
                    text = "";
            }
            else
            {
                nint data = GetClipboardData(CfUnicodeText);
                nint pointer = GlobalLock(data);
                if (pointer == 0)
                    return false;
                try
                {
                    text = Marshal.PtrToStringUni(pointer);
                }
                finally
                {
                    GlobalUnlock(data);
                }
            }
            sequence = GetClipboardSequenceNumber();
            return true;
        }
        finally
        {
            CloseClipboard();
        }
    }

    internal static bool TryWriteClipboard(nint hwnd, string text)
    {
        const uint GmemMoveable = 0x0002;
        nint data = GlobalAlloc(GmemMoveable, checked((nuint)(text.Length + 1) * sizeof(char)));
        if (data == 0)
            return false;
        try
        {
            nint pointer = GlobalLock(data);
            if (pointer == 0)
                return false;
            try
            {
                Marshal.Copy(text.ToCharArray(), 0, pointer, text.Length);
                Marshal.WriteInt16(pointer, text.Length * sizeof(char), 0);
            }
            finally
            {
                GlobalUnlock(data);
            }
            if (!OpenClipboard(hwnd))
                return false;
            try
            {
                if (!EmptyClipboard() || SetClipboardData(CfUnicodeText, data) == 0)
                    return false;
                data = 0; // The system owns the allocation after SetClipboardData succeeds.
                return true;
            }
            finally
            {
                CloseClipboard();
            }
        }
        finally
        {
            if (data != 0)
                GlobalFree(data);
        }
    }

    [DllImport("user32.dll")] internal static extern uint GetDpiForSystem();
    [DllImport("user32.dll")] internal static extern uint GetDpiForWindow(nint hwnd);
    [DllImport("user32.dll")] internal static extern int GetSystemMetricsForDpi(int index, uint dpi);
    [DllImport("user32.dll")] internal static extern nint MonitorFromPoint(NativePoint point, uint flags);
    [DllImport("user32.dll")] internal static extern nint MonitorFromRect(in NativeRect rect, uint flags);
    [DllImport("user32.dll")] internal static extern nint MonitorFromWindow(nint hwnd, uint flags);
    [DllImport("user32.dll", EntryPoint = "GetMonitorInfoW", SetLastError = true)]
    private static extern bool GetMonitorInfo(nint monitor, ref MonitorArea info);
    [DllImport("user32.dll")] internal static extern bool GetWindowRect(nint hwnd, out NativeRect rect);
    [DllImport("user32.dll")] internal static extern bool GetClientRect(nint hwnd, out NativeRect rect);
    [DllImport("user32.dll")] internal static extern bool ClientToScreen(nint hwnd, ref NativePoint point);
    [DllImport("user32.dll")] internal static extern bool ScreenToClient(nint hwnd, ref NativePoint point);
    [DllImport("user32.dll")] internal static extern bool IsZoomed(nint hwnd);
    [DllImport("user32.dll")] internal static extern bool IsIconic(nint hwnd);
    [DllImport("user32.dll")] internal static extern bool IsWindowVisible(nint hwnd);
    [DllImport("user32.dll")] internal static extern bool ShowWindow(nint hwnd, int command);
    [DllImport("user32.dll")] internal static extern nint GetForegroundWindow();
    [DllImport("user32.dll")] internal static extern bool SetForegroundWindow(nint hwnd);
    [DllImport("user32.dll", EntryPoint = "SetWindowTextW", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern bool SetWindowText(nint hwnd, string title);
    [DllImport("user32.dll")] internal static extern nint GetWindow(nint hwnd, uint command);
    [DllImport("user32.dll", EntryPoint = "SetWindowLongPtrW", SetLastError = true)]
    private static extern nint SetWindowLongPtr(nint hwnd, int index, nint value);
    [DllImport("user32.dll", EntryPoint = "SetWindowLongW", SetLastError = true)]
    private static extern int SetWindowLong(nint hwnd, int index, int value);
    [DllImport("user32.dll", SetLastError = true)]
    internal static extern bool SetWindowPos(nint hwnd, nint insertAfter, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll")] internal static extern bool GetCursorPos(out NativePoint point);
    [DllImport("user32.dll")] internal static extern nint SetCursor(nint cursor);
    [DllImport("user32.dll")] internal static extern nint GetCapture();
    [DllImport("user32.dll")] internal static extern nint SetCapture(nint hwnd);
    [DllImport("user32.dll")] internal static extern bool ReleaseCapture();
    [DllImport("user32.dll")] internal static extern short GetKeyState(int key);
    [DllImport("user32.dll", EntryPoint = "SendMessageW")]
    internal static extern nint SendMessage(nint hwnd, int message, nint wParam, nint lParam);
    [DllImport("user32.dll", EntryPoint = "PostMessageW")]
    internal static extern bool PostMessage(nint hwnd, int message, nint wParam, nint lParam);
    [DllImport("user32.dll", EntryPoint = "GetMessageW", SetLastError = true)]
    internal static extern int GetMessage(out NativeMessage message, nint hwnd, uint first, uint last);
    [DllImport("user32.dll", EntryPoint = "DispatchMessageW")]
    internal static extern nint Dispatch(in NativeMessage message);
    [DllImport("user32.dll")] internal static extern void PostQuitMessage(int exitCode);
    [DllImport("user32.dll")] internal static extern bool TrackMouseEvent(ref MouseTracking request);
    [DllImport("user32.dll")] internal static extern bool InvalidateRect(nint hwnd, nint rect, bool erase);
    [DllImport("user32.dll")] internal static extern nint BeginPaint(nint hwnd, out PaintData data);
    [DllImport("user32.dll")] internal static extern bool EndPaint(nint hwnd, in PaintData data);
    [DllImport("user32.dll")] internal static extern bool AddClipboardFormatListener(nint hwnd);
    [DllImport("user32.dll")] internal static extern bool RemoveClipboardFormatListener(nint hwnd);
    [DllImport("user32.dll")] internal static extern nint GetClipboardOwner();
    [DllImport("user32.dll")] private static extern uint GetClipboardSequenceNumber();
    [DllImport("user32.dll")] private static extern bool OpenClipboard(nint hwnd);
    [DllImport("user32.dll")] private static extern bool CloseClipboard();
    [DllImport("user32.dll")] private static extern bool EmptyClipboard();
    [DllImport("user32.dll")] private static extern int CountClipboardFormats();
    [DllImport("user32.dll")] private static extern bool IsClipboardFormatAvailable(uint format);
    [DllImport("user32.dll")] private static extern nint GetClipboardData(uint format);
    [DllImport("user32.dll")] private static extern nint SetClipboardData(uint format, nint data);
    [DllImport("kernel32.dll")] private static extern nint GlobalAlloc(uint flags, nuint bytes);
    [DllImport("kernel32.dll")] private static extern nint GlobalFree(nint data);
    [DllImport("kernel32.dll")] private static extern nint GlobalLock(nint data);
    [DllImport("kernel32.dll")] private static extern bool GlobalUnlock(nint data);
    [DllImport("dwmapi.dll")] internal static extern int DwmExtendFrameIntoClientArea(nint hwnd, in Margins margins);
    [DllImport("dwmapi.dll")] internal static extern int DwmSetWindowAttribute(nint hwnd, int attribute, in int value, int size);
    [DllImport("dwmapi.dll")] internal static extern int DwmGetWindowAttribute(nint hwnd, int attribute, out NativeRect rect, int size);
    [DllImport("imm32.dll")] internal static extern nint ImmAssociateContext(nint hwnd, nint context);
    [DllImport("shell32.dll")] internal static extern void DragAcceptFiles(nint hwnd, bool accept);
    [DllImport("shell32.dll")] internal static extern void DragFinish(nint drop);
    [DllImport("shell32.dll")] internal static extern bool DragQueryPoint(nint drop, out NativePoint point);
    [DllImport("shell32.dll", EntryPoint = "DragQueryFileW", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern uint DragQueryFile(nint drop, uint index, StringBuilder? path, uint size);
    [DllImport("shell32.dll")]
    private static extern int SHGetPropertyStoreForWindow(nint hwnd, in Guid iid, out IPropertyStore store);
}
