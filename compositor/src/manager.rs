use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing::{info, warn};

use crate::protocol::{infer_stage_kind, DisplayState, StageKind};
use crate::sink::DisplaySink;
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
    ) -> Result<(String, Option<u32>), String> {
        self.next_stage_seq += 1;
        let stage_id = format!("stage-{}", self.next_stage_seq);
        let kind = kind_opt.unwrap_or_else(|| infer_stage_kind(&app));
        let w = width.unwrap_or(1280).max(64);
        let h = height.unwrap_or(800).max(64);

        let default_display = if show_on_desktop.unwrap_or(self.wslg_sink.is_some()) {
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

        let profile_dir = self.runtime_base_dir.join("profiles").join(&stage_id);
        let _ = std::fs::create_dir_all(&profile_dir);
        stage.user_data_dir = Some(profile_dir.clone());

        if kind == StageKind::X11 {
            stage.pending_x11_launch = Some(PendingX11Launch {
                app,
                args,
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

        let child = spawn_wayland_stage_process(
            &self.wayland_socket_name,
            &stage_id,
            &app,
            &args,
            kind,
            &profile_dir,
            &extra_env,
            cwd.as_deref(),
        )?;
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

        let mut cmd = Command::new(&p.app);
        cmd.args(&p.args);
        apply_common_stage_env(&mut cmd, &self.wayland_socket_name, Some(display_number));
        cmd.env("GDK_BACKEND", "x11");
        cmd.env("QT_QPA_PLATFORM", "xcb");
        for (k, v) in &p.env {
            cmd.env(k, v);
        }
        if let Some(ref dir) = p.cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                if let Some(stage) = self.stages.get_mut(stage_id) {
                    stage.pid = Some(pid);
                    stage.child = Some(child);
                }
                info!("Launched X11 client for {} on DISPLAY=:{} (pid={})", stage_id, display_number, pid);
            }
            Err(err) => {
                warn!("Failed to launch X11 app '{}' for {}: {}", p.app, stage_id, err);
            }
        }
    }

    pub fn close_stage(&mut self, stage_id: &str) -> Result<(), String> {
        let mut stage = self
            .stages
            .remove(stage_id)
            .ok_or_else(|| format!("Stage '{}' not found", stage_id))?;

        if let Some(ref mut child) = stage.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(token) = stage.xwayland_token.take() {
            self.loop_handle.remove(token);
        }
        if let Some(xwm_id) = stage.xwm_id {
            self.xwms.remove(&xwm_id);
            self.xwm_to_stage.remove(&xwm_id);
            self.x11_override_windows.remove(&xwm_id);
        }
        if let Some(ref mut sink) = self.wslg_sink {
            sink.destroy(stage_id);
        }
        if let Some(dir) = stage.user_data_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    pub fn reap_exited_stages(&mut self) {
        let mut exited = Vec::new();
        for (id, stage) in &mut self.stages {
            if let Some(ref mut child) = stage.child {
                if let Ok(Some(status)) = child.try_wait() {
                    info!("Stage {} process exited with {}", id, status);
                    exited.push(id.clone());
                }
            }
        }
        for id in exited {
            if let Some(mut stage) = self.stages.remove(&id) {
                if let Some(token) = stage.xwayland_token.take() {
                    self.loop_handle.remove(token);
                }
                if let Some(xwm_id) = stage.xwm_id {
                    self.xwms.remove(&xwm_id);
                    self.xwm_to_stage.remove(&xwm_id);
                    self.x11_override_windows.remove(&xwm_id);
                }
                if let Some(ref mut sink) = self.wslg_sink {
                    sink.destroy(&id);
                }
                if let Some(dir) = stage.user_data_dir.take() {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
        }
    }

    /// Map a connecting Wayland client's PID (via `/proc` parent walk) to its StageId.
    pub fn resolve_stage_for_pid(&self, mut pid: u32) -> Option<String> {
        let mut visited = HashSet::new();
        while pid > 1 && visited.insert(pid) {
            for (id, stage) in &self.stages {
                if stage.pid == Some(pid) {
                    return Some(id.clone());
                }
            }
            pid = read_ppid(pid)?;
        }
        None
    }

    /// Collect all PIDs belonging to a Stage (root pid + descendants) for AT-SPI matching.
    pub fn collect_stage_pids(&self, stage_id: &str) -> HashSet<u32> {
        let mut pids = HashSet::new();
        let Some(root_pid) = self.stages.get(stage_id).and_then(|s| s.pid) else {
            return pids;
        };
        pids.insert(root_pid);

        if let Ok(entries) = std::fs::read_dir("/proc") {
            let mut parent_map: HashMap<u32, Vec<u32>> = HashMap::new();
            for entry in entries.flatten() {
                if let Ok(p) = entry.file_name().to_string_lossy().parse::<u32>() {
                    if let Some(ppid) = read_ppid(p) {
                        parent_map.entry(ppid).or_default().push(p);
                    }
                }
            }
            let mut queue = vec![root_pid];
            while let Some(cur) = queue.pop() {
                if let Some(children) = parent_map.get(&cur) {
                    for &c in children {
                        if pids.insert(c) {
                            queue.push(c);
                        }
                    }
                }
            }
        }
        pids
    }
}

fn spawn_wayland_stage_process(
    wayland_socket: &str,
    stage_id: &str,
    app: &str,
    args: &[String],
    kind: StageKind,
    profile_dir: &Path,
    extra_env: &HashMap<String, String>,
    cwd: Option<&str>,
) -> Result<std::process::Child, String> {
    let mut cmd = Command::new(app);

    match kind {
        StageKind::Chromium | StageKind::Electron => {
            cmd.arg("--ozone-platform=wayland");
            cmd.arg("--enable-features=UseOzonePlatform");
            cmd.arg("--force-renderer-accessibility");
            cmd.arg(format!("--user-data-dir={}", profile_dir.display()));
            if kind == StageKind::Chromium {
                cmd.arg("--no-first-run");
                cmd.arg("--no-default-browser-check");
            }
        }
        StageKind::Wayland | StageKind::X11 => {}
    }

    cmd.args(args);
    apply_common_stage_env(&mut cmd, wayland_socket, None);
    cmd.env("FOCUSSTAGE_STAGE_ID", stage_id);

    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    cmd.spawn()
        .map_err(|e| format!("failed to spawn '{}': {e}", app))
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

fn read_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 2..)?;
    let mut parts = rest.split_whitespace();
    let _state = parts.next()?;
    let ppid_str = parts.next()?;
    ppid_str.parse::<u32>().ok()
}
