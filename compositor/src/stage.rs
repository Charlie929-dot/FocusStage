use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::time::Instant;

use smithay::input::keyboard::{xkb, Keycode, ModifiersState};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::utils::{IsAlive, Logical, Point, Size};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::xwayland::xwm::XwmId;
use smithay::xwayland::{X11Surface, XWayland};

use crate::protocol::{DisplayState, InputOwner, SavedRect, StageInfo, StageKind};

#[derive(Debug, Clone)]
pub struct Lease {
    pub lease_id: String,
    pub agent_name: Option<String>,
    pub expires_at: Instant,
}

impl Lease {
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

#[derive(Debug, Clone)]
pub enum StageToplevel {
    Wayland(ToplevelSurface),
    X11(X11Surface),
}

impl StageToplevel {
    pub fn wl_surface(&self) -> Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface> {
        match self {
            StageToplevel::Wayland(t) => Some(t.wl_surface().clone()),
            StageToplevel::X11(x) => x.wl_surface(),
        }
    }

    pub fn alive(&self) -> bool {
        match self {
            StageToplevel::Wayland(t) => t.alive(),
            StageToplevel::X11(x) => x.alive(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StageInputTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl StageInputTarget {
    pub fn wl_surface(&self) -> Option<WlSurface> {
        match self {
            StageInputTarget::Wayland(s) => Some(s.clone()),
            StageInputTarget::X11(x) => x.wl_surface(),
        }
    }

    pub fn alive(&self) -> bool {
        match self {
            StageInputTarget::Wayland(s) => s.alive(),
            StageInputTarget::X11(x) => x.alive() && x.wl_surface().is_some_and(|s| s.alive()),
        }
    }

    pub fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (StageInputTarget::Wayland(a), StageInputTarget::Wayland(b)) => a == b,
            (StageInputTarget::X11(a), StageInputTarget::X11(b)) => a == b,
            _ => false,
        }
    }
}

pub struct StageInputState {
    pub xkb_context: xkb::Context,
    pub xkb_keymap: xkb::Keymap,
    pub xkb_state: xkb::State,
    pub mods_state: ModifiersState,
    /// Evdev keycodes (raw = xkb_keycode - 8) currently pressed on this Stage
    pub pressed_keys: HashSet<u32>,
    /// Pointer coordinates in Stage-logical pixels (origin = main window top-left)
    pub pointer_pos: Point<f64, Logical>,
    /// Evdev mouse button codes currently pressed on this Stage
    pub pressed_buttons: HashSet<u32>,
    /// Current surface holding `wl_keyboard` focus on this Stage
    pub current_keyboard_target: Option<StageInputTarget>,
    /// Current surface holding `wl_pointer` focus on this Stage
    pub current_pointer_target: Option<StageInputTarget>,
    /// Whether a temporary custom keymap is currently uploaded to clients
    pub custom_keymap_active: bool,
}

impl StageInputState {
    pub fn new() -> Self {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile default US XKB keymap");
        let state = xkb::State::new(&keymap);
        let mut mods_state = ModifiersState::default();
        mods_state.update_with(&state);

        Self {
            xkb_context: context,
            xkb_keymap: keymap,
            xkb_state: state,
            mods_state,
            pressed_keys: HashSet::new(),
            pointer_pos: Point::from((10.0, 10.0)),
            pressed_buttons: HashSet::new(),
            current_keyboard_target: None,
            current_pointer_target: None,
            custom_keymap_active: false,
        }
    }

    pub fn update_key(&mut self, evdev_code: u32, pressed: bool) -> (Keycode, ModifiersState) {
        let xkb_code = Keycode::new(evdev_code + 8);
        let direction = if pressed {
            self.pressed_keys.insert(evdev_code);
            xkb::KeyDirection::Down
        } else {
            self.pressed_keys.remove(&evdev_code);
            xkb::KeyDirection::Up
        };
        self.xkb_state.update_key(xkb_code, direction);
        self.mods_state.update_with(&self.xkb_state);
        (xkb_code, self.mods_state)
    }

    pub fn reset_xkb_state(&mut self) {
        self.xkb_state = xkb::State::new(&self.xkb_keymap);
        self.pressed_keys.clear();
        self.mods_state.update_with(&self.xkb_state);
    }
}

#[derive(Debug, Default)]
pub struct StageClipboard {
    pub text: Option<String>,
    pub mime_types: Vec<String>,
    pub seq: u64,
    pub client_source: Option<WlDataSource>,
    pub x11_has_selection: bool,
}

pub struct PendingX11Launch {
    pub app: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub cwd: Option<String>,
}

pub struct Stage {
    pub id: String,
    pub app: String,
    pub wm_class: String,
    pub args: Vec<String>,
    pub kind: StageKind,
    pub title: String,
    pub pid: Option<u32>,
    pub child: Option<Child>,
    pub user_data_dir: Option<std::path::PathBuf>,

    pub display: DisplayState,
    pub input_owner: InputOwner,
    pub lease: Option<Lease>,

    pub logical_size: Size<i32, Logical>,
    pub saved_rect: Option<SavedRect>,
    pub scale: f64,

    pub frame_seq: u64,
    pub dirty_for_sink: bool,

    pub wayland_clients: HashMap<ClientId, Client>,
    pub toplevels: Vec<StageToplevel>,

    pub input: StageInputState,
    pub clipboard: StageClipboard,

    /// X11 Stage fields
    pub xwayland: Option<XWayland>,
    pub xwayland_token: Option<RegistrationToken>,
    pub xwayland_client: Option<Client>,
    pub x11_display_number: Option<u32>,
    pub xwm_id: Option<XwmId>,
    pub pending_x11_launch: Option<PendingX11Launch>,
}

impl Stage {
    pub fn derive_wm_class(app: &str) -> String {
        let base = std::path::Path::new(app)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(app)
            .trim()
            .to_ascii_lowercase();
        let slug: String = base
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-');
        if slug.is_empty() {
            return "FocusStage".to_string();
        }

        let dirs = [
            "/usr/share/applications",
            "/usr/local/share/applications",
            "/var/lib/snapd/desktop/applications",
            "/var/lib/flatpak/exports/share/applications",
        ];

        // 1. Exact focusstage-<slug>.desktop
        let exact_fs = format!("focusstage-{slug}");
        let exact_fs_file = format!("{exact_fs}.desktop");
        for dir in &dirs {
            if std::path::Path::new(dir).join(&exact_fs_file).exists() {
                return exact_fs;
            }
        }

        // 2. Any focusstage-*.desktop whose Exec command basename matches `base`
        if let Some(found) = find_desktop_id_by_exec(&dirs, &base, true) {
            return found;
        }

        // 3. Exact original <slug>.desktop
        let exact_orig_file = format!("{slug}.desktop");
        for dir in &dirs {
            if std::path::Path::new(dir).join(&exact_orig_file).exists() {
                return slug.to_string();
            }
        }

        // 4. Any original *.desktop whose Exec command basename matches `base` (e.g. chromium_chromium, debian-xterm)
        if let Some(found) = find_desktop_id_by_exec(&dirs, &base, false) {
            return found;
        }

        slug.to_string()
    }

    pub fn new(
        id: String,
        app: String,
        args: Vec<String>,
        kind: StageKind,
        width: u32,
        height: u32,
        display: DisplayState,
        scale: f64,
    ) -> Self {
        let wm_class = Self::derive_wm_class(&app);
        Self {
            id,
            app: app.clone(),
            wm_class,
            args,
            kind,
            title: app,
            pid: None,
            child: None,
            user_data_dir: None,
            display,
            input_owner: InputOwner::Human,
            lease: None,
            logical_size: Size::from((width as i32, height as i32)),
            saved_rect: Some(SavedRect {
                x: 100,
                y: 100,
                width,
                height,
            }),
            scale,
            frame_seq: 0,
            dirty_for_sink: true,
            wayland_clients: HashMap::new(),
            toplevels: Vec::new(),
            input: StageInputState::new(),
            clipboard: StageClipboard::default(),
            xwayland: None,
            xwayland_token: None,
            xwayland_client: None,
            x11_display_number: None,
            xwm_id: None,
            pending_x11_launch: None,
        }
    }

    pub fn active_toplevel(&self) -> Option<&StageToplevel> {
        self.toplevels.iter().rev().find(|t| t.alive())
    }

    pub fn has_live_surface(&self) -> bool {
        self.toplevels
            .iter()
            .any(|t| t.alive() && t.wl_surface().is_some())
    }

    pub fn bump_frame(&mut self) {
        self.frame_seq = self.frame_seq.saturating_add(1);
        self.dirty_for_sink = true;
    }

    pub fn info(&self) -> StageInfo {
        StageInfo {
            stage_id: self.id.clone(),
            app: self.app.clone(),
            title: self.title.clone(),
            kind: self.kind,
            pid: self.pid,
            display: self.display,
            input_owner: self.input_owner.clone(),
            size: [
                self.logical_size.w.max(1) as u32,
                self.logical_size.h.max(1) as u32,
            ],
            rect: self.saved_rect,
            frame_seq: self.frame_seq,
            has_surface: self.has_live_surface(),
            scale: self.scale,
            wm_class: Some(self.wm_class.clone()),
        }
    }
}

fn find_desktop_id_by_exec(dirs: &[&str], target_cmd_lower: &str, only_focusstage: bool) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for dir in dirs {
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
            let is_fs = stem.starts_with("focusstage-");
            if only_focusstage != is_fs {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut in_main = false;
            let mut exec_line = None;
            let mut no_display = false;
            for raw in text.lines() {
                let line = raw.trim();
                if line.starts_with('[') && line.ends_with(']') {
                    in_main = line == "[Desktop Entry]";
                    continue;
                }
                if !in_main {
                    continue;
                }
                if let Some(val) = line.strip_prefix("NoDisplay=") {
                    if val.trim().eq_ignore_ascii_case("true") {
                        no_display = true;
                    }
                } else if let Some(val) = line.strip_prefix("Exec=") {
                    exec_line = Some(val.trim().to_string());
                }
            }
            if no_display {
                continue;
            }
            let Some(exec_val) = exec_line else {
                continue;
            };
            let tokens = split_exec_tokens(&exec_val);
            let matched = if is_fs {
                extract_focusstage_target_cmd(&tokens)
                    .map(|c| cmd_basename_lower(c) == target_cmd_lower)
                    .unwrap_or(false)
            } else {
                tokens
                    .iter()
                    .find(|t| !t.starts_with('%') && *t != "env" && !t.contains('='))
                    .map(|c| cmd_basename_lower(c) == target_cmd_lower)
                    .unwrap_or(false)
            };
            if matched {
                let extra_score = tokens.len();
                let replace = match &best {
                    None => true,
                    Some((prev_score, prev_id)) => {
                        extra_score < *prev_score || (extra_score == *prev_score && stem.len() < prev_id.len())
                    }
                };
                if replace {
                    best = Some((extra_score, stem.to_string()));
                }
            }
        }
    }
    best.map(|(_, id)| id)
}

fn cmd_basename_lower(cmd: &str) -> String {
    std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
        .to_ascii_lowercase()
}

fn extract_focusstage_target_cmd(tokens: &[String]) -> Option<&str> {
    let mut i = 0;
    if tokens.first().map(|t| cmd_basename_lower(t)).as_deref() == Some("focusstage-launch") {
        i = 1;
    } else if tokens.first().map(|t| cmd_basename_lower(t)).as_deref() == Some("focusstage")
        && tokens.get(1).map(|s| s.as_str()) == Some("launch")
    {
        i = 2;
    }
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "--" {
            return tokens.get(i + 1).map(|s| s.as_str());
        }
        if matches!(tok, "--wm-class" | "--kind" | "--width" | "--height" | "--cwd" | "--socket") {
            i += 2;
            continue;
        }
        if tok.starts_with("--") {
            i += 1;
            continue;
        }
        return Some(tok);
    }
    None
}

fn split_exec_tokens(s: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_target_cmd_from_focusstage_exec() {
        let tokens = split_exec_tokens(
            "/usr/local/bin/focusstage-launch --wm-class focusstage-chromium_chromium /snap/bin/chromium",
        );
        assert_eq!(extract_focusstage_target_cmd(&tokens), Some("/snap/bin/chromium"));

        let tokens2 = split_exec_tokens(
            "/usr/local/bin/focusstage-launch --wm-class focusstage-foot-server foot -- --server",
        );
        assert_eq!(extract_focusstage_target_cmd(&tokens2), Some("foot"));
    }
}
