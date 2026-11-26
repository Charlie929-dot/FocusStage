use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use clap::{Parser, Subcommand};
use focusstage_compositor::accounts::{self, AccountAction, AccountApplyOutcome};
use focusstage_compositor::control::ControlClient;
use focusstage_compositor::desktop;
use focusstage_compositor::keyring;
use focusstage_compositor::protocol::{
    default_socket_path, windows_path_to_wsl, Action, ControlRequest, ControlResponse,
    DesktopAppInfo, DisplayState, StageKind,
};
use focusstage_compositor::session_env;
use focusstage_compositor::xdg::{self, SettingsAction};
use focusstage_mcp::{apps::discover_desktop_apps_via_daemon, run_mcp_stdio};

mod uninstall;

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
    /// Ensure focusstaged is running and ready on the control socket
    EnsureDaemon {
        /// Native Windows host executable path (WSL or Windows path)
        #[arg(long)]
        native_host: Option<String>,
    },
    /// Ensure the FocusStage-hosted Polkit authentication agent is ready
    EnsurePolkitAgent,
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
        #[arg(long, default_value_t = false)]
        as_root: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Open an interactive root terminal Stage (authenticated via Polkit)
    RootTerminal,
    #[command(hide = true)]
    RootExec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    RootShell,
    /// Route one or more links or local files through FocusStage
    Open {
        #[arg(long)]
        ensure_daemon: bool,
        #[arg(long)]
        handler: Option<String>,
        #[arg(required = true)]
        uris: Vec<String>,
    },
    /// Inspect or change user-level Linux desktop preferences
    Settings {
        #[command(subcommand)]
        action: SettingsCommand,
    },
    /// Inspect or manage WSL Linux user accounts and default WSL user
    Accounts {
        #[command(subcommand)]
        action: AccountsCommand,
    },
    /// Inspect, unlock, or lock the default GNOME keyring
    Keyring {
        #[command(subcommand)]
        action: KeyringCommand,
    },
    /// List discovered Linux GUI applications from .desktop entries
    ListApps,
    /// Uninstall a Linux application or delete a downloaded installer
    UninstallApp {
        #[arg(long)]
        app_id: String,
    },
    /// Generate FocusStage wrapper .desktop entries for Linux GUI apps
    SyncApps {
        /// Remove previously generated focusstage-*.desktop wrappers
        #[arg(long, default_value_t = false)]
        remove: bool,
        /// Target applications directory (defaults to /usr/share/applications for root)
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Read discovered apps JSON ({"apps": [...]} or [...]) from stdin instead of scanning
        #[arg(long, default_value_t = false)]
        from_stdin: bool,
        /// Scan using the specified Ubuntu user ID when invoked as root
        #[arg(long)]
        uid: Option<u32>,
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

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    Get,
    InitDefaults,
    SetBrowser { id: String },
    SetTerminal { id: String },
    SyncHandlers,
    RemoveHandlers,
    SetColorScheme { value: String },
    SetAccentColor { value: String },
    SetGtkTheme { id: String },
    SetIconTheme { id: String },
    SetCursorTheme { id: String },
    SetFontName { value: String },
    SetTextScale { value: String },
    ResetTheme,
    SetHideWsl { enabled: String },
}

#[derive(Debug, Subcommand)]
enum AccountsCommand {
    Status,
    VerifyPassword {
        #[arg(long)]
        username: String,
    },
    Apply,
    ApplyStdin,
}

#[derive(Debug, Subcommand)]
enum KeyringCommand {
    Status,
    Unlock,
    Lock,
}

fn parse_kind(s: &str) -> anyhow::Result<StageKind> {
    match s.to_ascii_lowercase().as_str() {
        "wayland" => Ok(StageKind::Wayland),
        "chromium" => Ok(StageKind::Chromium),
        "electron" => Ok(StageKind::Electron),
        "gecko" => Ok(StageKind::Gecko),
        "x11" => Ok(StageKind::X11),
        _ => anyhow::bail!("Invalid stage kind: {s}"),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Commands::RootExec { .. }) {
        return focusstage_compositor::root_exec::run_from_stdin().map_err(anyhow::Error::msg);
    }
    if matches!(cli.command, Commands::RootShell) {
        return focusstage_compositor::root_exec::run_root_shell().map_err(anyhow::Error::msg);
    }

    session_env::ensure_runtime_env_for_current_user();
    let sock = cli.socket.unwrap_or_else(default_socket_path);

    if matches!(cli.command, Commands::Mcp) {
        return run_mcp_stdio(sock);
    }

    if let Commands::EnsureDaemon { native_host } = cli.command {
        let host_path = native_host
            .as_deref()
            .map(|p| PathBuf::from(windows_path_to_wsl(p)));
        ensure_focusstaged_running(&sock, host_path.as_deref())?;
        let resp = ControlClient::new(sock).call(&ControlRequest::Ping)?;
        print_resp(&resp, cli.json);
        return Ok(());
    }

    if matches!(cli.command, Commands::EnsurePolkitAgent) {
        let client = ControlClient::new(sock);
        let mut last = ControlResponse::PolkitAgent { ready: false };
        for attempt in 0..60 {
            last = client.call(&ControlRequest::EnsurePolkitAgent)?;
            if matches!(last, ControlResponse::PolkitAgent { ready: true }) {
                break;
            }
            if attempt < 59 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        print_resp(&last, cli.json);
        return Ok(());
    }

    if matches!(cli.command, Commands::ListApps) {
        return run_list_apps(&sock, cli.json);
    }

    if let Commands::UninstallApp { app_id } = &cli.command {
        session_env::init_cli_environment(None);
        return uninstall::run(app_id, &sock, cli.json);
    }

    if let Commands::SyncApps {
        remove,
        out_dir,
        from_stdin,
        uid,
    } = cli.command
    {
        let target_dir = out_dir.unwrap_or_else(default_sync_apps_dir);
        return run_sync_apps(&sock, &target_dir, remove, from_stdin, uid, cli.json);
    }

    if let Commands::Accounts { action } = cli.command {
        return run_accounts_command(action, &sock, cli.json);
    }

    if let Commands::Keyring { action } = cli.command {
        return run_keyring_command(action, cli.json);
    }

    if let Commands::Settings { action } = cli.command {
        let action = match action {
            SettingsCommand::Get => SettingsAction::Get,
            SettingsCommand::InitDefaults => SettingsAction::InitDefaults,
            SettingsCommand::SetBrowser { id } => SettingsAction::SetBrowser(id),
            SettingsCommand::SetTerminal { id } => SettingsAction::SetTerminal(id),
            SettingsCommand::SyncHandlers => SettingsAction::SyncHandlers,
            SettingsCommand::RemoveHandlers => SettingsAction::RemoveHandlers,
            SettingsCommand::SetColorScheme { value } => SettingsAction::SetColorScheme(value),
            SettingsCommand::SetAccentColor { value } => SettingsAction::SetAccentColor(value),
            SettingsCommand::SetGtkTheme { id } => SettingsAction::SetGtkTheme(id),
            SettingsCommand::SetIconTheme { id } => SettingsAction::SetIconTheme(id),
            SettingsCommand::SetCursorTheme { id } => SettingsAction::SetCursorTheme(id),
            SettingsCommand::SetFontName { value } => SettingsAction::SetFontName(value),
            SettingsCommand::SetTextScale { value } => SettingsAction::SetTextScale(value),
            SettingsCommand::ResetTheme => SettingsAction::ResetTheme,
            SettingsCommand::SetHideWsl { enabled } => {
                let flag = match enabled.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "on" | "yes" => true,
                    "false" | "0" | "off" | "no" => false,
                    _ => anyhow::bail!("Invalid boolean value for set-hide-wsl: {enabled}"),
                };
                SettingsAction::SetHideWsl(flag)
            }
        };
        let view = if !matches!(action, SettingsAction::RemoveHandlers) {
            match ControlClient::new(&sock).call(&ControlRequest::Settings {
                action: action.clone(),
            }) {
                Ok(ControlResponse::Settings { view }) if !view.theme.color_scheme.is_empty() => Some(view),
                Ok(ControlResponse::Settings { .. }) => None,
                Ok(ControlResponse::Error { code, .. }) if code == "bad_request" => None,
                Ok(ControlResponse::Error { message, .. }) => anyhow::bail!("{message}"),
                Ok(other) => anyhow::bail!("Unexpected response from focusstaged: {:?}", other),
                Err(_) => None,
            }
        } else {
            None
        };
        let view = match view {
            Some(v) => v,
            None => {
                session_env::init_cli_environment(None);
                xdg::settings(action)?
            }
        };
        println!(
            "{}",
            if cli.json {
                serde_json::to_string(&view)?
            } else {
                serde_json::to_string_pretty(&view)?
            }
        );
        return Ok(());
    }

    if matches!(cli.command, Commands::RootTerminal) {
        session_env::init_cli_environment(None);
        ensure_focusstaged_running(&sock, None)?;
        let client = ControlClient::new(sock);
        for attempt in 0..40 {
            if matches!(
                client.call(&ControlRequest::EnsurePolkitAgent),
                Ok(ControlResponse::PolkitAgent { ready: true })
            ) {
                break;
            }
            if attempt < 39 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let focusstage_bin = std::env::current_exe()
            .ok()
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("/usr/local/bin/focusstage"));
        let (app, args, wm_class, run_as_root) = xdg::root_terminal_launch(&focusstage_bin)?;
        let resp = client.call(&ControlRequest::Launch {
            app,
            args,
            kind: None,
            env: Default::default(),
            cwd: None,
            width: None,
            height: None,
            show_on_desktop: Some(true),
            wm_class,
            wait_ready_ms: None,
            launch_image: None,
            launch_wait_for_update: None,
            run_as_root,
        })?;
        let failed = matches!(resp, ControlResponse::Error { .. });
        print_resp(&resp, cli.json);
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }

    if matches!(
        cli.command,
        Commands::Launch {
            ensure_daemon: true,
            ..
        } | Commands::Open {
            ensure_daemon: true,
            ..
        }
    ) {
        ensure_focusstaged_running(&sock, None)?;
    }

    let client = ControlClient::new(sock);
    let req = match cli.command {
        Commands::Mcp
        | Commands::EnsureDaemon { .. }
        | Commands::EnsurePolkitAgent
        | Commands::RootTerminal
        | Commands::RootExec { .. }
        | Commands::RootShell => unreachable!(),
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
            as_root,
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
                wait_ready_ms: None,
                launch_image: None,
                launch_wait_for_update: None,
                run_as_root: as_root,
            }
        }
        Commands::Open { uris, handler, .. } => {
            let mut ok = true;
            for uri in uris {
                let uri = if !uri.contains(':') && !uri.starts_with('/') {
                    std::env::current_dir()?.join(uri).into_os_string().into_string()
                        .map_err(|_| anyhow::anyhow!("Local path is not valid UTF-8"))?
                } else { uri };
                let response = client.call(&ControlRequest::OpenUri { uri, handler: handler.clone() })?;
                ok &= !matches!(response, ControlResponse::Error { .. });
                print_resp(&response, cli.json);
            }
            if !ok { std::process::exit(1); }
            return Ok(());
        }
        Commands::ListApps
        | Commands::SyncApps { .. }
        | Commands::Settings { .. }
        | Commands::Accounts { .. }
        | Commands::Keyring { .. }
        | Commands::UninstallApp { .. } => unreachable!(),
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
                settle: None,
                settle_ms: None,
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

fn read_stdin_secret_line() -> anyhow::Result<String> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_string())
}

fn print_json_value(value: &serde_json::Value, json_mode: bool) {
    if json_mode {
        println!("{value}");
    } else {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    }
}

fn run_accounts_command(
    action: AccountsCommand,
    sock: &Path,
    json_mode: bool,
) -> anyhow::Result<()> {
    match action {
        AccountsCommand::Status => {
            session_env::init_cli_environment(None);
            let status = accounts::accounts_status();
            print_json_value(&serde_json::to_value(&status)?, json_mode);
            Ok(())
        }
        AccountsCommand::VerifyPassword { username } => {
            let password = read_stdin_secret_line()?;
            match accounts::verify_password(&username, &password) {
                Ok(valid) => {
                    let out = serde_json::json!({
                        "status": "ok",
                        "valid": valid,
                        "username": username,
                    });
                    print_json_value(&out, json_mode);
                    Ok(())
                }
                Err(message) => {
                    let out = serde_json::json!({
                        "status": "error",
                        "message": message,
                    });
                    print_json_value(&out, json_mode);
                    std::process::exit(1);
                }
            }
        }
        AccountsCommand::Apply => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let parsed: AccountAction = serde_json::from_str(input.trim())?;
            match accounts::run_privileged_action(sock, &parsed) {
                Ok(outcome) => {
                    print_json_value(&serde_json::to_value(&outcome)?, json_mode);
                    Ok(())
                }
                Err(message) => {
                    let outcome = AccountApplyOutcome {
                        status: "error".into(),
                        message,
                        username: None,
                    };
                    print_json_value(&serde_json::to_value(&outcome)?, json_mode);
                    std::process::exit(1);
                }
            }
        }
        AccountsCommand::ApplyStdin => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let parsed: AccountAction = serde_json::from_str(input.trim())?;
            match accounts::apply_action_as_root(parsed) {
                Ok(outcome) => {
                    print_json_value(&serde_json::to_value(&outcome)?, json_mode);
                    Ok(())
                }
                Err(message) => {
                    let outcome = AccountApplyOutcome {
                        status: "error".into(),
                        message,
                        username: None,
                    };
                    print_json_value(&serde_json::to_value(&outcome)?, json_mode);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn run_keyring_command(action: KeyringCommand, json_mode: bool) -> anyhow::Result<()> {
    session_env::init_cli_environment(None);
    match action {
        KeyringCommand::Status => {
            let status = keyring::query_keyring_status();
            print_json_value(&serde_json::to_value(&status)?, json_mode);
            Ok(())
        }
        KeyringCommand::Unlock => {
            let password = read_stdin_secret_line()?;
            match keyring::unlock_default_keyring(&password) {
                Ok(true) => {
                    print_json_value(
                        &serde_json::json!({ "status": "ok", "unlocked": true }),
                        json_mode,
                    );
                    Ok(())
                }
                Ok(false) => {
                    print_json_value(
                        &serde_json::json!({
                            "status": "error",
                            "unlocked": false,
                            "message": "密钥环密码不正确，请检查后重试。"
                        }),
                        json_mode,
                    );
                    std::process::exit(1);
                }
                Err(message) => {
                    print_json_value(
                        &serde_json::json!({
                            "status": "error",
                            "unlocked": false,
                            "message": message
                        }),
                        json_mode,
                    );
                    std::process::exit(1);
                }
            }
        }
        KeyringCommand::Lock => match keyring::lock_default_keyring() {
            Ok(()) => {
                print_json_value(
                    &serde_json::json!({ "status": "ok", "locked": true }),
                    json_mode,
                );
                Ok(())
            }
            Err(message) => {
                print_json_value(
                    &serde_json::json!({
                        "status": "error",
                        "message": message
                    }),
                    json_mode,
                );
                std::process::exit(1);
            }
        },
    }
}

fn ensure_focusstaged_running(sock: &std::path::Path, native_host: Option<&Path>) -> anyhow::Result<()> {
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
        PathBuf::from(format!("/tmp/focusstaged-{}.log", session_env::current_uid()))
    };

    let home = session_env::UserAccount::lookup(session_env::current_uid()).map(|a| a.home);
    let normalized_path = session_env::build_normalized_path(
        std::env::var("PATH").ok().as_deref(),
        home.as_deref(),
    );
    let started_via_systemd = if session_env::systemd_user_available() {
        let mut sd = std::process::Command::new("systemd-run");
        sd.args(["--user", "--collect"])
            .args(["-E", &format!("PATH={normalized_path}")])
            .args([
                "-p",
                &format!("StandardOutput=append:{}", effective_log_path.display()),
                "-p",
                &format!("StandardError=append:{}", effective_log_path.display()),
            ]);
        for &key in session_env::caller_runtime_bridge_keys() {
            if let Some(val) = session_env::os_var_nonempty(key) {
                let val_str = val.to_string_lossy();
                if key == "WAYLAND_DISPLAY" && val_str.starts_with("focusstage-") {
                    continue;
                }
                sd.args(["-E", &format!("{key}={val_str}")]);
            }
        }
        sd.arg(&exe).arg("--socket").arg(sock);
        if let Some(host) = native_host {
            if !host.as_os_str().is_empty() {
                sd.arg("--native-host").arg(host);
            }
        }
        sd.stdin(std::process::Stdio::null())
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
            .arg(sock);
        if let Some(host) = native_host {
            if !host.as_os_str().is_empty() {
                cmd.arg("--native-host").arg(host);
            }
        }
        cmd.stdin(std::process::Stdio::null())
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

    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if matches!(client.call(&ControlRequest::Ping), Ok(ControlResponse::Pong { .. })) {
            return Ok(());
        }
    }
    anyhow::bail!("Timed out waiting for focusstaged on {}", sock.display());
}

fn default_sync_apps_dir() -> PathBuf {
    if session_env::current_uid() == 0 {
        PathBuf::from("/usr/share/applications")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/applications")
    } else {
        PathBuf::from("/usr/share/applications")
    }
}

fn run_list_apps(sock: &Path, json_mode: bool) -> anyhow::Result<()> {
    let client = ControlClient::new(sock.to_path_buf());
    let resp = match client.call(&ControlRequest::ListApps) {
        Ok(resp) => resp,
        Err(_) => {
            session_env::init_cli_environment(None);
            let apps = desktop::discover_launcher_apps(true);
            ControlResponse::Apps {
                count: apps.len(),
                apps,
            }
        }
    };
    print_resp(&resp, json_mode);
    if matches!(resp, ControlResponse::Error { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

fn parse_sync_apps_json(raw: &str) -> anyhow::Result<Vec<DesktopAppInfo>> {
    #[derive(serde::Deserialize)]
    struct AppsEnvelope {
        apps: Vec<DesktopAppInfo>,
    }
    if let Ok(envelope) = serde_json::from_str::<AppsEnvelope>(raw) {
        return Ok(envelope.apps);
    }
    Ok(serde_json::from_str::<Vec<DesktopAppInfo>>(raw)?)
}

fn resolve_sync_apps_uid(explicit_uid: Option<u32>) -> Option<u32> {
    if explicit_uid.is_some() {
        return explicit_uid;
    }
    if session_env::current_uid() == 0 {
        for key in ["SUDO_UID", "PKEXEC_UID"] {
            if let Ok(val) = std::env::var(key) {
                if let Ok(uid) = val.trim().parse::<u32>() {
                    if uid != 0 {
                        return Some(uid);
                    }
                }
            }
        }
    }
    None
}

fn run_sync_apps(
    sock: &Path,
    out_dir: &std::path::Path,
    remove: bool,
    from_stdin: bool,
    uid: Option<u32>,
    json_mode: bool,
) -> anyhow::Result<()> {
    if remove {
        let mut removed = Vec::new();
        if let Ok(entries) = std::fs::read_dir(out_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with("focusstage-")
                        && !name.starts_with("focusstage-handler-")
                        && name.ends_with(".desktop")
                    {
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

    let discovered = if from_stdin {
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw)?;
        parse_sync_apps_json(&raw)?
    } else if let Some(target_uid) = resolve_sync_apps_uid(uid) {
        session_env::init_cli_environment(Some(target_uid));
        desktop::discover_launcher_apps(false)
    } else {
        let client = ControlClient::new(sock.to_path_buf());
        if let Ok(ControlResponse::Apps { apps, .. }) = client.call(&ControlRequest::ListApps) {
            apps
        } else {
            session_env::init_cli_environment(None);
            discover_desktop_apps_via_daemon(None)
        }
    };

    std::fs::create_dir_all(out_dir)?;
    let mut created = Vec::new();
    let mut kept = std::collections::HashSet::new();
    for app in discovered {
        if app.source != "desktop" {
            continue;
        }
        let wrapper_id = &app.id;
        let filename = format!("{wrapper_id}.desktop");
        kept.insert(filename.clone());
        let wrapper_path = out_dir.join(&filename);
        let mut exec_line = format!(
            "/usr/local/bin/focusstage-launch --wm-class {} {}",
            shell_quote_desktop(wrapper_id),
            shell_quote_desktop(&app.cmd)
        );
        if !app.args.is_empty() {
            exec_line.push_str(" --");
            for a in &app.args {
                exec_line.push(' ');
                exec_line.push_str(&shell_quote_desktop(a));
            }
        }

        let mut out = String::new();
        out.push_str("[Desktop Entry]\n");
        out.push_str("Type=Application\n");
        out.push_str(&format!("Name={} (FocusStage)\n", app.name));
        if let Some(ref zh) = app.name_zh_cn {
            out.push_str(&format!("Name[zh_CN]={zh} (FocusStage)\n"));
        }
        if let Some(ref g) = app.generic_name {
            out.push_str(&format!("GenericName={g}\n"));
        }
        out.push_str(&format!("Exec={exec_line}\n"));
        if let Some(ref icon) = app.icon {
            out.push_str(&format!("Icon={icon}\n"));
        }
        out.push_str("Terminal=false\n");
        if let Some(ref cats) = app.categories {
            out.push_str(&format!("Categories={cats}\n"));
        }
        out.push_str(&format!("StartupWMClass={wrapper_id}\n"));

        std::fs::write(&wrapper_path, out)?;
        created.push(serde_json::json!({
            "id": wrapper_id,
            "name": format!("{} (FocusStage)", app.name),
            "path": wrapper_path.display().to_string(),
            "cmd": app.cmd,
        }));
    }

    let mut removed = Vec::new();
    if let Ok(entries) = std::fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("focusstage-")
                    && !name.starts_with("focusstage-handler-")
                    && name.ends_with(".desktop")
                    && !kept.contains(name)
                    && std::fs::remove_file(&p).is_ok()
                {
                    removed.push(p.display().to_string());
                }
            }
        }
    }

    let payload = serde_json::json!({
        "status": "ok",
        "count": created.len(),
        "entries": created,
        "removed": removed,
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
    use focusstage_compositor::desktop;

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
        let parsed = desktop::parse(desktop).expect("should parse Application entry");
        assert_eq!(parsed.name, "Chromium Web Browser");
        assert_eq!(parsed.name_zh_cn.as_deref(), Some("Chromium 网页浏览器"));
        assert_eq!(parsed.cmd, "/snap/bin/chromium");
        assert!(parsed.launch_args().is_empty());
        assert_eq!(parsed.icon.as_deref(), Some("/snap/chromium/current/chromium.png"));
    }

    #[test]
    fn skips_terminal_and_nodisplay_entries() {
        let term = "[Desktop Entry]\nType=Application\nName=Vim\nExec=vim %F\nTerminal=true\n";
        assert!(desktop::parse(term).is_none());

        let hidden = "[Desktop Entry]\nType=Application\nName=Helper\nExec=helper\nNoDisplay=true\n";
        assert!(desktop::parse(hidden).unwrap().no_display);
    }

    #[test]
    fn parses_sync_apps_envelope_and_array() {
        let envelope = r#"{"status":"apps","count":1,"apps":[{"id":"focusstage-foo","stem":"foo","source":"desktop","name":"Foo","cmd":"/usr/bin/foo","args":["--bar"],"wm_class":"focusstage-foo"}]}"#;
        let parsed = super::parse_sync_apps_json(envelope).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "focusstage-foo");
        assert_eq!(parsed[0].args, ["--bar"]);

        let array = r#"[{"id":"focusstage-baz","stem":"baz","source":"appimage","name":"Baz","cmd":"/tmp/Baz.AppImage","wm_class":"focusstage-baz"}]"#;
        let parsed_arr = super::parse_sync_apps_json(array).unwrap();
        assert_eq!(parsed_arr.len(), 1);
        assert_eq!(parsed_arr[0].source, "appimage");
    }
}
