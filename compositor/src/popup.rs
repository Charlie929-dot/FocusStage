use std::collections::HashSet;

use smithay::desktop::{find_popup_root_surface, PopupKind, PopupManager};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_popup;
use smithay::reexports::wayland_server::{
    protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
    Resource,
};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::compositor::{get_parent, with_states};
use smithay::wayland::shell::xdg::{PopupSurface, XdgPopupSurfaceData};

use crate::{stage::StageToplevel, state::FocusStageState};

pub(crate) fn surface_root(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

#[derive(Default)]
pub(crate) struct StagePopups {
    stack: Vec<(PopupSurface, Serial)>,
    last_press: Option<(Serial, WlSurface)>,
    last_release: Option<(Serial, WlSurface)>,
    swallowed: HashSet<u32>,
}

impl StagePopups {
    pub(crate) fn record_input(&mut self, serial: Serial, target: &WlSurface, pressed: bool) {
        if pressed {
            self.last_press = Some((serial, target.clone()));
            self.last_release = None;
        } else {
            self.last_release = Some((serial, target.clone()));
        }
    }

    pub(crate) fn prune_dead(&mut self) {
        self.stack.retain(|(popup, _)| popup.alive());
    }

    pub(crate) fn top(&self) -> Option<&WlSurface> {
        self.stack
            .iter()
            .rev()
            .find(|(popup, _)| popup.alive())
            .map(|(popup, _)| popup.wl_surface())
    }

    pub(crate) fn top_for_root(&self, root: &WlSurface) -> Option<&WlSurface> {
        let top = self
            .stack
            .iter()
            .rev()
            .find(|(popup, _)| popup.alive())
            .map(|(popup, _)| popup)?;
        (find_popup_root_surface(&PopupKind::Xdg(top.clone())).ok().as_ref() == Some(root))
            .then(|| top.wl_surface())
    }

    pub(crate) fn contains_surface(&self, surface: &WlSurface) -> bool {
        self.stack
            .iter()
            .any(|(popup, _)| popup.alive() && popup.wl_surface() == surface)
    }

    pub(crate) fn clear_serials(&mut self) {
        self.last_press = None;
        self.last_release = None;
    }
}

impl FocusStageState {
    pub fn has_active_popup(&self, stage_id: &str) -> bool {
        self.stages
            .get(stage_id)
            .is_some_and(|stage| stage.input.popups.top().is_some())
    }

    pub(crate) fn grab_stage_popup(&mut self, popup: PopupSurface, seat: WlSeat, serial: Serial) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            popup.send_popup_done();
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            popup.send_popup_done();
            return;
        };
        let window_id = window.id.clone();
        let stage_id = window_id.rsplit_once('/').unwrap().0;
        self.stages
            .get_mut(stage_id)
            .unwrap()
            .input
            .popups
            .prune_dead();

        let committed = with_states(popup.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgPopupSurfaceData>()
                .is_some_and(|d| d.lock().unwrap().committed)
        });
        if committed {
            popup
                .xdg_popup()
                .post_error(xdg_popup::Error::InvalidGrab, "popup already committed");
            return;
        }

        let grabs = &self.stages[stage_id].input.popups;
        let same_window = |target: &WlSurface| {
            target.is_alive()
                && target.id().same_client_as(&popup.wl_surface().id())
                && self
                    .window_for_surface(target)
                    .is_some_and(|window| window.id == window_id)
        };
        let authorized = [&grabs.last_press, &grabs.last_release]
            .into_iter()
            .flatten()
            .any(|(event, target)| *event == serial && same_window(target))
            || grabs
                .stack
                .iter()
                .any(|(existing, grab_serial)| *grab_serial == serial && same_window(existing.wl_surface()));
        let parent = popup.get_parent_surface();
        if !self.seat.owns(&seat) || !authorized || parent.as_ref() != Some(grabs.top().unwrap_or(&root)) {
            let _ = PopupManager::dismiss_popup(&root, &kind);
            self.stages
                .get_mut(stage_id)
                .unwrap()
                .bump_window_frame(&window_id);
            return;
        }
        self.stages
            .get_mut(stage_id)
            .unwrap()
            .input
            .popups
            .stack
            .push((popup, serial));
        self.ensure_virtual_focus(stage_id);
    }

    pub(crate) fn popup_gone(&mut self, popup: PopupSurface) {
        let popup_surface = popup.wl_surface().clone();
        let root = find_popup_root_surface(&PopupKind::Xdg(popup.clone())).ok();
        let window_id = root
            .as_ref()
            .and_then(|r| self.window_for_surface(r))
            .map(|window| window.id.clone());
        let Some(id) = window_id else {
            for stage in self.stages.values_mut() {
                if let Some(index) = stage
                    .input
                    .popups
                    .stack
                    .iter()
                    .position(|(member, _)| member == &popup)
                {
                    stage.input.popups.stack.truncate(index);
                }
                stage.input.popups.prune_dead();
            }
            return;
        };

        let stage_id = id.rsplit_once('/').unwrap().0.to_string();
        let (was_grabbed, kbd_was_popup, ptr_needs_refresh, clear_ptr_first, ptr_window, ptr_pos, virtual_focus) = {
            let stage = self.stages.get_mut(&stage_id).unwrap();
            let was_grabbed = if let Some(index) = stage
                .input
                .popups
                .stack
                .iter()
                .position(|(member, _)| member == &popup)
            {
                stage.input.popups.stack.truncate(index);
                true
            } else {
                false
            };
            stage.input.popups.prune_dead();
            stage.bump_window_frame(&id);

            let kbd_was_popup = stage
                .input
                .current_keyboard_target
                .as_ref()
                .is_some_and(|target| {
                    !target.alive()
                        || target
                            .wl_surface()
                            .is_some_and(|s| surface_root(&s) == popup_surface)
                });
            let ptr_needs_refresh = stage
                .input
                .current_pointer_target
                .as_ref()
                .is_some_and(|target| {
                    !target.alive()
                        || target
                            .wl_surface()
                            .is_some_and(|s| surface_root(&s) == popup_surface)
                });
            (
                was_grabbed,
                kbd_was_popup,
                ptr_needs_refresh,
                !stage.input.pressed_buttons.is_empty(),
                stage.input.pointer_window_id.clone(),
                stage.input.pointer_pos,
                stage.uses_virtual_focus(),
            )
        };

        if ptr_needs_refresh {
            if ptr_window.as_deref() == Some(id.as_str()) || virtual_focus {
                if clear_ptr_first {
                    self.clear_stage_pointer_target(&stage_id);
                }
                self.send_window_pointer_motion_at(
                    &stage_id,
                    ptr_window.as_deref(),
                    ptr_pos.x,
                    ptr_pos.y,
                    self.now_ms(),
                );
            } else {
                self.clear_stage_pointer_target(&stage_id);
            }
        }
        if was_grabbed || kbd_was_popup {
            self.ensure_virtual_focus(&stage_id);
        }
    }

    pub(crate) fn dismiss_stage_popups(&mut self, stage_id: &str) {
        let Some(popped) = self.stages.get_mut(stage_id).map(|stage| {
            stage.input.popups.prune_dead();
            std::mem::take(&mut stage.input.popups.stack)
        }) else {
            return;
        };
        let Some((first, _)) = popped.first().cloned() else {
            return;
        };
        let kind = PopupKind::Xdg(first);
        if let Ok(root) = find_popup_root_surface(&kind) {
            let in_tree =
                PopupManager::popups_for_surface(&root).any(|(p, _)| p.wl_surface() == kind.wl_surface());
            let _ = PopupManager::dismiss_popup(&root, &kind);
            if !in_tree {
                for (popup, _) in popped.into_iter().rev() {
                    popup.send_popup_done();
                }
            }
            if let Some(id) = self.window_for_surface(&root).map(|window| window.id.clone()) {
                self.stages.get_mut(stage_id).unwrap().bump_window_frame(&id);
            }
        } else {
            for (popup, _) in popped.into_iter().rev() {
                popup.send_popup_done();
            }
        }
        self.ensure_virtual_focus(stage_id);
    }

    pub(crate) fn dismiss_window_popups(&mut self, stage_id: &str, window_id: &str) {
        let should_dismiss = self.stages.get(stage_id).is_some_and(|stage| {
            stage.input.popups.stack.iter().any(|(popup, _)| {
                !popup.alive()
                    || find_popup_root_surface(&PopupKind::Xdg(popup.clone()))
                        .ok()
                        .and_then(|root| self.window_for_surface(&root))
                        .is_none_or(|window| window.id == window_id)
            })
        });
        if should_dismiss {
            self.dismiss_stage_popups(stage_id);
        }
    }

    pub(crate) fn consume_popup_button(
        &mut self,
        stage_id: &str,
        window: Option<&str>,
        point: Point<f64, Logical>,
        button: u32,
        pressed: bool,
    ) -> bool {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return false;
        };
        if !pressed {
            return stage.input.popups.swallowed.remove(&button);
        }
        stage.input.popups.prune_dead();
        if stage.input.popups.top().is_none() {
            return false;
        }
        if let Some(StageToplevel::Wayland(toplevel)) = stage.input_toplevel(window).cloned() {
            let (hit, _) = self.hit_test_wayland_surface(toplevel.wl_surface(), point);
            let hit_root = surface_root(&hit);
            if self.stages[stage_id].input.popups.contains_surface(&hit_root) {
                return false;
            }
        }
        self.dismiss_stage_popups(stage_id);
        let grabs = &mut self.stages.get_mut(stage_id).unwrap().input.popups;
        grabs.clear_serials();
        grabs.swallowed.insert(button);
        true
    }
}
