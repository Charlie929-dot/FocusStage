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
        #[arg(last = true)]
        args: Vec<String>,
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
            args,
        } => ControlRequest::Launch {
            app,
            args,
            kind: kind.as_deref().map(parse_kind).transpose()?,
            env: Default::default(),
            cwd,
            width,
            height,
            show_on_desktop: Some(!hidden),
        },
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
