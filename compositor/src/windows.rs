use smithay::desktop::{find_popup_root_surface, PopupManager};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::get_parent;

use crate::protocol::{DisplayState, SavedRect};
use crate::sink::{DisplaySink, WindowInfo, WindowState};
use crate::stage::{Stage, StageToplevel, StageWindow};
use crate::state::FocusStageState;
use crate::xwayland::hosted_x11_geometry;

fn show(sink: &mut dyn DisplaySink, stage: &Stage, window: &StageWindow) {
    sink.show(WindowInfo {
        window_id: &window.id,
        stage_id: &stage.id,
        title: &window.title,
        wm_class: &stage.wm_class,
        rect: window.saved_rect,
        scale: window.scale,
        parent_window_id: window.parent_window_id.as_deref(),
        maximized: window.maximized,
    });
}

impl FocusStageState {
    pub fn sync_surface_output(&self, surface: &WlSurface) {
        let (monitor, scale) = self
            .window_for_surface(surface)
            .map_or((None, self.default_scale), |w| (w.monitor, w.scale));
        self.virtual_outputs.enter_tree(surface, monitor, scale);
    }

    pub fn window_id_for_toplevel(&self, toplevel: &StageToplevel) -> Option<String> {
        self.stages
            .values()
            .flat_map(|s| &s.windows)
            .find(|w| &w.toplevel == toplevel)
            .map(|w| w.id.clone())
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<&StageWindow> {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(popup) = self.popups.find_popup(&root) {
            root = find_popup_root_surface(&popup).ok()?;
        }
        self.stages
            .values()
            .flat_map(|s| &s.windows)
            .find(|w| w.toplevel.wl_surface().as_ref() == Some(&root))
    }

    pub fn stage_for_window(&self, window_id: &str) -> Option<&Stage> {
        let (stage_id, _) = window_id.rsplit_once('/')?;
        self.stages
            .get(stage_id)
            .filter(|stage| stage.window(window_id).is_some())
    }

    pub fn desktop_stage_for_window(&self, window_id: &str) -> Option<String> {
        let stage = self.stage_for_window(window_id)?;
        (stage.display == DisplayState::Desktop && stage.window(window_id)?.mapped)
            .then(|| stage.id.clone())
    }

    pub fn set_window_mapped(&mut self, window_id: &str, mapped: bool) {
        let Some(stage_id) = self.stage_for_window(window_id).map(|s| s.id.clone()) else {
            return;
        };
        if self.stages[&stage_id].window(window_id).unwrap().mapped == mapped {
            return;
        }
        if !mapped {
            self.clear_window_inputs(&stage_id, window_id);
        }
        let stage = self.stages.get_mut(&stage_id).unwrap();
        let window = stage.window_mut(window_id).unwrap();
        window.mapped = mapped;
        if mapped {
            window.dirty = true;
            if stage.display == DisplayState::Desktop {
                if let Some(sink) = self.display_sink.as_mut() {
                    show(sink.as_mut(), stage, stage.window(window_id).unwrap());
                }
            }
        } else if let Some(sink) = self.display_sink.as_mut() {
            sink.hide(window_id);
        }
        self.ensure_virtual_focus(&stage_id);
    }

    pub fn remove_window(&mut self, window_id: &str) {
        let Some(stage_id) = self.stage_for_window(window_id).map(|s| s.id.clone()) else {
            return;
        };
        self.clear_window_inputs(&stage_id, window_id);
        let stage = self.stages.get_mut(&stage_id).unwrap();
        let host_close_requested = stage
            .window(window_id)
            .is_some_and(|w| w.host_close_requested);
        stage.windows.retain(|w| w.id != window_id);
        if host_close_requested && stage.windows.is_empty() {
            stage.retire_when_empty = true;
        }
        if stage.active_window_id.as_deref() == Some(window_id) {
            stage.active_window_id = None;
        }
        for window in &mut stage.windows {
            if window.parent_window_id.as_deref() == Some(window_id) {
                window.parent_window_id = None;
                if let Some(sink) = self.display_sink.as_mut() {
                    sink.set_parent(&window.id, None);
                }
            }
        }
        if let Some(sink) = self.display_sink.as_mut() {
            sink.destroy(window_id);
        }
        stage.bump_frame();
        self.ensure_virtual_focus(&stage_id);
    }

    pub fn request_window_maximized(&mut self, window_id: &str, maximized: bool) {
        let Some((stage_id, _)) = window_id.rsplit_once('/') else {
            return;
        };
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        let desktop = stage.display == DisplayState::Desktop;
        let Some(window) = stage.window_mut(window_id) else {
            return;
        };
        window.maximized = maximized;
        if desktop && window.mapped {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.set_maximized(window_id, maximized);
            }
        }
    }

    pub fn update_window_title(&mut self, window_id: &str, title: String) {
        let Some(stage_id) = self.stage_for_window(window_id).map(|s| s.id.clone()) else {
            return;
        };
        if let Some(sink) = self.display_sink.as_mut() {
            sink.set_title(window_id, &title);
        }
        self.stages
            .get_mut(&stage_id)
            .unwrap()
            .window_mut(window_id)
            .unwrap()
            .title = title;
    }

    pub fn update_window_parent(&mut self, window_id: &str, parent_window_id: Option<String>) {
        let Some(stage_id) = self.stage_for_window(window_id).map(|s| s.id.clone()) else {
            return;
        };
        if let Some(sink) = self.display_sink.as_mut() {
            sink.set_parent(window_id, parent_window_id.as_deref());
        }
        self.stages
            .get_mut(&stage_id)
            .unwrap()
            .window_mut(window_id)
            .unwrap()
            .parent_window_id = parent_window_id;
    }

    pub fn update_stage_window_visibility(&mut self, stage_id: &str) {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        let Some(sink) = self.display_sink.as_mut() else {
            return;
        };
        match stage.display {
            DisplayState::Hidden => {
                for window in &stage.windows {
                    sink.hide(&window.id);
                }
            }
            DisplayState::Desktop => {
                for window in stage.windows.iter().filter(|w| w.mapped) {
                    show(sink.as_mut(), stage, window);
                }
                for window in &mut stage.windows {
                    window.dirty = true;
                }
            }
        }
    }

    pub fn activate_sink_window(&mut self, stage_id: &str, window_id: &str) {
        if self.stages[stage_id]
            .active_window()
            .is_none_or(|w| w.id != window_id)
        {
            self.release_all_stage_inputs(stage_id);
        }
        self.stages.get_mut(stage_id).unwrap().active_window_id = Some(window_id.to_string());
        self.ensure_virtual_focus(stage_id);
    }

    pub fn send_sink_key(&mut self, stage_id: &str, window_id: &str, key: u32, pressed: bool) {
        if pressed {
            self.activate_sink_window(stage_id, window_id);
        } else {
            let stage = &self.stages[stage_id];
            // A focus switch already releases the old window's keys. Ignore its late releases.
            if stage.active_window().is_none_or(|w| w.id != window_id) || !stage.input.pressed_keys.contains(&key) {
                return;
            }
        }
        self.send_stage_evdev_key(stage_id, key, pressed);
    }

    pub fn close_window_from_sink(&mut self, window_id: &str) {
        let Some(window) = window_id
            .rsplit_once('/')
            .and_then(|(stage_id, _)| self.stages.get_mut(stage_id)?.window_mut(window_id))
        else {
            return;
        };
        window.host_close_requested = true;
        match &window.toplevel {
            StageToplevel::Wayland(w) => w.send_close(),
            StageToplevel::X11(w) => {
                let _ = w.close();
            }
        }
    }

    pub fn update_window_from_sink(&mut self, window_id: &str, mut state: WindowState) {
        let Some(stage) = self.stage_for_window(window_id) else {
            return;
        };
        if !stage.window(window_id).unwrap().mapped {
            return;
        }
        let stage_id = stage.id.clone();
        if stage.uses_virtual_focus() {
            state.activated = stage.active_window().is_some_and(|w| w.id == window_id);
        } else if state.activated {
            self.activate_sink_window(&stage_id, window_id);
        } else if stage.active_window_id.as_deref() == Some(window_id) {
            self.release_all_stage_inputs(&stage_id);
        }
        self.configure_sink_window(window_id, state.rect, Some(state));
    }

    fn configure_sink_window(
        &mut self,
        window_id: &str,
        rect: SavedRect,
        status: Option<WindowState>,
    ) {
        let Some((stage_id, _)) = window_id.rsplit_once('/') else {
            return;
        };
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        let Some(window) = stage.window_mut(window_id) else {
            return;
        };
        let size = (rect.width.max(1) as i32, rect.height.max(1) as i32).into();
        let size_changed = window.logical_size != size;
        let output_changed = status
            .as_ref()
            .is_some_and(|s| s.scale != window.scale || s.monitor != window.monitor);
        let pixels_changed = status
            .as_ref()
            .is_some_and(|s| s.pixel_size != window.pixel_size);
        let surface = window.toplevel.wl_surface();
        window.saved_rect = rect;
        window.logical_size = size;
        if let Some(status) = &status {
            window.scale = status.scale;
            window.monitor = status.monitor;
            window.pixel_size = status.pixel_size;
            window.maximized = status.maximized;
            if let Some(monitor) = status.monitor {
                self.virtual_outputs
                    .update(&self.display_handle, monitor, status.scale);
            }
        }
        match &window.toplevel {
            StageToplevel::Wayland(w) => {
                w.with_pending_state(|pending| {
                    pending.size = Some(size);
                    if let Some(status) = &status {
                        for (state, enabled) in [
                            (State::Activated, status.activated),
                            (State::Maximized, status.maximized),
                            (State::Resizing, status.resizing),
                            (State::TiledLeft, status.tiled[0]),
                            (State::TiledRight, status.tiled[1]),
                            (State::TiledTop, status.tiled[2]),
                            (State::TiledBottom, status.tiled[3]),
                        ] {
                            if enabled {
                                pending.states.set(state);
                            } else {
                                pending.states.unset(state);
                            }
                        }
                    }
                });
                w.send_pending_configure();
            }
            StageToplevel::X11(w) => {
                let geometry = hosted_x11_geometry(size);
                if w.geometry() != geometry {
                    let _ = w.configure(geometry);
                }
                if let Some(status) = &status {
                    let _ = w.set_activated(status.activated);
                    let _ = w.set_maximized(status.maximized);
                }
            }
        }
        if size_changed || output_changed || pixels_changed {
            stage.bump_window_frame(window_id);
        }
        if output_changed {
            if let Some(surface) = surface {
                self.sync_surface_output(&surface);
                for (popup, _) in PopupManager::popups_for_surface(&surface) {
                    self.sync_surface_output(popup.wl_surface());
                }
            }
        }
    }
}
