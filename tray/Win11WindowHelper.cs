using System.Runtime.InteropServices;

namespace FocusStageTray;

internal static class Win11WindowHelper
{
    private const int DWMWA_WINDOW_CORNER_PREFERENCE = 33;
    private const int DWMWCP_ROUND = 2;

    public const int WM_NCCALCSIZE = 0x0083;
    public const int WM_NCLBUTTONDOWN = 0x00A1;
    public const int HTCAPTION = 2;

    public const int WS_CAPTION = 0x00C00000;
    public const int WS_THICKFRAME = 0x00040000;
    public const int WS_MINIMIZEBOX = 0x00020000;
    public const int WS_MAXIMIZEBOX = 0x00010000;
    public const int WS_SYSMENU = 0x00080000;

    private const int SM_CXSIZEFRAME = 32;
    private const int SM_CYSIZEFRAME = 33;
    private const int SM_CXPADDEDBORDER = 92;

    [StructLayout(LayoutKind.Sequential)]
    private struct RECT
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NCCALCSIZE_PARAMS
    {
        public RECT Rgrc0;
        public RECT Rgrc1;
        public RECT Rgrc2;
        public IntPtr Lppos;
    }

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
    private static extern int DwmExtendFrameIntoClientArea(IntPtr hwnd, ref MARGINS margins);

    [DllImport("user32.dll")]
    public static extern bool ReleaseCapture();

    [DllImport("user32.dll")]
    public static extern IntPtr SendMessage(IntPtr hWnd, int msg, int wParam, int lParam);

    [DllImport("user32.dll")]
    private static extern bool IsZoomed(IntPtr hWnd);

    [DllImport("user32.dll")]
    private static extern int GetSystemMetricsForDpi(int nIndex, uint dpi);

    [DllImport("user32.dll")]
    private static extern uint GetDpiForWindow(IntPtr hwnd);

    public static void ApplyWin11Chrome(IntPtr handle)
    {
        try
        {
            int pref = DWMWCP_ROUND;
            DwmSetWindowAttribute(handle, DWMWA_WINDOW_CORNER_PREFERENCE, ref pref, sizeof(int));

            // Keep subtle DWM drop shadow on frameless WS_THICKFRAME window.
            var margins = new MARGINS { Left = 0, Right = 0, Top = 1, Bottom = 0 };
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

    public static bool HandleNcCalcSize(IntPtr handle, ref Message m)
    {
        if (m.WParam == IntPtr.Zero)
        {
            m.Result = IntPtr.Zero;
            return true;
        }

        if (IsZoomed(handle))
        {
            uint dpi = GetDpiForWindow(handle);
            if (dpi == 0) dpi = 96;
            int pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            int frameX = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + pad;
            int frameY = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + pad;

            var calc = Marshal.PtrToStructure<NCCALCSIZE_PARAMS>(m.LParam);
            calc.Rgrc0.Left += frameX;
            calc.Rgrc0.Top += frameY;
            calc.Rgrc0.Right -= frameX;
            calc.Rgrc0.Bottom -= frameY;
            Marshal.StructureToPtr(calc, m.LParam, false);
        }

        m.Result = IntPtr.Zero;
        return true;
    }
}
