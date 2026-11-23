using System.Buffers;
using System.Collections.Concurrent;
using System.ComponentModel;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;
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

    private sealed record Command(
        string Type, string WindowId = "", string StageId = "", string Title = "",
        WindowRect Rect = default, double Scale = 1, string? ParentWindowId = null,
        int Direction = -1, bool Maximized = false, string Text = "",
        int Width = 0, int Height = 0, ulong Seq = 0, string WmClass = "", string Url = "",
        string? IconPng = null);

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
            SingleWriter = true,
        });
        private readonly NativeHostRenderer.Factory _renderer = new();
        private readonly Dispatcher _dispatcher;
        private bool _stopping;
        private int _status;

        internal Session()
        {
            try
            {
                _dispatcher = new Dispatcher(ex => Stop(ex));
                _dispatcher.ClipboardChanged += text => Emit(new { type = "host_clipboard_changed", text });
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
            Emit(new { type = "ready", scale = GetDpiForSystem() / 96.0,
                width = primary.Monitor.Width, height = primary.Monitor.Height });

            Task<bool> writer = Task.Factory.StartNew(() => WriteEvents(output), CancellationToken.None,
                TaskCreationOptions.LongRunning, TaskScheduler.Default);
            _ = Task.Factory.StartNew(() => ReadCommands(input), CancellationToken.None,
                TaskCreationOptions.LongRunning, TaskScheduler.Default);

            try
            {
                // Dispatch raw keys directly; the Linux client handles character input.
                while (true)
                {
                    int result = GetMessage(out var message, 0, 0, 0);
                    if (result == 0)
                        break;
                    if (result < 0)
                        throw new Win32Exception();
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
                    _dispatcher.SetClipboard(command.Text);
                    break;
                case "open_url":
                    OpenWebUrl(command.Url);
                    break;
                case "activate":
                    window?.Activate();
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
                int buffered = Math.Min(_end - _offset, length);
                _buffer.AsSpan(_offset, buffered).CopyTo(pixels);
                _offset += buffered;
                input.ReadExactly(pixels.AsSpan(buffered, length - buffered));
                return pixels;
            }
            catch
            {
                ArrayPool<byte>.Shared.Return(pixels);
                throw;
            }
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

        internal event Action<string>? ClipboardChanged;

        internal Dispatcher(Action<Exception> failed)
        {
            _failed = failed;
            CreateHandle(new CreateParams());
            _hwnd = Handle;
            _listening = AddClipboardFormatListener(_hwnd);
            if (!_listening)
                Console.Error.WriteLine("FocusStage window host: unable to listen for clipboard changes.");
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
                    if (GetClipboardOwner() != _hwnd && TryReadClipboard(_hwnd, out uint sequence, out var text)
                        && sequence != _lastClipboardSequence)
                    {
                        _lastClipboardSequence = sequence;
                        if (text is not null)
                            ClipboardChanged?.Invoke(text);
                    }
                    return;
            }
            base.WndProc(ref message);
        }

        internal void SetClipboard(string text)
        {
            if (!TryWriteClipboard(_hwnd, text))
                Console.Error.WriteLine("FocusStage window host: unable to write Windows clipboard text.");
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
