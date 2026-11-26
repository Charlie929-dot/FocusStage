use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use netstack_smoltcp::StackBuilder;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::dns::VirtualDns;
use crate::socks5::{self, TargetAddr};

pub const TCP_COPY_BUFFER_SIZE: usize = 65536;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub proxy_addr: SocketAddr,
    pub mtu: u16,
    pub tcp_recv_buffer_size: u32,
    pub tcp_send_buffer_size: u32,
    pub max_sessions: usize,
    pub ipv6_fake_dns: bool,
    pub udp_idle_timeout: Duration,
}

pub async fn run_engine<R, W>(
    mut tun_reader: R,
    mut tun_writer: W,
    cfg: EngineConfig,
    shutdown: CancellationToken,
) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (stack, runner, udp_socket, tcp_listener) = StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(true)
        .enable_icmp(true)
        .mtu(cfg.mtu as usize)
        .stack_buffer_size(2048)
        .tcp_buffer_size(1024)
        .udp_buffer_size(1024)
        .tcp_recv_buffer_size(cfg.tcp_recv_buffer_size)
        .tcp_send_buffer_size(cfg.tcp_send_buffer_size)
        .build()?;

    let mut tcp_listener = tcp_listener
        .ok_or_else(|| io::Error::other("netstack-smoltcp did not initialize TCP listener"))?;
    let udp_socket = udp_socket
        .ok_or_else(|| io::Error::other("netstack-smoltcp did not initialize UDP socket"))?;

    let (mut stack_sink, mut stack_stream) = stack.split();
    let virtual_dns = Arc::new(Mutex::new(VirtualDns::new(cfg.ipv6_fake_dns)));
    let session_permits = Arc::new(Semaphore::new(cfg.max_sessions));

    let runner_cancel = shutdown.clone();
    let mut runner_task = tokio::spawn(async move {
        if let Some(runner) = runner {
            tokio::select! {
                _ = runner_cancel.cancelled() => Ok(()),
                res = runner => res,
            }
        } else {
            runner_cancel.cancelled().await;
            Ok(())
        }
    });

    let rx_cancel = shutdown.clone();
    let read_buf_len = cfg.mtu as usize + 64;
    let mut rx_task = tokio::spawn(async move {
        let mut buf = vec![0u8; read_buf_len];
        loop {
            let n = tokio::select! {
                _ = rx_cancel.cancelled() => break Ok(()),
                res = tun_reader.read(&mut buf) => match res {
                    Ok(0) => break Ok(()),
                    Ok(n) => n,
                    Err(e) => break Err(e),
                }
            };
            if let Err(e) = stack_sink.send(buf[..n].to_vec()).await {
                break Err(e);
            }
        }
    });

    let tx_cancel = shutdown.clone();
    let mut tx_task = tokio::spawn(async move {
        loop {
            let pkt = tokio::select! {
                _ = tx_cancel.cancelled() => break Ok(()),
                item = stack_stream.next() => match item {
                    Some(Ok(pkt)) => pkt,
                    Some(Err(e)) => break Err(e),
                    None => break Ok(()),
                }
            };
            if let Err(e) = tun_writer.write_all(&pkt).await {
                break Err(e);
            }
        }
    });

    let tcp_cancel = shutdown.clone();
    let tcp_dns = virtual_dns.clone();
    let tcp_permits = session_permits.clone();
    let proxy_addr = cfg.proxy_addr;
    let mut tcp_task = tokio::spawn(async move {
        loop {
            let next_conn = tokio::select! {
                _ = tcp_cancel.cancelled() => break,
                item = tcp_listener.next() => item,
            };
            let Some((mut tun_stream, _local_addr, dst_addr)) = next_conn else {
                break;
            };
            let Ok(permit) = tcp_permits.clone().try_acquire_owned() else {
                tracing::warn!(%dst_addr, "session limit reached; dropping TCP connection");
                continue;
            };
            let target = {
                let mut dns_guard = tcp_dns.lock().unwrap();
                match dns_guard.resolve_ip(dst_addr.ip(), Instant::now()) {
                    Some(domain) => TargetAddr::Domain(domain, dst_addr.port()),
                    None => TargetAddr::Ip(dst_addr),
                }
            };
            let conn_cancel = tcp_cancel.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let connect_fut = tokio::time::timeout(
                    Duration::from_secs(15),
                    socks5::connect_tcp(proxy_addr, &target),
                );
                let mut upstream = tokio::select! {
                    _ = conn_cancel.cancelled() => return,
                    res = connect_fut => match res {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(err)) => {
                            tracing::debug!(?target, "SOCKS5 TCP connect failed: {err}");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!(?target, "SOCKS5 TCP connect timed out");
                            return;
                        }
                    }
                };

                tokio::select! {
                    _ = conn_cancel.cancelled() => {}
                    res = tokio::io::copy_bidirectional_with_sizes(
                        &mut tun_stream,
                        &mut upstream,
                        TCP_COPY_BUFFER_SIZE,
                        TCP_COPY_BUFFER_SIZE,
                    ) => {
                        if let Err(err) = res {
                            tracing::trace!(?target, "TCP relay ended: {err}");
                        }
                    }
                }
            });
        }
        Ok::<(), io::Error>(())
    });

    let udp_cancel = shutdown.clone();
    let udp_dns = virtual_dns;
    let udp_permits = session_permits;
    let udp_mtu = cfg.mtu;
    let udp_idle_timeout = cfg.udp_idle_timeout;
    let mut udp_task = tokio::spawn(async move {
        let (mut udp_rx, mut udp_tx) = udp_socket.split();
        let (reply_tx, mut reply_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(1024);

        let reply_cancel = udp_cancel.clone();
        let reply_writer = tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = reply_cancel.cancelled() => break,
                    item = reply_rx.recv() => match item {
                        Some(msg) => msg,
                        None => break,
                    }
                };
                if let Err(err) = udp_tx.send(msg).await {
                    tracing::debug!("UDP write to stack failed: {err}");
                    break;
                }
            }
        });

        let sessions: Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<(Vec<u8>, SocketAddr)>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        loop {
            let msg = tokio::select! {
                _ = udp_cancel.cancelled() => break,
                item = udp_rx.next() => match item {
                    Some(msg) => msg,
                    None => break,
                }
            };
            let (payload, src_addr, dst_addr) = msg;

            if dst_addr.port() == 53 {
                let resp = udp_dns.lock().unwrap().handle_query(&payload);
                if let Some(resp) = resp {
                    let _ = reply_tx.try_send((resp, dst_addr, src_addr));
                }
                continue;
            }

            let existing_tx = {
                let guard = sessions.lock().unwrap();
                guard.get(&src_addr).filter(|tx| !tx.is_closed()).cloned()
            };

            let tx = if let Some(tx) = existing_tx {
                tx
            } else {
                let Ok(permit) = udp_permits.clone().try_acquire_owned() else {
                    tracing::warn!(%src_addr, "session limit reached; dropping UDP session");
                    continue;
                };
                let (sess_tx, sess_rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(128);
                sessions.lock().unwrap().insert(src_addr, sess_tx.clone());

                let sessions_map = sessions.clone();
                let sess_dns = udp_dns.clone();
                let sess_reply = reply_tx.clone();
                let sess_cancel = udp_cancel.clone();
                let sess_tx_cmp = sess_tx.clone();

                tokio::spawn(async move {
                    let _permit = permit;
                    run_udp_session(
                        proxy_addr,
                        src_addr,
                        sess_rx,
                        sess_reply,
                        sess_dns,
                        udp_mtu,
                        udp_idle_timeout,
                        sess_cancel,
                    )
                    .await;
                    let mut guard = sessions_map.lock().unwrap();
                    if guard.get(&src_addr).is_some_and(|cur| cur.same_channel(&sess_tx_cmp)) {
                        guard.remove(&src_addr);
                    }
                });

                sess_tx
            };

            let _ = tx.try_send((payload, dst_addr));
        }

        reply_writer.abort();
        Ok::<(), io::Error>(())
    });

    let res = tokio::select! {
        _ = shutdown.cancelled() => Ok(()),
        r = &mut runner_task => flatten_join_res(r),
        r = &mut rx_task => flatten_join_res(r),
        r = &mut tx_task => flatten_join_res(r),
        r = &mut tcp_task => flatten_join_res(r),
        r = &mut udp_task => flatten_join_res(r),
    };

    shutdown.cancel();
    runner_task.abort();
    rx_task.abort();
    tx_task.abort();
    tcp_task.abort();
    udp_task.abort();

    res
}

fn flatten_join_res(res: Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
    match res {
        Ok(inner) => inner,
        Err(join_err) if join_err.is_cancelled() => Ok(()),
        Err(join_err) => Err(io::Error::other(join_err)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_udp_session(
    proxy_addr: SocketAddr,
    src_addr: SocketAddr,
    mut rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
    reply_tx: mpsc::Sender<(Vec<u8>, SocketAddr, SocketAddr)>,
    dns: Arc<Mutex<VirtualDns>>,
    mtu: u16,
    idle_timeout: Duration,
    shutdown: CancellationToken,
) {
    let assoc = tokio::select! {
        _ = shutdown.cancelled() => return,
        res = tokio::time::timeout(Duration::from_secs(10), socks5::udp_associate(proxy_addr)) => {
            match res {
                Ok(Ok(assoc)) => assoc,
                Ok(Err(err)) => {
                    tracing::debug!(%src_addr, "SOCKS5 UDP ASSOCIATE failed: {err}");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%src_addr, "SOCKS5 UDP ASSOCIATE timed out");
                    return;
                }
            }
        }
    };
    let (mut ctrl_tcp, relay_addr) = assoc;

    let bind_addr: SocketAddr = if relay_addr.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    };
    let Ok(udp) = TokioUdpSocket::bind(bind_addr).await else {
        return;
    };
    if udp.connect(relay_addr).await.is_err() {
        return;
    }

    let mut target_to_orig: HashMap<String, SocketAddr> = HashMap::new();
    let mut last_orig_dst: Option<SocketAddr> = None;
    let mut ctrl_buf = [0u8; 64];
    let mut recv_buf = vec![0u8; (mtu as usize).max(4096) + 256];
    let idle_sleep = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_sleep);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = &mut idle_sleep => break,
            ctrl_res = ctrl_tcp.read(&mut ctrl_buf) => {
                match ctrl_res {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            pkt = rx.recv() => {
                let Some((data, orig_dst)) = pkt else {
                    break;
                };
                idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let target = {
                    let mut dns_guard = dns.lock().unwrap();
                    match dns_guard.resolve_ip(orig_dst.ip(), Instant::now()) {
                        Some(domain) => TargetAddr::Domain(domain, orig_dst.port()),
                        None => TargetAddr::Ip(orig_dst),
                    }
                };
                if target_to_orig.len() >= 512 {
                    target_to_orig.clear();
                }
                target_to_orig.insert(target_key(&target), orig_dst);
                last_orig_dst = Some(orig_dst);

                let encoded = socks5::encode_udp_packet(&target, &data);
                if udp.send(&encoded).await.is_err() {
                    break;
                }
            }
            recv_res = udp.recv(&mut recv_buf) => {
                let Ok(n) = recv_res else {
                    break;
                };
                idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let Some((from_target, payload)) = socks5::decode_udp_packet(&recv_buf[..n]) else {
                    continue;
                };
                let remote_addr = target_to_orig
                    .get(&target_key(&from_target))
                    .copied()
                    .or_else(|| match &from_target {
                        TargetAddr::Domain(dom, port) => dns
                            .lock()
                            .unwrap()
                            .lookup_fake_ip_for_name(dom, src_addr.is_ipv6(), Instant::now())
                            .map(|ip| SocketAddr::new(ip, *port)),
                        TargetAddr::Ip(addr) => Some(*addr),
                    })
                    .or(last_orig_dst);

                if let Some(remote) = remote_addr {
                    let _ = reply_tx.try_send((payload.to_vec(), remote, src_addr));
                }
            }
        }
    }
}

fn target_key(target: &TargetAddr) -> String {
    match target {
        TargetAddr::Ip(addr) => addr.to_string(),
        TargetAddr::Domain(dom, port) => format!("{}:{}", dom.to_ascii_lowercase(), port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use netstack_smoltcp::smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
    use netstack_smoltcp::smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
    use netstack_smoltcp::smoltcp::socket::{tcp, udp};
    use netstack_smoltcp::smoltcp::time::Instant as SmolInstant;
    use netstack_smoltcp::smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv6Address};
    use tokio::io::ReadBuf;
    use tokio::net::TcpListener;
    use tokio_util::sync::PollSender;

    struct PacketPipeReader {
        rx: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
        offset: usize,
    }

    impl AsyncRead for PacketPipeReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.offset < self.pending.len() {
                let n = buf.remaining().min(self.pending.len() - self.offset);
                buf.put_slice(&self.pending[self.offset..self.offset + n]);
                self.offset += n;
                if self.offset == self.pending.len() {
                    self.pending.clear();
                    self.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(pkt)) => {
                    let n = buf.remaining().min(pkt.len());
                    buf.put_slice(&pkt[..n]);
                    if n < pkt.len() {
                        self.pending = pkt;
                        self.offset = n;
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    struct PacketPipeWriter {
        tx: PollSender<Vec<u8>>,
    }

    impl AsyncWrite for PacketPipeWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.tx.poll_reserve(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
                Poll::Ready(Ok(())) => {
                    let len = buf.len();
                    self.tx
                        .send_item(buf.to_vec())
                        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                    Poll::Ready(Ok(len))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.tx.close();
            Poll::Ready(Ok(()))
        }
    }

    struct TestClientDevice {
        mtu: usize,
        rx_queue: VecDeque<Vec<u8>>,
        tx_queue: VecDeque<Vec<u8>>,
    }

    impl TestClientDevice {
        fn new(mtu: usize) -> Self {
            Self {
                mtu,
                rx_queue: VecDeque::new(),
                tx_queue: VecDeque::new(),
            }
        }
    }

    struct TestRxToken(Vec<u8>);
    impl RxToken for TestRxToken {
        fn consume<R, F>(self, f: F) -> R
        where
            F: FnOnce(&[u8]) -> R,
        {
            f(&self.0)
        }
    }

    struct TestTxToken<'a>(&'a mut VecDeque<Vec<u8>>);
    impl<'a> TxToken for TestTxToken<'a> {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            let mut buf = vec![0u8; len];
            let res = f(&mut buf);
            self.0.push_back(buf);
            res
        }
    }

    impl Device for TestClientDevice {
        type RxToken<'a> = TestRxToken;
        type TxToken<'a> = TestTxToken<'a>;

        fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            self.rx_queue
                .pop_front()
                .map(|pkt| (TestRxToken(pkt), TestTxToken(&mut self.tx_queue)))
        }

        fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
            Some(TestTxToken(&mut self.tx_queue))
        }

        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = DeviceCapabilities::default();
            caps.medium = Medium::Ip;
            caps.max_transmission_unit = self.mtu;
            caps
        }
    }

    fn create_test_client_iface(mtu: usize) -> (TestClientDevice, Interface) {
        let mut dev = TestClientDevice::new(mtu);
        let mut cfg = IfaceConfig::new(HardwareAddress::Ip);
        cfg.random_seed = 0x1234_5678;
        let mut iface = Interface::new(cfg, &mut dev, SmolInstant::now());
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 0, 2), 24)).unwrap();
            addrs
                .push(IpCidr::new(IpAddress::v6(0xfd00, 0, 0, 0, 0, 0, 0, 2), 64))
                .unwrap();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 0, 1))
            .unwrap();
        iface
            .routes_mut()
            .add_default_ipv6_route(Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1))
            .unwrap();
        (dev, iface)
    }

    async fn pump_device(
        dev: &mut TestClientDevice,
        iface: &mut Interface,
        sockets: &mut SocketSet<'_>,
        to_engine: &mpsc::Sender<Vec<u8>>,
        from_engine: &mut mpsc::Receiver<Vec<u8>>,
    ) {
        while let Ok(pkt) = from_engine.try_recv() {
            dev.rx_queue.push_back(pkt);
        }
        iface.poll(SmolInstant::now(), dev, sockets);
        while let Some(pkt) = dev.tx_queue.pop_front() {
            let _ = to_engine.send(pkt).await;
        }
        if dev.rx_queue.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                maybe_pkt = from_engine.recv() => {
                    if let Some(pkt) = maybe_pkt {
                        dev.rx_queue.push_back(pkt);
                        while let Ok(p) = from_engine.try_recv() {
                            dev.rx_queue.push_back(p);
                        }
                    }
                }
            }
        }
    }

    enum MockTcpBehavior {
        EchoAndClose,
        StreamZeros(usize),
    }

    async fn start_mock_socks5(
        tcp_behavior: MockTcpBehavior,
    ) -> (
        SocketAddr,
        Arc<Mutex<Vec<TargetAddr>>>,
        Arc<Mutex<Vec<TargetAddr>>>,
        CancellationToken,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp_targets = Arc::new(Mutex::new(Vec::new()));
        let udp_targets = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let tcp_behavior = Arc::new(tcp_behavior);

        let tcp_targets_c = tcp_targets.clone();
        let udp_targets_c = udp_targets.clone();
        let cancel_c = cancel.clone();

        tokio::spawn(async move {
            loop {
                let accept_res = tokio::select! {
                    _ = cancel_c.cancelled() => break,
                    res = listener.accept() => res,
                };
                let Ok((mut client, _)) = accept_res else {
                    break;
                };
                let _ = client.set_nodelay(true);
                let tcp_targets = tcp_targets_c.clone();
                let udp_targets = udp_targets_c.clone();
                let behavior = tcp_behavior.clone();
                let conn_cancel = cancel_c.clone();

                tokio::spawn(async move {
                    let mut greeting_head = [0u8; 2];
                    if client.read_exact(&mut greeting_head).await.is_err() {
                        return;
                    }
                    let mut methods = vec![0u8; greeting_head[1] as usize];
                    if client.read_exact(&mut methods).await.is_err() {
                        return;
                    }
                    if client.write_all(&[0x05, 0x00]).await.is_err() {
                        return;
                    }

                    let mut req_head = [0u8; 4];
                    if client.read_exact(&mut req_head).await.is_err() {
                        return;
                    }
                    let target = match req_head[3] {
                        0x01 => {
                            let mut b = [0u8; 6];
                            if client.read_exact(&mut b).await.is_err() {
                                return;
                            }
                            let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
                            let port = u16::from_be_bytes([b[4], b[5]]);
                            TargetAddr::Ip(SocketAddr::new(IpAddr::V4(ip), port))
                        }
                        0x04 => {
                            let mut b = [0u8; 18];
                            if client.read_exact(&mut b).await.is_err() {
                                return;
                            }
                            let ip_bytes: [u8; 16] = b[..16].try_into().unwrap();
                            let ip = Ipv6Addr::from(ip_bytes);
                            let port = u16::from_be_bytes([b[16], b[17]]);
                            TargetAddr::Ip(SocketAddr::new(IpAddr::V6(ip), port))
                        }
                        0x03 => {
                            let mut lb = [0u8; 1];
                            if client.read_exact(&mut lb).await.is_err() {
                                return;
                            }
                            let mut db = vec![0u8; lb[0] as usize];
                            if client.read_exact(&mut db).await.is_err() {
                                return;
                            }
                            let mut pb = [0u8; 2];
                            if client.read_exact(&mut pb).await.is_err() {
                                return;
                            }
                            TargetAddr::Domain(
                                String::from_utf8_lossy(&db).into_owned(),
                                u16::from_be_bytes(pb),
                            )
                        }
                        _ => return,
                    };

                    if req_head[1] == 0x01 {
                        tcp_targets.lock().unwrap().push(target);
                        if client
                            .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                            .await
                            .is_err()
                        {
                            return;
                        }
                        match behavior.as_ref() {
                            MockTcpBehavior::EchoAndClose => {
                                let mut received = Vec::new();
                                let mut buf = [0u8; 4096];
                                loop {
                                    match client.read(&mut buf).await {
                                        Ok(0) => break,
                                        Ok(n) => received.extend_from_slice(&buf[..n]),
                                        Err(_) => return,
                                    }
                                }
                                let mut resp = b"echo:".to_vec();
                                resp.extend_from_slice(&received);
                                let _ = client.write_all(&resp).await;
                                let _ = client.shutdown().await;
                            }
                            MockTcpBehavior::StreamZeros(total) => {
                                let chunk = vec![0x5Au8; 65536];
                                let mut remaining = *total;
                                while remaining > 0 {
                                    let n = remaining.min(chunk.len());
                                    if client.write_all(&chunk[..n]).await.is_err() {
                                        return;
                                    }
                                    remaining -= n;
                                }
                                let _ = client.shutdown().await;
                            }
                        }
                    } else if req_head[1] == 0x03 {
                        let udp = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
                        let udp_port = udp.local_addr().unwrap().port();
                        let mut rep = vec![0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1];
                        rep.extend_from_slice(&udp_port.to_be_bytes());
                        if client.write_all(&rep).await.is_err() {
                            return;
                        }
                        let mut buf = vec![0u8; 65536];
                        let mut ctrl_buf = [0u8; 32];
                        loop {
                            tokio::select! {
                                _ = conn_cancel.cancelled() => break,
                                r = client.read(&mut ctrl_buf) => {
                                    if matches!(r, Ok(0) | Err(_)) {
                                        break;
                                    }
                                }
                                recv_res = udp.recv_from(&mut buf) => {
                                    let Ok((n, peer)) = recv_res else { break; };
                                    if let Some((dst, payload)) = socks5::decode_udp_packet(&buf[..n]) {
                                        udp_targets.lock().unwrap().push(dst.clone());
                                        let mut out_payload = b"echo:".to_vec();
                                        out_payload.extend_from_slice(payload);
                                        let reply = socks5::encode_udp_packet(&dst, &out_payload);
                                        let _ = udp.send_to(&reply, peer).await;
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        (addr, tcp_targets, udp_targets, cancel)
    }

    #[tokio::test]
    async fn virtual_dns_and_tcp_domain_routing_ipv4_and_ipv6() {
        let (proxy_addr, tcp_targets, _udp_targets, mock_cancel) =
            start_mock_socks5(MockTcpBehavior::EchoAndClose).await;

        let (to_engine_tx, to_engine_rx) = mpsc::channel(256);
        let (from_engine_tx, mut from_engine_rx) = mpsc::channel(256);
        let engine_cancel = CancellationToken::new();
        let engine_handle = tokio::spawn(run_engine(
            PacketPipeReader {
                rx: to_engine_rx,
                pending: Vec::new(),
                offset: 0,
            },
            PacketPipeWriter {
                tx: PollSender::new(from_engine_tx),
            },
            EngineConfig {
                proxy_addr,
                mtu: 9000,
                tcp_recv_buffer_size: 512 * 1024,
                tcp_send_buffer_size: 512 * 1024,
                max_sessions: 256,
                ipv6_fake_dns: true,
                udp_idle_timeout: Duration::from_secs(10),
            },
            engine_cancel.clone(),
        ));

        let (mut dev, mut iface) = create_test_client_iface(9000);
        let mut sockets = SocketSet::new(vec![]);

        let udp_sock = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8192]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8192]),
        );
        let udp_handle = sockets.add(udp_sock);
        sockets.get_mut::<udp::Socket>(udp_handle).bind(40000).unwrap();

        let dns_server: IpAddress = Ipv4Address::new(10, 255, 255, 254).into();
        let q_a = crate::dns::tests::encode_query_wire(101, "example.test", 1, false);
        let q_aaaa = crate::dns::tests::encode_query_wire(102, "example.test", 28, false);
        sockets
            .get_mut::<udp::Socket>(udp_handle)
            .send_slice(&q_a, (dns_server, 53))
            .unwrap();
        sockets
            .get_mut::<udp::Socket>(udp_handle)
            .send_slice(&q_aaaa, (dns_server, 53))
            .unwrap();

        let mut fake_v4: Option<Ipv4Addr> = None;
        let mut fake_v6: Option<Ipv6Addr> = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while (fake_v4.is_none() || fake_v6.is_none()) && Instant::now() < deadline {
            pump_device(&mut dev, &mut iface, &mut sockets, &to_engine_tx, &mut from_engine_rx).await;
            let u = sockets.get_mut::<udp::Socket>(udp_handle);
            let mut rbuf = [0u8; 1024];
            while let Ok((n, _)) = u.recv_slice(&mut rbuf) {
                let id = u16::from_be_bytes([rbuf[0], rbuf[1]]);
                if id == 101 {
                    fake_v4 = Some(Ipv4Addr::new(rbuf[n - 4], rbuf[n - 3], rbuf[n - 2], rbuf[n - 1]));
                } else if id == 102 {
                    let b: [u8; 16] = rbuf[n - 16..n].try_into().unwrap();
                    fake_v6 = Some(Ipv6Addr::from(b));
                }
            }
        }
        let fake_v4 = fake_v4.expect("Fake-IPv4 response");
        let fake_v6 = fake_v6.expect("Fake-IPv6 response");

        // Drive IPv4 and IPv6 TCP connections to the resolved Fake-IPs
        let v4_sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 65536]),
            tcp::SocketBuffer::new(vec![0u8; 65536]),
        );
        let v4_handle = sockets.add(v4_sock);
        sockets
            .get_mut::<tcp::Socket>(v4_handle)
            .connect(iface.context(), (IpAddress::from(fake_v4), 8080), 41001)
            .unwrap();

        let v6_sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 65536]),
            tcp::SocketBuffer::new(vec![0u8; 65536]),
        );
        let v6_handle = sockets.add(v6_sock);
        sockets
            .get_mut::<tcp::Socket>(v6_handle)
            .connect(iface.context(), (IpAddress::from(fake_v6), 8081), 41002)
            .unwrap();

        let mut sent_v4 = false;
        let mut sent_v6 = false;
        let mut recv_v4 = Vec::new();
        let mut recv_v6 = Vec::new();
        let mut closed_v4 = false;
        let mut closed_v6 = false;

        let tcp_deadline = Instant::now() + Duration::from_secs(5);
        while (!closed_v4 || !closed_v6) && Instant::now() < tcp_deadline {
            pump_device(&mut dev, &mut iface, &mut sockets, &to_engine_tx, &mut from_engine_rx).await;

            for (handle, msg, sent, recv_buf, done) in [
                (v4_handle, &b"ping-v4"[..], &mut sent_v4, &mut recv_v4, &mut closed_v4),
                (v6_handle, &b"ping-v6"[..], &mut sent_v6, &mut recv_v6, &mut closed_v6),
            ] {
                let s = sockets.get_mut::<tcp::Socket>(handle);
                if !*sent && s.can_send() {
                    s.send_slice(msg).unwrap();
                    s.close();
                    *sent = true;
                }
                while s.can_recv() {
                    s.recv(|buf| {
                        recv_buf.extend_from_slice(buf);
                        (buf.len(), ())
                    })
                    .unwrap();
                }
                if *sent && !s.may_recv() && matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) {
                    *done = true;
                }
            }
        }

        assert_eq!(recv_v4, b"echo:ping-v4");
        assert_eq!(recv_v6, b"echo:ping-v6");
        let recorded = tcp_targets.lock().unwrap().clone();
        assert!(recorded.contains(&TargetAddr::Domain("example.test".to_string(), 8080)));
        assert!(recorded.contains(&TargetAddr::Domain("example.test".to_string(), 8081)));

        engine_cancel.cancel();
        mock_cancel.cancel();
        let _ = engine_handle.await;
    }

    #[tokio::test]
    async fn udp_associate_roundtrip_via_socks5() {
        let (proxy_addr, _tcp_targets, udp_targets, mock_cancel) =
            start_mock_socks5(MockTcpBehavior::EchoAndClose).await;

        let (to_engine_tx, to_engine_rx) = mpsc::channel(256);
        let (from_engine_tx, mut from_engine_rx) = mpsc::channel(256);
        let engine_cancel = CancellationToken::new();
        let engine_handle = tokio::spawn(run_engine(
            PacketPipeReader {
                rx: to_engine_rx,
                pending: Vec::new(),
                offset: 0,
            },
            PacketPipeWriter {
                tx: PollSender::new(from_engine_tx),
            },
            EngineConfig {
                proxy_addr,
                mtu: 9000,
                tcp_recv_buffer_size: 512 * 1024,
                tcp_send_buffer_size: 512 * 1024,
                max_sessions: 256,
                ipv6_fake_dns: true,
                udp_idle_timeout: Duration::from_secs(10),
            },
            engine_cancel.clone(),
        ));

        let (mut dev, mut iface) = create_test_client_iface(9000);
        let mut sockets = SocketSet::new(vec![]);
        let udp_sock = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8192]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8192]),
        );
        let udp_handle = sockets.add(udp_sock);
        sockets.get_mut::<udp::Socket>(udp_handle).bind(50000).unwrap();

        let dns_server: IpAddress = Ipv4Address::new(10, 255, 255, 254).into();
        let q_a = crate::dns::tests::encode_query_wire(201, "udp.test", 1, false);
        sockets
            .get_mut::<udp::Socket>(udp_handle)
            .send_slice(&q_a, (dns_server, 53))
            .unwrap();

        let mut fake_v4 = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake_v4.is_none() && Instant::now() < deadline {
            pump_device(&mut dev, &mut iface, &mut sockets, &to_engine_tx, &mut from_engine_rx).await;
            let u = sockets.get_mut::<udp::Socket>(udp_handle);
            let mut rbuf = [0u8; 1024];
            if let Ok((n, _)) = u.recv_slice(&mut rbuf) {
                fake_v4 = Some(Ipv4Addr::new(rbuf[n - 4], rbuf[n - 3], rbuf[n - 2], rbuf[n - 1]));
            }
        }
        let fake_v4 = fake_v4.expect("Fake-IPv4 for udp.test");
        let dst_ep = (IpAddress::from(fake_v4), 9999u16);
        sockets
            .get_mut::<udp::Socket>(udp_handle)
            .send_slice(b"hello-udp", dst_ep)
            .unwrap();

        let mut reply = None;
        let udp_deadline = Instant::now() + Duration::from_secs(5);
        while reply.is_none() && Instant::now() < udp_deadline {
            pump_device(&mut dev, &mut iface, &mut sockets, &to_engine_tx, &mut from_engine_rx).await;
            let u = sockets.get_mut::<udp::Socket>(udp_handle);
            let mut rbuf = [0u8; 1024];
            if let Ok((n, meta)) = u.recv_slice(&mut rbuf) {
                reply = Some((rbuf[..n].to_vec(), meta.endpoint));
            }
        }
        let (data, from_ep) = reply.expect("UDP reply from SOCKS5 relay");
        assert_eq!(data, b"echo:hello-udp");
        assert_eq!(from_ep.addr, IpAddress::from(fake_v4));
        assert_eq!(from_ep.port, 9999);
        assert!(udp_targets
            .lock()
            .unwrap()
            .contains(&TargetAddr::Domain("udp.test".to_string(), 9999)));

        engine_cancel.cancel();
        mock_cancel.cancel();
        let _ = engine_handle.await;
    }

    #[tokio::test]
    async fn smoltcp_single_stream_throughput_benchmark() {
        const TOTAL_BYTES: usize = 32 * 1024 * 1024;
        let (proxy_addr, _tcp_targets, _udp_targets, mock_cancel) =
            start_mock_socks5(MockTcpBehavior::StreamZeros(TOTAL_BYTES)).await;

        let (to_engine_tx, to_engine_rx) = mpsc::channel(1024);
        let (from_engine_tx, mut from_engine_rx) = mpsc::channel(1024);
        let engine_cancel = CancellationToken::new();
        let engine_handle = tokio::spawn(run_engine(
            PacketPipeReader {
                rx: to_engine_rx,
                pending: Vec::new(),
                offset: 0,
            },
            PacketPipeWriter {
                tx: PollSender::new(from_engine_tx),
            },
            EngineConfig {
                proxy_addr,
                mtu: 9000,
                tcp_recv_buffer_size: 512 * 1024,
                tcp_send_buffer_size: 512 * 1024,
                max_sessions: 256,
                ipv6_fake_dns: true,
                udp_idle_timeout: Duration::from_secs(10),
            },
            engine_cancel.clone(),
        ));

        let (mut dev, mut iface) = create_test_client_iface(9000);
        let mut sockets = SocketSet::new(vec![]);
        let tcp_sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 512 * 1024]),
            tcp::SocketBuffer::new(vec![0u8; 65536]),
        );
        let tcp_handle = sockets.add(tcp_sock);
        sockets
            .get_mut::<tcp::Socket>(tcp_handle)
            .connect(iface.context(), (IpAddress::v4(93, 184, 216, 34), 443), 42000)
            .unwrap();

        let t0 = Instant::now();
        let mut received = 0usize;
        let deadline = Instant::now() + Duration::from_secs(30);
        while received < TOTAL_BYTES && Instant::now() < deadline {
            while let Ok(pkt) = from_engine_rx.try_recv() {
                dev.rx_queue.push_back(pkt);
            }
            iface.poll(SmolInstant::now(), &mut dev, &mut sockets);
            let s = sockets.get_mut::<tcp::Socket>(tcp_handle);
            while s.can_recv() {
                s.recv(|buf| {
                    received += buf.len();
                    (buf.len(), ())
                })
                .unwrap();
            }
            iface.poll(SmolInstant::now(), &mut dev, &mut sockets);
            while let Some(pkt) = dev.tx_queue.pop_front() {
                let _ = to_engine_tx.send(pkt).await;
            }
            if received < TOTAL_BYTES && dev.rx_queue.is_empty() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                    maybe_pkt = from_engine_rx.recv() => {
                        if let Some(pkt) = maybe_pkt {
                            dev.rx_queue.push_back(pkt);
                            while let Ok(p) = from_engine_rx.try_recv() {
                                dev.rx_queue.push_back(p);
                            }
                        }
                    }
                }
            }
        }
        let elapsed = t0.elapsed();
        assert_eq!(received, TOTAL_BYTES);
        let mb_per_sec = (TOTAL_BYTES as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
        println!(
            "smoltcp 32 MiB single-stream throughput: {:.2} MB/s ({:.3}s)",
            mb_per_sec,
            elapsed.as_secs_f64()
        );

        engine_cancel.cancel();
        mock_cancel.cancel();
        let _ = engine_handle.await;
    }
}
