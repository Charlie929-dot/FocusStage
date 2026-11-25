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
    string? AllProxyEnv,
    string NoProxyEnv)
{
    public string Signature =>
        $"{Enabled}|{ProxyServerRaw}|{ProxyOverrideRaw}|{AutoConfigUrl}";
}

internal sealed class ProxyBridge : IDisposable
{
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();
    private readonly ConcurrentDictionary<string, (bool SupportsSocks5, DateTime CheckedUtc)> _probeCache = new(StringComparer.OrdinalIgnoreCase);
    private readonly Action<ProxyBridge, SystemProxyState> _onStateChanged;
    private readonly object _stateLock = new();
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
        _ = Task.Run(WatchRegistryLoopAsync);
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

        return new
        {
            type = "host_proxy_changed",
            enabled = true,
            bridge_port = Port,
            http_proxy = $"http://127.0.0.1:{Port}",
            https_proxy = $"http://127.0.0.1:{Port}",
            all_proxy = $"socks5://127.0.0.1:{Port}",
            no_proxy = state.NoProxyEnv,
        };
    }

    private async Task WatchRegistryLoopAsync()
    {
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
                        _probeCache.Clear();
                    }
                }
                if (changed)
                {
                    _onStateChanged(this, next);
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
                var first = new byte[1];
                int n = await stream.ReadAsync(first.AsMemory(0, 1), ct);
                if (n <= 0)
                {
                    return;
                }

                if (first[0] == 0x05)
                {
                    await HandleSocks5ClientAsync(client, stream, ct);
                }
                else
                {
                    await HandleHttpClientAsync(stream, first[0], ct);
                }
            }
            catch
            {
                // Normal connection reset / cancellation.
            }
        }
    }

    private async Task HandleSocks5ClientAsync(TcpClient client, NetworkStream stream, CancellationToken ct)
    {
        var lenBuf = new byte[1];
        if (!await ReadExactAsync(stream, lenBuf, ct))
            return;
        int nMethods = lenBuf[0];
        if (nMethods <= 0)
            return;
        var methods = new byte[nMethods];
        if (!await ReadExactAsync(stream, methods, ct))
            return;

        bool supportsNoAuth = Array.IndexOf(methods, (byte)0x00) >= 0;
        if (!supportsNoAuth)
        {
            await stream.WriteAsync(new byte[] { 0x05, 0xFF }, ct);
            return;
        }
        await stream.WriteAsync(new byte[] { 0x05, 0x00 }, ct);

        var header = new byte[4];
        if (!await ReadExactAsync(stream, header, ct))
            return;
        if (header[0] != 0x05)
            return;

        byte cmd = header[1];
        byte atyp = header[3];
        var target = await ReadSocks5AddressAsync(stream, atyp, ct);
        if (target is null)
        {
            await SendSocks5ReplyAsync(stream, 0x08, null, ct);
            return;
        }
        var (dstHost, dstPort) = target.Value;

        if (cmd == 0x01) // CONNECT
        {
            TcpClient? upstreamConn = null;
            try
            {
                upstreamConn = await OpenTcpTargetAsync(dstHost, dstPort, ct);
                var boundEp = upstreamConn.Client.LocalEndPoint as IPEndPoint;
                await SendSocks5ReplyAsync(stream, 0x00, boundEp, ct);
                using (upstreamConn)
                {
                    await RelayBidirectionalAsync(stream, upstreamConn.GetStream(), ct);
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
            await HandleSocks5UdpAssociateAsync(client, stream, ct);
        }
        else
        {
            await SendSocks5ReplyAsync(stream, 0x07, null, ct);
        }
    }

    private async Task HandleSocks5UdpAssociateAsync(TcpClient tcpClient, NetworkStream tcpStream, CancellationToken ct)
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

                var route = await ResolveRouteAsync(dstHost, dstPort, preferUdp: true, act);
                if (route.Kind == UpstreamKind.Direct)
                {
                    var ips = await Dns.GetHostAddressesAsync(dstHost, act);
                    if (ips.Length == 0) continue;
                    var destIp = ips[0];
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
                            var pkt = BuildSocks5UdpDatagram(fromHost, fromPort, data);
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

    private async Task HandleHttpClientAsync(NetworkStream clientStream, byte firstByte, CancellationToken ct)
    {
        var headerBytes = new List<byte>(1024) { firstByte };
        var one = new byte[1];
        int match = 0;
        while (headerBytes.Count < 65536)
        {
            int n = await clientStream.ReadAsync(one.AsMemory(0, 1), ct);
            if (n <= 0) return;
            byte b = one[0];
            headerBytes.Add(b);
            if ((match == 0 || match == 2) && b == '\r') match++;
            else if ((match == 1 || match == 3) && b == '\n') match++;
            else match = b == '\r' ? 1 : 0;
            if (match == 4) break;
        }
        if (match != 4) return;

        var headerText = Encoding.ASCII.GetString(headerBytes.ToArray());
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

            var route = await ResolveRouteAsync(host, port, preferUdp: false, ct);
            if (route.Kind == UpstreamKind.Http)
            {
                using var upstream = new TcpClient { NoDelay = true };
                await upstream.ConnectAsync(route.Host, route.Port, ct);
                var upStream = upstream.GetStream();
                await upStream.WriteAsync(headerBytes.ToArray(), ct);
                await RelayBidirectionalAsync(clientStream, upStream, ct);
                return;
            }

            using var conn = await OpenTcpTargetWithRouteAsync(route, host, port, ct);
            var okResp = Encoding.ASCII.GetBytes($"{version} 200 Connection Established\r\n\r\n");
            await clientStream.WriteAsync(okResp, ct);
            await RelayBidirectionalAsync(clientStream, conn.GetStream(), ct);
            return;
        }

        if (!Uri.TryCreate(target, UriKind.Absolute, out var uri))
        {
            return;
        }

        string httpHost = uri.Host;
        int httpPort = uri.IsDefaultPort ? (uri.Scheme.Equals("https", StringComparison.OrdinalIgnoreCase) ? 443 : 80) : uri.Port;
        var httpRoute = await ResolveRouteAsync(httpHost, httpPort, preferUdp: false, ct);

        if (httpRoute.Kind == UpstreamKind.Http)
        {
            using var upstream = new TcpClient { NoDelay = true };
            await upstream.ConnectAsync(httpRoute.Host, httpRoute.Port, ct);
            var upStream = upstream.GetStream();
            await upStream.WriteAsync(headerBytes.ToArray(), ct);
            await RelayBidirectionalAsync(clientStream, upStream, ct);
            return;
        }

        using var targetConn = await OpenTcpTargetWithRouteAsync(httpRoute, httpHost, httpPort, ct);
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
        await RelayBidirectionalAsync(clientStream, outStream, ct);
    }

    private async Task<TcpClient> OpenTcpTargetAsync(string host, int port, CancellationToken ct)
    {
        var route = await ResolveRouteAsync(host, port, preferUdp: false, ct);
        return await OpenTcpTargetWithRouteAsync(route, host, port, ct);
    }

    private async Task<TcpClient> OpenTcpTargetWithRouteAsync(UpstreamTarget route, string host, int port, CancellationToken ct)
    {
        if (route.Kind == UpstreamKind.Direct)
        {
            var client = new TcpClient { NoDelay = true };
            try
            {
                await client.ConnectAsync(host, port, ct);
                return client;
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
                await Socks5ConnectAsync(s, host, port, ct);
                return client;
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
            var resp = await ReadHttpResponseHeaderAsync(s, ct);
            if (!resp.StartsWith("HTTP/", StringComparison.OrdinalIgnoreCase) ||
                (!resp.Contains(" 200 ", StringComparison.Ordinal) && !resp.Contains(" 200\r\n", StringComparison.Ordinal)))
            {
                throw new IOException($"HTTP proxy CONNECT failed: {resp.Split('\r', '\n')[0]}");
            }
            return httpClient;
        }
        catch
        {
            httpClient.Dispose();
            throw;
        }
    }

    private async Task<UpstreamTarget> ResolveRouteAsync(string dstHost, int dstPort, bool preferUdp, CancellationToken ct)
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
            var pacTarget = ResolvePacProxy(state.AutoConfigUrl, dstHost, dstPort);
            if (pacTarget is null)
            {
                return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
            }
            return await FinalizeUpstreamKindAsync(pacTarget.Value, ct);
        }

        if (preferUdp && state.ExplicitSocksUpstream is { } socksUp)
        {
            return socksUp;
        }

        if (state.DefaultTcpUpstream is { } tcpUp)
        {
            return await FinalizeUpstreamKindAsync(tcpUp, ct);
        }

        return new UpstreamTarget(UpstreamKind.Direct, dstHost, dstPort);
    }

    private async Task<UpstreamTarget> FinalizeUpstreamKindAsync(UpstreamTarget candidate, CancellationToken ct)
    {
        if (candidate.Kind == UpstreamKind.Socks5)
        {
            return candidate;
        }

        var key = $"{candidate.Host}:{candidate.Port}";
        if (_probeCache.TryGetValue(key, out var cached) && (DateTime.UtcNow - cached.CheckedUtc).TotalSeconds < 30)
        {
            return candidate with { Kind = cached.SupportsSocks5 ? UpstreamKind.Socks5 : UpstreamKind.Http };
        }

        bool supportsSocks5 = await ProbeSocks5SupportAsync(candidate.Host, candidate.Port, ct);
        _probeCache[key] = (supportsSocks5, DateTime.UtcNow);
        return candidate with { Kind = supportsSocks5 ? UpstreamKind.Socks5 : UpstreamKind.Http };
    }

    private static async Task<bool> ProbeSocks5SupportAsync(string host, int port, CancellationToken ct)
    {
        try
        {
            using var probeCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            probeCts.CancelAfter(500);
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

    private static async Task Socks5ConnectAsync(NetworkStream s, string host, int port, CancellationToken ct)
    {
        await s.WriteAsync(new byte[] { 0x05, 0x01, 0x00 }, ct);
        var greeting = new byte[2];
        if (!await ReadExactAsync(s, greeting, ct) || greeting[0] != 0x05 || greeting[1] != 0x00)
        {
            throw new IOException("Upstream SOCKS5 auth handshake failed.");
        }

        var req = BuildSocks5AddressCommand(0x01, host, port);
        await s.WriteAsync(req, ct);

        var respHead = new byte[4];
        if (!await ReadExactAsync(s, respHead, ct) || respHead[0] != 0x05 || respHead[1] != 0x00)
        {
            throw new IOException($"Upstream SOCKS5 CONNECT failed (rep={respHead[1]}).");
        }
        _ = await ReadSocks5AddressAsync(s, respHead[3], ct);
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

    private static async Task<(string Host, int Port)?> ReadSocks5AddressAsync(Stream s, byte atyp, CancellationToken ct)
    {
        string host;
        switch (atyp)
        {
            case 0x01:
            {
                var ip4 = new byte[4];
                if (!await ReadExactAsync(s, ip4, ct)) return null;
                host = new IPAddress(ip4).ToString();
                break;
            }
            case 0x03:
            {
                var lenBuf = new byte[1];
                if (!await ReadExactAsync(s, lenBuf, ct)) return null;
                var dom = new byte[lenBuf[0]];
                if (!await ReadExactAsync(s, dom, ct)) return null;
                host = Encoding.UTF8.GetString(dom);
                break;
            }
            case 0x04:
            {
                var ip6 = new byte[16];
                if (!await ReadExactAsync(s, ip6, ct)) return null;
                host = new IPAddress(ip6).ToString();
                break;
            }
            default:
                return null;
        }

        var portBuf = new byte[2];
        if (!await ReadExactAsync(s, portBuf, ct)) return null;
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
        var head = new List<byte>(32 + payload.Length) { 0x00, 0x00, 0x00 };
        AppendSocks5Address(head, host, port);
        var result = new byte[head.Count + payload.Length];
        head.CopyTo(result);
        payload.CopyTo(result.AsSpan(head.Count));
        return result;
    }

    private static async Task<string> ReadHttpResponseHeaderAsync(Stream s, CancellationToken ct)
    {
        var list = new List<byte>(256);
        var one = new byte[1];
        int match = 0;
        while (list.Count < 16384)
        {
            int n = await s.ReadAsync(one.AsMemory(0, 1), ct);
            if (n <= 0) break;
            byte b = one[0];
            list.Add(b);
            if ((match == 0 || match == 2) && b == '\r') match++;
            else if ((match == 1 || match == 3) && b == '\n') match++;
            else match = b == '\r' ? 1 : 0;
            if (match == 4) break;
        }
        return Encoding.ASCII.GetString(list.ToArray());
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
        var buffer = new byte[32768];
        try
        {
            while (!cts.IsCancellationRequested)
            {
                int read = await src.ReadAsync(buffer, cts.Token);
                if (read <= 0) break;
                await dst.WriteAsync(buffer.AsMemory(0, read), cts.Token);
                await dst.FlushAsync(cts.Token);
            }
        }
        catch
        {
        }
        finally
        {
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

    private sealed class Socks5UdpRelay : IDisposable
    {
        private readonly string _proxyHost;
        private readonly int _proxyPort;
        private readonly Func<string, int, byte[], Task> _onResponse;
        private readonly CancellationToken _ct;
        private readonly SemaphoreSlim _initLock = new(1, 1);
        private TcpClient? _ctrlTcp;
        private UdpClient? _udp;
        private IPEndPoint? _relayEp;

        public Socks5UdpRelay(string proxyHost, int proxyPort, Func<string, int, byte[], Task> onResponse, CancellationToken ct)
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
                var greeting = new byte[2];
                if (!await ReadExactAsync(s, greeting, ct) || greeting[0] != 0x05 || greeting[1] != 0x00)
                {
                    tcp.Dispose();
                    throw new IOException("Upstream SOCKS5 UDP auth failed");
                }
                await s.WriteAsync(new byte[] { 0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0 }, ct);
                var head = new byte[4];
                if (!await ReadExactAsync(s, head, ct) || head[0] != 0x05 || head[1] != 0x00)
                {
                    tcp.Dispose();
                    throw new IOException("Upstream SOCKS5 UDP ASSOCIATE failed");
                }
                var relayAddr = await ReadSocks5AddressAsync(s, head[3], ct);
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
                var relayEp = new IPEndPoint(ips[0], relayAddr.Value.Port);
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
                                await _onResponse(fh, fp, data.ToArray());
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
        string? allEnv = null;

        if (enable == 1 && !string.IsNullOrWhiteSpace(server))
        {
            ParseProxyServer(server, out defaultTcp, out explicitSocks, out httpEnv, out allEnv);
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
            AllProxyEnv: allEnv,
            NoProxyEnv: noProxy);
    }

    private static void ParseProxyServer(
        string raw,
        out UpstreamTarget? defaultTcp,
        out UpstreamTarget? explicitSocks,
        out string? httpEnv,
        out string? allEnv)
    {
        defaultTcp = null;
        explicitSocks = null;
        httpEnv = null;
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
                    allEnv = $"socks5://{FormatHostPort(ep.Host, ep.Port)}";
                }
                else
                {
                    httpEnv = $"http://{FormatHostPort(ep.Host, ep.Port)}";
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
            if (!TryParseEndpoint(addr, proto.StartsWith("socks") ? UpstreamKind.Socks5 : UpstreamKind.Http, out var ep, out _))
                continue;

            switch (proto)
            {
                case "socks":
                case "socks5":
                    explicitSocks = ep with { Kind = UpstreamKind.Socks5 };
                    allEnv = $"socks5://{FormatHostPort(ep.Host, ep.Port)}";
                    break;
                case "https":
                    https = ep;
                    break;
                case "http":
                    http = ep;
                    httpEnv = $"http://{FormatHostPort(ep.Host, ep.Port)}";
                    break;
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

    private static UpstreamTarget? ResolvePacProxy(string pacUrl, string host, int port)
    {
        IntPtr session = WinHttpOpen("FocusStage/1.0", 1, null, null, 0);
        if (session == IntPtr.Zero) return null;
        try
        {
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
        finally
        {
            WinHttpCloseHandle(session);
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
        _cts.Dispose();
    }
}
