use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};

const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

const DIRECT_PROBE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProxyConfig {
    pub enabled: bool,
    pub bridge_port: u16,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub direct_http_proxy: Option<String>,
    pub direct_https_proxy: Option<String>,
    pub direct_socks_proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProxyConfig {
    pub enabled: bool,
    pub bridge_port: u16,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub tun_proxy_port: Option<u16>,
}

enum EnvSyncCmd {
    Apply(EffectiveProxyConfig),
    Reset(Option<Sender<()>>),
}

pub struct HostProxyManager {
    current: Option<HostProxyConfig>,
    tun_child: Option<(Child, Option<ChildStdin>, u16)>,
    env_tx: Sender<EnvSyncCmd>,
}

impl HostProxyManager {
    pub fn new() -> Self {
        let (env_tx, env_rx) = mpsc::channel::<EnvSyncCmd>();
        let _ = thread::Builder::new()
            .name("focusstage-proxy-env".into())
            .spawn(move || {
                while let Ok(mut cmd) = env_rx.recv() {
                    // Coalesce bursty proxy updates to the latest state.
                    while let Ok(newer) = env_rx.try_recv() {
                        if let EnvSyncCmd::Reset(Some(ack)) = cmd {
                            let _ = ack.send(());
                        }
                        cmd = newer;
                    }
                    match cmd {
                        EnvSyncCmd::Apply(cfg) => sync_external_proxy_settings(&cfg),
                        EnvSyncCmd::Reset(ack) => {
                            reset_external_proxy_settings();
                            if let Some(tx) = ack {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            });

        Self {
            current: None,
            tun_child: None,
            env_tx,
        }
    }

    pub fn apply(&mut self, cfg: HostProxyConfig) {
        if self.current.as_ref() == Some(&cfg) {
            return;
        }

        let effective = resolve_effective_proxy(&cfg, probe_tcp_reachable);
        if effective.enabled {
            info!(
                http_proxy = ?effective.http_proxy,
                https_proxy = ?effective.https_proxy,
                all_proxy = ?effective.all_proxy,
                tun_proxy_port = ?effective.tun_proxy_port,
                bridge_port = effective.bridge_port,
                "Resolved WSL host proxy configuration"
            );
        } else {
            info!("Host proxy disabled; clearing WSL proxy environment");
        }

        apply_process_env(&effective);
        let _ = self.env_tx.send(EnvSyncCmd::Apply(effective.clone()));

        let tun_disabled = tun_disabled_by_env();
        if effective.enabled && !tun_disabled {
            if let Some(tun_port) = effective.tun_proxy_port {
                let need_restart = self
                    .tun_child
                    .as_ref()
                    .is_none_or(|(_, _, port)| *port != tun_port);
                if need_restart {
                    self.stop_tun();
                    self.start_tun(tun_port);
                }
            } else {
                self.stop_tun();
            }
        } else {
            if effective.enabled && tun_disabled {
                info!("focusstage-tun disabled via FOCUSSTAGE_DISABLE_TUN (env/gsettings sync still active)");
            }
            self.stop_tun();
        }

        self.current = Some(cfg);
    }

    pub fn clear(&mut self) {
        self.stop_tun();
        if self.current.is_some() {
            clear_process_env();
            let (ack_tx, ack_rx) = mpsc::channel();
            if self.env_tx.send(EnvSyncCmd::Reset(Some(ack_tx))).is_ok() {
                let _ = ack_rx.recv_timeout(Duration::from_secs(1));
            }
            self.current = None;
        }
    }

    fn start_tun(&mut self, proxy_port: u16) {
        let Some(tun_bin) = resolve_tun_helper_path() else {
            warn!("focusstage-tun helper not found; WSL transparent TUN proxy disabled (env/gsettings sync still active)");
            return;
        };

        let proxy_arg = format!("socks5://127.0.0.1:{proxy_port}");
        let mut cmd = Command::new(&tun_bin);
        cmd.arg("--proxy")
            .arg(&proxy_arg)
            .arg("--tun")
            .arg("fs-tun0")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                // Drain stderr briefly/async so pipe never blocks and startup failures are logged.
                if let Some(stderr) = child.stderr.take() {
                    let _ = thread::Builder::new()
                        .name("focusstage-tun-stderr".into())
                        .spawn(move || {
                            use std::io::{BufRead, BufReader};
                            let reader = BufReader::new(stderr);
                            for line in reader.lines().map_while(Result::ok) {
                                let trimmed = line.trim();
                                if !trimmed.is_empty() {
                                    warn!("focusstage-tun: {trimmed}");
                                }
                            }
                        });
                }
                info!(proxy_port, bin = %tun_bin.display(), "Started WSL transparent TUN proxy");
                self.tun_child = Some((child, stdin, proxy_port));
            }
            Err(err) => {
                warn!(bin = %tun_bin.display(), "Failed to start focusstage-tun: {err}");
            }
        }
    }

    fn stop_tun(&mut self) {
        let Some((mut child, stdin, port)) = self.tun_child.take() else {
            return;
        };
        // Closing stdin signals focusstage-tun to exit cleanly and restore routes.
        drop(stdin);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut exited = false;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    exited = true;
                    break;
                }
            }
        }
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }
        info!(proxy_port = port, "Stopped WSL transparent TUN proxy");
    }
}

impl Drop for HostProxyManager {
    fn drop(&mut self) {
        self.clear();
    }
}

pub fn tun_disabled_by_env() -> bool {
    std::env::var("FOCUSSTAGE_DISABLE_TUN")
        .ok()
        .is_some_and(|v| tun_disabled_by_env_value(&v))
}

pub fn tun_disabled_by_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

pub fn parse_proxy_endpoint(url: &str) -> Option<(String, u16, Option<SocketAddr>)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    if host_port.is_empty() {
        return None;
    }

    let (host, port_str) = if let Some(rest) = host_port.strip_prefix('[') {
        let (h, after_bracket) = rest.split_once(']')?;
        let p = after_bracket.strip_prefix(':')?;
        (h, p)
    } else {
        let (h, p) = host_port.rsplit_once(':')?;
        if h.contains(':') {
            return None;
        }
        (h, p)
    };

    if host.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }

    let probe_addr = if host.eq_ignore_ascii_case("localhost") {
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
    } else if let Ok(ip) = host.parse::<IpAddr>() {
        Some(SocketAddr::new(ip, port))
    } else {
        None
    };

    Some((host.to_string(), port, probe_addr))
}

pub fn probe_tcp_reachable(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, DIRECT_PROBE_TIMEOUT).is_ok()
}

pub fn resolve_effective_proxy<F>(cfg: &HostProxyConfig, mut tcp_probe: F) -> EffectiveProxyConfig
where
    F: FnMut(SocketAddr) -> bool,
{
    if !cfg.enabled {
        return EffectiveProxyConfig {
            enabled: false,
            bridge_port: cfg.bridge_port,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: cfg.no_proxy.clone(),
            tun_proxy_port: None,
        };
    }

    let mut probe_cache: HashMap<SocketAddr, bool> = HashMap::new();
    let mut probe_url = |candidate: Option<&String>| -> Option<(String, u16, Option<SocketAddr>)> {
        let url = candidate.map(String::as_str)?;
        let parsed = parse_proxy_endpoint(url)?;
        let addr = parsed.2?;
        let ok = *probe_cache.entry(addr).or_insert_with(|| tcp_probe(addr));
        ok.then_some(parsed)
    };

    let http_proxy = if probe_url(cfg.direct_http_proxy.as_ref()).is_some() {
        cfg.direct_http_proxy.clone()
    } else {
        cfg.http_proxy.clone()
    };

    let https_proxy = if probe_url(cfg.direct_https_proxy.as_ref()).is_some() {
        cfg.direct_https_proxy.clone()
    } else {
        cfg.https_proxy.clone()
    };

    let direct_socks_parsed = probe_url(cfg.direct_socks_proxy.as_ref());
    let all_proxy = if direct_socks_parsed.is_some() {
        cfg.direct_socks_proxy.clone()
    } else {
        cfg.all_proxy.clone()
    };

    let tun_proxy_port = match direct_socks_parsed {
        Some((_, port, Some(SocketAddr::V4(v4)))) if v4.ip().is_loopback() && port > 0 => {
            Some(port)
        }
        _ => (cfg.bridge_port > 0).then_some(cfg.bridge_port),
    };

    EffectiveProxyConfig {
        enabled: true,
        bridge_port: cfg.bridge_port,
        http_proxy,
        https_proxy,
        all_proxy,
        no_proxy: cfg.no_proxy.clone(),
        tun_proxy_port,
    }
}

fn resolve_tun_helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("focusstage-tun");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    for path in [
        "/usr/local/libexec/focusstage-tun",
        "/usr/local/bin/focusstage-tun",
    ] {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn apply_process_env(cfg: &EffectiveProxyConfig) {
    if !cfg.enabled {
        clear_process_env();
        return;
    }

    set_or_remove("HTTP_PROXY", cfg.http_proxy.as_deref());
    set_or_remove("http_proxy", cfg.http_proxy.as_deref());
    set_or_remove("HTTPS_PROXY", cfg.https_proxy.as_deref());
    set_or_remove("https_proxy", cfg.https_proxy.as_deref());
    set_or_remove("ALL_PROXY", cfg.all_proxy.as_deref());
    set_or_remove("all_proxy", cfg.all_proxy.as_deref());
    set_or_remove("NO_PROXY", cfg.no_proxy.as_deref());
    set_or_remove("no_proxy", cfg.no_proxy.as_deref());
}

fn clear_process_env() {
    for key in PROXY_ENV_KEYS {
        std::env::remove_var(key);
    }
}

fn set_or_remove(key: &str, val: Option<&str>) {
    match val.filter(|s| !s.is_empty()) {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

fn sync_external_proxy_settings(cfg: &EffectiveProxyConfig) {
    if !cfg.enabled {
        reset_external_proxy_settings();
        return;
    }

    sync_gsettings_manual(cfg);

    let mut pairs = Vec::new();
    for (key, val) in [
        ("HTTP_PROXY", cfg.http_proxy.as_deref()),
        ("http_proxy", cfg.http_proxy.as_deref()),
        ("HTTPS_PROXY", cfg.https_proxy.as_deref()),
        ("https_proxy", cfg.https_proxy.as_deref()),
        ("ALL_PROXY", cfg.all_proxy.as_deref()),
        ("all_proxy", cfg.all_proxy.as_deref()),
        ("NO_PROXY", cfg.no_proxy.as_deref()),
        ("no_proxy", cfg.no_proxy.as_deref()),
    ] {
        if let Some(v) = val.filter(|s| !s.is_empty()) {
            pairs.push(format!("{key}={v}"));
        }
    }

    if !pairs.is_empty() {
        let has_systemd = crate::session_env::systemd_user_available();
        let mut cmd = Command::new("dbus-update-activation-environment");
        if has_systemd {
            cmd.arg("--systemd");
        }
        cmd.args(&pairs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.status();
    }
}

fn reset_external_proxy_settings() {
    if crate::session_env::systemd_user_available() {
        let _ = Command::new("systemctl")
            .args(["--user", "unset-environment"])
            .args(PROXY_ENV_KEYS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let empty_pairs: Vec<String> = PROXY_ENV_KEYS.iter().map(|k| format!("{k}=")).collect();
    let _ = Command::new("dbus-update-activation-environment")
        .args(&empty_pairs)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    run_gsettings(&["set", "org.gnome.system.proxy", "mode", "none"]);
}

fn gsettings_host_port(proxy_url: Option<&str>, fallback_port: u16) -> Option<(String, String)> {
    if let Some((host, port, _)) = proxy_url.and_then(parse_proxy_endpoint) {
        return Some((host, port.to_string()));
    }
    if fallback_port > 0 {
        return Some(("127.0.0.1".to_string(), fallback_port.to_string()));
    }
    None
}

fn sync_gsettings_manual(cfg: &EffectiveProxyConfig) {
    let http_hp = gsettings_host_port(cfg.http_proxy.as_deref(), cfg.bridge_port);
    let https_hp = gsettings_host_port(cfg.https_proxy.as_deref(), cfg.bridge_port);
    let socks_hp = gsettings_host_port(cfg.all_proxy.as_deref(), cfg.bridge_port);

    if http_hp.is_none() && https_hp.is_none() && socks_hp.is_none() {
        return;
    }

    run_gsettings(&["set", "org.gnome.system.proxy", "use-same-proxy", "false"]);
    if let Some((host, port)) = http_hp {
        run_gsettings(&["set", "org.gnome.system.proxy.http", "host", &host]);
        run_gsettings(&["set", "org.gnome.system.proxy.http", "port", &port]);
    }
    if let Some((host, port)) = https_hp {
        run_gsettings(&["set", "org.gnome.system.proxy.https", "host", &host]);
        run_gsettings(&["set", "org.gnome.system.proxy.https", "port", &port]);
    }
    if let Some((host, port)) = socks_hp {
        run_gsettings(&["set", "org.gnome.system.proxy.socks", "host", &host]);
        run_gsettings(&["set", "org.gnome.system.proxy.socks", "port", &port]);
    }

    let mut hosts = vec![
        "'localhost'".to_string(),
        "'127.0.0.0/8'".to_string(),
        "'::1'".to_string(),
    ];
    if let Some(np) = cfg.no_proxy.as_deref() {
        for raw in np.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let clean = raw.replace('\'', "");
            if clean.is_empty() {
                continue;
            }
            let quoted = format!("'{clean}'");
            if !hosts.contains(&quoted) {
                hosts.push(quoted);
            }
        }
    }
    let gvariant_list = format!("[{}]", hosts.join(", "));
    run_gsettings(&["set", "org.gnome.system.proxy", "ignore-hosts", &gvariant_list]);
    run_gsettings(&["set", "org.gnome.system.proxy", "mode", "manual"]);
}

fn run_gsettings(args: &[&str]) {
    let _ = Command::new("gsettings")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> HostProxyConfig {
        HostProxyConfig {
            enabled: true,
            bridge_port: 32142,
            http_proxy: Some("http://127.0.0.1:32142".to_string()),
            https_proxy: Some("http://127.0.0.1:32142".to_string()),
            all_proxy: Some("socks5h://127.0.0.1:32142".to_string()),
            no_proxy: Some("localhost,127.0.0.1,::1".to_string()),
            direct_http_proxy: Some("http://127.0.0.1:7897".to_string()),
            direct_https_proxy: Some("http://127.0.0.1:7897".to_string()),
            direct_socks_proxy: Some("socks5h://127.0.0.1:7897".to_string()),
        }
    }

    #[test]
    fn parse_proxy_endpoint_handles_ipv4_localhost_ipv6_and_domains() {
        assert_eq!(
            parse_proxy_endpoint("http://127.0.0.1:7897"),
            Some((
                "127.0.0.1".to_string(),
                7897,
                Some("127.0.0.1:7897".parse().unwrap())
            ))
        );
        assert_eq!(
            parse_proxy_endpoint("socks5h://localhost:1080/"),
            Some((
                "localhost".to_string(),
                1080,
                Some("127.0.0.1:1080".parse().unwrap())
            ))
        );
        assert_eq!(
            parse_proxy_endpoint("socks5h://[::1]:7897"),
            Some(("::1".to_string(), 7897, Some("[::1]:7897".parse().unwrap())))
        );
        assert_eq!(
            parse_proxy_endpoint("http://proxy.internal:8080"),
            Some(("proxy.internal".to_string(), 8080, None))
        );
        assert_eq!(parse_proxy_endpoint("http://127.0.0.1:0"), None);
    }

    #[test]
    fn resolve_effective_proxy_uses_direct_when_reachable() {
        let mut probe_calls = 0usize;
        let effective = resolve_effective_proxy(&sample_cfg(), |addr| {
            probe_calls += 1;
            addr == "127.0.0.1:7897".parse().unwrap()
        });
        assert_eq!(probe_calls, 1);
        assert_eq!(
            effective.http_proxy.as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            effective.https_proxy.as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            effective.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:7897")
        );
        assert_eq!(effective.tun_proxy_port, Some(7897));
    }

    #[test]
    fn resolve_effective_proxy_falls_back_to_bridge_when_unreachable_or_pac() {
        let nat_mode = resolve_effective_proxy(&sample_cfg(), |_| false);
        assert_eq!(
            nat_mode.http_proxy.as_deref(),
            Some("http://127.0.0.1:32142")
        );
        assert_eq!(
            nat_mode.https_proxy.as_deref(),
            Some("http://127.0.0.1:32142")
        );
        assert_eq!(
            nat_mode.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:32142")
        );
        assert_eq!(nat_mode.tun_proxy_port, Some(32142));

        let mut pac_cfg = sample_cfg();
        pac_cfg.direct_http_proxy = None;
        pac_cfg.direct_https_proxy = None;
        pac_cfg.direct_socks_proxy = None;
        let pac_mode = resolve_effective_proxy(&pac_cfg, |_| true);
        assert_eq!(
            pac_mode.http_proxy.as_deref(),
            Some("http://127.0.0.1:32142")
        );
        assert_eq!(
            pac_mode.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:32142")
        );
        assert_eq!(pac_mode.tun_proxy_port, Some(32142));
    }

    #[test]
    fn resolve_effective_proxy_handles_http_only_and_non_loopback_socks() {
        let mut http_only = sample_cfg();
        http_only.direct_socks_proxy = None;
        let resolved = resolve_effective_proxy(&http_only, |_| true);
        assert_eq!(
            resolved.http_proxy.as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            resolved.https_proxy.as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            resolved.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:32142")
        );
        assert_eq!(resolved.tun_proxy_port, Some(32142));

        let mut lan_socks = sample_cfg();
        lan_socks.direct_socks_proxy = Some("socks5h://192.168.1.50:1080".to_string());
        let resolved_lan = resolve_effective_proxy(&lan_socks, |_| true);
        assert_eq!(
            resolved_lan.all_proxy.as_deref(),
            Some("socks5h://192.168.1.50:1080")
        );
        assert_eq!(resolved_lan.tun_proxy_port, Some(32142));
    }

    #[test]
    fn tun_disabled_env_values_are_parsed_correctly() {
        assert!(tun_disabled_by_env_value("1"));
        assert!(tun_disabled_by_env_value("true"));
        assert!(tun_disabled_by_env_value(" YES "));
        assert!(!tun_disabled_by_env_value("0"));
        assert!(!tun_disabled_by_env_value("false"));
        assert!(!tun_disabled_by_env_value(""));
    }
}
