//! Unified Ubuntu user session environment resolution and desktop service synchronization.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, OsString};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

const ENV_BEGIN_MARKER: &[u8] = b"__FOCUSSTAGE_ENV_BEGIN__\0";
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const STANDARD_PATH_FALLBACKS: &[&str] = &[
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/snap/bin",
    "/var/lib/flatpak/exports/bin",
];

const STANDARD_XDG_DATA_FALLBACKS: &[&str] = &["/usr/local/share", "/usr/share"];

/// Live WSL / session variables that must stay bound to the active WSL invocation when present.
const LIVE_RUNTIME_KEYS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "PULSE_SERVER",
    "WSL_INTEROP",
    "WSL_DISTRO_NAME",
    "WSL2_GUI_APPS_ENABLED",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
    "FOCUSSTAGE_SOCKET",
];

/// Variables owned by shells, systemd units, or individual FocusStage child Stages that must
/// never be imported into focusstaged / focusstage's own session snapshot.
const IGNORED_IMPORT_KEYS: &[&str] = &[
    "_",
    "SHLVL",
    "PWD",
    "OLDPWD",
    "NOTIFY_SOCKET",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "MANAGERPID",
    "SYSTEMD_EXEC_PID",
    "LISTEN_PID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
    "FOCUSSTAGE_STAGE_ID",
    "FOCUSSTAGE_LAUNCH_TOKEN",
];

const DESKTOP_SYNC_KEYS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "PULSE_SERVER",
    "WSL_INTEROP",
    "WSL_DISTRO_NAME",
    "WSL2_GUI_APPS_ENABLED",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "PATH",
    "HOME",
    "BROWSER",
    "FOCUSSTAGE_SOCKET",
    "LANG",
    "LANGUAGE",
    "GTK_IM_MODULE",
    "QT_IM_MODULE",
    "XMODIFIERS",
    "GTK_THEME",
    "QT_QPA_PLATFORMTHEME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Debug, Clone)]
pub struct UserAccount {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: PathBuf,
    pub shell: PathBuf,
}

impl UserAccount {
    pub fn lookup(uid: u32) -> Option<Self> {
        let mut pwd = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        loop {
            let rc = unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut pwd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &mut result,
                )
            };
            if rc == libc::ERANGE && buf.len() < 65536 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || result.is_null() {
                return None;
            }
            break;
        }
        let cstr = |ptr: *const libc::c_char| -> String {
            if ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let name = cstr(pwd.pw_name);
        let home = PathBuf::from(cstr(pwd.pw_dir));
        let shell = PathBuf::from(cstr(pwd.pw_shell));
        Some(Self {
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
            name,
            home,
            shell,
        })
    }
}

pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub(crate) fn apply_font_preferences(cmd: &mut Command, overrides: &HashMap<String, String>) {
    let effective = |key: &str| overrides.get(key).cloned().or_else(|| std::env::var(key).ok());
    if allows_font_fallback(effective) {
        if let Some(profile) = crate::xdg::stage_fontconfig_profile() {
            cmd.env("FONTCONFIG_FILE", profile);
        }
    }
}

fn allows_font_fallback(env: impl Fn(&str) -> Option<String>) -> bool {
    let nonempty = |key| env(key).filter(|value| !value.is_empty());
    if ["FONTCONFIG_FILE", "FONTCONFIG_PATH", "FC_LANG", "LANGUAGE"]
        .into_iter().any(|key| nonempty(key).is_some()) {
        return false;
    }
    ["LC_CTYPE", "LC_MESSAGES"].into_iter().all(|category| {
        ["LC_ALL", category, "LANG"].into_iter().find_map(nonempty)
            .is_none_or(|locale| matches!(locale.split('.').next(), Some("C" | "POSIX")))
    })
}

/// Ensure `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` are populated for `uid` when the
/// caller was spawned by a minimal `wsl.exe -e` invocation without PAM environment setup.
pub fn ensure_runtime_env_for_uid(uid: u32) {
    let runtime_missing = std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|v| v.is_empty());
    let candidate_runtime = PathBuf::from(format!("/run/user/{uid}"));
    if runtime_missing {
        if let Ok(meta) = std::fs::metadata(&candidate_runtime) {
            if meta.is_dir() && meta.uid() == uid {
                std::env::set_var("XDG_RUNTIME_DIR", &candidate_runtime);
            }
        }
    }

    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none_or(|v| v.is_empty()) {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(candidate_runtime);
        let bus_path = runtime_dir.join("bus");
        if bus_path.exists() {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus_path.display()),
            );
        }
    }
}

pub fn ensure_runtime_env_for_current_user() {
    ensure_runtime_env_for_uid(current_uid());
}

/// Build a normalized PATH that preserves existing entries and includes standard Linux/WSL app bins.
pub fn build_normalized_path(existing: Option<&str>, home: Option<&Path>) -> String {
    let mut extras: Vec<String> = STANDARD_PATH_FALLBACKS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(home) = home {
        if !home.as_os_str().is_empty() {
            extras.push(format!("{}/.local/bin", home.display()));
        }
    }
    let existing_list: Vec<&str> = existing.into_iter().collect();
    let extra_refs: Vec<String> = vec![extras.join(":")];
    let mut sources = existing_list;
    for item in &extra_refs {
        sources.push(item.as_str());
    }
    merge_path_lists(&sources)
}

/// Initialize `focusstaged`'s own single-source-of-truth user session environment.
/// Must be called on the main thread before spawning worker threads.
pub fn init_daemon_environment() {
    init_environment_for_uid(current_uid(), false);
}

/// Initialize CLI / fallback environment for `target_uid` (or the current user when `None`).
/// Must be called on the main thread before spawning worker threads.
pub fn init_cli_environment(target_uid: Option<u32>) {
    let current = current_uid();
    let uid = target_uid.unwrap_or(current);
    init_environment_for_uid(uid, uid != current);
}

fn init_environment_for_uid(uid: u32, cross_user: bool) {
    // Never inherit Stage child isolation tokens into the compositor or CLI session.
    std::env::remove_var("FOCUSSTAGE_STAGE_ID");
    std::env::remove_var("FOCUSSTAGE_LAUNCH_TOKEN");
    if std::env::var("WAYLAND_DISPLAY")
        .is_ok_and(|v| v.starts_with("focusstage-"))
    {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    let account = UserAccount::lookup(uid);
    if !cross_user {
        ensure_runtime_env_for_uid(uid);
    }

    let initial_env: HashMap<String, String> = if cross_user {
        let mut base = HashMap::new();
        for key in ["WSL_DISTRO_NAME", "WSL_INTEROP", "WSL2_GUI_APPS_ENABLED"] {
            if let Ok(val) = std::env::var(key) {
                if !val.is_empty() {
                    base.insert(key.to_string(), val);
                }
            }
        }
        if let Some(ref acct) = account {
            base.insert("HOME".into(), acct.home.display().to_string());
            base.insert("USER".into(), acct.name.clone());
            base.insert("LOGNAME".into(), acct.name.clone());
            base.insert("SHELL".into(), acct.shell.display().to_string());
        }
        let runtime = PathBuf::from(format!("/run/user/{uid}"));
        if runtime.is_dir() {
            base.insert("XDG_RUNTIME_DIR".into(), runtime.display().to_string());
            let bus = runtime.join("bus");
            if bus.exists() {
                base.insert(
                    "DBUS_SESSION_BUS_ADDRESS".into(),
                    format!("unix:path={}", bus.display()),
                );
            }
        }
        base
    } else {
        let mut map: HashMap<String, String> = std::env::vars().collect();
        if let Some(ref acct) = account {
            if map.get("HOME").is_none_or(|v| v.is_empty()) && !acct.home.as_os_str().is_empty() {
                map.insert("HOME".into(), acct.home.display().to_string());
            }
            if map.get("USER").is_none_or(|v| v.is_empty()) && !acct.name.is_empty() {
                map.insert("USER".into(), acct.name.clone());
            }
            if map.get("LOGNAME").is_none_or(|v| v.is_empty()) && !acct.name.is_empty() {
                map.insert("LOGNAME".into(), acct.name.clone());
            }
        }
        map
    };

    let drop_privs_to = cross_user.then_some(account.as_ref()).flatten();
    let systemd_env = read_systemd_user_environment(&initial_env, drop_privs_to);
    let login_shell = resolve_login_shell(account.as_ref());
    let login_env = read_login_shell_environment(&login_shell, &initial_env, &systemd_env, drop_privs_to);

    let merged = merge_session_environments(
        &initial_env,
        &systemd_env,
        &login_env,
        account.as_ref().map(|a| a.home.as_path()),
    );

    for (key, value) in merged {
        std::env::set_var(key, value);
    }
}

pub fn systemd_user_available() -> bool {
    if !Path::new("/run/systemd/system").exists() {
        return false;
    }
    run_command_with_timeout(
        Command::new("systemctl").args(["--user", "status"]),
        Duration::from_secs(2),
    )
    .is_some_and(|out| out.status.success())
}

/// Synchronize the unified session environment to D-Bus activation and `systemd --user`
/// so portals, gnome-keyring, at-spi, polkit, and `focusstage-open` share the exact same
/// environment snapshot.
pub fn sync_desktop_session_environment() {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let pairs = desktop_sync_env_pairs(&env_map);
    if pairs.is_empty() {
        return;
    }

    let kv_args: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let key_args: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
    let has_systemd = systemd_user_available();

    let mut dbus_synced = false;
    if has_systemd {
        let mut cmd = Command::new("dbus-update-activation-environment");
        cmd.arg("--systemd").args(&kv_args);
        if run_command_with_timeout(&mut cmd, PROBE_TIMEOUT)
            .is_some_and(|out| out.status.success())
        {
            dbus_synced = true;
            debug!("Synced {} session variables via dbus-update-activation-environment --systemd", pairs.len());
        }
    }

    if !dbus_synced {
        let mut cmd = Command::new("dbus-update-activation-environment");
        cmd.args(&kv_args);
        if run_command_with_timeout(&mut cmd, PROBE_TIMEOUT)
            .is_some_and(|out| out.status.success())
        {
            debug!("Synced {} session variables via dbus-update-activation-environment", pairs.len());
        }

        if has_systemd {
            let mut imp = Command::new("systemctl");
            imp.args(["--user", "import-environment"]).args(&key_args);
            if run_command_with_timeout(&mut imp, PROBE_TIMEOUT)
                .is_some_and(|out| out.status.success())
            {
                debug!("Synced {} session variables via systemctl --user import-environment", pairs.len());
            } else {
                warn!("Failed to import session environment into systemd --user");
            }
        }
    }
}

fn resolve_login_shell(account: Option<&UserAccount>) -> PathBuf {
    let is_valid = |path: &Path| {
        if !path.is_file() {
            return false;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        !matches!(name, "nologin" | "false" | "true" | "csh" | "tcsh")
    };
    if let Some(acct) = account {
        if is_valid(&acct.shell) {
            return acct.shell.clone();
        }
    }
    if let Some(shell) = std::env::var_os("SHELL").map(PathBuf::from) {
        if is_valid(&shell) {
            return shell;
        }
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("/bin/sh")
}

fn apply_child_context(
    cmd: &mut Command,
    base_env: &HashMap<String, String>,
    drop_privs: Option<&UserAccount>,
) {
    if let Some(acct) = drop_privs {
        cmd.env_clear();
        cmd.envs(base_env);
        cmd.uid(acct.uid).gid(acct.gid);
    } else {
        cmd.envs(base_env);
    }
}

fn read_systemd_user_environment(
    base_env: &HashMap<String, String>,
    drop_privs: Option<&UserAccount>,
) -> HashMap<String, String> {
    if !Path::new("/run/systemd/system").exists() {
        return HashMap::new();
    }
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "show-environment"]);
    apply_child_context(&mut cmd, base_env, drop_privs);
    let Some(output) = run_command_with_timeout(&mut cmd, PROBE_TIMEOUT) else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_systemd_environment(&String::from_utf8_lossy(&output.stdout))
}

fn read_login_shell_environment(
    shell: &Path,
    initial_env: &HashMap<String, String>,
    systemd_env: &HashMap<String, String>,
    drop_privs: Option<&UserAccount>,
) -> HashMap<String, String> {
    let mut probe_env = initial_env.clone();
    for (k, v) in systemd_env {
        if !IGNORED_IMPORT_KEYS.contains(&k.as_str())
            && !LIVE_RUNTIME_KEYS.contains(&k.as_str())
            && !probe_env.contains_key(k)
        {
            probe_env.insert(k.clone(), v.clone());
        }
    }

    let mut cmd = Command::new(shell);
    cmd.args(["-lc", "printf '%s\\0' '__FOCUSSTAGE_ENV_BEGIN__'; exec env -0"]);
    apply_child_context(&mut cmd, &probe_env, drop_privs);
    if let Some(home) = probe_env.get("HOME").filter(|h| !h.is_empty()) {
        let path = Path::new(home);
        if path.is_dir() {
            cmd.current_dir(path);
        }
    }
    let Some(output) = run_command_with_timeout(&mut cmd, PROBE_TIMEOUT) else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_nul_env_with_marker(&output.stdout)
}

struct TimedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

fn run_command_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<TimedOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
    let buf = rx.recv_timeout(remaining).unwrap_or_default();
    Some(TimedOutput { status, stdout: buf })
}

pub fn parse_systemd_environment(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        if valid_env_key(key) && !IGNORED_IMPORT_KEYS.contains(&key) {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

pub fn parse_nul_env_with_marker(bytes: &[u8]) -> HashMap<String, String> {
    let Some(pos) = bytes
        .windows(ENV_BEGIN_MARKER.len())
        .position(|w| w == ENV_BEGIN_MARKER)
    else {
        return HashMap::new();
    };
    let tail = &bytes[pos + ENV_BEGIN_MARKER.len()..];
    let mut map = HashMap::new();
    for entry in tail.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((key, val)) = text.split_once('=') else {
            continue;
        };
        if valid_env_key(key) && !IGNORED_IMPORT_KEYS.contains(&key) {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn merge_path_lists(lists: &[&str]) -> String {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for list in lists {
        for item in list.split(':').filter(|s| !s.is_empty()) {
            if seen.insert(item.to_string()) {
                out.push(item.to_string());
            }
        }
    }
    out.join(":")
}

pub fn merge_session_environments(
    initial: &HashMap<String, String>,
    systemd: &HashMap<String, String>,
    login: &HashMap<String, String>,
    home_fallback: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    for (key, val) in initial {
        if valid_env_key(key) && !IGNORED_IMPORT_KEYS.contains(&key.as_str()) {
            out.insert(key.clone(), val.clone());
        }
    }

    let is_protected_live = |key: &str| -> bool {
        if !LIVE_RUNTIME_KEYS.contains(&key) {
            return false;
        }
        initial.get(key).is_some_and(|v| {
            !v.is_empty() && !(key == "WAYLAND_DISPLAY" && v.starts_with("focusstage-"))
        })
    };

    for source in [systemd, login] {
        for (key, val) in source {
            if !valid_env_key(key)
                || IGNORED_IMPORT_KEYS.contains(&key.as_str())
                || matches!(key.as_str(), "PATH" | "XDG_DATA_DIRS")
                || is_protected_live(key)
            {
                continue;
            }
            if key == "WAYLAND_DISPLAY" && val.starts_with("focusstage-") {
                continue;
            }
            out.insert(key.clone(), val.clone());
        }
    }

    // Re-assert non-empty live WSL/session bridge variables from the active invocation.
    for &key in LIVE_RUNTIME_KEYS {
        if let Some(val) = initial.get(key) {
            if !val.is_empty() && !(key == "WAYLAND_DISPLAY" && val.starts_with("focusstage-")) {
                out.insert(key.to_string(), val.clone());
            }
        }
    }

    let home = out
        .get("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_fallback.map(Path::to_path_buf));

    let mut path_fallbacks: Vec<String> = STANDARD_PATH_FALLBACKS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(ref home_dir) = home {
        if !home_dir.as_os_str().is_empty() {
            path_fallbacks.push(format!("{}/.local/bin", home_dir.display()));
        }
    }
    let path_fallback_joined = path_fallbacks.join(":");
    let merged_path = merge_path_lists(&[
        login.get("PATH").map(String::as_str).unwrap_or(""),
        systemd.get("PATH").map(String::as_str).unwrap_or(""),
        initial.get("PATH").map(String::as_str).unwrap_or(""),
        path_fallback_joined.as_str(),
    ]);
    if !merged_path.is_empty() {
        out.insert("PATH".to_string(), merged_path);
    }

    let xdg_fallback_joined = STANDARD_XDG_DATA_FALLBACKS.join(":");
    let merged_xdg_data = merge_path_lists(&[
        login.get("XDG_DATA_DIRS").map(String::as_str).unwrap_or(""),
        systemd.get("XDG_DATA_DIRS").map(String::as_str).unwrap_or(""),
        initial.get("XDG_DATA_DIRS").map(String::as_str).unwrap_or(""),
        xdg_fallback_joined.as_str(),
    ]);
    if !merged_xdg_data.is_empty() {
        out.insert("XDG_DATA_DIRS".to_string(), merged_xdg_data);
    }

    if out
        .get("XDG_CURRENT_DESKTOP")
        .is_none_or(|v| v.is_empty())
    {
        out.insert("XDG_CURRENT_DESKTOP".to_string(), "GNOME".to_string());
    }

    out
}

pub fn desktop_sync_env_pairs(env_map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut keys: Vec<String> = DESKTOP_SYNC_KEYS.iter().map(|s| (*s).to_string()).collect();
    let mut lc_keys: Vec<String> = env_map
        .keys()
        .filter(|k| k.starts_with("LC_") && valid_env_key(k))
        .cloned()
        .collect();
    lc_keys.sort();
    for k in lc_keys {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }

    let mut pairs = Vec::new();
    for key in keys {
        if let Some(val) = env_map.get(&key) {
            if val.is_empty() {
                continue;
            }
            if key == "WAYLAND_DISPLAY" && val.starts_with("focusstage-") {
                continue;
            }
            pairs.push((key, val.clone()));
        }
    }
    pairs
}

pub fn caller_runtime_bridge_keys() -> &'static [&'static str] {
    LIVE_RUNTIME_KEYS
}

pub fn os_var_nonempty(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn han_fallback_respects_effective_session_environment() {
        let allows = |pairs: &[(&str, &str)]| {
            allows_font_fallback(|key| pairs.iter().find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned()))
        };
        assert!(allows(&[]));
        assert!(allows(&[("LANG", "C.UTF-8")]));
        assert!(allows(&[("LC_ALL", "C.UTF-8"), ("LANG", "ja_JP.UTF-8")]));
        for key in ["LC_ALL", "LC_CTYPE", "LC_MESSAGES", "LANG", "LANGUAGE", "FC_LANG", "FONTCONFIG_PATH", "FONTCONFIG_FILE"] {
            assert!(!allows(&[(key, "explicit")]));
        }
        assert!(!allows(&[("LC_ALL", ""), ("LANG", "zh_TW.UTF-8")]));
    }

    #[test]
    fn parses_nul_env_ignoring_profile_banner() {
        let mut raw = b"Welcome to Ubuntu!\nSome banner=noise\n".to_vec();
        raw.extend_from_slice(ENV_BEGIN_MARKER);
        raw.extend_from_slice(b"HOME=/home/alice\0PATH=/home/alice/.cargo/bin:/usr/bin\0MULTI=a\nb\0SHLVL=3\0");

        let parsed = parse_nul_env_with_marker(&raw);
        assert_eq!(parsed.get("HOME").map(String::as_str), Some("/home/alice"));
        assert_eq!(
            parsed.get("PATH").map(String::as_str),
            Some("/home/alice/.cargo/bin:/usr/bin")
        );
        assert_eq!(parsed.get("MULTI").map(String::as_str), Some("a\nb"));
        assert!(!parsed.contains_key("SHLVL"));
    }

    #[test]
    fn merges_paths_and_preserves_live_runtime_and_user_desktop() {
        let initial = HashMap::from([
            ("DISPLAY".into(), ":0".into()),
            ("WAYLAND_DISPLAY".into(), "focusstage-0".into()),
            ("WSL_INTEROP".into(), "/run/WSL/10_interop".into()),
            ("PATH".into(), "/usr/bin:/mnt/c/Windows/System32".into()),
            ("FOCUSSTAGE_STAGE_ID".into(), "stage-1".into()),
        ]);
        let systemd = HashMap::from([
            ("XDG_CURRENT_DESKTOP".into(), "Ubuntu:GNOME".into()),
            ("XDG_DATA_DIRS".into(), "/var/lib/flatpak/exports/share:/usr/share".into()),
            ("DISPLAY".into(), ":99".into()),
            ("WAYLAND_DISPLAY".into(), "wayland-0".into()),
            ("GTK_IM_MODULE".into(), "ibus".into()),
        ]);
        let login = HashMap::from([
            ("HOME".into(), "/home/alice".into()),
            ("PATH".into(), "/home/alice/.cargo/bin:/usr/bin".into()),
            ("XDG_DATA_DIRS".into(), "/home/alice/.local/share/flatpak/exports/share:/usr/local/share".into()),
            ("LANG".into(), "zh_CN.UTF-8".into()),
        ]);

        let merged = merge_session_environments(&initial, &systemd, &login, None);
        assert_eq!(merged.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(merged.get("WAYLAND_DISPLAY").map(String::as_str), Some("wayland-0"));
        assert_eq!(merged.get("WSL_INTEROP").map(String::as_str), Some("/run/WSL/10_interop"));
        assert_eq!(merged.get("XDG_CURRENT_DESKTOP").map(String::as_str), Some("Ubuntu:GNOME"));
        assert_eq!(merged.get("GTK_IM_MODULE").map(String::as_str), Some("ibus"));
        assert_eq!(merged.get("LANG").map(String::as_str), Some("zh_CN.UTF-8"));
        assert!(!merged.contains_key("FOCUSSTAGE_STAGE_ID"));

        let path_parts: Vec<&str> = merged["PATH"].split(':').collect();
        assert_eq!(path_parts[0], "/home/alice/.cargo/bin");
        assert!(path_parts.contains(&"/mnt/c/Windows/System32"));
        assert!(path_parts.contains(&"/home/alice/.local/bin"));
        assert_eq!(path_parts.iter().filter(|&&p| p == "/usr/bin").count(), 1);

        let xdg_parts: Vec<&str> = merged["XDG_DATA_DIRS"].split(':').collect();
        assert!(xdg_parts.contains(&"/home/alice/.local/share/flatpak/exports/share"));
        assert!(xdg_parts.contains(&"/var/lib/flatpak/exports/share"));
        assert!(xdg_parts.contains(&"/usr/share"));
    }

    #[test]
    fn desktop_sync_excludes_focusstage_wayland_and_empty_vars() {
        let env_map = HashMap::from([
            ("DISPLAY".into(), ":0".into()),
            ("WAYLAND_DISPLAY".into(), "focusstage-0".into()),
            ("XDG_CURRENT_DESKTOP".into(), "GNOME".into()),
            ("LC_CTYPE".into(), "zh_CN.UTF-8".into()),
            ("LANG".into(), "".into()),
        ]);
        let pairs = desktop_sync_env_pairs(&env_map);
        assert!(pairs.iter().any(|(k, v)| k == "DISPLAY" && v == ":0"));
        assert!(pairs.iter().any(|(k, v)| k == "LC_CTYPE" && v == "zh_CN.UTF-8"));
        assert!(!pairs.iter().any(|(k, _)| k == "WAYLAND_DISPLAY"));
        assert!(!pairs.iter().any(|(k, _)| k == "LANG"));
    }
}
