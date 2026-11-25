use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::session_env::UserAccount;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootExecSpec {
    pub caller_uid: u32,
    pub caller_runtime_dir: String,
    pub wayland_socket: String,
    #[serde(default)]
    pub x11_display: Option<u32>,
    #[serde(default)]
    pub is_chromium_or_electron: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub app: String,
    #[serde(default)]
    pub args: Vec<String>,
}

pub fn run_from_stdin() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("focusstage root-exec 需要以 root 身份运行。".into());
    }

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| format!("读取 root-exec 配置失败: {e}"))?;
    let spec: RootExecSpec =
        serde_json::from_str(raw.trim()).map_err(|e| format!("解析 root-exec 配置失败: {e}"))?;

    redirect_stdin_to_dev_null();

    let runtime_dir = prepare_root_runtime_dir(&spec.caller_runtime_dir, &spec.wayland_socket)?;
    let root_acct = UserAccount::lookup(0);
    let root_home = root_acct
        .as_ref()
        .map(|a| a.home.clone())
        .unwrap_or_else(|| PathBuf::from("/root"));
    let root_shell = root_acct
        .as_ref()
        .map(|a| a.shell.clone())
        .unwrap_or_else(|| PathBuf::from("/bin/bash"));

    std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_DATA_HOME");
    std::env::remove_var("XDG_CACHE_HOME");
    std::env::remove_var("XDG_STATE_HOME");
    std::env::remove_var("XAUTHORITY");

    std::env::set_var("HOME", &root_home);
    std::env::set_var("USER", "root");
    std::env::set_var("LOGNAME", "root");
    std::env::set_var("SHELL", &root_shell);
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("WAYLAND_DISPLAY", &spec.wayland_socket);

    ensure_root_path();

    for (key, val) in &spec.env {
        if matches!(
            key.as_str(),
            "HOME"
                | "USER"
                | "LOGNAME"
                | "SHELL"
                | "XDG_RUNTIME_DIR"
                | "WAYLAND_DISPLAY"
                | "DBUS_SESSION_BUS_ADDRESS"
        ) {
            continue;
        }
        std::env::set_var(key, val);
    }

    if let Some(d) = spec.x11_display {
        std::env::set_var("DISPLAY", format!(":{d}"));
    } else {
        std::env::remove_var("DISPLAY");
    }

    if let Some(cwd) = spec.cwd.as_deref().filter(|s| !s.is_empty()) {
        if Path::new(cwd).is_dir() {
            let _ = std::env::set_current_dir(cwd);
        }
    } else if root_home.is_dir() {
        let _ = std::env::set_current_dir(&root_home);
    }

    let final_args = build_effective_args(&spec.args, spec.is_chromium_or_electron);

    let mut cmd = if Path::new("/usr/bin/dbus-run-session").is_file() {
        let mut c = Command::new("/usr/bin/dbus-run-session");
        c.arg("--").arg(&spec.app).args(&final_args);
        c
    } else {
        let mut c = Command::new(&spec.app);
        c.args(&final_args);
        c
    };

    let err = cmd.exec();
    Err(format!("以 root 启动 '{}' 失败: {err}", spec.app))
}

pub fn run_root_shell() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("focusstage root-shell 需要以 root 身份运行。".into());
    }
    let root_acct = UserAccount::lookup(0);
    let root_home = root_acct
        .as_ref()
        .map(|a| a.home.clone())
        .unwrap_or_else(|| PathBuf::from("/root"));
    let root_shell = root_acct
        .as_ref()
        .map(|a| a.shell.clone())
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/bash"));

    std::env::set_var("HOME", &root_home);
    std::env::set_var("USER", "root");
    std::env::set_var("LOGNAME", "root");
    std::env::set_var("SHELL", &root_shell);
    std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    std::env::remove_var("XDG_RUNTIME_DIR");
    ensure_root_path();

    if root_home.is_dir() {
        let _ = std::env::set_current_dir(&root_home);
    }

    let err = Command::new(&root_shell).arg("-l").exec();
    Err(format!("启动 root shell '{}' 失败: {err}", root_shell.display()))
}

fn redirect_stdin_to_dev_null() {
    if let Ok(devnull) = fs::File::open("/dev/null") {
        unsafe {
            libc::dup2(devnull.as_raw_fd(), 0);
        }
    }
}

fn ensure_root_path() {
    let required = [
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        "/snap/bin",
    ];
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<PathBuf> = std::env::split_paths(&current).collect();
    for dir in required.iter().rev() {
        let p = PathBuf::from(dir);
        if !parts.iter().any(|existing| existing == &p) {
            parts.insert(0, p);
        }
    }
    if let Ok(joined) = std::env::join_paths(parts) {
        std::env::set_var("PATH", joined);
    }
}

fn prepare_root_runtime_dir(caller_runtime_dir: &str, wayland_socket: &str) -> Result<PathBuf, String> {
    if wayland_socket.is_empty()
        || wayland_socket.contains('/')
        || wayland_socket.contains('\\')
        || wayland_socket == "."
        || wayland_socket == ".."
    {
        return Err("非法的 Wayland 套接字名称。".into());
    }

    let runtime_dir = PathBuf::from("/run/focusstage-root-runtime");
    fs::create_dir_all(&runtime_dir)
        .map_err(|e| format!("创建 root 运行目录 {} 失败: {e}", runtime_dir.display()))?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("设置 {} 权限失败: {e}", runtime_dir.display()))?;

    let target_socket = Path::new(caller_runtime_dir).join(wayland_socket);
    let link_path = runtime_dir.join(wayland_socket);
    if fs::symlink_metadata(&link_path).is_ok() {
        let _ = fs::remove_file(&link_path);
    }
    symlink(&target_socket, &link_path).map_err(|e| {
        format!(
            "链接 Wayland 套接字 {} -> {} 失败: {e}",
            link_path.display(),
            target_socket.display()
        )
    })?;

    Ok(runtime_dir)
}

pub fn build_effective_args(args: &[String], is_chromium_or_electron: bool) -> Vec<String> {
    let mut out = args.to_vec();
    if is_chromium_or_electron
        && !out
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "--no-sandbox")
    {
        out.insert(0, "--no-sandbox".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_args_adds_no_sandbox_for_chromium_when_missing() {
        let args = vec!["--new-window".to_string()];
        assert_eq!(
            build_effective_args(&args, true),
            vec!["--no-sandbox".to_string(), "--new-window".to_string()]
        );
        assert_eq!(build_effective_args(&args, false), args);

        let already = vec!["--no-sandbox".to_string(), "--incognito".to_string()];
        assert_eq!(build_effective_args(&already, true), already);
    }
}
