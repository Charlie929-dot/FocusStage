use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{get_popup_toplevel_coords, PopupKind, PopupManager};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::channel as calloop_channel;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};
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

    pub(crate) fn constrain_popup_geometry(
        &self,
        surface: &PopupSurface,
        positioner: PositionerState,
    ) -> Rectangle<i32, Logical> {
        let stage_size = surface
            .wl_surface()
            .client()
            .and_then(|c| self.stage_id_for_client(&c.id()))
            .and_then(|sid| self.stages.get(&sid))
            .map(|s| s.logical_size);

        let Some(stage_size) = stage_size else {
            return positioner.get_geometry();
        };

        let offset = get_popup_toplevel_coords(&PopupKind::Xdg(surface.clone()));
        let target = popup_constraint_target(stage_size, offset);
        positioner.get_unconstrained_geometry(target)
    }
}

pub(crate) fn popup_constraint_target(
    stage_size: Size<i32, Logical>,
    parent_toplevel_offset: Point<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (-parent_toplevel_offset.x, -parent_toplevel_offset.y).into(),
        (stage_size.w.max(1), stage_size.h.max(1)).into(),
    )
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

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = self.constrain_popup_geometry(&surface, positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
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
        let geometry = self.constrain_popup_geometry(&surface, positioner);
        surface.with_pending_state(|state| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner;
    use smithay::reexports::wayland_server::Display;
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols::xdg::shell::client::{
        xdg_positioner as c_pos, xdg_popup as c_popup, xdg_surface as c_surface,
        xdg_toplevel as c_toplevel, xdg_wm_base as c_wm,
    };

    #[test]
    fn popup_constraint_target_flips_direct_and_nested_popups_within_stage() {
        let stage_size = Size::from((1280, 800));

        // Direct popup near bottom-right corner of the Stage
        let direct_pos = PositionerState {
            rect_size: Size::from((200, 240)),
            anchor_rect: Rectangle::new((1200, 700).into(), (1, 1).into()),
            anchor_edges: xdg_positioner::Anchor::BottomRight,
            gravity: xdg_positioner::Gravity::BottomRight,
            constraint_adjustment: xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::FlipY,
            ..Default::default()
        };
        let direct_target = popup_constraint_target(stage_size, (0, 0).into());
        let direct_geo = direct_pos.get_unconstrained_geometry(direct_target);
        assert_eq!(direct_geo, Rectangle::new((1000, 460).into(), (200, 240).into()));

        // Nested submenu whose parent popup is at (1000, 460) in Stage coordinates
        let nested_pos = PositionerState {
            rect_size: Size::from((200, 200)),
            anchor_rect: Rectangle::new((190, 220).into(), (10, 20).into()),
            anchor_edges: xdg_positioner::Anchor::BottomRight,
            gravity: xdg_positioner::Gravity::BottomRight,
            constraint_adjustment: xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::FlipY,
            ..Default::default()
        };
        let nested_target = popup_constraint_target(stage_size, direct_geo.loc);
        assert_eq!(
            nested_target,
            Rectangle::new((-1000, -460).into(), (1280, 800).into())
        );
        let nested_geo = nested_pos.get_unconstrained_geometry(nested_target);
        // Anchor left edge = 190 -> flip X places right edge at 190 -> x = -10
        // Anchor top edge = 220 -> flip Y places bottom edge at 220 -> y = 20
        assert_eq!(nested_geo, Rectangle::new((-10, 20).into(), (200, 200).into()));
        // Absolute Stage location of nested popup: (1000 - 10, 460 + 20) = (990, 480)
        let abs_loc = direct_geo.loc + nested_geo.loc;
        assert!(abs_loc.x >= 0 && abs_loc.x + nested_geo.size.w <= stage_size.w);
        assert!(abs_loc.y >= 0 && abs_loc.y + nested_geo.size.h <= stage_size.h);
    }

    #[derive(Default)]
    struct TestClient {
        compositor: Option<wl_compositor::WlCompositor>,
        wm_base: Option<c_wm::XdgWmBase>,
        shm: Option<wl_shm::WlShm>,
        seat: Option<wl_seat::WlSeat>,
        pointer_enters: Vec<(u32, f64, f64)>,
        toplevel_configured: bool,
        popup1_configs: Vec<(i32, i32, i32, i32)>,
        popup1_repositioned: Vec<u32>,
        popup1_last_serial: Option<u32>,
        popup2_configs: Vec<(i32, i32, i32, i32)>,
        popup2_last_serial: Option<u32>,
        toplevel_xdg_id: Option<u32>,
        popup1_xdg_id: Option<u32>,
        popup2_xdg_id: Option<u32>,
        popup1_id: Option<u32>,
        popup2_id: Option<u32>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for TestClient {
        fn event(
            state: &mut Self,
            reg: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor = Some(reg.bind(name, version.min(6), qh, ()));
                    }
                    "xdg_wm_base" => {
                        state.wm_base = Some(reg.bind(name, version.min(6), qh, ()));
                    }
                    "wl_shm" => {
                        state.shm = Some(reg.bind(name, version.min(1), qh, ()));
                    }
                    "wl_seat" => {
                        let seat: wl_seat::WlSeat = reg.bind(name, version.min(7), qh, ());
                        let _ = seat.get_pointer(qh, ());
                        state.seat = Some(seat);
                    }
                    _ => {}
                }
            }
        }
    }

    impl Dispatch<wl_compositor::WlCompositor, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_surface::WlSurface, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_shm::WlShm, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_shm_pool::WlShmPool, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_buffer::WlBuffer, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_seat::WlSeat, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_pointer::WlPointer, ()> for TestClient {
        fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } = event {
                use wayland_client::Proxy;
                state.pointer_enters.push((surface.id().protocol_id(), surface_x, surface_y));
            }
        }
    }
    impl Dispatch<c_wm::XdgWmBase, ()> for TestClient {
        fn event(_: &mut Self, wm: &c_wm::XdgWmBase, event: c_wm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let c_wm::Event::Ping { serial } = event {
                wm.pong(serial);
            }
        }
    }
    impl Dispatch<c_pos::XdgPositioner, ()> for TestClient {
        fn event(_: &mut Self, _: &c_pos::XdgPositioner, _: c_pos::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<c_toplevel::XdgToplevel, ()> for TestClient {
        fn event(_: &mut Self, _: &c_toplevel::XdgToplevel, _: c_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<c_surface::XdgSurface, ()> for TestClient {
        fn event(state: &mut Self, surf: &c_surface::XdgSurface, event: c_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let c_surface::Event::Configure { serial } = event {
                use wayland_client::Proxy;
                let id = surf.id().protocol_id();
                if Some(id) == state.toplevel_xdg_id {
                    surf.ack_configure(serial);
                    state.toplevel_configured = true;
                } else if Some(id) == state.popup1_xdg_id {
                    state.popup1_last_serial = Some(serial);
                } else if Some(id) == state.popup2_xdg_id {
                    state.popup2_last_serial = Some(serial);
                }
            }
        }
    }
    impl Dispatch<c_popup::XdgPopup, ()> for TestClient {
        fn event(state: &mut Self, popup: &c_popup::XdgPopup, event: c_popup::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use wayland_client::Proxy;
            let id = popup.id().protocol_id();
            match event {
                c_popup::Event::Configure { x, y, width, height } => {
                    if Some(id) == state.popup1_id {
                        state.popup1_configs.push((x, y, width, height));
                    } else if Some(id) == state.popup2_id {
                        state.popup2_configs.push((x, y, width, height));
                    }
                }
                c_popup::Event::Repositioned { token } => {
                    if Some(id) == state.popup1_id {
                        state.popup1_repositioned.push(token);
                    }
                }
                _ => {}
            }
        }
    }

    fn pump(
        display: &mut Display<FocusStageState>,
        state: &mut FocusStageState,
        conn: &Connection,
        queue: &mut wayland_client::EventQueue<TestClient>,
        client: &mut TestClient,
    ) {
        for _ in 0..4 {
            let _ = conn.flush();
            let _ = display.dispatch_clients(state);
            let _ = display.flush_clients();
            if let Some(guard) = conn.prepare_read() {
                let _ = guard.read();
            }
            let _ = queue.dispatch_pending(client);
        }
    }

    fn create_shm_buffer(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<TestClient>,
        w: i32,
        h: i32,
    ) -> (wl_shm_pool::WlShmPool, wl_buffer::WlBuffer) {
        let size = (w * h * 4) as usize;
        let name = CString::new("fs-test-shm").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        unsafe { libc::ftruncate(fd, size as libc::off_t) };
        let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&owned), size as i32, qh, ());
        let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
        (pool, buf)
    }

    #[test]
    fn wayland_popup_new_reposition_and_nested_submenu_constrain_to_stage() {
        use wayland_client::Proxy;

        let mut event_loop: EventLoop<'static, FocusStageState> =
            EventLoop::try_new().expect("create calloop");
        let mut display: Display<FocusStageState> = Display::new().expect("create display");
        let mut dh = display.handle();
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-test-sock".to_string(),
            std::env::temp_dir(),
            1.0,
            60,
            false,
        );

        let (server_sock, client_sock) = UnixStream::pair().expect("unix socket pair");
        let _server_client = dh
            .insert_client(server_sock, Arc::new(ClientState::default()))
            .expect("insert server client");

        let conn = Connection::from_socket(client_sock).expect("connect client");
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();

        let _reg = conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let compositor = client.compositor.as_ref().unwrap().clone();
        let wm_base = client.wm_base.as_ref().unwrap().clone();
        let shm = client.shm.as_ref().unwrap().clone();

        // Create toplevel so FocusStage auto-assigns a Stage (1280x800), then resize it to 800x600.
        let tl_wl = compositor.create_surface(&qh, ());
        let tl_xdg = wm_base.get_xdg_surface(&tl_wl, &qh, ());
        client.toplevel_xdg_id = Some(tl_xdg.id().protocol_id());
        let _tl = tl_xdg.get_toplevel(&qh, ());
        tl_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.toplevel_configured);

        let stage_id = state.stages.keys().next().unwrap().clone();
        state.stages.get_mut(&stage_id).unwrap().logical_size = (800, 600).into();

        let (_pool_tl, buf_tl) = create_shm_buffer(&shm, &qh, 800, 600);
        tl_xdg.set_window_geometry(0, 0, 800, 600);
        tl_wl.attach(Some(&buf_tl), 0, 0);
        tl_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 1. Direct popup near bottom-right corner (720, 520), size 200x200, FlipX|FlipY -> (520, 320, 200, 200)
        let pos1 = wm_base.create_positioner(&qh, ());
        pos1.set_size(200, 200);
        pos1.set_anchor_rect(720, 520, 1, 1);
        pos1.set_anchor(c_pos::Anchor::BottomRight);
        pos1.set_gravity(c_pos::Gravity::BottomRight);
        pos1.set_constraint_adjustment(c_pos::ConstraintAdjustment::FlipX | c_pos::ConstraintAdjustment::FlipY);

        let p1_wl = compositor.create_surface(&qh, ());
        let p1_xdg = wm_base.get_xdg_surface(&p1_wl, &qh, ());
        client.popup1_xdg_id = Some(p1_xdg.id().protocol_id());
        let p1 = p1_xdg.get_popup(Some(&tl_xdg), &pos1, &qh, ());
        client.popup1_id = Some(p1.id().protocol_id());
        p1_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        assert_eq!(client.popup1_configs.last(), Some(&(520, 320, 200, 200)));
        let serial1 = client.popup1_last_serial.expect("popup1 configure serial");
        let (_pool1, buf1) = create_shm_buffer(&shm, &qh, 200, 200);
        p1_xdg.ack_configure(serial1);
        p1_xdg.set_window_geometry(0, 0, 200, 200);
        p1_wl.attach(Some(&buf1), 0, 0);
        p1_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 2. Reposition popup1 near (750, 550), size 180x180, FlipX|FlipY -> (570, 370, 180, 180)
        let pos_re = wm_base.create_positioner(&qh, ());
        pos_re.set_size(180, 180);
        pos_re.set_anchor_rect(750, 550, 1, 1);
        pos_re.set_anchor(c_pos::Anchor::BottomRight);
        pos_re.set_gravity(c_pos::Gravity::BottomRight);
        pos_re.set_constraint_adjustment(c_pos::ConstraintAdjustment::FlipX | c_pos::ConstraintAdjustment::FlipY);
        p1.reposition(&pos_re, 42);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        assert_eq!(client.popup1_repositioned, vec![42]);
        assert_eq!(client.popup1_configs.last(), Some(&(570, 370, 180, 180)));
        let serial1_re = client.popup1_last_serial.expect("popup1 reposition serial");
        let (_pool1b, buf1b) = create_shm_buffer(&shm, &qh, 180, 180);
        p1_xdg.ack_configure(serial1_re);
        p1_xdg.set_window_geometry(0, 0, 180, 180);
        p1_wl.attach(Some(&buf1b), 0, 0);
        p1_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 3. Nested submenu popup2 parented to popup1 (now at 570, 370).
        // Anchor (170, 160, 10, 20) inside popup1 -> unconstrained right/down (180, 180) => Stage (750, 550),
        // which overflows 800x600. FlipX|FlipY places it at (170 - 150, 160 - 150) = (20, 10) relative to popup1,
        // i.e. Stage (590, 380)..(740, 530).
        let pos2 = wm_base.create_positioner(&qh, ());
        pos2.set_size(150, 150);
        pos2.set_anchor_rect(170, 160, 10, 20);
        pos2.set_anchor(c_pos::Anchor::BottomRight);
        pos2.set_gravity(c_pos::Gravity::BottomRight);
        pos2.set_constraint_adjustment(c_pos::ConstraintAdjustment::FlipX | c_pos::ConstraintAdjustment::FlipY);

        let p2_wl = compositor.create_surface(&qh, ());
        let p2_xdg = wm_base.get_xdg_surface(&p2_wl, &qh, ());
        client.popup2_xdg_id = Some(p2_xdg.id().protocol_id());
        let _p2 = p2_xdg.get_popup(Some(&p1_xdg), &pos2, &qh, ());
        client.popup2_id = Some(_p2.id().protocol_id());
        p2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        assert_eq!(client.popup2_configs.last(), Some(&(20, 10, 150, 150)));

        // Commit popup2 and verify pointer hit-testing routes to popup2 at flipped Stage coordinates (595, 385)
        let serial2 = client.popup2_last_serial.expect("popup2 configure serial");
        let (_pool2, buf2) = create_shm_buffer(&shm, &qh, 150, 150);
        p2_xdg.ack_configure(serial2);
        p2_xdg.set_window_geometry(0, 0, 150, 150);
        p2_wl.attach(Some(&buf2), 0, 0);
        p2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        state.send_stage_pointer_motion(&stage_id, 595.0, 385.0);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let ptr_target = state
            .stages
            .get(&stage_id)
            .and_then(|s| s.input.current_pointer_target.as_ref())
            .and_then(|t| t.wl_surface());
        assert_eq!(
            ptr_target.map(|s| Resource::id(&s).protocol_id()),
            Some(p2_wl.id().protocol_id())
        );
        assert_eq!(
            client.pointer_enters.last().copied(),
            Some((p2_wl.id().protocol_id(), 5.0, 5.0))
        );

        let _ = event_loop.dispatch(Some(std::time::Duration::ZERO), &mut state);
    }
}
