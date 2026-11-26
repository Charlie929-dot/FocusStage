use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

pub async fn connect_tcp(proxy_addr: SocketAddr, target: &TargetAddr) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    let _ = stream.set_nodelay(true);

    negotiate_no_auth(&mut stream).await?;

    let mut req = Vec::with_capacity(64);
    req.extend_from_slice(&[0x05, 0x01, 0x00]); // VER=5, CMD=CONNECT, RSV=0
    append_target_addr(&mut req, target);
    stream.write_all(&req).await?;

    let mut resp_head = [0u8; 4];
    stream.read_exact(&mut resp_head).await?;
    if resp_head[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid SOCKS5 version in CONNECT reply: {}", resp_head[0]),
        ));
    }
    if resp_head[1] != 0x00 {
        return Err(io::Error::other(format!(
            "SOCKS5 CONNECT rejected (rep={})",
            resp_head[1]
        )));
    }
    let _ = read_socks5_addr(&mut stream, resp_head[3]).await?;
    Ok(stream)
}

pub async fn udp_associate(proxy_addr: SocketAddr) -> io::Result<(TcpStream, SocketAddr)> {
    let mut ctrl = TcpStream::connect(proxy_addr).await?;
    let _ = ctrl.set_nodelay(true);

    negotiate_no_auth(&mut ctrl).await?;

    // VER=5, CMD=UDP ASSOCIATE (0x03), RSV=0, ATYP=IPv4, 0.0.0.0:0
    ctrl.write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    let mut resp_head = [0u8; 4];
    ctrl.read_exact(&mut resp_head).await?;
    if resp_head[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid SOCKS5 version in UDP ASSOCIATE reply: {}", resp_head[0]),
        ));
    }
    if resp_head[1] != 0x00 {
        return Err(io::Error::other(format!(
            "SOCKS5 UDP ASSOCIATE rejected (rep={})",
            resp_head[1]
        )));
    }

    let (bound_ip, bound_port, is_domain) = read_socks5_addr(&mut ctrl, resp_head[3]).await?;
    let relay_ip = if is_domain || bound_ip.is_unspecified() {
        proxy_addr.ip()
    } else {
        bound_ip
    };
    let relay_port = if bound_port == 0 {
        proxy_addr.port()
    } else {
        bound_port
    };
    Ok((ctrl, SocketAddr::new(relay_ip, relay_port)))
}

async fn negotiate_no_auth(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting != [0x05, 0x00] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "SOCKS5 proxy rejected NoAuth handshake: [{:#x}, {:#x}]",
                greeting[0], greeting[1]
            ),
        ));
    }
    Ok(())
}

async fn read_socks5_addr(stream: &mut TcpStream, atyp: u8) -> io::Result<(IpAddr, u16, bool)> {
    let (ip, is_domain) = match atyp {
        0x01 => {
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            (IpAddr::V4(Ipv4Addr::from(buf)), false)
        }
        0x04 => {
            let mut buf = [0u8; 16];
            stream.read_exact(&mut buf).await?;
            (IpAddr::V6(Ipv6Addr::from(buf)), false)
        }
        0x03 => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let mut dom = vec![0u8; len_buf[0] as usize];
            stream.read_exact(&mut dom).await?;
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), true)
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported SOCKS5 ATYP: {other}"),
            ));
        }
    };

    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);
    Ok((ip, port, is_domain))
}

fn append_target_addr(buf: &mut Vec<u8>, target: &TargetAddr) {
    match target {
        TargetAddr::Ip(SocketAddr::V4(v4)) => {
            buf.push(0x01);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        TargetAddr::Ip(SocketAddr::V6(v6)) => {
            buf.push(0x04);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
        TargetAddr::Domain(domain, port) => {
            let bytes = domain.as_bytes();
            let len = bytes.len().min(255);
            buf.push(0x03);
            buf.push(len as u8);
            buf.extend_from_slice(&bytes[..len]);
            buf.extend_from_slice(&port.to_be_bytes());
        }
    }
}

pub fn encode_udp_packet(target: &TargetAddr, payload: &[u8]) -> Vec<u8> {
    let header_cap = match target {
        TargetAddr::Ip(SocketAddr::V4(_)) => 10,
        TargetAddr::Ip(SocketAddr::V6(_)) => 22,
        TargetAddr::Domain(d, _) => 7 + d.len().min(255),
    };
    let mut pkt = Vec::with_capacity(header_cap + payload.len());
    pkt.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV=0, FRAG=0
    append_target_addr(&mut pkt, target);
    pkt.extend_from_slice(payload);
    pkt
}

pub fn decode_udp_packet(buf: &[u8]) -> Option<(TargetAddr, &[u8])> {
    if buf.len() < 4 || buf[2] != 0x00 {
        return None;
    }
    let atyp = buf[3];
    let mut pos = 4usize;
    let target = match atyp {
        0x01 => {
            let ip_bytes: [u8; 4] = buf.get(pos..pos + 4)?.try_into().ok()?;
            pos += 4;
            let port_bytes: [u8; 2] = buf.get(pos..pos + 2)?.try_into().ok()?;
            pos += 2;
            TargetAddr::Ip(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(ip_bytes),
                u16::from_be_bytes(port_bytes),
            )))
        }
        0x03 => {
            let dom_len = *buf.get(pos)? as usize;
            pos += 1;
            let dom_bytes = buf.get(pos..pos + dom_len)?;
            let dom = std::str::from_utf8(dom_bytes).ok()?.to_string();
            pos += dom_len;
            let port_bytes: [u8; 2] = buf.get(pos..pos + 2)?.try_into().ok()?;
            pos += 2;
            TargetAddr::Domain(dom, u16::from_be_bytes(port_bytes))
        }
        0x04 => {
            let ip_bytes: [u8; 16] = buf.get(pos..pos + 16)?.try_into().ok()?;
            pos += 16;
            let port_bytes: [u8; 2] = buf.get(pos..pos + 2)?.try_into().ok()?;
            pos += 2;
            TargetAddr::Ip(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(ip_bytes),
                u16::from_be_bytes(port_bytes),
                0,
                0,
            )))
        }
        _ => return None,
    };

    Some((target, &buf[pos..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_packet_roundtrips_ipv4_ipv6_and_domain() {
        let targets = [
            TargetAddr::Ip("1.2.3.4:5353".parse().unwrap()),
            TargetAddr::Ip("[2001:db8::1]:443".parse().unwrap()),
            TargetAddr::Domain("quic.example.com".to_string(), 443),
        ];
        let payload = b"hello-quic";
        for target in &targets {
            let encoded = encode_udp_packet(target, payload);
            let (decoded_target, decoded_payload) = decode_udp_packet(&encoded).expect("decode");
            assert_eq!(&decoded_target, target);
            assert_eq!(decoded_payload, payload);
        }
    }

    #[test]
    fn rejects_fragmented_socks5_udp_packets() {
        let mut pkt = encode_udp_packet(&TargetAddr::Ip("8.8.8.8:53".parse().unwrap()), b"abc");
        pkt[2] = 0x01; // non-zero FRAG
        assert!(decode_udp_packet(&pkt).is_none());
    }
}
