using System.Runtime.InteropServices;
using static FocusStageTray.NativeHostInterop;

namespace FocusStageTray;

internal sealed partial class NativeHostWindow
{
    private const int WmKeyDown = 0x0100, WmKeyUp = 0x0101, WmChar = 0x0102, WmDeadChar = 0x0103,
        WmSysKeyDown = 0x0104, WmSysKeyUp = 0x0105, WmSysChar = 0x0106, WmSysDeadChar = 0x0107;
    private const int WmMouseMove = 0x0200, WmLeftDown = 0x0201, WmLeftUp = 0x0202, WmLeftDoubleClick = 0x0203,
        WmRightDown = 0x0204, WmRightUp = 0x0205, WmRightDoubleClick = 0x0206,
        WmMiddleDown = 0x0207, WmMiddleUp = 0x0208, WmMiddleDoubleClick = 0x0209, WmMouseWheel = 0x020A,
        WmXDown = 0x020B, WmXUp = 0x020C, WmXDoubleClick = 0x020D, WmMouseHWheel = 0x020E,
        WmEnterMenuLoop = 0x0211, WmCaptureChanged = 0x0215, WmMouseLeave = 0x02A3;
    private const uint TmeLeave = 0x00000002, TmeCancel = 0x80000000;
    private const int BtnLeft = 0x110, BtnRight = 0x111, BtnMiddle = 0x112, BtnSide = 0x113, BtnExtra = 0x114;
    private const int VkTab = 0x09, VkPause = 0x13, VkEscape = 0x1B, VkSpace = 0x20, VkSnapshot = 0x2C, VkF4 = 0x73;
    private const int KeyLeftShift = 42, KeyRightShift = 54, KeyLeftAlt = 56, KeyRightAlt = 100;

    private readonly HashSet<int> _keysDown = new();
    private readonly HashSet<int> _buttonsDown = new();
    private int _pendingAltKey;
    private NativePoint _lastPointer;
    private bool _trackingPointer;

    private bool HandleInput(ref Message message)
    {
        switch (message.Msg)
        {
            case WmMouseMove:
                _lastPointer = NativePoint.FromLParam(message.LParam);
                TrackPointerLeave();
                var (x, y) = LogicalPoint(_lastPointer);
                _emit(new { type = "pointer_motion", window_id = _id, x, y });
                return true;
            case WmMouseLeave:
                if (_trackingPointer)
                {
                    _trackingPointer = false;
                    if (_buttonsDown.Count == 0)
                        EmitPointerLeave();
                }
                return true;
            case WmLeftDown:
            case WmLeftDoubleClick:
            case WmLeftUp:
                ButtonMessage(message.LParam, BtnLeft, message.Msg != WmLeftUp);
                return true;
            case WmRightDown:
            case WmRightDoubleClick:
            case WmRightUp:
                ButtonMessage(message.LParam, BtnRight, message.Msg != WmRightUp);
                return true;
            case WmMiddleDown:
            case WmMiddleDoubleClick:
            case WmMiddleUp:
                ButtonMessage(message.LParam, BtnMiddle, message.Msg != WmMiddleUp);
                return true;
            case WmXDown:
            case WmXDoubleClick:
            case WmXUp:
                ButtonMessage(message.LParam, HighWord(message.WParam) == 1 ? BtnSide : BtnExtra, message.Msg != WmXUp);
                message.Result = 1;
                return true;
            case WmMouseWheel:
            case WmMouseHWheel:
                ScrollMessage(message);
                return true;
            case WmCaptureChanged:
                ReleaseButtons(releaseCapture: false);
                break;
            case WmEnterMenuLoop:
                ReleaseInput();
                break;
            case WmKeyDown:
            case WmKeyUp:
            case WmSysKeyDown:
            case WmSysKeyUp:
                return KeyMessage(message);
            case WmChar:
            case WmDeadChar:
            case WmSysChar:
            case WmSysDeadChar:
                // The daemon receives physical keys and supplies the app's layout and repeat behavior.
                return true;
        }
        return false;
    }

    private bool IsPointerInClient() => _lastPointer.X >= 0 && _lastPointer.Y >= 0
        && GetClientRect(Handle, out var client) && _lastPointer.X < client.Right && _lastPointer.Y < client.Bottom;

    private void TrackPointerLeave()
    {
        if (_trackingPointer || !IsPointerInClient())
            return;
        var tracking = new MouseTracking { Size = Marshal.SizeOf<MouseTracking>(), Flags = TmeLeave, Hwnd = Handle };
        _trackingPointer = TrackMouseEvent(ref tracking);
    }

    private void EmitPointerLeave() => _emit(new { type = "pointer_leave", window_id = _id });

    private void FinishPointerDrag()
    {
        if (_trackingPointer && IsPointerInClient())
            return;
        if (_trackingPointer)
        {
            var tracking = new MouseTracking { Size = Marshal.SizeOf<MouseTracking>(), Flags = TmeLeave | TmeCancel, Hwnd = Handle };
            TrackMouseEvent(ref tracking);
            _trackingPointer = false;
        }
        // Linux retains pointer focus during the drag. Send leave after the final release.
        EmitPointerLeave();
    }

    private (double X, double Y) LogicalPoint(NativePoint point)
    {
        double scale = Scale;
        return (point.X / scale, point.Y / scale);
    }

    private void ButtonMessage(nint lParam, int button, bool pressed)
    {
        _lastPointer = NativePoint.FromLParam(lParam);
        if (pressed ? !_buttonsDown.Add(button) : !_buttonsDown.Remove(button))
            return;
        if (pressed)
            ForwardPendingAlt();
        EmitButton(button, pressed);
        if (pressed)
        {
            TrackPointerLeave();
            if (GetCapture() != Handle)
                SetCapture(Handle);
        }
        else if (_buttonsDown.Count == 0)
        {
            FinishPointerDrag();
            if (GetCapture() == Handle)
                ReleaseCapture();
        }
    }

    private void EmitButton(int button, bool pressed)
    {
        var (x, y) = LogicalPoint(_lastPointer);
        _emit(new { type = "pointer_button", window_id = _id, x, y, button, pressed });
    }

    private void ScrollMessage(Message message)
    {
        var point = NativePoint.FromLParam(message.LParam);
        if (!ScreenToClient(Handle, ref point))
            return;
        _lastPointer = point;
        var (x, y) = LogicalPoint(point);
        int delta = SignedHighWord(message.WParam);
        bool horizontal = message.Msg == WmMouseHWheel;
        int h = horizontal ? delta : 0;
        int v = horizontal ? 0 : -delta;
        ForwardPendingAlt();
        // Match the existing sink's logical distance of 15 per wheel detent, retaining partial detents.
        _emit(new { type = "pointer_scroll", window_id = _id, x, y, dx = h / 8.0, dy = v / 8.0, v120 = new[] { h, v } });
    }

    private bool KeyMessage(Message message)
    {
        int vk = (int)message.WParam;
        int info = (int)message.LParam;
        bool pressed = message.Msg is WmKeyDown or WmSysKeyDown;
        bool repeated = (info & (1 << 30)) != 0;
        bool alt = (info & (1 << 29)) != 0;
        if (alt && message.Msg == WmSysKeyDown)
        {
            if (vk is VkTab or VkEscape)
            {
                _pendingAltKey = 0;
                return false; // Let Windows handle its task and window switching mnemonics.
            }
            // AltGr arrives as Ctrl+Alt on Windows. Preserve it for Linux keyboard layouts.
            if (vk is VkF4 or VkSpace && !(GetKeyState(0xA5) < 0 && GetKeyState(0x11) < 0))
            {
                _pendingAltKey = 0;
                if (!repeated)
                {
                    if (vk == VkSpace)
                        ReleaseInput();
                    SendMessage(Handle, WmSysCommand, vk == VkF4 ? ScClose : ScKeyMenu, vk == VkF4 ? 0 : ' ');
                }
                return true;
            }
        }

        int key = EvdevKey(vk, (info >> 16) & 0xFF, (info & (1 << 24)) != 0);
        if (key == 0)
            return true;
        if (_pendingAltKey == key)
        {
            if (!pressed)
            {
                _pendingAltKey = 0;
                TapKey(key);
            }
            return true;
        }
        // Windows may consume the rest of Alt+Tab before it reaches this HWND. Keep an
        // uncommitted Alt local, including while Shift is held for reverse switching.
        if (pressed && !repeated && key is KeyLeftAlt or KeyRightAlt && _pendingAltKey == 0
            && _buttonsDown.Count == 0 && _keysDown.All(k => k is KeyLeftShift or KeyRightShift))
        {
            _pendingAltKey = key;
            return true;
        }
        if (pressed ? !repeated && key is not (KeyLeftShift or KeyRightShift) : _keysDown.Contains(key))
            ForwardPendingAlt();

        if (vk == VkPause)
        {
            // Pause has no independent break scan code.
            if (pressed && !repeated)
                TapKey(key);
        }
        else if (pressed)
        {
            if (!repeated && _keysDown.Add(key))
                EmitKey(key, true);
        }
        else if (_keysDown.Remove(key))
        {
            EmitKey(key, false);
        }
        else if (vk == VkSnapshot)
        {
            // Windows can deliver a Print Screen up message without its down message.
            TapKey(key);
        }
        return true;
    }

    private static int EvdevKey(int vk, int scan, bool extended)
    {
        if (vk == VkPause)
            return 119;
        if (vk == VkSnapshot)
            return 99;
        if (extended)
        {
            return scan switch
            {
                0x10 => 165, // previous track
                0x19 => 163, // next track
                0x1C => 96,  // keypad Enter
                0x1D => 97,  // right Ctrl
                0x20 => 113, // mute
                0x21 => 140, // calculator
                0x22 => 164, // play/pause
                0x24 => 166, // stop playback
                0x2E => 114, // volume down
                0x30 => 115, // volume up
                0x32 => 172, // browser home
                0x35 => 98,  // keypad divide
                0x37 => 99,  // Print Screen
                0x38 => 100, // right Alt
                0x46 => 119, // Ctrl+Pause (Break)
                0x47 => 102, // Home
                0x48 => 103, // Up
                0x49 => 104, // Page Up
                0x4B => 105, // Left
                0x4D => 106, // Right
                0x4F => 107, // End
                0x50 => 108, // Down
                0x51 => 109, // Page Down
                0x52 => 110, // Insert
                0x53 => 111, // Delete
                0x5B => 125, // left Meta
                0x5C => 126, // right Meta
                0x5D => 127, // Compose (Windows context menu key)
                0x5E => 116, // power
                0x5F => 142, // sleep
                0x63 => 143, // wake
                0x65 => 217, // browser search
                0x66 => 156, // browser favorites
                0x67 => 173, // browser refresh
                0x68 => 128, // browser stop
                0x69 => 159, // browser forward
                0x6A => 158, // browser back
                0x6B => 157, // file browser
                0x6C => 155, // mail
                0x6D => 226, // media select
                _ => 0,
            };
        }
        return scan switch
        {
            >= 0x01 and <= 0x53 => scan, // evdev preserves the base PC set-1 scan codes
            0x54 => 99,  // SysRq
            0x56 => 86,  // ISO extra key
            0x57 => 87,  // F11
            0x58 => 88,  // F12
            0x59 => 117, // keypad equals
            >= 0x64 and <= 0x6E => scan - 0x64 + 183, // F13–F23
            0x70 => 93,  // Katakana/Hiragana
            0x73 => 89,  // Ro
            0x76 => 194, // F24
            0x79 => 92,  // Henkan
            0x7B => 94,  // Muhenkan
            0x7D => 124, // Yen
            0x7E => 95,  // Japanese keypad comma
            _ => 0,
        };
    }

    private void TapKey(int key)
    {
        EmitKey(key, true);
        EmitKey(key, false);
    }

    private void EmitKey(int key, bool pressed) => _emit(new { type = "key", window_id = _id, evdev_keycode = key, pressed });

    private void ForwardPendingAlt()
    {
        if (_pendingAltKey == 0)
            return;
        int key = _pendingAltKey;
        _pendingAltKey = 0;
        _keysDown.Add(key);
        EmitKey(key, true);
    }

    private void ReleaseInput(bool releaseCapture = true)
    {
        _pendingAltKey = 0;
        ReleaseButtons(releaseCapture);
        foreach (int key in _keysDown)
            EmitKey(key, false);
        _keysDown.Clear();
    }

    private void ReleaseButtons(bool releaseCapture)
    {
        if (_buttonsDown.Count != 0)
        {
            if (GetCursorPos(out var point) && ScreenToClient(Handle, ref point))
                _lastPointer = point;
            foreach (int button in _buttonsDown)
                EmitButton(button, false);
            _buttonsDown.Clear();
            FinishPointerDrag();
        }
        if (releaseCapture && GetCapture() == Handle)
            ReleaseCapture();
    }
}
