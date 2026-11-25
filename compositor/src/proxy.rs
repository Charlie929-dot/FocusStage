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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProxyConfig {
    pub enabled: bool,
    pub bridge_port: u16,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

enum EnvSyncCmd {
    Apply(HostProxyConfig),
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

        apply_process_env(&cfg);
        let _ = self.env_tx.send(EnvSyncCmd::Apply(cfg.clone()));

        if cfg.enabled && cfg.bridge_port > 0 {
            let need_restart = self
                .tun_child
                .as_ref()
                .is_none_or(|(_, _, port)| *port != cfg.bridge_port);
            if need_restart {
                self.stop_tun();
                self.start_tun(cfg.bridge_port);
            }
        } else {
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

    fn start_tun(&mut self, bridge_port: u16) {
        let Some(tun_bin) = resolve_tun_helper_path() else {
            warn!("focusstage-tun helper not found; WSL transparent TUN proxy disabled (env/gsettings sync still active)");
            return;
        };

        let proxy_arg = format!("socks5://127.0.0.1:{bridge_port}");
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
                info!(bridge_port, bin = %tun_bin.display(), "Started WSL transparent TUN proxy");
                self.tun_child = Some((child, stdin, bridge_port));
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
        info!(bridge_port = port, "Stopped WSL transparent TUN proxy");
    }
}

impl Drop for HostProxyManager {
    fn drop(&mut self) {
        self.clear();
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

fn apply_process_env(cfg: &HostProxyConfig) {
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

fn sync_external_proxy_settings(cfg: &HostProxyConfig) {
    if !cfg.enabled {
        reset_external_proxy_settings();
        return;
    }

    if cfg.bridge_port > 0 {
        sync_gsettings_manual(cfg.bridge_port, cfg.no_proxy.as_deref());
    }

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

fn sync_gsettings_manual(port: u16, no_proxy: Option<&str>) {
    let port_str = port.to_string();
    run_gsettings(&["set", "org.gnome.system.proxy", "use-same-proxy", "false"]);
    run_gsettings(&["set", "org.gnome.system.proxy.http", "host", "127.0.0.1"]);
    run_gsettings(&["set", "org.gnome.system.proxy.http", "port", &port_str]);
    run_gsettings(&["set", "org.gnome.system.proxy.https", "host", "127.0.0.1"]);
    run_gsettings(&["set", "org.gnome.system.proxy.https", "port", &port_str]);
    run_gsettings(&["set", "org.gnome.system.proxy.socks", "host", "127.0.0.1"]);
    run_gsettings(&["set", "org.gnome.system.proxy.socks", "port", &port_str]);

    let mut hosts = vec![
        "'localhost'".to_string(),
        "'127.0.0.0/8'".to_string(),
        "'::1'".to_string(),
    ];
    if let Some(np) = no_proxy {
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
