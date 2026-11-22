use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use smithay::reexports::calloop::channel::{self, Channel, Sender as CalloopSender};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point};
use tracing::{info, warn};

use crate::protocol::{
    ActStatus, ControlRequest, ControlResponse, DisplayState, InputOwner, SavedRect,
};
use crate::router::{InputSource, PendingActJob, SettleState};
use crate::stage::{Lease, StageToplevel};
use crate::state::FocusStageState;

pub struct ControlServerMessage {
    pub request: ControlRequest,
    pub reply_tx: mpsc::Sender<ControlResponse>,
}

pub fn spawn_control_server(
    socket_path: &Path,
) -> std::io::Result<Channel<ControlServerMessage>> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = UnixListener::bind(socket_path)?;
    let (tx, rx) = channel::channel::<ControlServerMessage>();

    thread::Builder::new()
        .name("focusstage-ctrl".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let tx_conn = tx.clone();
                        thread::spawn(move || handle_client(stream, tx_conn));
                    }
                    Err(err) => {
                        warn!("Control socket accept error: {}", err);
                        break;
                    }
                }
            }
        })
        .expect("spawn control server thread");

    Ok(rx)
}

fn handle_client(stream: UnixStream, tx: CalloopSender<ControlServerMessage>) {
    let Ok(write_stream) = stream.try_clone() else {
        return;
    };
    let mut writer = write_stream;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<ControlRequest>(trimmed) {
            Ok(req) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlServerMessage { request: req, reply_tx }).is_err() {
                    ControlResponse::Error {
                        code: "daemon_down".to_string(),
                        message: "Compositor loop shut down".to_string(),
                    }
                } else {
                    reply_rx
                        .recv_timeout(Duration::from_secs(30))
                        .unwrap_or_else(|_| ControlResponse::Error {
                            code: "timeout".to_string(),
                            message: "Compositor request timed out".to_string(),
                        })
                }
            }
            Err(e) => ControlResponse::Error {
                code: "bad_request".to_string(),
                message: format!("Invalid JSON request: {e}"),
            },
        };

        if let Ok(json) = serde_json::to_string(&resp) {
            if writer.write_all(json.as_bytes()).is_err()
                || writer.write_all(b"\n").is_err()
                || writer.flush().is_err()
            {
                break;
            }
        }
    }
}

pub struct ControlClient {
    socket_path: std::path::PathBuf,
}

impl ControlClient {
    pub fn new(socket_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn call(&self, req: &ControlRequest) -> anyhow::Result<ControlResponse> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|e| {
            anyhow::anyhow!(
                "Cannot connect to focusstaged at {}: {} (is focusstaged running?)",
                self.socket_path.display(),
                e
            )
        })?;
        let payload = serde_json::to_string(req)?;
        stream.write_all(payload.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            anyhow::bail!("Empty response from focusstaged");
        }
        Ok(serde_json::from_str(&line)?)
    }
}

impl FocusStageState {
    pub fn expire_leases(&mut self) {
        let expired: Vec<String> = self
            .stages
            .iter()
            .filter_map(|(id, s)| {
                if let Some(ref l) = s.lease {
                    if l.is_expired() {
                        return Some(id.clone());
                    }
                }
                None
            })
            .collect();

        for sid in expired {
            if let Some(stage) = self.stages.get_mut(&sid) {
                info!("Lease expired on Stage {}, reverting to Human", sid);
                stage.lease = None;
                stage.input_owner = InputOwner::Human;
            }
        }
    }

    pub fn handle_control_message(&mut self, msg: ControlServerMessage) {
        let reply_tx = msg.reply_tx;
        match msg.request {
            ControlRequest::Ping => {
                let _ = reply_tx.send(ControlResponse::Pong {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    wayland_display: self.wayland_socket_name.clone(),
                    wslg_active: self.wslg_sink.is_some(),
                });
            }
            ControlRequest::List => {
                let mut stages: Vec<_> = self.stages.values().map(|s| s.info()).collect();
                stages.sort_by(|a, b| a.stage_id.cmp(&b.stage_id));
                let _ = reply_tx.send(ControlResponse::Stages { stages });
            }
            ControlRequest::Launch {
                app,
                args,
                kind,
                env,
                cwd,
                width,
                height,
                show_on_desktop,
                wm_class,
            } => {
                let resp = match self.launch_stage(
                    app,
                    args,
                    kind,
                    env,
                    cwd,
                    width,
                    height,
                    show_on_desktop,
                    wm_class,
                ) {
                    Ok((stage_id, pid)) => ControlResponse::Launched { stage_id, pid },
                    Err(e) => ControlResponse::Error {
                        code: "launch_failed".to_string(),
                        message: e,
                    },
                };
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Close { stage_id } => {
                let resp = match self.close_stage(&stage_id) {
                    Ok(()) => ControlResponse::Ok,
                    Err(e) => ControlResponse::Error {
                        code: "not_found".to_string(),
                        message: e,
                    },
                };
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Acquire {
                stage_id,
                ttl_s,
                hide,
                agent_name,
            } => {
                let resp = self.handle_acquire(&stage_id, ttl_s, hide, agent_name);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Renew { lease, ttl_s } => {
                let resp = self.handle_renew(&lease, ttl_s);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Observe {
                lease,
                stage_id,
                image,
                a11y,
            } => {
                let resp = self.handle_observe(lease.as_deref(), stage_id.as_deref(), image, a11y);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Act {
                lease,
                actions,
                based_on,
            } => {
                self.handle_act(lease, actions, based_on, reply_tx);
            }
            ControlRequest::Release {
                lease,
                show_on_desktop,
            } => {
                let resp = self.handle_release(&lease, show_on_desktop);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Takeover { stage_id } => {
                self.human_takeover(stage_id.as_deref());
                let _ = reply_tx.send(ControlResponse::Ok);
            }
            ControlRequest::SetDisplay { stage_id, display } => {
                let resp = self.set_stage_display(&stage_id, display);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::SetFrozen { stage_id, frozen } => {
                let resp = self.set_stage_frozen(&stage_id, frozen);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::ClipboardGet { lease, stage_id } => {
                let resp = self.handle_clipboard_get(lease.as_deref(), stage_id.as_deref());
                let _ = reply_tx.send(resp);
            }
            ControlRequest::ClipboardSet {
                lease,
                stage_id,
                text,
            } => {
                let resp = self.handle_clipboard_set(lease.as_deref(), stage_id.as_deref(), text);
                let _ = reply_tx.send(resp);
            }
            ControlRequest::SyncHostClipboard { text } => {
                self.sync_from_host_clipboard(text);
                let _ = reply_tx.send(ControlResponse::Ok);
            }
            ControlRequest::GetHostClipboard => {
                let _ = reply_tx.send(ControlResponse::Clipboard {
                    text: self.clipboard_state.latest_host_clipboard.clone(),
                    seq: self.clipboard_state.host_clipboard_seq,
                });
            }
            ControlRequest::DropFiles {
                stage_id,
                paths,
                x,
                y,
            } => {
                let pos = match (x, y) {
                    (Some(px), Some(py)) => Some(Point::<f64, Logical>::from((px, py))),
                    _ => None,
                };
                let resp = match self.drop_files_to_stage(&stage_id, &paths, pos) {
                    Ok(()) => ControlResponse::Ok,
                    Err(e) => ControlResponse::Error {
                        code: "drop_failed".to_string(),
                        message: e,
                    },
                };
                let _ = reply_tx.send(resp);
            }
            ControlRequest::Shutdown => {
                self.should_quit = true;
                let _ = reply_tx.send(ControlResponse::Ok);
            }
        }
    }

    fn find_stage_by_lease(&self, lease_id: &str) -> Option<String> {
        self.stages.iter().find_map(|(id, s)| {
            if let Some(ref l) = s.lease {
                if l.lease_id == lease_id {
                    return Some(id.clone());
                }
            }
            None
        })
    }

    fn handle_acquire(
        &mut self,
        stage_id: &str,
        ttl_s: u64,
        hide: bool,
        agent_name: Option<String>,
    ) -> ControlResponse {
        let Some(stage) = self.stages.get(stage_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", stage_id),
            };
        };

        if let Some(ref existing) = stage.lease {
            if !existing.is_expired() {
                return ControlResponse::Error {
                    code: "busy".to_string(),
                    message: format!(
                        "Stage '{}' is already held by lease '{}' (agent={:?})",
                        stage_id, existing.lease_id, existing.agent_name
                    ),
                };
            }
        }

        self.next_lease_seq += 1;
        let lease_id = format!("lease-{}-{}", stage_id, self.next_lease_seq);
        let ttl = ttl_s.max(1).min(3600);

        if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.lease = Some(Lease {
                lease_id: lease_id.clone(),
                agent_name: agent_name.clone(),
                expires_at: Instant::now() + Duration::from_secs(ttl),
            });
            stage.input_owner = InputOwner::Agent {
                lease_id: lease_id.clone(),
                agent_name,
            };
        }

        if hide {
            let _ = self.set_stage_display(stage_id, DisplayState::Hidden);
        }

        ControlResponse::Acquired {
            lease: lease_id,
            stage_id: stage_id.to_string(),
            expires_in_s: ttl,
        }
    }

    fn handle_renew(&mut self, lease_id: &str, ttl_s: u64) -> ControlResponse {
        let Some(stage_id) = self.find_stage_by_lease(lease_id) else {
            return ControlResponse::Error {
                code: "invalid_lease".to_string(),
                message: "Lease not found or preempted".to_string(),
            };
        };
        let ttl = ttl_s.max(1).min(3600);
        if let Some(stage) = self.stages.get_mut(&stage_id) {
            if let Some(ref mut l) = stage.lease {
                l.expires_at = Instant::now() + Duration::from_secs(ttl);
                return ControlResponse::Acquired {
                    lease: lease_id.to_string(),
                    stage_id,
                    expires_in_s: ttl,
                };
            }
        }
        ControlResponse::Error {
            code: "invalid_lease".to_string(),
            message: "Lease invalid".to_string(),
        }
    }

    fn handle_observe(
        &mut self,
        lease: Option<&str>,
        stage_id: Option<&str>,
        image: bool,
        a11y: bool,
    ) -> ControlResponse {
        let target_id = if let Some(l) = lease {
            match self.find_stage_by_lease(l) {
                Some(id) => id,
                None => {
                    return ControlResponse::Error {
                        code: "preempted".to_string(),
                        message: "Lease expired or preempted".to_string(),
                    };
                }
            }
        } else if let Some(sid) = stage_id {
            sid.to_string()
        } else {
            return ControlResponse::Error {
                code: "bad_request".to_string(),
                message: "Either lease or stage_id is required".to_string(),
            };
        };

        let Some(stage) = self.stages.get(&target_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", target_id),
            };
        };
        let frame_seq = stage.frame_seq;
        let scale = stage.scale.max(1.0);
        let xwm_id = stage.xwm_id;
        let x11_ovrs = xwm_id
            .map(|id| self.x11_override_windows_for(id))
            .unwrap_or_default();

        let (width, height, png_base64) = if image {
            let stage_ref = self.stages.get(&target_id).unwrap();
            match self.offscreen.render_stage_png(stage_ref, &x11_ovrs) {
                Ok((w, h, png)) => (
                    w,
                    h,
                    Some(base64::engine::general_purpose::STANDARD.encode(png)),
                ),
                Err(e) => {
                    return ControlResponse::Error {
                        code: "render_failed".to_string(),
                        message: e,
                    };
                }
            }
        } else {
            let s = self.stages.get(&target_id).unwrap();
            (
                s.logical_size.w.max(1) as u32,
                s.logical_size.h.max(1) as u32,
                None,
            )
        };

        let a11y_tree = if a11y {
            let pids = self.collect_stage_pids(&target_id);
            Some(self.a11y.query_pids(pids))
        } else {
            None
        };

        ControlResponse::Observed {
            stage_id: target_id,
            frame_seq,
            width,
            height,
            scale,
            png_base64,
            a11y_tree,
        }
    }

    fn handle_act(
        &mut self,
        lease_id: String,
        actions: Vec<crate::protocol::Action>,
        based_on: Option<u64>,
        reply_tx: mpsc::Sender<ControlResponse>,
    ) {
        let Some(stage_id) = self.find_stage_by_lease(&lease_id) else {
            let _ = reply_tx.send(ControlResponse::ActResult {
                act_status: ActStatus::Preempted,
                executed_steps: 0,
                frame_seq: 0,
                message: Some("Lease not found or preempted".to_string()),
            });
            return;
        };

        if !self.is_input_allowed(&stage_id, InputSource::Agent, Some(&lease_id)) {
            let frame_seq = self.stages.get(&stage_id).map(|s| s.frame_seq).unwrap_or(0);
            let _ = reply_tx.send(ControlResponse::ActResult {
                act_status: ActStatus::Expired,
                executed_steps: 0,
                frame_seq,
                message: Some("Lease expired or input owner changed".to_string()),
            });
            return;
        }

        let current_seq = self.stages.get(&stage_id).map(|s| s.frame_seq).unwrap_or(0);
        if let Some(expected) = based_on {
            if current_seq > expected {
                let _ = reply_tx.send(ControlResponse::ActResult {
                    act_status: ActStatus::Stale,
                    executed_steps: 0,
                    frame_seq: current_seq,
                    message: Some(format!(
                        "Frame changed from {} to {} before acting",
                        expected, current_seq
                    )),
                });
                return;
            }
        }

        let mut remaining: VecDeque<_> = actions.into();
        let mut executed_steps = 0usize;

        while let Some(action) = remaining.pop_front() {
            match self.execute_single_action(&stage_id, &action) {
                Ok(Some(dur)) => {
                    executed_steps += 1;
                    self.pending_act_jobs.push(PendingActJob {
                        stage_id,
                        lease_id,
                        remaining,
                        executed_steps,
                        wait_until: Some(Instant::now() + dur),
                        settling: None,
                        reply_tx,
                    });
                    return;
                }
                Ok(None) => {
                    executed_steps += 1;
                }
                Err(e) => {
                    let frame_seq = self.stages.get(&stage_id).map(|s| s.frame_seq).unwrap_or(0);
                    let _ = reply_tx.send(ControlResponse::ActResult {
                        act_status: ActStatus::Error,
                        executed_steps,
                        frame_seq,
                        message: Some(e),
                    });
                    return;
                }
            }
        }

        if executed_steps > 0 {
            let cur_seq = self.stages.get(&stage_id).map(|s| s.frame_seq).unwrap_or(0);
            let now = Instant::now();
            self.pending_act_jobs.push(PendingActJob {
                stage_id,
                lease_id,
                remaining,
                executed_steps,
                wait_until: None,
                settling: Some(SettleState {
                    initial_seq: cur_seq,
                    last_seq: cur_seq,
                    quiet_since: now,
                    started_at: now,
                    deadline: now + Duration::from_millis(1200),
                }),
                reply_tx,
            });
            return;
        }

        let frame_seq = self.stages.get(&stage_id).map(|s| s.frame_seq).unwrap_or(0);
        let _ = reply_tx.send(ControlResponse::ActResult {
            act_status: ActStatus::Ok,
            executed_steps,
            frame_seq,
            message: None,
        });
    }

    fn handle_release(&mut self, lease_id: &str, show_on_desktop: bool) -> ControlResponse {
        let Some(stage_id) = self.find_stage_by_lease(lease_id) else {
            return ControlResponse::Error {
                code: "invalid_lease".to_string(),
                message: "Lease not found or already preempted".to_string(),
            };
        };
        self.release_all_stage_inputs_pub(&stage_id);
        if let Some(stage) = self.stages.get_mut(&stage_id) {
            stage.lease = None;
            stage.input_owner = InputOwner::Human;
        }
        if show_on_desktop {
            let _ = self.set_stage_display(&stage_id, DisplayState::Desktop);
        }
        ControlResponse::Ok
    }

    fn release_all_stage_inputs_pub(&mut self, stage_id: &str) {
        let (keys, btns, pos) = match self.stages.get(stage_id) {
            Some(s) => (
                s.input.pressed_keys.iter().copied().collect::<Vec<_>>(),
                s.input.pressed_buttons.iter().copied().collect::<Vec<_>>(),
                s.input.pointer_pos,
            ),
            None => return,
        };
        for kc in keys {
            self.send_stage_evdev_key(stage_id, kc, false);
        }
        for b in btns {
            self.send_stage_button(stage_id, pos.x, pos.y, b, false);
        }
    }

    pub fn set_stage_display(&mut self, stage_id: &str, display: DisplayState) -> ControlResponse {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", stage_id),
            };
        };

        if stage.display == display {
            return ControlResponse::Ok;
        }
        stage.display = display;

        match display {
            DisplayState::Hidden => {
                if let Some(ref mut sink) = self.wslg_sink {
                    if let Some(rect) = sink.hide(stage_id) {
                        if let Some(stage) = self.stages.get_mut(stage_id) {
                            stage.saved_rect = Some(rect);
                        }
                    }
                }
            }
            DisplayState::Desktop => {
                let (title, wm_class, rect) = {
                    let stage = self.stages.get(stage_id).unwrap();
                    let rect = stage.saved_rect.unwrap_or(SavedRect {
                        x: 100,
                        y: 100,
                        width: stage.logical_size.w.max(1) as u32,
                        height: stage.logical_size.h.max(1) as u32,
                    });
                    (stage.title.clone(), stage.wm_class.clone(), rect)
                };
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.show(stage_id, &title, &wm_class, rect);
                }
                if let Some(stage) = self.stages.get_mut(stage_id) {
                    stage.dirty_for_sink = true;
                }
            }
        }
        ControlResponse::Ok
    }

    pub fn set_stage_frozen(&mut self, stage_id: &str, frozen: bool) -> ControlResponse {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", stage_id),
            };
        };
        if frozen {
            stage.lease = None;
            stage.input_owner = InputOwner::Frozen;
        } else if stage.input_owner == InputOwner::Frozen {
            stage.input_owner = InputOwner::Human;
        }
        ControlResponse::Ok
    }

    pub fn resize_stage_from_sink(&mut self, stage_id: &str, rect: SavedRect) {
        let toplevel = if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.saved_rect = Some(rect);
            stage.logical_size = (rect.width as i32, rect.height as i32).into();
            stage.dirty_for_sink = true;
            stage.active_toplevel().cloned()
        } else {
            None
        };

        if let Some(tl) = toplevel {
            match tl {
                StageToplevel::Wayland(w) => {
                    w.with_pending_state(|s| {
                        s.size = Some((rect.width as i32, rect.height as i32).into());
                        s.states.set(xdg_toplevel::State::Activated);
                    });
                    w.send_configure();
                }
                StageToplevel::X11(x) => {
                    let _ = x.configure(smithay::utils::Rectangle::new(
                        (0, 0).into(),
                        (rect.width as i32, rect.height as i32).into(),
                    ));
                }
            }
        }
    }

    fn handle_clipboard_get(
        &self,
        lease: Option<&str>,
        stage_id: Option<&str>,
    ) -> ControlResponse {
        let target_id = if let Some(l) = lease {
            match self.find_stage_by_lease(l) {
                Some(id) => id,
                None => {
                    return ControlResponse::Error {
                        code: "preempted".to_string(),
                        message: "Lease expired or preempted".to_string(),
                    };
                }
            }
        } else if let Some(sid) = stage_id {
            sid.to_string()
        } else {
            return ControlResponse::Error {
                code: "bad_request".to_string(),
                message: "Either lease or stage_id is required".to_string(),
            };
        };

        let Some(stage) = self.stages.get(&target_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", target_id),
            };
        };

        ControlResponse::Clipboard {
            text: stage.clipboard.text.clone(),
            seq: stage.clipboard.seq,
        }
    }

    fn handle_clipboard_set(
        &mut self,
        lease: Option<&str>,
        stage_id: Option<&str>,
        text: String,
    ) -> ControlResponse {
        let target_id = if let Some(l) = lease {
            match self.find_stage_by_lease(l) {
                Some(id) => id,
                None => {
                    return ControlResponse::Error {
                        code: "preempted".to_string(),
                        message: "Lease expired or preempted".to_string(),
                    };
                }
            }
        } else if let Some(sid) = stage_id {
            sid.to_string()
        } else {
            return ControlResponse::Error {
                code: "bad_request".to_string(),
                message: "Either lease or stage_id is required".to_string(),
            };
        };

        if !self.stages.contains_key(&target_id) {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", target_id),
            };
        }

        self.set_stage_clipboard_text(&target_id, text, false);
        let seq = self.stages.get(&target_id).map(|s| s.clipboard.seq).unwrap_or(0);
        ControlResponse::Clipboard { text: None, seq }
    }
}
