use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4};

use clap::Parser;
use futures_util::TryStreamExt;
use netlink_packet_route::route::RouteAttribute;
use netlink_packet_route::rule::{RuleAction, RuleAttribute, RulePortRange};
use netlink_packet_route::IpProtocol;
use rtnetlink::{Handle, IpVersion, LinkMessageBuilder, LinkUnspec, RouteMessageBuilder};
use tokio::io::AsyncReadExt;
use tun2proxy::{ArgDns, Args, CancellationToken, LevelFilter, ProxyParameters};

const TUN_ADDR: Ipv4Addr = Ipv4Addr::new(198, 19, 254, 1);
const TUN_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 252);
const TUN_DEST: Ipv4Addr = Ipv4Addr::new(198, 19, 254, 2);
const TUN_MTU: u16 = 1500;
const ROUTE_TABLE: u32 = 200;
const DNS_RULE_PREF: u32 = 1;
const SUPPRESS_PREF: u32 = 5200;
const TUN_RULE_PREF: u32 = 5201;
const MAIN_TABLE: u32 = 254;

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

async fn cleanup_routing_async(handle: &Handle) {
    let mut rules = handle.rule().get(IpVersion::V4).execute();
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
        if table == ROUTE_TABLE || priority == Some(SUPPRESS_PREF) || priority == Some(TUN_RULE_PREF) {
            to_delete.push(rule);
        }
    }
    for rule in to_delete {
        let _ = handle.rule().del(rule).execute().await;
    }

    let query = RouteMessageBuilder::<Ipv4Addr>::new()
        .table_id(ROUTE_TABLE)
        .build();
    let mut routes = handle.route().get(query).execute();
    let mut routes_to_delete = Vec::new();
    while let Ok(Some(route)) = routes.try_next().await {
        let table = route
            .attributes
            .iter()
            .find_map(|a| match a {
                RouteAttribute::Table(t) => Some(*t),
                _ => None,
            })
            .unwrap_or(route.header.table as u32);
        if table == ROUTE_TABLE {
            routes_to_delete.push(route);
        }
    }
    for route in routes_to_delete {
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

async fn setup_routing_async(handle: &Handle, tun_name: &str) -> Result<(), String> {
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

    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let tun_name = validate_tun_name(&cli.tun)?.to_string();
    let proxy_addr = validate_loopback_socks5(&cli.proxy)?;
    let proxy_url = format!("socks5://{proxy_addr}");

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
        cfg.packet_information(true);
        cfg.ensure_root_privileges(false);
    });

    let device = tun::create_as_async(&tun_config)?;

    let (connection, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(connection);

    if let Err(err) = setup_routing_async(&handle, &tun_name).await {
        cleanup_routing_async(&handle).await;
        return Err(err.into());
    }

    let mut args = Args::default();
    args.proxy(ProxyParameters::try_from(proxy_url.as_str())?);
    args.dns(ArgDns::Virtual);
    args.tun(tun_name);
    args.setup(false);
    args.ipv6_enabled(false);
    args.verbosity(LevelFilter::Warn);

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

    let run_res = tun2proxy::run(device, TUN_MTU, args, shutdown).await;
    cleanup_routing_async(&handle).await;
    run_res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
