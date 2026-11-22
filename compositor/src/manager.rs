use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::protocol::{infer_stage_kind, DisplayState, StageKind};
use crate::stage::{PendingX11Launch, Stage};
use crate::state::FocusStageState;

impl FocusStageState {
    pub fn launch_stage(
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
    ) -> Result<(String, Option<u32>), String> {
        self.next_stage_seq += 1;
        let stage_id = format!("stage-{}", self.next_stage_seq);
        let kind = kind_opt.unwrap_or_else(|| infer_stage_kind(&app));
        let w = width.unwrap_or(1280).max(64);
        let h = height.unwrap_or(800).max(64);

        let default_display = if show_on_desktop.unwrap_or(self.display_sink.is_some()) {
            DisplayState::Desktop
        } else {
            DisplayState::Hidden
        };

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
        if let Some(cls) = wm_class {
            let trimmed = cls.trim();
            if !trimmed.is_empty() {
                stage.wm_class = trimmed.to_string();
            }
        }

        stage.launch_token = Some(new_launch_token().map_err(|err| format!("create Stage launch token: {err}"))?);
        let profile_dir = self.runtime_base_dir.join("profiles").join(&stage_id);
        std::fs::create_dir_all(&profile_dir)
            .map_err(|err| format!("create Stage profile: {err}"))?;
        stage.user_data_dir = Some(profile_dir.clone());

        // Snap wrappers can select X11 after we pass the default Wayland flags.
        if kind == StageKind::X11
            || (matches!(kind, StageKind::Electron | StageKind::Chromium) && is_snap_launcher(&app))
        {
            stage.pending_x11_launch = Some(PendingX11Launch {
                env: extra_env,
                cwd,
            });
            self.stages.insert(stage_id.clone(), stage);
            if let Err(err) = self.spawn_xwayland_for_stage(stage_id.clone()) {
                if let Some(removed) = self.stages.remove(&stage_id) {
                    if let Some(dir) = removed.user_data_dir {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
                return Err(err);
            }
            return Ok((stage_id, None));
        }

        let child = match spawn_stage_process(
            &self.wayland_socket_name,
            &stage,
            None,
            &extra_env,
            cwd.as_deref(),
        ) {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&profile_dir);
                return Err(format!("failed to spawn '{app}': {err}"));
            }
        };
        let pid = child.id();
        stage.pid = Some(pid);
        stage.child = Some(child);
        self.stages.insert(stage_id.clone(), stage);
        info!("Launched {} Stage {} (pid={})", format!("{:?}", kind), stage_id, pid);
        Ok((stage_id, Some(pid)))
    }

    pub fn launch_pending_x11_client(&mut self, stage_id: &str, display_number: u32) {
        let pending = self
            .stages
            .get_mut(stage_id)
            .and_then(|s| s.pending_x11_launch.take());
        let Some(p) = pending else {
            return;
        };

        let stage = self.stages.get_mut(stage_id).unwrap();
        match spawn_stage_process(
            &self.wayland_socket_name,
            stage,
            Some(display_number),
            &p.env,
            p.cwd.as_deref(),
        ) {
            Ok(child) => {
                let pid = child.id();
                stage.pid = Some(pid);
                stage.child = Some(child);
                info!(stage = stage_id, kind = ?stage.kind, display_number, pid, "Launched Stage with XWayland");
            }
            Err(err) => {
                warn!("Failed to launch '{}' for {}: {}", stage.app, stage_id, err);
                let _ = self.close_stage(stage_id);
            }
        }
    }

    pub fn close_stage(&mut self, stage_id: &str) -> Result<(), String> {
        let mut stage = self
            .stages
            .remove(stage_id)
            .ok_or_else(|| format!("Stage '{}' not found", stage_id))?;

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
        if let Some(dir) = stage.user_data_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    pub fn reap_exited_stages(&mut self) {
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
                    stage.child = None;
                    stage.pid = None;
                    // A launcher can exit before its GUI connects or even finishes exec().
                    stage.next_process_check = Some(now + Duration::from_secs(2));
                }
            } else if let Some(next_check) = stage.next_process_check {
                if now >= next_check && !stage.has_live_surface() {
                    if stage_processes(stage).is_empty() {
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
                stage.pid == Some(pid) || (token.is_some() && stage.launch_token == token)
            }) {
                return Some(stage.id.clone());
            }
            pid = read_process(pid)?.0;
        }
        None
    }

    /// Include detached GUI processes and their descendants in AT-SPI matching.
    pub fn collect_stage_pids(&self, stage_id: &str) -> HashSet<u32> {
        self.stages.get(stage_id).map(stage_processes).unwrap_or_default()
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

fn spawn_stage_process(
    wayland_socket: &str,
    stage: &Stage,
    x11_display: Option<u32>,
    extra_env: &HashMap<String, String>,
    cwd: Option<&str>,
) -> std::io::Result<Child> {
    let resolved_app = resolve_launch_executable(&stage.app);
    let mut cmd = Command::new(&resolved_app);
    if stage.app.to_ascii_lowercase().ends_with(".appimage") {
        cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    }

    apply_common_stage_env(&mut cmd, wayland_socket, x11_display);
    match stage.kind {
        StageKind::Chromium | StageKind::Electron => {
            let profile_dir = stage.user_data_dir.as_ref().unwrap();
            cmd.arg("--ozone-platform=wayland");
            cmd.arg("--enable-features=UseOzonePlatform");
            cmd.arg("--force-renderer-accessibility");
            cmd.arg(format!("--user-data-dir={}", profile_dir.display()));
            if stage.kind == StageKind::Chromium {
                cmd.arg("--no-first-run");
                cmd.arg("--no-default-browser-check");
            }
        }
        StageKind::X11 => {
            cmd.env("GDK_BACKEND", "x11");
            cmd.env("QT_QPA_PLATFORM", "xcb");
        }
        StageKind::Wayland => {}
    }

    cmd.args(&stage.args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    spawn_stage_command(&mut cmd, stage)
}

const LAUNCH_TOKEN_ENV: &str = "FOCUSSTAGE_LAUNCH_TOKEN";

fn new_launch_token() -> std::io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim().to_owned())
}

fn spawn_stage_command(cmd: &mut Command, stage: &Stage) -> std::io::Result<Child> {
    cmd.env("FOCUSSTAGE_STAGE_ID", &stage.id)
        .env(LAUNCH_TOKEN_ENV, stage.launch_token.as_deref().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
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

    static IS_WSL: OnceLock<bool> = OnceLock::new();
    if *IS_WSL.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
    }) {
        // Launching a Linux desktop app here is intentional and stdin is closed.
        cmd.env("DONT_PROMPT_WSL_INSTALL", "1");
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

    // Redirect XDG User Dirs to Windows user directories when /mnt/c/Users/<user> exists
    if let Some(win_home) = detect_windows_user_dir() {
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

fn detect_windows_user_dir() -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir("/mnt/c/Users") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(s.as_ref(), "Public" | "Default" | "Default User" | "All Users") {
                continue;
            }
            let path = entry.path();
            if path.join("Downloads").is_dir() && path.join("Desktop").is_dir() {
                return Some(path);
            }
        }
    }
    None
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

fn stage_processes(stage: &Stage) -> HashSet<u32> {
    let mut members: HashSet<_> = stage.pid.into_iter().collect();
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
            if stage.pid == Some(current) || read_launch_token(current).as_deref() == Some(token) {
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
    fn detached_process_keeps_its_stage_after_launcher_exit() {
        let event_loop = EventLoop::try_new().unwrap();
        let display = Display::<FocusStageState>::new().unwrap();
        let runtime = std::env::temp_dir().join(format!("focusstage-test-{}", new_launch_token().unwrap()));
        let mut state = FocusStageState::new(
            &display.handle(), event_loop.handle(), "unused".into(), runtime.clone(), 1.0, 60,
            SavedRect { x: 0, y: 0, width: 640, height: 480 }, None,
        );
        let pid_file = runtime.join("worker.pid");
        let (id, launcher_pid) = state.launch_stage(
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 10 & printf '%s\\n' \"$!\" > \"$1\"".into(),
                 "sh".into(), pid_file.to_string_lossy().into_owned()],
            Some(StageKind::Wayland), HashMap::new(), None, None, None, None, None,
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
        assert!(state.collect_stage_pids(&id).contains(&worker));

        state.close_stage(&id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while read_process(worker).is_some_and(|(_, live)| live) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!read_process(worker).is_some_and(|(_, live)| live));
        assert!(!runtime.join("profiles").join(id).exists());
        std::fs::remove_dir_all(runtime).unwrap();
    }
}
