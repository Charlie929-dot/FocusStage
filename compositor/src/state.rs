use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::channel as calloop_channel;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::XwmId;
use smithay::xwayland::{X11Surface, X11Wm, XWaylandClientData};
use smithay::{
    delegate_compositor, delegate_output, delegate_seat, delegate_shm,
    delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_decoration,
    delegate_xdg_shell, delegate_xwayland_shell,
};
use tracing::{debug, info};

use crate::a11y::A11yCollector;
use crate::clipboard::StageDataDeviceManagerState;
use crate::offscreen::OffscreenCore;
use crate::protocol::{DisplayState, StageKind};
use crate::router::{PendingActJob, StageTextInputManagerState};
use crate::sink::wslg::WslgSink;
use crate::sink::wslg_wayland::WslgWaylandSink;
use crate::sink::DisplaySink;
use crate::stage::{Stage, StageToplevel};

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub stage_id: Option<String>,
    pub pid: Option<u32>,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        debug!("Wayland client initialized: {:?} (stage={:?})", client_id, self.stage_id);
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        debug!("Wayland client disconnected: {:?} ({:?})", client_id, reason);
    }
}

pub struct FocusStageState {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, FocusStageState>,
    pub wayland_socket_name: String,
    pub runtime_base_dir: PathBuf,
    pub default_scale: f64,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub output_manager_state: OutputManagerState,
    pub viewporter_state: ViewporterState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub xwayland_shell_state: XWaylandShellState,

    pub seat: Seat<Self>,
    pub popups: PopupManager,
    pub offscreen: OffscreenCore,

    pub clipboard_state: StageDataDeviceManagerState,
    pub clipboard_update_tx: calloop_channel::Sender<(String, String)>,
    pub text_input_state: StageTextInputManagerState,

    pub stages: HashMap<String, Stage>,
    pub next_stage_seq: u64,
    pub next_lease_seq: u64,

    pub xwms: HashMap<XwmId, X11Wm>,
    pub xwm_to_stage: HashMap<XwmId, String>,
    pub x11_override_windows: HashMap<XwmId, Vec<X11Surface>>,

    pub pending_act_jobs: Vec<PendingActJob>,
    pub a11y: A11yCollector,
    pub wslg_sink: Option<Box<dyn DisplaySink>>,
    pub wslg_time_anchor: Option<(u32, u32)>,
    pub should_quit: bool,
}

impl FocusStageState {
    pub fn new(
        dh: &DisplayHandle,
        loop_handle: LoopHandle<'static, FocusStageState>,
        wayland_socket_name: String,
        runtime_base_dir: PathBuf,
        default_scale: f64,
        frame_hz: u32,
        enable_wslg: bool,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(dh);
        let xdg_shell_state = XdgShellState::new::<Self>(dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(dh);
        let shm_state = ShmState::new::<Self>(dh, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "seat-0");
        let _ = seat.add_keyboard(Default::default(), 200, 25);
        let _ = seat.add_pointer();

        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(dh);
        let viewporter_state = ViewporterState::new::<Self>(dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(dh);
        let clipboard_state = StageDataDeviceManagerState::new(dh);
        let text_input_state = StageTextInputManagerState::new(dh);
        let offscreen = OffscreenCore::new(dh, 1920, 1080, default_scale, frame_hz);
        let (clipboard_update_tx, clipboard_update_rx) = calloop_channel::channel::<(String, String)>();
        let _ = loop_handle.insert_source(clipboard_update_rx, |event, _, state| {
            if let calloop_channel::Event::Msg((sid, text)) = event {
                state.handle_clipboard_update_from_stage(&sid, text);
            }
        });
        let a11y = A11yCollector::spawn();

        let wslg_sink: Option<Box<dyn DisplaySink>> = if enable_wslg {
            match WslgWaylandSink::connect() {
                Ok(ws) => Some(Box::new(ws)),
                Err(wl_err) => {
                    let disp = std::env::var("WSLG_DISPLAY")
                        .ok()
                        .or_else(|| std::env::var("DISPLAY").ok());
                    match disp.as_deref() {
                        Some(d) if !d.is_empty() => match WslgSink::connect(Some(d)) {
                            Ok(s) => Some(Box::new(s)),
                            Err(e) => {
                                info!("WSLg sink unavailable (wl: {}, x11: {}): running headless", wl_err, e);
                                None
                            }
                        },
                        _ => {
                            info!("WSLg sink unavailable (wl: {}): running headless", wl_err);
                            None
                        }
                    }
                }
            }
        } else {
            None
        };

        Self {
            start_time: Instant::now(),
            display_handle: dh.clone(),
            loop_handle,
            wayland_socket_name,
            runtime_base_dir,
            default_scale,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            seat_state,
            output_manager_state,
            viewporter_state,
            single_pixel_buffer_state,
            xwayland_shell_state,
            seat,
            popups: PopupManager::default(),
            offscreen,
            clipboard_state,
            clipboard_update_tx,
            text_input_state,
            stages: HashMap::new(),
            next_stage_seq: 0,
            next_lease_seq: 0,
            xwms: HashMap::new(),
            xwm_to_stage: HashMap::new(),
            x11_override_windows: HashMap::new(),
            pending_act_jobs: Vec::new(),
            a11y,
            wslg_sink,
            wslg_time_anchor: None,
            should_quit: false,
        }
    }

    pub fn ensure_stage_for_client(&mut self, client: &Client) -> String {
        let cid = client.id();
        if let Some(sid) = self.stage_id_for_client(&cid) {
            return sid;
        }

        // Try PID-tree attribution via peer credentials
        let pid = client.get_data::<ClientState>().and_then(|d| d.pid);
        if let Some(p) = pid {
            if let Some(sid) = self.resolve_stage_for_pid(p) {
                if let Some(stage) = self.stages.get_mut(&sid) {
                    stage.wayland_clients.insert(cid, client.clone());
                }
                return sid;
            }
        }

        // Fallback: auto-create an external Stage for manually launched Wayland clients
        self.next_stage_seq += 1;
        let sid = format!("stage-{}", self.next_stage_seq);
        let disp = if self.wslg_sink.is_some() {
            DisplayState::Desktop
        } else {
            DisplayState::Hidden
        };
        let mut stage = Stage::new(
            sid.clone(),
            "external-wayland".to_string(),
            Vec::new(),
            StageKind::Wayland,
            1280,
            800,
            disp,
            self.default_scale,
        );
        stage.pid = pid;
        stage.wayland_clients.insert(cid, client.clone());
        self.stages.insert(sid.clone(), stage);
        info!("Auto-assigned external Wayland client (pid={:?}) to {}", pid, sid);
        sid
    }
}

impl BufferHandler for FocusStageState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for FocusStageState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(cs) = client.get_data::<ClientState>() {
            &cs.compositor_state
        } else if let Some(xs) = client.get_data::<XWaylandClientData>() {
            &xs.compositor_state
        } else {
            panic!("Unknown Wayland ClientData type");
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);
        self.offscreen.ensure_surface_entered(surface);

        if let Some(client) = surface.client() {
            let sid = self.stage_id_for_client(&client.id());
            if let Some(stage_id) = sid {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.bump_frame();
                }
                self.ensure_virtual_focus(&stage_id);
            }
        }
    }
}

impl XdgShellHandler for FocusStageState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let Some(client) = surface.wl_surface().client() else {
            return;
        };
        let stage_id = self.ensure_stage_for_client(&client);
        let size = self
            .stages
            .get(&stage_id)
            .map(|s| s.logical_size)
            .unwrap_or_else(|| (1280, 800).into());

        surface.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        let mut show_rect = None;
        let mut show_title = String::new();
        let mut show_wm_class = String::new();
        if let Some(stage) = self.stages.get_mut(&stage_id) {
            stage.wayland_clients.insert(client.id(), client.clone());
            stage.toplevels.push(StageToplevel::Wayland(surface.clone()));
            stage.bump_frame();
            if stage.display == DisplayState::Desktop {
                show_rect = stage.saved_rect;
                show_title = stage.title.clone();
                show_wm_class = stage.wm_class.clone();
            }
        }

        self.offscreen.ensure_surface_entered(surface.wl_surface());
        self.ensure_virtual_focus(&stage_id);

        if let (Some(ref mut sink), Some(rect)) = (&mut self.wslg_sink, show_rect) {
            sink.show(&stage_id, &show_title, &show_wm_class, rect);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface.clone()));
        let _ = surface.send_configure();
        self.offscreen.ensure_surface_entered(surface.wl_surface());
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Virtual focus stays active; allow popup without stealing focus from other Stages.
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let title = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().title.clone())
        });
        if let (Some(t), Some(client)) = (title, surface.wl_surface().client()) {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.title = t.clone();
                }
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.set_title(&stage_id, &t);
                }
            }
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.toplevels.retain(|t| match t {
                        StageToplevel::Wayland(w) => w.wl_surface() != surface.wl_surface(),
                        _ => true,
                    });
                }
            }
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.begin_move(&stage_id);
                }
            }
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let dir = match edges {
            xdg_toplevel::ResizeEdge::TopLeft => 0,
            xdg_toplevel::ResizeEdge::Top => 1,
            xdg_toplevel::ResizeEdge::TopRight => 2,
            xdg_toplevel::ResizeEdge::Right => 3,
            xdg_toplevel::ResizeEdge::BottomRight => 4,
            xdg_toplevel::ResizeEdge::Bottom => 5,
            xdg_toplevel::ResizeEdge::BottomLeft => 6,
            xdg_toplevel::ResizeEdge::Left => 7,
            _ => return,
        };
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.begin_resize(&stage_id, dir);
                }
            }
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.minimize(&stage_id);
                }
            }
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.set_maximized(&stage_id, true);
                }
            }
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
        if let Some(client) = surface.wl_surface().client() {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.set_maximized(&stage_id, false);
                }
            }
        }
    }
}

impl ShmHandler for FocusStageState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for FocusStageState {}

impl SeatHandler for FocusStageState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl XdgDecorationHandler for FocusStageState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        let chosen = match mode {
            DecorationMode::ClientSide | DecorationMode::ServerSide => DecorationMode::ClientSide,
            _ => DecorationMode::ClientSide,
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(chosen);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
        toplevel.send_configure();
    }
}

delegate_compositor!(FocusStageState);
delegate_xdg_shell!(FocusStageState);
delegate_xdg_decoration!(FocusStageState);
delegate_shm!(FocusStageState);
delegate_seat!(FocusStageState);
delegate_output!(FocusStageState);
delegate_viewporter!(FocusStageState);
delegate_single_pixel_buffer!(FocusStageState);
delegate_xwayland_shell!(FocusStageState);
