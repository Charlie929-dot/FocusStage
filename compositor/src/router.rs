use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::desktop::PopupManager;
use smithay::input::keyboard::{xkb, KeyboardTarget, KeymapFile, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerTarget};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::{wl_keyboard::KeyState as WlKeyState, wl_surface::WlSurface},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Serial, SERIAL_COUNTER};
use tracing::{debug, warn};

use crate::offscreen::wayland_window_geometry_loc;
use crate::protocol::{ActStatus, Action, DisplayState, InputOwner};
use crate::stage::{StageInputTarget, StageToplevel};
use crate::state::FocusStageState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    Human,
    Agent,
}

#[derive(Debug)]
pub struct TextInputInstance {
    pub resource: ZwpTextInputV3,
    pub serial: u32,
    pub enabled: bool,
    pub client_id: ClientId,
}

#[derive(Debug, Default)]
pub struct StageTextInputManagerState {
    pub global: Option<GlobalId>,
    pub instances: Vec<TextInputInstance>,
}

impl StageTextInputManagerState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<FocusStageState, ZwpTextInputManagerV3, ()>(1, ());
        Self {
            global: Some(global),
            instances: Vec::new(),
        }
    }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for FocusStageState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for FocusStageState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &ZwpTextInputManagerV3,
        request: zwp_text_input_manager_v3::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_text_input_manager_v3::Request::GetTextInput { id, seat: _ } = request {
            let res = data_init.init(id, ());
            let cid = client.id();
            state.text_input_state.instances.push(TextInputInstance {
                resource: res.clone(),
                serial: 0,
                enabled: false,
                client_id: cid.clone(),
            });
            // If this client's Stage already has a live surface, send enter immediately.
            if let Some(sid) = state.stage_id_for_client(&cid) {
                if let Some(stage) = state.stages.get(&sid) {
                    if let Some(surf) = stage.active_toplevel().and_then(|t| t.wl_surface()) {
                        res.enter(&surf);
                    }
                }
            }
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for FocusStageState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let Some(inst) = state
            .text_input_state
            .instances
            .iter_mut()
            .find(|i| i.resource == *resource)
        {
            match request {
                zwp_text_input_v3::Request::Enable => inst.enabled = true,
                zwp_text_input_v3::Request::Disable => inst.enabled = false,
                zwp_text_input_v3::Request::Commit => inst.serial = inst.serial.wrapping_add(1),
                zwp_text_input_v3::Request::Destroy => {}
                _ => {}
            }
        }
    }

    fn destroyed(
        state: &mut Self,
        _client_id: ClientId,
        resource: &ZwpTextInputV3,
        _data: &(),
    ) {
        state
            .text_input_state
            .instances
            .retain(|i| i.resource != *resource);
    }
}

pub struct SettleState {
    pub initial_seq: u64,
    pub last_seq: u64,
    pub quiet_since: Instant,
    pub started_at: Instant,
    pub deadline: Instant,
}

pub struct PendingActJob {
    pub stage_id: String,
    pub lease_id: String,
    pub remaining: VecDeque<Action>,
    pub executed_steps: usize,
    pub wait_until: Option<Instant>,
    pub settling: Option<SettleState>,
    pub reply_tx: std::sync::mpsc::Sender<crate::protocol::ControlResponse>,
}

impl FocusStageState {
    pub fn now_ms(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    pub fn resolve_input_time(&mut self, sink_time: u32) -> u32 {
        let now = self.now_ms();
        if sink_time == 0 {
            return now;
        }
        match self.wslg_time_anchor {
            Some((base_sink, base_local)) => {
                let delta = sink_time.wrapping_sub(base_sink) as i32;
                let mapped_i64 = (base_local as i64).wrapping_add(delta as i64);
                let mapped = if mapped_i64 >= 0 { mapped_i64 as u32 } else { 0 };
                let drift = (mapped.wrapping_sub(now) as i32).abs();
                if drift > 5000 {
                    self.wslg_time_anchor = Some((sink_time, now));
                    now
                } else {
                    mapped
                }
            }
            None => {
                self.wslg_time_anchor = Some((sink_time, now));
                now
            }
        }
    }

    fn set_keyboard_target(
        &mut self,
        stage_id: &str,
        new_target: StageInputTarget,
        mods_state: ModifiersState,
        serial: Serial,
    ) {
        let prev = self
            .stages
            .get(stage_id)
            .and_then(|s| s.input.current_keyboard_target.clone());
        if prev.as_ref().is_some_and(|p| p.same_target(&new_target)) && new_target.alive() {
            return;
        }

        let seat = self.seat.clone();
        if let Some(old) = prev {
            if old.alive() {
                match &old {
                    StageInputTarget::Wayland(old_surf) => {
                        if let Some(client) = old_surf.client() {
                            let cid = client.id();
                            let ti_res: Vec<_> = self
                                .text_input_state
                                .instances
                                .iter()
                                .filter(|ti| ti.client_id == cid)
                                .map(|ti| ti.resource.clone())
                                .collect();
                            for res in ti_res {
                                res.leave(old_surf);
                            }
                        }
                        KeyboardTarget::leave(old_surf, &seat, self, serial);
                    }
                    StageInputTarget::X11(old_x11) => {
                        KeyboardTarget::leave(old_x11, &seat, self, serial);
                    }
                }
            }
        }

        match &new_target {
            StageInputTarget::Wayland(wl_surf) => {
                if let Some(client) = wl_surf.client() {
                    let has_kbd = seat
                        .get_keyboard()
                        .map(|k| k.client_keyboards(&client).next().is_some())
                        .unwrap_or(false);
                    if !has_kbd {
                        if let Some(stage) = self.stages.get_mut(stage_id) {
                            stage.input.current_keyboard_target = None;
                        }
                        return;
                    }
                    KeyboardTarget::enter(wl_surf, &seat, self, vec![], serial);
                    KeyboardTarget::modifiers(wl_surf, &seat, self, mods_state, serial);

                    let cid = client.id();
                    let ti_res: Vec<_> = self
                        .text_input_state
                        .instances
                        .iter()
                        .filter(|ti| ti.client_id == cid)
                        .map(|ti| ti.resource.clone())
                        .collect();
                    for res in ti_res {
                        res.enter(wl_surf);
                    }
                }
            }
            StageInputTarget::X11(x11_tl) => {
                KeyboardTarget::enter(x11_tl, &seat, self, vec![], serial);
                KeyboardTarget::modifiers(x11_tl, &seat, self, mods_state, serial);
            }
        }

        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.current_keyboard_target = Some(new_target);
        }
    }

    /// Switch `wl_pointer` focus on this Stage, sending `leave` to the previous target first.
    /// Returns `true` if `PointerTarget::enter` was sent to `new_target`.
    fn set_pointer_target(
        &mut self,
        stage_id: &str,
        new_target: StageInputTarget,
        local: Point<f64, Logical>,
        serial: Serial,
        time: u32,
    ) -> bool {
        let prev = self
            .stages
            .get(stage_id)
            .and_then(|s| s.input.current_pointer_target.clone());
        if prev.as_ref().is_some_and(|p| p.same_target(&new_target)) && new_target.alive() {
            return false;
        }

        let seat = self.seat.clone();
        if let Some(old) = prev {
            if old.alive() {
                match &old {
                    StageInputTarget::Wayland(old_surf) => {
                        PointerTarget::leave(old_surf, &seat, self, serial, time);
                        PointerTarget::frame(old_surf, &seat, self);
                    }
                    StageInputTarget::X11(old_x11) => {
                        PointerTarget::leave(old_x11, &seat, self, serial, time);
                        PointerTarget::frame(old_x11, &seat, self);
                    }
                }
            }
        }

        let ev = MotionEvent {
            location: local,
            serial,
            time,
        };
        match &new_target {
            StageInputTarget::Wayland(wl_surf) => {
                let has_ptr = wl_surf
                    .client()
                    .and_then(|client| {
                        seat.get_pointer()
                            .map(|p| p.client_pointers(&client).next().is_some())
                    })
                    .unwrap_or(false);
                if !has_ptr {
                    if let Some(stage) = self.stages.get_mut(stage_id) {
                        stage.input.current_pointer_target = None;
                    }
                    return false;
                }
                PointerTarget::enter(wl_surf, &seat, self, &ev);
                PointerTarget::frame(wl_surf, &seat, self);
            }
            StageInputTarget::X11(x11_tl) => {
                PointerTarget::enter(x11_tl, &seat, self, &ev);
                PointerTarget::frame(x11_tl, &seat, self);
            }
        }

        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.current_pointer_target = Some(new_target);
        }
        true
    }

    /// Ensure virtual focus is permanently active for a Stage without sending `leave` to any other Stage.
    pub fn ensure_virtual_focus(&mut self, stage_id: &str) {
        let Some(stage) = self.stages.get(stage_id) else {
            return;
        };
        let toplevel = match stage.active_toplevel() {
            Some(t) => t.clone(),
            None => return,
        };
        let mods_state = stage.input.mods_state;
        let pointer_pos = stage.input.pointer_pos;
        let cur_kbd = stage.input.current_keyboard_target.clone();
        let cur_ptr = stage.input.current_pointer_target.clone();
        let time = self.now_ms();

        match toplevel {
            StageToplevel::Wayland(ref wl_tl) => {
                let wl_surf = wl_tl.wl_surface().clone();
                let kbd_valid = cur_kbd.as_ref().is_some_and(|t| {
                    t.alive()
                        && matches!(t, StageInputTarget::Wayland(s) if s == &wl_surf || PopupManager::popups_for_surface(&wl_surf).any(|(p, _)| p.wl_surface() == s))
                });
                if !kbd_valid {
                    let serial = SERIAL_COUNTER.next_serial();
                    self.set_keyboard_target(
                        stage_id,
                        StageInputTarget::Wayland(wl_surf.clone()),
                        mods_state,
                        serial,
                    );
                }

                let ptr_valid = cur_ptr.as_ref().is_some_and(|t| {
                    t.alive()
                        && matches!(t, StageInputTarget::Wayland(s) if s == &wl_surf || PopupManager::popups_for_surface(&wl_surf).any(|(p, _)| p.wl_surface() == s))
                });
                if !ptr_valid {
                    let (target, local) =
                        self.hit_test_wayland_surface(&wl_surf, pointer_pos.x, pointer_pos.y);
                    let serial = SERIAL_COUNTER.next_serial();
                    self.set_pointer_target(
                        stage_id,
                        StageInputTarget::Wayland(target),
                        local,
                        serial,
                        time,
                    );
                }
            }
            StageToplevel::X11(ref x11_tl) => {
                let _ = x11_tl.set_activated(true);
                if x11_tl.wl_surface().is_some() {
                    let kbd_valid = cur_kbd.as_ref().is_some_and(|t| {
                        t.alive() && t.same_target(&StageInputTarget::X11(x11_tl.clone()))
                    });
                    if !kbd_valid {
                        let serial = SERIAL_COUNTER.next_serial();
                        self.set_keyboard_target(
                            stage_id,
                            StageInputTarget::X11(x11_tl.clone()),
                            mods_state,
                            serial,
                        );
                    }
                    let ptr_valid = cur_ptr.as_ref().is_some_and(|t| t.alive());
                    if !ptr_valid {
                        let serial = SERIAL_COUNTER.next_serial();
                        self.set_pointer_target(
                            stage_id,
                            StageInputTarget::X11(x11_tl.clone()),
                            pointer_pos,
                            serial,
                            time,
                        );
                    }
                }
            }
        }
    }

    pub fn is_input_allowed(&self, stage_id: &str, source: InputSource, lease_id: Option<&str>) -> bool {
        let Some(stage) = self.stages.get(stage_id) else {
            return false;
        };
        match (&stage.input_owner, source) {
            (InputOwner::Human, InputSource::Human) => true,
            (InputOwner::Agent { lease_id: active, .. }, InputSource::Agent) => {
                if let Some(lease) = &stage.lease {
                    !lease.is_expired() && Some(active.as_str()) == lease_id
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Hard preemption: human takes over one or all Stages immediately.
    pub fn human_takeover(&mut self, stage_id: Option<&str>) -> Vec<String> {
        let ids: Vec<String> = match stage_id {
            Some(id) => {
                if self.stages.contains_key(id) {
                    vec![id.to_string()]
                } else {
                    Vec::new()
                }
            }
            None => self.stages.keys().cloned().collect(),
        };

        for id in &ids {
            // Release any held keys/buttons from AI before switching to Human.
            self.release_all_stage_inputs(id);

            let (should_show, title, wm_class, rect) = if let Some(stage) = self.stages.get_mut(id) {
                stage.lease = None;
                stage.input_owner = InputOwner::Human;
                let was_hidden = stage.display == DisplayState::Hidden;
                stage.display = DisplayState::Desktop;
                let rect = stage.saved_rect.unwrap_or(crate::protocol::SavedRect {
                    x: 100,
                    y: 100,
                    width: stage.logical_size.w.max(1) as u32,
                    height: stage.logical_size.h.max(1) as u32,
                });
                (was_hidden, stage.title.clone(), stage.wm_class.clone(), rect)
            } else {
                (false, String::new(), String::from("FocusStage"), crate::protocol::SavedRect { x: 100, y: 100, width: 800, height: 600 })
            };

            if should_show {
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.show(id, &title, &wm_class, rect);
                }
                if let Some(stage) = self.stages.get_mut(id) {
                    stage.dirty_for_sink = true;
                }
            }
        }

        // Abort any pending multi-step Agent jobs for taken-over Stages.
        let mut remaining_jobs = Vec::new();
        for job in self.pending_act_jobs.drain(..) {
            if ids.contains(&job.stage_id) {
                let frame_seq = self
                    .stages
                    .get(&job.stage_id)
                    .map(|s| s.frame_seq)
                    .unwrap_or(0);
                let _ = job.reply_tx.send(crate::protocol::ControlResponse::ActResult {
                    act_status: ActStatus::Preempted,
                    executed_steps: job.executed_steps,
                    frame_seq,
                    message: Some("Preempted by human takeover".to_string()),
                });
            } else {
                remaining_jobs.push(job);
            }
        }
        self.pending_act_jobs = remaining_jobs;
        ids
    }

    fn release_all_stage_inputs(&mut self, stage_id: &str) {
        let (pressed_keys, pressed_buttons, pos) = match self.stages.get(stage_id) {
            Some(s) => (
                s.input.pressed_keys.iter().copied().collect::<Vec<_>>(),
                s.input.pressed_buttons.iter().copied().collect::<Vec<_>>(),
                s.input.pointer_pos,
            ),
            None => return,
        };
        for kc in pressed_keys {
            self.send_stage_evdev_key(stage_id, kc, false);
        }
        for btn in pressed_buttons {
            self.send_stage_button(stage_id, pos.x, pos.y, btn, false);
        }
    }

    /// Resolve Stage-relative `(x, y)` to `(WlSurface | X11Surface target, local_point)` checking popups first.
    fn hit_test_wayland_surface(&self, root: &WlSurface, x: f64, y: f64) -> (WlSurface, Point<f64, Logical>) {
        // PopupManager::popups_for_surface yields topmost child popups before their parents.
        for (popup, popup_loc) in PopupManager::popups_for_surface(root) {
            let psurf = popup.wl_surface().clone();
            let geo = popup.geometry();
            let px = popup_loc.x as f64;
            let py = popup_loc.y as f64;
            let pw = geo.size.w.max(1) as f64;
            let ph = geo.size.h.max(1) as f64;
            if x >= px && x < px + pw && y >= py && y < py + ph {
                return (psurf, Point::from((x - px + geo.loc.x as f64, y - py + geo.loc.y as f64)));
            }
        }
        let geo_loc = wayland_window_geometry_loc(root);
        (
            root.clone(),
            Point::from((x + geo_loc.x as f64, y + geo_loc.y as f64)),
        )
    }

    pub fn send_stage_pointer_motion(&mut self, stage_id: &str, x: f64, y: f64) {
        let time = self.now_ms();
        self.send_stage_pointer_motion_at(stage_id, x, y, time);
    }

    pub fn send_stage_pointer_motion_at(&mut self, stage_id: &str, x: f64, y: f64, time: u32) {
        self.ensure_virtual_focus(stage_id);
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.active_toplevel().cloned()) else {
            return;
        };
        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.pointer_pos = Point::from((x, y));
        }
        let seat = self.seat.clone();
        let serial = SERIAL_COUNTER.next_serial();

        match toplevel {
            StageToplevel::Wayland(tl) => {
                let (target, local) = self.hit_test_wayland_surface(tl.wl_surface(), x, y);
                let entered = self.set_pointer_target(
                    stage_id,
                    StageInputTarget::Wayland(target.clone()),
                    local,
                    serial,
                    time,
                );
                if !entered {
                    let ev = MotionEvent {
                        location: local,
                        serial,
                        time,
                    };
                    PointerTarget::motion(&target, &seat, self, &ev);
                    PointerTarget::frame(&target, &seat, self);
                }
            }
            StageToplevel::X11(x11) => {
                let geo = x11.geometry();
                // Check override-redirect X11 popup windows (menus/dropdowns) first
                let mut hit_x11 = x11.clone();
                let mut local = Point::from((x, y));
                if let Some(xwm_id) = self.stages.get(stage_id).and_then(|s| s.xwm_id) {
                    for ovr in self.x11_override_windows_for(xwm_id).into_iter().rev() {
                        let ogeo = ovr.geometry();
                        let rx = (ogeo.loc.x - geo.loc.x) as f64;
                        let ry = (ogeo.loc.y - geo.loc.y) as f64;
                        let rw = ogeo.size.w.max(1) as f64;
                        let rh = ogeo.size.h.max(1) as f64;
                        if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
                            hit_x11 = ovr;
                            local = Point::from((x - rx, y - ry));
                            break;
                        }
                    }
                }
                if hit_x11.wl_surface().is_some() {
                    let entered = self.set_pointer_target(
                        stage_id,
                        StageInputTarget::X11(hit_x11.clone()),
                        local,
                        serial,
                        time,
                    );
                    if !entered {
                        let ev = MotionEvent {
                            location: local,
                            serial,
                            time,
                        };
                        PointerTarget::motion(&hit_x11, &seat, self, &ev);
                        PointerTarget::frame(&hit_x11, &seat, self);
                    }
                }
            }
        }
    }

    pub fn send_stage_button(&mut self, stage_id: &str, x: f64, y: f64, button: u32, pressed: bool) {
        let time = self.now_ms();
        self.send_stage_button_at(stage_id, x, y, button, pressed, time);
    }

    pub fn send_stage_button_at(
        &mut self,
        stage_id: &str,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        time: u32,
    ) {
        self.send_stage_pointer_motion_at(stage_id, x, y, time);
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.active_toplevel().cloned()) else {
            return;
        };
        if let Some(stage) = self.stages.get_mut(stage_id) {
            if pressed {
                stage.input.pressed_buttons.insert(button);
            } else {
                stage.input.pressed_buttons.remove(&button);
            }
        }

        let seat = self.seat.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let ev = ButtonEvent {
            serial,
            time,
            button,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        };

        match toplevel {
            StageToplevel::Wayland(tl) => {
                let (target, _) = self.hit_test_wayland_surface(tl.wl_surface(), x, y);
                PointerTarget::button(&target, &seat, self, &ev);
                PointerTarget::frame(&target, &seat, self);
            }
            StageToplevel::X11(x11) => {
                let geo = x11.geometry();
                let mut hit_x11 = x11;
                if let Some(xwm_id) = self.stages.get(stage_id).and_then(|s| s.xwm_id) {
                    for ovr in self.x11_override_windows_for(xwm_id).into_iter().rev() {
                        let ogeo = ovr.geometry();
                        let rx = (ogeo.loc.x - geo.loc.x) as f64;
                        let ry = (ogeo.loc.y - geo.loc.y) as f64;
                        if x >= rx
                            && x < rx + ogeo.size.w.max(1) as f64
                            && y >= ry
                            && y < ry + ogeo.size.h.max(1) as f64
                        {
                            hit_x11 = ovr;
                            break;
                        }
                    }
                }
                PointerTarget::button(&hit_x11, &seat, self, &ev);
                PointerTarget::frame(&hit_x11, &seat, self);
            }
        }
    }

    pub fn send_stage_scroll(&mut self, stage_id: &str, x: f64, y: f64, dx: f64, dy: f64) {
        let h120 = if dx.abs() > f64::EPSILON {
            let steps = ((dx / 15.0) * 120.0).round() as i32;
            if steps == 0 {
                if dx > 0.0 { 1 } else { -1 }
            } else {
                steps
            }
        } else {
            0
        };
        let v120 = if dy.abs() > f64::EPSILON {
            let steps = ((dy / 15.0) * 120.0).round() as i32;
            if steps == 0 {
                if dy > 0.0 { 1 } else { -1 }
            } else {
                steps
            }
        } else {
            0
        };
        let synth_v120 = if h120 != 0 || v120 != 0 {
            Some((h120, v120))
        } else {
            None
        };
        self.send_stage_scroll_frame(
            stage_id,
            x,
            y,
            dx,
            dy,
            Some(AxisSource::Wheel),
            synth_v120,
            (false, false),
            None,
        );
    }

    pub fn send_stage_scroll_frame(
        &mut self,
        stage_id: &str,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        source: Option<AxisSource>,
        v120: Option<(i32, i32)>,
        stop: (bool, bool),
        time: Option<u32>,
    ) {
        let time = time.unwrap_or_else(|| self.now_ms());
        let need_motion = self.stages.get(stage_id).is_none_or(|s| {
            s.input.current_pointer_target.as_ref().is_none_or(|t| !t.alive())
                || (s.input.pointer_pos.x - x).abs() > 0.01
                || (s.input.pointer_pos.y - y).abs() > 0.01
        });
        if need_motion {
            self.send_stage_pointer_motion_at(stage_id, x, y, time);
        } else {
            self.ensure_virtual_focus(stage_id);
        }
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.active_toplevel().cloned()) else {
            return;
        };
        let seat = self.seat.clone();
        let resolved_source = source.unwrap_or(AxisSource::Wheel);
        debug!(
            "[scroll-router] stage={} pos=({:.1},{:.1}) d=({:.3},{:.3}) source={:?} v120={:?} stop={:?} time={} target={}",
            stage_id,
            x,
            y,
            dx,
            dy,
            resolved_source,
            v120,
            stop,
            time,
            match &toplevel {
                StageToplevel::Wayland(_) => "wayland",
                StageToplevel::X11(_) => "x11",
            }
        );
        let mut frame = AxisFrame::new(time).source(resolved_source);

        if dx.abs() > f64::EPSILON {
            frame = frame.value(Axis::Horizontal, dx);
        }
        if dy.abs() > f64::EPSILON {
            frame = frame.value(Axis::Vertical, dy);
        }

        if let Some((vx, vy)) = v120 {
            if vx != 0 {
                frame = frame.v120(Axis::Horizontal, vx);
            }
            if vy != 0 {
                frame = frame.v120(Axis::Vertical, vy);
            }
        }

        if stop.0 {
            frame = frame.stop(Axis::Horizontal);
        }
        if stop.1 {
            frame = frame.stop(Axis::Vertical);
        }

        match toplevel {
            StageToplevel::Wayland(tl) => {
                let (target, local) = self.hit_test_wayland_surface(tl.wl_surface(), x, y);
                let same = self
                    .stages
                    .get(stage_id)
                    .and_then(|s| s.input.current_pointer_target.as_ref())
                    .is_some_and(|cur| cur.same_target(&StageInputTarget::Wayland(target.clone())));
                if !same {
                    self.set_pointer_target(
                        stage_id,
                        StageInputTarget::Wayland(target.clone()),
                        local,
                        SERIAL_COUNTER.next_serial(),
                        time,
                    );
                }
                PointerTarget::axis(&target, &seat, self, frame);
                PointerTarget::frame(&target, &seat, self);
            }
            StageToplevel::X11(x11) => {
                let geo = x11.geometry();
                let mut hit_x11 = x11;
                if let Some(xwm_id) = self.stages.get(stage_id).and_then(|s| s.xwm_id) {
                    for ovr in self.x11_override_windows_for(xwm_id).into_iter().rev() {
                        let ogeo = ovr.geometry();
                        let rx = (ogeo.loc.x - geo.loc.x) as f64;
                        let ry = (ogeo.loc.y - geo.loc.y) as f64;
                        if x >= rx
                            && x < rx + ogeo.size.w.max(1) as f64
                            && y >= ry
                            && y < ry + ogeo.size.h.max(1) as f64
                        {
                            hit_x11 = ovr;
                            break;
                        }
                    }
                }
                PointerTarget::axis(&hit_x11, &seat, self, frame);
                PointerTarget::frame(&hit_x11, &seat, self);
            }
        }
    }

    pub fn send_stage_evdev_key(&mut self, stage_id: &str, evdev_code: u32, pressed: bool) {
        let time = self.now_ms();
        self.send_stage_evdev_key_at(stage_id, evdev_code, pressed, time);
    }

    pub fn send_stage_evdev_key_at(
        &mut self,
        stage_id: &str,
        evdev_code: u32,
        pressed: bool,
        time: u32,
    ) {
        self.ensure_virtual_focus(stage_id);
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.active_toplevel().cloned()) else {
            return;
        };

        let (_xkb_code, mods_state, prev_mods) = {
            let Some(stage) = self.stages.get_mut(stage_id) else {
                return;
            };
            let prev = stage.input.mods_state;
            let (code, mods) = stage.input.update_key(evdev_code, pressed);
            (code, mods, prev)
        };

        let seat = self.seat.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let wl_key_state = if pressed {
            WlKeyState::Pressed
        } else {
            WlKeyState::Released
        };

        // Route keyboard to active popup if any (e.g. menu navigation), else toplevel
        match toplevel {
            StageToplevel::Wayland(tl) => {
                let wl_surf = tl.wl_surface();
                let kbd_target = PopupManager::popups_for_surface(wl_surf)
                    .next()
                    .map(|(p, _)| p.wl_surface().clone())
                    .unwrap_or_else(|| wl_surf.clone());

                self.set_keyboard_target(
                    stage_id,
                    StageInputTarget::Wayland(kbd_target.clone()),
                    prev_mods,
                    serial,
                );

                if let (Some(client), Some(kbd_handle)) = (kbd_target.client(), seat.get_keyboard()) {
                    for kbd in kbd_handle.client_keyboards(&client) {
                        kbd.key(serial.into(), time, evdev_code, wl_key_state);
                    }
                }
                if mods_state != prev_mods {
                    KeyboardTarget::modifiers(&kbd_target, &seat, self, mods_state, serial);
                }
            }
            StageToplevel::X11(x11) => {
                if let (Some(wl_surf), Some(kbd_handle)) = (x11.wl_surface(), seat.get_keyboard()) {
                    if let Some(client) = wl_surf.client() {
                        for kbd in kbd_handle.client_keyboards(&client) {
                            kbd.key(serial.into(), time, evdev_code, wl_key_state);
                        }
                    }
                }
                if mods_state != prev_mods {
                    KeyboardTarget::modifiers(&x11, &seat, self, mods_state, serial);
                }
            }
        }
    }

    pub fn send_stage_key_named(&mut self, stage_id: &str, key_name: &str, modifiers: &[String], mode: &str) -> Result<(), String> {
        let evdev_code = key_name_to_evdev(key_name)
            .ok_or_else(|| format!("Unknown key name: '{}'", key_name))?;
        let mod_codes: Vec<u32> = modifiers
            .iter()
            .filter_map(|m| mod_name_to_evdev(m))
            .collect();

        match mode {
            "press" => {
                for &mc in &mod_codes {
                    self.send_stage_evdev_key(stage_id, mc, true);
                }
                self.send_stage_evdev_key(stage_id, evdev_code, true);
            }
            "release" => {
                self.send_stage_evdev_key(stage_id, evdev_code, false);
                for &mc in mod_codes.iter().rev() {
                    self.send_stage_evdev_key(stage_id, mc, false);
                }
            }
            _ => {
                for &mc in &mod_codes {
                    self.send_stage_evdev_key(stage_id, mc, true);
                }
                self.send_stage_evdev_key(stage_id, evdev_code, true);
                self.send_stage_evdev_key(stage_id, evdev_code, false);
                for &mc in mod_codes.iter().rev() {
                    self.send_stage_evdev_key(stage_id, mc, false);
                }
            }
        }
        Ok(())
    }

    pub fn send_stage_type_text(&mut self, stage_id: &str, text: &str) -> Result<(), String> {
        self.ensure_virtual_focus(stage_id);
        let (wl_clients, is_x11, xwm_id) = {
            let stage = self
                .stages
                .get(stage_id)
                .ok_or_else(|| format!("Stage '{}' not found", stage_id))?;
            (
                stage.wayland_clients.keys().cloned().collect::<Vec<_>>(),
                stage.kind == crate::protocol::StageKind::X11,
                stage.xwm_id,
            )
        };

        // 1. Prefer zwp_text_input_v3 if an enabled instance exists for this Stage's client.
        if !is_x11 {
            let mut committed_via_ti = false;
            for ti in &self.text_input_state.instances {
                if ti.enabled && wl_clients.contains(&ti.client_id) {
                    ti.resource.commit_string(Some(text.to_string()));
                    ti.resource.done(ti.serial);
                    committed_via_ti = true;
                }
            }
            if committed_via_ti {
                debug!("Stage {} typed {} chars via zwp_text_input_v3", stage_id, text.len());
                return Ok(());
            }
        }

        // 2. Fallback: type ASCII via standard US evdev keycodes, and non-ASCII via temporary keysym mapping.
        for ch in text.chars() {
            if let Some((evdev_code, shift)) = us_char_to_evdev(ch) {
                if shift {
                    self.send_stage_evdev_key(stage_id, 42, true); // KEY_LEFTSHIFT
                }
                self.send_stage_evdev_key(stage_id, evdev_code, true);
                self.send_stage_evdev_key(stage_id, evdev_code, false);
                if shift {
                    self.send_stage_evdev_key(stage_id, 42, false);
                }
            } else {
                self.send_unicode_char_via_keymap(stage_id, ch, xwm_id);
            }
        }
        Ok(())
    }

    fn send_unicode_char_via_keymap(
        &mut self,
        stage_id: &str,
        ch: char,
        xwm_id: Option<smithay::xwayland::xwm::XwmId>,
    ) {
        let keysym_val = if (ch as u32) >= 0x20 {
            0x0100_0000 + (ch as u32)
        } else {
            return;
        };

        // Wayland temporary keymap upload on a single unused evdev keycode (200 -> XKB <I208>)
        if let Some(stage) = self.stages.get(stage_id) {
            let base_str = stage
                .input
                .xkb_keymap
                .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            // Replace key <FK13> or <I208> symbol in the XKB keymap string if present, or append a custom symbol
            let sym_name = format!("U{:04X}", ch as u32);
            let patched = inject_keysym_into_xkb(&base_str, &sym_name);
            if let Some(temp_km) = xkb::Keymap::new_from_string(
                &stage.input.xkb_context,
                patched,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) {
                let km_file = KeymapFile::new(&temp_km);
                self.send_keymap_to_stage_clients(stage_id, &km_file);
                // Keycode 191 = <FK13> (evdev 183 + 8 = 191)
                self.send_stage_evdev_key(stage_id, 183, true);
                self.send_stage_evdev_key(stage_id, 183, false);

                // Restore standard keymap
                if let Some(stage) = self.stages.get(stage_id) {
                    let orig_file = KeymapFile::new(&stage.input.xkb_keymap);
                    self.send_keymap_to_stage_clients(stage_id, &orig_file);
                }
                return;
            }
        }

        // Fallback for X11 if needed (uses x11rb change_keyboard_mapping if available)
        if let Some(id) = xwm_id {
            let _ = (id, keysym_val);
            warn!("Unicode character '{}' fallback skipped on X11 without XKB keymap", ch);
        }
    }

    fn send_keymap_to_stage_clients(&self, stage_id: &str, keymap_file: &KeymapFile) {
        let Some(stage) = self.stages.get(stage_id) else {
            return;
        };
        let Some(kbd_handle) = self.seat.get_keyboard() else {
            return;
        };
        for client in stage.wayland_clients.values() {
            for kbd in kbd_handle.client_keyboards(client) {
                let _ = keymap_file.send(&kbd);
            }
        }
        if let Some(ref client) = stage.xwayland_client {
            for kbd in kbd_handle.client_keyboards(client) {
                let _ = keymap_file.send(&kbd);
            }
        }
    }

    pub fn execute_single_action(&mut self, stage_id: &str, action: &Action) -> Result<Option<Duration>, String> {
        match action {
            Action::Move { x, y } => {
                self.send_stage_pointer_motion(stage_id, *x, *y);
                Ok(None)
            }
            Action::Click { x, y, button } => {
                let btn = mouse_button_code(button.as_deref().unwrap_or("left"));
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                Ok(None)
            }
            Action::DoubleClick { x, y, button } => {
                let btn = mouse_button_code(button.as_deref().unwrap_or("left"));
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                Ok(None)
            }
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                steps,
            } => {
                let btn = mouse_button_code(button.as_deref().unwrap_or("left"));
                let n = steps.unwrap_or(8).max(1);
                self.send_stage_button(stage_id, *from_x, *from_y, btn, true);
                for i in 1..=n {
                    let t = (i as f64) / (n as f64);
                    let ix = from_x + (to_x - from_x) * t;
                    let iy = from_y + (to_y - from_y) * t;
                    self.send_stage_pointer_motion(stage_id, ix, iy);
                }
                self.send_stage_button(stage_id, *to_x, *to_y, btn, false);
                Ok(None)
            }
            Action::Scroll { x, y, dx, dy } => {
                self.send_stage_scroll(stage_id, *x, *y, *dx, *dy);
                Ok(None)
            }
            Action::Key {
                key,
                modifiers,
                state,
            } => {
                let mods = modifiers.as_deref().unwrap_or(&[]);
                let mode = state.as_deref().unwrap_or("tap");
                self.send_stage_key_named(stage_id, key, mods, mode)?;
                Ok(None)
            }
            Action::TypeText { text } => {
                self.send_stage_type_text(stage_id, text)?;
                Ok(None)
            }
            Action::Wait { ms } => Ok(Some(Duration::from_millis(*ms))),
        }
    }

    pub fn tick_pending_act_jobs(&mut self) {
        let now = Instant::now();
        let jobs = std::mem::take(&mut self.pending_act_jobs);
        let mut still_pending = Vec::new();

        for mut job in jobs {
            if !self.is_input_allowed(&job.stage_id, InputSource::Agent, Some(&job.lease_id)) {
                let frame_seq = self.stages.get(&job.stage_id).map(|s| s.frame_seq).unwrap_or(0);
                let _ = job.reply_tx.send(crate::protocol::ControlResponse::ActResult {
                    act_status: ActStatus::Preempted,
                    executed_steps: job.executed_steps,
                    frame_seq,
                    message: Some("Lease expired or preempted".to_string()),
                });
                continue;
            }

            if let Some(ref mut st) = job.settling {
                let cur_seq = self.stages.get(&job.stage_id).map(|s| s.frame_seq).unwrap_or(0);
                if cur_seq != st.last_seq {
                    st.last_seq = cur_seq;
                    st.quiet_since = now;
                }
                let settled = now >= st.deadline
                    || (st.last_seq != st.initial_seq
                        && now.duration_since(st.quiet_since) >= Duration::from_millis(180))
                    || (st.last_seq == st.initial_seq
                        && now.duration_since(st.started_at) >= Duration::from_millis(220));
                if settled {
                    let _ = job.reply_tx.send(crate::protocol::ControlResponse::ActResult {
                        act_status: ActStatus::Ok,
                        executed_steps: job.executed_steps,
                        frame_seq: cur_seq,
                        message: None,
                    });
                } else {
                    still_pending.push(job);
                }
                continue;
            }

            if let Some(until) = job.wait_until {
                if now < until {
                    still_pending.push(job);
                    continue;
                }
                job.wait_until = None;
            }

            let mut paused_for_wait = false;
            let mut failed_msg: Option<String> = None;

            while let Some(action) = job.remaining.pop_front() {
                match self.execute_single_action(&job.stage_id, &action) {
                    Ok(Some(dur)) => {
                        job.executed_steps += 1;
                        job.wait_until = Some(Instant::now() + dur);
                        paused_for_wait = true;
                        break;
                    }
                    Ok(None) => {
                        job.executed_steps += 1;
                    }
                    Err(e) => {
                        failed_msg = Some(e);
                        break;
                    }
                }
            }

            if paused_for_wait {
                still_pending.push(job);
            } else if failed_msg.is_none() && job.executed_steps > 0 {
                let cur_seq = self.stages.get(&job.stage_id).map(|s| s.frame_seq).unwrap_or(0);
                let t = Instant::now();
                job.settling = Some(SettleState {
                    initial_seq: cur_seq,
                    last_seq: cur_seq,
                    quiet_since: t,
                    started_at: t,
                    deadline: t + Duration::from_millis(1200),
                });
                still_pending.push(job);
            } else {
                let frame_seq = self.stages.get(&job.stage_id).map(|s| s.frame_seq).unwrap_or(0);
                let act_status = if failed_msg.is_some() {
                    ActStatus::Error
                } else {
                    ActStatus::Ok
                };
                let _ = job.reply_tx.send(crate::protocol::ControlResponse::ActResult {
                    act_status,
                    executed_steps: job.executed_steps,
                    frame_seq,
                    message: failed_msg,
                });
            }
        }

        self.pending_act_jobs = still_pending;
    }
}

fn inject_keysym_into_xkb(keymap_str: &str, keysym_name: &str) -> String {
    if let Some(idx) = keymap_str.find("key <FK13>") {
        if let Some(end) = keymap_str[idx..].find("};") {
            let mut out = String::with_capacity(keymap_str.len() + 64);
            out.push_str(&keymap_str[..idx]);
            out.push_str(&format!("key <FK13> {{ [ {} ] }};", keysym_name));
            out.push_str(&keymap_str[idx + end + 2..]);
            return out;
        }
    }
    keymap_str.to_string()
}

pub fn mouse_button_code(name: &str) -> u32 {
    match name.to_ascii_lowercase().as_str() {
        "right" | "r" => 0x111,  // BTN_RIGHT
        "middle" | "m" => 0x112, // BTN_MIDDLE
        _ => 0x110,              // BTN_LEFT
    }
}

fn mod_name_to_evdev(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "control_l" => Some(29),
        "ctrl_r" | "control_r" => Some(97),
        "shift" | "shift_l" => Some(42),
        "shift_r" => Some(54),
        "alt" | "alt_l" => Some(56),
        "alt_r" => Some(100),
        "super" | "meta" | "win" | "logo" | "super_l" => Some(125),
        _ => None,
    }
}

pub fn key_name_to_evdev(name: &str) -> Option<u32> {
    if name.chars().count() == 1 {
        let ch = name.chars().next().unwrap();
        if let Some((code, _)) = us_char_to_evdev(ch.to_ascii_lowercase()) {
            return Some(code);
        }
    }
    match name.to_ascii_lowercase().as_str() {
        "escape" | "esc" => Some(1),
        "backspace" => Some(14),
        "tab" => Some(15),
        "return" | "enter" => Some(28),
        "space" => Some(57),
        "capslock" | "caps_lock" => Some(58),
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(87),
        "f12" => Some(88),
        "home" => Some(102),
        "up" => Some(103),
        "page_up" | "pageup" | "prior" => Some(104),
        "left" => Some(105),
        "right" => Some(106),
        "end" => Some(107),
        "down" => Some(108),
        "page_down" | "pagedown" | "next" => Some(109),
        "insert" => Some(110),
        "delete" | "del" => Some(111),
        "control_l" | "ctrl" | "control" => Some(29),
        "control_r" => Some(97),
        "shift_l" | "shift" => Some(42),
        "shift_r" => Some(54),
        "alt_l" | "alt" => Some(56),
        "alt_r" => Some(100),
        "super_l" | "super" | "meta" | "win" => Some(125),
        _ => None,
    }
}

/// Maps ASCII characters to `(evdev_keycode, needs_shift)` for a standard US QWERTY layout.
pub fn us_char_to_evdev(ch: char) -> Option<(u32, bool)> {
    Some(match ch {
        '1' => (2, false),
        '!' => (2, true),
        '2' => (3, false),
        '@' => (3, true),
        '3' => (4, false),
        '#' => (4, true),
        '4' => (5, false),
        '$' => (5, true),
        '5' => (6, false),
        '%' => (6, true),
        '6' => (7, false),
        '^' => (7, true),
        '7' => (8, false),
        '&' => (8, true),
        '8' => (9, false),
        '*' => (9, true),
        '9' => (10, false),
        '(' => (10, true),
        '0' => (11, false),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '\t' => (15, false),
        'q' => (16, false),
        'Q' => (16, true),
        'w' => (17, false),
        'W' => (17, true),
        'e' => (18, false),
        'E' => (18, true),
        'r' => (19, false),
        'R' => (19, true),
        't' => (20, false),
        'T' => (20, true),
        'y' => (21, false),
        'Y' => (21, true),
        'u' => (22, false),
        'U' => (22, true),
        'i' => (23, false),
        'I' => (23, true),
        'o' => (24, false),
        'O' => (24, true),
        'p' => (25, false),
        'P' => (25, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\n' | '\r' => (28, false),
        'a' => (30, false),
        'A' => (30, true),
        's' => (31, false),
        'S' => (31, true),
        'd' => (32, false),
        'D' => (32, true),
        'f' => (33, false),
        'F' => (33, true),
        'g' => (34, false),
        'G' => (34, true),
        'h' => (35, false),
        'H' => (35, true),
        'j' => (36, false),
        'J' => (36, true),
        'k' => (37, false),
        'K' => (37, true),
        'l' => (38, false),
        'L' => (38, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        'z' => (44, false),
        'Z' => (44, true),
        'x' => (45, false),
        'X' => (45, true),
        'c' => (46, false),
        'C' => (46, true),
        'v' => (47, false),
        'V' => (47, true),
        'b' => (48, false),
        'B' => (48, true),
        'n' => (49, false),
        'N' => (49, true),
        'm' => (50, false),
        'M' => (50, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        ' ' => (57, false),
        _ => return None,
    })
}
