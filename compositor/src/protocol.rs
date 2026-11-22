use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("FOCUSSTAGE_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("focusstage.sock")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    Desktop,
    Hidden,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputOwner {
    Human,
    Agent {
        lease_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
    },
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Wayland,
    Chromium,
    Electron,
    X11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageInfo {
    pub stage_id: String,
    pub app: String,
    pub title: String,
    pub kind: StageKind,
    pub pid: Option<u32>,
    pub display: DisplayState,
    pub input_owner: InputOwner,
    pub size: [u32; 2],
    pub rect: Option<SavedRect>,
    pub frame_seq: u64,
    pub has_surface: bool,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yNode {
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<A11yBounds>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<A11yNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Move {
        x: f64,
        y: f64,
    },
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: Option<String>,
    },
    DoubleClick {
        x: f64,
        y: f64,
        #[serde(default)]
        button: Option<String>,
    },
    Drag {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        steps: Option<u32>,
    },
    Scroll {
        x: f64,
        y: f64,
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Option<Vec<String>>,
        /// "tap" (default), "press", or "release"
        #[serde(default)]
        state: Option<String>,
    },
    TypeText {
        text: String,
    },
    Wait {
        ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActStatus {
    Ok,
    Stale,
    Preempted,
    Expired,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,
    List,
    Launch {
        app: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        kind: Option<StageKind>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        #[serde(default)]
        show_on_desktop: Option<bool>,
    },
    Close {
        stage_id: String,
    },
    Acquire {
        stage_id: String,
        #[serde(default = "default_ttl")]
        ttl_s: u64,
        #[serde(default = "default_true")]
        hide: bool,
        #[serde(default)]
        agent_name: Option<String>,
    },
    Renew {
        lease: String,
        #[serde(default = "default_ttl")]
        ttl_s: u64,
    },
    Observe {
        #[serde(default)]
        lease: Option<String>,
        #[serde(default)]
        stage_id: Option<String>,
        #[serde(default = "default_true")]
        image: bool,
        #[serde(default)]
        a11y: bool,
    },
    Act {
        lease: String,
        actions: Vec<Action>,
        #[serde(default)]
        based_on: Option<u64>,
    },
    Release {
        lease: String,
        #[serde(default = "default_true")]
        show_on_desktop: bool,
    },
    Takeover {
        #[serde(default)]
        stage_id: Option<String>,
    },
    SetDisplay {
        stage_id: String,
        display: DisplayState,
    },
    SetFrozen {
        stage_id: String,
        frozen: bool,
    },
    ClipboardGet {
        #[serde(default)]
        lease: Option<String>,
        #[serde(default)]
        stage_id: Option<String>,
    },
    ClipboardSet {
        #[serde(default)]
        lease: Option<String>,
        #[serde(default)]
        stage_id: Option<String>,
        text: String,
    },
    SyncHostClipboard {
        text: String,
    },
    GetHostClipboard,
    DropFiles {
        stage_id: String,
        paths: Vec<String>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    Shutdown,
}

fn default_ttl() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    Ok,
    Error {
        code: String,
        message: String,
    },
    Pong {
        version: String,
        wayland_display: String,
        wslg_active: bool,
    },
    Stages {
        stages: Vec<StageInfo>,
    },
    Launched {
        stage_id: String,
        pid: Option<u32>,
    },
    Acquired {
        lease: String,
        stage_id: String,
        expires_in_s: u64,
    },
    Observed {
        stage_id: String,
        frame_seq: u64,
        width: u32,
        height: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        png_base64: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a11y_tree: Option<Vec<A11yNode>>,
    },
    ActResult {
        act_status: ActStatus,
        executed_steps: usize,
        frame_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Clipboard {
        text: Option<String>,
        seq: u64,
    },
}

/// Translate Windows path (e.g. `C:\Users\HP\foo.txt`) to WSL path (`/mnt/c/Users/HP/foo.txt`).
pub fn windows_path_to_wsl(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = path[3..].replace('\\', "/");
        format!("/mnt/{}/{}", drive, rest)
    } else {
        path.to_string()
    }
}

/// Detect StageKind automatically from app executable/name when caller does not specify one.
pub fn infer_stage_kind(app: &str) -> StageKind {
    let base = std::path::Path::new(app)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(app)
        .to_ascii_lowercase();

    if base.contains("chromium") || base.contains("chrome") || base.contains("edge") || base.contains("brave") {
        StageKind::Chromium
    } else if base.contains("code")
        || base.contains("electron")
        || base.contains("obsidian")
        || base.contains("slack")
        || base.contains("discord")
        || base.contains("feishu")
        || base.contains("lark")
    {
        StageKind::Electron
    } else if matches!(
        base.as_str(),
        "xterm" | "xeyes" | "xclock" | "xcalc" | "xlogo" | "rxvt" | "urxvt" | "wechat" | "qq" | "wps"
    ) {
        StageKind::X11
    } else {
        StageKind::Wayland
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_path_translation() {
        assert_eq!(
            windows_path_to_wsl(r"C:\Users\HP\FocusStage\README.md"),
            "/mnt/c/Users/HP/FocusStage/README.md"
        );
        assert_eq!(
            windows_path_to_wsl("D:/work/test.txt"),
            "/mnt/d/work/test.txt"
        );
        assert_eq!(windows_path_to_wsl("/home/scl/file.txt"), "/home/scl/file.txt");
    }

    #[test]
    fn stage_kind_inference() {
        assert_eq!(infer_stage_kind("chromium"), StageKind::Chromium);
        assert_eq!(infer_stage_kind("/usr/bin/google-chrome"), StageKind::Chromium);
        assert_eq!(infer_stage_kind("code"), StageKind::Electron);
        assert_eq!(infer_stage_kind("feishu"), StageKind::Electron);
        assert_eq!(infer_stage_kind("xterm"), StageKind::X11);
        assert_eq!(infer_stage_kind("wechat"), StageKind::X11);
        assert_eq!(infer_stage_kind("foot"), StageKind::Wayland);
    }

    #[test]
    fn control_protocol_roundtrip() {
        let req = ControlRequest::Act {
            lease: "lease-1".into(),
            actions: vec![
                Action::Click {
                    x: 120.0,
                    y: 240.0,
                    button: None,
                },
                Action::TypeText {
                    text: "hello".into(),
                },
            ],
            based_on: Some(42),
        };
        let raw = serde_json::to_string(&req).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&raw).unwrap();
        match parsed {
            ControlRequest::Act {
                lease,
                actions,
                based_on,
            } => {
                assert_eq!(lease, "lease-1");
                assert_eq!(actions.len(), 2);
                assert_eq!(based_on, Some(42));
            }
            other => panic!("unexpected parsed request: {:?}", other),
        }
    }
}

