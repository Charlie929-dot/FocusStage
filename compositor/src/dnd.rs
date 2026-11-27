use std::io::Read;
use std::os::fd::AsFd;
use std::path::Path;

use smithay::reexports::wayland_server::{
    protocol::wl_data_device_manager::DndAction,
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_source::WlDataSource,
        wl_surface::WlSurface,
    },
    Resource,
};
use smithay::utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER};
use smithay::wayland::{
    compositor::give_role,
    selection::data_device::{with_source_metadata, DnDGrab, DND_ICON_ROLE},
};

use crate::protocol::{DisplayState, InputOwner};
use crate::sink::CapturedDragTarget;
use crate::stage::StageToplevel;
use crate::state::FocusStageState;

pub(crate) struct StageDrag {
    grab: DnDGrab<FocusStageState>,
    origin: WlSurface,
    source: Option<WlDataSource>,
    pub(crate) window_id: String,
    serial: u32,
    windows_paths: Vec<String>,
    /// Windows currently owns the physical pointer gesture on behalf of this grab.
    native_drag: bool,
}

const URI_MIME: &str = "text/uri-list";
const MAX_URI_LIST_BYTES: u64 = 1024 * 1024;

impl FocusStageState {
    pub(crate) fn start_stage_drag(
        &mut self,
        stage_id: &str,
        device: &WlDataDevice,
        source: Option<WlDataSource>,
        origin: WlSurface,
        icon: Option<WlSurface>,
        serial: Serial,
    ) {
        let stage = self.stages.get(stage_id);
        if stage.is_some_and(|s| s.input.drag.is_some()) {
            return;
        }
        let window_id = stage.and_then(|s| {
            let (press_serial, focus) = s.input.pointer_press.as_ref()?;
            if *press_serial != serial || !focus.id().same_client_as(&origin.id()) {
                return None;
            }
            let window = self.window_for_surface(focus)?;
            (s.window(&window.id).is_some() && self.window_for_surface(&origin)?.id == window.id)
                .then(|| window.id.clone())
        });
        let Some(window_id) = window_id else {
            // A client can process the initiating button event after the button has already
            // been released. It still needs to be told to retire its drag source.
            if let Some(source) = source {
                source.cancelled();
            }
            return;
        };
        if let Some(icon) = &icon {
            if give_role(icon, DND_ICON_ROLE).is_err() {
                device.post_error(wl_data_device::Error::Role, "Surface already has a role");
                return;
            }
        }
        let grab = DnDGrab::new_manual(
            &self.display_handle,
            source.clone(),
            origin.clone(),
            self.seat.clone(),
            icon,
        );
        let time = self.now_ms();
        let serial = serial.into();
        let (window, pos) = {
            let input = &mut self.stages.get_mut(stage_id).unwrap().input;
            input.pointer_press = None;
            input.drag = Some(StageDrag {
                grab,
                origin,
                source: source.clone(),
                window_id,
                serial,
                windows_paths: Vec::new(),
                native_drag: false,
            });
            (input.pointer_window_id.clone(), input.pointer_pos)
        };
        self.clear_stage_pointer_target(stage_id);
        if window.is_some()
            && self.display_sink.is_some()
            && self.stages[stage_id].input_owner == InputOwner::Human
        {
            if let Some(source) = source.filter(|source| {
                with_source_metadata(source, |meta| {
                    meta.dnd_action.contains(DndAction::Copy)
                        && meta.mime_types.iter().any(|mime| mime == URI_MIME)
                })
                .unwrap_or(false)
            }) {
                self.read_drag_paths(stage_id, serial, &source);
            }
        }
        self.route_drag_motion(stage_id, window.as_deref(), pos, time);
    }

    pub fn set_captured_drag_target(
        &mut self,
        stage_id: &str,
        window_id: &str,
        target: Option<CapturedDragTarget>,
    ) {
        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.captured_drag_target = target.map(|target| (window_id.to_owned(), target));
        }
    }

    fn read_drag_paths(&mut self, stage_id: &str, serial: u32, source: &WlDataSource) {
        let Ok((read, write)) = crate::clipboard::create_pipe() else {
            return;
        };
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<Vec<String>>();
        let stage_id = stage_id.to_owned();
        if self
            .loop_handle
            .insert_source(rx, move |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(paths) = event {
                    if let Some(stage) = state.stages.get_mut(&stage_id) {
                        if let Some(drag) = stage
                            .input
                            .drag
                            .as_mut()
                            .filter(|drag| drag.serial == serial)
                        {
                            drag.windows_paths = paths;
                            let pos = stage.input.pointer_pos;
                            let window = stage.input.pointer_window_id.clone();
                            state.route_drag_motion(
                                &stage_id,
                                window.as_deref(),
                                pos,
                                state.now_ms(),
                            );
                        }
                    }
                }
            })
            .is_err()
        {
            return;
        }
        source.send(URI_MIME.into(), write.as_fd());
        std::thread::spawn(move || {
            let file = std::fs::File::from(read);
            let mut payload = String::new();
            if file
                .take(MAX_URI_LIST_BYTES + 1)
                .read_to_string(&mut payload)
                .is_ok()
                && payload.len() as u64 <= MAX_URI_LIST_BYTES
            {
                if let Some(paths) = windows_drag_paths(&payload) {
                    let _ = tx.send(paths);
                }
            }
        });
    }

    pub(crate) fn route_drag_motion(
        &mut self,
        stage_id: &str,
        window_id: Option<&str>,
        pos: Point<f64, Logical>,
        time: u32,
    ) -> bool {
        let Some(mut drag) = self.stages.get_mut(stage_id).unwrap().input.drag.take() else {
            return false;
        };
        let serial = SERIAL_COUNTER.next_serial();
        if !drag.origin.is_alive() || drag.source.as_ref().is_some_and(|src| !src.is_alive()) {
            if drag.native_drag {
                if let Some(sink) = self.display_sink.as_mut() {
                    sink.cancel_file_drag(&drag.window_id, drag.serial);
                }
            }
            drag.grab.update_focus::<WlSurface>(None, pos, serial, time);
            drag.grab.finish(self);
            return true;
        }
        let stage = &self.stages[stage_id];
        let captured = stage
            .input
            .captured_drag_target
            .as_ref()
            .filter(|(owner, _)| window_id == Some(owner.as_str()))
            .map(|(_, target)| target);
        let (window, pos) = match captured {
            Some(CapturedDragTarget::Window { window_id, x, y }) => {
                let target_stage = self.stage_for_window(window_id);
                let window = target_stage
                    .filter(|target| {
                        target.display == DisplayState::Desktop
                            && target.input_owner == InputOwner::Human
                    })
                    .and_then(|target| target.window(window_id));
                (window, (*x, *y).into())
            }
            Some(CapturedDragTarget::External) => (None, pos),
            None => (
                match window_id {
                    Some(id) => stage.window(id),
                    None => stage.active_window(),
                },
                pos,
            ),
        };
        let target = window
            .filter(|w| w.mapped && Rectangle::from_size(w.logical_size.to_f64()).contains(pos))
            .and_then(|w| match &w.toplevel {
                StageToplevel::Wayland(toplevel) => {
                    Some(self.hit_test_wayland_surface(toplevel.wl_surface(), pos))
                }
                StageToplevel::X11(_) => None,
            });
        if !drag.native_drag {
            drag.grab.update_focus(target, pos, serial, time);
            if matches!(captured, Some(CapturedDragTarget::External))
                && window_id == Some(drag.window_id.as_str())
                && !drag.windows_paths.is_empty()
                && stage.input.pressed_buttons.contains(&0x110)
            {
                if let Some(sink) = self.display_sink.as_mut() {
                    drag.native_drag =
                        sink.begin_file_drag(&drag.window_id, drag.serial, &drag.windows_paths);
                }
            }
        }
        self.stages.get_mut(stage_id).unwrap().input.drag = Some(drag);
        true
    }

    pub(crate) fn finish_stage_drag(&mut self, stage_id: &str) {
        let input = &mut self.stages.get_mut(stage_id).unwrap().input;
        if input.drag.as_ref().is_some_and(|drag| drag.native_drag) {
            return; // the OLE result decides whether the file was copied or returned to Wayland
        }
        input.captured_drag_target = None;
        if let Some(drag) = input.drag.take() {
            drag.grab.finish(self);
        }
    }

    pub fn handle_file_drag_ended(
        &mut self,
        window_id: &str,
        serial: u32,
        dropped: bool,
        continuing: bool,
        target: Option<CapturedDragTarget>,
    ) {
        let Some(stage_id) = self
            .stage_for_window(window_id)
            .map(|stage| stage.id.clone())
        else {
            return;
        };
        let input = &mut self.stages.get_mut(&stage_id).unwrap().input;
        let Some(drag) = input.drag.as_mut().filter(|drag| {
            drag.native_drag && drag.serial == serial && drag.window_id == window_id
        }) else {
            return;
        };
        drag.native_drag = false;
        if continuing && input.pressed_buttons.contains(&0x110) {
            let pos = input.pointer_pos;
            self.set_captured_drag_target(&stage_id, window_id, target);
            self.route_drag_motion(&stage_id, Some(window_id), pos, self.now_ms());
        } else {
            let drag = input.drag.take().unwrap();
            input.pressed_buttons.clear();
            input.pointer_press = None;
            input.captured_drag_target = None;
            if dropped {
                if let Some(source) = drag
                    .source
                    .filter(|source| source.is_alive() && source.version() >= 3)
                {
                    source.action(DndAction::Copy);
                    source.dnd_drop_performed();
                    source.dnd_finished();
                }
            } else {
                drag.grab.finish(self);
            }
        }
    }

    pub fn cancel_stage_drag(&mut self, stage_id: &str) -> bool {
        let Some(drag) = self.stages.get_mut(stage_id).and_then(|stage| {
            stage.input.captured_drag_target = None;
            stage.input.drag.take()
        }) else {
            return false;
        };
        if drag.native_drag {
            if let Some(sink) = self.display_sink.as_mut() {
                sink.cancel_file_drag(&drag.window_id, drag.serial);
            }
        }
        let mut grab = drag.grab;
        grab.update_focus::<WlSurface>(
            None,
            Point::default(),
            SERIAL_COUNTER.next_serial(),
            self.now_ms(),
        );
        grab.finish(self);
        true
    }

    pub fn cancel_desktop_drags(&mut self) {
        let stage_ids: Vec<String> = self
            .stages
            .iter()
            .filter(|(_, stage)| {
                stage.input.drag.is_some()
                    || stage.input.captured_drag_target.is_some()
                    || !stage.input.pressed_buttons.is_empty()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for stage_id in stage_ids {
            self.cancel_stage_drag(&stage_id);
            if let Some(stage) = self.stages.get_mut(&stage_id) {
                stage.input.captured_drag_target = None;
                stage.input.pressed_buttons.clear();
                stage.input.pointer_press = None;
            }
        }
    }
}

fn translate_to_windows_path(path: &str) -> Option<String> {
    if let Ok(result) = std::process::Command::new("/usr/bin/wslpath")
        .args(["-w", path])
        .output()
    {
        let translated = std::str::from_utf8(&result.stdout)
            .ok()?
            .trim_end_matches(['\r', '\n']);
        if result.status.success()
            && !translated.is_empty()
            && !translated.chars().any(char::is_control)
        {
            return Some(translated.to_owned());
        }
        return None;
    }
    #[cfg(test)]
    if !Path::new("/usr/bin/wslpath").exists() {
        return Some(format!(
            r"\\wsl.localhost\test\{}",
            path.trim_start_matches('/').replace('/', "\\")
        ));
    }
    None
}

fn windows_drag_paths(payload: &str) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for uri in payload
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let target = crate::xdg::OpenTarget::parse(uri).ok()?;
        let path = target.local_path()?;
        if !uri.starts_with("file:")
            || !Path::new(path).is_absolute()
            || !std::fs::metadata(path).is_ok_and(|m| m.is_file() || m.is_dir())
        {
            return None;
        }
        paths.push(translate_to_windows_path(path)?);
    }
    (!paths.is_empty()).then_some(paths)
}
