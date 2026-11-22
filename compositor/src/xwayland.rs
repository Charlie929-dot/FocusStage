use std::io::Write;
use std::os::fd::OwnedFd;
use std::process::Stdio;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowProperty, X11Surface, X11Wm, XwmId, XwmHandler};
use smithay::xwayland::{XWayland, XWaylandEvent};
use tracing::{debug, info, warn};

use crate::clipboard::pick_text_mime;
use crate::stage::StageToplevel;
use crate::state::FocusStageState;

impl XWaylandShellHandler for FocusStageState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, xwm_id: XwmId, surface: WlSurface, window: X11Surface) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            self.set_window_mapped(&id, true);
        } else if let Some(stage_id) = self.xwm_to_stage.get(&xwm_id) {
            if let Some(stage) = self.stages.get_mut(stage_id) {
                stage.bump_frame();
            }
        }
        self.sync_surface_output(&surface);
    }
}

impl XwmHandler for FocusStageState {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.xwms.get_mut(&xwm).expect("X11Wm must exist for XwmId")
    }

    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        debug!("X11 new_window {:?} on {:?}", window.window_id(), xwm);
    }

    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        debug!("X11 override_redirect window {:?} on {:?}", window.window_id(), xwm);
    }

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() else {
            let _ = window.set_mapped(true);
            return;
        };
        let toplevel = StageToplevel::X11(window.clone());
        let stage = self.stages.get_mut(&stage_id).unwrap();
        let id = if let Some(known) = stage.windows.iter_mut().find(|w| w.toplevel == toplevel) {
            // Configure requests while unmapped update the X11 surface's geometry.
            known.set_mapped_size(window.geometry().size);
            known.id.clone()
        } else {
            let size = if stage.windows.is_empty() {
                stage.logical_size()
            } else {
                let geo = window.geometry();
                (geo.size.w.max(1), geo.size.h.max(1)).into()
            };
            stage.add_window(toplevel, size)
        };
        let size = stage.window(&id).unwrap().logical_size;
        let _ = window.configure(Rectangle::new(window.geometry().loc, size));
        let _ = window.set_mapped(true);
        let _ = window.set_activated(true);
        self.update_x11_parent(&id, &window);
        let title = window.title();
        if !title.is_empty() {
            self.update_window_title(&id, title);
        }
        if window.wl_surface().is_some() {
            self.set_window_mapped(&id, true);
        }
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        let Some(stage) = self.xwm_to_stage.get(&xwm).and_then(|id| self.stages.get_mut(id)) else {
            return;
        };
        let owner = window.is_transient_for().or_else(|| {
            match stage.active_toplevel()? {
                StageToplevel::X11(x) => Some(x.window_id()),
                _ => None,
            }
        });
        let list = self.x11_override_windows.entry(xwm).or_default();
        list.retain(|(w, _)| w != &window);
        list.push((window, owner));
        stage.bump_frame();
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        if let Some(list) = self.x11_override_windows.get_mut(&xwm) {
            list.retain(|(w, _)| w != &window);
        }
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            self.set_window_mapped(&id, false);
        }
        if let Some(stage) = self.xwm_to_stage.get(&xwm).and_then(|id| self.stages.get_mut(id)) {
            stage.bump_frame();
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.unmapped_window(xwm, window.clone());
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            self.remove_window(&id);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let current = window.geometry();
        let id = self.window_id_for_toplevel(&StageToplevel::X11(window.clone()));
        // Once mapped, the host owns the size. Before mapping, honor the application's hints.
        let mapped = id.as_deref().and_then(|id| self.stage_for_window(id)?.window(id))
            .is_some_and(|w| w.mapped);
        let size = if mapped {
            current.size
        } else {
            (width.map_or(current.size.w, |w| w as i32).max(1),
             height.map_or(current.size.h, |h| h as i32).max(1)).into()
        };
        let loc = (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into();
        let _ = window.configure(Rectangle::new(loc, size));
    }

    fn configure_notify(
        &mut self,
        xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if let Some(stage) = self.xwm_to_stage.get(&xwm).and_then(|id| self.stages.get_mut(id)) {
            stage.bump_frame();
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window.clone())) {
            match property {
                WmWindowProperty::Title => self.update_window_title(&id, window.title()),
                WmWindowProperty::TransientFor => self.update_x11_parent(&id, &window),
                _ => {}
            }
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edge: ResizeEdge) {
        let direction = match edge {
            ResizeEdge::TopLeft => 0,
            ResizeEdge::Top => 1,
            ResizeEdge::TopRight => 2,
            ResizeEdge::Right => 3,
            ResizeEdge::BottomRight => 4,
            ResizeEdge::Bottom => 5,
            ResizeEdge::BottomLeft => 6,
            ResizeEdge::Left => 7,
        };
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.begin_resize(&id, direction);
            }
        }
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.begin_move(&id);
            }
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.maximize_x11(window, true);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.maximize_x11(window, false);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.minimize(&id);
            }
        }
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    fn send_selection(&mut self, xwm: XwmId, _selection: SelectionTarget, _mime_type: String, fd: OwnedFd) {
        let text = self
            .xwm_to_stage
            .get(&xwm)
            .and_then(|sid| self.stages.get(sid))
            .and_then(|s| s.clipboard.text.clone())
            .unwrap_or_default();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(fd);
            let _ = file.write_all(text.as_bytes());
        });
    }

    fn new_selection(&mut self, xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        if selection != SelectionTarget::Clipboard {
            return;
        }
        let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() else {
            return;
        };
        let Some(mime) = pick_text_mime(&mime_types) else {
            return;
        };
        if let Some(stage) = self.stages.get_mut(&stage_id) {
            stage.clipboard.x11_has_selection = true;
            stage.clipboard.mime_types = mime_types;
        }
        // Read selection content into StageClipboard asynchronously via pipe
        let (read_fd, write_fd) = match create_x11_pipe() {
            Ok(p) => p,
            Err(_) => return,
        };
        let handle = self.loop_handle.clone();
        if let Some(wm) = self.xwms.get_mut(&xwm) {
            if wm.send_selection(selection, mime, write_fd, handle).is_ok() {
                let tx = self.clipboard_update_tx.clone();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut file = std::fs::File::from(read_fd);
                    let mut buf = Vec::new();
                    let _ = file.read_to_end(&mut buf);
                    if let Ok(text) = String::from_utf8(buf) {
                        let _ = tx.send((stage_id, text));
                    }
                });
            }
        }
    }

    fn cleared_selection(&mut self, xwm: XwmId, selection: SelectionTarget) {
        if selection == SelectionTarget::Clipboard {
            if let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.clipboard.x11_has_selection = false;
                }
            }
        }
    }

    fn disconnected(&mut self, xwm: XwmId) {
        self.xwms.remove(&xwm);
        self.xwm_to_stage.remove(&xwm);
        self.x11_override_windows.remove(&xwm);
    }
}

fn create_x11_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl FocusStageState {
    pub fn spawn_xwayland_for_stage(&mut self, stage_id: String) -> Result<(), String> {
        let dh = self.display_handle.clone();
        let (xwayland, client) = XWayland::spawn(
            &dh,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        )
        .map_err(|e| format!("spawn XWayland: {e}"))?;

        if let Some(stage) = self.stages.get_mut(&stage_id) {
            stage.xwayland_client = Some(client.clone());
        }

        let handle = self.loop_handle.clone();
        let sid_for_cb = stage_id.clone();
        let token = self
            .loop_handle
            .insert_source(xwayland, move |event, _, state| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    info!("XWayland ready for {} on DISPLAY=:{}", sid_for_cb, display_number);
                    match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
                        Ok(wm) => {
                            let xwm_id = wm.id();
                            state.xwms.insert(xwm_id, wm);
                            state.xwm_to_stage.insert(xwm_id, sid_for_cb.clone());
                            if let Some(stage) = state.stages.get_mut(&sid_for_cb) {
                                stage.x11_display_number = Some(display_number);
                                stage.xwm_id = Some(xwm_id);
                            }
                            state.launch_pending_x11_client(&sid_for_cb, display_number);
                        }
                        Err(err) => {
                            warn!("Failed to start X11Wm for {}: {}", sid_for_cb, err);
                        }
                    }
                }
                XWaylandEvent::Error => {
                    warn!("XWayland failed for Stage {}", sid_for_cb);
                }
            })
            .map_err(|e| format!("insert XWayland source: {e}"))?;

        if let Some(stage) = self.stages.get_mut(&stage_id) {
            stage.xwayland_token = Some(token);
        }
        Ok(())
    }

    fn update_x11_parent(&mut self, id: &str, window: &X11Surface) {
        let parent = window.is_transient_for().and_then(|parent| {
            self.stage_for_window(id)?.windows.iter().find(|w| {
                matches!(&w.toplevel, StageToplevel::X11(x) if x.window_id() == parent)
            }).map(|w| w.id.clone())
        });
        self.update_window_parent(id, parent);
    }

    fn maximize_x11(&mut self, window: X11Surface, maximized: bool) {
        let _ = window.set_maximized(maximized);
        if let Some(id) = self.window_id_for_toplevel(&StageToplevel::X11(window)) {
            self.request_window_maximized(&id, maximized);
        }
    }

    pub fn x11_override_windows_for(&self, toplevel: &StageToplevel) -> Vec<X11Surface> {
        let StageToplevel::X11(toplevel) = toplevel else {
            return Vec::new();
        };
        toplevel.xwm_id().and_then(|id| self.x11_override_windows.get(&id))
            .map(|list| list.iter().filter(|(w, owner)| {
                let mut owner = w.is_transient_for().or(*owner);
                // Nested X11 menus can be transient for another override-redirect window.
                for _ in 0..list.len() {
                    let Some((_, next)) = list.iter().find(|(parent, _)| Some(parent.window_id()) == owner) else {
                        break;
                    };
                    owner = *next;
                }
                w.alive() && owner == Some(toplevel.window_id())
            }).map(|(w, _)| w.clone()).collect())
            .unwrap_or_default()
    }
}
