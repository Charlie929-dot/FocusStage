use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface, Resource};
use smithay::utils::Serial;
use smithay::wayland::compositor::{with_states, SurfaceAttributes};

use crate::offscreen::send_frames_surface_tree;
use crate::popup::surface_root;
use crate::protocol::{DisplayState, InputOwner, SavedRect};
use crate::sink::NativeCursor;
use crate::stage::{StageInputTarget, StageWindow};
use crate::state::FocusStageState;

pub(crate) struct CursorFocus {
    serial: Serial,
    pub(crate) window: String,
    pub(crate) image: CursorImageStatus,
    presented_scale: Option<f64>,
}

impl FocusStageState {
    fn reset_window_cursor(&mut self, stage_id: &str, window_id: &str) {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        let desktop = stage.display == DisplayState::Desktop;
        let Some(window) = stage.window_mut(window_id) else {
            return;
        };
        if window.cursor_default {
            return;
        }
        window.cursor_default = true;
        if desktop && window.mapped {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.set_cursor(window_id, NativeCursor::Named("default"));
            }
        }
    }

    pub(crate) fn enter_cursor_focus(&mut self, stage_id: &str, serial: Serial, target: &StageInputTarget) {
        let new_window_id = {
            let Some(stage) = self.stages.get(stage_id) else {
                return;
            };
            target
                .wl_surface()
                .and_then(|surface| self.window_for_surface(&surface))
                .or_else(|| stage.input.pointer_window_id.as_deref().and_then(|id| stage.window(id)))
                .or_else(|| stage.active_window())
                .map(|window| window.id.clone())
        };
        let prev = self
            .stages
            .get_mut(stage_id)
            .and_then(|stage| stage.input.cursor.take());
        match (prev, new_window_id) {
            (Some(prev), Some(new_id)) => {
                if prev.window != new_id {
                    self.reset_window_cursor(stage_id, &prev.window);
                }
                self.stages.get_mut(stage_id).unwrap().input.cursor = Some(CursorFocus {
                    serial,
                    window: new_id,
                    image: CursorImageStatus::default_named(),
                    presented_scale: None,
                });
            }
            (Some(prev), None) => {
                self.reset_window_cursor(stage_id, &prev.window);
            }
            (None, Some(new_id)) => {
                self.stages.get_mut(stage_id).unwrap().input.cursor = Some(CursorFocus {
                    serial,
                    window: new_id,
                    image: CursorImageStatus::default_named(),
                    presented_scale: None,
                });
            }
            (None, None) => {}
        }
    }

    pub(crate) fn clear_cursor_focus(&mut self, stage_id: &str) {
        if let Some(focus) = self
            .stages
            .get_mut(stage_id)
            .and_then(|stage| stage.input.cursor.take())
        {
            self.reset_window_cursor(stage_id, &focus.window);
        }
    }

    pub(crate) fn clear_window_cursor_focus(&mut self, stage_id: &str, window_id: &str) {
        if self
            .stages
            .get(stage_id)
            .and_then(|stage| stage.input.cursor.as_ref())
            .is_some_and(|focus| focus.window == window_id)
        {
            self.clear_cursor_focus(stage_id);
        }
    }

    pub(crate) fn cursor_request_allowed(&self, serial: Serial, object: &ObjectId) -> bool {
        self.stages.values().any(|stage| {
            stage
                .input
                .cursor
                .as_ref()
                .is_some_and(|focus| focus.serial == serial)
                && stage
                    .input
                    .current_pointer_target
                    .as_ref()
                    .filter(|target| target.alive())
                    .and_then(StageInputTarget::wl_surface)
                    .is_some_and(|surface| surface.id().same_client_as(object))
        })
    }

    pub(crate) fn update_cursor_image(&mut self, serial: Serial, image: CursorImageStatus) {
        let mut sync_target: Option<(WlSurface, Option<SavedRect>, f64)> = None;
        for stage in self.stages.values_mut() {
            let Some(focus) = stage.input.cursor.as_ref() else {
                continue;
            };
            if focus.serial != serial {
                continue;
            }
            if let CursorImageStatus::Surface(surface) = &image {
                if let Some(window) = stage.window(&focus.window) {
                    sync_target = Some((surface.clone(), window.monitor, window.scale));
                }
            }
            let focus = stage.input.cursor.as_mut().unwrap();
            focus.image = image;
            focus.presented_scale = None;
            break;
        }
        if let Some((surface, monitor, scale)) = sync_target {
            self.virtual_outputs.enter_tree(&surface, monitor, scale);
        }
    }

    pub(crate) fn cursor_committed(&mut self, surface: &WlSurface) {
        with_states(surface, |states| {
            if let Some(cursor_data) = states.data_map.get::<CursorImageSurfaceData>() {
                if let Some(delta) = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                {
                    cursor_data.lock().unwrap().hotspot -= delta;
                }
            }
        });

        let root = surface_root(surface);
        let mut sync_target: Option<(Option<SavedRect>, f64)> = None;
        for stage in self.stages.values_mut() {
            let Some(focus) = stage.input.cursor.as_ref() else {
                continue;
            };
            if matches!(&focus.image, CursorImageStatus::Surface(cursor) if cursor == &root) {
                if sync_target.is_none() {
                    if let Some(window) = stage.window(&focus.window) {
                        sync_target = Some((window.monitor, window.scale));
                    }
                }
                stage.input.cursor.as_mut().unwrap().presented_scale = None;
            }
        }
        if let Some((monitor, scale)) = sync_target {
            self.virtual_outputs.enter_tree(&root, monitor, scale);
        }
    }

    pub(crate) fn window_for_cursor_surface(&self, surface: &WlSurface) -> Option<&StageWindow> {
        let root = surface_root(surface);
        self.stages.values().find_map(|stage| {
            let focus = stage.input.cursor.as_ref()?;
            matches!(&focus.image, CursorImageStatus::Surface(cursor) if cursor == &root)
                .then(|| stage.window(&focus.window))
                .flatten()
        })
    }

    pub fn tick_cursors(&mut self, time: u32) {
        for stage in self.stages.values_mut() {
            let desktop = stage.display == DisplayState::Desktop;
            let human = stage.input_owner == InputOwner::Human;
            let Some(window_id) = stage.input.cursor.as_ref().map(|focus| focus.window.clone()) else {
                continue;
            };

            {
                let focus = stage.input.cursor.as_mut().unwrap();
                if let CursorImageStatus::Surface(surface) = &focus.image {
                    if surface.is_alive() {
                        send_frames_surface_tree(surface, time);
                    } else {
                        focus.image = CursorImageStatus::default_named();
                        focus.presented_scale = None;
                    }
                }
            }

            let Some((mapped, scale, cursor_default)) = stage
                .window(&window_id)
                .map(|w| (w.mapped, w.scale, w.cursor_default))
            else {
                stage.input.cursor.as_mut().unwrap().presented_scale = None;
                continue;
            };

            if !(desktop && human && mapped) {
                if desktop && mapped && !cursor_default {
                    if let Some(sink) = self.display_sink.as_mut() {
                        sink.set_cursor(&window_id, NativeCursor::Named("default"));
                    }
                    stage.window_mut(&window_id).unwrap().cursor_default = true;
                }
                stage.input.cursor.as_mut().unwrap().presented_scale = None;
                continue;
            }

            let focus = stage.input.cursor.as_ref().unwrap();
            if focus.presented_scale == Some(scale) {
                continue;
            }

            let image = match &focus.image {
                CursorImageStatus::Hidden => NativeCursor::Hidden,
                CursorImageStatus::Named(icon) => NativeCursor::Named(icon.name()),
                CursorImageStatus::Surface(surface) => self.offscreen.cursor_bitmap(surface, scale),
            };
            let is_default = image == NativeCursor::Named("default");
            if !(is_default && cursor_default) {
                if let Some(sink) = self.display_sink.as_mut() {
                    sink.set_cursor(&window_id, image);
                }
                stage.window_mut(&window_id).unwrap().cursor_default = is_default;
            }
            stage.input.cursor.as_mut().unwrap().presented_scale = Some(scale);
        }
    }
}
