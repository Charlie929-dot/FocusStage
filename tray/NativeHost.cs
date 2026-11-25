using System.Buffers;
using System.Collections.Concurrent;
using System.ComponentModel;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;
using K4os.Compression.LZ4;
using static FocusStageTray.NativeHostInterop;

namespace FocusStageTray;

internal static class NativeHost
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    };

    internal static int Run()
    {
        try
        {
            using var input = Console.OpenStandardInput();
            using var output = Console.OpenStandardOutput();
            using var session = new Session();
            return session.Run(input, output);
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"FocusStage window host: {ex}");
            return 1;
        }
    }

    internal readonly record struct WindowRect(int X, int Y, int Width, int Height);

    internal readonly record struct AgentOverlayState(
        bool Active,
        string? AgentName,
        double PointerX,
        double PointerY,
        bool PointerVisible,
        string PointerState);

    private sealed record Command(
        string Type, string WindowId = "", string StageId = "", string Title = "",
        WindowRect Rect = default, double Scale = 1, string? ParentWindowId = null,
        int Direction = -1, bool Maximized = false, string? Text = null, string? ImagePngBase64 = null,
        int Width = 0, int Height = 0, ulong Seq = 0, string WmClass = "", string Url = "",
        string? IconPng = null, int? Lz4Bytes = null,
        bool Active = false, string? AgentName = null, double PointerX = 0, double PointerY = 0,
        bool PointerVisible = false, string PointerState = "idle",
        string Cookie = "", string ActionId = "", string Message = "",
        string? ProgramName = null, string? CommandLine = null,
        string User = "", string[]? Users = null, string Prompt = "",
        bool EchoOn = false, string? Info = null, string? Error = null,
        bool Retry = false, bool Succeeded = false);

    private static void OpenWebUrl(string url)
    {
        if (!Uri.TryCreate(url, UriKind.Absolute, out var uri) ||
            uri.Scheme is not ("http" or "https") || string.IsNullOrEmpty(uri.Host))
        {
            Console.Error.WriteLine("FocusStage window host: rejected non-HTTP(S) browser URL.");
            return;
        }
        try
        {
            using var browser = System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(uri.AbsoluteUri)
            {
                UseShellExecute = true,
            });
        }
        catch (Win32Exception)
        {
            Console.Error.WriteLine("FocusStage window host: could not open the Windows default browser.");
        }
    }

    private sealed class Session : IDisposable
    {
        private readonly Dictionary<string, NativeHostWindow> _windows = new(StringComparer.Ordinal);
        private readonly Channel<object> _events = Channel.CreateUnbounded<object>(new UnboundedChannelOptions
        {
            SingleReader = true,
            SingleWriter = false,
        });
        private readonly NativeHostRenderer.Factory _renderer = new();
        private readonly Dispatcher _dispatcher;
        private readonly ElevationDialogHost _elevationDialogs;
        private bool _stopping;
        private int _status;

        internal Session()
        {
            try
            {
                _dispatcher = new Dispatcher(ex => Stop(ex));
                _elevationDialogs = new ElevationDialogHost(
                    onSubmit: (cookie, user, response) =>
                        Emit(new { type = "auth_submit", cookie, user, response }),
                    onCancel: cookie =>
                        Emit(new { type = "auth_cancel", cookie }));
                _dispatcher.ClipboardChanged += payload =>
                {
                    if (payload.ImagePng is not null)
                        Emit(new { type = "host_clipboard_changed", image_png_base64 = Convert.ToBase64String(payload.ImagePng) });
                    else if (payload.Text is not null)
                        Emit(new { type = "host_clipboard_changed", text = payload.Text });
                    else
                        Emit(new { type = "host_clipboard_changed" });
                };
            }
            catch
            {
                _renderer.Dispose();
                throw;
            }
        }

        internal int Run(Stream input, Stream output)
        {
            var primary = WorkAreaForMonitor(MonitorFromPoint(default, MonitorDefaultToPrimary));
            Emit(new {
                type = "ready",
                scale = GetDpiForSystem() / 96.0,
                width = primary.Monitor.Width,
                height = primary.Monitor.Height,
                lz4 = true,
                locale = System.Globalization.CultureInfo.CurrentUICulture.Name,
            });
            _dispatcher.SyncInitialClipboard();

            Task<bool> writer = Task.Factory.StartNew(() => WriteEvents(output), CancellationToken.None,
                TaskCreationOptions.LongRunning, TaskScheduler.Default);
            _ = Task.Factory.StartNew(() => ReadCommands(input), CancellationToken.None,
                TaskCreationOptions.LongRunning, TaskScheduler.Default);

            try
            {
                const uint wmKeyDown = 0x0100, wmSysKeyDown = 0x0104;
                const nuint vkPacket = 0xE7;
                // Dispatch physical keys directly; translate only VK_PACKET (KEYEVENTF_UNICODE)
                // so remote/synthetic Unicode text produces WM_CHAR without duplicating hardware keys.
                while (true)
                {
                    int result = GetMessage(out var message, 0, 0, 0);
                    if (result == 0)
                        break;
                    if (result < 0)
                        throw new Win32Exception();
                    if (message.Message is wmKeyDown or wmSysKeyDown && message.WParam == vkPacket)
                        TranslateMessage(in message);
                    Dispatch(in message);
                }
            }
            finally
            {
                _events.Writer.TryComplete();
                if (!writer.GetAwaiter().GetResult())
                    _status = 1;
            }
            return _status;
        }

        private void Emit(object value) => _events.Writer.TryWrite(value);

        private bool WriteEvents(Stream output)
        {
            try
            {
                using var writer = new StreamWriter(output, new UTF8Encoding(false), leaveOpen: true)
                {
                    NewLine = "\n",
                };
                var events = _events.Reader;
                while (events.WaitToReadAsync().AsTask().GetAwaiter().GetResult())
                {
                    while (events.TryRead(out var value))
                    {
                        writer.WriteLine(JsonSerializer.Serialize(value, JsonOptions));
                        writer.Flush();
                    }
                }
                return true;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"FocusStage window host output: {ex}");
                _dispatcher.Post(() => Stop());
                return false;
            }
        }

        private void ReadCommands(Stream input)
        {
            try
            {
                var reader = new WireReader(input);
                while (reader.TryReadCommand(out var command))
                {
                    byte[]? pixels = command.Type == "frame" ? reader.ReadFrame(command) : null;
                    _dispatcher.Post(() => Apply(command, pixels));
                }
                _dispatcher.Post(() => Stop());
            }
            catch (Exception ex)
            {
                _dispatcher.Post(() => Stop(ex));
            }
        }

        private void Apply(Command command, byte[]? pixels)
        {
            if (_stopping)
            {
                if (pixels is not null)
                    ArrayPool<byte>.Shared.Return(pixels);
                return;
            }

            string id = command.WindowId;
            _windows.TryGetValue(id, out var window);
            switch (command.Type)
            {
                case "show":
                    bool added = window is null;
                    if (window is null)
                    {
                        window = new NativeHostWindow(id, _renderer, Emit, command.Rect, command.Scale,
                            command.WmClass, command.IconPng);
                        _windows.Add(id, window);
                    }
                    window.SetStage(command.StageId);
                    if (added || window.ParentId != command.ParentWindowId)
                    {
                        window.ParentId = command.ParentWindowId;
                        UpdateOwners();
                    }
                    window.Show(command.Title, command.Rect, command.Maximized, added);
                    break;
                case "frame":
                    if (window is null)
                        ArrayPool<byte>.Shared.Return(pixels!);
                    else
                        window.SetFrame(command.Width, command.Height, pixels!);
                    Emit(new { type = "frame_done", window_id = id, seq = command.Seq });
                    break;
                case "clipboard":
                    if (command.ImagePngBase64 is not null)
                    {
                        try
                        {
                            _dispatcher.SetClipboardImage(Convert.FromBase64String(command.ImagePngBase64));
                        }
                        catch (FormatException ex)
                        {
                            Console.Error.WriteLine($"FocusStage window host: invalid clipboard PNG base64: {ex.Message}");
                        }
                    }
                    else if (command.Text is not null)
                    {
                        _dispatcher.SetClipboard(command.Text);
                    }
                    else
                    {
                        _dispatcher.ClearClipboard();
                    }
                    break;
                case "open_url":
                    OpenWebUrl(command.Url);
                    break;
                case "activate":
                    window?.Activate();
                    break;
                case "agent_overlay":
                    window?.SetAgentOverlay(new AgentOverlayState(
                        command.Active,
                        command.AgentName,
                        command.PointerX,
                        command.PointerY,
                        command.PointerVisible,
                        string.IsNullOrEmpty(command.PointerState) ? "idle" : command.PointerState));
                    break;
                case "destroy" when window is not null:
                    _windows.Remove(id);
                    UpdateOwners();
                    window.Dispose();
                    break;
                case "hide":
                    window?.Hide();
                    break;
                case "title":
                    window?.SetTitle(command.Title);
                    break;
                case "parent" when window is not null:
                    window.ParentId = command.ParentWindowId;
                    UpdateOwners();
                    break;
                case "maximize":
                    window?.Maximize(command.Maximized);
                    break;
                case "minimize":
                    window?.Minimize();
                    break;
                case "move":
                    window?.StartMove();
                    break;
                case "resize":
                    window?.StartResize(command.Direction);
                    break;
                case "auth_begin":
                    _elevationDialogs.Begin(new AuthBeginRequest(
                        command.Cookie,
                        command.ActionId,
                        command.Message,
                        command.IconPng,
                        command.ProgramName,
                        command.CommandLine,
                        command.User,
                        command.Users ?? Array.Empty<string>(),
                        command.Prompt,
                        command.EchoOn,
                        command.Info,
                        command.Error));
                    break;
                case "auth_prompt":
                    _elevationDialogs.Prompt(new AuthPromptRequest(
                        command.Cookie,
                        command.Prompt,
                        command.EchoOn,
                        command.Info,
                        command.Error,
                        command.Retry));
                    break;
                case "auth_end":
                    _elevationDialogs.End(new AuthEndRequest(
                        command.Cookie,
                        command.Succeeded,
                        command.Message));
                    break;
            }
        }

        private void UpdateOwners()
        {
            // Detach obsolete links first so reversing an ownership chain cannot create a temporary cycle.
            foreach (var window in _windows.Values)
            {
                if (window.Owner != 0 && window.Owner != DesiredOwner(window))
                    window.SetOwner(0);
            }
            foreach (var window in _windows.Values)
            {
                nint owner = DesiredOwner(window);
                if (owner != window.Owner)
                    window.SetOwner(owner);
            }
        }

        private nint DesiredOwner(NativeHostWindow window) =>
            window.ParentId is { } id && _windows.TryGetValue(id, out var parent) ? parent.Handle : 0;

        private void Stop(Exception? error = null)
        {
            if (error is not null)
            {
                _status = 1;
                Console.Error.WriteLine($"FocusStage window host: {error}");
            }
            if (_stopping)
                return;
            _stopping = true;
            foreach (var window in _windows.Values)
                window.CancelInteraction();
            PostQuitMessage(_status);
        }

        public void Dispose()
        {
            _elevationDialogs.Dispose();
            foreach (var window in _windows.Values)
                window.SetOwner(0);
            foreach (var window in _windows.Values)
                window.Dispose();
            _windows.Clear();
            _dispatcher.Dispose();
            _renderer.Dispose();
        }
    }

    private sealed class WireReader(Stream input)
    {
        private readonly byte[] _buffer = new byte[16 * 1024];
        private readonly ArrayBufferWriter<byte> _line = new();
        private int _offset;
        private int _end;

        internal bool TryReadCommand(out Command command)
        {
            _line.Clear();
            while (true)
            {
                if (_offset == _end)
                {
                    _end = input.Read(_buffer);
                    _offset = 0;
                    if (_end == 0)
                    {
                        if (_line.WrittenCount != 0)
                            throw new EndOfStreamException("Incomplete command header.");
                        command = null!;
                        return false;
                    }
                }

                var unread = _buffer.AsSpan(_offset, _end - _offset);
                int newline = unread.IndexOf((byte)'\n');
                if (newline >= 0)
                {
                    _line.Write(unread[..newline]);
                    _offset += newline + 1;
                    command = JsonSerializer.Deserialize<Command>(_line.WrittenSpan, JsonOptions)
                        ?? throw new JsonException("Expected a command object.");
                    return true;
                }
                _line.Write(unread);
                _offset = _end;
            }
        }

        internal byte[] ReadFrame(Command command)
        {
            if (command.Width <= 0 || command.Height <= 0)
                throw new InvalidDataException("Frame dimensions must be positive.");
            int length = checked(command.Width * command.Height * 4);
            byte[] pixels = ArrayPool<byte>.Shared.Rent(length);
            try
            {
                if (command.Lz4Bytes is int compressedLength)
                {
                    if (compressedLength <= 0 || compressedLength >= length)
                        throw new InvalidDataException("Invalid LZ4 frame length.");
                    byte[] compressed = ArrayPool<byte>.Shared.Rent(compressedLength);
                    try
                    {
                        var block = compressed.AsSpan(0, compressedLength);
                        ReadPayload(block);
                        if (LZ4Codec.Decode(block, pixels.AsSpan(0, length)) != length)
                            throw new InvalidDataException("LZ4 frame did not decode to its declared dimensions.");
                    }
                    finally
                    {
                        ArrayPool<byte>.Shared.Return(compressed);
                    }
                }
                else
                {
                    ReadPayload(pixels.AsSpan(0, length));
                }
                return pixels;
            }
            catch
            {
                ArrayPool<byte>.Shared.Return(pixels);
                throw;
            }
        }

        private void ReadPayload(Span<byte> payload)
        {
            int buffered = Math.Min(_end - _offset, payload.Length);
            _buffer.AsSpan(_offset, buffered).CopyTo(payload);
            _offset += buffered;
            input.ReadExactly(payload[buffered..]);
        }
    }

    private sealed class Dispatcher : NativeWindow, IDisposable
    {
        private const int DispatchMessage = 0x8001;
        private const int ClipboardUpdate = 0x031D;
        private readonly ConcurrentQueue<Action> _commands = new();
        private readonly Action<Exception> _failed;
        private readonly nint _hwnd;
        private int _scheduled;
        private bool _listening;
        private uint? _lastClipboardSequence;

        internal event Action<ClipboardPayload>? ClipboardChanged;

        internal Dispatcher(Action<Exception> failed)
        {
            _failed = failed;
            CreateHandle(new CreateParams());
            _hwnd = Handle;
            _listening = AddClipboardFormatListener(_hwnd);
            if (!_listening)
                Console.Error.WriteLine("FocusStage window host: unable to listen for clipboard changes.");
        }

        internal void SyncInitialClipboard()
        {
            if (TryReadClipboard(_hwnd, out uint sequence, out var payload))
            {
                _lastClipboardSequence = sequence;
                if (!payload.IsEmpty)
                    ClipboardChanged?.Invoke(payload);
            }
        }

        internal void Post(Action action)
        {
            _commands.Enqueue(action);
            Schedule();
        }

        private void Schedule()
        {
            if (Interlocked.Exchange(ref _scheduled, 1) == 0)
                PostMessage(_hwnd, DispatchMessage, 0, 0);
        }

        protected override void WndProc(ref Message message)
        {
            switch (message.Msg)
            {
                case DispatchMessage:
                    Interlocked.Exchange(ref _scheduled, 0);
                    if (_commands.TryDequeue(out var action))
                    {
                        // Native move/resize opens a nested message loop. Let it dispatch later commands.
                        if (!_commands.IsEmpty)
                            Schedule();
                        try
                        {
                            action();
                        }
                        catch (Exception ex)
                        {
                            _failed(ex);
                        }
                    }
                    return;
                case ClipboardUpdate:
                    if (GetClipboardOwner() != _hwnd && TryReadClipboard(_hwnd, out uint sequence, out var payload)
                        && sequence != _lastClipboardSequence)
                    {
                        _lastClipboardSequence = sequence;
                        ClipboardChanged?.Invoke(payload);
                    }
                    return;
            }
            base.WndProc(ref message);
        }

        internal void SetClipboard(string text)
        {
            if (TryWriteClipboard(_hwnd, text))
                _lastClipboardSequence = GetClipboardSequenceNumber();
            else
                Console.Error.WriteLine("FocusStage window host: unable to write Windows clipboard text.");
        }

        internal void SetClipboardImage(byte[] pngBytes)
        {
            if (TryWriteClipboardImage(_hwnd, pngBytes))
                _lastClipboardSequence = GetClipboardSequenceNumber();
            else
                Console.Error.WriteLine("FocusStage window host: unable to write Windows clipboard image.");
        }

        internal void ClearClipboard()
        {
            if (TryClearClipboard(_hwnd))
                _lastClipboardSequence = GetClipboardSequenceNumber();
        }

        public void Dispose()
        {
            if (_listening)
            {
                RemoveClipboardFormatListener(_hwnd);
                _listening = false;
            }
            DestroyHandle();
        }
    }
}
