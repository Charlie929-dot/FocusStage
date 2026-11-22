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
use crate::protocol::DisplayState;
use crate::sink::DisplaySink;
use crate::stage::StageToplevel;
use crate::state::FocusStageState;

impl XWaylandShellHandler for FocusStageState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, xwm_id: XwmId, surface: WlSurface, window: X11Surface) {
        self.offscreen.ensure_surface_entered(&surface);
        if let Some(stage_id) = self.xwm_to_stage.get(&xwm_id).cloned() {
            if !window.is_override_redirect() {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    let already = stage.toplevels.iter().any(|t| match t {
                        StageToplevel::X11(x) => x == &window,
                        _ => false,
                    });
                    if !already {
                        stage.toplevels.push(StageToplevel::X11(window));
                    }
                    stage.bump_frame();
                }
                self.ensure_virtual_focus(&stage_id);
            }
        }
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
        self.x11_override_windows
            .entry(xwm)
            .or_default()
            .push(window);
    }

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() else {
            let _ = window.set_mapped(true);
            return;
        };

        let size = self
            .stages
            .get(&stage_id)
            .map(|s| s.logical_size)
            .unwrap_or_else(|| (1024, 768).into());

        let _ = window.configure(Rectangle::new((0, 0).into(), size));
        let _ = window.set_mapped(true);
        let _ = window.set_activated(true);

        let title = window.title();
        let mut need_show = false;
        let mut rect = None;
        if let Some(stage) = self.stages.get_mut(&stage_id) {
            if !title.is_empty() {
                stage.title = title.clone();
            }
            if !window.is_override_redirect() {
                let already = stage.toplevels.iter().any(|t| match t {
                    StageToplevel::X11(x) => x == &window,
                    _ => false,
                });
                if !already {
                    stage.toplevels.push(StageToplevel::X11(window));
                }
            }
            stage.bump_frame();
            if stage.display == DisplayState::Desktop {
                need_show = true;
                rect = stage.saved_rect;
            }
        }
        if need_show {
            if let (Some(ref mut sink), Some(r)) = (&mut self.wslg_sink, rect) {
                let t = self
                    .stages
                    .get(&stage_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                sink.show(&stage_id, &t, r);
            }
        }
        self.ensure_virtual_focus(&stage_id);
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        let list = self.x11_override_windows.entry(xwm).or_default();
        if !list.contains(&window) {
            list.push(window);
        }
        if let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() {
            if let Some(stage) = self.stages.get_mut(&stage_id) {
                stage.bump_frame();
            }
        }
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        if let Some(list) = self.x11_override_windows.get_mut(&xwm) {
            list.retain(|w| w != &window);
        }
        if let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() {
            if let Some(stage) = self.stages.get_mut(&stage_id) {
                stage.toplevels.retain(|t| match t {
                    StageToplevel::X11(x) => x != &window,
                    _ => true,
                });
                stage.bump_frame();
            }
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.unmapped_window(xwm, window);
    }

    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let stage_size = self
            .xwm_to_stage
            .get(&xwm)
            .and_then(|sid| self.stages.get(sid))
            .map(|s| s.logical_size);
        let cur = window.geometry();
        let target_w = w.map(|v| v as i32).unwrap_or_else(|| {
            stage_size.map(|s| s.w).unwrap_or(cur.size.w.max(1))
        });
        let target_h = h.map(|v| v as i32).unwrap_or_else(|| {
            stage_size.map(|s| s.h).unwrap_or(cur.size.h.max(1))
        });
        let _ = window.configure(Rectangle::new((0, 0).into(), (target_w, target_h).into()));
    }

    fn configure_notify(
        &mut self,
        xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() {
            if let Some(stage) = self.stages.get_mut(&stage_id) {
                stage.bump_frame();
            }
        }
    }

    fn property_notify(&mut self, xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if property == WmWindowProperty::Title {
            if let Some(stage_id) = self.xwm_to_stage.get(&xwm).cloned() {
                let title = window.title();
                if !title.is_empty() {
                    if let Some(stage) = self.stages.get_mut(&stage_id) {
                        stage.title = title.clone();
                    }
                    if let Some(ref mut sink) = self.wslg_sink {
                        sink.set_title(&stage_id, &title);
                    }
                }
            }
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

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

    pub fn x11_override_windows_for(&self, xwm_id: XwmId) -> Vec<X11Surface> {
        self.x11_override_windows
            .get(&xwm_id)
            .map(|v| v.iter().filter(|w| w.alive()).cloned().collect())
            .unwrap_or_default()
    }
}
