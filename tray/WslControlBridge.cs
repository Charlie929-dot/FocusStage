using System.Diagnostics;
using System.Text;

namespace FocusStageTray;

/// <summary>
/// Keeps a single persistent <c>wsl.exe -d &lt;distro&gt; -e focusstage control-bridge</c>
/// process alive for background tray polling so WSL does not tear down/recreate session
/// plumbing or churn <c>/etc/resolv.conf</c> every 1.2s.
/// </summary>
internal sealed class WslControlBridge : IDisposable
{
    private static readonly UTF8Encoding Utf8NoBom = new(encoderShouldEmitUTF8Identifier: false);
    private const int MaxStderrLines = 24;
    private static readonly TimeSpan RestartBackoff = TimeSpan.FromSeconds(3);

    private readonly string _distro;
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly CancellationTokenSource _disposeCts = new();
    private readonly object _procLock = new();
    private readonly object _stderrLock = new();
    private readonly Queue<string> _recentStderr = new();

    private Process? _proc;
    private DateTime _nextSpawnAllowedUtc = DateTime.MinValue;
    private bool _hasServedResponse;
    private bool _unsupported;
    private int _disposed;

    public WslControlBridge(string distro)
    {
        _distro = distro;
    }

    public bool IsUnsupported => Volatile.Read(ref _unsupported);

    public async Task<(bool Ok, string Stdout)> CallAsync(
        string requestJson,
        int timeoutMs = 8000,
        CancellationToken cancellationToken = default)
    {
        if (Volatile.Read(ref _disposed) != 0 || IsUnsupported)
        {
            return (false, "");
        }

        using var linkedCts = CancellationTokenSource.CreateLinkedTokenSource(_disposeCts.Token, cancellationToken);
        try
        {
            await _gate.WaitAsync(linkedCts.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            return (false, "");
        }

        try
        {
            if (Volatile.Read(ref _disposed) != 0 || IsUnsupported)
            {
                return (false, "");
            }

            var proc = EnsureStartedLocked();
            if (proc is null)
            {
                return (false, "");
            }

            using var reqCts = CancellationTokenSource.CreateLinkedTokenSource(linkedCts.Token);
            reqCts.CancelAfter(timeoutMs);

            var singleLine = requestJson.Replace("\r", "").Replace("\n", "");
            await proc.StandardInput.WriteLineAsync(singleLine.AsMemory(), reqCts.Token).ConfigureAwait(false);
            await proc.StandardInput.FlushAsync(reqCts.Token).ConfigureAwait(false);

            var line = await proc.StandardOutput.ReadLineAsync(reqCts.Token).AsTask().WaitAsync(reqCts.Token).ConfigureAwait(false);
            if (string.IsNullOrWhiteSpace(line))
            {
                HandleUnexpectedExitLocked(proc);
                return (false, "");
            }

            _hasServedResponse = true;
            return (true, line.Trim());
        }
        catch
        {
            ResetProcessLocked();
            _nextSpawnAllowedUtc = DateTime.UtcNow.Add(RestartBackoff);
            return (false, "");
        }
        finally
        {
            _gate.Release();
        }
    }

    private Process? EnsureStartedLocked()
    {
        lock (_procLock)
        {
            if (Volatile.Read(ref _disposed) != 0 || _unsupported)
            {
                return null;
            }
            if (_proc is { HasExited: false })
            {
                return _proc;
            }
        }

        HandleUnexpectedExitLocked(null);

        if (DateTime.UtcNow < _nextSpawnAllowedUtc)
        {
            return null;
        }

        lock (_stderrLock)
        {
            _recentStderr.Clear();
        }

        var psi = new ProcessStartInfo("wsl.exe")
        {
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
            StandardInputEncoding = Utf8NoBom,
            StandardOutputEncoding = Encoding.UTF8,
            StandardErrorEncoding = Encoding.UTF8
        };
        psi.ArgumentList.Add("-d");
        psi.ArgumentList.Add(_distro);
        psi.ArgumentList.Add("-e");
        psi.ArgumentList.Add("focusstage");
        psi.ArgumentList.Add("control-bridge");

        try
        {
            var proc = Process.Start(psi);
            if (proc is null)
            {
                _nextSpawnAllowedUtc = DateTime.UtcNow.Add(RestartBackoff);
                return null;
            }

            proc.ErrorDataReceived += OnErrorDataReceived;
            proc.BeginErrorReadLine();

            lock (_procLock)
            {
                if (Volatile.Read(ref _disposed) != 0)
                {
                    TryKillAndDispose(proc);
                    return null;
                }
                _proc = proc;
            }
            return proc;
        }
        catch
        {
            _nextSpawnAllowedUtc = DateTime.UtcNow.Add(RestartBackoff);
            return null;
        }
    }

    private void OnErrorDataReceived(object sender, DataReceivedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(e.Data))
        {
            return;
        }

        lock (_stderrLock)
        {
            _recentStderr.Enqueue(e.Data);
            while (_recentStderr.Count > MaxStderrLines)
            {
                _recentStderr.Dequeue();
            }
        }
    }

    private void HandleUnexpectedExitLocked(Process? expected)
    {
        Process? exited;
        lock (_procLock)
        {
            if (expected is not null && !ReferenceEquals(_proc, expected))
            {
                return;
            }
            exited = _proc;
            _proc = null;
        }

        if (exited is not null)
        {
            try
            {
                if (!exited.HasExited)
                {
                    exited.Kill(entireProcessTree: true);
                }
                exited.WaitForExit(150);
            }
            catch
            {
                // Best-effort cleanup of exited/hung bridge process.
            }
            finally
            {
                exited.ErrorDataReceived -= OnErrorDataReceived;
                exited.Dispose();
            }

            if (!_hasServedResponse && StderrIndicatesUnsupported())
            {
                Volatile.Write(ref _unsupported, true);
            }
            _nextSpawnAllowedUtc = DateTime.UtcNow.Add(RestartBackoff);
        }
    }

    private bool StderrIndicatesUnsupported()
    {
        lock (_stderrLock)
        {
            foreach (var line in _recentStderr)
            {
                if (line.Contains("unrecognized subcommand", StringComparison.OrdinalIgnoreCase) ||
                    line.Contains("unexpected argument", StringComparison.OrdinalIgnoreCase))
                {
                    return true;
                }
            }
        }
        return false;
    }

    private void ResetProcessLocked()
    {
        Process? proc;
        lock (_procLock)
        {
            proc = _proc;
            _proc = null;
        }
        if (proc is not null)
        {
            proc.ErrorDataReceived -= OnErrorDataReceived;
            TryKillAndDispose(proc);
        }
    }

    private static void TryKillAndDispose(Process proc)
    {
        try
        {
            try
            {
                proc.StandardInput.Close();
            }
            catch
            {
                // Ignore if stdin was not redirected or already closed.
            }
            if (!proc.HasExited)
            {
                proc.Kill(entireProcessTree: true);
            }
        }
        catch
        {
            // Process may already have exited.
        }
        finally
        {
            proc.Dispose();
        }
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }

        _disposeCts.Cancel();
        ResetProcessLocked();
        _disposeCts.Dispose();
    }
}
