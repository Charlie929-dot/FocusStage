use std::path::{Path, PathBuf};
use std::process::Command;

use focusstage_compositor::control::ControlClient;
use focusstage_compositor::desktop::WRAPPER_PREFIX;
use focusstage_compositor::protocol::DesktopAppInfo;
use focusstage_mcp::apps::discover_desktop_apps_via_daemon;
use serde_json::json;

#[derive(Debug)]
struct UninstallError {
    code: &'static str,
    message: String,
}

impl UninstallError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct Outcome {
    action: &'static str,
    message: String,
    removed: Vec<String>,
}

pub fn run(app_id: &str, sock: &Path, json_mode: bool) -> anyhow::Result<()> {
    let payload = match uninstall(app_id, sock) {
        Ok(outcome) => json!({
            "status": "ok",
            "app_id": app_id,
            "action": outcome.action,
            "message": outcome.message,
            "removed": outcome.removed,
        }),
        Err(err) => json!({
            "status": "error",
            "code": err.code,
            "message": err.message,
        }),
    };
    let failed = payload.get("status").and_then(|value| value.as_str()) == Some("error");
    println!(
        "{}",
        if json_mode {
            payload.to_string()
        } else {
            serde_json::to_string_pretty(&payload)?
        }
    );
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn uninstall(app_id: &str, sock: &Path) -> Result<Outcome, UninstallError> {
    let client = ControlClient::new(sock.to_path_buf());
    let apps = discover_desktop_apps_via_daemon(Some(&client));
    let app = apps
        .iter()
        .find(|candidate| candidate.id == app_id)
        .ok_or_else(|| {
            UninstallError::new("not_found", format!("Application {app_id} not found"))
        })?;
    let path = app
        .file_path
        .as_deref()
        .ok_or_else(|| UninstallError::new("unsupported", "Application has no source file"))?;
    match app.source.as_str() {
        "deb" => delete_installer(path),
        "appimage" => delete_appimage(path),
        "desktop" => uninstall_desktop_app(app, path),
        source => Err(UninstallError::new(
            "unsupported",
            format!("Unsupported app source: {source}"),
        )),
    }
}

fn delete_installer(path: &str) -> Result<Outcome, UninstallError> {
    remove_file(path)?;
    Ok(Outcome {
        action: "delete-package",
        message: format!("已删除安装包 {}", display_name(path)),
        removed: vec![path.to_string()],
    })
}

fn delete_appimage(path: &str) -> Result<Outcome, UninstallError> {
    let mut removed = Vec::new();
    remove_appimage_files(path, &mut removed)?;
    Ok(Outcome {
        action: "delete-appimage",
        message: format!("已删除 AppImage {}", display_name(path)),
        removed,
    })
}

fn uninstall_desktop_app(
    app: &DesktopAppInfo,
    path: &str,
) -> Result<Outcome, UninstallError> {
    let (action, mut removed) = if let Some(managed) = remove_managed_package(path)? {
        managed
    } else if app.cmd.to_ascii_lowercase().ends_with(".appimage") {
        let mut files = Vec::new();
        remove_appimage_files(&app.cmd, &mut files)?;
        remove_file(path)?;
        files.push(path.to_string());
        ("delete-appimage", files)
    } else {
        return Err(UninstallError::new(
            "unsupported",
            format!("Cannot determine package manager for {path}"),
        ));
    };

    for wrapper in wrapper_paths(&app.stem) {
        if !wrapper.is_file() {
            continue;
        }
        let removed_ok = if std::fs::remove_file(&wrapper).is_ok() {
            true
        } else if wrapper.starts_with("/usr/share/applications") {
            let path_str = wrapper.to_string_lossy().into_owned();
            run_root(&["rm", "-f", path_str.as_str()]).is_ok() && !wrapper.exists()
        } else {
            false
        };
        if removed_ok {
            removed.push(wrapper.display().to_string());
        }
    }

    Ok(Outcome {
        action,
        message: format!("已卸载 {}", app.name),
        removed,
    })
}

fn remove_managed_package(
    path: &str,
) -> Result<Option<(&'static str, Vec<String>)>, UninstallError> {
    if let Some(pkg) = dpkg_package(path)? {
        run_root(&[
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "apt-get",
            "remove",
            "-y",
            pkg.as_str(),
        ])?;
        ensure_desktop_removed(path, &format!("apt-get remove {pkg}"))?;
        return Ok(Some(("apt-remove", vec![path.to_string()])));
    }
    if let Some(snap) = snap_name(path) {
        run_root(&["snap", "remove", snap.as_str()])?;
        if snap_is_installed(&snap) {
            return Err(UninstallError::new(
                "command_failed",
                format!("snap {snap} is still installed after snap remove"),
            ));
        }
        let mut removed = remove_stale_snap_desktop_files(&snap)?;
        ensure_desktop_removed(path, &format!("snap remove {snap}"))?;
        if !removed.iter().any(|p| p == path) {
            removed.insert(0, path.to_string());
        }
        return Ok(Some(("snap-remove", removed)));
    }
    if let Some((id, system)) = flatpak_id(path) {
        let scope = if system { "--system" } else { "--user" };
        let args = ["flatpak", "uninstall", "-y", scope, id.as_str()];
        if system {
            run_root(&args)?;
        } else {
            run_user(&args)?;
        }
        ensure_desktop_removed(path, &format!("flatpak uninstall {id}"))?;
        return Ok(Some(("flatpak-uninstall", vec![path.to_string()])));
    }
    Ok(None)
}

fn ensure_desktop_removed(path: &str, action: &str) -> Result<(), UninstallError> {
    if Path::new(path).exists() {
        return Err(UninstallError::new(
            "command_failed",
            format!("{action} completed, but desktop entry still exists: {path}"),
        ));
    }
    Ok(())
}

fn snap_is_installed(snap: &str) -> bool {
    Command::new("snap")
        .args(["list", snap])
        .output()
        .is_ok_and(|out| out.status.success())
}

fn remove_stale_snap_desktop_files(snap: &str) -> Result<Vec<String>, UninstallError> {
    let dir = Path::new("/var/lib/snapd/desktop/applications");
    let prefix = format!("{snap}_");
    let mut targets = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with(&prefix) && name.ends_with(".desktop") {
                    targets.push(p.display().to_string());
                }
            }
        }
    }
    if targets.is_empty() {
        return Ok(targets);
    }
    let mut args = vec!["rm", "-f"];
    for target in &targets {
        args.push(target.as_str());
    }
    run_root(&args)?;
    Ok(targets)
}

fn dpkg_package(path: &str) -> Result<Option<String>, UninstallError> {
    let output = Command::new("dpkg")
        .args(["-S", path])
        .output()
        .map_err(|e| {
            UninstallError::new("command_failed", format!("Failed to run dpkg -S: {e}"))
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().find_map(parse_dpkg_owner))
}

fn parse_dpkg_owner(line: &str) -> Option<String> {
    let (pkg, _) = line.trim().rsplit_once(": ")?;
    let pkg = pkg.trim();
    (!pkg.is_empty()).then(|| pkg.to_string())
}

fn snap_name(path: &str) -> Option<String> {
    let p = Path::new(path);
    if !p.starts_with("/var/lib/snapd/desktop/applications") {
        return None;
    }
    let stem = p.file_stem()?.to_str()?;
    let snap = stem.split('_').next()?;
    (!snap.is_empty()).then(|| snap.to_string())
}

fn flatpak_id(path: &str) -> Option<(String, bool)> {
    let p = Path::new(path);
    let system = p.starts_with("/var/lib/flatpak/exports/share/applications");
    let user = path.contains("/.local/share/flatpak/exports/share/applications/");
    if !system && !user {
        return None;
    }
    let stem = p.file_stem()?.to_str()?;
    (!stem.is_empty()).then(|| (stem.to_string(), system))
}

fn wrapper_paths(stem: &str) -> Vec<PathBuf> {
    let name = format!("{WRAPPER_PREFIX}{stem}.desktop");
    let handler_name = format!("focusstage-handler-{stem}.desktop");
    let mut paths = vec![PathBuf::from("/usr/share/applications").join(&name)];
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data_home) = data_home {
        let apps_dir = data_home.join("applications");
        paths.push(apps_dir.join(&name));
        paths.push(apps_dir.join(&handler_name));
    }
    paths
}

fn remove_appimage_files(appimage: &str, removed: &mut Vec<String>) -> Result<(), UninstallError> {
    remove_file(appimage)?;
    removed.push(appimage.to_string());
    if let Some(fname) = Path::new(appimage).file_name().and_then(|s| s.to_str()) {
        if let Ok(home) = std::env::var("HOME") {
            let cache = PathBuf::from(home)
                .join(".cache/focusstage/appimages")
                .join(fname);
            if cache.is_file() && std::fs::remove_file(&cache).is_ok() {
                removed.push(cache.display().to_string());
            }
        }
    }
    Ok(())
}

fn remove_file(path: &str) -> Result<(), UninstallError> {
    std::fs::remove_file(path)
        .map_err(|e| UninstallError::new("command_failed", format!("Failed to delete {path}: {e}")))
}

fn run_root(args: &[&str]) -> Result<(), UninstallError> {
    let distro = std::env::var("WSL_DISTRO_NAME")
        .map_err(|_| UninstallError::new("internal", "WSL_DISTRO_NAME is not set"))?;
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro.as_str(), "-u", "root", "-e"]);
    cmd.args(args);
    run_command(&mut cmd)
}

fn run_user(args: &[&str]) -> Result<(), UninstallError> {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]);
    run_command(&mut cmd)
}

fn run_command(cmd: &mut Command) -> Result<(), UninstallError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd.output().map_err(|e| {
        UninstallError::new("command_failed", format!("Failed to run {program}: {e}"))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    let message = if detail.is_empty() {
        format!("{program} exited with {}", output.status)
    } else {
        format!("{program} failed: {detail}")
    };
    Err(UninstallError::new("command_failed", message))
}

fn display_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dpkg_owner_lines() {
        assert_eq!(
            parse_dpkg_owner("t3-code: /usr/share/applications/t3-code.desktop"),
            Some("t3-code".into())
        );
        assert_eq!(
            parse_dpkg_owner("t3-code:amd64: /usr/share/applications/t3-code.desktop"),
            Some("t3-code:amd64".into())
        );
        assert_eq!(parse_dpkg_owner("no separator"), None);
    }

    #[test]
    fn recognizes_snap_and_flatpak_desktop_paths() {
        assert_eq!(
            snap_name("/var/lib/snapd/desktop/applications/code_code.desktop"),
            Some("code".into())
        );
        assert_eq!(snap_name("/usr/share/applications/code.desktop"), None);
        assert_eq!(
            flatpak_id("/var/lib/flatpak/exports/share/applications/org.gnome.Builder.desktop"),
            Some(("org.gnome.Builder".into(), true))
        );
        assert_eq!(
            flatpak_id("/home/scl/.local/share/flatpak/exports/share/applications/org.gnome.Builder.desktop"),
            Some(("org.gnome.Builder".into(), false))
        );
        assert_eq!(flatpak_id("/usr/share/applications/code.desktop"), None);
    }
}
