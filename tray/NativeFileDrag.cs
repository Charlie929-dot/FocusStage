using System.Runtime.InteropServices;
using OleDataObject = System.Runtime.InteropServices.ComTypes.IDataObject;

namespace FocusStageTray;

internal static class NativeFileDrag
{
    private const int Drop = 0x00040100, Cancel = 0x00040101, UseDefaultCursors = 0x00040102;
    private const uint LeftButton = 1, Copy = 1;

    // The host normally uses a NativeWindow rather than WinForms Control.DoDragDrop.
    // Keep the OLE source explicit so it can yield back to the Wayland grab.
    internal static bool Run(DataObject data, Source source)
    {
        Application.OleRequired();
        int result = DoDragDrop(data, source, Copy, out uint effect);
        if (result < 0)
            Marshal.ThrowExceptionForHR(result);
        return result == Drop && (effect & Copy) != 0;
    }

    [ComVisible(true)]
    [ClassInterface(ClassInterfaceType.None)]
    internal sealed class Source(Func<bool> shouldCancel, Func<bool> overStage) : IDropSource
    {
        internal bool ReturnedToStage { get; private set; }

        int IDropSource.QueryContinueDrag(bool escapePressed, uint keyState)
        {
            if (escapePressed || shouldCancel())
                return Cancel;
            if (overStage())
            {
                ReturnedToStage = (keyState & LeftButton) != 0;
                return Cancel;
            }
            return (keyState & LeftButton) != 0 ? 0 : Drop;
        }

        int IDropSource.GiveFeedback(uint effect) => UseDefaultCursors;
    }

    [ComImport]
    [Guid("00000121-0000-0000-C000-000000000046")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IDropSource
    {
        [PreserveSig] int QueryContinueDrag([MarshalAs(UnmanagedType.Bool)] bool escapePressed, uint keyState);
        [PreserveSig] int GiveFeedback(uint effect);
    }

    [DllImport("ole32.dll", ExactSpelling = true)]
    private static extern int DoDragDrop(OleDataObject data, IDropSource source, uint allowedEffects, out uint effect);
}
