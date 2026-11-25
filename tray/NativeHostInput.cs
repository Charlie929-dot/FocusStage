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
    private const int VkTab = 0x09, VkPause = 0x13, VkEscape = 0x1B, VkSpace = 0x20, VkSnapshot = 0x2C, VkF4 = 0x73, VkPacket = 0xE7;
    private const int KeyLeftShift = 42, KeyRightShift = 54, KeyLeftAlt = 56, KeyRightAlt = 100;

    private readonly HashSet<int> _keysDown = new();
    private readonly HashSet<int> _buttonsDown = new();
    private int _pendingAltKey;
    private char _pendingHighSurrogate;
    private bool _lastCharWasCr;
    private NativePoint _lastPointer;
    private bool _trackingPointer;

    private bool HandleInput(ref Message message)
    {
        switch (message.Msg)
        {
            case WmMouseMove:
                _lastPointer = NativePoint.FromLParam(message.LParam);
                TrackPointerLeave();
                if (IsOverOverlayBanner(_lastPointer))
                {
                    UpdateBannerHover(true);
                    return true;
                }
                UpdateBannerHover(false);
                var (x, y) = LogicalPoint(_lastPointer);
                _emit(new { type = "pointer_motion", window_id = _id, x, y });
                return true;
            case WmMouseLeave:
                UpdateBannerHover(false);
                if (_trackingPointer)
                {
                    _trackingPointer = false;
                    if (_buttonsDown.Count == 0)
                        EmitPointerLeave();
                }
                return true;
            case WmLeftDown:
            case WmLeftDoubleClick:
                if (TryCancelTakeoverOnBannerClick(NativePoint.FromLParam(message.LParam)))
                    return true;
                ButtonMessage(message.LParam, BtnLeft, true);
                return true;
            case WmLeftUp:
                if (_swallowBannerLeftUp)
                {
                    _swallowBannerLeftUp = false;
                    return true;
                }
                ButtonMessage(message.LParam, BtnLeft, false);
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
            case WmSysChar:
                return CharMessage(message);
            case WmDeadChar:
            case WmSysDeadChar:
                _pendingHighSurrogate = '\0';
                _lastCharWasCr = false;
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

    private bool CharMessage(Message message)
    {
        char ch = (char)(ushort)message.WParam;
        if (char.IsHighSurrogate(ch))
        {
            _pendingHighSurrogate = ch;
            _lastCharWasCr = false;
            return true;
        }
        string? text = null;
        if (char.IsLowSurrogate(ch))
        {
            char high = _pendingHighSurrogate;
            _pendingHighSurrogate = '\0';
            _lastCharWasCr = false;
            if (high != '\0')
                text = new string([high, ch]);
        }
        else
        {
            _pendingHighSurrogate = '\0';
            if (ch == '\r')
            {
                _lastCharWasCr = true;
                ForwardPendingAlt();
                TapKey(28); // Enter
                return true;
            }
            if (ch == '\n')
            {
                bool skip = _lastCharWasCr;
                _lastCharWasCr = false;
                if (!skip)
                {
                    ForwardPendingAlt();
                    TapKey(28);
                }
                return true;
            }
            _lastCharWasCr = false;
            if (ch == '\t')
            {
                ForwardPendingAlt();
                TapKey(15); // Tab
                return true;
            }
            if (ch == '\b')
            {
                ForwardPendingAlt();
                TapKey(14); // Backspace
                return true;
            }
            if (ch >= 0x20 && ch != 0x7F)
                text = ch.ToString();
        }
        if (text is not null)
        {
            ForwardPendingAlt();
            _emit(new { type = "text_input", window_id = _id, text });
        }
        return true;
    }

    private static int EvdevKey(int vk, int scan, bool extended)
    {
        if (vk == VkPacket)
            return 0;
        if (vk == VkPause)
            return 119;
        if (vk == VkSnapshot)
            return 99;
        // Synthetic injectors (e.g. remote desktop tools) may pass explicit left/right/meta VKs
        // with scan == 0 or without KEYEVENTF_EXTENDEDKEY.
        int explicitVk = vk switch
        {
            0x5B => 125, // VK_LWIN
            0x5C => 126, // VK_RWIN
            0x5D => 127, // VK_APPS
            0xA0 => 42,  // VK_LSHIFT
            0xA1 => 54,  // VK_RSHIFT
            0xA2 => 29,  // VK_LCONTROL
            0xA3 => 97,  // VK_RCONTROL
            0xA4 => 56,  // VK_LMENU
            0xA5 => 100, // VK_RMENU
            _ => 0,
        };
        if (explicitVk != 0)
            return explicitVk;

        if (scan != 0)
        {
            int fromScan = EvdevKeyFromScan(scan, extended);
            if (fromScan != 0)
                return fromScan;
        }
        return EvdevKeyFromVk(vk, extended);
    }

    private static int EvdevKeyFromScan(int scan, bool extended)
    {
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

    private static int EvdevKeyFromVk(int vk, bool extended)
    {
        int key = vk switch
        {
            0x08 => 14,                  // VK_BACK
            0x09 => 15,                  // VK_TAB
            0x0D => extended ? 96 : 28,  // VK_RETURN
            0x10 => 42,                  // VK_SHIFT
            0x11 => extended ? 97 : 29,  // VK_CONTROL
            0x12 => extended ? 100 : 56, // VK_MENU (Alt)
            0x14 => 58,                  // VK_CAPITAL
            0x1B => 1,                   // VK_ESCAPE
            0x20 => 57,                  // VK_SPACE
            0x21 => 104,                 // VK_PRIOR (Page Up)
            0x22 => 109,                 // VK_NEXT (Page Down)
            0x23 => 107,                 // VK_END
            0x24 => 102,                 // VK_HOME
            0x25 => 105,                 // VK_LEFT
            0x26 => 103,                 // VK_UP
            0x27 => 106,                 // VK_RIGHT
            0x28 => 108,                 // VK_DOWN
            0x2D => 110,                 // VK_INSERT
            0x2E => 111,                 // VK_DELETE
            0x5F => 142,                 // VK_SLEEP
            0x60 => 82,                  // VK_NUMPAD0
            0x61 => 79,                  // VK_NUMPAD1
            0x62 => 80,                  // VK_NUMPAD2
            0x63 => 81,                  // VK_NUMPAD3
            0x64 => 75,                  // VK_NUMPAD4
            0x65 => 76,                  // VK_NUMPAD5
            0x66 => 77,                  // VK_NUMPAD6
            0x67 => 71,                  // VK_NUMPAD7
            0x68 => 72,                  // VK_NUMPAD8
            0x69 => 73,                  // VK_NUMPAD9
            0x6A => 55,                  // VK_MULTIPLY
            0x6B => 78,                  // VK_ADD
            0x6D => 74,                  // VK_SUBTRACT
            0x6E => 83,                  // VK_DECIMAL
            0x6F => 98,                  // VK_DIVIDE
            >= 0x70 and <= 0x79 => vk - 0x70 + 59, // VK_F1..VK_F10
            0x7A => 87,                  // VK_F11
            0x7B => 88,                  // VK_F12
            >= 0x7C and <= 0x86 => vk - 0x7C + 183, // VK_F13..VK_F23
            0x87 => 194,                 // VK_F24
            0x90 => 69,                  // VK_NUMLOCK
            0x91 => 70,                  // VK_SCROLL
            0xA6 => 158,                 // VK_BROWSER_BACK
            0xA7 => 159,                 // VK_BROWSER_FORWARD
            0xA8 => 173,                 // VK_BROWSER_REFRESH
            0xA9 => 128,                 // VK_BROWSER_STOP
            0xAA => 217,                 // VK_BROWSER_SEARCH
            0xAB => 156,                 // VK_BROWSER_FAVORITES
            0xAC => 172,                 // VK_BROWSER_HOME
            0xAD => 113,                 // VK_VOLUME_MUTE
            0xAE => 114,                 // VK_VOLUME_DOWN
            0xAF => 115,                 // VK_VOLUME_UP
            0xB0 => 163,                 // VK_MEDIA_NEXT_TRACK
            0xB1 => 165,                 // VK_MEDIA_PREV_TRACK
            0xB2 => 166,                 // VK_MEDIA_STOP
            0xB3 => 164,                 // VK_MEDIA_PLAY_PAUSE
            0xB4 => 155,                 // VK_LAUNCH_MAIL
            0xB5 => 226,                 // VK_LAUNCH_MEDIA_SELECT
            0xB6 => 157,                 // VK_LAUNCH_APP1
            0xB7 => 140,                 // VK_LAUNCH_APP2
            _ => 0,
        };
        if (key != 0)
            return key;

        const uint mapVkToVscEx = 4;
        uint mapped = MapVirtualKeyEx((uint)vk, mapVkToVscEx, GetKeyboardLayout(0));
        if (mapped != 0)
        {
            int mappedScan = (int)(mapped & 0xFF);
            bool mappedExt = (mapped & 0xFF00) is 0xE000 or 0xE100;
            int fromMapped = EvdevKeyFromScan(mappedScan, mappedExt);
            if (fromMapped != 0)
                return fromMapped;
        }

        return vk switch
        {
            0x30 => 11, // 0
            >= 0x31 and <= 0x39 => vk - 0x31 + 2, // 1..9
            0x41 => 30, // A
            0x42 => 48, // B
            0x43 => 46, // C
            0x44 => 32, // D
            0x45 => 18, // E
            0x46 => 33, // F
            0x47 => 34, // G
            0x48 => 35, // H
            0x49 => 23, // I
            0x4A => 36, // J
            0x4B => 37, // K
            0x4C => 38, // L
            0x4D => 50, // M
            0x4E => 49, // N
            0x4F => 24, // O
            0x50 => 25, // P
            0x51 => 16, // Q
            0x52 => 19, // R
            0x53 => 31, // S
            0x54 => 20, // T
            0x55 => 22, // U
            0x56 => 47, // V
            0x57 => 17, // W
            0x58 => 45, // X
            0x59 => 21, // Y
            0x5A => 44, // Z
            0xBA => 39, // VK_OEM_1 (;:)
            0xBB => 13, // VK_OEM_PLUS (=+)
            0xBC => 51, // VK_OEM_COMMA (,<)
            0xBD => 12, // VK_OEM_MINUS (-_)
            0xBE => 52, // VK_OEM_PERIOD (.>)
            0xBF => 53, // VK_OEM_2 (/?)
            0xC0 => 41, // VK_OEM_3 (`~)
            0xDB => 26, // VK_OEM_4 ([{)
            0xDC => 43, // VK_OEM_5 (\|)
            0xDD => 27, // VK_OEM_6 (]})
            0xDE => 40, // VK_OEM_7 ('")
            0xE2 => 86, // VK_OEM_102
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
        _pendingHighSurrogate = '\0';
        _lastCharWasCr = false;
        _swallowBannerLeftUp = false;
        UpdateBannerHover(false);
        ReleaseButtons(releaseCapture);
        foreach (int key in _keysDown)
            EmitKey(key, false);
        _keysDown.Clear();
    }

    private bool IsOverOverlayBanner(NativePoint point) =>
        _renderer.IsAgentOverlayActive && _renderer.HitTestBanner(point.X, point.Y);

    private void UpdateBannerHover(bool hovered)
    {
        if (_bannerHovered == hovered)
            return;
        _bannerHovered = hovered;
        _renderer.SetBannerHovered(hovered);
        if (Handle != 0)
            InvalidateRect(Handle, 0, false);
    }

    private bool TryCancelTakeoverOnBannerClick(NativePoint point)
    {
        if (!IsOverOverlayBanner(point))
            return false;
        _swallowBannerLeftUp = true;
        UpdateBannerHover(false);
        _emit(new { type = "cancel_takeover", window_id = _id });
        return true;
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
