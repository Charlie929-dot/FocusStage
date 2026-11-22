use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::time::Instant;

use smithay::input::keyboard::{xkb, Keycode, ModifiersState};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::backend::{ClientId, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::Client;
use smithay::utils::{Logical, Point, Size};
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
    /// Surface ObjectIds that already received `wl_keyboard.enter`
    pub keyboard_entered: HashSet<ObjectId>,
    /// Surface ObjectIds that already received `wl_pointer.enter`
    pub pointer_entered: HashSet<ObjectId>,
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
            keyboard_entered: HashSet::new(),
            pointer_entered: HashSet::new(),
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
        Self {
            id,
            app: app.clone(),
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
        }
    }
}
