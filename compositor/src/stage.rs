use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::time::{Duration, Instant};

use smithay::input::keyboard::{xkb, Keycode, ModifiersState};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{IsAlive, Logical, Point, Size};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::xwayland::xwm::XwmId;
use smithay::xwayland::{X11Surface, XWayland};

use crate::protocol::{DisplayState, InputOwner, SavedRect, StageInfo, StageKind};

#[derive(Debug, Clone)]
pub struct Lease {
    pub lease_id: String,
    pub agent_name: Option<String>,
    pub ttl: Duration,
    pub expires_at: Instant,
}

impl Lease {
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    pub fn refresh(&mut self) {
        self.expires_at = Instant::now() + self.ttl;
    }
}

#[derive(Debug, Clone, PartialEq)]
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

    /// The window connection's PID, independent of wrapper processes or launch tokens.
    pub(crate) fn peer_pid(&self) -> Option<u32> {
        if !self.alive() {
            return None;
        }
        match self {
            Self::Wayland(toplevel) => toplevel.wl_surface().client()?
                .get_data::<crate::state::ClientState>()?.pid,
            Self::X11(window) => window.get_client_pid().ok().filter(|&pid| pid > 1),
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
    /// Position of the focused Wayland surface relative to the window.
    pub pointer_surface_origin: Point<f64, Logical>,
    pub pointer_window_id: Option<String>,
    /// Evdev mouse button codes currently pressed on this Stage
    pub pressed_buttons: HashSet<u32>,
    /// Current surface holding `wl_keyboard` focus on this Stage
    pub current_keyboard_target: Option<StageInputTarget>,
    /// Current surface holding `wl_pointer` focus on this Stage
    pub current_pointer_target: Option<StageInputTarget>,
    /// Whether a temporary custom keymap is currently uploaded to clients
    pub custom_keymap_active: bool,
    /// Whether the AI virtual cursor should be drawn on desktop overlay windows
    pub agent_pointer_visible: bool,
    /// Current AI virtual cursor visual action state ("idle" | "move" | "down" | "click" | "drag" | "scroll")
    pub agent_pointer_state: &'static str,
    /// Tracks whether XWayland's active master-pointer slave is already the axis device (`xwayland-relative-pointer`).
    pub x11_axis_primed: bool,
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
            pointer_surface_origin: Point::default(),
            pointer_window_id: None,
            pressed_buttons: HashSet::new(),
            current_keyboard_target: None,
            current_pointer_target: None,
            custom_keymap_active: false,
            agent_pointer_visible: false,
            agent_pointer_state: "idle",
            x11_axis_primed: false,
        }
    }

    pub fn reset_agent_pointer(&mut self) {
        self.agent_pointer_visible = false;
        self.agent_pointer_state = "idle";
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    ImagePng(Vec<u8>),
}

impl ClipboardContent {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s.as_str()),
            Self::ImagePng(_) => None,
        }
    }

    pub fn mime_types(&self) -> Vec<String> {
        match self {
            Self::Text(_) => vec![
                "text/plain;charset=utf-8".to_string(),
                "UTF8_STRING".to_string(),
                "text/plain".to_string(),
            ],
            Self::ImagePng(_) => vec!["image/png".to_string()],
        }
    }

    pub fn x11_mime_types(&self) -> Vec<String> {
        match self {
            Self::Text(_) => vec![
                "UTF8_STRING".to_string(),
                "text/plain;charset=utf-8".to_string(),
                "text/plain".to_string(),
                "STRING".to_string(),
            ],
            Self::ImagePng(_) => vec!["image/png".to_string()],
        }
    }
}

#[derive(Debug, Default)]
pub struct StageClipboard {
    pub content: Option<ClipboardContent>,
    pub mime_types: Vec<String>,
    pub seq: u64,
    pub client_source: Option<WlDataSource>,
    pub x11_has_selection: bool,
}

impl StageClipboard {
    pub fn text(&self) -> Option<String> {
        self.content.as_ref().and_then(|c| c.text().map(str::to_owned))
    }
}

pub struct StageWindow {
    pub id: String,
    pub toplevel: StageToplevel,
    pub title: String,
    pub logical_size: Size<i32, Logical>,
    pub saved_rect: SavedRect,
    pub scale: f64,
    pub parent_window_id: Option<String>,
    pub monitor: Option<SavedRect>,
    pub pixel_size: Option<[u32; 2]>,
    pub maximized: bool,
    pub host_close_requested: bool,
    pub mapped: bool,
    pub dirty: bool,
    pub(crate) content_seq: u64,
}

impl StageWindow {
    pub(crate) fn set_mapped_size(&mut self, size: Size<i32, Logical>) {
        self.logical_size = size;
        self.saved_rect.width = size.w.max(1) as u32;
        self.saved_rect.height = size.h.max(1) as u32;
        self.pixel_size = None;
    }
}

pub struct Stage {
    pub id: String,
    pub app: String,
    pub wm_class: String,
    desktop_icon_png: OnceCell<Option<String>>,
    pub args: Vec<String>,
    pub kind: StageKind,
    pub pid: Option<u32>,
    pub child: Option<Child>,
    pub(crate) launch_token: Option<String>,
    pub(crate) next_process_check: Option<Instant>,
    pub(crate) launch_env: HashMap<String, String>,
    pub(crate) launch_cwd: Option<String>,

    pub display: DisplayState,
    pub input_owner: InputOwner,
    pub lease: Option<Lease>,
    pub(crate) expired_lease_for_restore: Option<String>,

    pub initial_rect: SavedRect,
    pub default_scale: f64,
    pub frame_seq: u64,
    pub(crate) settle_seq: u64,

    pub wayland_clients: HashMap<ClientId, Client>,
    pub windows: Vec<StageWindow>,
    pub active_window_id: Option<String>,
    pub window_seq: u64,
    pub retire_when_empty: bool,

    pub input: StageInputState,
    pub clipboard: StageClipboard,

    /// X11 Stage fields
    pub xwayland: Option<XWayland>,
    pub xwayland_token: Option<RegistrationToken>,
    pub xwayland_client: Option<Client>,
    pub x11_display_number: Option<u32>,
    pub xwm_id: Option<XwmId>,
    pub(crate) pending_x11_launch: bool,
}

impl Stage {
    pub(crate) fn desktop_icon_png(&self) -> Option<&str> {
        self.desktop_icon_png
            .get_or_init(|| crate::icons::app_icon_png(&self.wm_class))
            .as_deref()
    }

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
            app,
            wm_class,
            desktop_icon_png: OnceCell::new(),
            args,
            kind,
            pid: None,
            child: None,
            launch_token: None,
            next_process_check: None,
            launch_env: HashMap::new(),
            launch_cwd: None,
            display,
            input_owner: InputOwner::Human,
            lease: None,
            expired_lease_for_restore: None,
            initial_rect: SavedRect { x: 100, y: 100, width, height },
            default_scale: scale,
            frame_seq: 0,
            settle_seq: 0,
            wayland_clients: HashMap::new(),
            windows: Vec::new(),
            active_window_id: None,
            window_seq: 0,
            retire_when_empty: false,
            input: StageInputState::new(),
            clipboard: StageClipboard::default(),
            xwayland: None,
            xwayland_token: None,
            xwayland_client: None,
            x11_display_number: None,
            xwm_id: None,
            pending_x11_launch: false,
        }
    }

    pub fn add_window(&mut self, toplevel: StageToplevel, size: Size<i32, Logical>) -> String {
        self.window_seq += 1;
        let id = format!("{}/{}", self.id, self.window_seq);
        let cascade = ((self.window_seq - 1) % 8) as i32 * 24;
        self.windows.push(StageWindow {
            id: id.clone(),
            toplevel,
            title: self.app.clone(),
            logical_size: size,
            saved_rect: SavedRect {
                x: self.initial_rect.x + cascade,
                y: self.initial_rect.y + cascade,
                width: size.w.max(1) as u32,
                height: size.h.max(1) as u32,
            },
            scale: self.default_scale,
            parent_window_id: None,
            monitor: None,
            pixel_size: None,
            maximized: false,
            host_close_requested: false,
            mapped: false,
            dirty: true,
            content_seq: 0,
        });
        id
    }

    pub fn window(&self, id: &str) -> Option<&StageWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_mut(&mut self, id: &str) -> Option<&mut StageWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn active_window(&self) -> Option<&StageWindow> {
        let live = || self.windows.iter().rev().filter(|w| w.toplevel.alive());
        self.active_window_id.as_deref().and_then(|id| self.window(id))
            .filter(|w| w.mapped && w.toplevel.alive())
            .or_else(|| live().find(|w| w.mapped))
            .or_else(|| live().next())
    }

    pub fn active_toplevel(&self) -> Option<&StageToplevel> {
        self.active_window().map(|w| &w.toplevel)
    }

    pub(crate) fn uses_virtual_focus(&self) -> bool {
        self.display == DisplayState::Hidden || self.input_owner != InputOwner::Human
    }

    pub fn input_toplevel(&self, window_id: Option<&str>) -> Option<&StageToplevel> {
        match window_id {
            Some(id) => self.window(id).map(|w| &w.toplevel),
            None => self.active_toplevel(),
        }
    }

    pub fn logical_size(&self) -> Size<i32, Logical> {
        self.active_window().map(|w| w.logical_size)
            .unwrap_or_else(|| (self.initial_rect.width as i32, self.initial_rect.height as i32).into())
    }

    pub fn has_live_surface(&self) -> bool {
        self.windows.iter().any(|w| w.toplevel.alive() && w.toplevel.wl_surface().is_some())
    }

    pub fn bump_frame(&mut self) {
        self.mark_visual_update(None, true);
    }

    pub fn bump_window_frame(&mut self, id: &str) {
        self.mark_visual_update(Some(id), true);
    }

    pub(crate) fn mark_visual_update(&mut self, window_id: Option<&str>, significant: bool) {
        if let Some(id) = window_id {
            let Some(window) = self.window_mut(id) else { return; };
            window.dirty = true;
        } else {
            for window in &mut self.windows {
                window.dirty = true;
            }
        }
        self.frame_seq = self.frame_seq.saturating_add(1);
        // Even a tiny repaint invalidates screenshot-based actions, but need not delay waiting.
        if significant {
            self.settle_seq = self.settle_seq.saturating_add(1);
        }
    }

    pub fn info(&self) -> StageInfo {
        let window = self.active_window();
        let size = self.logical_size();
        StageInfo {
            stage_id: self.id.clone(),
            app: self.app.clone(),
            title: window.map(|w| &w.title).unwrap_or(&self.app).clone(),
            kind: self.kind,
            pid: window.and_then(|w| w.toplevel.peer_pid()).or(self.pid),
            display: self.display,
            input_owner: self.input_owner.clone(),
            size: [size.w.max(1) as u32, size.h.max(1) as u32],
            rect: Some(window.map(|w| w.saved_rect).unwrap_or(self.initial_rect)),
            frame_seq: self.frame_seq,
            has_surface: self.has_live_surface(),
            scale: window.map(|w| w.scale).unwrap_or(self.default_scale),
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
