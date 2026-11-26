using System.Buffers.Binary;
using System.Runtime.InteropServices;
using static FocusStageTray.NativeHostInterop;

namespace FocusStageTray;

internal sealed partial class NativeHostWindow
{
    private Cursor? _systemCursor = Cursors.Arrow;
    private nint _bitmapCursor;
    private nint AppCursorHandle => _bitmapCursor != 0 ? _bitmapCursor : _systemCursor?.Handle ?? 0;

    internal void SetAppCursor(string shape, string? png, int hotspotX, int hotspotY)
    {
        nint bitmap = shape == "bitmap" ? CreateBitmapCursor(png, hotspotX, hotspotY) : 0;
        var old = _bitmapCursor;
        _bitmapCursor = bitmap;
        _systemCursor = shape switch
        {
            "hidden" or "none" => null,
            "pointer" or "grab" or "grabbing" => Cursors.Hand,
            "text" or "vertical-text" => Cursors.IBeam,
            "crosshair" or "cell" => Cursors.Cross,
            "wait" => Cursors.WaitCursor,
            "progress" => Cursors.AppStarting,
            "help" => Cursors.Help,
            "not-allowed" or "no-drop" => Cursors.No,
            "move" or "all-scroll" or "all-resize" => Cursors.SizeAll,
            "col-resize" or "e-resize" or "w-resize" or "ew-resize" => Cursors.SizeWE,
            "row-resize" or "n-resize" or "s-resize" or "ns-resize" => Cursors.SizeNS,
            "nwse-resize" or "nw-resize" or "se-resize" => Cursors.SizeNWSE,
            "nesw-resize" or "ne-resize" or "sw-resize" => Cursors.SizeNESW,
            _ => Cursors.Arrow
        };
        RefreshAppCursor();
        DestroyOwnedCursor(old);
    }

    private void RefreshAppCursor()
    {
        if (Handle != 0 && GetCursorPos(out var point) && (GetCapture() == Handle || (WindowFromPoint(point) == Handle && HitTest(point) == HtClient)))
            SetCursor(_bannerHovered ? Cursors.Hand.Handle : AppCursorHandle);
    }

    private void ReleaseBitmapCursor()
    {
        DestroyOwnedCursor(_bitmapCursor);
        _bitmapCursor = 0;
    }

    private static void DestroyOwnedCursor(nint cursor)
    {
        if (cursor == 0) return;
        if (GetCursor() == cursor) SetCursor(Cursors.Arrow.Handle);
        DestroyCursor(cursor);
    }

    internal static nint CreateBitmapCursor(string? encoded, int hotspotX, int hotspotY)
    {
        if (encoded is null || encoded.Length is < 32 or > 8_000_000) return 0;
        byte[] resource = new byte[4 + (encoded.Length / 4 + 1) * 3];
        if (!Convert.TryFromBase64String(encoded, resource.AsSpan(4), out int length) || length < 33) return 0;
        var png = resource.AsSpan(4, length);
        if (BinaryPrimitives.ReadUInt64BigEndian(png) != 0x89504e470d0a1a0aUL ||
            !png.Slice(12, 4).SequenceEqual("IHDR"u8)) return 0;
        int width = BinaryPrimitives.ReadInt32BigEndian(png[16..]);
        int height = BinaryPrimitives.ReadInt32BigEndian(png[20..]);
        if (width is < 1 or > 1024 || height is < 1 or > 1024) return 0;
        BinaryPrimitives.WriteUInt16LittleEndian(resource, (ushort)Math.Clamp(hotspotX, 0, width - 1));
        BinaryPrimitives.WriteUInt16LittleEndian(resource.AsSpan(2), (ushort)Math.Clamp(hotspotY, 0, height - 1));
        return CreateIconFromResourceEx(resource, (uint)(length + 4), false, 0x30000, width, height, 0);
    }

    [DllImport("user32.dll")] private static extern nint CreateIconFromResourceEx(byte[] data, uint size,
        bool icon, uint version, int width, int height, uint flags);
    [DllImport("user32.dll")] private static extern bool DestroyCursor(nint cursor);
    [DllImport("user32.dll")] private static extern nint GetCursor();
}
