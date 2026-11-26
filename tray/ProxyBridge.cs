using System.Buffers;
using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32;

namespace FocusStageTray;

internal enum UpstreamKind
{
    Direct,
    Socks5,
    Http,
}

internal readonly record struct UpstreamTarget(UpstreamKind Kind, string Host, int Port);

internal sealed record SystemProxyState(
    bool Enabled,
    string ProxyServerRaw,
    string ProxyOverrideRaw,
    string AutoConfigUrl,
    UpstreamTarget? DefaultTcpUpstream,
    UpstreamTarget? ExplicitSocksUpstream,
    string? HttpProxyEnv,
    string? HttpsProxyEnv,
    string? AllProxyEnv,
    string NoProxyEnv)
{
    public string Signature =>
        $"{Enabled}|{ProxyServerRaw}|{ProxyOverrideRaw}|{AutoConfigUrl}";
}

internal sealed class ProxyBridge : IDisposable
{
    private const int Socks5ProbeSuccessTtlSeconds = 300;
    private const int Socks5ProbeFailureTtlSeconds = 60;
    private const int CacheTtlSeconds = 60;
    private const int MaxPacCacheEntries = 2048;
    private const int MaxDnsCacheEntries = 1024;
    private const int RelayBufferSize = 65536;

    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();
    private readonly ConcurrentDictionary<string, (bool SupportsSocks5, DateTime CheckedUtc)> _probeCache = new(StringComparer.OrdinalIgnoreCase);
    private readonly ConcurrentDictionary<string, Lazy<Task<bool>>> _inflightProbes = new(StringComparer.OrdinalIgnoreCase);
    private readonly ConcurrentDictionary<string, (UpstreamTarget? Target, DateTime CachedUtc)> _pacCache = new(StringComparer.OrdinalIgnoreCase);
    private readonly ConcurrentDictionary<string, (IPAddress Address, DateTime CachedUtc)> _dnsCache = new(StringComparer.OrdinalIgnoreCase);
    private readonly Action<ProxyBridge, SystemProxyState> _onStateChanged;
    private readonly object _stateLock = new();
    private readonly object _winHttpLock = new();
    private IntPtr _winHttpSession;
    private long _stateEpoch;
    private SystemProxyState _currentState;
    private bool _disposed;

    public int Port { get; }

    public ProxyBridge(Action<ProxyBridge, SystemProxyState> onStateChanged)
    {
        _onStateChanged = onStateChanged;
        _listener = new TcpListener(IPAddress.Loopback, 0);
        _listener.Start();
        Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
        _currentState = ReadSystemProxyState();
        _ = Task.Run(AcceptLoopAsync);
        _ = Task.Run(EmitInitialAndWatchRegistryLoopAsync);
    }

    public SystemProxyState CurrentState
    {
        get
        {
            lock (_stateLock)
            {
                return _currentState;
            }
        }
    }

    public object BuildHostEventPayload(SystemProxyState state)
    {
        if (!state.Enabled)
        {
            return new
            {
                type = "host_proxy_changed",
                enabled = false,
                bridge_port = Port,
            };
        }

        bool noPac = string.IsNullOrWhiteSpace(state.AutoConfigUrl);
        string? directHttp = noPac ? state.HttpProxyEnv : null;
        string? directHttps = noPac ? (state.HttpsProxyEnv ?? state.HttpProxyEnv) : null;
        string? directSocks = null;
        if (noPac)
        {
            if (state.ExplicitSocksUpstream is { } s)
            {
                directSocks = $"socks5h://{FormatHostPort(s.Host, s.Port)}";
            }
            else if (state.DefaultTcpUpstream is { } tcp
                && TryGetCachedSocks5Support(tcp.Host, tcp.Port) == true)
            {
                directSocks = $"socks5h://{FormatHostPort(tcp.Host, tcp.Port)}";
            }
        }

        return new
        {
            type = "host_proxy_changed",
            enabled = true,
            bridge_port = Port,
            http_proxy = $"http://127.0.0.1:{Port}",
            https_proxy = $"http://127.0.0.1:{Port}",
            all_proxy = $"socks5h://127.0.0.1:{Port}",
            no_proxy = state.NoProxyEnv,
            direct_http_proxy = directHttp,
            direct_https_proxy = directHttps,
            direct_socks_proxy = directSocks,
        };
    }

    private async Task EmitInitialAndWatchRegistryLoopAsync()
    {
        try
        {
            var initial = CurrentState;
            await PreProbeStateAsync(initial, _cts.Token);
            if (!_cts.IsCancellationRequested)
            {
                _onStateChanged(this, initial);
            }
        }
        catch (OperationCanceledException)
        {
            return;
        }
        catch
        {
            // Keep watching even if initial probe/emit hit a transient error.
        }

        while (!_cts.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(1000, _cts.Token);
                var next = ReadSystemProxyState();
                bool changed;
                lock (_stateLock)
                {
                    changed = !string.Equals(_currentState.Signature, next.Signature, StringComparison.Ordinal);
                    if (changed)
                    {
                        _currentState = next;
                        Interlocked.Increment(ref _stateEpoch);
                        _probeCache.Clear();
                        _inflightProbes.Clear();
                        _pacCache.Clear();
                    }
                }
                if (changed)
                {
                    await PreProbeStateAsync(next, _cts.Token);
                    if (!_cts.IsCancellationRequested)
                    {
                        _onStateChanged(this, next);
                    }
                }
            }
            catch (OperationCanceledException)
            {
                break;
            }
            catch
            {
                // Keep watching on transient registry access errors.
            }
        }
    }

    private async Task PreProbeStateAsync(SystemProxyState state, CancellationToken ct)
    {
        if (state.Enabled
            && string.IsNullOrWhiteSpace(state.AutoConfigUrl)
            && state.ExplicitSocksUpstream is null
            && state.DefaultTcpUpstream is { } tcp
            && tcp.Kind != UpstreamKind.Socks5)
        {
            await GetSocks5SupportAsync(tcp.Host, tcp.Port, ct);
        }
    }

    private async Task AcceptLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient? client = null;
            try
            {
                client = await _listener.AcceptTcpClientAsync(_cts.Token);
                client.NoDelay = true;
                _ = HandleClientAsync(client);
            }
            catch (OperationCanceledException)
            {
                client?.Dispose();
                break;
            }
            catch (ObjectDisposedException)
            {
                client?.Dispose();
                break;
            }
            catch
            {
                client?.Dispose();
            }
        }
    }

    private async Task HandleClientAsync(TcpClient client)
    {
        using (client)
        using (var linked = CancellationTokenSource.CreateLinkedTokenSource(_cts.Token))
        {
            var ct = linked.Token;
            try
            {
                var stream = client.GetStream();
                using var reader = new BufferedStreamReader(stream);
                if (!await reader.EnsureAvailableAsync(1, ct))
                {
                    return;
                }

                if (reader.PeekByte() == 0x05)
                {
                    reader.Consume(1);
                    await HandleSocks5ClientAsync(stream, reader, ct);
                }
                else
                {
                    await HandleHttpClientAsync(stream, reader, ct);
                }
            }
            catch
            {
                // Normal connection reset / cancellation.
            }
        }
    }

    private async Task HandleSocks5ClientAsync(NetworkStream stream, BufferedStreamReader reader, CancellationToken ct)
    {
        int nMethods = await reader.ReadByteAsync(ct);
        if (nMethods <= 0)
            return;
        var methods = new byte[nMethods];
        if (!await reader.ReadExactAsync(methods, ct))
            return;

        bool supportsNoAuth = Array.IndexOf(methods, (byte)0x00) >= 0;
        if (!supportsNoAuth)
        {
            await stream.WriteAsync(new byte[] { 0x05, 0xFF }, ct);
            return;
        }
        await stream.WriteAsync(new byte[] { 0x05, 0x00 }, ct);

        var header = new byte[4];
        if (!await reader.ReadExactAsync(header, ct))
            return;
        if (header[0] != 0x05)
            return;

        byte cmd = header[1];
        byte atyp = header[3];
        var target = await ReadSocks5AddressAsync(reader, atyp, ct);
        if (target is null)
        {
            await SendSocks5ReplyAsync(stream, 0x08, null, ct);
            return;
        }
        var (dstHost, dstPort) = target.Value;

        if (cmd == 0x01) // CONNECT
        {
            byte[]? clientTrailing = reader.DetachRemaining();
            reader.Dispose();

            TcpClient? upstreamConn = null;
            try
            {
                byte[]? serverTrailing;
                (upstreamConn, serverTrailing) = await OpenTcpTargetAsync(dstHost, dstPort, ct);
                var boundEp = upstreamConn.Client.LocalEndPoint as IPEndPoint;
                await SendSocks5ReplyAsync(stream, 0x00, boundEp, ct);
                using (upstreamConn)
                {
                    var upStream = upstreamConn.GetStream();
                    if (clientTrailing is { Length: > 0 })
                    {
                        await upStream.WriteAsync(clientTrailing, ct);
                    }
                    if (serverTrailing is { Length: > 0 })
                    {
                        await stream.WriteAsync(serverTrailing, ct);
                    }
                    await RelayBidirectionalAsync(stream, upStream, ct);
                }
            }
            catch
            {
                upstreamConn?.Dispose();
                try
                {
                    await SendSocks5ReplyAsync(stream, 0x05, null, ct);
                }
                catch
                {
                }
            }
        }
        else if (cmd == 0x03) // UDP ASSOCIATE
        {
            reader.Dispose();
            await HandleSocks5UdpAssociateAsync(stream, ct);
        }
        else
        {
            await SendSocks5ReplyAsync(stream, 0x07, null, ct);
        }
    }

    private async Task HandleSocks5UdpAssociateAsync(NetworkStream tcpStream, CancellationToken ct)
    {
        using var localUdp = new UdpClient(new IPEndPoint(IPAddress.Loopback, 0));
        var boundPort = ((IPEndPoint)localUdp.Client.LocalEndPoint!).Port;
        await SendSocks5ReplyAsync(tcpStream, 0x00, new IPEndPoint(IPAddress.Loopback, boundPort), ct);

        using var assocCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        var act = assocCts.Token;

        // Monitor TCP control socket; RFC 1928 closes UDP association when TCP closes.
        var tcpMonitor = Task.Run(async () =>
        {
            var buf = new byte[64];
            try
            {
                while (!act.IsCancellationRequested)
                {
                    int read = await tcpStream.ReadAsync(buf, act);
                    if (read <= 0) break;
                }
            }
            catch
            {
            }
            finally
            {
                assocCts.Cancel();
            }
        }, CancellationToken.None);

        IPEndPoint? clientUdpEp = null;
        var directSockets = new ConcurrentDictionary<AddressFamily, UdpClient>();
        var socksRelays = new ConcurrentDictionary<string, Socks5UdpRelay>(StringComparer.OrdinalIgnoreCase);

        try
        {
            while (!act.IsCancellationRequested)
            {
                var recv = await localUdp.ReceiveAsync(act);
                clientUdpEp ??= recv.RemoteEndPoint;
                if (!recv.RemoteEndPoint.Equals(clientUdpEp))
                    continue;

                if (!TryParseSocks5UdpDatagram(recv.Buffer, out var dstHost, out var dstPort, out var payload))
                    continue;

                var route = await ResolveRouteAsync(dstHost, dstPort, preferUdp: true, preferHttp: false, act);
                if (route.Kind == UpstreamKind.Direct)
                {
                    var destIp = await ResolveUdpDestinationAsync(dstHost, act);
                    if (destIp is null) continue;
                    var destEp = new IPEndPoint(destIp, dstPort);
                    var sock = directSockets.GetOrAdd(destIp.AddressFamily, af =>
                    {
                        var u = new UdpClient(af);
                        _ = Task.Run(async () =>
                        {
                            try
                            {
                                while (!act.IsCancellationRequested)
                                {
                                    var r = await u.ReceiveAsync(act);
                                    if (clientUdpEp is not null)
                                    {
                                        var pkt = BuildSocks5UdpDatagram(r.RemoteEndPoint.Address.ToString(), r.RemoteEndPoint.Port, r.Buffer);
                                        await localUdp.SendAsync(pkt, clientUdpEp, act);
                                    }
                                }
                            }
                            catch
                            {
                            }
                        }, CancellationToken.None);
                        return u;
                    });
                    await sock.SendAsync(payload, destEp, act);
                }
                else if (route.Kind == UpstreamKind.Socks5)
                {
                    var key = $"{route.Host}:{route.Port}";
                    var relay = socksRelays.GetOrAdd(key, _ => new Socks5UdpRelay(route.Host, route.Port, async (fromHost, fromPort, data) =>
                    {
                        if (clientUdpEp is not null)
                        {
                            var pkt = BuildSocks5UdpDatagram(fromHost, fromPort, data.Span);
                            await localUdp.SendAsync(pkt, clientUdpEp, act);
                        }
                    }, act));
                    await relay.SendAsync(dstHost, dstPort, payload, act);
                }
            }
        }
        catch (OperationCanceledException)
        {
        }
        catch
        {
        }
        finally
        {
            assocCts.Cancel();
            foreach (var s in directSockets.Values) s.Dispose();
            foreach (var r in socksRelays.Values) r.Dispose();
            try { await tcpMonitor; } catch { }
        }
    }

    private async Task<IPAddress?> ResolveUdpDestinationAsync(string host, CancellationToken ct)
    {
        if (IPAddress.TryParse(host, out var literal))
        {
            return literal;
        }

        var now = DateTime.UtcNow;
        if (_dnsCache.TryGetValue(host, out var cached)
            && (now - cached.CachedUtc).TotalSeconds < CacheTtlSeconds)
        {
            return cached.Address;
        }

        var ips = await Dns.GetHostAddressesAsync(host, ct);
        if (ips.Length == 0)
        {
            return null;
        }

        var chosen = Array.Find(ips, ip => ip.AddressFamily == AddressFamily.InterNetwork) ?? ips[0];
        if (_dnsCache.Count >= MaxDnsCacheEntries)
        {
            _dnsCache.Clear();
        }
        _dnsCache[host] = (chosen, DateTime.UtcNow);
        return chosen;
    }

    private async Task HandleHttpClientAsync(NetworkStream clientStream, BufferedStreamReader reader, CancellationToken ct)
    {
        int headerLen;
        int searchFrom = 0;
        while (true)
        {
            headerLen = reader.TryFindHeaderEnd(65536, ref searchFrom, out int checkLen);
            if (headerLen >= 0)
            {
                break;
            }
            if (checkLen >= 65536)
            {
                return;
            }
            if (!await reader.FillAsync(ct))
            {
                return;
            }
        }

        byte[] headerBytes = reader.TakeHeaderBytes(headerLen);
        byte[]? clientTrailing = reader.DetachRemaining();
        reader.Dispose();

        var headerText = Encoding.ASCII.GetString(headerBytes);
        var lines = headerText.Split("\r\n", StringSplitOptions.None);
        if (lines.Length == 0) return;
        var reqParts = lines[0].Split(' ', 3, StringSplitOptions.RemoveEmptyEntries);
        if (reqParts.Length < 3) return;

        string method = reqParts[0];
        string target = reqParts[1];
        string version = reqParts[2];

        if (string.Equals(method, "CONNECT", StringComparison.OrdinalIgnoreCase))
        {
            if (!TrySplitHostPort(target, 443, out var host, out var port))
                return;

            var route = await ResolveRouteAsync(host, port, preferUdp: false, preferHttp: true, ct);
            if (route.Kind == UpstreamKind.Http)
            {
                using var upstream = new TcpClient { NoDelay = true };
                await upstream.ConnectAsync(route.Host, route.Port, ct);
                var upStream = upstream.GetStream();
                await upStream.WriteAsync(headerBytes, ct);
                if (clientTrailing is { Length: > 0 })
                {
                    await upStream.WriteAsync(clientTrailing, ct);
                }
                await RelayBidirectionalAsync(clientStream, upStream, ct);
                return;
            }

            var (conn, serverTrailing) = await OpenTcpTargetWithRouteAsync(route, host, port, ct);
            using (conn)
            {
                var okResp = Encoding.ASCII.GetBytes($"{version} 200 Connection Established\r\n\r\n");
                await clientStream.WriteAsync(okResp, ct);
                var connStream = conn.GetStream();
                if (clientTrailing is { Length: > 0 })
                {
                    await connStream.WriteAsync(clientTrailing, ct);
                }
                if (serverTrailing is { Length: > 0 })
                {
                    await clientStream.WriteAsync(serverTrailing, ct);
                }
                await RelayBidirectionalAsync(clientStream, connStream, ct);
            }
            return;
        }

        if (!Uri.TryCreate(target, UriKind.Absolute, out var uri))
        {
            return;
        }

        string httpHost = uri.Host;
        int httpPort = uri.IsDefaultPort ? (uri.Scheme.Equals("https", StringComparison.OrdinalIgnoreCase) ? 443 : 80) : uri.Port;
        var httpRoute = await ResolveRouteAsync(httpHost, httpPort, preferUdp: false, preferHttp: true, ct);

        if (httpRoute.Kind == UpstreamKind.Http)
        {
            using var upstream = new TcpClient { NoDelay = true };
            await upstream.ConnectAsync(httpRoute.Host, httpRoute.Port, ct);
            var upStream = upstream.GetStream();
            await upStream.WriteAsync(headerBytes, ct);
            if (clientTrailing is { Length: > 0 })
            {
                await upStream.WriteAsync(clientTrailing, ct);
            }
            await RelayBidirectionalAsync(clientStream, upStream, ct);
            return;
        }

        var (targetConn, targetServerTrailing) = await OpenTcpTargetWithRouteAsync(httpRoute, httpHost, httpPort, ct);
        using (targetConn)
        {
            var rewritten = new StringBuilder();
            rewritten.Append(method).Append(' ').Append(uri.PathAndQuery).Append(' ').Append(version).Append("\r\n");
            for (int i = 1; i < lines.Length; i++)
            {
                var line = lines[i];
                if (line.Length == 0) break;
                if (line.StartsWith("Proxy-Connection:", StringComparison.OrdinalIgnoreCase) ||
                    line.StartsWith("Connection:", StringComparison.OrdinalIgnoreCase))
                    continue;
                rewritten.Append(line).Append("\r\n");
            }
            rewritten.Append("Connection: close\r\n");
            rewritten.Append("\r\n");
            var outStream = targetConn.GetStream();
            await outStream.WriteAsync(Encoding.ASCII.GetBytes(rewritten.ToString()), ct);
            if (clientTrailing is { Length: > 0 })
            {
                await outStream.WriteAsync(clientTrailing, ct);
            }
            if (targetServerTrailing is { Length: > 0 })
            {
                await clientStream.WriteAsync(targetServerTrailing, ct);
            }
            await RelayBidirectionalAsync(clientStream, outStream, ct);
        }
    }

    private async Task<(TcpClient Client, byte[]? TrailingBytes)> OpenTcpTargetAsync(string host, int port, CancellationToken ct)
    {
        var route = await ResolveRouteAsync(host, port, preferUdp: false, preferHttp: false, ct);
        return await OpenTcpTargetWithRouteAsync(route, host, port, ct);
    }

    private async Task<(TcpClient Client, byte[]? TrailingBytes)> OpenTcpTargetWithRouteAsync(
        UpstreamTarget route,
        string host,
        int port,
        CancellationToken ct)
    {
        if (route.Kind == UpstreamKind.Direct)
        {
            var client = new TcpClient { NoDelay = true };
            try
            {
                await client.ConnectAsync(host, port, ct);
                return (client, null);
            }
            catch
            {
                client.Dispose();
                throw;
            }
        }

        if (route.Kind == UpstreamKind.Socks5)
        {
            var client = new TcpClient { NoDelay = true };
            try
            {
                await client.ConnectAsync(route.Host, route.Port, ct);
                var s = client.GetStream();
                byte[]? trailing = await Socks5ConnectAsync(s, host, port, ct);
                return (client, trailing);
            }
            catch
            {
                _probeCache.TryRemove($"{route.Host}:{route.Port}", out _);
                client.Dispose();
                throw;
            }
        }

        // HTTP CONNECT upstream
        var httpClient = new TcpClient { NoDelay = true };
        try
        {
            await httpClient.ConnectAsync(route.Host, route.Port, ct);
            var s = httpClient.GetStream();
            var hostHeader = host.Contains(':') ? $"[{host}]:{port}" : $"{host}:{port}";
            var req = Encoding.ASCII.GetBytes($"CONNECT {hostHeader} HTTP/1.1\r\nHost: {hostHeader}\r\nProxy-Connection: Keep-Alive\r\n\r\n");
            await s.WriteAsync(req, ct);
            var (resp, trailing) = await ReadHttpResponseHeaderAsync(s, ct);
            if (!resp.StartsWith("HTTP/", StringComparison.OrdinalIgnoreCase) ||
                (!resp.Contains(" 200 ", StringComparison.Ordinal) && !resp.Contains(" 200\r\n", StringComparison.Ordinal)))
            {
                throw new IOException($"HTTP proxy CONNECT failed: {resp.Split('\r', '\n')[0]}");
            }
            return (httpClient, trailing);
        }
        catch
        {
            httpClient.Dispose();
            throw;
        }
    }

    private async Task<UpstreamTarget> ResolveRouteAsync(
        string dstHost,
        int dstPort,
        bool preferUdp,
        bool preferHttp,
        CancellationToken ct)
    {
        var state = CurrentState;
        if (!state.Enabled)
        {
            return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
        }

        if (IsBypassed(dstHost, state.ProxyOverrideRaw))
        {
            return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
        }

        if (state.DefaultTcpUpstream is null && !string.IsNullOrWhiteSpace(state.AutoConfigUrl))
        {
            var pacTarget = await ResolvePacProxyAsync(state.AutoConfigUrl, dstHost, dstPort, ct);
            if (pacTarget is null)
            {
                return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
            }
            if (preferHttp && pacTarget.Value.Kind == UpstreamKind.Http)
            {
                return pacTarget.Value;
            }
            return await FinalizeUpstreamKindAsync(pacTarget.Value, ct);
        }

        if (preferUdp && state.ExplicitSocksUpstream is { } socksUp)
        {
            return socksUp;
        }

        if (state.DefaultTcpUpstream is { } tcpUp)
        {
            if (preferHttp && tcpUp.Kind == UpstreamKind.Http)
            {
                return tcpUp;
            }
            return await FinalizeUpstreamKindAsync(tcpUp, ct);
        }

        return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
    }

    private bool? TryGetCachedSocks5Support(string host, int port) =>
        TryGetCachedSocks5Support($"{host}:{port}", out bool supported) ? supported : null;

    private bool TryGetCachedSocks5Support(string key, out bool supportsSocks5)
    {
        if (_probeCache.TryGetValue(key, out var cached))
        {
            int ttl = cached.SupportsSocks5 ? Socks5ProbeSuccessTtlSeconds : Socks5ProbeFailureTtlSeconds;
            if ((DateTime.UtcNow - cached.CheckedUtc).TotalSeconds < ttl)
            {
                supportsSocks5 = cached.SupportsSocks5;
                return true;
            }
        }
        supportsSocks5 = false;
        return false;
    }

    private async Task<bool> GetSocks5SupportAsync(string host, int port, CancellationToken ct)
    {
        var key = $"{host}:{port}";
        if (TryGetCachedSocks5Support(key, out bool cached))
        {
            return cached;
        }

        long epoch = Volatile.Read(ref _stateEpoch);
        var lazy = _inflightProbes.GetOrAdd(key, _ => new Lazy<Task<bool>>(
            () => ProbeAndCacheSocks5Async(key, host, port, epoch, _cts.Token),
            LazyThreadSafetyMode.ExecutionAndPublication));

        try
        {
            return await lazy.Value.WaitAsync(ct);
        }
        finally
        {
            if (lazy.IsValueCreated && lazy.Value.IsCompleted)
            {
                _inflightProbes.TryRemove(new KeyValuePair<string, Lazy<Task<bool>>>(key, lazy));
            }
        }
    }

    private async Task<bool> ProbeAndCacheSocks5Async(string key, string host, int port, long epoch, CancellationToken ct)
    {
        bool supportsSocks5 = await ProbeSocks5SupportAsync(host, port, ct);
        if (Volatile.Read(ref _stateEpoch) == epoch)
        {
            _probeCache[key] = (supportsSocks5, DateTime.UtcNow);
        }
        return supportsSocks5;
    }

    private async Task<UpstreamTarget> FinalizeUpstreamKindAsync(UpstreamTarget candidate, CancellationToken ct)
    {
        if (candidate.Kind == UpstreamKind.Socks5)
        {
            return candidate;
        }

        bool supportsSocks5 = await GetSocks5SupportAsync(candidate.Host, candidate.Port, ct);
        return candidate with { Kind = supportsSocks5 ? UpstreamKind.Socks5 : UpstreamKind.Http };
    }

    private static bool IsLoopbackHost(string host) =>
        host.Equals("localhost", StringComparison.OrdinalIgnoreCase)
        || (IPAddress.TryParse(host, out var ip) && IPAddress.IsLoopback(ip));

    private static async Task<bool> ProbeSocks5SupportAsync(string host, int port, CancellationToken ct)
    {
        try
        {
            int timeoutMs = IsLoopbackHost(host) ? 150 : 500;
            using var probeCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            probeCts.CancelAfter(timeoutMs);
            using var client = new TcpClient { NoDelay = true };
            await client.ConnectAsync(host, port, probeCts.Token);
            var s = client.GetStream();
            await s.WriteAsync(new byte[] { 0x05, 0x01, 0x00 }, probeCts.Token);
            var reply = new byte[2];
            if (!await ReadExactAsync(s, reply, probeCts.Token))
            {
                return false;
            }
            return reply[0] == 0x05 && reply[1] == 0x00;
        }
        catch
        {
            return false;
        }
    }

    private static async Task<byte[]?> Socks5ConnectAsync(NetworkStream s, string host, int port, CancellationToken ct)
    {
        await s.WriteAsync(new byte[] { 0x05, 0x01, 0x00 }, ct);
        using var reader = new BufferedStreamReader(s, 512);
        var greeting = new byte[2];
        if (!await reader.ReadExactAsync(greeting, ct) || greeting[0] != 0x05 || greeting[1] != 0x00)
        {
            throw new IOException("Upstream SOCKS5 auth handshake failed.");
        }

        var req = BuildSocks5AddressCommand(0x01, host, port);
        await s.WriteAsync(req, ct);

        var respHead = new byte[4];
        if (!await reader.ReadExactAsync(respHead, ct) || respHead[0] != 0x05 || respHead[1] != 0x00)
        {
            throw new IOException($"Upstream SOCKS5 CONNECT failed (rep={respHead[1]}).");
        }
        _ = await ReadSocks5AddressAsync(reader, respHead[3], ct);
        return reader.DetachRemaining();
    }

    private static byte[] BuildSocks5AddressCommand(byte cmd, string host, int port)
    {
        var list = new List<byte>(64) { 0x05, cmd, 0x00 };
        AppendSocks5Address(list, host, port);
        return list.ToArray();
    }

    private static void AppendSocks5Address(List<byte> buf, string host, int port)
    {
        if (IPAddress.TryParse(host, out var ip))
        {
            var bytes = ip.GetAddressBytes();
            buf.Add(bytes.Length == 4 ? (byte)0x01 : (byte)0x04);
            buf.AddRange(bytes);
        }
        else
        {
            var domainBytes = Encoding.UTF8.GetBytes(host);
            if (domainBytes.Length > 255)
            {
                domainBytes = domainBytes[..255];
            }
            buf.Add(0x03);
            buf.Add((byte)domainBytes.Length);
            buf.AddRange(domainBytes);
        }
        buf.Add((byte)(port >> 8));
        buf.Add((byte)(port & 0xFF));
    }

    private static async Task<(string Host, int Port)?> ReadSocks5AddressAsync(BufferedStreamReader reader, byte atyp, CancellationToken ct)
    {
        string host;
        switch (atyp)
        {
            case 0x01:
            {
                var ip4 = new byte[4];
                if (!await reader.ReadExactAsync(ip4, ct)) return null;
                host = new IPAddress(ip4).ToString();
                break;
            }
            case 0x03:
            {
                int domLen = await reader.ReadByteAsync(ct);
                if (domLen < 0) return null;
                var dom = new byte[domLen];
                if (!await reader.ReadExactAsync(dom, ct)) return null;
                host = Encoding.UTF8.GetString(dom);
                break;
            }
            case 0x04:
            {
                var ip6 = new byte[16];
                if (!await reader.ReadExactAsync(ip6, ct)) return null;
                host = new IPAddress(ip6).ToString();
                break;
            }
            default:
                return null;
        }

        var portBuf = new byte[2];
        if (!await reader.ReadExactAsync(portBuf, ct)) return null;
        int port = BinaryPrimitives.ReadUInt16BigEndian(portBuf);
        return (host, port);
    }

    private static async Task SendSocks5ReplyAsync(Stream s, byte rep, IPEndPoint? bindEp, CancellationToken ct)
    {
        var ipBytes = bindEp?.Address.GetAddressBytes() ?? new byte[] { 0, 0, 0, 0 };
        byte atyp = ipBytes.Length == 16 ? (byte)0x04 : (byte)0x01;
        int port = bindEp?.Port ?? 0;
        var reply = new byte[4 + ipBytes.Length + 2];
        reply[0] = 0x05;
        reply[1] = rep;
        reply[2] = 0x00;
        reply[3] = atyp;
        ipBytes.CopyTo(reply, 4);
        BinaryPrimitives.WriteUInt16BigEndian(reply.AsSpan(4 + ipBytes.Length, 2), (ushort)port);
        await s.WriteAsync(reply, ct);
    }

    private static bool TryParseSocks5UdpDatagram(byte[] buf, out string host, out int port, out ReadOnlyMemory<byte> payload)
    {
        host = "";
        port = 0;
        payload = default;
        if (buf.Length < 4 || buf[2] != 0x00)
            return false;

        byte atyp = buf[3];
        int pos = 4;
        switch (atyp)
        {
            case 0x01:
                if (buf.Length < pos + 4 + 2) return false;
                host = new IPAddress(buf.AsSpan(pos, 4)).ToString();
                pos += 4;
                break;
            case 0x03:
                if (buf.Length < pos + 1) return false;
                int len = buf[pos++];
                if (buf.Length < pos + len + 2) return false;
                host = Encoding.UTF8.GetString(buf, pos, len);
                pos += len;
                break;
            case 0x04:
                if (buf.Length < pos + 16 + 2) return false;
                host = new IPAddress(buf.AsSpan(pos, 16)).ToString();
                pos += 16;
                break;
            default:
                return false;
        }

        port = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos, 2));
        pos += 2;
        payload = buf.AsMemory(pos);
        return true;
    }

    private static byte[] BuildSocks5UdpDatagram(string host, int port, ReadOnlySpan<byte> payload)
    {
        if (IPAddress.TryParse(host, out var ip))
        {
            Span<byte> ipBytes = stackalloc byte[16];
            ip.TryWriteBytes(ipBytes, out int ipLen);
            byte atyp = ipLen == 4 ? (byte)0x01 : (byte)0x04;
            int headerLen = 4 + ipLen + 2;
            var result = new byte[headerLen + payload.Length];
            result[3] = atyp;
            ipBytes[..ipLen].CopyTo(result.AsSpan(4));
            BinaryPrimitives.WriteUInt16BigEndian(result.AsSpan(4 + ipLen, 2), (ushort)port);
            payload.CopyTo(result.AsSpan(headerLen));
            return result;
        }
        else
        {
            int byteCount = Encoding.UTF8.GetByteCount(host);
            int domainLen = Math.Min(byteCount, 255);
            int headerLen = 5 + domainLen + 2;
            var result = new byte[headerLen + payload.Length];
            result[3] = 0x03;
            result[4] = (byte)domainLen;
            if (byteCount <= 255)
            {
                Encoding.UTF8.GetBytes(host.AsSpan(), result.AsSpan(5, domainLen));
            }
            else
            {
                var full = Encoding.UTF8.GetBytes(host);
                full.AsSpan(0, 255).CopyTo(result.AsSpan(5, 255));
            }
            BinaryPrimitives.WriteUInt16BigEndian(result.AsSpan(5 + domainLen, 2), (ushort)port);
            payload.CopyTo(result.AsSpan(headerLen));
            return result;
        }
    }

    private static async Task<(string Header, byte[]? TrailingBytes)> ReadHttpResponseHeaderAsync(Stream s, CancellationToken ct)
    {
        using var reader = new BufferedStreamReader(s, 4096);
        int headerLen = -1;
        int searchFrom = 0;
        while (true)
        {
            headerLen = reader.TryFindHeaderEnd(16384, ref searchFrom, out int checkLen);
            if (headerLen >= 0)
            {
                break;
            }
            if (checkLen >= 16384)
            {
                return (reader.ReadAllAsciiString(16384), null);
            }
            if (!await reader.FillAsync(ct))
            {
                break;
            }
        }

        if (headerLen < 0)
        {
            return (reader.ReadAllAsciiString(16384), null);
        }

        string header = reader.TakeHeaderAsciiString(headerLen);
        return (header, reader.DetachRemaining());
    }

    private static async Task RelayBidirectionalAsync(Stream a, Stream b, CancellationToken ct)
    {
        using var relayCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        var t1 = CopyStreamAsync(a, b, relayCts);
        var t2 = CopyStreamAsync(b, a, relayCts);
        await Task.WhenAll(t1, t2);
    }

    private static async Task CopyStreamAsync(Stream src, Stream dst, CancellationTokenSource cts)
    {
        byte[] buffer = ArrayPool<byte>.Shared.Rent(RelayBufferSize);
        try
        {
            while (!cts.IsCancellationRequested)
            {
                int read = await src.ReadAsync(buffer, cts.Token);
                if (read <= 0) break;
                await dst.WriteAsync(buffer.AsMemory(0, read), cts.Token);
            }
        }
        catch
        {
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(buffer);
            try
            {
                if (dst is NetworkStream ns)
                {
                    ns.Socket.Shutdown(SocketShutdown.Send);
                }
            }
            catch
            {
            }
        }
    }

    private static async Task<bool> ReadExactAsync(Stream s, byte[] buf, CancellationToken ct)
    {
        int offset = 0;
        while (offset < buf.Length)
        {
            int n = await s.ReadAsync(buf.AsMemory(offset, buf.Length - offset), ct);
            if (n <= 0) return false;
            offset += n;
        }
        return true;
    }

    private sealed class BufferedStreamReader : IDisposable
    {
        private readonly Stream _stream;
        private byte[]? _buffer;
        private int _pos;
        private int _len;

        public BufferedStreamReader(Stream stream, int initialCapacity = 4096)
        {
            _stream = stream;
            _buffer = ArrayPool<byte>.Shared.Rent(initialCapacity);
        }

        private ReadOnlySpan<byte> UnreadSpan =>
            _buffer is null ? ReadOnlySpan<byte>.Empty : _buffer.AsSpan(_pos, _len - _pos);

        public int TryFindHeaderEnd(int maxBytes, ref int searchFrom, out int checkedLen)
        {
            var span = UnreadSpan;
            checkedLen = Math.Min(span.Length, maxBytes);
            int idx = span[searchFrom..checkedLen].IndexOf("\r\n\r\n"u8);
            if (idx >= 0)
            {
                return searchFrom + idx + 4;
            }
            searchFrom = Math.Max(0, checkedLen - 3);
            return -1;
        }

        public byte[] TakeHeaderBytes(int headerLen)
        {
            byte[] headerBytes = UnreadSpan[..headerLen].ToArray();
            _pos += headerLen;
            return headerBytes;
        }

        public string TakeHeaderAsciiString(int headerLen)
        {
            string header = Encoding.ASCII.GetString(UnreadSpan[..headerLen]);
            _pos += headerLen;
            return header;
        }

        public string ReadAllAsciiString(int maxBytes)
        {
            var span = UnreadSpan;
            int len = Math.Min(span.Length, maxBytes);
            return Encoding.ASCII.GetString(span[..len]);
        }

        public async ValueTask<bool> EnsureAvailableAsync(int minBytes, CancellationToken ct)
        {
            while (_len - _pos < minBytes)
            {
                if (!await FillAsync(ct))
                {
                    return false;
                }
            }
            return true;
        }

        public async ValueTask<bool> FillAsync(CancellationToken ct)
        {
            if (_buffer is null) return false;
            if (_pos > 0 && _pos < _len)
            {
                Buffer.BlockCopy(_buffer, _pos, _buffer, 0, _len - _pos);
                _len -= _pos;
                _pos = 0;
            }
            else if (_pos == _len)
            {
                _pos = 0;
                _len = 0;
            }

            if (_len == _buffer.Length)
            {
                var grown = ArrayPool<byte>.Shared.Rent(_buffer.Length * 2);
                Buffer.BlockCopy(_buffer, 0, grown, 0, _len);
                ArrayPool<byte>.Shared.Return(_buffer);
                _buffer = grown;
            }

            int n = await _stream.ReadAsync(_buffer.AsMemory(_len, _buffer.Length - _len), ct);
            if (n <= 0) return false;
            _len += n;
            return true;
        }

        public byte PeekByte() => _buffer![_pos];

        public async ValueTask<int> ReadByteAsync(CancellationToken ct)
        {
            if (!await EnsureAvailableAsync(1, ct)) return -1;
            return _buffer![_pos++];
        }

        public async ValueTask<bool> ReadExactAsync(byte[] dst, CancellationToken ct)
        {
            int offset = 0;
            int count = dst.Length;
            while (count > 0)
            {
                if (_len - _pos == 0)
                {
                    if (!await FillAsync(ct)) return false;
                }
                int take = Math.Min(count, _len - _pos);
                Buffer.BlockCopy(_buffer!, _pos, dst, offset, take);
                _pos += take;
                offset += take;
                count -= take;
            }
            return true;
        }

        public void Consume(int count)
        {
            _pos += count;
        }

        public byte[]? DetachRemaining()
        {
            int rem = _len - _pos;
            if (rem <= 0 || _buffer is null) return null;
            var copy = _buffer.AsSpan(_pos, rem).ToArray();
            _pos = _len;
            return copy;
        }

        public void Dispose()
        {
            var buf = _buffer;
            _buffer = null;
            _pos = 0;
            _len = 0;
            if (buf is not null)
            {
                ArrayPool<byte>.Shared.Return(buf);
            }
        }
    }

    private sealed class Socks5UdpRelay : IDisposable
    {
        private readonly string _proxyHost;
        private readonly int _proxyPort;
        private readonly Func<string, int, ReadOnlyMemory<byte>, Task> _onResponse;
        private readonly CancellationToken _ct;
        private readonly SemaphoreSlim _initLock = new(1, 1);
        private TcpClient? _ctrlTcp;
        private UdpClient? _udp;
        private IPEndPoint? _relayEp;

        public Socks5UdpRelay(string proxyHost, int proxyPort, Func<string, int, ReadOnlyMemory<byte>, Task> onResponse, CancellationToken ct)
        {
            _proxyHost = proxyHost;
            _proxyPort = proxyPort;
            _onResponse = onResponse;
            _ct = ct;
        }

        public async Task SendAsync(string dstHost, int dstPort, ReadOnlyMemory<byte> payload, CancellationToken ct)
        {
            await EnsureInitializedAsync(ct);
            if (_udp is null || _relayEp is null) return;
            var pkt = BuildSocks5UdpDatagram(dstHost, dstPort, payload.Span);
            await _udp.SendAsync(pkt, _relayEp, ct);
        }

        private async Task EnsureInitializedAsync(CancellationToken ct)
        {
            if (_relayEp is not null && _ctrlTcp is { Connected: true })
                return;

            await _initLock.WaitAsync(ct);
            try
            {
                if (_relayEp is not null && _ctrlTcp is { Connected: true })
                    return;

                _ctrlTcp?.Dispose();
                _udp?.Dispose();

                var tcp = new TcpClient { NoDelay = true };
                await tcp.ConnectAsync(_proxyHost, _proxyPort, ct);
                var s = tcp.GetStream();
                await s.WriteAsync(new byte[] { 0x05, 0x01, 0x00 }, ct);
                using var reader = new BufferedStreamReader(s, 512);
                var greeting = new byte[2];
                if (!await reader.ReadExactAsync(greeting, ct) || greeting[0] != 0x05 || greeting[1] != 0x00)
                {
                    tcp.Dispose();
                    throw new IOException("Upstream SOCKS5 UDP auth failed");
                }
                await s.WriteAsync(new byte[] { 0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0 }, ct);
                var head = new byte[4];
                if (!await reader.ReadExactAsync(head, ct) || head[0] != 0x05 || head[1] != 0x00)
                {
                    tcp.Dispose();
                    throw new IOException("Upstream SOCKS5 UDP ASSOCIATE failed");
                }
                var relayAddr = await ReadSocks5AddressAsync(reader, head[3], ct);
                if (relayAddr is null)
                {
                    tcp.Dispose();
                    throw new IOException("Invalid SOCKS5 UDP relay address");
                }

                var relayHost = relayAddr.Value.Host;
                if (relayHost is "0.0.0.0" or "::")
                {
                    relayHost = _proxyHost;
                }
                var ips = await Dns.GetHostAddressesAsync(relayHost, ct);
                var relayIp = Array.Find(ips, ip => ip.AddressFamily == AddressFamily.InterNetwork) ?? ips[0];
                var relayEp = new IPEndPoint(relayIp, relayAddr.Value.Port);
                var udp = new UdpClient(relayEp.AddressFamily);

                _ctrlTcp = tcp;
                _udp = udp;
                _relayEp = relayEp;

                _ = Task.Run(async () =>
                {
                    try
                    {
                        while (!_ct.IsCancellationRequested)
                        {
                            var r = await udp.ReceiveAsync(_ct);
                            if (TryParseSocks5UdpDatagram(r.Buffer, out var fh, out var fp, out var data))
                            {
                                await _onResponse(fh, fp, data);
                            }
                        }
                    }
                    catch
                    {
                    }
                }, CancellationToken.None);
            }
            finally
            {
                _initLock.Release();
            }
        }

        public void Dispose()
        {
            _ctrlTcp?.Dispose();
            _udp?.Dispose();
            _initLock.Dispose();
        }
    }

    internal static SystemProxyState ReadSystemProxyState()
    {
        using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Internet Settings", writable: false);
        int enable = key?.GetValue("ProxyEnable") as int? ?? 0;
        string server = (key?.GetValue("ProxyServer") as string ?? "").Trim();
        string overrideRaw = (key?.GetValue("ProxyOverride") as string ?? "").Trim();
        string pacUrl = (key?.GetValue("AutoConfigURL") as string ?? "").Trim();

        UpstreamTarget? defaultTcp = null;
        UpstreamTarget? explicitSocks = null;
        string? httpEnv = null;
        string? httpsEnv = null;
        string? allEnv = null;

        if (enable == 1 && !string.IsNullOrWhiteSpace(server))
        {
            ParseProxyServer(server, out defaultTcp, out explicitSocks, out httpEnv, out httpsEnv, out allEnv);
        }

        bool isActive = (enable == 1 && (defaultTcp is not null || explicitSocks is not null))
            || !string.IsNullOrWhiteSpace(pacUrl);

        var noProxy = BuildNoProxyEnv(overrideRaw);
        return new SystemProxyState(
            Enabled: isActive,
            ProxyServerRaw: server,
            ProxyOverrideRaw: overrideRaw,
            AutoConfigUrl: pacUrl,
            DefaultTcpUpstream: defaultTcp,
            ExplicitSocksUpstream: explicitSocks,
            HttpProxyEnv: httpEnv,
            HttpsProxyEnv: httpsEnv,
            AllProxyEnv: allEnv,
            NoProxyEnv: noProxy);
    }

    private static void ParseProxyServer(
        string raw,
        out UpstreamTarget? defaultTcp,
        out UpstreamTarget? explicitSocks,
        out string? httpEnv,
        out string? httpsEnv,
        out string? allEnv)
    {
        defaultTcp = null;
        explicitSocks = null;
        httpEnv = null;
        httpsEnv = null;
        allEnv = null;

        if (!raw.Contains('='))
        {
            if (TryParseEndpoint(raw, UpstreamKind.Http, out var ep, out var scheme))
            {
                defaultTcp = ep;
                if (scheme is "socks" or "socks5" or "socks4")
                {
                    explicitSocks = ep with { Kind = UpstreamKind.Socks5 };
                    defaultTcp = explicitSocks;
                    allEnv = $"socks5h://{FormatHostPort(ep.Host, ep.Port)}";
                }
                else
                {
                    var urlScheme = scheme == "https" ? "https" : "http";
                    httpEnv = $"{urlScheme}://{FormatHostPort(ep.Host, ep.Port)}";
                    httpsEnv = httpEnv;
                }
            }
            return;
        }

        UpstreamTarget? http = null;
        UpstreamTarget? https = null;
        foreach (var part in raw.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            var idx = part.IndexOf('=');
            if (idx <= 0) continue;
            var proto = part[..idx].Trim().ToLowerInvariant();
            var addr = part[(idx + 1)..].Trim();
            if (!TryParseEndpoint(addr, proto.StartsWith("socks") ? UpstreamKind.Socks5 : UpstreamKind.Http, out var ep, out var scheme))
                continue;

            switch (proto)
            {
                case "socks":
                case "socks5":
                    explicitSocks = ep with { Kind = UpstreamKind.Socks5 };
                    allEnv = $"socks5h://{FormatHostPort(ep.Host, ep.Port)}";
                    break;
                case "https":
                {
                    https = ep;
                    var urlScheme = scheme == "https" ? "https" : "http";
                    httpsEnv = $"{urlScheme}://{FormatHostPort(ep.Host, ep.Port)}";
                    break;
                }
                case "http":
                {
                    http = ep;
                    var urlScheme = scheme == "https" ? "https" : "http";
                    httpEnv = $"{urlScheme}://{FormatHostPort(ep.Host, ep.Port)}";
                    break;
                }
            }
        }

        defaultTcp = explicitSocks ?? https ?? http;
    }

    private static bool TryParseEndpoint(string raw, UpstreamKind defaultKind, out UpstreamTarget target, out string scheme)
    {
        target = default;
        scheme = "";
        var s = raw.Trim();
        var schemeIdx = s.IndexOf("://", StringComparison.Ordinal);
        if (schemeIdx > 0)
        {
            scheme = s[..schemeIdx].ToLowerInvariant();
            s = s[(schemeIdx + 3)..];
        }
        int slashIdx = s.IndexOf('/');
        if (slashIdx >= 0) s = s[..slashIdx];

        if (!TrySplitHostPort(s, 80, out var host, out var port))
            return false;

        var kind = scheme.StartsWith("socks") ? UpstreamKind.Socks5 : defaultKind;
        target = new UpstreamTarget(kind, host, port);
        return true;
    }

    private static bool TrySplitHostPort(string value, int defaultPort, out string host, out int port)
    {
        host = "";
        port = defaultPort;
        var s = value.Trim();
        if (string.IsNullOrEmpty(s)) return false;

        if (s.StartsWith('['))
        {
            int end = s.IndexOf(']');
            if (end <= 1) return false;
            host = s[1..end];
            if (end + 1 < s.Length && s[end + 1] == ':')
            {
                return int.TryParse(s[(end + 2)..], out port) && port is > 0 and <= 65535;
            }
            return true;
        }

        int colon = s.LastIndexOf(':');
        if (colon > 0 && s.IndexOf(':') == colon)
        {
            host = s[..colon];
            return int.TryParse(s[(colon + 1)..], out port) && port is > 0 and <= 65535;
        }

        host = s;
        return true;
    }

    private static string FormatHostPort(string host, int port) =>
        host.Contains(':') ? $"[{host}]:{port}" : $"{host}:{port}";

    private static string BuildNoProxyEnv(string overrideRaw)
    {
        var entries = new List<string> { "localhost", "127.0.0.1", "::1" };
        foreach (var raw in overrideRaw.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            if (raw.Equals("<local>", StringComparison.OrdinalIgnoreCase)) continue;
            var item = raw.StartsWith("*.", StringComparison.Ordinal) ? raw[1..] : raw;
            if (!entries.Contains(item, StringComparer.OrdinalIgnoreCase))
            {
                entries.Add(item);
            }
        }
        return string.Join(',', entries);
    }

    private static bool IsBypassed(string host, string overrideRaw)
    {
        if (IPAddress.TryParse(host, out var ip))
        {
            if (IPAddress.IsLoopback(ip))
                return true;
        }
        if (host.Equals("localhost", StringComparison.OrdinalIgnoreCase))
            return true;
        if (string.IsNullOrWhiteSpace(overrideRaw))
            return false;

        foreach (var pattern in overrideRaw.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            if (pattern.Equals("<local>", StringComparison.OrdinalIgnoreCase))
            {
                if (!host.Contains('.') && !IPAddress.TryParse(host, out _))
                    return true;
                continue;
            }

            if (WildcardMatch(pattern, host))
                return true;
        }
        return false;
    }

    private static bool WildcardMatch(string pattern, string input)
    {
        int p = 0, s = 0, star = -1, mark = 0;
        while (s < input.Length)
        {
            if (p < pattern.Length && (pattern[p] == '?' || char.ToLowerInvariant(pattern[p]) == char.ToLowerInvariant(input[s])))
            {
                p++;
                s++;
            }
            else if (p < pattern.Length && pattern[p] == '*')
            {
                star = p++;
                mark = s;
            }
            else if (star != -1)
            {
                p = star + 1;
                s = ++mark;
            }
            else
            {
                return false;
            }
        }
        while (p < pattern.Length && pattern[p] == '*') p++;
        return p == pattern.Length;
    }

    private async Task<UpstreamTarget?> ResolvePacProxyAsync(string pacUrl, string host, int port, CancellationToken ct)
    {
        var key = $"{pacUrl}|{host}:{port}";
        if (_pacCache.TryGetValue(key, out var cached)
            && (DateTime.UtcNow - cached.CachedUtc).TotalSeconds < CacheTtlSeconds)
        {
            return cached.Target;
        }

        long epoch = Volatile.Read(ref _stateEpoch);
        var resolved = await Task.Run(() => ResolvePacProxySync(pacUrl, host, port), ct);
        if (Volatile.Read(ref _stateEpoch) == epoch)
        {
            if (_pacCache.Count >= MaxPacCacheEntries)
            {
                _pacCache.Clear();
            }
            _pacCache[key] = (resolved, DateTime.UtcNow);
        }
        return resolved;
    }

    private IntPtr GetOrCreateWinHttpSession()
    {
        lock (_winHttpLock)
        {
            if (_disposed) return IntPtr.Zero;
            if (_winHttpSession == IntPtr.Zero)
            {
                _winHttpSession = WinHttpOpen("FocusStage/1.0", 1, null, null, 0);
            }
            return _winHttpSession;
        }
    }

    private UpstreamTarget? ResolvePacProxySync(string pacUrl, string host, int port)
    {
        IntPtr session = GetOrCreateWinHttpSession();
        if (session == IntPtr.Zero) return null;

        var opts = new WINHTTP_AUTOPROXY_OPTIONS
        {
            dwFlags = 0x00000002, // WINHTTP_AUTOPROXY_CONFIG_URL
            lpszAutoConfigUrl = pacUrl,
            fAutoLogonIfChallenged = true,
        };
        var url = $"https://{FormatHostPort(host, port)}/";
        if (!WinHttpGetProxyForUrl(session, url, ref opts, out var info))
        {
            return null;
        }
        try
        {
            if (info.dwAccessType != 3 || info.lpszProxy == IntPtr.Zero) // 3 = WINHTTP_ACCESS_TYPE_NAMED_PROXY
                return null;
            var proxyStr = Marshal.PtrToStringUni(info.lpszProxy) ?? "";
            foreach (var entry in proxyStr.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
            {
                var parts = entry.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
                if (parts.Length == 0 || parts[0].Equals("DIRECT", StringComparison.OrdinalIgnoreCase))
                    return null;
                if (parts.Length >= 2)
                {
                    var kind = parts[0].StartsWith("SOCKS", StringComparison.OrdinalIgnoreCase)
                        ? UpstreamKind.Socks5
                        : UpstreamKind.Http;
                    if (TrySplitHostPort(parts[1], 8080, out var ph, out var pp))
                        return new UpstreamTarget(kind, ph, pp);
                }
                else if (TrySplitHostPort(parts[0], 8080, out var ph, out var pp))
                {
                    return new UpstreamTarget(UpstreamKind.Http, ph, pp);
                }
            }
            return null;
        }
        finally
        {
            if (info.lpszProxy != IntPtr.Zero) GlobalFree(info.lpszProxy);
            if (info.lpszProxyBypass != IntPtr.Zero) GlobalFree(info.lpszProxyBypass);
        }
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct WINHTTP_AUTOPROXY_OPTIONS
    {
        public uint dwFlags;
        public uint dwAutoDetectFlags;
        public string? lpszAutoConfigUrl;
        public IntPtr lpvReserved;
        public uint dwReserved;
        [MarshalAs(UnmanagedType.Bool)]
        public bool fAutoLogonIfChallenged;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct WINHTTP_PROXY_INFO
    {
        public uint dwAccessType;
        public IntPtr lpszProxy;
        public IntPtr lpszProxyBypass;
    }

    [DllImport("winhttp.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr WinHttpOpen(string pszAgentW, uint dwAccessType, string? pszProxyW, string? pszProxyBypassW, uint dwFlags);

    [DllImport("winhttp.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool WinHttpGetProxyForUrl(IntPtr hSession, string lpcwszUrl, ref WINHTTP_AUTOPROXY_OPTIONS pAutoProxyOptions, out WINHTTP_PROXY_INFO pProxyInfo);

    [DllImport("winhttp.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool WinHttpCloseHandle(IntPtr hInternet);

    [DllImport("kernel32.dll")]
    private static extern IntPtr GlobalFree(IntPtr hMem);

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _cts.Cancel();
        try { _listener.Stop(); } catch { }
        lock (_winHttpLock)
        {
            if (_winHttpSession != IntPtr.Zero)
            {
                WinHttpCloseHandle(_winHttpSession);
                _winHttpSession = IntPtr.Zero;
            }
        }
        _cts.Dispose();
    }
}
