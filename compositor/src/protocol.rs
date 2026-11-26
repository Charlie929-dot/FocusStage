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
    Gecko,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wm_class: Option<String>,
    #[serde(default)]
    pub run_as_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopAppInfo {
    pub id: String,
    pub stem: String,
    pub source: String,
    #[serde(default)]
    pub file_path: Option<String>,
    pub name: String,
    #[serde(default)]
    pub name_zh_cn: Option<String>,
    #[serde(default)]
    pub generic_name: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub wm_class: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_path: Option<String>,
    #[serde(default)]
    pub categories: Option<String>,
}

impl DesktopAppInfo {
    pub fn is_launchable(&self) -> bool {
        matches!(self.source.as_str(), "desktop" | "appimage")
    }

    pub fn matches_name(&self, matches: impl Fn(&str) -> bool) -> bool {
        matches(&self.name) || self.name_zh_cn.as_deref().is_some_and(matches)
    }
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
        #[serde(default)]
        wm_class: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_ready_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_image: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_wait_for_update: Option<bool>,
        #[serde(default)]
        run_as_root: bool,
    },
    OpenUri {
        uri: String,
        #[serde(default)]
        handler: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settle: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settle_ms: Option<u64>,
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
    DropFilesLeased {
        lease: String,
        paths: Vec<String>,
        #[serde(default)]
        position: Option<[f64; 2]>,
    },
    ListApps,
    EnsurePolkitAgent,
    Settings {
        action: crate::xdg::SettingsAction,
    },
    Shutdown,
}

fn default_ttl() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

fn default_scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchFrame {
    pub frame_seq: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub png_base64: String,
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
        #[serde(alias = "wslg_active")]
        desktop_active: bool,
    },
    Stages {
        stages: Vec<StageInfo>,
    },
    Launched {
        stage_id: String,
        pid: Option<u32>,
    },
    LaunchReady {
        stage_id: String,
        pid: Option<u32>,
        ready: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<LaunchFrame>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launcher_exit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observe_message: Option<String>,
    },
    Opened {
        stage_id: Option<String>,
        reused: bool,
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
        #[serde(default = "default_scale")]
        scale: f64,
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
    Apps {
        count: usize,
        apps: Vec<DesktopAppInfo>,
    },
    Settings {
        view: crate::xdg::SettingsView,
    },
    PolkitAgent {
        ready: bool,
    },
}

/// Translate a Windows path (e.g. `C:\work\foo.txt`) to its WSL path (`/mnt/c/work/foo.txt`).
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

    if ["firefox", "librewolf", "waterfox"].iter().any(|name| base.starts_with(name)) {
        StageKind::Gecko
    } else if base.contains("chromium") || base.contains("chrome") || base.contains("edge") || base.contains("brave") {
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
            windows_path_to_wsl(r"C:\work\project\README.md"),
            "/mnt/c/work/project/README.md"
        );
        assert_eq!(
            windows_path_to_wsl("D:/work/test.txt"),
            "/mnt/d/work/test.txt"
        );
        assert_eq!(windows_path_to_wsl("/home/user/file.txt"), "/home/user/file.txt");
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
    fn legacy_act_request_omits_settle_options() {
        let request: ControlRequest =
            serde_json::from_str(r#"{"op":"act","lease":"lease-1","actions":[]}"#).unwrap();
        assert!(matches!(request, ControlRequest::Act { settle: None, settle_ms: None, .. }));
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
            settle: Some(false),
            settle_ms: Some(350),
        };
        let raw = serde_json::to_string(&req).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&raw).unwrap();
        match parsed {
            ControlRequest::Act {
                lease,
                actions,
                based_on,
                settle,
                settle_ms,
            } => {
                assert_eq!(lease, "lease-1");
                assert_eq!(actions.len(), 2);
                assert_eq!(based_on, Some(42));
                assert_eq!(settle, Some(false));
                assert_eq!(settle_ms, Some(350));
            }
            other => panic!("unexpected parsed request: {:?}", other),
        }
    }

    #[test]
    fn session_protocol_roundtrip() {
        let req = ControlRequest::Settings {
            action: crate::xdg::SettingsAction::SetBrowser("firefox".into()),
        };
        let raw = serde_json::to_string(&req).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&raw).unwrap();
        assert!(matches!(
            parsed,
            ControlRequest::Settings {
                action: crate::xdg::SettingsAction::SetBrowser(ref id)
            } if id == "firefox"
        ));

        let theme_req = ControlRequest::Settings {
            action: crate::xdg::SettingsAction::SetColorScheme("prefer-dark".into()),
        };
        let theme_raw = serde_json::to_string(&theme_req).unwrap();
        let theme_parsed: ControlRequest = serde_json::from_str(&theme_raw).unwrap();
        assert!(matches!(
            theme_parsed,
            ControlRequest::Settings {
                action: crate::xdg::SettingsAction::SetColorScheme(ref v)
            } if v == "prefer-dark"
        ));

        let resp = ControlResponse::Apps {
            count: 1,
            apps: vec![DesktopAppInfo {
                id: "focusstage-code".into(),
                stem: "code".into(),
                source: "desktop".into(),
                file_path: Some("/usr/share/applications/code.desktop".into()),
                name: "VS Code".into(),
                name_zh_cn: None,
                generic_name: None,
                cmd: "/usr/bin/code".into(),
                args: vec!["--new-window".into()],
                wm_class: "focusstage-code".into(),
                icon: Some("vscode".into()),
                icon_path: None,
                categories: Some("Development;".into()),
            }],
        };
        let resp_json = serde_json::to_value(&resp).unwrap();
        assert_eq!(resp_json["status"], "apps");
        assert_eq!(resp_json["count"], 1);
        assert_eq!(resp_json["apps"][0]["id"], "focusstage-code");
    }
}

