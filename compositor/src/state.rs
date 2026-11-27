use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

use smithay::backend::renderer::utils::{
    on_commit_buffer_handler, with_renderer_surface_state, RendererSurfaceStateUserData,
};
use smithay::desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager};
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
    is_sync_subsurface, with_states, with_surface_tree_upward, BufferAssignment,
    CompositorClientState, CompositorHandler, CompositorState, Damage, SurfaceAttributes,
    TraversalAction,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData, XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::XwmId;
use smithay::xwayland::{X11Surface, X11Wm, XWaylandClientData};
use smithay::{
    delegate_compositor, delegate_fractional_scale, delegate_output, delegate_seat, delegate_shm,
    delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_decoration,
    delegate_xdg_shell, delegate_xwayland_shell,
};
use tracing::{debug, info};

use crate::a11y::A11yCollector;
use crate::clipboard::StageDataDeviceManagerState;
use crate::dpi::VirtualOutputs;
use crate::offscreen::OffscreenCore;
use crate::protocol::{DisplayState, SavedRect, StageKind};
use crate::router::{PendingActJob, StageTextInputManagerState};
use crate::sink::DisplaySink;
use crate::stage::{ClipboardContent, Stage, StageToplevel};

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
    pub virtual_outputs: VirtualOutputs,

    pub clipboard_state: StageDataDeviceManagerState,
    pub clipboard_update_tx: calloop_channel::Sender<(String, u64, ClipboardContent)>,
    pub text_input_state: StageTextInputManagerState,

    pub stages: HashMap<String, Stage>,
    pub next_stage_seq: u64,
    pub next_lease_seq: u64,
    pub(crate) ended_leases: std::collections::VecDeque<(String, crate::control::LeaseEnd)>,
    pub(crate) handoff_children: Vec<std::process::Child>,

    pub xwms: HashMap<XwmId, X11Wm>,
    pub xwm_to_stage: HashMap<XwmId, String>,
    pub x11_override_windows: HashMap<XwmId, Vec<(X11Surface, Option<u32>)>>,

    pub pending_act_jobs: Vec<PendingActJob>,
    pub(crate) pending_launches: Vec<crate::control::PendingLaunch>,
    pub a11y: A11yCollector,
    pub(crate) polkit_cmd_tx: calloop_channel::Sender<crate::polkit::PolkitHostCommand>,
    pub(crate) polkit_agent: Option<crate::polkit::PolkitAgentHandle>,
    pub(crate) keyring_prompter: Option<crate::keyring::KeyringPrompterHandle>,
    pub(crate) host_proxy: crate::proxy::HostProxyManager,
    pub display_sink: Option<Box<dyn DisplaySink>>,
    pub should_quit: bool,
}

impl FocusStageState {
    pub fn new(
        dh: &DisplayHandle,
        loop_handle: LoopHandle<'static, FocusStageState>,
        wayland_socket_name: String,
        default_scale: f64,
        frame_hz: u32,
        primary_monitor: SavedRect,
        display_sink: Option<Box<dyn DisplaySink>>,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(dh);
        let xdg_shell_state = XdgShellState::new::<Self>(dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(dh);
        let shm_state = ShmState::new::<Self>(dh, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "seat-0");
        let _ = seat.add_keyboard(Default::default(), 200, 25);
        let _ = seat.add_pointer();
        smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(dh);

        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(dh);
        let viewporter_state = ViewporterState::new::<Self>(dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(dh);
        let clipboard_state = StageDataDeviceManagerState::new(dh);
        let text_input_state = StageTextInputManagerState::new(dh);
        let offscreen = OffscreenCore::new();
        let virtual_outputs = VirtualOutputs::new(dh, primary_monitor, default_scale, frame_hz);
        FractionalScaleManagerState::new::<Self>(dh);
        let (clipboard_update_tx, clipboard_update_rx) =
            calloop_channel::channel::<(String, u64, ClipboardContent)>();
        let _ = loop_handle.insert_source(clipboard_update_rx, |event, _, state| {
            if let calloop_channel::Event::Msg((sid, expected_seq, content)) = event {
                state.handle_clipboard_update_from_stage(&sid, expected_seq, content);
            }
        });
        let (polkit_cmd_tx, polkit_cmd_rx) =
            calloop_channel::channel::<crate::polkit::PolkitHostCommand>();
        let _ = loop_handle.insert_source(polkit_cmd_rx, |event, _, state| {
            if let calloop_channel::Event::Msg(cmd) = event {
                if let Some(sink) = state.display_sink.as_mut() {
                    sink.send_polkit_command(&cmd);
                }
            }
        });
        let a11y = A11yCollector::spawn();

        Self {
            start_time: Instant::now(),
            display_handle: dh.clone(),
            loop_handle,
            wayland_socket_name,
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
            virtual_outputs,
            clipboard_state,
            clipboard_update_tx,
            text_input_state,
            stages: HashMap::new(),
            next_stage_seq: 0,
            next_lease_seq: 0,
            ended_leases: Default::default(),
            handoff_children: Vec::new(),
            xwms: HashMap::new(),
            xwm_to_stage: HashMap::new(),
            x11_override_windows: HashMap::new(),
            pending_act_jobs: Vec::new(),
            pending_launches: Vec::new(),
            a11y,
            polkit_cmd_tx,
            polkit_agent: None,
            keyring_prompter: None,
            host_proxy: crate::proxy::HostProxyManager::new(),
            display_sink,
            should_quit: false,
        }
    }

    pub fn apply_host_proxy(&mut self, cfg: crate::proxy::HostProxyConfig) {
        self.host_proxy.apply(cfg);
    }

    pub fn clear_host_proxy(&mut self) {
        self.host_proxy.clear();
    }

    pub fn send_polkit_reply(&self, reply: crate::polkit::PolkitHostReply) {
        if let Some(agent) = self.polkit_agent.as_ref() {
            agent.send_host_reply(reply);
        }
    }

    pub fn send_keyring_reply(&self, reply: crate::polkit::PolkitHostReply) {
        if let Some(prompter) = self.keyring_prompter.as_ref() {
            prompter.send_host_reply(reply);
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
        let disp = if self.display_sink.is_some() {
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

    fn maximize_toplevel(&mut self, surface: ToplevelSurface, maximized: bool) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface.clone())) {
            self.request_window_maximized(&id, maximized);
        }
        surface.with_pending_state(|state| {
            if maximized {
                state.states.set(xdg_toplevel::State::Maximized);
            } else {
                state.states.unset(xdg_toplevel::State::Maximized);
            }
        });
        surface.send_configure();
    }

    pub(crate) fn constrain_popup_geometry(
        &self,
        surface: &PopupSurface,
        positioner: PositionerState,
    ) -> Rectangle<i32, Logical> {
        let stage_size = find_popup_root_surface(&PopupKind::Xdg(surface.clone()))
            .ok()
            .and_then(|root| self.window_for_surface(&root))
            .map(|window| window.logical_size);

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
        // Ignore only tiny, explicitly damaged regions for settling, not for painting.
        const TINY_DAMAGE_AREA: u64 = 256;
        #[derive(Default)]
        struct CommittedGeometryOffset(Cell<Point<i32, Logical>>);

        let mut damage_area = 0u64;
        let mut significant = false;
        let mut previous_views = Vec::new();
        // Smithay applies the committed subtree together and consumes its buffer assignments.
        if !is_sync_subsurface(surface) {
            with_surface_tree_upward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    let mut cached = states.cached_state.get::<SurfaceAttributes>();
                    let attrs = cached.current();
                    let view = states.data_map.get::<RendererSurfaceStateUserData>()
                        .and_then(|state| state.lock().unwrap().view());
                    if matches!(states.role, Some(XDG_TOPLEVEL_ROLE | XDG_POPUP_ROLE)) {
                        let offset = states.cached_state.get::<SurfaceCachedState>()
                            .current().geometry.map_or_else(Point::default, |geometry| geometry.loc);
                        states.data_map.insert_if_missing(CommittedGeometryOffset::default);
                        let old = states.data_map.get::<CommittedGeometryOffset>().unwrap().0.replace(offset);
                        significant |= view.is_some() && offset != old;
                    }
                    let area = attrs.damage.iter().fold(0u64, |area, damage| {
                        let (width, height) = match damage {
                            Damage::Buffer(rect) => (rect.size.w, rect.size.h),
                            Damage::Surface(rect) => (rect.size.w, rect.size.h),
                        };
                        area.saturating_add(width.max(0) as u64 * height.max(0) as u64)
                    });
                    if matches!(attrs.buffer, Some(BufferAssignment::NewBuffer(_))) {
                        // No damage hint gives us no basis to recognize a tiny update.
                        significant |= area == 0;
                    } else {
                        // The renderer consumes damage only on buffer attachment.
                        attrs.damage.clear();
                    }
                    // A small source rect may cover a large area in an upscaled view.
                    significant |= area > 0 && view.is_some_and(|view| {
                        f64::from(view.dst.w) > view.src.size.w || f64::from(view.dst.h) > view.src.size.h
                    });
                    damage_area = damage_area.saturating_add(area);
                    previous_views.push((surface.clone(), view));
                },
                |_, _, _| true,
            );
        }
        on_commit_buffer_handler::<Self>(surface);
        self.cursor_committed(surface);
        significant |= damage_area > TINY_DAMAGE_AREA || previous_views.into_iter().any(|(surface, previous)| {
            with_renderer_surface_state(&surface, |state| state.view()).flatten() != previous
        });
        self.popups.commit(surface);
        self.sync_surface_output(surface);

        if let Some(window) = self.window_for_surface(surface) {
            let id = window.id.clone();
            let (stage_id, _) = id.rsplit_once('/').unwrap();
            let is_root = window.toplevel.wl_surface().as_ref() == Some(surface);
            if is_root {
                if let StageToplevel::Wayland(toplevel) = &window.toplevel {
                    let mapped = with_renderer_surface_state(surface, |state| state.buffer().is_some())
                        .unwrap_or(false);
                    if !mapped && !toplevel.is_initial_configure_sent() {
                        toplevel.send_configure();
                    }
                    significant |= mapped != window.mapped;
                    if mapped && !window.mapped {
                        let geometry = with_states(surface, |states| {
                            states.cached_state.get::<SurfaceCachedState>().current().geometry
                        });
                        let size = geometry.map(|rect| rect.size).or_else(|| {
                            with_renderer_surface_state(surface, |state| state.surface_size()).flatten()
                        });
                        if let Some(size) = size {
                            self.stages.get_mut(stage_id).unwrap().window_mut(&id).unwrap().set_mapped_size(size);
                        }
                    }
                    self.set_window_mapped(&id, mapped);
                }
            }
            if significant || damage_area > 0 {
                let stage = self.stages.get_mut(stage_id).unwrap();
                if significant {
                    stage.window_mut(&id).unwrap().content_seq += 1;
                }
                stage.mark_visual_update(Some(&id), significant);
            }
        } else if let Some(client) = surface.client().filter(|_| significant || damage_area > 0) {
            if let Some(stage_id) = self.stage_id_for_client(&client.id()) {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    // X11 override-redirect surfaces are composited with their owning window.
                    stage.mark_visual_update(None, significant);
                }
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
        let stage = self.stages.get_mut(&stage_id).unwrap();
        let size = (stage.initial_rect.width as i32, stage.initial_rect.height as i32).into();
        let initial_size = stage.windows.is_empty().then_some(size);
        stage.wayland_clients.insert(client.id(), client);
        stage.add_window(StageToplevel::Wayland(surface.clone()), size);

        surface.with_pending_state(|state| {
            state.size = initial_size;
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.sync_surface_output(surface.wl_surface());
        self.ensure_virtual_focus(&stage_id);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = self.constrain_popup_geometry(&surface, positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        let _ = self.popups.track_popup(PopupKind::Xdg(surface.clone()));
        let _ = surface.send_configure();
        self.sync_surface_output(surface.wl_surface());
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.grab_stage_popup(surface, seat, serial);
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.popup_gone(surface);
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
        if let (Some(title), Some(id)) = (title, self.window_id_for_toplevel(&StageToplevel::Wayland(surface))) {
            self.update_window_title(&id, title);
        }
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        let parent = surface.parent().and_then(|wl| self.window_for_surface(&wl)).map(|w| w.id.clone());
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface)) {
            self.update_window_parent(&id, parent);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface)) {
            self.remove_window(&id);
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.begin_move(&id);
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
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.begin_resize(&id, dir);
            }
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::Wayland(surface)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.minimize(&id);
            }
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize_toplevel(surface, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize_toplevel(surface, false);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.request_window_fullscreen(StageToplevel::Wayland(surface), true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_window_fullscreen(StageToplevel::Wayland(surface), false);
    }
}

impl ShmHandler for FocusStageState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for FocusStageState {}

impl FractionalScaleHandler for FocusStageState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        if self
            .window_for_surface(&surface)
            .or_else(|| self.window_for_cursor_surface(&surface))
            .is_some()
        {
            self.sync_surface_output(&surface);
        }
    }
}

impl SeatHandler for FocusStageState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn allow_cursor_request(&self, _: &smithay::input::pointer::PointerHandle<Self>, serial: Serial, object: &smithay::reexports::wayland_server::backend::ObjectId) -> bool {
        self.cursor_request_allowed(serial, object)
    }
    fn client_cursor_image(&mut self, _: &Seat<Self>, serial: Serial, image: CursorImageStatus) {
        self.update_cursor_image(serial, image);
    }
}

impl smithay::wayland::tablet_manager::TabletSeatHandler for FocusStageState {}
smithay::delegate_cursor_shape!(FocusStageState);

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
delegate_fractional_scale!(FocusStageState);
delegate_viewporter!(FocusStageState);
delegate_single_pixel_buffer!(FocusStageState);
delegate_xwayland_shell!(FocusStageState);

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner;
    use smithay::reexports::wayland_server::Display;
    use wayland_client::protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols::wp::cursor_shape::v1::client::{
        wp_cursor_shape_device_v1 as c_cursor_shape_device,
        wp_cursor_shape_manager_v1 as c_cursor_shape_manager,
    };
    use wayland_protocols::wp::fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1 as c_scale_manager, wp_fractional_scale_v1 as c_scale,
    };
    use wayland_protocols::wp::text_input::zv3::client::{
        zwp_text_input_manager_v3 as c_text_manager, zwp_text_input_v3 as c_text,
    };
    use wayland_protocols::xdg::shell::client::{
        xdg_positioner as c_pos, xdg_popup as c_popup, xdg_surface as c_surface,
        xdg_toplevel as c_toplevel, xdg_wm_base as c_wm,
    };
    use wayland_protocols::xwayland::shell::v1::client::{xwayland_shell_v1, xwayland_surface_v1};

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
        subcompositor: Option<wl_subcompositor::WlSubcompositor>,
        wm_base: Option<c_wm::XdgWmBase>,
        shm: Option<wl_shm::WlShm>,
        seat: Option<wl_seat::WlSeat>,
        data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
        pointer_enters: Vec<(u32, f64, f64)>,
        pointer: Option<wl_pointer::WlPointer>,
        cursor_shape_manager: Option<c_cursor_shape_manager::WpCursorShapeManagerV1>,
        frame_callbacks: usize,
        pointer_enter_serial: u32,
        button_events: usize,
        popup_done: Vec<u32>,
        pointer_press_serial: Option<u32>,
        client_drag_offer: Option<(u32, wl_data_offer::WlDataOffer)>,
        drag_drops: usize,
        drag_cancellations: Vec<u32>,
        drag_finished: Vec<u32>,
        text_input_manager: Option<c_text_manager::ZwpTextInputManagerV3>,
        text_input_enters: Vec<wl_surface::WlSurface>,
        drop_enters: Vec<(u32, f64, f64)>,
        offer_mimes: HashMap<u32, Vec<String>>,
        active_selection: Option<wl_data_offer::WlDataOffer>,
        selection_cleared_count: usize,
        source_payloads: HashMap<u32, Vec<u8>>,
        fractional_scale: Option<c_scale_manager::WpFractionalScaleManagerV1>,
        preferred_scales: HashMap<u32, u32>,
        window_configures: HashMap<u32, (i32, i32, Vec<u32>)>,
        window_closes: Vec<u32>,
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
                    "wl_subcompositor" => {
                        state.subcompositor = Some(reg.bind(name, 1, qh, ()));
                    }
                    "xdg_wm_base" => {
                        state.wm_base = Some(reg.bind(name, version.min(6), qh, ()));
                    }
                    "wl_shm" => {
                        state.shm = Some(reg.bind(name, version.min(1), qh, ()));
                    }
                    "wp_fractional_scale_manager_v1" => {
                        state.fractional_scale = Some(reg.bind(name, 1, qh, ()));
                    }
                    "wp_cursor_shape_manager_v1" => {
                        state.cursor_shape_manager = Some(reg.bind(name, version.min(2), qh, ()));
                    }
                    "zwp_text_input_manager_v3" => {
                        state.text_input_manager = Some(reg.bind(name, 1, qh, ()));
                    }
                    "wl_data_device_manager" => {
                        state.data_device_manager = Some(reg.bind(name, version.min(3), qh, ()));
                    }
                    "wl_seat" => {
                        let seat: wl_seat::WlSeat = reg.bind(name, version.min(7), qh, ());
                        state.pointer = Some(seat.get_pointer(qh, ()));
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
    impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_subcompositor::WlSubcompositor, _: wl_subcompositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_subsurface::WlSubsurface, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_subsurface::WlSubsurface, _: wl_subsurface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_callback::WlCallback, ()> for TestClient {
        fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if matches!(event, wl_callback::Event::Done { .. }) {
                state.frame_callbacks += 1;
            }
        }
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
    impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_data_device_manager::WlDataDeviceManager, _: wl_data_device_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_data_offer::WlDataOffer, ()> for TestClient {
        fn event(state: &mut Self, offer: &wl_data_offer::WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_data_offer::Event::Offer { mime_type } = event {
                state.offer_mimes.entry(offer.id().protocol_id()).or_default().push(mime_type);
            }
        }
    }
    impl Dispatch<wl_data_source::WlDataSource, ()> for TestClient {
        fn event(state: &mut Self, source: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                wl_data_source::Event::Send { fd, .. } => {
                    use std::io::Write;
                    if let Some(bytes) = state.source_payloads.get(&source.id().protocol_id()) {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(bytes);
                    }
                }
                wl_data_source::Event::Cancelled => state.drag_cancellations.push(source.id().protocol_id()),
                wl_data_source::Event::DndFinished => state.drag_finished.push(source.id().protocol_id()),
                _ => {}
            }
        }
    }
    impl Dispatch<wl_data_device::WlDataDevice, ()> for TestClient {
        wayland_client::event_created_child!(TestClient, wl_data_device::WlDataDevice, [
            wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
        ]);

        fn event(state: &mut Self, _: &wl_data_device::WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                wl_data_device::Event::Enter { serial, surface, x, y, id } => {
                    state.drop_enters.push((surface.id().protocol_id(), x, y));
                    state.client_drag_offer = id.map(|id| (serial, id));
                }
                wl_data_device::Event::Drop => state.drag_drops += 1,
                wl_data_device::Event::Selection { id } => {
                    if id.is_none() {
                        state.selection_cleared_count += 1;
                    }
                    state.active_selection = id;
                }
                _ => {}
            }
        }
    }
    impl Dispatch<wl_seat::WlSeat, ()> for TestClient {
        fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    wayland_client::delegate_noop!(TestClient: ignore wl_keyboard::WlKeyboard);
    wayland_client::delegate_noop!(TestClient: ignore c_text_manager::ZwpTextInputManagerV3);
    wayland_client::delegate_noop!(TestClient: ignore c_cursor_shape_manager::WpCursorShapeManagerV1);
    wayland_client::delegate_noop!(TestClient: ignore c_cursor_shape_device::WpCursorShapeDeviceV1);

    impl Dispatch<c_text::ZwpTextInputV3, ()> for TestClient {
        fn event(
            state: &mut Self,
            _: &c_text::ZwpTextInputV3,
            event: c_text::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let c_text::Event::Enter { surface } = event {
                state.text_input_enters.push(surface);
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for TestClient {
        fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                wl_pointer::Event::Enter { surface, surface_x, surface_y, serial } => {
                    state.pointer_enter_serial = serial;
                    state.pointer_enters.push((surface.id().protocol_id(), surface_x, surface_y));
                }
                wl_pointer::Event::Button { serial, state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                    state.pointer_press_serial = Some(serial);
                    state.button_events += 1;
                }
                wl_pointer::Event::Button { .. } => state.button_events += 1,
                _ => {}
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
    impl Dispatch<c_scale_manager::WpFractionalScaleManagerV1, ()> for TestClient {
        fn event(_: &mut Self, _: &c_scale_manager::WpFractionalScaleManagerV1, _: c_scale_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<c_scale::WpFractionalScaleV1, u32> for TestClient {
        fn event(state: &mut Self, _: &c_scale::WpFractionalScaleV1, event: c_scale::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
            if let c_scale::Event::PreferredScale { scale } = event {
                state.preferred_scales.insert(*id, scale);
            }
        }
    }
    impl Dispatch<c_toplevel::XdgToplevel, ()> for TestClient {
        fn event(state: &mut Self, window: &c_toplevel::XdgToplevel, event: c_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            let id = window.id().protocol_id();
            match event {
                c_toplevel::Event::Configure { width, height, states } => {
                    let states = states.chunks_exact(4).map(|v| u32::from_ne_bytes(v.try_into().unwrap())).collect();
                    state.window_configures.insert(id, (width, height, states));
                }
                c_toplevel::Event::Close => state.window_closes.push(id),
                _ => {}
            }
        }
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
                } else {
                    surf.ack_configure(serial);
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
                c_popup::Event::PopupDone => state.popup_done.push(id),
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
        create_shm_buffer_rgba(shm, qh, w, h, [0, 0, 0, 0])
    }

    fn create_shm_buffer_rgba(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<TestClient>,
        w: i32,
        h: i32,
        bgra: [u8; 4],
    ) -> (wl_shm_pool::WlShmPool, wl_buffer::WlBuffer) {
        let size = (w * h * 4) as usize;
        let name = CString::new("fs-test-shm").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        unsafe { libc::ftruncate(fd, size as libc::off_t) };
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let bytes = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
        for px in bytes.chunks_exact_mut(4) {
            px.copy_from_slice(&bgra);
        }
        unsafe { libc::munmap(ptr, size) };
        let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&owned), size as i32, qh, ());
        let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
        (pool, buf)
    }

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedOverlay {
        window_id: String,
        active: bool,
        agent_name: Option<String>,
        pointer_x: f64,
        pointer_y: f64,
        pointer_visible: bool,
        pointer_state: String,
    }

    #[derive(Default)]
    struct WindowLog {
        shown: Vec<(String, String, Option<String>)>,
        hidden: Vec<String>,
        destroyed: Vec<String>,
        host_clipboard_texts: Vec<String>,
        host_clipboard_images: Vec<Vec<u8>>,
        overlays: Vec<RecordedOverlay>,
        fullscreen_requests: Vec<(String, bool)>,
        cursors: Vec<(String, crate::sink::NativeCursor)>,
        file_drags_started: Vec<(String, u32, Vec<String>)>,
        file_drags_cancelled: Vec<(String, u32)>,
    }

    struct TestSink(Arc<Mutex<WindowLog>>);

    impl DisplaySink for TestSink {
        fn show(&mut self, w: crate::sink::WindowInfo<'_>) {
            self.0.lock().unwrap().shown.push((w.stage_id.into(), w.window_id.into(), w.parent_window_id.map(str::to_owned)));
        }
        fn hide(&mut self, id: &str) {
            self.0.lock().unwrap().hidden.push(id.into());
        }
        fn destroy(&mut self, id: &str) { self.0.lock().unwrap().destroyed.push(id.into()); }
        fn set_title(&mut self, _: &str, _: &str) {}
        fn set_cursor(&mut self, id: &str, cursor: crate::sink::NativeCursor) {
            self.0.lock().unwrap().cursors.push((id.into(), cursor));
        }
        fn set_fullscreen(&mut self, window_id: &str, fullscreen: bool) {
            self.0.lock().unwrap().fullscreen_requests.push((window_id.into(), fullscreen));
        }
        fn begin_file_drag(&mut self, window_id: &str, serial: u32, paths: &[String]) -> bool {
            self.0.lock().unwrap().file_drags_started.push((window_id.into(), serial, paths.to_vec()));
            true
        }
        fn cancel_file_drag(&mut self, window_id: &str, serial: u32) {
            self.0.lock().unwrap().file_drags_cancelled.push((window_id.into(), serial));
        }
        fn set_host_clipboard(&mut self, text: &str) {
            self.0.lock().unwrap().host_clipboard_texts.push(text.to_string());
        }
        fn set_host_clipboard_image(&mut self, png_bytes: &[u8]) {
            self.0.lock().unwrap().host_clipboard_images.push(png_bytes.to_vec());
        }
        fn set_agent_overlay(&mut self, overlay: crate::sink::AgentOverlay<'_>) {
            self.0.lock().unwrap().overlays.push(RecordedOverlay {
                window_id: overlay.window_id.to_string(),
                active: overlay.active,
                agent_name: overlay.agent_name.map(str::to_owned),
                pointer_x: overlay.pointer_x,
                pointer_y: overlay.pointer_y,
                pointer_visible: overlay.pointer_visible,
                pointer_state: overlay.pointer_state.to_string(),
            });
        }
        fn present_owned(&mut self, _: &str, _: u32, _: u32, _: Vec<u8>) {}
        fn poll_events(&mut self) -> Vec<crate::sink::SinkInputEvent> { Vec::new() }
    }

    #[test]
    fn text_input_registration_only_enters_the_clients_keyboard_focus() {
        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-text-input-test".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            None,
        );
        let mut connect = || {
            let (server_sock, client_sock) = UnixStream::pair().unwrap();
            let peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
            let conn = Connection::from_socket(client_sock).unwrap();
            let queue = conn.new_event_queue();
            conn.display().get_registry(&queue.handle(), ());
            (peer, conn, queue, TestClient::default())
        };
        let (first_peer, first_conn, mut first_queue, mut first) = connect();
        let (second_peer, second_conn, mut second_queue, mut second) = connect();
        let sid = state.ensure_stage_for_client(&first_peer);
        state.stages.get_mut(&sid).unwrap().wayland_clients.insert(second_peer.id(), second_peer);

        pump(&mut display, &mut state, &first_conn, &mut first_queue, &mut first);
        let first_qh = first_queue.handle();
        let first_seat = first.seat.as_ref().unwrap();
        first_seat.get_keyboard(&first_qh, ());
        let first_surface = first.compositor.as_ref().unwrap().create_surface(&first_qh, ());
        let first_xdg = first.wm_base.as_ref().unwrap().get_xdg_surface(&first_surface, &first_qh, ());
        let _first_window = first_xdg.get_toplevel(&first_qh, ());
        first_surface.commit();
        pump(&mut display, &mut state, &first_conn, &mut first_queue, &mut first);
        assert!(state.stages[&sid].input.current_keyboard_target.is_some());

        pump(&mut display, &mut state, &second_conn, &mut second_queue, &mut second);
        let second_qh = second_queue.handle();
        let second_seat = second.seat.as_ref().unwrap();
        second_seat.get_keyboard(&second_qh, ());
        let _second_input = second.text_input_manager.as_ref().unwrap().get_text_input(second_seat, &second_qh, ());
        pump(&mut display, &mut state, &second_conn, &mut second_queue, &mut second);
        assert!(second.text_input_enters.is_empty());

        let _first_input = first.text_input_manager.as_ref().unwrap().get_text_input(first.seat.as_ref().unwrap(), &first_qh, ());
        pump(&mut display, &mut state, &first_conn, &mut first_queue, &mut first);
        assert_eq!(first.text_input_enters, [first_surface]);

        let second_surface = second.compositor.as_ref().unwrap().create_surface(&second_qh, ());
        let second_xdg = second.wm_base.as_ref().unwrap().get_xdg_surface(&second_surface, &second_qh, ());
        let _second_window = second_xdg.get_toplevel(&second_qh, ());
        second_surface.commit();
        pump(&mut display, &mut state, &second_conn, &mut second_queue, &mut second);
        assert_eq!(second.text_input_enters, [second_surface]);
    }

    #[test]
    fn wayland_client_drag_finishes_or_cancels_before_the_next_drop() {
        use std::io::Read;
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileExt;
        use wayland_client::protocol::wl_data_device_manager::DndAction;

        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let mut state = FocusStageState::new(
            &dh, event_loop.handle(), "focusstage-drag-test".into(), 1.0, 60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 }, None,
        );
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
        let conn = Connection::from_socket(client_sock).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();
        let sid = state.ensure_stage_for_client(&peer);
        conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let manager = client.data_device_manager.clone().unwrap();
        let device = manager.get_data_device(client.seat.as_ref().unwrap(), &qh, ());
        let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
        let xdg = client.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg.get_toplevel(&qh, ());
        surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (_pool, buffer) = create_shm_buffer(client.shm.as_ref().unwrap(), &qh, 640, 480);
        xdg.set_window_geometry(0, 0, 640, 480);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let window_id = state.stages[&sid].active_window().unwrap().id.clone();

        let mime = "text/uri-list";
        let payload = b"file:///tmp/source.txt\r\n";
        let new_source = || {
            let source = manager.create_data_source(&qh, ());
            source.offer(mime.into());
            source.set_actions(DndAction::Copy | DndAction::Move);
            source
        };
        let source = new_source();
        let source_id = source.id().protocol_id();
        client.source_payloads.insert(source_id, payload.to_vec());
        state.send_window_button_at(&sid, Some(&window_id), 20.0, 30.0, 0x110, true, 1);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let press = client.pointer_press_serial.unwrap();
        device.start_drag(Some(&source), &surface, None, press);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (enter, offer) = client.client_drag_offer.take().unwrap();
        offer.set_actions(DndAction::Move, DndAction::Move);
        offer.accept(enter, Some(mime.into()));
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        state.send_window_button_at(&sid, Some(&window_id), 180.0, 130.0, 0x110, false, 2);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.drag_drops, 1);
        let file = tempfile::tempfile().unwrap();
        offer.receive(mime.into(), file.as_fd());
        offer.finish();
        offer.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let mut bytes = vec![0; payload.len()];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, payload);
        assert_eq!(client.drag_finished, [source_id]);
        assert!(state.stages[&sid].input.drag.is_none());

        let cancelled = new_source();
        state.send_window_button_at(&sid, Some(&window_id), 20.0, 30.0, 0x110, true, 3);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let press = client.pointer_press_serial.unwrap();
        device.start_drag(Some(&cancelled), &surface, None, press);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (enter, offer) = client.client_drag_offer.take().unwrap();
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        offer.accept(enter, Some(mime.into()));
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        state.send_window_button_at(&sid, Some(&window_id), -10.0, 130.0, 0x110, false, 4);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.drag_drops, 1);
        assert_eq!(client.drag_cancellations, [cancelled.id().protocol_id()]);

        let stale = new_source();
        device.start_drag(Some(&stale), &surface, None, press);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.drag_cancellations, [cancelled.id().protocol_id(), stale.id().protocol_id()]);
        state.drop_files_to_window(&sid, Some(&window_id), &[r"C:\probe.txt".into()], Some((180.0, 130.0).into())).unwrap();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.drag_drops, 2);
        let (_, offer) = client.client_drag_offer.take().unwrap();
        let (mut read, write) = UnixStream::pair().unwrap();
        read.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        offer.receive(mime.into(), write.as_fd());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        drop(write);
        let mut uri = String::new();
        read.read_to_string(&mut uri).unwrap();
        assert_eq!(uri, "file:///mnt/c/probe.txt\r\n");
    }

    #[test]
    fn wayland_drag_crosses_stages_and_bridges_files_to_windows() {
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileExt;
        use wayland_client::protocol::wl_data_device_manager::DndAction;
        use crate::sink::CapturedDragTarget;

        let mut event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh, event_loop.handle(), "focusstage-cross-drag-test".into(), 1.0, 60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 }, Some(Box::new(TestSink(log.clone()))),
        );

        let setup_client = |dh: &mut DisplayHandle, state: &mut FocusStageState, display: &mut Display<FocusStageState>| {
            let (server_sock, client_sock) = UnixStream::pair().unwrap();
            let peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
            let conn = Connection::from_socket(client_sock).unwrap();
            let mut queue = conn.new_event_queue();
            let qh = queue.handle();
            let mut client = TestClient::default();
            let sid = state.ensure_stage_for_client(&peer);
            conn.display().get_registry(&qh, ());
            pump(display, state, &conn, &mut queue, &mut client);
            let manager = client.data_device_manager.clone().unwrap();
            let device = manager.get_data_device(client.seat.as_ref().unwrap(), &qh, ());
            let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
            let xdg = client.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
            let _toplevel = xdg.get_toplevel(&qh, ());
            surface.commit();
            pump(display, state, &conn, &mut queue, &mut client);
            let (_pool, buffer) = create_shm_buffer(client.shm.as_ref().unwrap(), &qh, 640, 480);
            xdg.set_window_geometry(0, 0, 640, 480);
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
            pump(display, state, &conn, &mut queue, &mut client);
            let wid = state.stages[&sid].active_window().unwrap().id.clone();
            (conn, queue, qh, client, sid, wid, manager, device, surface, _pool, buffer, xdg, _toplevel)
        };

        let (conn_a, mut queue_a, qh_a, mut client_a, sid_a, wid_a, mgr_a, dev_a, surf_a, ..) =
            setup_client(&mut dh, &mut state, &mut display);
        let (conn_b, mut queue_b, _qh_b, mut client_b, _sid_b, wid_b, ..) =
            setup_client(&mut dh, &mut state, &mut display);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let uri_payload = format!("file://{}\r\n", tmp.path().display()).into_bytes();
        let mime = "text/uri-list";

        // 1. Cross-stage Wayland drag from stage A to stage B.
        let src1 = mgr_a.create_data_source(&qh_a, ());
        src1.offer(mime.into());
        src1.set_actions(DndAction::Copy | DndAction::Move);
        let src1_id = src1.id().protocol_id();
        client_a.source_payloads.insert(src1_id, uri_payload.clone());
        state.send_window_button_at(&sid_a, Some(&wid_a), 20.0, 30.0, 0x110, true, 1);
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        let press1 = client_a.pointer_press_serial.unwrap();
        dev_a.start_drag(Some(&src1), &surf_a, None, press1);
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        state.set_captured_drag_target(
            &sid_a,
            &wid_a,
            Some(CapturedDragTarget::Window { window_id: wid_b.clone(), x: 140.0, y: 120.0 }),
        );
        state.send_window_pointer_motion_at(&sid_a, Some(&wid_a), 720.0, 120.0, 2);
        pump(&mut display, &mut state, &conn_b, &mut queue_b, &mut client_b);
        let (enter_b, offer_b) = client_b.client_drag_offer.take().unwrap();
        offer_b.set_actions(DndAction::Copy, DndAction::Copy);
        offer_b.accept(enter_b, Some(mime.into()));
        pump(&mut display, &mut state, &conn_b, &mut queue_b, &mut client_b);
        state.send_window_button_at(&sid_a, Some(&wid_a), 720.0, 120.0, 0x110, false, 3);
        pump(&mut display, &mut state, &conn_b, &mut queue_b, &mut client_b);
        assert_eq!(client_b.drag_drops, 1);
        let recv_file = tempfile::tempfile().unwrap();
        offer_b.receive(mime.into(), recv_file.as_fd());
        offer_b.finish();
        offer_b.destroy();
        pump(&mut display, &mut state, &conn_b, &mut queue_b, &mut client_b);
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        let mut received = vec![0; uri_payload.len()];
        recv_file.read_exact_at(&mut received, 0).unwrap();
        assert_eq!(received, uri_payload);
        assert_eq!(client_a.drag_finished, [src1_id]);

        // 2. Dragging outside starts Windows OLE drag; returning to stage B resumes Wayland, then dragging out and dropping finishes as Copy.
        let src2 = mgr_a.create_data_source(&qh_a, ());
        src2.offer(mime.into());
        src2.set_actions(DndAction::Copy | DndAction::Move);
        let src2_id = src2.id().protocol_id();
        client_a.source_payloads.insert(src2_id, uri_payload);
        state.send_window_button_at(&sid_a, Some(&wid_a), 20.0, 30.0, 0x110, true, 4);
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        let press2 = client_a.pointer_press_serial.unwrap();
        dev_a.start_drag(Some(&src2), &surf_a, None, press2);
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        state.set_captured_drag_target(&sid_a, &wid_a, Some(CapturedDragTarget::External));
        state.send_window_pointer_motion_at(&sid_a, Some(&wid_a), -40.0, 50.0, 5);
        for _ in 0..40 {
            pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
            event_loop.dispatch(Some(std::time::Duration::from_millis(10)), &mut state).unwrap();
            if !log.lock().unwrap().file_drags_started.is_empty() {
                break;
            }
        }
        let started = log.lock().unwrap().file_drags_started.clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, wid_a);
        assert_eq!(started[0].2.len(), 1);
        assert!(started[0].2[0].ends_with(tmp.path().file_name().unwrap().to_str().unwrap()));

        // Re-entering stage B while button is held resumes Wayland drag on window B.
        state.handle_file_drag_ended(
            &wid_a,
            started[0].1,
            false,
            true,
            Some(CapturedDragTarget::Window { window_id: wid_b.clone(), x: 80.0, y: 90.0 }),
        );
        pump(&mut display, &mut state, &conn_b, &mut queue_b, &mut client_b);
        assert!(client_b.client_drag_offer.take().is_some());

        // Moving back outside starts OLE drag again; dropping on Windows finishes the Wayland source.
        state.set_captured_drag_target(&sid_a, &wid_a, Some(CapturedDragTarget::External));
        state.send_window_pointer_motion_at(&sid_a, Some(&wid_a), -40.0, 50.0, 6);
        assert_eq!(log.lock().unwrap().file_drags_started.len(), 2);
        state.handle_file_drag_ended(&wid_a, started[0].1, true, false, Some(CapturedDragTarget::External));
        pump(&mut display, &mut state, &conn_a, &mut queue_a, &mut client_a);
        assert_eq!(client_a.drag_finished, [src1_id, src2_id]);
        assert!(state.stages[&sid_a].input.drag.is_none());
        assert!(state.stages[&sid_a].input.pressed_buttons.is_empty());
    }

    #[test]
    fn same_stage_windows_map_resize_scale_and_close_independently() {
        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh, event_loop.handle(), "focusstage-multitest".into(),
            1.25, 60, SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            Some(Box::new(TestSink(log.clone()))),
        );
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let _client = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
        let conn = Connection::from_socket(client_sock).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();
        let _registry = conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let compositor = client.compositor.as_ref().unwrap().clone();
        let wm = client.wm_base.as_ref().unwrap().clone();
        let scale = client.fractional_scale.as_ref().unwrap().clone();
        let shm = client.shm.as_ref().unwrap().clone();

        let mut make_window = |title: &str, parent: Option<&c_toplevel::XdgToplevel>, w: i32, h: i32| {
            let surface = compositor.create_surface(&qh, ());
            let xdg = wm.get_xdg_surface(&surface, &qh, ());
            let window = xdg.get_toplevel(&qh, ());
            window.set_title(title.to_string());
            window.set_parent(parent);
            let fraction = scale.get_fractional_scale(&surface, &qh, window.id().protocol_id());
            surface.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut client);
            let buffer_scale = if parent.is_some() { 2 } else { 1 };
            let (pool, buffer) = create_shm_buffer(&shm, &qh, w * buffer_scale, h * buffer_scale);
            surface.set_buffer_scale(buffer_scale);
            if parent.is_none() {
                xdg.set_window_geometry(0, 0, w, h);
            }
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut client);
            (window, surface, xdg, fraction, pool, buffer)
        };
        let (first, first_surface, first_xdg, _, _, _) = make_window("First", None, 640, 480);
        let (second, second_surface, second_xdg, second_fraction, _, _) = make_window("Second", Some(&first), 320, 240);
        let (sid, first_id, second_id) = {
            let log = log.lock().unwrap();
            assert_eq!(log.shown.len(), 2);
            let (sid, first_id, parent) = &log.shown[0];
            assert!(parent.is_none());
            assert_eq!(log.shown[1].0, *sid);
            assert_eq!(log.shown[1].2.as_deref(), Some(first_id.as_str()));
            (sid.clone(), first_id.clone(), log.shown[1].1.clone())
        };
        assert_eq!(state.stages.len(), 1);
        let initial = state.stages[&sid].frame_seq;
        let settled = state.stages[&sid].settle_seq;
        state.stages.get_mut(&sid).unwrap().window_mut(&first_id).unwrap().dirty = false;
        first_surface.frame(&qh, ());
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].frame_seq, initial);
        assert!(!state.stages[&sid].window(&first_id).unwrap().dirty);
        first_surface.damage(2, 3, 4, 5);
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].frame_seq, initial + 1);
        assert!(state.stages[&sid].window(&first_id).unwrap().dirty);
        first_surface.damage(2, 3, 0, 5);
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].frame_seq, initial + 1);
        assert_eq!(state.stages[&sid].settle_seq, settled);
        first_surface.damage(0, 0, 80, 80);
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].frame_seq, initial + 2);
        assert_eq!(state.stages[&sid].settle_seq, settled + 1);
        first_xdg.set_window_geometry(2, 3, 638, 477);
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].frame_seq, initial + 3);
        assert_eq!(state.stages[&sid].settle_seq, settled + 2);
        assert_eq!(state.stages[&sid].window(&first_id).unwrap().title, "First");
        assert_eq!(state.stages[&sid].window(&second_id).unwrap().title, "Second");
        assert_eq!(client.preferred_scales[&first.id().protocol_id()], 150);
        assert_eq!(client.preferred_scales[&second.id().protocol_id()], 150);

        assert_eq!(state.stages[&sid].window(&second_id).unwrap().logical_size, (320, 240).into());
        let host_state = crate::sink::WindowState {
            rect: SavedRect { x: -1800, y: 0, width: 511, height: 480 },
            scale: 1.5,
            monitor: Some(SavedRect { x: -1920, y: 0, width: 1920, height: 1080 }),
            pixel_size: Some([767, 720]),
            activated: true, maximized: false, fullscreen: false, resizing: true, tiled: [true, false, true, true],
        };
        state.update_window_from_sink(&second_id, host_state.clone());
        state.send_window_pointer_motion_at(&sid, Some(&first_id), 20.0, 30.0, 0);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(state.stages[&sid].active_window().unwrap().id, second_id);
        assert_eq!(client.pointer_enters.last().unwrap().0, first_surface.id().protocol_id());

        // Client-side title bars may be separate surfaces above the toplevel buffer.
        let title = compositor.create_surface(&qh, ());
        let decoration = client.subcompositor.as_ref().unwrap().get_subsurface(&title, &first_surface, &qh, ());
        decoration.set_position(2, 3);
        let (_pool, title_buffer) = create_shm_buffer(&shm, &qh, 638, 24);
        title.attach(Some(&title_buffer), 0, 0);
        title.commit();
        first_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        state.send_window_pointer_motion_at(&sid, Some(&first_id), 20.0, 10.0, 0);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let title_enter = (title.id().protocol_id(), 20.0, 10.0);
        assert_eq!(client.pointer_enters.last(), Some(&title_enter));
        state.send_window_button_at(&sid, Some(&first_id), 20.0, 10.0, 0x110, true, 1);
        state.send_window_pointer_motion_at(&sid, Some(&first_id), 28.0, 40.0, 2);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.pointer_enters.last(), Some(&title_enter));
        state.send_window_button_at(&sid, Some(&first_id), 28.0, 40.0, 0x110, false, 3);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.pointer_enters.last().unwrap().0, first_surface.id().protocol_id());

        assert_eq!(client.preferred_scales[&first.id().protocol_id()], 150);
        assert_eq!(client.preferred_scales[&second.id().protocol_id()], 180);
        let (width, height, flags) = &client.window_configures[&second.id().protocol_id()];
        assert_eq!((*width, *height), (511, 480));
        for flag in [c_toplevel::State::Activated, c_toplevel::State::Resizing, c_toplevel::State::TiledLeft] {
            assert!(flags.contains(&(flag as u32)));
        }
        assert_eq!(state.stages[&sid].window(&first_id).unwrap().logical_size, (640, 480).into());
        let second_window = state.stages[&sid].window(&second_id).unwrap();
        assert_eq!(state.offscreen.render_window_bgra_into(Some(second_window), second_window.logical_size,
            second_window.scale, &[], &mut Vec::new()).unwrap(), (767, 720));

        let fullscreen = c_toplevel::State::Fullscreen as u32;
        let maximized = c_toplevel::State::Maximized as u32;
        second.set_fullscreen(None);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(log.lock().unwrap().fullscreen_requests, [(second_id.clone(), true)]);
        assert!(!client.window_configures[&second.id().protocol_id()].2.contains(&fullscreen));

        state.update_window_from_sink(&second_id, crate::sink::WindowState {
            rect: SavedRect { x: -1920, y: 0, width: 1280, height: 720 },
            pixel_size: Some([1920, 1080]),
            fullscreen: true, resizing: false, tiled: [false; 4], ..host_state.clone()
        });
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (width, height, flags) = &client.window_configures[&second.id().protocol_id()];
        assert_eq!((*width, *height), (1280, 720));
        assert!(flags.contains(&fullscreen));
        assert_eq!(state.stages[&sid].window(&second_id).unwrap().saved_rect, host_state.rect);

        second.unset_fullscreen();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(log.lock().unwrap().fullscreen_requests.last(), Some(&(second_id.clone(), false)));
        state.update_window_from_sink(&second_id, crate::sink::WindowState {
            maximized: true, ..host_state.clone()
        });
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (width, height, flags) = &client.window_configures[&second.id().protocol_id()];
        assert_eq!((*width, *height), (511, 480));
        assert!(!flags.contains(&fullscreen));
        assert!(flags.contains(&maximized));

        let shift = 42;
        state.send_sink_key(&sid, &first_id, shift, true);
        state.update_window_from_sink(&second_id, host_state.clone());
        state.send_sink_key(&sid, &second_id, shift, true);
        state.send_sink_key(&sid, &first_id, shift, false);
        assert_eq!(state.stages[&sid].active_window().unwrap().id, second_id);
        assert!(state.stages[&sid].input.pressed_keys.contains(&shift));
        state.send_sink_key(&sid, &second_id, shift, false);
        assert!(state.stages[&sid].input.pressed_keys.is_empty());

        let inactive = crate::sink::WindowState { activated: false, ..host_state };
        state.update_window_from_sink(&second_id, inactive.clone());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let activated = c_toplevel::State::Activated as u32;
        assert!(!client.window_configures[&second.id().protocol_id()].2.contains(&activated));
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        state.handle_control_message(crate::control::ControlServerMessage {
            request: crate::protocol::ControlRequest::Acquire {
                stage_id: sid.clone(), ttl_s: 30, hide: false, agent_name: None,
            },
            reply_tx,
            peer_pid: None,
        });
        let crate::protocol::ControlResponse::Acquired { lease, .. } = reply_rx.try_recv().unwrap() else {
            panic!("expected a lease");
        };
        let _data_device = client.data_device_manager.as_ref().unwrap()
            .get_data_device(client.seat.as_ref().unwrap(), &qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        state.activate_sink_window(&sid, &first_id);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        state.handle_control_message(crate::control::ControlServerMessage {
            request: crate::protocol::ControlRequest::DropFilesLeased {
                lease, paths: vec!["/tmp/drop-point.txt".into()], position: Some([20.0, 30.0]),
            },
            reply_tx,
            peer_pid: None,
        });
        assert!(matches!(reply_rx.try_recv().unwrap(), crate::protocol::ControlResponse::Ok));
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.drop_enters, [(first_surface.id().protocol_id(), 22.0, 33.0)]);

        state.activate_sink_window(&sid, &second_id);
        state.update_window_from_sink(&second_id, inactive.clone());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.window_configures[&second.id().protocol_id()].2.contains(&activated));
        state.human_takeover(Some(&sid));

        second_surface.attach(None, 0, 0);
        second_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let unmapped = state.stages[&sid].window(&second_id).unwrap();
        assert!(!unmapped.mapped);
        assert!(state.offscreen.render_launch_png(unmapped, &[]).unwrap().is_none());
        let (_pool, new_buffer) = create_shm_buffer(&shm, &qh, 768, 576);
        second_surface.attach(Some(&new_buffer), 0, 0);
        second_surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let remapped = state.stages[&sid].window(&second_id).unwrap();
        assert!(remapped.mapped);
        assert_eq!(remapped.logical_size, (384, 288).into());
        assert_eq!(remapped.pixel_size, None);
        assert_eq!((remapped.saved_rect.width, remapped.saved_rect.height), (384, 288));
        assert_eq!(state.offscreen.render_window_bgra_into(Some(remapped), remapped.logical_size,
            remapped.scale, &[], &mut Vec::new()).unwrap(), (576, 432));

        state.send_sink_key(&sid, &second_id, shift, true);
        state.send_window_button_at(&sid, Some(&second_id), 20.0, 30.0, 0x110, true, 0);
        state.close_window_from_sink(&second_id);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.window_closes, [second.id().protocol_id()]);
        assert_eq!(state.stages[&sid].windows.len(), 2, "wait for the client to close the window");
        second_fraction.destroy();
        second.destroy();
        second_xdg.destroy();
        second_surface.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(log.lock().unwrap().destroyed.as_slice(), std::slice::from_ref(&second_id));
        let stage = &state.stages[&sid];
        assert_eq!(stage.active_window().unwrap().id, first_id);
        assert!(stage.input.pressed_keys.is_empty());
        assert!(stage.input.pressed_buttons.is_empty());
        assert!(stage.input.pointer_window_id.is_none());
        state.send_window_pointer_motion_at(&sid, Some(&first_id), 20.0, 30.0, 0);
        state.leave_window_pointer(&sid, &first_id);
        assert!(state.stages[&sid].input.current_pointer_target.is_none());

        state.set_stage_display(&sid, DisplayState::Hidden);
        state.update_window_from_sink(&first_id, inactive);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(log.lock().unwrap().hidden, [second_id, first_id]);
        assert!(client.window_configures[&first.id().protocol_id()].2.contains(&activated));
    }

    #[test]
    fn fractional_client_scale_preserves_native_buffer_dimensions() {
        use smithay::backend::renderer::element::surface::{
            render_elements_from_surface_tree, WaylandSurfaceRenderElement,
        };
        use smithay::backend::renderer::element::{Element, Kind};

        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let mut state = FocusStageState::new(
            &dh, event_loop.handle(), "focusstage-buffer-scale-test".into(), 1.25, 60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 }, None,
        );
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let peer_data = Arc::new(ClientState::default());
        peer_data.compositor_state.set_client_scale(1.25);
        let peer = dh.insert_client(server_sock, peer_data).unwrap();
        let conn = Connection::from_socket(client_sock).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();
        conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
        let (_pool, buffer) = create_shm_buffer(client.shm.as_ref().unwrap(), &qh, 227, 237);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let surface = peer.object_from_protocol_id::<WlSurface>(&dh, surface.id().protocol_id()).unwrap();
        let elements: Vec<WaylandSurfaceRenderElement<_>> = render_elements_from_surface_tree(
            &mut state.offscreen.renderer, &surface, (13, 19), 1.25, 1.0, Kind::Unspecified,
        );
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].geometry(1.25.into()), Rectangle::new((13, 19).into(), (227, 237).into()));
        assert_eq!(elements[0].geometry(1.5.into()).size, (272, 284).into());
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
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            None,
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
        state.stages.get_mut(&stage_id).unwrap().windows[0].logical_size = (800, 600).into();

        let (_pool_tl, buf_tl) = create_shm_buffer(&shm, &qh, 800, 600);
        tl_xdg.set_window_geometry(0, 0, 800, 600);
        tl_wl.attach(Some(&buf_tl), 0, 0);
        tl_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 1. Direct popup near bottom-right corner (720, 520), size 200x200, FlipX|FlipY -> (520, 320, 200, 200)
        client.seat.as_ref().unwrap().get_keyboard(&qh, ());
        state.send_stage_button(&stage_id, 720.0, 520.0, 273, true);
        state.send_stage_button(&stage_id, 720.0, 520.0, 273, false);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let press = client.pointer_press_serial.unwrap();
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
        p1.grab(client.seat.as_ref().unwrap(), press);
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
        _p2.grab(client.seat.as_ref().unwrap(), press);
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

        let keyboard = |state: &FocusStageState| state.stages[&stage_id].input.current_keyboard_target.as_ref().unwrap().wl_surface().unwrap().id().protocol_id();
        assert_eq!(keyboard(&state), p2_wl.id().protocol_id());
        state.stages.get_mut(&stage_id).unwrap().windows[0].dirty = false;
        _p2.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(state.stages[&stage_id].windows[0].dirty);
        assert_eq!(keyboard(&state), p1_wl.id().protocol_id());

        let before = client.button_events;
        state.send_stage_button(&stage_id, 20.0, 20.0, 272, true);
        state.send_stage_button(&stage_id, 20.0, 20.0, 272, false);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.popup_done, [p1.id().protocol_id()]);
        assert_eq!(client.button_events, before);
        assert_eq!(keyboard(&state), tl_wl.id().protocol_id());
        state.send_stage_button(&stage_id, 20.0, 20.0, 272, true);
        state.send_stage_button(&stage_id, 20.0, 20.0, 272, false);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.button_events, before + 2);

        let _ = event_loop.dispatch(Some(std::time::Duration::ZERO), &mut state);
    }

    #[test]
    fn clipboard_inherits_host_and_syncs_across_human_stages_for_text_image_and_clear() {
        use std::io::Read;
        use std::os::fd::AsFd;

        let mut event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-clipboard-test".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            Some(Box::new(TestSink(log.clone()))),
        );

        // Host already had clipboard text before any Stage connected.
        state.sync_from_host_clipboard("from-windows".into());

        let mut connect = || {
            let (server_sock, client_sock) = UnixStream::pair().unwrap();
            let peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
            let conn = Connection::from_socket(client_sock).unwrap();
            let queue = conn.new_event_queue();
            conn.display().get_registry(&queue.handle(), ());
            let sid = state.ensure_stage_for_client(&peer);
            (sid, conn, queue, TestClient::default())
        };

        let (sid1, conn1, mut q1, mut c1) = connect();
        let (sid2, conn2, mut q2, mut c2) = connect();
        let (sid_agent, conn_agent, mut q_agent, mut c_agent) = connect();
        state.stages.get_mut(&sid_agent).unwrap().input_owner = crate::protocol::InputOwner::Agent {
            lease_id: "lease-agent".into(),
            agent_name: Some("agent".into()),
        };

        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
        pump(&mut display, &mut state, &conn_agent, &mut q_agent, &mut c_agent);

        let read_offer = |offer: &wl_data_offer::WlDataOffer,
                          mime: &str,
                          display: &mut Display<FocusStageState>,
                          state: &mut FocusStageState,
                          conn: &Connection,
                          queue: &mut wayland_client::EventQueue<TestClient>,
                          client: &mut TestClient| {
            let mut fds = [0i32; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            let writer = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
            offer.receive(mime.to_string(), writer.as_fd());
            drop(writer);
            pump(display, state, conn, queue, client);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).unwrap();
            out
        };

        // 1. New Human stage inherits latest_host_clipboard on get_data_device.
        let qh1 = q1.handle();
        let _dev1 = c1.data_device_manager.as_ref().unwrap()
            .get_data_device(c1.seat.as_ref().unwrap(), &qh1, ());
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        let offer1 = c1.active_selection.clone().expect("stage 1 should inherit host clipboard");
        assert!(c1.offer_mimes[&offer1.id().protocol_id()].contains(&"text/plain;charset=utf-8".to_string()));
        assert_eq!(
            read_offer(&offer1, "text/plain;charset=utf-8", &mut display, &mut state, &conn1, &mut q1, &mut c1),
            b"from-windows"
        );

        // Agent stage must remain isolated from host clipboard.
        let qh_agent = q_agent.handle();
        let _dev_agent = c_agent.data_device_manager.as_ref().unwrap()
            .get_data_device(c_agent.seat.as_ref().unwrap(), &qh_agent, ());
        pump(&mut display, &mut state, &conn_agent, &mut q_agent, &mut c_agent);
        assert!(c_agent.active_selection.is_none());

        // 2. Stage 2 copies an image (offering text/html + image/png) -> propagates to Stage 1 and Host.
        let qh2 = q2.handle();
        let dev2 = c2.data_device_manager.as_ref().unwrap()
            .get_data_device(c2.seat.as_ref().unwrap(), &qh2, ());
        let src2 = c2.data_device_manager.as_ref().unwrap().create_data_source(&qh2, ());
        let fake_png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        c2.source_payloads.insert(src2.id().protocol_id(), fake_png.clone());
        src2.offer("text/html".into());
        src2.offer("image/png".into());
        dev2.set_selection(Some(&src2), 1);

        // Pump client 2 so it responds to WlDataSource::Send, then dispatch event_loop for clipboard_update_rx.
        for _ in 0..10 {
            pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
            let _ = event_loop.dispatch(Some(std::time::Duration::from_millis(5)), &mut state);
            if !log.lock().unwrap().host_clipboard_images.is_empty() {
                break;
            }
        }
        assert_eq!(log.lock().unwrap().host_clipboard_images, vec![fake_png.clone()]);
        assert_eq!(
            state.stages[&sid2].clipboard.content,
            Some(crate::stage::ClipboardContent::ImagePng(fake_png.clone()))
        );
        assert_eq!(
            state.stages[&sid1].clipboard.content,
            Some(crate::stage::ClipboardContent::ImagePng(fake_png.clone()))
        );
        assert!(state.stages[&sid_agent].clipboard.content.is_none());

        let manager = c2.data_device_manager.clone().unwrap();
        let _late = manager.get_data_device(c2.seat.as_ref().unwrap(), &qh2, ());
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
        let late_offer = c2.active_selection.clone().unwrap();
        assert!(c2.offer_mimes[&late_offer.id().protocol_id()].iter().any(|mime| mime == "text/html"));
        assert!(read_offer(&late_offer, "text/rtf", &mut display, &mut state, &conn2, &mut q2, &mut c2).is_empty());
        c2.source_payloads.insert(src2.id().protocol_id(), b"<b>native format</b>".to_vec());
        assert_eq!(read_offer(&late_offer, "text/html", &mut display, &mut state, &conn2, &mut q2, &mut c2), b"<b>native format</b>");
        src2.destroy();
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
        assert_eq!(
            read_offer(&late_offer, "image/png", &mut display, &mut state, &conn2, &mut q2, &mut c2),
            fake_png
        );
        let _after_source_exit = manager.get_data_device(c2.seat.as_ref().unwrap(), &qh2, ());
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
        let cached_offer = c2.active_selection.clone().unwrap();
        assert_eq!(c2.offer_mimes[&cached_offer.id().protocol_id()], ["image/png"]);

        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        let offer1_img = c1.active_selection.clone().expect("stage 1 should receive PNG offer from stage 2");
        assert_eq!(c1.offer_mimes[&offer1_img.id().protocol_id()], vec!["image/png".to_string()]);
        assert_eq!(
            read_offer(&offer1_img, "image/png", &mut display, &mut state, &conn1, &mut q1, &mut c1),
            fake_png
        );

        // 3. Host clears clipboard -> Human stages clear their selection.
        let cleared_before = c1.selection_cleared_count;
        state.sync_from_host_clipboard_content(None);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        assert_eq!(c1.selection_cleared_count, cleared_before + 1);
        assert!(c1.active_selection.is_none());
        assert!(state.stages[&sid1].clipboard.content.is_none());
        assert!(state.stages[&sid2].clipboard.content.is_none());
    }

    #[test]
    fn agent_overlay_syncs_actions_visibility_and_esc_cancel_takeover() {
        use std::sync::mpsc;
        use std::time::Duration;
        use crate::control::ControlServerMessage;
        use crate::protocol::{Action, ControlRequest, ControlResponse, InputOwner};

        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-overlay-test".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            Some(Box::new(TestSink(log.clone()))),
        );
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let _client = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
        let conn = Connection::from_socket(client_sock).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();
        let _registry = conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let compositor = client.compositor.as_ref().unwrap().clone();
        let wm = client.wm_base.as_ref().unwrap().clone();
        let shm = client.shm.as_ref().unwrap().clone();
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg.get_toplevel(&qh, ());
        surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (_pool, buffer) = create_shm_buffer(&shm, &qh, 640, 480);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let (sid, wid) = {
            let log = log.lock().unwrap();
            assert_eq!(log.shown.len(), 1);
            (log.shown[0].0.clone(), log.shown[0].1.clone())
        };
        log.lock().unwrap().overlays.clear();

        let send_ctrl = |state: &mut FocusStageState, request: ControlRequest| -> ControlResponse {
            let (tx, rx) = mpsc::channel();
            state.handle_control_message(ControlServerMessage {
                request,
                reply_tx: tx,
                peer_pid: None,
            });
            rx.recv().unwrap()
        };

        // 1. Acquire with hide=false preserves Desktop visibility and turns on the overlay.
        let resp = send_ctrl(
            &mut state,
            ControlRequest::Acquire {
                stage_id: sid.clone(),
                ttl_s: 60,
                hide: false,
                agent_name: Some("Claude".into()),
            },
        );
        assert!(matches!(resp, ControlResponse::Acquired { .. }));
        assert_eq!(state.stages[&sid].display, DisplayState::Desktop);
        assert!(matches!(state.stages[&sid].input_owner, InputOwner::Agent { .. }));
        {
            let overlays = &log.lock().unwrap().overlays;
            let last = overlays.last().expect("acquire should sync active overlay");
            assert_eq!(last.window_id, wid);
            assert!(last.active);
            assert_eq!(last.agent_name.as_deref(), Some("Claude"));
            assert!(!last.pointer_visible);
            assert_eq!(last.pointer_state, "idle");
        }

        // 2. Pointer actions sync virtual cursor coordinates and action states.
        log.lock().unwrap().overlays.clear();
        state.execute_single_action(&sid, &Action::Move { x: 100.0, y: 120.0 }).unwrap();
        state.execute_single_action(&sid, &Action::Click { x: 200.0, y: 240.0, button: None }).unwrap();
        state.execute_single_action(
            &sid,
            &Action::Drag { from_x: 10.0, from_y: 20.0, to_x: 50.0, to_y: 60.0, button: None, steps: Some(2) },
        ).unwrap();
        state.execute_single_action(&sid, &Action::Scroll { x: 300.0, y: 150.0, dx: 0.0, dy: 45.0 }).unwrap();
        {
            let overlays = &log.lock().unwrap().overlays;
            assert!(overlays.iter().any(|o| o.active && o.pointer_visible && o.pointer_state == "move" && (o.pointer_x - 100.0).abs() < 1e-6));
            assert!(overlays.iter().any(|o| o.active && o.pointer_visible && o.pointer_state == "click" && (o.pointer_x - 200.0).abs() < 1e-6));
            assert!(overlays.iter().any(|o| o.active && o.pointer_visible && o.pointer_state == "down" && (o.pointer_x - 10.0).abs() < 1e-6));
            assert!(overlays.iter().any(|o| o.active && o.pointer_visible && o.pointer_state == "drag"));
            let last = overlays.last().unwrap();
            assert_eq!(last.pointer_state, "scroll");
            assert!(last.pointer_visible);
            assert!((last.pointer_x - 300.0).abs() < 1e-6 && (last.pointer_y - 150.0).abs() < 1e-6);
        }

        // 3. Non-Esc keys are ignored while Agent owns the stage; Esc triggers human_takeover and clears overlay.
        log.lock().unwrap().overlays.clear();
        state.handle_sink_key(&wid, 30, true); // 'a' key
        assert!(matches!(state.stages[&sid].input_owner, InputOwner::Agent { .. }));
        state.handle_sink_key(&wid, 1, true); // KEY_ESC
        assert_eq!(state.stages[&sid].input_owner, InputOwner::Human);
        assert!(!state.stages[&sid].input.pressed_keys.contains(&1));
        {
            let overlays = &log.lock().unwrap().overlays;
            let last = overlays.last().expect("Esc takeover should clear overlay");
            assert!(!last.active);
            assert!(!last.pointer_visible);
        }

        // 4. Acquire with hide=true hides stage; switching back to Desktop shows active overlay; banner cancel clears it.
        log.lock().unwrap().overlays.clear();
        let resp = send_ctrl(
            &mut state,
            ControlRequest::Acquire {
                stage_id: sid.clone(),
                ttl_s: 60,
                hide: true,
                agent_name: Some("Tester".into()),
            },
        );
        assert!(matches!(resp, ControlResponse::Acquired { .. }));
        assert_eq!(state.stages[&sid].display, DisplayState::Hidden);
        assert!(log.lock().unwrap().overlays.is_empty());

        assert!(matches!(state.set_stage_display(&sid, DisplayState::Desktop), ControlResponse::Ok));
        {
            let overlays = &log.lock().unwrap().overlays;
            let last = overlays.last().expect("switching Agent stage to Desktop should sync active overlay");
            assert!(last.active);
            assert_eq!(last.agent_name.as_deref(), Some("Tester"));
        }

        log.lock().unwrap().overlays.clear();
        state.handle_sink_cancel_takeover(&wid);
        assert_eq!(state.stages[&sid].input_owner, InputOwner::Human);
        assert!(!log.lock().unwrap().overlays.last().unwrap().active);

        // 5. Lease expiry reverts to Human and clears overlay.
        let _ = send_ctrl(
            &mut state,
            ControlRequest::Acquire {
                stage_id: sid.clone(),
                ttl_s: 60,
                hide: false,
                agent_name: None,
            },
        );
        log.lock().unwrap().overlays.clear();
        state.send_stage_evdev_key(&sid, 29, true); // KEY_LEFTCTRL
        state.send_stage_button(&sid, 50.0, 50.0, 0x110, true); // BTN_LEFT
        assert!(state.stages[&sid].input.pressed_keys.contains(&29));
        assert!(state.stages[&sid].input.pressed_buttons.contains(&0x110));
        state.stages.get_mut(&sid).unwrap().lease.as_mut().unwrap().expires_at =
            Instant::now() - Duration::from_secs(1);
        state.expire_leases();
        assert_eq!(state.stages[&sid].input_owner, InputOwner::Human);
        assert!(state.stages[&sid].input.pressed_keys.is_empty());
        assert!(state.stages[&sid].input.pressed_buttons.is_empty());
        assert!(!log.lock().unwrap().overlays.last().unwrap().active);
    }

    #[test]
    fn lease_activity_and_late_release_follow_the_idle_deadline() {
        use std::sync::mpsc;
        use std::time::Duration;
        use crate::control::ControlServerMessage;
        use crate::protocol::{ActStatus, Action, ControlRequest, ControlResponse, InputOwner};

        let event_loop = EventLoop::try_new().unwrap();
        let display = Display::<FocusStageState>::new().unwrap();
        let mut state = FocusStageState::new(
            &display.handle(), event_loop.handle(), "focusstage-lease-test".into(), 1.0, 60,
            SavedRect { x: 0, y: 0, width: 640, height: 480 }, None,
        );
        let sid = "stage-1".to_string();
        state.stages.insert(sid.clone(), Stage::new(
            sid.clone(), "test".into(), Vec::new(), StageKind::Wayland,
            640, 480, DisplayState::Desktop, 1.0,
        ));
        let dispatch = |state: &mut FocusStageState, request| {
            let (reply_tx, rx) = mpsc::channel();
            state.handle_control_message(ControlServerMessage { request, reply_tx, peer_pid: None });
            rx
        };
        let send = |state: &mut FocusStageState, request| dispatch(state, request).try_recv().unwrap();
        let acquire = |state: &mut FocusStageState| {
            match send(state, ControlRequest::Acquire {
                stage_id: sid.clone(), ttl_s: 60, hide: true, agent_name: None,
            }) {
                ControlResponse::Acquired { lease, .. } => lease,
                other => panic!("expected a lease, got {other:?}"),
            }
        };
        let release = |state: &mut FocusStageState, lease: &str| {
            send(state, ControlRequest::Release { lease: lease.into(), show_on_desktop: true })
        };
        let set_deadline = |state: &mut FocusStageState, at| {
            state.stages.get_mut(&sid).unwrap().lease.as_mut().unwrap().expires_at = at;
        };
        let observe = |lease| ControlRequest::Observe {
            lease, stage_id: Some(sid.clone()), image: false, a11y: false,
        };
        let lease = acquire(&mut state);
        let deadline = Instant::now() + Duration::from_secs(5);
        set_deadline(&mut state, deadline);
        assert!(matches!(send(&mut state, observe(None)), ControlResponse::Observed { .. }));
        assert_eq!(state.stages[&sid].lease.as_ref().unwrap().expires_at, deadline);
        assert!(matches!(send(&mut state, observe(Some(lease.clone()))), ControlResponse::Observed { .. }));
        assert!(state.stages[&sid].lease.as_ref().unwrap().expires_at > deadline);

        assert!(matches!(send(&mut state, ControlRequest::Renew { lease: lease.clone(), ttl_s: 20 }),
            ControlResponse::Acquired { expires_in_s: 20, .. }));
        for request in [
            ControlRequest::ClipboardGet { lease: Some(lease.clone()), stage_id: None },
            ControlRequest::ClipboardSet { lease: Some(lease.clone()), stage_id: None, text: "test".into() },
            ControlRequest::DropFilesLeased { lease: lease.clone(), paths: vec!["/tmp/test".into()], position: None },
        ] {
            let before = Instant::now();
            set_deadline(&mut state, before + Duration::from_secs(5));
            send(&mut state, request);
            let after = Instant::now();
            let expires_at = state.stages[&sid].lease.as_ref().unwrap().expires_at;
            assert!((before + Duration::from_secs(20)..=after + Duration::from_secs(20)).contains(&expires_at));
        }

        set_deadline(&mut state, Instant::now() + Duration::from_secs(5));
        let pending = dispatch(&mut state, ControlRequest::Act {
            lease: lease.clone(), actions: vec![Action::Wait { ms: 100 }], based_on: None,
            settle: Some(false), settle_ms: None,
        });
        assert!(state.stages[&sid].lease.as_ref().unwrap().expires_at > Instant::now() + Duration::from_secs(5));
        set_deadline(&mut state, Instant::now());
        state.tick_pending_act_jobs();
        assert!(matches!(pending.try_recv().unwrap(), ControlResponse::ActResult {
            act_status: ActStatus::Expired, executed_steps: 1, ..
        }));
        assert!(matches!(send(&mut state, ControlRequest::Renew { lease: lease.clone(), ttl_s: 60 }),
            ControlResponse::Error { code, .. } if code == "expired"));
        assert_eq!(state.stages[&sid].input_owner, InputOwner::Human);
        assert_eq!(state.stages[&sid].display, DisplayState::Hidden);
        assert!(matches!(release(&mut state, &lease), ControlResponse::Ok));
        assert_eq!(state.stages[&sid].display, DisplayState::Desktop);

        let replaced_lease = acquire(&mut state);
        set_deadline(&mut state, Instant::now());
        let new_lease = acquire(&mut state);
        assert!(matches!(release(&mut state, &replaced_lease), ControlResponse::Ok));
        assert!(matches!(release(&mut state, &lease), ControlResponse::Ok));
        assert_eq!(state.stages[&sid].display, DisplayState::Hidden);
        assert_eq!(state.stages[&sid].lease.as_ref().unwrap().lease_id, new_lease);
        state.human_takeover(Some(&sid));
        assert!(matches!(send(&mut state, observe(Some(new_lease.clone()))),
            ControlResponse::Error { code, .. } if code == "preempted"));
        assert!(matches!(release(&mut state, &new_lease), ControlResponse::Ok));
        assert!(matches!(release(&mut state, "unknown"),
            ControlResponse::Error { code, .. } if code == "invalid_lease"));
    }

    #[test]
    fn wayland_popup_grab_lifecycle_and_idle_parent_repaint() {
        use wayland_client::Proxy;

        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display: Display<FocusStageState> = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-popup-lifecycle-test".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            Some(Box::new(TestSink(log.clone()))),
        );

        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let _peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
        let conn = Connection::from_socket(client_sock).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut client = TestClient::default();
        let _reg = conn.display().get_registry(&qh, ());
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let compositor = client.compositor.as_ref().unwrap().clone();
        let wm_base = client.wm_base.as_ref().unwrap().clone();
        let shm = client.shm.as_ref().unwrap().clone();
        let seat = client.seat.as_ref().unwrap().clone();
        seat.get_keyboard(&qh, ());

        let tl_wl = compositor.create_surface(&qh, ());
        let tl_xdg = wm_base.get_xdg_surface(&tl_wl, &qh, ());
        client.toplevel_xdg_id = Some(tl_xdg.id().protocol_id());
        let _tl = tl_xdg.get_toplevel(&qh, ());
        tl_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let stage_id = state.stages.keys().next().unwrap().clone();
        let w1_id = state.stages[&stage_id].windows[0].id.clone();

        let (_pool_tl, buf_tl) = create_shm_buffer_rgba(&shm, &qh, 800, 600, [255, 0, 0, 255]);
        tl_xdg.set_window_geometry(0, 0, 800, 600);
        tl_wl.attach(Some(&buf_tl), 0, 0);
        tl_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let pixel_at = |state: &mut FocusStageState, wid: &str, x: usize, y: usize| -> [u8; 4] {
            let mut out = Vec::new();
            let window = state.stages[&stage_id].window(wid).unwrap();
            let (w, _h) = state
                .offscreen
                .render_window_bgra_into(Some(window), window.logical_size, 1.0, &[], &mut out)
                .unwrap();
            let idx = (y * w as usize + x) * 4;
            out[idx..idx + 4].try_into().unwrap()
        };
        let keyboard_id = |state: &FocusStageState| {
            state.stages[&stage_id]
                .input
                .current_keyboard_target
                .as_ref()
                .unwrap()
                .wl_surface()
                .unwrap()
                .id()
                .protocol_id()
        };

        // 1. Invalid grab serial is immediately dismissed and marks parent dirty.
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 273, true, 1);
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 273, false, 2);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let press_serial = client.pointer_press_serial.unwrap();

        let bad_pos = wm_base.create_positioner(&qh, ());
        bad_pos.set_size(120, 120);
        bad_pos.set_anchor_rect(100, 100, 1, 1);
        let p_bad_wl = compositor.create_surface(&qh, ());
        let p_bad_xdg = wm_base.get_xdg_surface(&p_bad_wl, &qh, ());
        let p_bad = p_bad_xdg.get_popup(Some(&tl_xdg), &bad_pos, &qh, ());
        state.stages.get_mut(&stage_id).unwrap().window_mut(&w1_id).unwrap().dirty = false;
        p_bad.grab(&seat, press_serial + 999);
        p_bad_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.popup_done.contains(&p_bad.id().protocol_id()));
        assert!(state.stages[&stage_id].window(&w1_id).unwrap().dirty);
        p_bad.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 2. Nested grab reusing original grab serial after an intermediate key event,
        //    inside-popup click delivers normally, outside click dismisses with no click-through.
        let pos1 = wm_base.create_positioner(&qh, ());
        pos1.set_size(200, 200);
        pos1.set_anchor_rect(100, 100, 1, 1);
        pos1.set_anchor(c_pos::Anchor::BottomRight);
        pos1.set_gravity(c_pos::Gravity::BottomRight);
        let p1_wl = compositor.create_surface(&qh, ());
        let p1_xdg = wm_base.get_xdg_surface(&p1_wl, &qh, ());
        client.popup1_xdg_id = Some(p1_xdg.id().protocol_id());
        let p1 = p1_xdg.get_popup(Some(&tl_xdg), &pos1, &qh, ());
        client.popup1_id = Some(p1.id().protocol_id());
        p1.grab(&seat, press_serial);
        p1_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (_pool1, buf1) = create_shm_buffer_rgba(&shm, &qh, 200, 200, [0, 255, 0, 255]);
        p1_xdg.ack_configure(client.popup1_last_serial.unwrap());
        p1_xdg.set_window_geometry(0, 0, 200, 200);
        p1_wl.attach(Some(&buf1), 0, 0);
        p1_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(keyboard_id(&state), p1_wl.id().protocol_id());

        // Intermediate keyboard event updates last_press/last_release, yet nested popup reusing press_serial is accepted.
        state.send_stage_evdev_key(&stage_id, 30, true);
        state.send_stage_evdev_key(&stage_id, 30, false);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let pos2 = wm_base.create_positioner(&qh, ());
        pos2.set_size(100, 100);
        pos2.set_anchor_rect(10, 10, 1, 1);
        pos2.set_anchor(c_pos::Anchor::BottomRight);
        pos2.set_gravity(c_pos::Gravity::BottomRight);
        let p2_wl = compositor.create_surface(&qh, ());
        let p2_xdg = wm_base.get_xdg_surface(&p2_wl, &qh, ());
        client.popup2_xdg_id = Some(p2_xdg.id().protocol_id());
        let p2 = p2_xdg.get_popup(Some(&p1_xdg), &pos2, &qh, ());
        client.popup2_id = Some(p2.id().protocol_id());
        p2.grab(&seat, press_serial);
        p2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(!client.popup_done.contains(&p2.id().protocol_id()));
        let (_pool2, buf2) = create_shm_buffer_rgba(&shm, &qh, 100, 100, [0, 0, 255, 255]);
        p2_xdg.ack_configure(client.popup2_last_serial.unwrap());
        p2_xdg.set_window_geometry(0, 0, 100, 100);
        p2_wl.attach(Some(&buf2), 0, 0);
        p2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(keyboard_id(&state), p2_wl.id().protocol_id());

        let before = client.button_events;
        state.send_window_button_at(&stage_id, Some(&w1_id), 120.0, 120.0, 272, true, 3);
        state.send_window_button_at(&stage_id, Some(&w1_id), 120.0, 120.0, 272, false, 4);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.button_events, before + 2);
        assert!(!client.popup_done.contains(&p1.id().protocol_id()));
        assert!(!client.popup_done.contains(&p2.id().protocol_id()));

        let before_outside = client.button_events;
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 272, true, 5);
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 272, false, 6);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.popup_done.contains(&p2.id().protocol_id()));
        assert!(client.popup_done.contains(&p1.id().protocol_id()));
        assert_eq!(client.button_events, before_outside);
        assert_eq!(keyboard_id(&state), tl_wl.id().protocol_id());

        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 272, true, 7);
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 272, false, 8);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert_eq!(client.button_events, before_outside + 2);
        p2.destroy();
        p1.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        // 3. Destroying grabbed and ungrabbed popups repaints completely idle parent with no ghost pixels.
        client.popup1_xdg_id = None;
        client.popup2_xdg_id = None;
        let fresh_press = client.pointer_press_serial.unwrap();
        for grabbed in [true, false] {
            let pos = wm_base.create_positioner(&qh, ());
            pos.set_size(120, 120);
            pos.set_anchor_rect(100, 100, 1, 1);
            pos.set_anchor(c_pos::Anchor::BottomRight);
            pos.set_gravity(c_pos::Gravity::BottomRight);
            let p_wl = compositor.create_surface(&qh, ());
            let p_xdg = wm_base.get_xdg_surface(&p_wl, &qh, ());
            let p = p_xdg.get_popup(Some(&tl_xdg), &pos, &qh, ());
            if grabbed {
                p.grab(&seat, fresh_press);
            }
            p_wl.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut client);
            let (_pool_p, buf_p) = create_shm_buffer_rgba(&shm, &qh, 120, 120, [0, 255, 0, 255]);
            p_xdg.set_window_geometry(0, 0, 120, 120);
            p_wl.attach(Some(&buf_p), 0, 0);
            p_wl.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut client);
            assert_eq!(pixel_at(&mut state, &w1_id, 120, 120), [0, 255, 0, 255]);

            state.stages.get_mut(&stage_id).unwrap().window_mut(&w1_id).unwrap().dirty = false;
            let seq_before = state.stages[&stage_id].frame_seq;
            let settle_before = state.stages[&stage_id].settle_seq;
            p.destroy();
            pump(&mut display, &mut state, &conn, &mut queue, &mut client);
            assert!(state.stages[&stage_id].window(&w1_id).unwrap().dirty);
            assert!(state.stages[&stage_id].frame_seq > seq_before);
            assert!(state.stages[&stage_id].settle_seq > settle_before);
            assert_eq!(pixel_at(&mut state, &w1_id, 120, 120), [255, 0, 0, 255]);
        }

        // 4. Host deactivation and same-Stage window switching dismiss active popups without click-through.
        let pos4 = wm_base.create_positioner(&qh, ());
        pos4.set_size(100, 100);
        pos4.set_anchor_rect(100, 100, 1, 1);
        let p4_wl = compositor.create_surface(&qh, ());
        let p4_xdg = wm_base.get_xdg_surface(&p4_wl, &qh, ());
        let p4 = p4_xdg.get_popup(Some(&tl_xdg), &pos4, &qh, ());
        p4.grab(&seat, fresh_press);
        p4_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        state.update_window_from_sink(
            &w1_id,
            crate::sink::WindowState {
                rect: SavedRect { x: 0, y: 0, width: 800, height: 600 },
                scale: 1.0,
                monitor: None,
                pixel_size: None,
                activated: false,
                maximized: false,
                fullscreen: false,
                resizing: false,
                tiled: [false; 4],
            },
        );
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.popup_done.contains(&p4.id().protocol_id()));
        p4.destroy();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        let tl2_wl = compositor.create_surface(&qh, ());
        let tl2_xdg = wm_base.get_xdg_surface(&tl2_wl, &qh, ());
        let _tl2 = tl2_xdg.get_toplevel(&qh, ());
        tl2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let (_pool_tl2, buf_tl2) = create_shm_buffer_rgba(&shm, &qh, 400, 300, [0, 0, 255, 255]);
        tl2_wl.attach(Some(&buf_tl2), 0, 0);
        tl2_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let w2_id = state.stages[&stage_id].windows[1].id.clone();

        state.activate_sink_window(&stage_id, &w1_id);
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 273, true, 9);
        state.send_window_button_at(&stage_id, Some(&w1_id), 20.0, 20.0, 273, false, 10);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        let p5_serial = client.pointer_press_serial.unwrap();
        let pos5 = wm_base.create_positioner(&qh, ());
        pos5.set_size(100, 100);
        pos5.set_anchor_rect(100, 100, 1, 1);
        let p5_wl = compositor.create_surface(&qh, ());
        let p5_xdg = wm_base.get_xdg_surface(&p5_wl, &qh, ());
        let p5 = p5_xdg.get_popup(Some(&tl_xdg), &pos5, &qh, ());
        p5.grab(&seat, p5_serial);
        p5_wl.commit();
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);

        assert!(state.has_active_popup(&stage_id));
        let before_switch = client.button_events;
        state.send_window_button_at(&stage_id, Some(&w2_id), 20.0, 20.0, 272, true, 11);
        state.activate_sink_window(&stage_id, &w2_id);
        state.send_window_button_at(&stage_id, Some(&w2_id), 20.0, 20.0, 272, false, 12);
        pump(&mut display, &mut state, &conn, &mut queue, &mut client);
        assert!(client.popup_done.contains(&p5.id().protocol_id()));
        assert_eq!(client.button_events, before_switch);
        assert_eq!(state.stages[&stage_id].active_window().unwrap().id, w2_id);
    }

    #[test]
    fn wayland_cursor_shape_custom_surface_scale_and_serial_isolation() {
        use crate::sink::NativeCursor;
        use wayland_client::Proxy;

        let event_loop: EventLoop<'static, FocusStageState> = EventLoop::try_new().unwrap();
        let mut display: Display<FocusStageState> = Display::new().unwrap();
        let mut dh = display.handle();
        let log = Arc::new(Mutex::new(WindowLog::default()));
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-cursor-test".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 1920, height: 1080 },
            Some(Box::new(TestSink(log.clone()))),
        );

        let mut connect = || {
            let (server_sock, client_sock) = UnixStream::pair().unwrap();
            let peer = dh.insert_client(server_sock, Arc::new(ClientState::default())).unwrap();
            let conn = Connection::from_socket(client_sock).unwrap();
            let queue = conn.new_event_queue();
            conn.display().get_registry(&queue.handle(), ());
            (peer, conn, queue, TestClient::default())
        };

        let (_peer1, conn1, mut q1, mut c1) = connect();
        let (_peer2, conn2, mut q2, mut c2) = connect();
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);

        let qh1 = q1.handle();
        let qh2 = q2.handle();
        let comp1 = c1.compositor.as_ref().unwrap().clone();
        let wm1 = c1.wm_base.as_ref().unwrap().clone();
        let shm1 = c1.shm.as_ref().unwrap().clone();

        let tl1_wl = comp1.create_surface(&qh1, ());
        let tl1_xdg = wm1.get_xdg_surface(&tl1_wl, &qh1, ());
        let _tl1 = tl1_xdg.get_toplevel(&qh1, ());
        tl1_wl.commit();
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        let (_pool1, buf1) = create_shm_buffer_rgba(&shm1, &qh1, 400, 300, [255, 255, 255, 255]);
        tl1_wl.attach(Some(&buf1), 0, 0);
        tl1_wl.commit();
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);

        let sid1 = state.stages.keys().next().unwrap().clone();
        let w1_id = state.stages[&sid1].windows[0].id.clone();

        // Add subsurface to test enter serial rotation within the same window.
        let sub_wl = comp1.create_surface(&qh1, ());
        let sub = c1.subcompositor.as_ref().unwrap().get_subsurface(&sub_wl, &tl1_wl, &qh1, ());
        sub.set_position(200, 0);
        let (_pool_sub, buf_sub) = create_shm_buffer_rgba(&shm1, &qh1, 100, 100, [100, 100, 100, 255]);
        sub_wl.attach(Some(&buf_sub), 0, 0);
        sub_wl.commit();
        tl1_wl.commit();
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);

        // 1. Named cursor shape via wp_cursor_shape_v1.
        state.send_window_pointer_motion_at(&sid1, Some(&w1_id), 50.0, 50.0, 1);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        let serial1 = c1.pointer_enter_serial;
        let shape_dev1 = c1
            .cursor_shape_manager
            .as_ref()
            .unwrap()
            .get_pointer(c1.pointer.as_ref().unwrap(), &qh1, ());
        let shape_dev2 = c2
            .cursor_shape_manager
            .as_ref()
            .unwrap()
            .get_pointer(c2.pointer.as_ref().unwrap(), &qh2, ());

        log.lock().unwrap().cursors.clear();
        shape_dev1.set_shape(serial1, c_cursor_shape_device::Shape::Text);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        state.tick_cursors(10);
        state.tick_cursors(20);
        assert_eq!(log.lock().unwrap().cursors, vec![(w1_id.clone(), NativeCursor::Named("text"))]);

        // 2. Same-window subsurface transition avoids Arrow flicker, stale serial and cross-client serials are ignored.
        log.lock().unwrap().cursors.clear();
        state.send_window_pointer_motion_at(&sid1, Some(&w1_id), 220.0, 20.0, 2);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        let serial2 = c1.pointer_enter_serial;
        assert_ne!(serial2, serial1);
        assert!(log.lock().unwrap().cursors.is_empty());

        shape_dev1.set_shape(serial1, c_cursor_shape_device::Shape::Wait);
        shape_dev2.set_shape(serial2, c_cursor_shape_device::Shape::Wait);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        pump(&mut display, &mut state, &conn2, &mut q2, &mut c2);
        shape_dev1.set_shape(serial2, c_cursor_shape_device::Shape::Pointer);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        state.tick_cursors(30);
        assert_eq!(
            log.lock().unwrap().cursors,
            vec![(w1_id.clone(), NativeCursor::Named("pointer"))]
        );

        // 3. Hidden cursor.
        log.lock().unwrap().cursors.clear();
        c1.pointer.as_ref().unwrap().set_cursor(serial2, None, 0, 0);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        state.tick_cursors(40);
        assert_eq!(log.lock().unwrap().cursors, vec![(w1_id.clone(), NativeCursor::Hidden)]);

        // 4. Custom cursor surface: straight-alpha PNG, frame callbacks, offset delta, and scale updates.
        let cursor_surf = comp1.create_surface(&qh1, ());
        let _cursor_scale = c1
            .fractional_scale
            .as_ref()
            .unwrap()
            .get_fractional_scale(&cursor_surf, &qh1, cursor_surf.id().protocol_id());
        let (_pool_c1, buf_c1) = create_shm_buffer_rgba(&shm1, &qh1, 16, 16, [0, 255, 0, 255]);
        cursor_surf.attach(Some(&buf_c1), 0, 0);
        cursor_surf.frame(&qh1, ());
        cursor_surf.commit();
        c1.pointer.as_ref().unwrap().set_cursor(serial2, Some(&cursor_surf), 5, 7);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);

        let decode_png = |png: &[u8]| -> (u32, u32, [u8; 4]) {
            let decoder = png::Decoder::new(std::io::Cursor::new(png));
            let mut reader = decoder.read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut buf).unwrap();
            let cx = (info.width / 2) as usize;
            let cy = (info.height / 2) as usize;
            let idx = (cy * info.width as usize + cx) * 4;
            (info.width, info.height, buf[idx..idx + 4].try_into().unwrap())
        };

        log.lock().unwrap().cursors.clear();
        let cb_before = c1.frame_callbacks;
        state.tick_cursors(50);
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        assert!(c1.frame_callbacks > cb_before);
        assert_eq!(c1.preferred_scales[&cursor_surf.id().protocol_id()], 120);
        let cursors = std::mem::take(&mut log.lock().unwrap().cursors);
        assert_eq!(cursors.len(), 1);
        match &cursors[0] {
            (wid, NativeCursor::Bitmap { png, hotspot_x, hotspot_y }) => {
                assert_eq!(wid, &w1_id);
                assert_eq!((*hotspot_x, *hotspot_y), (5, 7));
                assert_eq!(decode_png(png), (16, 16, [0, 255, 0, 255]));
            }
            other => panic!("expected bitmap cursor, got {other:?}"),
        }

        // Animated commit with wl_surface.offset adjusts hotspot and re-emits bitmap.
        let (_pool_c2, buf_c2) = create_shm_buffer_rgba(&shm1, &qh1, 16, 16, [255, 0, 0, 255]);
        cursor_surf.attach(Some(&buf_c2), 0, 0);
        cursor_surf.offset(2, 1);
        cursor_surf.commit();
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        log.lock().unwrap().cursors.clear();
        state.tick_cursors(60);
        let cursors = std::mem::take(&mut log.lock().unwrap().cursors);
        assert_eq!(cursors.len(), 1);
        match &cursors[0] {
            (_, NativeCursor::Bitmap { png, hotspot_x, hotspot_y }) => {
                assert_eq!((*hotspot_x, *hotspot_y), (3, 6));
                assert_eq!(decode_png(png), (16, 16, [0, 0, 255, 255]));
            }
            other => panic!("expected updated bitmap cursor, got {other:?}"),
        }

        // Scale update re-renders cursor bitmap and syncs preferred fractional scale on cursor surface.
        state.update_window_from_sink(
            &w1_id,
            crate::sink::WindowState {
                rect: SavedRect { x: 0, y: 0, width: 400, height: 300 },
                scale: 2.0,
                monitor: None,
                pixel_size: Some([800, 600]),
                activated: true,
                maximized: false,
                fullscreen: false,
                resizing: false,
                tiled: [false; 4],
            },
        );
        pump(&mut display, &mut state, &conn1, &mut q1, &mut c1);
        assert_eq!(c1.preferred_scales[&cursor_surf.id().protocol_id()], 240);
        log.lock().unwrap().cursors.clear();
        state.tick_cursors(70);
        let cursors = std::mem::take(&mut log.lock().unwrap().cursors);
        assert_eq!(cursors.len(), 1);
        match &cursors[0] {
            (_, NativeCursor::Bitmap { png, hotspot_x, hotspot_y }) => {
                assert_eq!((*hotspot_x, *hotspot_y), (6, 12));
                assert_eq!(decode_png(png), (32, 32, [0, 0, 255, 255]));
            }
            other => panic!("expected 2x bitmap cursor, got {other:?}"),
        }

        // 5. Agent takeover resets cursor to default; returning to human restores cursor; pointer leave resets to default.
        log.lock().unwrap().cursors.clear();
        state.stages.get_mut(&sid1).unwrap().input_owner = crate::protocol::InputOwner::Agent {
            lease_id: "lease-cursor".into(),
            agent_name: None,
        };
        state.tick_cursors(80);
        assert_eq!(
            log.lock().unwrap().cursors,
            vec![(w1_id.clone(), NativeCursor::Named("default"))]
        );

        log.lock().unwrap().cursors.clear();
        state.stages.get_mut(&sid1).unwrap().input_owner = crate::protocol::InputOwner::Human;
        state.tick_cursors(90);
        assert!(matches!(
            log.lock().unwrap().cursors.as_slice(),
            [(wid, NativeCursor::Bitmap { .. })] if wid == &w1_id
        ));

        log.lock().unwrap().cursors.clear();
        state.leave_window_pointer(&sid1, &w1_id);
        assert_eq!(
            log.lock().unwrap().cursors,
            vec![(w1_id.clone(), NativeCursor::Named("default"))]
        );
    }

    #[derive(Default)]
    struct MiniXwaylandClient {
        compositor: Option<wl_compositor::WlCompositor>,
        xwayland_shell: Option<xwayland_shell_v1::XwaylandShellV1>,
    }

    impl wayland_client::Dispatch<wl_registry::WlRegistry, ()> for MiniXwaylandClient {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &wayland_client::Connection,
            qh: &wayland_client::QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor = Some(
                            registry.bind::<wl_compositor::WlCompositor, _, _>(
                                name,
                                version.min(6),
                                qh,
                                (),
                            ),
                        );
                    }
                    "xwayland_shell_v1" => {
                        state.xwayland_shell = Some(
                            registry.bind::<xwayland_shell_v1::XwaylandShellV1, _, _>(
                                name,
                                version.min(1),
                                qh,
                                (),
                            ),
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    wayland_client::delegate_noop!(MiniXwaylandClient: ignore wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(MiniXwaylandClient: ignore wl_surface::WlSurface);
    wayland_client::delegate_noop!(MiniXwaylandClient: ignore xwayland_shell_v1::XwaylandShellV1);
    wayland_client::delegate_noop!(MiniXwaylandClient: ignore xwayland_surface_v1::XwaylandSurfaceV1);

    fn pump_mini(
        display: &mut Display<FocusStageState>,
        state: &mut FocusStageState,
        conn: &wayland_client::Connection,
        queue: &mut wayland_client::EventQueue<MiniXwaylandClient>,
        client: &mut MiniXwaylandClient,
    ) {
        for _ in 0..4 {
            let _ = conn.flush();
            display.dispatch_clients(state).unwrap();
            display.flush_clients().unwrap();
            if let Some(guard) = conn.prepare_read() {
                let _ = guard.read();
            }
            queue.dispatch_pending(client).unwrap();
        }
    }

    #[test]
    fn xwayland_shell_serials_are_isolated_per_client_and_cleaned_up() {
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;
        use smithay::wayland::compositor::with_states;
        use smithay::wayland::xwayland_shell::XWaylandShellCachedState;
        use smithay::xwayland::XWaylandClientData;

        let event_loop = smithay::reexports::calloop::EventLoop::<FocusStageState>::try_new().unwrap();
        let mut display = Display::<FocusStageState>::new().unwrap();
        let mut dh = display.handle();
        let mut state = FocusStageState::new(
            &dh,
            event_loop.handle(),
            "focusstage-test-xwayland-shell".to_string(),
            1.0,
            60,
            SavedRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            None,
        );

        let (server_a, client_a_sock) = UnixStream::pair().unwrap();
        let (server_b, client_b_sock) = UnixStream::pair().unwrap();
        client_a_sock.set_nonblocking(true).unwrap();
        client_b_sock.set_nonblocking(true).unwrap();

        let client_a = dh
            .insert_client(server_a, Arc::new(XWaylandClientData::new_detached()))
            .unwrap();
        let client_b = dh
            .insert_client(server_b, Arc::new(XWaylandClientData::new_detached()))
            .unwrap();

        let conn_a = wayland_client::Connection::from_socket(client_a_sock).unwrap();
        let mut queue_a = conn_a.new_event_queue::<MiniXwaylandClient>();
        let qh_a = queue_a.handle();
        let mut mini_a = MiniXwaylandClient::default();
        let _reg_a = conn_a.display().get_registry(&qh_a, ());

        let conn_b = wayland_client::Connection::from_socket(client_b_sock).unwrap();
        let mut queue_b = conn_b.new_event_queue::<MiniXwaylandClient>();
        let qh_b = queue_b.handle();
        let mut mini_b = MiniXwaylandClient::default();
        let _reg_b = conn_b.display().get_registry(&qh_b, ());

        pump_mini(&mut display, &mut state, &conn_a, &mut queue_a, &mut mini_a);
        pump_mini(&mut display, &mut state, &conn_b, &mut queue_b, &mut mini_b);

        let comp_a = mini_a.compositor.as_ref().expect("client A wl_compositor");
        let shell_a = mini_a.xwayland_shell.as_ref().expect("client A xwayland_shell_v1");
        let comp_b = mini_b.compositor.as_ref().expect("client B wl_compositor");
        let shell_b = mini_b.xwayland_shell.as_ref().expect("client B xwayland_shell_v1");

        // Client A creates surf_a1 (serial 1) and surf_a2 (serial 2) and commits both.
        let surf_a1 = comp_a.create_surface(&qh_a, ());
        let xsurf_a1 = shell_a.get_xwayland_surface(&surf_a1, &qh_a, ());
        xsurf_a1.set_serial(1, 0);
        surf_a1.commit();

        let surf_a2 = comp_a.create_surface(&qh_a, ());
        let xsurf_a2 = shell_a.get_xwayland_surface(&surf_a2, &qh_a, ());
        xsurf_a2.set_serial(2, 0);
        surf_a2.commit();

        pump_mini(&mut display, &mut state, &conn_a, &mut queue_a, &mut mini_a);

        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_a.id(), 1)
            .is_some());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_a.id(), 2)
            .is_some());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_b.id(), 1)
            .is_none());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_b.id(), 2)
            .is_none());

        // Taking serial 1 for client B must fail without evicting client A's surface.
        assert!(state
            .xwayland_shell_state
            .take_surface_for_serial(&client_b.id(), 1)
            .is_none());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_a.id(), 1)
            .is_some());

        // Client B creates surf_b1 (serial 1) and surf_b2 (serial 2) and commits both.
        let surf_b1 = comp_b.create_surface(&qh_b, ());
        let xsurf_b1 = shell_b.get_xwayland_surface(&surf_b1, &qh_b, ());
        xsurf_b1.set_serial(1, 0);
        surf_b1.commit();

        let surf_b2 = comp_b.create_surface(&qh_b, ());
        let xsurf_b2 = shell_b.get_xwayland_surface(&surf_b2, &qh_b, ());
        xsurf_b2.set_serial(2, 0);
        surf_b2.commit();

        pump_mini(&mut display, &mut state, &conn_b, &mut queue_b, &mut mini_b);

        // Consuming serial 1 on client B returns client B's surface once and consumes pending serial.
        let taken_b1 = state
            .xwayland_shell_state
            .take_surface_for_serial(&client_b.id(), 1)
            .expect("client B serial 1 should resolve");
        assert_eq!(
            taken_b1.client().map(|c| c.id()),
            Some(client_b.id()),
            "serial 1 for client B must belong to client B"
        );
        assert!(state
            .xwayland_shell_state
            .take_surface_for_serial(&client_b.id(), 1)
            .is_none());

        // Subsequent commit on surf_b1 must not re-insert serial 1 while current().serial remains Some(1).
        surf_b1.commit();
        pump_mini(&mut display, &mut state, &conn_b, &mut queue_b, &mut mini_b);
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_b.id(), 1)
            .is_none());
        let committed_serial = with_states(&taken_b1, |states| {
            states
                .cached_state
                .get::<XWaylandShellCachedState>()
                .current()
                .serial
        });
        assert_eq!(committed_serial, Some(1));

        // Destroying surf_a2 cleans up client A's serial 2 entry.
        xsurf_a2.destroy();
        surf_a2.destroy();
        pump_mini(&mut display, &mut state, &conn_a, &mut queue_a, &mut mini_a);
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_a.id(), 2)
            .is_none());

        // Removing client A clears its remaining serials without touching client B's serial 2.
        state.xwayland_shell_state.remove_client(&client_a.id());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_a.id(), 1)
            .is_none());
        assert!(state
            .xwayland_shell_state
            .surface_for_client_serial(&client_b.id(), 2)
            .is_some());
    }
}
