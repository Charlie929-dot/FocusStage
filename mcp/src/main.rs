use std::path::PathBuf;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use focusstage_compositor::control::ControlClient;
use focusstage_compositor::protocol::{
    default_socket_path, Action, ControlRequest, ControlResponse, DisplayState, StageKind,
};
use focusstage_mcp::run_mcp_stdio;

#[derive(Debug, Parser)]
#[command(name = "focusstage", about = "FocusStage CLI & MCP stdio bridge")]
struct Cli {
    /// Control socket path
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Output raw JSON responses
    #[arg(long, global = true, default_value_t = false)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run MCP server over stdio (for Claude Code / Cursor / AI agents)
    Mcp,
    /// Ping focusstaged
    Ping,
    /// List active Stages
    List,
    /// Launch an app inside a new Stage
    Launch {
        app: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long, default_value_t = false)]
        hidden: bool,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        wm_class: Option<String>,
        #[arg(long, default_value_t = false)]
        ensure_daemon: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List discovered Linux GUI applications from .desktop entries
    ListApps,
    /// Generate FocusStage wrapper .desktop entries for Linux GUI apps so WSLg adds them to Win11 Start Menu & Taskbar
    SyncApps {
        /// Remove previously generated focusstage-*.desktop wrappers
        #[arg(long, default_value_t = false)]
        remove: bool,
        /// Target applications directory watched by WSLg (defaults to /usr/share/applications for root)
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Close a Stage
    Close { stage_id: String },
    /// Acquire AI lease
    Acquire {
        stage_id: String,
        #[arg(long, default_value_t = 60)]
        ttl: u64,
        #[arg(long, default_value_t = false)]
        keep_visible: bool,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Observe Stage screenshot and/or A11y tree
    Observe {
        #[arg(long)]
        lease: Option<String>,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long, default_value_t = false)]
        no_image: bool,
        #[arg(long, default_value_t = false)]
        som: bool,
        #[arg(long, default_value_t = false)]
        a11y: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Execute actions JSON on a leased Stage
    Act {
        #[arg(long)]
        lease: String,
        #[arg(long)]
        actions_json: String,
        #[arg(long)]
        based_on: Option<u64>,
    },
    /// Release an AI lease
    Release {
        lease: String,
        #[arg(long, default_value_t = false)]
        keep_hidden: bool,
    },
    /// Human takeover (preempt AI lease & restore desktop window)
    Takeover { stage_id: Option<String> },
    /// Show Stage on Windows desktop
    Show { stage_id: String },
    /// Hide Stage to background
    Hide { stage_id: String },
    /// Freeze or unfreeze Stage input
    Freeze {
        stage_id: String,
        #[arg(long, default_value_t = false)]
        unfreeze: bool,
    },
    /// Get Stage clipboard
    ClipboardGet {
        #[arg(long)]
        lease: Option<String>,
        #[arg(long)]
        stage: Option<String>,
    },
    /// Set Stage clipboard
    ClipboardSet {
        #[arg(long)]
        lease: Option<String>,
        #[arg(long)]
        stage: Option<String>,
        text: String,
    },
    /// Push Windows host clipboard text into Human-owned Stages
    HostClipboardSync { text: String },
    /// Read latest clipboard text exported by Human-owned Stages
    HostClipboardGet,
    /// Drop files onto a Stage (translates C:\... to /mnt/c/...)
    DropFiles {
        stage_id: String,
        #[arg(long)]
        x: Option<f64>,
        #[arg(long)]
        y: Option<f64>,
        paths: Vec<String>,
    },
    /// Stop focusstaged
    Shutdown,
}

fn parse_kind(s: &str) -> anyhow::Result<StageKind> {
    match s.to_ascii_lowercase().as_str() {
        "wayland" => Ok(StageKind::Wayland),
        "chromium" => Ok(StageKind::Chromium),
        "electron" => Ok(StageKind::Electron),
        "x11" => Ok(StageKind::X11),
        _ => anyhow::bail!("Invalid stage kind: {s}"),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let sock = cli.socket.unwrap_or_else(default_socket_path);

    if matches!(cli.command, Commands::Mcp) {
        return run_mcp_stdio(sock);
    }

    if matches!(cli.command, Commands::ListApps) {
        return run_list_apps(cli.json);
    }

    if let Commands::SyncApps { remove, out_dir } = cli.command {
        let target_dir = out_dir.unwrap_or_else(default_sync_apps_dir);
        return run_sync_apps(&target_dir, remove, cli.json);
    }

    if let Commands::Launch {
        ensure_daemon: true,
        ..
    } = &cli.command
    {
        ensure_focusstaged_running(&sock)?;
    }

    let client = ControlClient::new(sock);
    let req = match cli.command {
        Commands::Mcp => unreachable!(),
        Commands::Ping => ControlRequest::Ping,
        Commands::List => ControlRequest::List,
        Commands::Launch {
            app,
            kind,
            width,
            height,
            hidden,
            cwd,
            wm_class,
            ensure_daemon: _,
            mut args,
        } => {
            if args.first().map(|s| s.as_str()) == Some("--") {
                args.remove(0);
            }
            ControlRequest::Launch {
                app,
                args,
                kind: kind.as_deref().map(parse_kind).transpose()?,
                env: Default::default(),
                cwd,
                width,
                height,
                show_on_desktop: Some(!hidden),
                wm_class,
            }
        }
        Commands::ListApps | Commands::SyncApps { .. } => unreachable!(),
        Commands::Close { stage_id } => ControlRequest::Close { stage_id },
        Commands::Acquire {
            stage_id,
            ttl,
            keep_visible,
            agent,
        } => ControlRequest::Acquire {
            stage_id,
            ttl_s: ttl,
            hide: !keep_visible,
            agent_name: agent,
        },
        Commands::Observe {
            lease,
            stage,
            no_image,
            som,
            a11y,
            out,
        } => {
            let mut resp = client.call(&ControlRequest::Observe {
                lease,
                stage_id: stage,
                image: !no_image,
                a11y,
            })?;
            if let ControlResponse::Observed {
                scale,
                ref mut png_base64,
                ..
            } = resp
            {
                if som {
                    if let Some(ref b64) = png_base64 {
                        let raw = base64::engine::general_purpose::STANDARD.decode(b64)?;
                        let ann = focusstage_mcp::som::annotate_png(&raw, scale)
                            .map_err(anyhow::Error::msg)?;
                        *png_base64 =
                            Some(base64::engine::general_purpose::STANDARD.encode(ann.png_bytes));
                    }
                }
                if let Some(out_path) = out {
                    if let Some(ref b64) = png_base64 {
                        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
                        std::fs::write(&out_path, bytes)?;
                    }
                }
            }
            print_resp(&resp, cli.json);
            return Ok(());
        }
        Commands::Act {
            lease,
            actions_json,
            based_on,
        } => {
            let actions: Vec<Action> = serde_json::from_str(&actions_json)?;
            ControlRequest::Act {
                lease,
                actions,
                based_on,
            }
        }
        Commands::Release { lease, keep_hidden } => ControlRequest::Release {
            lease,
            show_on_desktop: !keep_hidden,
        },
        Commands::Takeover { stage_id } => ControlRequest::Takeover { stage_id },
        Commands::Show { stage_id } => ControlRequest::SetDisplay {
            stage_id,
            display: DisplayState::Desktop,
        },
        Commands::Hide { stage_id } => ControlRequest::SetDisplay {
            stage_id,
            display: DisplayState::Hidden,
        },
        Commands::Freeze { stage_id, unfreeze } => ControlRequest::SetFrozen {
            stage_id,
            frozen: !unfreeze,
        },
        Commands::ClipboardGet { lease, stage } => ControlRequest::ClipboardGet {
            lease,
            stage_id: stage,
        },
        Commands::ClipboardSet { lease, stage, text } => ControlRequest::ClipboardSet {
            lease,
            stage_id: stage,
            text,
        },
        Commands::HostClipboardSync { text } => ControlRequest::SyncHostClipboard { text },
        Commands::HostClipboardGet => ControlRequest::GetHostClipboard,
        Commands::DropFiles {
            stage_id,
            x,
            y,
            paths,
        } => ControlRequest::DropFiles {
            stage_id,
            paths,
            x,
            y,
        },
        Commands::Shutdown => ControlRequest::Shutdown,
    };

    let resp = client.call(&req)?;
    print_resp(&resp, cli.json);
    if let ControlResponse::Error { .. } = resp {
        std::process::exit(1);
    }
    Ok(())
}

fn print_resp(resp: &ControlResponse, json_mode: bool) {
    if json_mode {
        println!("{}", serde_json::to_string(resp).unwrap());
    } else {
        println!("{}", serde_json::to_string_pretty(resp).unwrap());
    }
}

fn ensure_focusstaged_running(sock: &std::path::Path) -> anyhow::Result<()> {
    let client = ControlClient::new(sock.to_path_buf());
    if matches!(client.call(&ControlRequest::Ping), Ok(ControlResponse::Pong { .. })) {
        return Ok(());
    }

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("focusstaged")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("focusstaged"));

    let log_path = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("focusstaged.log");
    let effective_log_path = if std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .is_ok()
    {
        log_path
    } else {
        PathBuf::from(format!("/tmp/focusstaged-{}.log", unsafe { libc::geteuid() }))
    };

    let normalized_path = build_normalized_path();
    let started_via_systemd = if std::path::Path::new("/run/systemd/system").exists()
        && std::process::Command::new("systemctl")
            .args(["--user", "status"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        let mut sd = std::process::Command::new("systemd-run");
        sd.args(["--user", "--collect"])
            .args(["-E", &format!("PATH={normalized_path}")])
            .args([
                "-p",
                &format!("StandardOutput=append:{}", effective_log_path.display()),
                "-p",
                &format!("StandardError=append:{}", effective_log_path.display()),
            ]);
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "PULSE_SERVER", "WSL_INTEROP", "XDG_CONFIG_HOME"] {
            if let Ok(val) = std::env::var(key) {
                if !val.is_empty() {
                    sd.args(["-E", &format!("{key}={val}")]);
                }
            }
        }
        sd.arg(&exe)
            .arg("--socket")
            .arg(sock)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        sd.status().is_ok_and(|s| s.success())
    } else {
        false
    };

    if !started_via_systemd {
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&effective_log_path)?;
        let log_err = log_file.try_clone()?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.env("PATH", normalized_path)
            .arg("--socket")
            .arg(sock)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_err));

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("Failed to start focusstaged: {e}"))?;
    }

    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if matches!(client.call(&ControlRequest::Ping), Ok(ControlResponse::Pong { .. })) {
            return Ok(());
        }
    }
    anyhow::bail!("Timed out waiting for focusstaged on {}", sock.display());
}

fn build_normalized_path() -> String {
    let mut parts: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut extras = vec![
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/snap/bin".to_string(),
        "/var/lib/flatpak/exports/bin".to_string(),
    ];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            extras.push(format!("{home}/.local/bin"));
        }
    }
    for dir in extras {
        if !parts.iter().any(|p| p == &dir) {
            parts.push(dir);
        }
    }
    parts.join(":")
}

fn default_sync_apps_dir() -> PathBuf {
    if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/usr/share/applications")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/applications")
    } else {
        PathBuf::from("/usr/share/applications")
    }
}

struct DiscoveredDesktopApp {
    stem: String,
    wrapper_id: String,
    source: &'static str,
    file_path: Option<String>,
    parsed: DesktopEntryParsed,
    icon_path: Option<String>,
}

fn detect_windows_user_dir_for_apps() -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir("/mnt/c/Users") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(s.as_ref(), "Public" | "Default" | "Default User" | "All Users") {
                continue;
            }
            let path = entry.path();
            if path.join("Downloads").is_dir() && path.join("Desktop").is_dir() {
                return Some(path);
            }
        }
    }
    None
}

fn resolve_icon_path(icon: &str) -> Option<String> {
    let icon = icon.trim();
    if icon.is_empty() {
        return None;
    }
    let p = std::path::Path::new(icon);
    if p.is_absolute() && p.is_file() {
        return Some(icon.to_string());
    }

    let exts = ["png", "ico", "bmp", "xpm", "svg"];
    let mut candidate_dirs = vec![
        PathBuf::from("/usr/share/icons/hicolor/512x512/apps"),
        PathBuf::from("/usr/share/icons/hicolor/256x256/apps"),
        PathBuf::from("/usr/share/icons/hicolor/128x128/apps"),
        PathBuf::from("/usr/share/icons/hicolor/64x64/apps"),
        PathBuf::from("/usr/share/icons/hicolor/48x48/apps"),
        PathBuf::from("/usr/share/icons/hicolor/32x32/apps"),
        PathBuf::from("/usr/share/icons/Yaru/256x256/apps"),
        PathBuf::from("/usr/share/icons/Yaru/128x128/apps"),
        PathBuf::from("/usr/share/icons/Yaru/48x48/apps"),
        PathBuf::from("/usr/share/pixmaps"),
        PathBuf::from("/usr/local/share/pixmaps"),
        PathBuf::from("/var/lib/snapd/desktop/icons"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor/256x256/apps"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor/128x128/apps"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor/48x48/apps"),
        PathBuf::from("/usr/share/icons/hicolor/scalable/apps"),
        PathBuf::from("/usr/share/icons/Yaru/scalable/apps"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            candidate_dirs.push(PathBuf::from(&home).join(".local/share/icons/hicolor/256x256/apps"));
            candidate_dirs.push(PathBuf::from(&home).join(".local/share/icons/hicolor/128x128/apps"));
            candidate_dirs.push(PathBuf::from(&home).join(".local/share/icons/hicolor/48x48/apps"));
            candidate_dirs.push(PathBuf::from(&home).join(".local/share/icons"));
        }
    }

    for ext in exts {
        for dir in &candidate_dirs {
            let candidate = dir.join(format!("{icon}.{ext}"));
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    None
}

fn discover_desktop_apps() -> Vec<DiscoveredDesktopApp> {
    let mut scan_dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/var/lib/snapd/desktop/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    let mut download_dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            let h = PathBuf::from(&home);
            scan_dirs.push(h.join(".local/share/applications"));
            scan_dirs.push(h.join(".local/share/flatpak/exports/share/applications"));
            scan_dirs.push(h.join("Desktop"));
            scan_dirs.push(h.join("Downloads"));
            download_dirs.push(h.join("Downloads"));
        }
    }
    if let Some(win_home) = detect_windows_user_dir_for_apps() {
        download_dirs.push(win_home.join("Downloads"));
    }

    let mut apps = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for dir in &scan_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with("focusstage-") || !seen_ids.insert(stem.to_string()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(parsed) = parse_desktop_entry(&text) else {
                continue;
            };
            // Windows opens these paths via the WSL share, which cannot follow Snap's `current` link.
            let icon_path = parsed
                .icon
                .as_deref()
                .and_then(resolve_icon_path)
                .and_then(|path| std::fs::canonicalize(path).ok())
                .map(|path| path.display().to_string());
            let wrapper_id = format!("focusstage-{stem}");
            apps.push(DiscoveredDesktopApp {
                stem: stem.to_string(),
                wrapper_id,
                source: "desktop",
                file_path: Some(path.display().to_string()),
                parsed,
                icon_path,
            });
        }
    }

    // Discover browser-downloaded .AppImage and .deb files
    for dir in &download_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let lower = fname.to_ascii_lowercase();
            let (source, ext_len) = if lower.ends_with(".appimage") {
                ("appimage", ".appimage".len())
            } else if lower.ends_with(".deb") {
                ("deb", ".deb".len())
            } else {
                continue;
            };
            let raw_stem = &fname[..fname.len() - ext_len];
            let safe_stem: String = raw_stem
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let id_key = format!("{source}-{safe_stem}");
            if !seen_ids.insert(id_key.clone()) {
                continue;
            }
            let full = path.display().to_string();
            apps.push(DiscoveredDesktopApp {
                stem: id_key.clone(),
                wrapper_id: format!("focusstage-{id_key}"),
                source,
                file_path: Some(full.clone()),
                parsed: DesktopEntryParsed {
                    name: raw_stem.to_string(),
                    name_zh_cn: None,
                    generic_name: Some(if source == "appimage" {
                        "便携应用 (AppImage)".to_string()
                    } else {
                        "安装包 (.deb)".to_string()
                    }),
                    cmd: full,
                    args: Vec::new(),
                    icon: None,
                    categories: Some("Downloads;".to_string()),
                },
                icon_path: None,
            });
        }
    }

    apps.sort_by(|a, b| a.parsed.name.to_lowercase().cmp(&b.parsed.name.to_lowercase()));
    apps
}

fn run_list_apps(json_mode: bool) -> anyhow::Result<()> {
    let apps: Vec<_> = discover_desktop_apps()
        .into_iter()
        .map(|app| {
            serde_json::json!({
                "id": app.wrapper_id,
                "stem": app.stem,
                "source": app.source,
                "file_path": app.file_path,
                "name": app.parsed.name,
                "name_zh_cn": app.parsed.name_zh_cn,
                "generic_name": app.parsed.generic_name,
                "cmd": app.parsed.cmd,
                "args": app.parsed.args,
                "wm_class": app.wrapper_id,
                "icon": app.parsed.icon,
                "icon_path": app.icon_path,
                "categories": app.parsed.categories,
            })
        })
        .collect();
    let payload = serde_json::json!({
        "status": "apps",
        "count": apps.len(),
        "apps": apps,
    });
    println!(
        "{}",
        if json_mode {
            serde_json::to_string(&payload)?
        } else {
            serde_json::to_string_pretty(&payload)?
        }
    );
    Ok(())
}

fn run_sync_apps(out_dir: &std::path::Path, remove: bool, json_mode: bool) -> anyhow::Result<()> {
    if remove {
        let mut removed = Vec::new();
        if let Ok(entries) = std::fs::read_dir(out_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with("focusstage-") && name.ends_with(".desktop") {
                        std::fs::remove_file(&p)?;
                        removed.push(p.display().to_string());
                    }
                }
            }
        }
        let payload = serde_json::json!({ "status": "ok", "removed": removed });
        println!(
            "{}",
            if json_mode {
                serde_json::to_string(&payload)?
            } else {
                serde_json::to_string_pretty(&payload)?
            }
        );
        return Ok(());
    }

    std::fs::create_dir_all(out_dir)?;
    let mut created = Vec::new();
    for app in discover_desktop_apps() {
        if app.source != "desktop" {
            continue;
        }
        let parsed = &app.parsed;
        let wrapper_id = &app.wrapper_id;
        let wrapper_path = out_dir.join(format!("{wrapper_id}.desktop"));
        let mut exec_line = format!(
            "/usr/local/bin/focusstage-launch --wm-class {} {}",
            shell_quote_desktop(wrapper_id),
            shell_quote_desktop(&parsed.cmd)
        );
        if !parsed.args.is_empty() {
            exec_line.push_str(" --");
            for a in &parsed.args {
                exec_line.push(' ');
                exec_line.push_str(&shell_quote_desktop(a));
            }
        }

        let mut out = String::new();
        out.push_str("[Desktop Entry]\n");
        out.push_str("Type=Application\n");
        out.push_str(&format!("Name={} (FocusStage)\n", parsed.name));
        if let Some(ref zh) = parsed.name_zh_cn {
            out.push_str(&format!("Name[zh_CN]={zh} (FocusStage)\n"));
        }
        if let Some(ref g) = parsed.generic_name {
            out.push_str(&format!("GenericName={g}\n"));
        }
        out.push_str(&format!("Exec={exec_line}\n"));
        if let Some(ref icon) = parsed.icon {
            out.push_str(&format!("Icon={icon}\n"));
        }
        out.push_str("Terminal=false\n");
        if let Some(ref cats) = parsed.categories {
            out.push_str(&format!("Categories={cats}\n"));
        }
        out.push_str(&format!("StartupWMClass={wrapper_id}\n"));

        std::fs::write(&wrapper_path, out)?;
        created.push(serde_json::json!({
            "id": wrapper_id,
            "name": format!("{} (FocusStage)", parsed.name),
            "path": wrapper_path.display().to_string(),
            "cmd": parsed.cmd,
        }));
    }

    let payload = serde_json::json!({
        "status": "ok",
        "count": created.len(),
        "entries": created,
    });
    println!(
        "{}",
        if json_mode {
            serde_json::to_string(&payload)?
        } else {
            serde_json::to_string_pretty(&payload)?
        }
    );
    Ok(())
}

struct DesktopEntryParsed {
    name: String,
    name_zh_cn: Option<String>,
    generic_name: Option<String>,
    cmd: String,
    args: Vec<String>,
    icon: Option<String>,
    categories: Option<String>,
}

fn parse_desktop_entry(text: &str) -> Option<DesktopEntryParsed> {
    let mut in_main = false;
    let mut entry_type = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;
    let mut only_show_in = None;
    let mut name = None;
    let mut name_zh_cn = None;
    let mut generic_name = None;
    let mut exec = None;
    let mut icon = None;
    let mut categories = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "Type" => entry_type = Some(v.to_string()),
            "NoDisplay" => no_display = v.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = v.eq_ignore_ascii_case("true"),
            "Terminal" => terminal = v.eq_ignore_ascii_case("true"),
            "OnlyShowIn" => only_show_in = Some(v.to_string()),
            "Name" => name = Some(v.to_string()),
            "Name[zh_CN]" => name_zh_cn = Some(v.to_string()),
            "GenericName" => generic_name = Some(v.to_string()),
            "Exec" => exec = Some(v.to_string()),
            "Icon" => icon = Some(v.to_string()),
            "Categories" => categories = Some(v.to_string()),
            _ => {}
        }
    }

    if entry_type.as_deref() != Some("Application") || no_display || hidden || terminal {
        return None;
    }
    if let Some(ref osi) = only_show_in {
        let allowed = osi
            .split(';')
            .any(|s| matches!(s.trim(), "GNOME" | "Ubuntu" | "ubuntu"));
        if !allowed {
            return None;
        }
    }
    let name = name?;
    let exec_str = exec?;
    let tokens = split_desktop_exec(&exec_str);
    let mut iter = tokens.into_iter().filter(|t| {
        !matches!(
            t.as_str(),
            "%u" | "%U" | "%f" | "%F" | "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m"
        )
    });
    let cmd = iter.next()?;
    let args: Vec<String> = iter.collect();
    Some(DesktopEntryParsed {
        name,
        name_zh_cn,
        generic_name,
        cmd,
        args,
        icon,
        categories,
    })
}

fn split_desktop_exec(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escape = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn shell_quote_desktop(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for ch in arg.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_desktop_entry_and_strips_field_codes() {
        let desktop = r#"
[Desktop Entry]
Type=Application
Name=Chromium Web Browser
Name[zh_CN]=Chromium 网页浏览器
Exec=/snap/bin/chromium %U
Icon=/snap/chromium/current/chromium.png
Terminal=false
Categories=Network;WebBrowser;

[Desktop Action new-window]
Name=New Window
Exec=/snap/bin/chromium
"#;
        let parsed = parse_desktop_entry(desktop).expect("should parse Application entry");
        assert_eq!(parsed.name, "Chromium Web Browser");
        assert_eq!(parsed.name_zh_cn.as_deref(), Some("Chromium 网页浏览器"));
        assert_eq!(parsed.cmd, "/snap/bin/chromium");
        assert!(parsed.args.is_empty());
        assert_eq!(parsed.icon.as_deref(), Some("/snap/chromium/current/chromium.png"));
    }

    #[test]
    fn skips_terminal_and_nodisplay_entries() {
        let term = "[Desktop Entry]\nType=Application\nName=Vim\nExec=vim %F\nTerminal=true\n";
        assert!(parse_desktop_entry(term).is_none());

        let hidden = "[Desktop Entry]\nType=Application\nName=Helper\nExec=helper\nNoDisplay=true\n";
        assert!(parse_desktop_entry(hidden).is_none());
    }
}
