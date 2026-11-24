use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use smithay::reexports::calloop::channel::{self, Channel, Sender as CalloopSender};
use smithay::utils::{Logical, Point};
use tracing::{info, warn};

use crate::manager::HandoffWindows;
use crate::protocol::{
    ActStatus, ControlRequest, ControlResponse, DisplayState, InputOwner, LaunchFrame,
};
use crate::router::{PendingActJob, SettleState};
use crate::stage::{Lease, Stage, StageWindow};
use crate::state::FocusStageState;

const ENDED_LEASE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseEnd {
    Expired,
    Preempted,
    Released,
}

impl LeaseEnd {
    fn error(end: Option<Self>) -> ControlResponse {
        let (code, message) = match end {
            Some(Self::Expired) => ("expired", "Lease expired"),
            Some(Self::Preempted) => ("preempted", "Lease was preempted"),
            Some(Self::Released) => ("invalid_lease", "Lease was already released"),
            None => ("invalid_lease", "Lease not found"),
        };
        ControlResponse::Error { code: code.into(), message: message.into() }
    }

    fn act_result(end: Option<Self>, executed_steps: usize, frame_seq: u64) -> ControlResponse {
        let (act_status, message) = match end {
            Some(Self::Expired) => (ActStatus::Expired, "Lease expired"),
            Some(Self::Preempted) => (ActStatus::Preempted, "Lease was preempted"),
            Some(Self::Released) => (ActStatus::Preempted, "Lease was already released"),
            None => (ActStatus::Preempted, "Lease not found"),
        };
        ControlResponse::ActResult {
            act_status,
            executed_steps,
            frame_seq,
            message: Some(message.into()),
        }
    }
}

pub struct ControlServerMessage {
    pub request: ControlRequest,
    pub reply_tx: mpsc::Sender<ControlResponse>,
    pub peer_pid: Option<u32>,
}

pub(crate) struct PendingLaunch {
    pub stage_id: String,
    pub launcher_exit: Option<std::process::ExitStatus>,
    deadline: Instant,
    image: bool,
    pub(crate) handoff: Option<HandoffWindows>,
    reply_tx: mpsc::Sender<ControlResponse>,
}

fn launch_window<'a>(stage: &'a Stage, handoff: Option<&HandoffWindows>) -> Option<&'a StageWindow> {
    let active = stage.active_window().filter(|window| window.mapped);
    let Some(handoff) = handoff else { return active; };
    if stage.input_owner != InputOwner::Human {
        return None;
    }
    stage.windows.iter().rev().find(|window| {
        window.mapped && window.toplevel.alive() && !handoff.previous.contains_key(&window.id)
    }).or_else(|| active.filter(|window| {
        !handoff.new_window && handoff.previous.get(&window.id).is_some_and(|&seq| window.content_seq > seq)
    }))
}

pub fn local_peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
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
    let peer_pid = local_peer_pid(&stream);
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
            Ok(ControlRequest::ListApps) => handle_list_apps(),
            Ok(ControlRequest::Settings { action }) => handle_settings(action),
            Ok(req) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlServerMessage { request: req, reply_tx, peer_pid }).is_err() {
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

fn handle_list_apps() -> ControlResponse {
    static APPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = APPS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let apps = crate::desktop::discover_launcher_apps(true);
    ControlResponse::Apps {
        count: apps.len(),
        apps,
    }
}

fn handle_settings(action: crate::xdg::SettingsAction) -> ControlResponse {
    static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match crate::xdg::settings(action) {
        Ok(view) => ControlResponse::Settings { view },
        Err(err) => ControlResponse::Error {
            code: "settings_failed".to_string(),
            message: err.to_string(),
        },
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
    pub fn tick_pending_launches(&mut self) {
        let now = Instant::now();
        let mut launches = std::mem::take(&mut self.pending_launches);
        launches.retain(|pending| {
            let Some(response) = self.poll_pending_launch(pending, now) else { return true; };
            let _ = pending.reply_tx.send(response);
            false
        });
        self.pending_launches = launches;
    }

    fn poll_pending_launch(&mut self, pending: &PendingLaunch, now: Instant) -> Option<ControlResponse> {
        let stage_id = &pending.stage_id;
        let Some(stage) = self.stages.get(stage_id) else {
            let message = match pending.launcher_exit {
                Some(status) => format!("Launcher exited ({status}) before a window was ready"),
                None => "Stage closed before a window was ready".into(),
            };
            return Some(ControlResponse::Error { code: "launch_failed".into(), message });
        };
        let window = launch_window(stage, pending.handoff.as_ref()).filter(|window| {
            window.toplevel.wl_surface().is_some_and(|surface| {
                smithay::backend::renderer::utils::with_renderer_surface_state(
                    &surface, |state| state.buffer().is_some(),
                ).unwrap_or(false)
            })
        });
        let mut ready = window.is_some();
        let mut frame = None;
        let mut observe_message = None;
        if let Some(window) = window.filter(|_| pending.image) {
            let overrides = self.x11_override_windows_for(&window.toplevel);
            match self.offscreen.render_launch_png(window, &overrides) {
                Ok(Some((width, height, png))) => {
                    frame = Some(LaunchFrame {
                        frame_seq: stage.frame_seq,
                        width,
                        height,
                        scale: window.scale,
                        png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                    });
                }
                Ok(None) => {
                    ready = false;
                    observe_message = Some("Window has no drawable content".into());
                }
                Err(error) => observe_message = Some(format!("render_failed: {error}")),
            }
        }
        if !ready && now < pending.deadline {
            return None;
        }
        let pid = window.and_then(|window| window.toplevel.peer_pid()).or(stage.pid);
        if let (Some(window), Some(handoff)) = (window.filter(|_| ready), &pending.handoff) {
            let window_id = window.id.clone();
            let activate_host = handoff.show && stage.display == DisplayState::Desktop;
            self.activate_sink_window(stage_id, &window_id);
            if let Some(sink) = self.display_sink.as_mut().filter(|_| activate_host) {
                sink.activate(&window_id);
            }
        }
        Some(ControlResponse::LaunchReady {
            stage_id: stage_id.clone(),
            pid,
            ready,
            frame,
            launcher_exit: pending.launcher_exit.filter(|_| !ready).map(|status| status.to_string()),
            observe_message,
        })
    }

    pub(crate) fn fail_pending_launch(&mut self, stage_id: &str, message: &str) {
        self.pending_launches.retain(|pending| {
            if pending.stage_id != stage_id {
                return true;
            }
            let _ = pending.reply_tx.send(ControlResponse::Error {
                code: "launch_failed".into(),
                message: message.into(),
            });
            false
        });
    }

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
            self.expire_stage_lease(&sid);
        }
    }

    fn expire_stage_lease(&mut self, stage_id: &str) {
        info!("Lease expired on Stage {}, reverting to Human", stage_id);
        self.end_stage_lease(stage_id, LeaseEnd::Expired);
        self.sync_stage_agent_overlay(stage_id);
    }

    pub(crate) fn end_stage_lease(&mut self, stage_id: &str, end: LeaseEnd) {
        self.release_all_stage_inputs(stage_id);
        let stage = self.stages.get_mut(stage_id).unwrap();
        let lease = stage.lease.take();
        stage.expired_lease_for_restore = lease.as_ref()
            .filter(|_| end == LeaseEnd::Expired)
            .map(|lease| lease.lease_id.clone());
        stage.input_owner = InputOwner::Human;
        stage.input.reset_agent_pointer();
        if let Some(lease) = lease {
            self.record_ended_lease(lease, end);
        }
    }

    pub(crate) fn record_ended_lease(&mut self, lease: Lease, end: LeaseEnd) {
        if self.ended_leases.len() == ENDED_LEASE_LIMIT {
            self.ended_leases.pop_front();
        }
        self.ended_leases.push_back((lease.lease_id, end));
    }

    pub(crate) fn act_lease_failure(
        &mut self, lease_id: &str, executed_steps: usize, frame_seq: u64,
    ) -> ControlResponse {
        let end = self.active_lease_stage(lease_id).err().flatten();
        LeaseEnd::act_result(end, executed_steps, frame_seq)
    }

    pub fn handle_control_message(&mut self, msg: ControlServerMessage) {
        let reply_tx = msg.reply_tx;
        match msg.request {
            ControlRequest::Ping => {
                let _ = reply_tx.send(ControlResponse::Pong {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    wayland_display: self.wayland_socket_name.clone(),
                    desktop_active: self.display_sink.is_some(),
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
                wait_ready_ms,
                launch_image,
                launch_wait_for_update,
            } => {
                let wait_ready_ms = wait_ready_ms.filter(|&ms| ms > 0);
                let wait_for_handoff = wait_ready_ms.map(|_| launch_wait_for_update.unwrap_or(!args.is_empty()));
                let result = self.launch_stage(
                    app, args, kind, env, cwd, width, height, show_on_desktop, wm_class, wait_for_handoff,
                );
                match (result, wait_ready_ms) {
                    (Ok(launch), Some(ms)) => {
                        self.pending_launches.push(PendingLaunch {
                            stage_id: launch.stage_id,
                            launcher_exit: None,
                            deadline: Instant::now() + Duration::from_millis(ms.min(20_000)),
                            image: launch_image.unwrap_or(false),
                            handoff: launch.handoff,
                            reply_tx,
                        });
                    }
                    (Ok(launch), None) => {
                        let _ = reply_tx.send(ControlResponse::Launched { stage_id: launch.stage_id, pid: launch.pid });
                    }
                    (Err(message), _) => {
                        let _ = reply_tx.send(ControlResponse::Error {
                            code: "launch_failed".into(),
                            message,
                        });
                    }
                }
            }
            ControlRequest::OpenUri { uri, handler } => {
                let response = self.open_uri(&uri, handler.as_deref(), msg.peer_pid).unwrap_or_else(|e| ControlResponse::Error {
                    code: "open_failed".into(),
                    message: e.to_string(),
                });
                let _ = reply_tx.send(response);
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
                settle,
                settle_ms,
            } => {
                let settle_timeout = SettleState::timeout(settle, settle_ms);
                self.handle_act(lease, actions, based_on, settle_timeout, reply_tx);
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
                    text: self.clipboard_state.latest_host_text(),
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
                let _ = reply_tx.send(self.handle_drop_files(&stage_id, &paths, pos));
            }
            ControlRequest::DropFilesLeased { lease, paths, position } => {
                let response = self.handle_leased_drop(&lease, &paths, position);
                let _ = reply_tx.send(response);
            }
            ControlRequest::ListApps => {
                let _ = reply_tx.send(handle_list_apps());
            }
            ControlRequest::Settings { action } => {
                let _ = reply_tx.send(handle_settings(action));
            }
            ControlRequest::Shutdown => {
                self.should_quit = true;
                let _ = reply_tx.send(ControlResponse::Ok);
            }
        }
    }

    fn handle_leased_drop(&mut self, lease: &str, paths: &[String], position: Option<[f64; 2]>) -> ControlResponse {
        let stage_id = match self.active_lease_stage(lease) {
            Ok(stage_id) => stage_id,
            Err(end) => return LeaseEnd::error(end),
        };
        if paths.is_empty() || paths.iter().any(|path| {
            path.contains('\0') || !Path::new(&crate::protocol::windows_path_to_wsl(path)).is_absolute()
        }) {
            return ControlResponse::Error { code: "bad_request".into(), message: "Expected one or more absolute Windows or WSL paths".into() };
        }
        self.refresh_stage_lease(&stage_id);
        let point = position.map(|[x, y]| Point::from((x, y)));
        let response = self.handle_drop_files(&stage_id, paths, point);
        if matches!(response, ControlResponse::Ok) && point.is_some() {
            self.update_agent_pointer_overlay(&stage_id, "move");
        }
        response
    }

    fn handle_drop_files(&mut self, stage_id: &str, paths: &[String], pos: Option<Point<f64, Logical>>) -> ControlResponse {
        match self.drop_files_to_stage(stage_id, paths, pos) {
            Ok(()) => ControlResponse::Ok,
            Err(message) => ControlResponse::Error { code: "drop_failed".into(), message },
        }
    }

    fn active_lease_stage(&mut self, lease_id: &str) -> Result<String, Option<LeaseEnd>> {
        let active = self.stages.iter().find_map(|(id, stage)| {
            stage.lease.as_ref().filter(|lease| lease.lease_id == lease_id)
                .map(|lease| (id.clone(), lease.is_expired()))
        });
        match active {
            Some((id, false)) => Ok(id),
            Some((id, true)) => {
                self.expire_stage_lease(&id);
                Err(Some(LeaseEnd::Expired))
            }
            None => Err(self.ended_leases.iter().rev().find_map(|(id, end)| {
                (id == lease_id).then_some(*end)
            })),
        }
    }

    // Call only after validating the active lease and accepting the request.
    fn refresh_stage_lease(&mut self, stage_id: &str) {
        self.stages.get_mut(stage_id).unwrap().lease.as_mut().unwrap().refresh();
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

        if let Some(existing) = &stage.lease {
            if !existing.is_expired() {
                return ControlResponse::Error {
                    code: "busy".to_string(),
                    message: format!(
                        "Stage '{}' is already held by lease '{}' (agent={:?})",
                        stage_id, existing.lease_id, existing.agent_name
                    ),
                };
            }
            self.expire_stage_lease(stage_id);
        }

        self.next_lease_seq += 1;
        let lease_id = format!("lease-{}-{}", stage_id, self.next_lease_seq);
        let ttl = Duration::from_secs(ttl_s.clamp(1, 3600));

        let stage = self.stages.get_mut(stage_id).unwrap();
        stage.expired_lease_for_restore = None;
        stage.lease = Some(Lease {
            lease_id: lease_id.clone(),
            agent_name: agent_name.clone(),
            ttl,
            expires_at: Instant::now() + ttl,
        });
        stage.input_owner = InputOwner::Agent {
            lease_id: lease_id.clone(),
            agent_name,
        };
        stage.input.reset_agent_pointer();

        if hide {
            let _ = self.set_stage_display(stage_id, DisplayState::Hidden);
        } else {
            self.ensure_virtual_focus(stage_id);
        }
        self.sync_stage_agent_overlay(stage_id);

        ControlResponse::Acquired {
            lease: lease_id,
            stage_id: stage_id.to_string(),
            expires_in_s: ttl.as_secs(),
        }
    }

    fn handle_renew(&mut self, lease_id: &str, ttl_s: u64) -> ControlResponse {
        let stage_id = match self.active_lease_stage(lease_id) {
            Ok(stage_id) => stage_id,
            Err(end) => return LeaseEnd::error(end),
        };
        let ttl = ttl_s.clamp(1, 3600);
        let lease = self.stages.get_mut(&stage_id).unwrap().lease.as_mut().unwrap();
        lease.ttl = Duration::from_secs(ttl);
        lease.refresh();
        ControlResponse::Acquired {
            lease: lease_id.to_string(),
            stage_id,
            expires_in_s: ttl,
        }
    }

    fn resolve_stage_or_refresh_lease(
        &mut self, lease: Option<&str>, stage_id: Option<&str>,
    ) -> Result<String, ControlResponse> {
        if let Some(lease) = lease {
            let stage_id = self.active_lease_stage(lease).map_err(LeaseEnd::error)?;
            self.refresh_stage_lease(&stage_id);
            return Ok(stage_id);
        }
        stage_id.map(str::to_owned).ok_or_else(|| ControlResponse::Error {
            code: "bad_request".into(),
            message: "Either lease or stage_id is required".into(),
        })
    }

    fn handle_observe(
        &mut self,
        lease: Option<&str>,
        stage_id: Option<&str>,
        image: bool,
        a11y: bool,
    ) -> ControlResponse {
        let target_id = match self.resolve_stage_or_refresh_lease(lease, stage_id) {
            Ok(id) => id,
            Err(error) => return error,
        };

        let Some(stage) = self.stages.get(&target_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", target_id),
            };
        };
        let frame_seq = stage.frame_seq;
        let scale = stage.active_window().map_or(stage.default_scale, |w| w.scale);
        let x11_ovrs = stage.active_toplevel().map(|t| self.x11_override_windows_for(t)).unwrap_or_default();

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
                s.logical_size().w.max(1) as u32,
                s.logical_size().h.max(1) as u32,
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
        settle_timeout: Option<Duration>,
        reply_tx: mpsc::Sender<ControlResponse>,
    ) {
        let stage_id = match self.active_lease_stage(&lease_id) {
            Ok(stage_id) => stage_id,
            Err(end) => {
                let _ = reply_tx.send(LeaseEnd::act_result(end, 0, 0));
                return;
            }
        };

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

        self.refresh_stage_lease(&stage_id);
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
                        settle_timeout,
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

        let stage = self.stages.get(&stage_id);
        let frame_seq = stage.map_or(0, |s| s.frame_seq);
        if let Some(timeout) = settle_timeout.filter(|_| executed_steps > 0) {
            let settle_seq = stage.map_or(0, |s| s.settle_seq);
            self.pending_act_jobs.push(PendingActJob {
                stage_id,
                lease_id,
                remaining,
                executed_steps,
                wait_until: None,
                settle_timeout,
                settling: Some(SettleState::new(settle_seq, Instant::now(), timeout)),
                reply_tx,
            });
            return;
        }

        let _ = reply_tx.send(ControlResponse::ActResult {
            act_status: ActStatus::Ok,
            executed_steps,
            frame_seq,
            message: None,
        });
    }

    fn handle_release(&mut self, lease_id: &str, show_on_desktop: bool) -> ControlResponse {
        let stage_id = match self.active_lease_stage(lease_id) {
            Ok(stage_id) => {
                self.end_stage_lease(&stage_id, LeaseEnd::Released);
                stage_id
            }
            Err(Some(LeaseEnd::Expired)) => {
                let stage = self.stages.values_mut().find(|stage| {
                    stage.input_owner == InputOwner::Human
                        && stage.expired_lease_for_restore.as_deref() == Some(lease_id)
                });
                let Some(stage) = stage else { return ControlResponse::Ok; };
                stage.expired_lease_for_restore = None;
                stage.id.clone()
            }
            Err(Some(LeaseEnd::Released | LeaseEnd::Preempted)) => return ControlResponse::Ok,
            Err(None) => return LeaseEnd::error(None),
        };
        if show_on_desktop {
            let _ = self.set_stage_display(&stage_id, DisplayState::Desktop);
        }
        self.sync_stage_agent_overlay(&stage_id);
        ControlResponse::Ok
    }

    pub fn set_stage_display(&mut self, stage_id: &str, display: DisplayState) -> ControlResponse {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", stage_id),
            };
        };

        stage.expired_lease_for_restore = None;
        if stage.display == display {
            return ControlResponse::Ok;
        }
        stage.display = display;

        self.update_stage_window_visibility(stage_id);
        if display == DisplayState::Hidden {
            self.ensure_virtual_focus(stage_id);
        }
        ControlResponse::Ok
    }

    pub fn set_stage_frozen(&mut self, stage_id: &str, frozen: bool) -> ControlResponse {
        let Some(stage) = self.stages.get(stage_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", stage_id),
            };
        };
        if frozen && stage.lease.is_some() {
            self.end_stage_lease(stage_id, LeaseEnd::Preempted);
        }
        let stage = self.stages.get_mut(stage_id).unwrap();
        if frozen {
            stage.expired_lease_for_restore = None;
            stage.input_owner = InputOwner::Frozen;
            stage.input.reset_agent_pointer();
            self.ensure_virtual_focus(stage_id);
        } else if stage.input_owner == InputOwner::Frozen {
            stage.input_owner = InputOwner::Human;
        }
        self.sync_stage_agent_overlay(stage_id);
        ControlResponse::Ok
    }

    fn handle_clipboard_get(
        &mut self,
        lease: Option<&str>,
        stage_id: Option<&str>,
    ) -> ControlResponse {
        let target_id = match self.resolve_stage_or_refresh_lease(lease, stage_id) {
            Ok(id) => id,
            Err(error) => return error,
        };

        let Some(stage) = self.stages.get(&target_id) else {
            return ControlResponse::Error {
                code: "not_found".to_string(),
                message: format!("Stage '{}' not found", target_id),
            };
        };

        ControlResponse::Clipboard {
            text: stage.clipboard.text(),
            seq: stage.clipboard.seq,
        }
    }

    fn handle_clipboard_set(
        &mut self,
        lease: Option<&str>,
        stage_id: Option<&str>,
        text: String,
    ) -> ControlResponse {
        let target_id = match self.resolve_stage_or_refresh_lease(lease, stage_id) {
            Ok(id) => id,
            Err(error) => return error,
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
