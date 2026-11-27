use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use tracing::{info, warn};

use crate::protocol::{infer_stage_kind, ControlResponse, DisplayState, InputOwner, StageKind};
use crate::xdg::{self, OpenRoute, OpenTarget};
use crate::stage::Stage;
use crate::state::FocusStageState;

pub(crate) struct StageLaunch {
    pub stage_id: String,
    pub pid: Option<u32>,
    pub handoff: Option<HandoffWindows>,
}

pub(crate) struct HandoffWindows {
    pub previous: HashMap<String, u64>,
    pub new_window: bool,
    pub show: bool,
}

impl FocusStageState {
    pub(crate) fn launch_stage(
        &mut self,
        app: String,
        args: Vec<String>,
        kind_opt: Option<StageKind>,
        extra_env: HashMap<String, String>,
        cwd: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        show_on_desktop: Option<bool>,
        wm_class: Option<String>,
        wait_for_handoff: Option<bool>,
        run_as_root: bool,
    ) -> Result<StageLaunch, String> {
        if run_as_root {
            if !Path::new("/usr/bin/pkexec").is_file() {
                return Err("未找到 /usr/bin/pkexec，无法以 root 身份启动应用。".into());
            }
            let _ = self.ensure_polkit_agent_stage();
        }

        let inferred_kind = infer_stage_kind(&app);
        let kind = kind_opt.unwrap_or(inferred_kind);
        let show = show_on_desktop.unwrap_or(self.display_sink.is_some());
        let wm_class = wm_class.as_deref().map(str::trim).filter(|class| !class.is_empty());
        if is_single_instance(kind) || is_single_instance(inferred_kind) {
            let existing = self.find_handoff_stage(&app, None, wm_class, run_as_root).map_err(|err| err.to_string())?;
            if let Some(stage_id) = existing {
                let stage = &self.stages[&stage_id];
                if extra_env.iter().any(|(key, value)| stage.launch_env.get(key) != Some(value)) {
                    return Err("The running Stage uses a different launch environment; close it before changing the environment".into());
                }
                let new_window = args.iter().take_while(|arg| arg.as_str() != "--")
                    .any(|arg| matches!(arg.as_str(), "--new-window" | "-new-window"));
                let handoff = wait_for_handoff.filter(|&wait_for_update| new_window || wait_for_update)
                    .map(|_| HandoffWindows {
                        previous: stage.windows.iter().filter(|window| window.mapped)
                            .map(|window| (window.id.clone(), window.content_seq)).collect(),
                        new_window,
                        show,
                    });
                if handoff.is_some() && self.pending_launches.iter().any(|pending| {
                    pending.stage_id == stage_id && pending.handoff.is_some()
                }) {
                    return Err("A window launch is already pending for this Stage".into());
                }
                let (stage_id, pid) = self.handoff_to_stage(stage_id, &app, args, cwd.as_deref(), show, false)
                    .map_err(|err| err.to_string())?;
                return Ok(StageLaunch { stage_id, pid, handoff });
            }
        }

        self.next_stage_seq += 1;
        let stage_id = format!("stage-{}", self.next_stage_seq);
        let w = width.unwrap_or(1280).max(64);
        let h = height.unwrap_or(800).max(64);
        let default_display = if show { DisplayState::Desktop } else { DisplayState::Hidden };

        let mut stage = Stage::new(
            stage_id.clone(),
            app.clone(),
            args.clone(),
            kind,
            w,
            h,
            default_display,
            self.default_scale,
        );
        if let Some(class) = wm_class {
            stage.wm_class = class.to_string();
        }

        stage.launch_token = Some(new_launch_token().map_err(|err| format!("create Stage launch token: {err}"))?);
        stage.launch_env = extra_env;
        stage.launch_cwd = cwd;
        stage.run_as_root = run_as_root;

        if is_single_instance(kind) {
            self.a11y.prepare_for_launch();
        }

        // Snap wrappers can select X11 even when the application requests Wayland.
        if kind == StageKind::X11
            || (is_chromium(kind) && (chromium_platform(&args, is_wsl2()) != "wayland" || is_snap_launcher(&app)))
        {
            stage.pending_x11_launch = true;
            self.stages.insert(stage_id.clone(), stage);
            if let Err(err) = self.spawn_xwayland_for_stage(stage_id.clone()) {
                self.stages.remove(&stage_id);
                return Err(err);
            }
            return Ok(StageLaunch { stage_id, pid: None, handoff: None });
        }

        let child = spawn_stage_process(&self.wayland_socket_name, &stage, None)
            .map_err(|err| format!("failed to spawn '{app}': {err}"))?;
        let pid = child.id();
        stage.pid = Some(pid);
        stage.child = Some(child);
        self.stages.insert(stage_id.clone(), stage);
        info!("Launched {} Stage {} (pid={})", format!("{:?}", kind), stage_id, pid);
        Ok(StageLaunch { stage_id, pid: Some(pid), handoff: None })
    }

    pub fn open_uri(&mut self, uri: &str, handler: Option<&str>, peer_pid: Option<u32>) -> anyhow::Result<ControlResponse> {
        if peer_pid.and_then(|pid| self.resolve_stage_for_pid(pid)).is_some_and(|id| self.stages[&id].input_owner != InputOwner::Human) {
            bail!("Only Human Stages can open links outside their Stage");
        }
        let target = OpenTarget::parse(uri)?;
        let OpenRoute::Linux { id, app, wm_class, args } = xdg::resolve(&target, handler)? else {
            if !self.display_sink.as_mut().is_some_and(|sink| sink.open_web_url(target.uri())) {
                bail!("Windows browser routing requires the native Windows host");
            }
            return Ok(ControlResponse::Opened { stage_id: None, reused: false });
        };
        if let Some(stage_id) = self.find_handoff_stage(&app, Some(&id), wm_class.as_deref(), false)? {
            let (stage_id, _) = self.handoff_to_stage(stage_id, &app, args, None, true, true)?;
            return Ok(ControlResponse::Opened { stage_id: Some(stage_id), reused: true });
        }
        let wm_class = Some(format!("{}{}", crate::desktop::WRAPPER_PREFIX, id));
        let launch = self.launch_stage(app, args, None, HashMap::new(), None, None, None, None, wm_class, None, false)
            .map_err(anyhow::Error::msg)?;
        Ok(ControlResponse::Opened { stage_id: Some(launch.stage_id), reused: false })
    }

    fn find_handoff_stage(&self, app: &str, id: Option<&str>, wm_class: Option<&str>, run_as_root: bool) -> anyhow::Result<Option<String>> {
        let matching = self.stages.values().filter(|stage| stage.run_as_root == run_as_root && matches_desktop_app(stage, app, id, wm_class));
        let stage = select_handoff_stage(matching)?;
        if stage.is_some_and(|stage| !stage.has_live_surface()) {
            bail!("The matching Stage has no visible surface yet; wait for it to open or close it before retrying");
        }
        Ok(stage.map(|stage| stage.id.clone()))
    }

    fn handoff_to_stage(
        &mut self,
        stage_id: String,
        app: &str,
        mut args: Vec<String>,
        cwd: Option<&str>,
        show: bool,
        opening_uri: bool,
    ) -> anyhow::Result<(String, Option<u32>)> {
        let stage = &self.stages[&stage_id];
        let window_id = stage.active_window().map(|window| window.id.clone());
        let pid = stage.pid;
        if opening_uri || !args.is_empty() {
            if is_chromium(stage.kind) && explicit_chromium_platform(&args)
                .is_some_and(|platform| platform != chromium_platform(&stage.args, is_wsl2()))
            {
                bail!("The running Stage uses a different Ozone platform; close it before changing the platform");
            }
            prepare_handoff_args(stage, &mut args, cwd)?;
            let mut cmd = stage_command(&self.wayland_socket_name, stage, app, &args, stage.x11_display_number);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            if opening_uri {
                cmd.stderr(Stdio::null()); // Callback URIs can contain credentials.
            }
            let child = spawn_stage_command(&mut cmd, stage).context("Could not dispatch to the running Stage")?;
            self.handoff_children.push(child);
        }
        if show && self.display_sink.is_some() {
            self.set_stage_display(&stage_id, DisplayState::Desktop);
            if let (Some(sink), Some(window)) = (&mut self.display_sink, window_id) {
                sink.activate(&window);
            }
        }
        Ok((stage_id, pid))
    }

    pub fn launch_pending_x11_client(&mut self, stage_id: &str, display_number: u32) {
        let Some(stage) = self.stages.get_mut(stage_id) else { return; };
        if !std::mem::take(&mut stage.pending_x11_launch) {
            return;
        }
        let spawned = if stage.persistent_helper {
            spawn_persistent_helper_process(&self.wayland_socket_name, stage, display_number)
        } else {
            spawn_stage_process(&self.wayland_socket_name, stage, Some(display_number))
        };
        match spawned {
            Ok(child) => {
                let pid = child.id();
                stage.pid = Some(pid);
                stage.child = Some(child);
                info!(stage = stage_id, kind = ?stage.kind, display_number, pid, "Launched Stage with XWayland");
            }
            Err(err) => {
                let message = format!("Failed to launch '{}': {err}", stage.app);
                warn!(stage = stage_id, "{message}");
                self.fail_pending_launch(stage_id, &message);
                let _ = self.close_stage(stage_id);
            }
        }
    }

    pub fn close_stage(&mut self, stage_id: &str) -> Result<(), String> {
        self.cancel_stage_drag(stage_id);
        let mut stage = self
            .stages
            .remove(stage_id)
            .ok_or_else(|| format!("Stage '{}' not found", stage_id))?;

        if let Some(lease) = stage.lease.take() {
            use crate::control::LeaseEnd;
            let end = if lease.is_expired() { LeaseEnd::Expired } else { LeaseEnd::Preempted };
            self.record_ended_lease(lease, end);
        }
        if let Some(unit) = stage.helper_unit.as_deref() {
            stop_helper_unit(unit);
        }
        if stage.run_as_root {
            for window in &stage.windows {
                match &window.toplevel {
                    crate::stage::StageToplevel::Wayland(w) => w.send_close(),
                    crate::stage::StageToplevel::X11(w) => {
                        let _ = w.close();
                    }
                }
            }
        }
        terminate_stage_processes(&stage);
        if let Some(ref mut child) = stage.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(token) = stage.xwayland_token.take() {
            self.loop_handle.remove(token);
        }
        if let Some(xwm_id) = stage.xwm_id {
            // Keep self.xwms entry alive until XwmHandler::disconnected fires so any
            // in-flight X11Source events draining during teardown never panic.
            self.xwm_to_stage.remove(&xwm_id);
            self.x11_override_windows.remove(&xwm_id);
        }
        if let Some(ref mut sink) = self.display_sink {
            for window in &stage.windows {
                sink.destroy(&window.id);
            }
        }
        Ok(())
    }

    pub fn reap_exited_stages(&mut self) {
        self.handoff_children.retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) if !status.success() => {
                warn!(%status, "Stage handoff process failed");
                false
            }
            _ => false,
        });
        let now = Instant::now();
        let mut finished = Vec::new();
        for (id, stage) in &mut self.stages {
            if let Some(child) = stage.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    if status.success() {
                        info!(stage = %id, app = %stage.app, %status, "Stage launcher exited");
                    } else {
                        warn!(stage = %id, app = %stage.app, %status, "Stage launcher failed");
                    }
                    for pending in self.pending_launches.iter_mut().filter(|pending| pending.stage_id == *id) {
                        pending.launcher_exit = Some(status);
                    }
                    stage.child = None;
                    stage.pid = None;
                    stage.pid = representative_pid(&stage_processes(stage));
                    // A launcher can exit before its GUI connects or even finishes exec().
                    stage.next_process_check = Some(now + Duration::from_secs(2));
                }
            } else if let Some(next_check) = stage.next_process_check {
                if now >= next_check {
                    if !stage.pid.is_some_and(|pid| live_stage_pid(stage, pid)) {
                        stage.pid = representative_pid(&stage_processes(stage));
                    }
                    if stage.pid.is_none() && !stage.has_live_surface() {
                        finished.push(id.clone());
                    } else {
                        stage.next_process_check = Some(now + Duration::from_secs(1));
                    }
                }
            } else if stage.retire_when_empty && stage.windows.is_empty() {
                finished.push(id.clone());
            }
        }
        for id in finished {
            let _ = self.close_stage(&id);
        }
    }

    /// Find a Stage by the client's launch token or the process that started it.
    pub fn resolve_stage_for_pid(&self, mut pid: u32) -> Option<String> {
        let mut visited = HashSet::new();
        while pid > 1 && visited.insert(pid) {
            let token = read_launch_token(pid);
            if let Some(stage) = self.stages.values().find(|stage| {
                (stage.pid == Some(pid) && (stage.child.is_some() || stage.run_as_root || stage.launch_token.is_none()))
                    || (token.is_some() && stage.launch_token == token)
            }) {
                return Some(stage.id.clone());
            }
            pid = read_process(pid)?.0;
        }
        None
    }

    /// Match AT-SPI against the process family and the actual window peers.
    pub fn collect_stage_pids(&self, stage_id: &str) -> HashSet<u32> {
        let Some(stage) = self.stages.get(stage_id) else { return HashSet::new(); };
        let mut pids = stage_processes(stage);
        pids.extend(stage.windows.iter().filter_map(|window| window.toplevel.peer_pid()));
        pids
    }

    pub fn ensure_session_helper_stages(&mut self) {
        if let Err(err) = self.ensure_polkit_agent_stage() {
            warn!("Failed to start FocusStage Polkit authentication host: {err}");
        }
        if let Err(err) = self.ensure_keyring_prompter_stage() {
            warn!("Failed to start FocusStage Keyring prompter host: {err}");
        }
    }

    pub fn ensure_polkit_agent_stage(&mut self) -> Result<bool, String> {
        let Some(sink) = self.display_sink.as_ref() else {
            return Ok(false);
        };
        if self.polkit_agent.is_none() {
            let locale = sink.host_locale().map(str::to_string);
            self.polkit_agent = Some(crate::polkit::PolkitAgentHandle::spawn(
                self.polkit_cmd_tx.clone(),
                locale,
            ));
        }
        Ok(self
            .polkit_agent
            .as_ref()
            .is_some_and(|agent| agent.ensure_registered()))
    }

    pub fn ensure_keyring_prompter_stage(&mut self) -> Result<bool, String> {
        if self.display_sink.is_none() {
            return Ok(false);
        }
        if self.keyring_prompter.is_none() {
            match crate::keyring::KeyringPrompterHandle::spawn(self.polkit_cmd_tx.clone()) {
                Ok(handle) => {
                    self.keyring_prompter = Some(handle);
                }
                Err(err) => {
                    warn!("Native Keyring SystemPrompter unavailable ({err}); falling back to XWayland gcr-prompter");
                    return self.ensure_persistent_helper_stage(&KEYRING_PROMPTER_SPEC);
                }
            }
        }
        Ok(self
            .keyring_prompter
            .as_ref()
            .is_some_and(|p| p.ensure_registered()))
    }

    fn ensure_persistent_helper_stage(&mut self, spec: &PersistentHelperSpec) -> Result<bool, String> {
        if let Some(existing) = self.stages.get_mut(spec.stage_id) {
            if let Some(child) = existing.child.as_mut() {
                if let Ok(Some(_)) = child.try_wait() {
                    existing.child = None;
                    existing.pid = None;
                }
            }
            let alive = existing.child.is_some()
                || existing.pending_x11_launch
                || existing.pid.is_some_and(|pid| live_stage_pid(existing, pid));
            if alive {
                let ready = existing.x11_display_number.is_some() && existing.pid.is_some();
                return Ok(ready);
            }
            let _ = self.close_stage(spec.stage_id);
        }

        let Some(helper_bin) = spec.candidates.iter().find(|p| Path::new(p).is_file()) else {
            return Ok(false);
        };

        // Stop any older WSLg-bound helper unit or D-Bus-activated instance before claiming inside FocusStage.
        stop_helper_unit(spec.unit_name);
        if let Some(proc_name) = spec.stop_process_name {
            stop_existing_user_process(proc_name);
        }

        let mut stage = Stage::new(
            spec.stage_id.to_string(),
            (*helper_bin).to_string(),
            Vec::new(),
            StageKind::X11,
            480,
            240,
            DisplayState::Desktop,
            self.default_scale,
        );
        stage.wm_class = spec.wm_class.to_string();
        stage.persistent_helper = true;
        stage.helper_unit = Some(spec.unit_name.to_string());
        stage.pending_x11_launch = true;
        for &(key, val) in spec.extra_env {
            stage.launch_env.insert(key.to_string(), val.to_string());
        }
        stage.launch_token = Some(
            new_launch_token()
                .map_err(|err| format!("create {} launch token: {err}", spec.label))?,
        );

        self.stages.insert(spec.stage_id.to_string(), stage);
        if let Err(err) = self.spawn_xwayland_for_stage(spec.stage_id.to_string()) {
            self.stages.remove(spec.stage_id);
            return Err(err);
        }
        info!("Started FocusStage XWayland {} ({})", spec.label, helper_bin);
        Ok(false)
    }
}

struct PersistentHelperSpec {
    stage_id: &'static str,
    wm_class: &'static str,
    unit_name: &'static str,
    label: &'static str,
    candidates: &'static [&'static str],
    extra_env: &'static [(&'static str, &'static str)],
    stop_process_name: Option<&'static str>,
}

const KEYRING_PROMPTER_SPEC: PersistentHelperSpec = PersistentHelperSpec {
    stage_id: "stage-keyring-prompter",
    wm_class: "focusstage-keyring-prompter",
    unit_name: "focusstage-gcr-prompter",
    label: "Keyring prompter host",
    candidates: &[
        "/usr/libexec/gcr-prompter",
        "/usr/lib/gcr/gcr-prompter",
    ],
    extra_env: &[("GCR_PERSIST", "1")],
    stop_process_name: Some("gcr-prompter"),
};

pub fn stop_helper_unit(unit_name: &str) {
    if !crate::session_env::systemd_user_available() {
        return;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "stop", unit_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed", unit_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub(crate) fn stop_existing_user_process(proc_name: &str) {
    let uid = unsafe { libc::geteuid() }.to_string();
    let signaled = Command::new("pkill")
        .args(["-TERM", "-u", &uid, "-x", proc_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !signaled {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        let still_alive = Command::new("pgrep")
            .args(["-u", &uid, "-x", proc_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !still_alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn build_helper_systemd_args(
    stage: &Stage,
    unit_name: &str,
    display_number: u32,
    xdg_desktop: &str,
) -> Vec<String> {
    let token = stage.launch_token.as_deref().unwrap_or_default();
    let mut args = vec![
        "--user".to_string(),
        "--wait".to_string(),
        "--collect".to_string(),
        format!("--unit={unit_name}"),
        "-E".to_string(),
        format!("DISPLAY=:{display_number}"),
        "-E".to_string(),
        format!("XDG_CURRENT_DESKTOP={xdg_desktop}"),
        "-E".to_string(),
        format!("FOCUSSTAGE_STAGE_ID={}", stage.id),
        "-E".to_string(),
        format!("{LAUNCH_TOKEN_ENV}={token}"),
    ];
    let mut extra_keys: Vec<_> = stage.launch_env.keys().collect();
    extra_keys.sort();
    for key in extra_keys {
        let val = &stage.launch_env[key];
        args.push("-E".to_string());
        args.push(format!("{key}={val}"));
    }
    args.extend([
        "/usr/bin/env".to_string(),
        "-u".to_string(),
        "WAYLAND_DISPLAY".to_string(),
        "-u".to_string(),
        "XAUTHORITY".to_string(),
        "GDK_BACKEND=x11".to_string(),
        "QT_QPA_PLATFORM=xcb".to_string(),
        stage.app.clone(),
    ]);
    args
}

fn spawn_persistent_helper_process(
    wayland_socket: &str,
    stage: &Stage,
    display_number: u32,
) -> std::io::Result<Child> {
    if crate::session_env::systemd_user_available() {
        let unit_name = stage
            .helper_unit
            .as_deref()
            .unwrap_or("focusstage-helper");
        let xdg_desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "GNOME".to_string());
        let args = build_helper_systemd_args(stage, unit_name, display_number, &xdg_desktop);
        let mut cmd = Command::new("systemd-run");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        return cmd.spawn();
    }

    let mut cmd = stage_command(wayland_socket, stage, &stage.app, &stage.args, Some(display_number));
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("XAUTHORITY");
    cmd.stderr(Stdio::inherit());
    spawn_stage_command(&mut cmd, stage)
}

fn is_chromium(kind: StageKind) -> bool {
    matches!(kind, StageKind::Electron | StageKind::Chromium)
}

fn is_single_instance(kind: StageKind) -> bool {
    is_chromium(kind) || kind == StageKind::Gecko
}

fn explicit_chromium_platform(args: &[String]) -> Option<&str> {
    args.iter().take_while(|arg| arg.as_str() != "--")
        .filter_map(|arg| arg.strip_prefix("--ozone-platform="))
        .last()
}

fn chromium_platform(args: &[String], wsl2: bool) -> &str {
    explicit_chromium_platform(args).unwrap_or(if wsl2 { "x11" } else { "wayland" })
}

fn kernel_release() -> &'static str {
    static RELEASE: OnceLock<String> = OnceLock::new();
    RELEASE.get_or_init(|| std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default().to_ascii_lowercase())
}

fn is_wsl2() -> bool {
    let release = kernel_release();
    release.contains("wsl2") || release.contains("microsoft-standard")
}

pub(crate) fn wsl_gallium_driver() -> Option<OsString> {
    if is_wsl2() && Path::new("/dev/dxg").exists() {
        Some(std::env::var_os("GALLIUM_DRIVER").unwrap_or_else(|| "d3d12".into()))
    } else {
        None
    }
}

fn profile_arguments(kind: StageKind, args: &[String]) -> anyhow::Result<Vec<&str>> {
    let mut profile = Vec::new();
    let mut args = args.iter().map(String::as_str).take_while(|arg| *arg != "--").peekable();
    while let Some(arg) = args.next() {
        let (flag, value) = arg.split_once('=').map_or((arg, None), |(flag, value)| (flag, Some(value)));
        let selects_profile = match kind {
            StageKind::Chromium | StageKind::Electron => flag == "--user-data-dir",
            StageKind::Gecko => match flag.trim_start_matches('-').to_ascii_lowercase().as_str() {
                "no-remote" | "new-instance" | "profilemanager" if flag.starts_with('-') => {
                    bail!("This launch option cannot be dispatched to a running Stage: {flag}");
                }
                "profile" | "p" => flag.starts_with('-'),
                _ => false,
            },
            _ => false,
        };
        if selects_profile {
            profile.push(arg);
            if value.is_none() && args.peek().is_some_and(|value| !value.starts_with('-')) {
                profile.push(args.next().unwrap());
            }
        }
    }
    Ok(profile)
}

fn prepare_handoff_args(stage: &Stage, args: &mut Vec<String>, cwd: Option<&str>) -> anyhow::Result<()> {
    let kind = if is_single_instance(stage.kind) { stage.kind } else { infer_stage_kind(&stage.app) };
    let existing = profile_arguments(kind, &stage.args)?;
    let requested = profile_arguments(kind, args)?;
    if (!requested.is_empty() && requested != existing)
        || (!existing.is_empty() && cwd.is_some() && cwd != stage.launch_cwd.as_deref())
    {
        bail!("The running Stage uses a different profile; close it before selecting another profile");
    }
    if requested.is_empty() {
        args.splice(..0, existing.into_iter().map(str::to_owned));
    }
    Ok(())
}

fn executable_path(app: &str) -> Option<PathBuf> {
    let path = Path::new(app);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else if path.components().count() == 1 {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(path))
            .find(|candidate| candidate.is_file())?
    } else {
        return None;
    };
    // Resolving Snap shims would collapse every snap to /usr/bin/snap.
    if is_snap_launcher(path.to_str()?) { Some(path) }
    else { path.canonicalize().ok() }
}

fn matches_desktop_app(stage: &Stage, app: &str, id: Option<&str>, wm_class: Option<&str>) -> bool {
    if stage.app == app
        || id.is_some_and(|id| stage.wm_class.strip_prefix(crate::desktop::WRAPPER_PREFIX) == Some(id))
        || wm_class.is_some_and(|class| stage.wm_class.eq_ignore_ascii_case(class))
    {
        return true;
    }
    match (executable_path(&stage.app), executable_path(app)) {
        (Some(stage), Some(app)) => stage == app,
        _ => !Path::new(&stage.app).is_absolute() && !Path::new(app).is_absolute()
            && Path::new(&stage.app).file_name() == Path::new(app).file_name(),
    }
}

fn select_handoff_stage<'a>(mut stages: impl Iterator<Item = &'a Stage>) -> anyhow::Result<Option<&'a Stage>> {
    let Some(stage) = stages.next() else { return Ok(None); };
    if stages.next().is_some() {
        bail!("More than one running Stage matches this application; the target is ambiguous");
    }
    match stage.input_owner {
        InputOwner::Human => Ok(Some(stage)),
        InputOwner::Agent { .. } => bail!("The application is leased to an Agent; release the lease before opening it again"),
        InputOwner::Frozen => bail!("The application is Frozen; unfreeze its Stage before opening it again"),
    }
}

fn is_snap_launcher(app: &str) -> bool {
    let is_snap_path = |path: &Path| {
        path.starts_with("/snap/bin") || path.starts_with("/var/lib/snapd/snap/bin")
    };
    let path = Path::new(app);
    if path.is_absolute() {
        return is_snap_path(path);
    }
    if path.components().count() != 1 {
        return false;
    }
    let Some(search_path) = std::env::var_os("PATH") else { return false; };
    std::env::split_paths(&search_path)
        .map(|dir| dir.join(path))
        .find(|candidate| candidate.is_file())
        .is_some_and(|candidate| is_snap_path(&candidate))
}

fn resolve_launch_executable(app: &str) -> String {
    if app.to_ascii_lowercase().ends_with(".appimage") {
        let src = Path::new(app);
        if src.is_file() {
            if let Ok(home) = std::env::var("HOME") {
                if !home.is_empty() {
                    if let Some(fname) = src.file_name() {
                        let cache_dir = PathBuf::from(home).join(".cache/focusstage/appimages");
                        if std::fs::create_dir_all(&cache_dir).is_ok() {
                            let dst = cache_dir.join(fname);
                            let need_copy = match (std::fs::metadata(src), std::fs::metadata(&dst)) {
                                (Ok(sm), Ok(dm)) => sm.len() != dm.len(),
                                (Ok(_), Err(_)) => true,
                                _ => false,
                            };
                            if !need_copy || std::fs::copy(src, &dst).is_ok() {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
                                }
                                return dst.display().to_string();
                            }
                        }
                    }
                }
            }
        }
    }
    app.to_string()
}

fn resolve_focusstage_cli_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("focusstage");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("/usr/local/bin/focusstage")
}

fn spawn_stage_process(
    wayland_socket: &str,
    stage: &Stage,
    x11_display: Option<u32>,
) -> std::io::Result<Child> {
    if stage.run_as_root {
        use std::io::Write;
        let resolved_app = resolve_launch_executable(&stage.app);
        let mut spec_cmd = stage_command(wayland_socket, stage, &stage.app, &stage.args, x11_display);
        apply_stage_opener_env(&mut spec_cmd, |key| std::env::var_os(key));
        spec_cmd
            .env("FOCUSSTAGE_STAGE_ID", &stage.id)
            .env(LAUNCH_TOKEN_ENV, stage.launch_token.as_deref().unwrap());

        let effective_args: Vec<String> = spec_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut env_map = HashMap::new();
        for (k, v) in spec_cmd.get_envs() {
            if let (Some(ks), Some(vs)) = (k.to_str(), v.and_then(|val| val.to_str())) {
                if !matches!(ks, "WAYLAND_DISPLAY" | "DISPLAY") {
                    env_map.insert(ks.to_string(), vs.to_string());
                }
            }
        }

        let caller_uid = crate::session_env::current_uid();
        let caller_runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("/run/user/{caller_uid}"));

        let spec = crate::root_exec::RootExecSpec {
            caller_uid,
            caller_runtime_dir,
            wayland_socket: wayland_socket.to_string(),
            x11_display,
            is_chromium_or_electron: is_chromium(stage.kind),
            cwd: stage.launch_cwd.clone(),
            env: env_map,
            app: resolved_app.clone(),
            args: effective_args.clone(),
        };

        let focusstage_bin = resolve_focusstage_cli_path();
        let mut pkexec = Command::new("/usr/bin/pkexec");
        pkexec
            .arg("--disable-internal-agent")
            .arg(&focusstage_bin)
            .arg("root-exec")
            .arg("--")
            .arg(&resolved_app)
            .args(&effective_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        let mut child = pkexec.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let payload = serde_json::to_string(&spec).map_err(std::io::Error::other)?;
            stdin.write_all(payload.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
        }
        return Ok(child);
    }

    let mut cmd = stage_command(wayland_socket, stage, &stage.app, &stage.args, x11_display);
    cmd.stderr(Stdio::inherit());
    spawn_stage_command(&mut cmd, stage)
}

fn stage_command(wayland_socket: &str, stage: &Stage, app: &str, args: &[String], x11_display: Option<u32>) -> Command {
    let resolved_app = resolve_launch_executable(app);
    let mut cmd = Command::new(&resolved_app);
    if app.to_ascii_lowercase().ends_with(".appimage") {
        cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    }
    apply_common_stage_env(&mut cmd, wayland_socket, x11_display);
    if !stage.run_as_root && !stage.persistent_helper {
        crate::session_env::apply_font_preferences(&mut cmd, &stage.launch_env);
    }
    match stage.kind {
        StageKind::Chromium | StageKind::Electron => {
            let platform = chromium_platform(&stage.args, is_wsl2());
            if explicit_chromium_platform(args).is_none() {
                cmd.arg(format!("--ozone-platform={platform}"));
            }
            if platform == "x11" {
                cmd.env("GDK_BACKEND", "x11");
            }
            cmd.arg("--enable-features=UseOzonePlatform");
            const FORCE_A11Y: &str = "--force-renderer-accessibility";
            if !args.iter().take_while(|arg| arg.as_str() != "--")
                .any(|arg| arg.split('=').next() == Some(FORCE_A11Y))
            {
                cmd.arg(FORCE_A11Y);
            }
            if stage.kind == StageKind::Chromium {
                cmd.arg("--no-first-run");
                cmd.arg("--no-default-browser-check");
            }
        }
        StageKind::X11 => {
            cmd.env("GDK_BACKEND", "x11");
            cmd.env("QT_QPA_PLATFORM", "xcb");
        }
        StageKind::Wayland | StageKind::Gecko => {}
    }
    cmd.args(args).envs(&stage.launch_env);
    if !stage.persistent_helper && !stage.run_as_root && is_wsl2() && xdg::hide_wsl_enabled() {
        let existing_preload = stage
            .launch_env
            .get("LD_PRELOAD")
            .cloned()
            .or_else(|| std::env::var("LD_PRELOAD").ok());
        apply_wsl_mask(&mut cmd, &resolved_app, existing_preload.as_deref());
    }
    if let Some(dir) = &stage.launch_cwd {
        cmd.current_dir(dir);
    }
    cmd
}

const WSL_MASK_SO_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/focusstage-wsl-mask.so"));

fn ensure_wsl_mask_so() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn install_mask() -> std::io::Result<PathBuf> {
        let mut builder = tempfile::Builder::new();
        builder
            .prefix("focusstage-wsl-mask-")
            .permissions(std::fs::Permissions::from_mode(0o700));
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| {
                path.is_absolute()
                    && !path.as_os_str().as_bytes().iter().any(|byte| matches!(byte, b' ' | b':'))
            });
        let dir = runtime
            .and_then(|runtime| builder.tempdir_in(runtime).ok())
            .map_or_else(|| builder.tempdir_in("/tmp"), Ok)?;
        let path = dir.path().join("focusstage-wsl-mask.so");
        std::fs::write(&path, WSL_MASK_SO_BYTES)?;
        let _ = dir.keep();
        Ok(path)
    }

    static MASK: OnceLock<Option<PathBuf>> = OnceLock::new();
    MASK.get_or_init(|| match install_mask() {
        Ok(path) => Some(path),
        Err(error) => {
            warn!(%error, "Could not install the WSL compatibility library");
            None
        }
    }).clone()
}

fn apply_wsl_mask(cmd: &mut Command, resolved_app: &str, existing_preload: Option<&str>) {
    cmd.env_remove("WSL_DISTRO_NAME").env_remove("WSL_INTEROP");
    let is_snap = resolved_app.starts_with("/snap/") || resolved_app.starts_with("/var/lib/snapd/")
        || resolved_app == "/usr/bin/snap" || resolved_app == "snap" || is_snap_launcher(resolved_app);
    if !is_snap {
        if let Some(mask_path) = ensure_wsl_mask_so() {
            let mask_str = mask_path.to_string_lossy();
            let merged = match existing_preload.map(str::trim).filter(|s| !s.is_empty()) {
                Some(prev) if !prev.split([':', ' ']).any(|p| p == mask_str.as_ref()) => {
                    format!("{mask_str}:{prev}")
                }
                Some(prev) => prev.to_owned(),
                None => mask_str.into_owned(),
            };
            cmd.env("LD_PRELOAD", merged);
        }
    }
}

const LAUNCH_TOKEN_ENV: &str = "FOCUSSTAGE_LAUNCH_TOKEN";

fn new_launch_token() -> std::io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim().to_owned())
}

fn spawn_stage_command(cmd: &mut Command, stage: &Stage) -> std::io::Result<Child> {
    apply_stage_opener_env(cmd, |key| std::env::var_os(key));
    cmd.env("FOCUSSTAGE_STAGE_ID", &stage.id)
        .env(LAUNCH_TOKEN_ENV, stage.launch_token.as_deref().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

fn apply_common_stage_env(cmd: &mut Command, wayland_socket: &str, x11_display: Option<u32>) {
    cmd.env("WAYLAND_DISPLAY", wayland_socket);
    if let Some(d) = x11_display {
        cmd.env("DISPLAY", format!(":{}", d));
    } else {
        cmd.env_remove("DISPLAY");
    }
    cmd.env("GDK_BACKEND", "wayland");
    cmd.env("QT_QPA_PLATFORM", "wayland");
    cmd.env("MOZ_ENABLE_WAYLAND", "1");
    cmd.env("GTK_A11Y", "atspi");
    cmd.env("QT_ACCESSIBILITY", "1");
    cmd.env("ACCESSIBILITY_ENABLED", "1");
    if kernel_release().contains("microsoft") {
        // Launching a Linux desktop app here is intentional and stdin is closed.
        cmd.env("DONT_PROMPT_WSL_INSTALL", "1");
    }
    if let Some(driver) = wsl_gallium_driver() {
        cmd.env("GALLIUM_DRIVER", driver);
    }

    // Chinese IME support (Fcitx5)
    if std::env::var_os("GTK_IM_MODULE").is_none() {
        cmd.env("GTK_IM_MODULE", "fcitx");
    }
    if std::env::var_os("QT_IM_MODULE").is_none() {
        cmd.env("QT_IM_MODULE", "fcitx");
    }
    if std::env::var_os("XMODIFIERS").is_none() {
        cmd.env("XMODIFIERS", "@im=fcitx");
    }

    // Redirect XDG User Dirs to the Windows user profile that owns this WSL session.
    if let Some(win_home) = crate::desktop::windows_user_home() {
        for (var, sub) in [
            ("XDG_DESKTOP_DIR", "Desktop"),
            ("XDG_DOWNLOAD_DIR", "Downloads"),
            ("XDG_DOCUMENTS_DIR", "Documents"),
            ("XDG_PICTURES_DIR", "Pictures"),
        ] {
            let p = win_home.join(sub);
            if p.is_dir() && std::env::var_os(var).is_none() {
                cmd.env(var, p);
            }
        }
    }
}

fn apply_stage_opener_env(cmd: &mut Command, inherited: impl Fn(&str) -> Option<OsString>) {
    // Without a desktop value xdg-open falls back to WSL explorer.exe and Chromium/Electron
    // skip libsecret; GNOME satisfies xdg-open (gio), portals, and org.freedesktop.secrets.
    for (key, fallback) in [
        ("XDG_CURRENT_DESKTOP", "GNOME"),
        ("BROWSER", "/usr/local/bin/focusstage-open"),
    ] {
        let current = match cmd.get_envs().find(|(name, _)| *name == key) {
            Some((_, value)) => value.map(std::ffi::OsStr::to_os_string),
            None => inherited(key),
        };
        if current.is_none_or(|value| value.is_empty()) {
            cmd.env(key, fallback);
        }
    }
}

fn read_process(pid: u32) -> Option<(u32, bool)> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end_of_name = stat.iter().rposition(|&byte| byte == b')')?;
    let mut fields = std::str::from_utf8(stat.get(end_of_name + 2..)?).ok()?.split_whitespace();
    let live = !matches!(fields.next()?, "Z" | "X" | "x");
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, live))
}

fn read_launch_token(pid: u32) -> Option<String> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let token = env.split(|&byte| byte == 0).find_map(|entry| {
        entry.strip_prefix(LAUNCH_TOKEN_ENV.as_bytes())?.strip_prefix(b"=")
    })?;
    String::from_utf8(token.to_vec()).ok()
}

fn live_stage_pid(stage: &Stage, pid: u32) -> bool {
    read_process(pid).is_some_and(|(_, live)| live)
        && (stage.child.is_some()
            || stage.run_as_root
            || stage.launch_token.is_none()
            || read_launch_token(pid) == stage.launch_token)
}

fn representative_pid(pids: &HashSet<u32>) -> Option<u32> {
    pids.iter().filter_map(|&pid| {
        let (parent, live) = read_process(pid)?;
        live.then_some((pids.contains(&parent), pid))
    }).min().map(|(_, pid)| pid)
}

fn stage_processes(stage: &Stage) -> HashSet<u32> {
    let mut members: HashSet<_> = stage.pid.filter(|&pid| live_stage_pid(stage, pid)).into_iter().collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse() else { continue; };
            let Some((parent, true)) = read_process(pid) else { continue; };
            children.entry(parent).or_default().push(pid);
            if stage.launch_token.is_some() && read_launch_token(pid) == stage.launch_token {
                members.insert(pid);
            }
        }
    }
    let mut pending: Vec<_> = members.iter().copied().collect();
    while let Some(pid) = pending.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if members.insert(child) {
                pending.push(child);
            }
        }
    }
    members
}

fn terminate_stage_processes(stage: &Stage) {
    let Some(token) = stage.launch_token.as_deref() else { return; };
    let processes: Vec<_> = stage_processes(stage).into_iter().filter_map(|pid| {
        // Use a stable process handle so a reused PID cannot receive the signal.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let mut current = pid;
        while current > 1 {
            if (stage.child.is_some() && stage.pid == Some(current))
                || read_launch_token(current).as_deref() == Some(token)
            {
                return Some(fd);
            }
            let (parent, _) = read_process(current)?;
            current = parent;
        }
        None
    }).collect();
    for process in processes {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                process.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SavedRect;
    use smithay::reexports::calloop::EventLoop;
    use smithay::reexports::wayland_server::Display;

    #[test]
    fn stage_opener_defaults_cover_missing_and_empty_values() {
        for inherited in [None, Some(OsString::new())] {
            let mut cmd = Command::new("unused");
            apply_stage_opener_env(&mut cmd, |_| inherited.clone());
            let env: HashMap<_, _> = cmd.get_envs().collect();
            assert_eq!(env[std::ffi::OsStr::new("XDG_CURRENT_DESKTOP")].unwrap(), "GNOME");
            assert_eq!(env[std::ffi::OsStr::new("BROWSER")].unwrap(), "/usr/local/bin/focusstage-open");
        }
    }

    #[test]
    fn stage_opener_preserves_nonempty_settings() {
        let mut cmd = Command::new("unused");
        cmd.env("BROWSER", "firefox");
        apply_stage_opener_env(&mut cmd, |_| Some("XFCE".into()));
        let env: Vec<_> = cmd.get_envs().collect();
        assert_eq!(env, [(std::ffi::OsStr::new("BROWSER"), Some(std::ffi::OsStr::new("firefox")))]);

        cmd.env("BROWSER", "");
        apply_stage_opener_env(&mut cmd, |_| Some("XFCE".into()));
        let env: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(env[std::ffi::OsStr::new("BROWSER")].unwrap(), "/usr/local/bin/focusstage-open");
    }

    #[test]
    fn renderer_accessibility_flag_is_not_duplicated() {
        let flag = "--force-renderer-accessibility";
        for kind in [StageKind::Chromium, StageKind::Electron] {
            let stage = Stage::new("stage-1".into(), "browser".into(), vec![], kind,
                640, 480, DisplayState::Hidden, 1.0);
            let default = stage_command("unused", &stage, &stage.app, &[], None);
            assert_eq!(default.get_args().filter(|&arg| arg == flag).count(), 1);
            let configured = format!("{flag}=basic");
            let explicit = stage_command("unused", &stage, &stage.app, &[configured.clone()], None);
            assert!(explicit.get_args().any(|arg| arg == configured.as_str()));
            assert!(!explicit.get_args().any(|arg| arg == flag));
        }
    }

    #[test]
    fn handoff_requires_one_human_stage() {
        let stage = |id: &str, owner| {
            let mut stage = Stage::new(id.into(), "chromium".into(), vec![],
                StageKind::Chromium, 640, 480, DisplayState::Desktop, 1.0);
            stage.input_owner = owner;
            stage
        };
        let agent = stage("stage-1", InputOwner::Agent { lease_id: "test".into(), agent_name: None });
        let frozen = stage("stage-2", InputOwner::Frozen);
        let human = stage("stage-3", InputOwner::Human);
        assert!(select_handoff_stage([&agent].into_iter()).is_err());
        assert!(select_handoff_stage([&frozen].into_iter()).is_err());
        assert!(select_handoff_stage([&agent, &human].into_iter()).is_err());
        assert_eq!(select_handoff_stage([&human].into_iter()).unwrap().unwrap().id, human.id);
        assert!(select_handoff_stage(std::iter::empty()).unwrap().is_none());
    }

    #[test]
    fn app_launch_uses_native_profile_and_preserves_explicit_arguments() {
        for (kind, profile) in [
            (StageKind::Chromium, vec!["--user-data-dir=/tmp/test profile".into()]),
            (StageKind::Electron, vec!["--user-data-dir".into(), "/tmp/test profile".into()]),
            (StageKind::Gecko, vec!["--profile".into(), "/tmp/test profile".into()]),
        ] {
            let mut stage = Stage::new("stage-1".into(), "app".into(), vec![], kind,
                640, 480, DisplayState::Desktop, 1.0);
            let default = stage_command("unused", &stage, &stage.app, &[], None);
            let default_args: Vec<_> = default.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            assert!(profile_arguments(kind, &default_args).unwrap().is_empty());
            let explicit = stage_command("unused", &stage, &stage.app, &profile, None);
            let explicit_args: Vec<_> = explicit.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            assert!(explicit_args.ends_with(&profile));

            stage.args = profile.clone();
            let mut handoff = vec!["https://example.com/".into()];
            prepare_handoff_args(&stage, &mut handoff, None).unwrap();
            assert_eq!(&handoff[..profile.len()], profile.as_slice());
            prepare_handoff_args(&stage, &mut handoff, None).unwrap();
            assert_eq!(handoff.len(), profile.len() + 1);
            assert!(prepare_handoff_args(&stage, &mut handoff, Some("/other")).is_err());
        }
    }

    #[test]
    fn non_remote_gecko_modes_are_preserved_at_launch_and_rejected_on_handoff() {
        let args = vec!["-no-remote".into()];
        let stage = Stage::new("stage-1".into(), "firefox".into(), args.clone(), StageKind::Gecko,
            640, 480, DisplayState::Desktop, 1.0);
        let command = stage_command("unused", &stage, &stage.app, &args, None);
        assert!(command.get_args().any(|arg| arg == "-no-remote"));
        assert!(prepare_handoff_args(&stage, &mut vec!["https://example.com/".into()], None).is_err());
    }

    #[test]
    fn detached_process_keeps_its_stage_after_launcher_exit() {
        let event_loop = EventLoop::try_new().unwrap();
        let display = Display::<FocusStageState>::new().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let mut state = FocusStageState::new(
            &display.handle(), event_loop.handle(), "unused".into(), 1.0, 60,
            SavedRect { x: 0, y: 0, width: 640, height: 480 }, None,
        );
        let pid_file = runtime.path().join("worker.pid");
        let StageLaunch { stage_id: id, pid: launcher_pid, .. } = state.launch_stage(
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 10 & printf '%s\\n' \"$!\" > \"$1\"".into(),
                 "sh".into(), pid_file.to_string_lossy().into_owned()],
            Some(StageKind::Wayland), HashMap::new(), None, None, None, None, None, None, false,
        ).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.stages[&id].child.is_some() && Instant::now() < deadline {
            state.reap_exited_stages();
            std::thread::sleep(Duration::from_millis(10));
        }
        let worker: u32 = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
        assert!(state.stages[&id].child.is_none());
        assert_ne!(read_process(worker).unwrap().0, launcher_pid.unwrap());
        state.stages.get_mut(&id).unwrap().next_process_check = Some(Instant::now());
        state.reap_exited_stages();
        assert_eq!(state.resolve_stage_for_pid(worker).as_deref(), Some(id.as_str()));
        assert_eq!(state.stages[&id].info().pid, Some(worker));
        assert!(state.collect_stage_pids(&id).contains(&worker));

        state.close_stage(&id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while read_process(worker).is_some_and(|(_, live)| live) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!read_process(worker).is_some_and(|(_, live)| live));
        assert!(!runtime.path().join("profiles").exists());
    }

    #[test]
    fn persistent_helper_stage_is_hidden_when_idle_and_does_not_retire_on_window_close() {
        use crate::protocol::ControlRequest;
        let event_loop = EventLoop::try_new().unwrap();
        let display = Display::<FocusStageState>::new().unwrap();
        let mut state = FocusStageState::new(
            &display.handle(),
            event_loop.handle(),
            "unused".into(),
            1.0,
            60,
            SavedRect { x: 0, y: 0, width: 640, height: 480 },
            None,
        );
        let mut stage = Stage::new(
            "stage-polkit-agent".into(),
            "polkit-gnome".into(),
            Vec::new(),
            StageKind::X11,
            480,
            240,
            DisplayState::Desktop,
            1.0,
        );
        stage.persistent_helper = true;
        state.stages.insert("stage-polkit-agent".into(), stage);

        let (tx, rx) = std::sync::mpsc::channel();
        state.handle_control_message(crate::control::ControlServerMessage {
            request: ControlRequest::List,
            reply_tx: tx,
            peer_pid: None,
        });
        match rx.recv().unwrap() {
            ControlResponse::Stages { stages } => assert!(stages.is_empty()),
            other => panic!("unexpected list response: {:?}", other),
        }

        // Removing a host-closed window from a persistent_helper stage must not mark retire_when_empty.
        assert!(!state.stages["stage-polkit-agent"].retire_when_empty);
    }

    #[test]
    fn keyring_prompter_systemd_args_forward_gcr_persist_and_x11_isolation() {
        let mut stage = Stage::new(
            "stage-keyring-prompter".into(),
            "/usr/libexec/gcr-prompter".into(),
            Vec::new(),
            StageKind::X11,
            480,
            240,
            DisplayState::Desktop,
            1.0,
        );
        stage.persistent_helper = true;
        stage.helper_unit = Some("focusstage-gcr-prompter".into());
        stage.launch_token = Some("tok-123".into());
        stage.launch_env.insert("GCR_PERSIST".into(), "1".into());

        let args = build_helper_systemd_args(&stage, "focusstage-gcr-prompter", 4, "GNOME");
        assert!(args.contains(&"--unit=focusstage-gcr-prompter".to_string()));
        assert!(args.contains(&"DISPLAY=:4".to_string()));
        assert!(args.contains(&"GCR_PERSIST=1".to_string()));
        assert!(args.contains(&"GDK_BACKEND=x11".to_string()));
        assert!(args.contains(&"WAYLAND_DISPLAY".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("/usr/libexec/gcr-prompter"));
    }

    #[test]
    fn wsl_mask_hides_detection_variables_and_preserves_windows_path() {
        let path = "/usr/bin:/mnt/c/Windows/System32/WindowsPowerShell/v1.0";
        let mut cmd = Command::new("/usr/bin/env");
        cmd.env("PATH", path)
            .env("WSL_DISTRO_NAME", "Ubuntu-24.04")
            .env("WSL_INTEROP", "/run/WSL/42_interop");
        apply_wsl_mask(&mut cmd, "/opt/T3 Code/t3code", None);
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(envs[std::ffi::OsStr::new("WSL_DISTRO_NAME")], None);
        assert_eq!(envs[std::ffi::OsStr::new("WSL_INTEROP")], None);
        assert_eq!(envs[std::ffi::OsStr::new("PATH")], Some(std::ffi::OsStr::new(path)));
        let mask = ensure_wsl_mask_so().unwrap();
        assert_eq!(envs[std::ffi::OsStr::new("LD_PRELOAD")], Some(mask.as_os_str()));
    }

    #[test]
    fn wsl_mask_removes_inherited_detection_variables_for_snap_without_replacing_preload() {
        let mut cmd = Command::new("/usr/bin/snap");
        let preload = "/usr/lib/existing.so";
        cmd.env("LD_PRELOAD", preload);
        apply_wsl_mask(&mut cmd, "/usr/bin/snap", Some(preload));
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(envs[std::ffi::OsStr::new("WSL_DISTRO_NAME")], None);
        assert_eq!(envs[std::ffi::OsStr::new("WSL_INTEROP")], None);
        assert_eq!(envs[std::ffi::OsStr::new("LD_PRELOAD")], Some(std::ffi::OsStr::new(preload)));
    }

    #[test]
    fn wsl_compatibility_library_preserves_proc_open_flags() {
        const PROBE: &str = "FOCUSSTAGE_TEST_WSL_PROC_PROBE";
        const INTEROP_PROBE: &str = "FOCUSSTAGE_TEST_WSL_INTEROP_PATHS";
        let visible_interop_paths = || {
            [
                "/run/WSL",
                "/proc/sys/fs/binfmt_misc/WSLInterop",
                "/proc/sys/fs/binfmt_misc/WSLInterop-late",
            ]
            .into_iter()
            .map(|path| if Path::new(path).exists() { '1' } else { '0' })
            .collect::<String>()
        };
        if let Some(raw_release) = std::env::var_os(PROBE) {
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;

            let path = "/proc/sys/kernel/osrelease";
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .unwrap();
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
            assert_ne!(flags & libc::O_NONBLOCK, 0);
            let fd_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(fd_flags, -1);
            assert_ne!(fd_flags & libc::FD_CLOEXEC, 0);
            let mut release = String::new();
            file.read_to_string(&mut release).unwrap();
            assert_ne!(release.trim(), raw_release.to_string_lossy());
            assert!(!release.to_ascii_lowercase().contains("microsoft"));

            let path_only = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH)
                .open(path)
                .unwrap();
            let flags = unsafe { libc::fcntl(path_only.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::O_PATH, 0);
            assert_eq!(visible_interop_paths(), std::env::var(INTEROP_PROBE).unwrap());
            return;
        }

        if is_wsl2() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "manager::tests::wsl_compatibility_library_preserves_proc_open_flags"])
                .env(PROBE, kernel_release().trim())
                .env(INTEROP_PROBE, visible_interop_paths())
                .env("LD_PRELOAD", ensure_wsl_mask_so().unwrap())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}\n{stderr}");
        }
    }
}
