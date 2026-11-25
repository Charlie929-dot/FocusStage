use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::control::ControlClient;
use crate::keyring::{query_keyring_status, KeyringStatus};
use crate::protocol::{ControlRequest, ControlResponse};
use crate::session_env::{current_uid, systemd_user_available, UserAccount};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxAccountInfo {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub full_name: String,
    pub home: String,
    pub shell: String,
    pub is_admin: bool,
    pub is_current: bool,
    pub is_default_wsl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsStatus {
    pub distro: String,
    pub current_user: String,
    pub default_wsl_user: Option<String>,
    pub accounts: Vec<LinuxAccountInfo>,
    pub keyring: KeyringStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AccountAction {
    CreateUser {
        username: String,
        #[serde(default)]
        full_name: String,
        password: String,
        #[serde(default)]
        is_admin: bool,
    },
    SetPassword {
        username: String,
        password: String,
    },
    SetAdmin {
        username: String,
        is_admin: bool,
    },
    DeleteUser {
        username: String,
        #[serde(default)]
        remove_home: bool,
    },
    SetDefaultWslUser {
        username: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountApplyOutcome {
    pub status: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

pub fn accounts_status() -> AccountsStatus {
    let distro = std::env::var("WSL_DISTRO_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Ubuntu".to_string());

    let uid = current_uid();
    let current_user = UserAccount::lookup(uid)
        .map(|u| u.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "ubuntu".to_string()));

    let default_wsl_user = read_default_wsl_user();
    let accounts = list_linux_accounts(&current_user, default_wsl_user.as_deref());
    let keyring = query_keyring_status();

    AccountsStatus {
        distro,
        current_user,
        default_wsl_user,
        accounts,
        keyring,
    }
}

pub fn is_valid_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("密码不能为空。".into());
    }
    if password.contains('\0') || password.contains('\n') || password.contains('\r') {
        return Err("密码不能包含换行符或空字符。".into());
    }
    Ok(())
}

fn admin_members() -> (HashSet<String>, HashSet<u32>) {
    let mut members = HashSet::new();
    let mut gids = HashSet::new();
    let Ok(content) = fs::read_to_string("/etc/group") else {
        return (members, gids);
    };
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() < 4 {
            continue;
        }
        let name = parts[0];
        if !matches!(name, "sudo" | "admin" | "wheel") {
            continue;
        }
        if let Ok(gid) = parts[2].parse::<u32>() {
            gids.insert(gid);
        }
        for member in parts[3].split(',') {
            let trimmed = member.trim();
            if !trimmed.is_empty() {
                members.insert(trimmed.to_string());
            }
        }
    }
    (members, gids)
}

fn list_linux_accounts(current_user: &str, default_wsl_user: Option<&str>) -> Vec<LinuxAccountInfo> {
    let (admin_users, admin_gids) = admin_members();
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return Vec::new();
    };

    let mut accounts = Vec::new();
    for line in passwd.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split(':').collect();
        if parts.len() < 7 {
            continue;
        }
        let username = parts[0];
        let Ok(uid) = parts[2].parse::<u32>() else {
            continue;
        };
        let Ok(gid) = parts[3].parse::<u32>() else {
            continue;
        };
        let shell = parts[6];
        if (uid < 1000 || uid >= 65534) && username != current_user {
            continue;
        }
        if matches!(shell, "/usr/sbin/nologin" | "/sbin/nologin" | "/bin/false")
            && username != current_user
        {
            continue;
        }

        let full_name = parts[4].split(',').next().unwrap_or("").trim().to_string();
        let is_admin =
            uid == 0 || admin_users.contains(username) || admin_gids.contains(&gid);
        let is_current = username == current_user;
        let is_default_wsl = default_wsl_user.is_some_and(|d| d == username);

        accounts.push(LinuxAccountInfo {
            username: username.to_string(),
            uid,
            gid,
            full_name,
            home: parts[5].to_string(),
            shell: shell.to_string(),
            is_admin,
            is_current,
            is_default_wsl,
        });
    }

    accounts.sort_by(|a, b| a.uid.cmp(&b.uid).then_with(|| a.username.cmp(&b.username)));
    accounts
}

fn read_default_wsl_user() -> Option<String> {
    let content = fs::read_to_string("/etc/wsl.conf").ok()?;
    let mut in_user_section = false;
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1].trim();
            in_user_section = section.eq_ignore_ascii_case("user");
            continue;
        }
        if in_user_section {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("default") {
                    let user = v.trim().trim_matches('"').trim_matches('\'').trim();
                    if !user.is_empty() {
                        return Some(user.to_string());
                    }
                }
            }
        }
    }
    None
}

fn update_wsl_conf_default_user(username: &str) -> Result<(), String> {
    let existing = fs::read_to_string("/etc/wsl.conf").unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut in_user_section = false;
    let mut saw_user_section = false;
    let mut replaced_default = false;

    for raw_line in existing.lines() {
        let trimmed = raw_line.split('#').next().unwrap_or("").trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_user_section && !replaced_default {
                lines.push(format!("default={username}"));
                replaced_default = true;
            }
            let section = trimmed[1..trimmed.len() - 1].trim();
            in_user_section = section.eq_ignore_ascii_case("user");
            if in_user_section {
                saw_user_section = true;
            }
            lines.push(raw_line.to_string());
            continue;
        }
        if in_user_section {
            if let Some((k, _)) = trimmed.split_once('=') {
                if k.trim().eq_ignore_ascii_case("default") {
                    if !replaced_default {
                        lines.push(format!("default={username}"));
                        replaced_default = true;
                    }
                    continue;
                }
            }
        }
        lines.push(raw_line.to_string());
    }

    if in_user_section && !replaced_default {
        lines.push(format!("default={username}"));
    }

    if !saw_user_section {
        if !lines.is_empty() && !lines.last().is_some_and(|l| l.is_empty()) {
            lines.push(String::new());
        }
        lines.push("[user]".to_string());
        lines.push(format!("default={username}"));
    }

    let mut out = lines.join("\n");
    out.push('\n');
    fs::write("/etc/wsl.conf", out).map_err(|e| format!("写入 /etc/wsl.conf 失败: {e}"))
}

pub fn verify_password(username: &str, password: &str) -> Result<bool, String> {
    if !is_valid_username(username) {
        return Err("无效的用户名格式。".into());
    }
    if password.contains('\0') || password.contains('\n') || password.contains('\r') {
        return Err("密码包含非法字符。".into());
    }

    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 || master_fd < 0 || slave_fd < 0 {
        return Err("无法创建伪终端以验证密码。".into());
    }

    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

    // Set master nonblocking so we can poll prompt output cleanly.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    let slave_raw = slave.as_raw_fd();
    let slave_in = slave.try_clone().map_err(|e| format!("clone pty: {e}"))?;
    let slave_out = slave.try_clone().map_err(|e| format!("clone pty: {e}"))?;

    let mut cmd = Command::new("/bin/su");
    cmd.args(["-c", "/bin/true", username])
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::from(slave_in))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave));

    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_raw, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| format!("启动 su 失败: {e}"))?;

    let mut master_file = unsafe { fs::File::from_raw_fd(master.as_raw_fd()) };
    // Ownership moved into master_file; forget OwnedFd so it isn't double-closed.
    std::mem::forget(master);

    let prompt_deadline = Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 512];
    let mut seen_prompt = false;
    while Instant::now() < prompt_deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Ok(status.success());
        }
        match master_file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if text.contains(':') || text.to_ascii_lowercase().contains("password") {
                    seen_prompt = true;
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }

    let _ = seen_prompt;
    let payload = format!("{password}\n");
    let _ = master_file.write_all(payload.as_bytes());
    let _ = master_file.flush();

    let wait_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Ok(status.success());
        }
        let mut drain = [0u8; 256];
        let _ = master_file.read(&mut drain);
        if Instant::now() >= wait_deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("密码验证超时。".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn apply_action_as_root(action: AccountAction) -> Result<AccountApplyOutcome, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("此操作需要 root 权限（请通过系统提权执行）。".into());
    }

    match action {
        AccountAction::CreateUser {
            username,
            full_name,
            password,
            is_admin,
        } => {
            if !is_valid_username(&username) {
                return Err("用户名格式无效：需以小写字母或下划线开头，仅含小写字母、数字、_ 或 -（1–32 位）。".into());
            }
            validate_password(&password)?;
            if user_exists(&username) {
                return Err(format!("Linux 账号 “{username}” 已存在。"));
            }
            let mut cmd = Command::new("/usr/sbin/useradd");
            cmd.args(["-m", "-s", "/bin/bash"]);
            let clean_name = full_name.trim().replace([',', ':'], " ");
            if !clean_name.is_empty() {
                cmd.args(["-c", &clean_name]);
            }
            cmd.arg(&username);
            run_checked(cmd, "创建账号失败")?;

            set_user_password(&username, &password)?;
            if is_admin {
                let mut mod_cmd = Command::new("/usr/sbin/usermod");
                mod_cmd.args(["-aG", "sudo", &username]);
                run_checked(mod_cmd, "授予 sudo 权限失败")?;
            }

            Ok(AccountApplyOutcome {
                status: "ok".into(),
                message: if is_admin {
                    format!("已创建管理员账号 “{username}”。")
                } else {
                    format!("已创建普通账号 “{username}”。")
                },
                username: Some(username),
            })
        }
        AccountAction::SetPassword { username, password } => {
            if !is_valid_username(&username) {
                return Err("用户名格式无效。".into());
            }
            validate_password(&password)?;
            ensure_regular_account_exists(&username)?;
            set_user_password(&username, &password)?;
            Ok(AccountApplyOutcome {
                status: "ok".into(),
                message: format!("已更新 “{username}” 的 Linux 密码。"),
                username: Some(username),
            })
        }
        AccountAction::SetAdmin { username, is_admin } => {
            if !is_valid_username(&username) {
                return Err("用户名格式无效。".into());
            }
            ensure_regular_account_exists(&username)?;
            if is_admin {
                let mut cmd = Command::new("/usr/sbin/usermod");
                cmd.args(["-aG", "sudo", &username]);
                run_checked(cmd, "授予 sudo 权限失败")?;
                Ok(AccountApplyOutcome {
                    status: "ok".into(),
                    message: format!("已为 “{username}” 开启管理员 (sudo) 权限。"),
                    username: Some(username),
                })
            } else {
                let other_admins = count_other_admins(&username);
                if other_admins == 0 {
                    return Err("至少需要保留一个管理员 (sudo) 账户，否则系统将无法进行提权认证。请先新建或指定另一个管理员账户。".into());
                }
                for group in ["sudo", "admin", "wheel"] {
                    let _ = Command::new("/usr/bin/gpasswd")
                        .args(["-d", &username, group])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                Ok(AccountApplyOutcome {
                    status: "ok".into(),
                    message: format!("已将 “{username}” 设为普通账户（已移除 sudo 权限）。"),
                    username: Some(username),
                })
            }
        }
        AccountAction::DeleteUser {
            username,
            remove_home,
        } => {
            if !is_valid_username(&username) {
                return Err("用户名格式无效。".into());
            }
            ensure_regular_account_exists(&username)?;
            if read_default_wsl_user().as_deref() == Some(username.as_str()) {
                return Err("无法删除当前 WSL 默认账户，请先将其他账户设为默认。".into());
            }
            if count_other_admins(&username) == 0 {
                return Err("无法删除最后一个管理员 (sudo) 账户。".into());
            }
            let mut cmd = Command::new("/usr/sbin/userdel");
            if remove_home {
                cmd.arg("-r");
            }
            cmd.arg(&username);
            run_checked(cmd, "删除账号失败")?;
            Ok(AccountApplyOutcome {
                status: "ok".into(),
                message: format!("已删除 Linux 账号 “{username}”。"),
                username: Some(username),
            })
        }
        AccountAction::SetDefaultWslUser { username } => {
            if !is_valid_username(&username) {
                return Err("用户名格式无效。".into());
            }
            ensure_regular_account_exists(&username)?;
            update_wsl_conf_default_user(&username)?;
            Ok(AccountApplyOutcome {
                status: "ok".into(),
                message: format!("已将默认 WSL 启动账户设为 “{username}”（重启 WSL 后生效）。"),
                username: Some(username),
            })
        }
    }
}

pub fn run_privileged_action(
    sock: &Path,
    action: &AccountAction,
) -> Result<AccountApplyOutcome, String> {
    if unsafe { libc::geteuid() } == 0 {
        return apply_action_as_root(action.clone());
    }

    let client = ControlClient::new(sock.to_path_buf());
    for _ in 0..40 {
        if matches!(
            client.call(&ControlRequest::EnsurePolkitAgent),
            Ok(ControlResponse::PolkitAgent { ready: true })
        ) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let payload = serde_json::to_string(action).map_err(|e| format!("序列化请求失败: {e}"))?;
    let focusstage_bin = std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .unwrap_or_else(|| Path::new("/usr/local/bin/focusstage").to_path_buf());

    let mut cmd = if systemd_user_available() && Path::new("/usr/bin/systemd-run").is_file() {
        let unit = format!("focusstage-acct-{}.service", std::process::id());
        let mut c = Command::new("/usr/bin/systemd-run");
        c.args([
            "--user",
            "--wait",
            "--pipe",
            "--quiet",
            "--collect",
            &format!("--unit={unit}"),
            "--property=RuntimeMaxSec=120s",
            "/usr/bin/pkexec",
            "--disable-internal-agent",
        ]);
        c.arg(&focusstage_bin);
        c.args(["--json", "accounts", "apply-stdin"]);
        c
    } else {
        let mut c = Command::new("/usr/bin/pkexec");
        c.args(["--disable-internal-agent"]);
        c.arg(&focusstage_bin);
        c.args(["--json", "accounts", "apply-stdin"]);
        c
    };

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("无法启动提权程序: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("发送账号操作参数失败: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("等待提权结果失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if let Ok(outcome) = serde_json::from_str::<AccountApplyOutcome>(&stdout) {
        if outcome.status == "ok" && output.status.success() {
            return Ok(outcome);
        }
        return Err(outcome.message);
    }

    match output.status.code() {
        Some(126) => Err("提权认证已取消。".into()),
        Some(127) => Err("无法完成 Polkit 提权认证（请确认有可用的管理员账户或稍后重试）。".into()),
        _ => {
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            Err(if detail.is_empty() {
                format!("账号操作失败（退出码 {:?}）。", output.status.code())
            } else {
                detail
            })
        }
    }
}

fn user_exists(username: &str) -> bool {
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return false;
    };
    passwd
        .lines()
        .any(|l| l.split(':').next() == Some(username))
}

fn ensure_regular_account_exists(username: &str) -> Result<(), String> {
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return Err("无法读取 /etc/passwd。".into());
    };
    for line in passwd.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 4 && parts[0] == username {
            let uid = parts[2].parse::<u32>().unwrap_or(0);
            if uid < 1000 || uid >= 65534 {
                return Err(format!("不允许修改系统内置账号 “{username}” (UID {uid})。"));
            }
            return Ok(());
        }
    }
    Err(format!("找不到 Linux 账号 “{username}”。"))
}

fn count_other_admins(exclude_user: &str) -> usize {
    let accounts = list_linux_accounts("", None);
    accounts
        .into_iter()
        .filter(|a| a.uid >= 1000 && a.is_admin && a.username != exclude_user)
        .count()
}

fn set_user_password(username: &str, password: &str) -> Result<(), String> {
    let mut child = Command::new("/usr/sbin/chpasswd")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 chpasswd 失败: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let line = format!("{username}:{password}\n");
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("写入密码失败: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("等待 chpasswd 失败: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() {
            "修改密码失败。".into()
        } else {
            format!("修改密码失败: {err}")
        })
    }
}

fn run_checked(mut cmd: Command, prefix: &str) -> Result<(), String> {
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{prefix}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        Err(if detail.is_empty() {
            format!("{prefix}（退出码 {:?}）", output.status.code())
        } else {
            format!("{prefix}: {detail}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation_rejects_invalid_and_accepts_posix_names() {
        assert!(is_valid_username("scl"));
        assert!(is_valid_username("work_user-1"));
        assert!(is_valid_username("_svc"));
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("Root"));
        assert!(!is_valid_username("1user"));
        assert!(!is_valid_username("bad user"));
        assert!(!is_valid_username("bad:user"));
        assert!(!is_valid_username(&"a".repeat(33)));
    }

    #[test]
    fn password_validation_rejects_empty_and_newlines() {
        assert!(validate_password("s3cr3t!").is_ok());
        assert!(validate_password("").is_err());
        assert!(validate_password("line1\nline2").is_err());
        assert!(validate_password("pass\r").is_err());
        assert!(validate_password("pass\0").is_err());
    }
}
