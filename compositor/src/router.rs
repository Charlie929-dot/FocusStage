use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::desktop::{utils::under_from_surface_tree, PopupManager, WindowSurfaceType};
use smithay::input::keyboard::{xkb, KeyboardTarget, KeymapFile, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerTarget};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::{wl_keyboard::KeyState as WlKeyState, wl_surface::WlSurface},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER};
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
            // Clients may share a Stage, but text input can only enter the requesting client's focus.
            let target = state
                .stage_id_for_client(&cid)
                .and_then(|sid| state.stages.get(&sid))
                .and_then(|stage| stage.input.current_keyboard_target.as_ref());
            if let Some(StageInputTarget::Wayland(surface)) = target {
                if surface.is_alive() && surface.id().same_client_as(&res.id()) {
                    res.enter(surface);
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
    initial_seq: u64,
    last_seq: u64,
    quiet_since: Instant,
    started_at: Instant,
    deadline: Instant,
}

impl SettleState {
    pub fn timeout(settle: Option<bool>, settle_ms: Option<u64>) -> Option<Duration> {
        const DEFAULT_MS: u64 = 600;
        // The control connection has its own deadline. Leave time for dispatch and reply.
        const MAX_MS: u64 = 10_000;
        let ms = settle_ms.unwrap_or(DEFAULT_MS).min(MAX_MS);
        (settle.unwrap_or(true) && ms > 0).then(|| Duration::from_millis(ms))
    }

    pub fn new(settle_seq: u64, now: Instant, timeout: Duration) -> Self {
        Self {
            initial_seq: settle_seq,
            last_seq: settle_seq,
            quiet_since: now,
            started_at: now,
            deadline: now + timeout,
        }
    }

    fn is_settled(&mut self, settle_seq: u64, now: Instant) -> bool {
        if settle_seq != self.last_seq {
            self.last_seq = settle_seq;
            self.quiet_since = now;
        }
        now >= self.deadline
            || (self.last_seq != self.initial_seq
                && now.duration_since(self.quiet_since) >= Duration::from_millis(180))
            || (self.last_seq == self.initial_seq
                && now.duration_since(self.started_at) >= Duration::from_millis(220))
    }
}

#[cfg(test)]
mod settle_tests {
    use super::*;

    #[test]
    fn settled_when_no_frames_arrive() {
        let start = Instant::now();
        let mut settle = SettleState::new(5, start, SettleState::timeout(None, None).unwrap());
        assert!(!settle.is_settled(5, start + Duration::from_millis(219)));
        assert!(settle.is_settled(5, start + Duration::from_millis(220)));
    }

    #[test]
    fn a_new_frame_restarts_the_quiet_period() {
        let start = Instant::now();
        let mut settle = SettleState::new(5, start, Duration::from_secs(1));
        assert!(!settle.is_settled(6, start + Duration::from_millis(50)));
        assert!(!settle.is_settled(7, start + Duration::from_millis(229)));
        assert!(!settle.is_settled(7, start + Duration::from_millis(408)));
        assert!(settle.is_settled(7, start + Duration::from_millis(409)));
    }

    #[test]
    fn timeout_bounds_continuous_updates_and_can_be_skipped() {
        assert_eq!(SettleState::timeout(Some(false), Some(350)), None);
        assert_eq!(SettleState::timeout(None, Some(0)), None);
        let timeout = SettleState::timeout(Some(true), Some(350)).unwrap();
        let start = Instant::now();
        let mut settle = SettleState::new(5, start, timeout);
        for elapsed in (50..350).step_by(50) {
            assert!(!settle.is_settled(5 + elapsed, start + Duration::from_millis(elapsed)));
        }
        assert!(settle.is_settled(12, start + Duration::from_millis(350)));
    }
}

pub struct PendingActJob {
    pub stage_id: String,
    pub lease_id: String,
    pub remaining: VecDeque<Action>,
    pub executed_steps: usize,
    pub wait_until: Option<Instant>,
    pub settle_timeout: Option<Duration>,
    pub settling: Option<SettleState>,
    pub reply_tx: std::sync::mpsc::Sender<crate::protocol::ControlResponse>,
}

impl FocusStageState {
    pub fn now_ms(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
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
                    self.clear_cursor_focus(stage_id);
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

        self.enter_cursor_focus(stage_id, serial, &new_target);
        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.x11_axis_primed = false;
            stage.input.current_pointer_target = Some(new_target);
        }
        true
    }

    pub fn leave_window_pointer(&mut self, stage_id: &str, window_id: &str) {
        let Some(stage) = self.stages.get_mut(stage_id) else { return; };
        if stage.input.pointer_window_id.as_deref() != Some(window_id) || !stage.input.pressed_buttons.is_empty() {
            return;
        }
        stage.input.pointer_window_id = None;
        stage.input.pointer_press = None;
        self.clear_stage_pointer_target(stage_id);
    }

    pub(crate) fn clear_stage_pointer_target(&mut self, stage_id: &str) {
        self.clear_cursor_focus(stage_id);
        let input = &mut self.stages.get_mut(stage_id).unwrap().input;
        input.x11_axis_primed = false;
        let Some(target) = input.current_pointer_target.take().filter(|t| t.alive()) else { return; };
        let seat = self.seat.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.now_ms();
        match target {
            StageInputTarget::Wayland(surface) => {
                PointerTarget::leave(&surface, &seat, self, serial, time);
                PointerTarget::frame(&surface, &seat, self);
            }
            StageInputTarget::X11(surface) => {
                PointerTarget::leave(&surface, &seat, self, serial, time);
                PointerTarget::frame(&surface, &seat, self);
            }
        }
    }

    pub(crate) fn clear_window_inputs(&mut self, stage_id: &str, window_id: &str) {
        self.dismiss_window_popups(stage_id, window_id);
        self.clear_window_cursor_focus(stage_id, window_id);
        if self.stages[stage_id].input.drag.as_ref().is_some_and(|d| d.window_id == window_id) {
            self.cancel_stage_drag(stage_id);
        }
        let stage = self.stages.get_mut(stage_id).unwrap();
        let focused = stage.active_window().is_some_and(|w| w.id == window_id)
            || (stage.active_window_id.as_deref() == Some(window_id)
                && stage.window(window_id).is_some_and(|w| w.mapped));
        if focused {
            let keys: Vec<_> = stage.input.pressed_keys.iter().copied().collect();
            for key in keys {
                stage.input.update_key(key, false);
            }
        }
        if stage.input.pointer_window_id.as_deref() == Some(window_id) {
            stage.input.pressed_buttons.clear();
            self.leave_window_pointer(stage_id, window_id);
        }
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
        let virtual_focus = stage.uses_virtual_focus();
        let x11_virtual_activation = virtual_focus || self.display_sink.is_none();
        let mods_state = stage.input.mods_state;
        let pointer_pos = stage.input.pointer_pos;
        let cur_kbd = stage.input.current_keyboard_target.clone();
        let cur_ptr = stage.input.current_pointer_target.clone();
        let time = self.now_ms();

        match toplevel {
            StageToplevel::Wayland(ref wl_tl) => {
                if virtual_focus {
                    wl_tl.with_pending_state(|pending| {
                        pending.states.set(ToplevelState::Activated);
                    });
                    wl_tl.send_pending_configure();
                }
                let wl_surf = wl_tl.wl_surface().clone();
                let keyboard = self.stages[stage_id].input.popups.top_for_root(&wl_surf).unwrap_or(&wl_surf).clone();
                let kbd_valid = cur_kbd.as_ref().is_some_and(|target| target.alive() && target.wl_surface().as_ref() == Some(&keyboard));
                if !kbd_valid {
                    let serial = SERIAL_COUNTER.next_serial();
                    self.set_keyboard_target(
                        stage_id,
                        StageInputTarget::Wayland(keyboard),
                        mods_state,
                        serial,
                    );
                }

                if virtual_focus && self.stages[stage_id].input.drag.is_none() {
                    let ptr_valid = cur_ptr.as_ref().is_some_and(|t| {
                        t.alive() && matches!(t, StageInputTarget::Wayland(s)
                            if self.window_for_surface(s).is_some_and(|w| w.toplevel.wl_surface().as_ref() == Some(&wl_surf)))
                    });
                    if !ptr_valid {
                        let (target, origin) = self.hit_test_wayland_surface(&wl_surf, pointer_pos);
                        self.set_wayland_pointer_target(stage_id, target, origin, pointer_pos, SERIAL_COUNTER.next_serial(), time);
                    }
                }
            }
            StageToplevel::X11(ref x11_tl) => {
                let kbd_valid = cur_kbd.as_ref().is_some_and(|t| {
                    t.alive() && t.same_target(&StageInputTarget::X11(x11_tl.clone()))
                });
                if x11_virtual_activation || !kbd_valid {
                    let _ = x11_tl.set_activated(true);
                }
                if x11_tl.wl_surface().is_some() {
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
                    if virtual_focus && !ptr_valid {
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
            self.end_stage_lease(id, crate::control::LeaseEnd::Preempted);
            self.set_stage_display(id, DisplayState::Desktop);
            self.sync_stage_agent_overlay(id);
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

    pub(crate) fn release_all_stage_inputs(&mut self, stage_id: &str) {
        self.cancel_stage_drag(stage_id);
        let (pressed_keys, pressed_buttons, pos, pointer_window_id) = match self.stages.get(stage_id) {
            Some(s) => (
                s.input.pressed_keys.iter().copied().collect::<Vec<_>>(),
                s.input.pressed_buttons.iter().copied().collect::<Vec<_>>(),
                s.input.pointer_pos,
                s.input.pointer_window_id.clone(),
            ),
            None => return,
        };
        for kc in pressed_keys {
            self.send_stage_evdev_key(stage_id, kc, false);
        }
        let time = self.now_ms();
        for btn in pressed_buttons {
            self.send_window_button_at(stage_id, pointer_window_id.as_deref(), pos.x, pos.y, btn, false, time);
        }
        let input = &mut self.stages.get_mut(stage_id).unwrap().input;
        input.pressed_buttons.clear();
        input.pointer_press = None;
        input.popups.clear_serials();
    }

    /// Return the surface and its origin in the same coordinates used for rendering.
    pub(crate) fn hit_test_wayland_surface(&self, root: &WlSurface, point: Point<f64, Logical>) -> (WlSurface, Point<f64, Logical>) {
        // PopupManager yields topmost child popups before their parents.
        for (popup, popup_loc) in PopupManager::popups_for_surface(root) {
            let origin = popup_loc - popup.geometry().loc;
            if let Some((surface, origin)) = under_from_surface_tree(popup.wl_surface(), point, origin, WindowSurfaceType::ALL) {
                return (surface, origin.to_f64());
            }
        }
        let geo_loc = wayland_window_geometry_loc(root);
        let origin = Point::from((-geo_loc.x, -geo_loc.y));
        under_from_surface_tree(root, point, origin, WindowSurfaceType::ALL)
            .map(|(surface, origin)| (surface, origin.to_f64()))
            .unwrap_or_else(|| (root.clone(), origin.to_f64()))
    }

    fn set_wayland_pointer_target(
        &mut self, stage_id: &str, target: WlSurface, origin: Point<f64, Logical>,
        point: Point<f64, Logical>, serial: Serial, time: u32,
    ) -> bool {
        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.input.pointer_surface_origin = origin;
        }
        self.set_pointer_target(stage_id, StageInputTarget::Wayland(target), point - origin, serial, time)
    }

    pub fn send_stage_pointer_motion(&mut self, stage_id: &str, x: f64, y: f64) {
        let time = self.now_ms();
        self.send_stage_pointer_motion_at(stage_id, x, y, time);
    }

    pub fn send_stage_pointer_motion_at(&mut self, stage_id: &str, x: f64, y: f64, time: u32) {
        self.send_window_pointer_motion_at(stage_id, None, x, y, time);
    }

    pub fn send_window_pointer_motion_at(&mut self, stage_id: &str, window_id: Option<&str>, x: f64, y: f64, time: u32) {
        if window_id.is_none() {
            self.ensure_virtual_focus(stage_id);
        }
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.input_toplevel(window_id)).cloned() else {
            return;
        };
        let point = Point::from((x, y));
        if self.route_drag_motion(stage_id, window_id, point, time) {
            let input = &mut self.stages.get_mut(stage_id).unwrap().input;
            input.pointer_pos = point;
            input.pointer_window_id = window_id.map(str::to_owned);
            return;
        }
        let input = &mut self.stages.get_mut(stage_id).unwrap().input;
        let has_popup_grab = input.popups.top().is_some();
        let grab = match &input.current_pointer_target {
            Some(target @ StageInputTarget::Wayland(surface)) if target.alive()
                && !input.pressed_buttons.is_empty()
                && input.pointer_window_id.as_deref() == window_id
                && (!has_popup_grab || input.popups.contains_surface(&crate::popup::surface_root(surface))) =>
            {
                Some((surface.clone(), input.pointer_surface_origin))
            }
            _ => None,
        };
        input.pointer_pos = point;
        input.pointer_window_id = window_id.map(str::to_owned);
        let seat = self.seat.clone();
        let serial = SERIAL_COUNTER.next_serial();

        match toplevel {
            StageToplevel::Wayland(tl) => {
                let (target, origin) = grab.unwrap_or_else(|| self.hit_test_wayland_surface(tl.wl_surface(), point));
                let entered = self.set_wayland_pointer_target(stage_id, target.clone(), origin, point, serial, time);
                if !entered {
                    let ev = MotionEvent {
                        location: point - origin,
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
                if x11.xwm_id().is_some() {
                    for ovr in self.x11_override_windows_for(&StageToplevel::X11(x11.clone())).into_iter().rev() {
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
                    if let Some(stage) = self.stages.get_mut(stage_id) {
                        stage.input.x11_axis_primed = false;
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
        self.send_window_button_at(stage_id, None, x, y, button, pressed, time);
    }

    pub fn send_window_button_at(
        &mut self, stage_id: &str, window_id: Option<&str>, x: f64, y: f64, button: u32, pressed: bool, time: u32,
    ) {
        let had_drag = self.stages.get(stage_id).is_some_and(|stage| stage.input.drag.is_some());
        if !had_drag && self.consume_popup_button(stage_id, window_id, (x, y).into(), button, pressed) {
            self.send_window_pointer_motion_at(stage_id, window_id, x, y, time);
            return;
        }
        self.send_window_pointer_motion_at(stage_id, window_id, x, y, time);
        let Some(stage) = self.stages.get_mut(stage_id) else { return; };
        let Some(toplevel) = stage.input_toplevel(window_id).cloned() else { return; };
        let first_press = pressed && stage.input.pressed_buttons.is_empty();
        if pressed {
            stage.input.pressed_buttons.insert(button);
        } else {
            stage.input.pressed_buttons.remove(&button);
            if stage.input.pressed_buttons.is_empty() {
                stage.input.pointer_press = None;
            }
        }
        if had_drag {
            if stage.input.pressed_buttons.is_empty() {
                let point = Point::from((x, y));
                let window = match window_id {
                    Some(id) => stage.window(id),
                    None => stage.active_window(),
                };
                let over_window = window.is_some_and(|w| Rectangle::from_size(w.logical_size.to_f64()).contains(point));
                self.finish_stage_drag(stage_id);
                if over_window {
                    self.send_window_pointer_motion_at(stage_id, window_id, x, y, time);
                }
            }
            return;
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
                let input = &mut self.stages.get_mut(stage_id).unwrap().input;
                let Some(StageInputTarget::Wayland(target)) = input.current_pointer_target.clone() else { return; };
                if first_press {
                    input.pointer_press = Some((serial, target.clone()));
                }
                input.popups.record_input(serial, &target, pressed);
                let grab_ended = !pressed && input.pressed_buttons.is_empty();
                PointerTarget::button(&target, &seat, self, &ev);
                PointerTarget::frame(&target, &seat, self);
                if grab_ended {
                    let point = Point::from((x, y));
                    let (target, origin) = self.hit_test_wayland_surface(tl.wl_surface(), point);
                    self.set_wayland_pointer_target(stage_id, target, origin, point, SERIAL_COUNTER.next_serial(), time);
                }
            }
            StageToplevel::X11(x11) => {
                let geo = x11.geometry();
                let mut hit_x11 = x11;
                if hit_x11.xwm_id().is_some() {
                    for ovr in self.x11_override_windows_for(&StageToplevel::X11(hit_x11.clone())).into_iter().rev() {
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
                if let Some(stage) = self.stages.get_mut(stage_id) {
                    stage.input.x11_axis_primed = false;
                }
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
        self.send_window_scroll_frame(stage_id, None, x, y, dx, dy, source, v120, stop, time);
    }

    pub fn send_window_scroll_frame(
        &mut self, stage_id: &str, window_id: Option<&str>, x: f64, y: f64, dx: f64, dy: f64,
        source: Option<AxisSource>, v120: Option<(i32, i32)>, stop: (bool, bool), time: Option<u32>,
    ) {
        let time = time.unwrap_or_else(|| self.now_ms());
        let need_motion = self.stages.get(stage_id).is_none_or(|s| {
            s.input.current_pointer_target.as_ref().is_none_or(|t| !t.alive())
                || (s.input.pointer_pos.x - x).abs() > 0.01
                || (s.input.pointer_pos.y - y).abs() > 0.01
                || s.input.pointer_window_id.as_deref() != window_id
        });
        if need_motion {
            self.send_window_pointer_motion_at(stage_id, window_id, x, y, time);
        } else if window_id.is_none() {
            self.ensure_virtual_focus(stage_id);
        }
        let Some(toplevel) = self.stages.get(stage_id).and_then(|s| s.input_toplevel(window_id)).cloned() else {
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
            StageToplevel::Wayland(_) => {
                if let Some(StageInputTarget::Wayland(target)) = self.stages[stage_id].input.current_pointer_target.clone() {
                    PointerTarget::axis(&target, &seat, self, frame);
                    PointerTarget::frame(&target, &seat, self);
                }
            }
            StageToplevel::X11(x11) => {
                let geo = x11.geometry();
                let mut hit_x11 = x11;
                if hit_x11.xwm_id().is_some() {
                    for ovr in self.x11_override_windows_for(&StageToplevel::X11(hit_x11.clone())).into_iter().rev() {
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
                let has_scroll = dx.abs() > f64::EPSILON
                    || dy.abs() > f64::EPSILON
                    || v120.is_some_and(|(vx, vy)| vx != 0 || vy != 0);
                // XWayland routes pointer motion via xwayland-pointer and axis via xwayland-relative-pointer.
                // Switching active slave devices emits XI_DeviceChanged, which causes Chromium's DeviceDataManagerX11
                // to reset scroll valuator baselines and swallow the first wheel event. Send a zero-delta axis_stop
                // frame first so the baseline is primed before the real wheel delta arrives.
                if has_scroll
                    && hit_x11.wl_surface().is_some()
                    && self.stages.get(stage_id).is_some_and(|s| !s.input.x11_axis_primed)
                {
                    let prime = AxisFrame::new(time)
                        .source(resolved_source)
                        .stop(Axis::Horizontal)
                        .stop(Axis::Vertical);
                    PointerTarget::axis(&hit_x11, &seat, self, prime);
                    PointerTarget::frame(&hit_x11, &seat, self);
                    if let Some(stage) = self.stages.get_mut(stage_id) {
                        stage.input.x11_axis_primed = true;
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
        if pressed && evdev_code == 1 && self.cancel_stage_drag(stage_id) {
            return;
        }
        self.ensure_virtual_focus(stage_id);
        let toplevel = self.stages.get(stage_id).and_then(|s| s.active_toplevel().cloned());
        if pressed && toplevel.is_none() {
            return;
        }

        let (_xkb_code, mods_state, prev_mods) = {
            let Some(stage) = self.stages.get_mut(stage_id) else {
                return;
            };
            let prev = stage.input.mods_state;
            let (code, mods) = stage.input.update_key(evdev_code, pressed);
            (code, mods, prev)
        };
        let Some(toplevel) = toplevel else {
            return;
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
                let kbd_target = self.stages[stage_id].input.popups.top_for_root(wl_surf).unwrap_or(wl_surf).clone();

                self.set_keyboard_target(
                    stage_id,
                    StageInputTarget::Wayland(kbd_target.clone()),
                    prev_mods,
                    serial,
                );

                if let (Some(client), Some(kbd_handle)) = (kbd_target.client(), seat.get_keyboard()) {
                    let mut sent = false;
                    for kbd in kbd_handle.client_keyboards(&client) {
                        kbd.key(serial.into(), time, evdev_code, wl_key_state);
                        sent = true;
                    }
                    if sent {
                        self.stages.get_mut(stage_id).unwrap().input.popups.record_input(serial, &kbd_target, pressed);
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
            let is_x11 = matches!(stage.active_toplevel(), Some(StageToplevel::X11(_)));
            (
                stage.wayland_clients.keys().cloned().collect::<Vec<_>>(),
                is_x11,
                stage.xwm_id.filter(|_| is_x11),
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
        if (ch as u32) < 0x20 {
            return;
        }

        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };

        if let Some((evdev_code, _)) = stage.input.unicode_slots.iter().copied().find(|(_, mapped)| *mapped == Some(ch)) {
            self.send_stage_evdev_key(stage_id, evdev_code, true);
            self.send_stage_evdev_key(stage_id, evdev_code, false);
            return;
        }

        let slot_idx = stage.input.unicode_next_slot % stage.input.unicode_slots.len();
        stage.input.unicode_next_slot = (slot_idx + 1) % stage.input.unicode_slots.len();
        let evdev_code = stage.input.unicode_slots[slot_idx].0;
        stage.input.unicode_slots[slot_idx].1 = Some(ch);
        let active_slots: Vec<(u32, char)> = stage
            .input
            .unicode_slots
            .iter()
            .filter_map(|(code, mapped)| mapped.map(|c| (*code, c)))
            .collect();

        let base_str = stage
            .input
            .xkb_keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let patched = inject_unicode_slots_into_xkb(&base_str, &active_slots);
        if let Some(temp_km) = xkb::Keymap::new_from_string(
            &stage.input.xkb_context,
            patched,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            stage.input.custom_keymap_active = true;
            let km_file = KeymapFile::new(&temp_km);
            self.send_keymap_to_stage_clients(stage_id, &km_file);
            // Keep the slot mapped rather than immediately reverting to orig_km in the same flush;
            // XWayland/X11 clients process MappingNotify asynchronously relative to KeyPress.
            self.send_stage_evdev_key(stage_id, evdev_code, true);
            self.send_stage_evdev_key(stage_id, evdev_code, false);
            return;
        }

        if let Some(id) = xwm_id {
            let _ = id;
            warn!("Unicode character '{}' fallback skipped without valid XKB keymap", ch);
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
                self.update_agent_pointer_overlay(stage_id, "move");
                Ok(None)
            }
            Action::Click { x, y, button } => {
                let btn = mouse_button_code(button.as_deref().unwrap_or("left"));
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                self.update_agent_pointer_overlay(stage_id, "click");
                Ok(None)
            }
            Action::DoubleClick { x, y, button } => {
                let btn = mouse_button_code(button.as_deref().unwrap_or("left"));
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                self.update_agent_pointer_overlay(stage_id, "click");
                self.send_stage_button(stage_id, *x, *y, btn, true);
                self.send_stage_button(stage_id, *x, *y, btn, false);
                self.update_agent_pointer_overlay(stage_id, "click");
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
                self.update_agent_pointer_overlay(stage_id, "down");
                for i in 1..=n {
                    let t = (i as f64) / (n as f64);
                    let ix = from_x + (to_x - from_x) * t;
                    let iy = from_y + (to_y - from_y) * t;
                    self.send_stage_pointer_motion(stage_id, ix, iy);
                    self.update_agent_pointer_overlay(stage_id, "drag");
                }
                self.send_stage_button(stage_id, *to_x, *to_y, btn, false);
                self.update_agent_pointer_overlay(stage_id, "move");
                Ok(None)
            }
            Action::Scroll { x, y, dx, dy } => {
                self.send_stage_scroll(stage_id, *x, *y, *dx, *dy);
                self.update_agent_pointer_overlay(stage_id, "scroll");
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
                let response = self.act_lease_failure(&job.lease_id, job.executed_steps, frame_seq);
                let _ = job.reply_tx.send(response);
                continue;
            }

            if let Some(ref mut st) = job.settling {
                let stage = self.stages.get(&job.stage_id);
                if st.is_settled(stage.map_or(0, |s| s.settle_seq), now) {
                    let _ = job.reply_tx.send(crate::protocol::ControlResponse::ActResult {
                        act_status: ActStatus::Ok,
                        executed_steps: job.executed_steps,
                        frame_seq: stage.map_or(0, |s| s.frame_seq),
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
                continue;
            }

            if let Some(timeout) = job.settle_timeout.filter(|_| failed_msg.is_none()) {
                let settle_seq = self.stages.get(&job.stage_id).map_or(0, |s| s.settle_seq);
                job.settling = Some(SettleState::new(settle_seq, Instant::now(), timeout));
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

fn inject_unicode_slots_into_xkb(keymap_str: &str, slots: &[(u32, char)]) -> String {
    let mut out = keymap_str.to_string();
    let mut missing = String::new();
    for &(evdev_code, ch) in slots {
        let xkb_code = evdev_code + 8;
        let key_name = format!("<I{xkb_code}>");
        let needle = format!("key {key_name}");
        let replacement = format!("key {key_name} {{ [ U{:04X} ] }};", ch as u32);
        if let Some(idx) = out.find(&needle) {
            if let Some(end) = out[idx..].find("};") {
                out.replace_range(idx..idx + end + 2, &replacement);
                continue;
            }
        }
        missing.push_str(&format!("\n    {replacement}"));
    }
    if !missing.is_empty() {
        if let Some(sym_idx) = out.find("xkb_symbols") {
            if let Some(rel_end) = out[sym_idx..].find("\n};") {
                out.insert_str(sym_idx + rel_end, &missing);
            }
        }
    }
    out
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

#[cfg(test)]
mod unicode_keymap_tests {
    use super::*;
    use crate::stage::StageInputState;

    #[test]
    fn unicode_slots_compile_and_preserve_multiple_mappings() {
        let input = StageInputState::new();
        let base_str = input.xkb_keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let slots = [(231, '你'), (232, '好'), (241, '🚀')];
        let patched = inject_unicode_slots_into_xkb(&base_str, &slots);
        let km = xkb::Keymap::new_from_string(
            &input.xkb_context,
            patched,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("patched unicode XKB keymap must compile");
        let state = xkb::State::new(&km);
        assert_eq!(state.key_get_utf8((231u32 + 8).into()), "你");
        assert_eq!(state.key_get_utf8((232u32 + 8).into()), "好");
        assert_eq!(state.key_get_utf8((241u32 + 8).into()), "🚀");
    }
}
