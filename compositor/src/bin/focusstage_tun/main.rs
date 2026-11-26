mod dns;
mod engine;
mod socks5;

use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use clap::Parser;
use futures_util::TryStreamExt;
use netlink_packet_route::route::RouteAttribute;
use netlink_packet_route::rule::{RuleAction, RuleAttribute, RulePortRange};
use netlink_packet_route::IpProtocol;
use rtnetlink::{Handle, IpVersion, LinkMessageBuilder, LinkUnspec, RouteMessageBuilder};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::engine::EngineConfig;

const TUN_ADDR: Ipv4Addr = Ipv4Addr::new(198, 19, 254, 1);
const TUN_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 252);
const TUN_DEST: Ipv4Addr = Ipv4Addr::new(198, 19, 254, 2);
const TUN_IPV6_ADDR: Ipv6Addr = Ipv6Addr::new(0xfd98, 0x0019, 0x0254, 0, 0, 0, 0, 1);
const TUN_IPV6_PREFIX: u8 = 126;
const TUN_MTU: u16 = 9000;
const TUN_TCP_BUFFER_SIZE: u32 = 512 * 1024;
const TUN_MAX_SESSIONS: usize = 4096;
const TUN_UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const ROUTE_TABLE: u32 = 200;
const DNS_RULE_PREF: u32 = 1;
const SUPPRESS_PREF: u32 = 5200;
const BYPASS_RULE_PREF: u32 = 5201;
const TUN_RULE_PREF: u32 = 5202;
const MAIN_TABLE: u32 = 254;

// Bypass local/LAN/CGNAT/Tailscale/multicast traffic from table 200.
// Do NOT bypass 198.18.0.0/15 (VirtualDns Fake-IP pool) or 198.19.254.0/30 (TUN link).
const BYPASS_CIDRS: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(255, 255, 255, 255), 32),
];

#[derive(Debug, Parser)]
#[command(name = "focusstage-tun", about = "FocusStage WSL transparent proxy TUN helper")]
struct Cli {
    /// Local Windows proxy bridge address (must be socks5://127.0.0.1:<port>)
    #[arg(long)]
    proxy: String,

    /// TUN interface name
    #[arg(long, default_value = "fs-tun0")]
    tun: String,
}

fn validate_tun_name(name: &str) -> Result<&str, String> {
    if name.is_empty()
        || name.len() > 15
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("invalid TUN interface name: '{name}'"));
    }
    Ok(name)
}

fn validate_loopback_socks5(proxy: &str) -> Result<SocketAddrV4, String> {
    let rest = proxy
        .strip_prefix("socks5://")
        .ok_or_else(|| format!("proxy must start with socks5://127.0.0.1:<port>, got '{proxy}'"))?;
    let addr: SocketAddrV4 = rest
        .parse()
        .map_err(|e| format!("invalid loopback proxy address '{proxy}': {e}"))?;
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err(format!("proxy must target 127.0.0.1:<nonzero-port>, got '{proxy}'"));
    }
    Ok(addr)
}

fn read_resolv_nameservers() -> Vec<Ipv4Addr> {
    let Ok(text) = fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        let mut parts = trimmed.split_whitespace();
        if parts.next() == Some("nameserver") {
            if let Some(ip_str) = parts.next() {
                if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                    if !ip.is_loopback() && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
            }
        }
    }
    out
}

async fn cleanup_rules_for_family(handle: &Handle, family: IpVersion) {
    let mut rules = handle.rule().get(family).execute();
    let mut to_delete = Vec::new();
    while let Ok(Some(rule)) = rules.try_next().await {
        let table = rule
            .attributes
            .iter()
            .find_map(|a| match a {
                RuleAttribute::Table(t) => Some(*t),
                _ => None,
            })
            .unwrap_or(rule.header.table as u32);
        let priority = rule.attributes.iter().find_map(|a| match a {
            RuleAttribute::Priority(p) => Some(*p),
            _ => None,
        });
        if table == ROUTE_TABLE
            || priority == Some(SUPPRESS_PREF)
            || priority == Some(BYPASS_RULE_PREF)
            || priority == Some(TUN_RULE_PREF)
        {
            to_delete.push(rule);
        }
    }
    for rule in to_delete {
        let _ = handle.rule().del(rule).execute().await;
    }
}

async fn cleanup_routing_async(handle: &Handle) {
    cleanup_rules_for_family(handle, IpVersion::V4).await;
    cleanup_rules_for_family(handle, IpVersion::V6).await;

    let query_v4 = RouteMessageBuilder::<Ipv4Addr>::new()
        .table_id(ROUTE_TABLE)
        .build();
    let mut routes_v4 = handle.route().get(query_v4).execute();
    let mut v4_to_delete = Vec::new();
    while let Ok(Some(route)) = routes_v4.try_next().await {
        let table = route
            .attributes
            .iter()
            .find_map(|a| match a {
                RouteAttribute::Table(t) => Some(*t),
                _ => None,
            })
            .unwrap_or(route.header.table as u32);
        if table == ROUTE_TABLE {
            v4_to_delete.push(route);
        }
    }
    for route in v4_to_delete {
        let _ = handle.route().del(route).execute().await;
    }

    let query_v6 = RouteMessageBuilder::<Ipv6Addr>::new()
        .table_id(ROUTE_TABLE)
        .build();
    let mut routes_v6 = handle.route().get(query_v6).execute();
    let mut v6_to_delete = Vec::new();
    while let Ok(Some(route)) = routes_v6.try_next().await {
        let table = route
            .attributes
            .iter()
            .find_map(|a| match a {
                RouteAttribute::Table(t) => Some(*t),
                _ => None,
            })
            .unwrap_or(route.header.table as u32);
        if table == ROUTE_TABLE {
            v6_to_delete.push(route);
        }
    }
    for route in v6_to_delete {
        let _ = handle.route().del(route).execute().await;
    }
}

async fn link_index(handle: &Handle, dev: &str) -> Result<u32, String> {
    let mut links = handle.link().get().match_name(dev.to_string()).execute();
    match links
        .try_next()
        .await
        .map_err(|e| format!("netlink link lookup '{dev}' failed: {e}"))?
    {
        Some(link) => Ok(link.header.index),
        None => Err(format!("TUN interface '{dev}' not found")),
    }
}

async fn setup_ipv6_routing_async(handle: &Handle, if_index: u32) -> Result<(), String> {
    handle
        .address()
        .add(if_index, IpAddr::V6(TUN_IPV6_ADDR), TUN_IPV6_PREFIX)
        .execute()
        .await
        .map_err(|e| format!("failed to add IPv6 address to TUN: {e}"))?;

    let fake_v6_route = RouteMessageBuilder::<Ipv6Addr>::new()
        .destination_prefix(dns::FAKE_IPV6_NET, dns::FAKE_IPV6_PREFIX)
        .table_id(ROUTE_TABLE)
        .output_interface(if_index)
        .build();
    handle
        .route()
        .add(fake_v6_route)
        .replace()
        .execute()
        .await
        .map_err(|e| format!("failed to add Fake-IPv6 route to table {ROUTE_TABLE}: {e}"))?;

    handle
        .rule()
        .add()
        .v6()
        .action(RuleAction::ToTable)
        .table_id(ROUTE_TABLE)
        .priority(TUN_RULE_PREF)
        .destination_prefix(dns::FAKE_IPV6_NET, dns::FAKE_IPV6_PREFIX)
        .execute()
        .await
        .map_err(|e| format!("failed to add Fake-IPv6 rule: {e}"))?;

    Ok(())
}

async fn setup_routing_async(handle: &Handle, tun_name: &str) -> Result<bool, String> {
    cleanup_routing_async(handle).await;

    // Allow DNS replies from WSL's local tunneling address (10.255.255.254 on lo)
    // to arrive back through the TUN interface without being dropped as martians.
    let accept_local_path = format!("/proc/sys/net/ipv4/conf/{tun_name}/accept_local");
    fs::write(&accept_local_path, b"1\n")
        .map_err(|e| format!("failed to write {accept_local_path}: {e}"))?;
    let rp_filter_path = format!("/proc/sys/net/ipv4/conf/{tun_name}/rp_filter");
    let _ = fs::write(&rp_filter_path, b"0\n");

    let if_index = link_index(handle, tun_name).await?;
    let link_up = LinkMessageBuilder::<LinkUnspec>::default()
        .index(if_index)
        .up()
        .build();
    handle
        .link()
        .set(link_up)
        .execute()
        .await
        .map_err(|e| format!("failed to bring TUN interface '{tun_name}' up: {e}"))?;

    let default_route = RouteMessageBuilder::<Ipv4Addr>::new()
        .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
        .table_id(ROUTE_TABLE)
        .output_interface(if_index)
        .build();
    handle
        .route()
        .add(default_route)
        .replace()
        .execute()
        .await
        .map_err(|e| format!("failed to add TUN default route to table {ROUTE_TABLE}: {e}"))?;

    for ns in read_resolv_nameservers() {
        let mut req = handle
            .rule()
            .add()
            .v4()
            .action(RuleAction::ToTable)
            .table_id(ROUTE_TABLE)
            .priority(DNS_RULE_PREF)
            .destination_prefix(ns, 32);
        req.message_mut()
            .attributes
            .push(RuleAttribute::IpProtocol(IpProtocol::Udp));
        req.message_mut()
            .attributes
            .push(RuleAttribute::DestinationPortRange(RulePortRange {
                start: 53,
                end: 53,
            }));
        req.execute()
            .await
            .map_err(|e| format!("failed to add DNS intercept rule for {ns}: {e}"))?;
    }

    let mut suppress_req = handle
        .rule()
        .add()
        .v4()
        .action(RuleAction::ToTable)
        .table_id(MAIN_TABLE)
        .priority(SUPPRESS_PREF);
    suppress_req
        .message_mut()
        .attributes
        .push(RuleAttribute::SuppressPrefixLen(0));
    suppress_req
        .execute()
        .await
        .map_err(|e| format!("failed to add suppress_prefixlength rule: {e}"))?;

    for &(net, prefix_len) in BYPASS_CIDRS {
        handle
            .rule()
            .add()
            .v4()
            .action(RuleAction::ToTable)
            .table_id(MAIN_TABLE)
            .priority(BYPASS_RULE_PREF)
            .destination_prefix(net, prefix_len)
            .execute()
            .await
            .map_err(|e| format!("failed to add bypass rule for {net}/{prefix_len}: {e}"))?;
    }

    handle
        .rule()
        .add()
        .v4()
        .action(RuleAction::ToTable)
        .table_id(ROUTE_TABLE)
        .priority(TUN_RULE_PREF)
        .execute()
        .await
        .map_err(|e| format!("failed to add TUN table rule: {e}"))?;

    let ipv6_ready = match setup_ipv6_routing_async(handle, if_index).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("IPv6 Fake-IP routing unavailable (falling back to IPv4-only): {err}");
            false
        }
    };

    Ok(ipv6_ready)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();

    let cli = Cli::parse();
    let tun_name = validate_tun_name(&cli.tun)?.to_string();
    let proxy_addr = validate_loopback_socks5(&cli.proxy)?;

    let mut tun_config = tun::Configuration::default();
    tun_config
        .address(TUN_ADDR)
        .netmask(TUN_NETMASK)
        .destination(TUN_DEST)
        .mtu(TUN_MTU)
        .tun_name(&tun_name)
        .up();
    tun_config.platform_config(|cfg| {
        #[allow(deprecated)]
        cfg.packet_information(false);
        cfg.ensure_root_privileges(false);
    });

    let device = tun::create_as_async(&tun_config)?;
    let (tun_writer, tun_reader) = device.split()?;

    let (connection, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(connection);

    let ipv6_fake_dns = match setup_routing_async(&handle, &tun_name).await {
        Ok(v6) => v6,
        Err(err) => {
            cleanup_routing_async(&handle).await;
            return Err(err.into());
        }
    };

    let shutdown = CancellationToken::new();

    // Exit cleanly when focusstaged closes stdin (or dies).
    let shutdown_stdin = shutdown.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    shutdown_stdin.cancel();
                    break;
                }
                Ok(_) => {}
            }
        }
    });

    // Exit cleanly on SIGTERM / SIGINT.
    let shutdown_sig = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).ok();
            let mut int = signal(SignalKind::interrupt()).ok();
            tokio::select! {
                _ = async {
                    if let Some(ref mut s) = term {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
                _ = async {
                    if let Some(ref mut s) = int {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
            shutdown_sig.cancel();
        }
    });

    let cfg = EngineConfig {
        proxy_addr: SocketAddr::V4(proxy_addr),
        mtu: TUN_MTU,
        tcp_recv_buffer_size: TUN_TCP_BUFFER_SIZE,
        tcp_send_buffer_size: TUN_TCP_BUFFER_SIZE,
        max_sessions: TUN_MAX_SESSIONS,
        ipv6_fake_dns,
        udp_idle_timeout: TUN_UDP_IDLE_TIMEOUT,
    };

    let run_res = engine::run_engine(tun_reader, tun_writer, cfg, shutdown).await;
    cleanup_routing_async(&handle).await;
    run_res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_bypassed_ip(ip: Ipv4Addr) -> bool {
        let addr = u32::from(ip);
        BYPASS_CIDRS.iter().any(|&(net, prefix_len)| {
            let mask = if prefix_len == 0 {
                0
            } else {
                !0u32 << (32 - prefix_len)
            };
            (addr & mask) == (u32::from(net) & mask)
        })
    }

    #[test]
    fn validates_loopback_socks5_only() {
        assert!(validate_loopback_socks5("socks5://127.0.0.1:54321").is_ok());
        assert!(validate_loopback_socks5("http://127.0.0.1:54321").is_err());
        assert!(validate_loopback_socks5("socks5://192.168.1.1:54321").is_err());
        assert!(validate_loopback_socks5("socks5://127.0.0.1:0").is_err());
    }

    #[test]
    fn validates_tun_interface_name() {
        assert!(validate_tun_name("fs-tun0").is_ok());
        assert!(validate_tun_name("").is_err());
        assert!(validate_tun_name("too-long-interface-name").is_err());
        assert!(validate_tun_name("fs;rm -rf /").is_err());
    }

    #[test]
    fn tun_tuning_constants_match_smoltcp_engine() {
        assert_eq!(TUN_MTU, 9000);
        assert_eq!(TUN_TCP_BUFFER_SIZE, 512 * 1024);
        assert!(TUN_MAX_SESSIONS >= 4096);
    }

    #[test]
    fn bypass_cidrs_cover_lan_and_tailscale_without_fake_ip_pool() {
        assert!(is_bypassed_ip(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_bypassed_ip(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_bypassed_ip(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_bypassed_ip(Ipv4Addr::new(100, 79, 13, 24)));
        assert!(!is_bypassed_ip(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_bypassed_ip(Ipv4Addr::new(8, 8, 8, 8)));
    }
}
