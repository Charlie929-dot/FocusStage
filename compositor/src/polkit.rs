use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use smithay::reexports::calloop::channel as calloop_channel;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, Proxy};

use crate::desktop;
use crate::icons::DesktopIcons;
use crate::manager::{stop_existing_user_process, stop_helper_unit};
use crate::session_env::{current_uid, UserAccount};

const AGENT_OBJECT_PATH: &str = "/org/freedesktop/PolicyKit1/AuthenticationAgent";
const POLKIT_BUS_NAME: &str = "org.freedesktop.PolicyKit1";
const POLKIT_AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const POLKIT_AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const AGENT_HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";
const AGENT_HELPER_BINS: &[&str] = &[
    "/usr/libexec/polkit-agent-helper-1",
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/lib/policykit-1/polkit-agent-helper-1",
];
const LEGACY_AGENT_PROCS: &[&str] = &[
    "polkit-gnome-authentication-agent-1",
    "polkit-mate-authentication-agent-1",
    "lxpolkit",
];

#[derive(Debug, Clone)]
pub enum PolkitHostCommand {
    Begin {
        cookie: String,
        action_id: String,
        message: String,
        icon_png: Option<String>,
        program_name: Option<String>,
        command_line: Option<String>,
        user: String,
        users: Vec<String>,
        prompt: String,
        echo_on: bool,
        info: Option<String>,
        error: Option<String>,
    },
    Prompt {
        cookie: String,
        prompt: String,
        echo_on: bool,
        info: Option<String>,
        error: Option<String>,
        retry: bool,
    },
    End {
        cookie: String,
        succeeded: bool,
        message: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum PolkitHostReply {
    Submit {
        cookie: String,
        user: String,
        response: String,
    },
    Cancel {
        cookie: String,
    },
    HostDisconnected,
}

enum AgentControl {
    EnsureRegistered(std_mpsc::Sender<bool>),
    HostReply(PolkitHostReply),
    Shutdown,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
enum PolkitDbusError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

enum AuthEvent {
    Begin {
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        users: Vec<String>,
        reply: oneshot::Sender<Result<(), PolkitDbusError>>,
    },
    Cancel {
        cookie: String,
    },
}

#[derive(Clone)]
struct DbusAgentObject {
    tx: mpsc::UnboundedSender<AuthEvent>,
}

#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl DbusAgentObject {
    #[zbus(name = "BeginAuthentication")]
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), PolkitDbusError> {
        let users = resolve_identity_users(&identities);
        if users.is_empty() {
            return Err(PolkitDbusError::Failed(
                "No supported Unix user identity in Polkit request".into(),
            ));
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AuthEvent::Begin {
                action_id,
                message,
                icon_name,
                details,
                cookie,
                users,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(PolkitDbusError::Failed(
                "FocusStage Polkit agent is shutting down".into(),
            ));
        }

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(PolkitDbusError::Cancelled(
                "Authentication session was closed".into(),
            )),
        }
    }

    #[zbus(name = "CancelAuthentication")]
    async fn cancel_authentication(&self, cookie: String) -> Result<(), PolkitDbusError> {
        let _ = self.tx.send(AuthEvent::Cancel { cookie });
        Ok(())
    }
}

pub struct PolkitAgentHandle {
    ctrl_tx: mpsc::UnboundedSender<AgentControl>,
    registered: Arc<AtomicBool>,
}

impl PolkitAgentHandle {
    pub fn spawn(
        host_tx: calloop_channel::Sender<PolkitHostCommand>,
        host_locale: Option<String>,
    ) -> Self {
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let registered = Arc::new(AtomicBool::new(false));
        let registered_flag = Arc::clone(&registered);
        let locale = resolve_polkit_locale(host_locale.as_deref());

        thread::Builder::new()
            .name("focusstage-polkit".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        warn!(%err, "Failed to initialize FocusStage Polkit runtime");
                        return;
                    }
                };
                rt.block_on(run_agent_loop(
                    ctrl_rx,
                    host_tx,
                    registered_flag,
                    locale,
                ));
            })
            .expect("spawn focusstage-polkit thread");

        Self {
            ctrl_tx,
            registered,
        }
    }

    pub fn ensure_registered(&self) -> bool {
        if self.registered.load(Ordering::Acquire) {
            return true;
        }
        let (tx, rx) = std_mpsc::channel();
        if self.ctrl_tx.send(AgentControl::EnsureRegistered(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(Duration::from_millis(1500)).unwrap_or(false)
    }

    pub fn send_host_reply(&self, reply: PolkitHostReply) {
        let _ = self.ctrl_tx.send(AgentControl::HostReply(reply));
    }
}

impl Drop for PolkitAgentHandle {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(AgentControl::Shutdown);
    }
}

enum LoopMsg {
    Ctrl(AgentControl),
    Auth(AuthEvent),
    Helper(HelperEvent),
    OwnerChange(zbus::fdo::NameOwnerChanged),
}

async fn run_agent_loop(
    mut ctrl_rx: mpsc::UnboundedReceiver<AgentControl>,
    host_tx: calloop_channel::Sender<PolkitHostCommand>,
    registered: Arc<AtomicBool>,
    locale: String,
) {
    let connection = match Connection::system().await {
        Ok(conn) => conn,
        Err(err) => {
            warn!(%err, "Failed to connect to system D-Bus for FocusStage Polkit agent");
            drain_ctrl_until_shutdown(&mut ctrl_rx).await;
            return;
        }
    };

    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<AuthEvent>();
    if let Err(err) = connection
        .object_server()
        .at(AGENT_OBJECT_PATH, DbusAgentObject { tx: auth_tx })
        .await
    {
        warn!(%err, "Failed to export FocusStage Polkit AuthenticationAgent interface");
        drain_ctrl_until_shutdown(&mut ctrl_rx).await;
        return;
    }

    let owner_stream = match DBusProxy::new(&connection).await {
        Ok(dbus) => dbus.receive_name_owner_changed().await.ok(),
        Err(_) => None,
    };

    let mut active_subject: Option<(String, HashMap<String, OwnedValue>)> = None;
    let initial_ok = register_agent(&connection, &locale, &mut active_subject).await;
    registered.store(initial_ok, Ordering::Release);

    let (helper_tx, mut helper_rx) = mpsc::unbounded_channel::<HelperEvent>();
    let (loop_tx, mut loop_rx) = mpsc::unbounded_channel::<LoopMsg>();

    {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                let is_shutdown = matches!(msg, AgentControl::Shutdown);
                if tx.send(LoopMsg::Ctrl(msg)).is_err() || is_shutdown {
                    break;
                }
            }
        });
    }
    {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = auth_rx.recv().await {
                if tx.send(LoopMsg::Auth(msg)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = helper_rx.recv().await {
                if tx.send(LoopMsg::Helper(msg)).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(mut stream) = owner_stream {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(change) = next_stream_item(&mut stream).await {
                if tx.send(LoopMsg::OwnerChange(change)).is_err() {
                    break;
                }
            }
        });
    }
    drop(loop_tx);

    let mut sessions: HashMap<String, ActiveSession> = HashMap::new();

    while let Some(msg) = loop_rx.recv().await {
        match msg {
            LoopMsg::Ctrl(AgentControl::EnsureRegistered(reply)) => {
                let ok = if registered.load(Ordering::Acquire) {
                    true
                } else {
                    let ok = register_agent(&connection, &locale, &mut active_subject).await;
                    registered.store(ok, Ordering::Release);
                    ok
                };
                let _ = reply.send(ok);
            }
            LoopMsg::Ctrl(AgentControl::HostReply(reply)) => {
                handle_host_reply(reply, &mut sessions, &host_tx, &helper_tx);
            }
            LoopMsg::Ctrl(AgentControl::Shutdown) => {
                cancel_all_sessions(&mut sessions, &host_tx, None);
                break;
            }
            LoopMsg::Auth(AuthEvent::Begin {
                action_id,
                message,
                icon_name,
                details,
                cookie,
                users,
                reply,
            }) => {
                handle_begin_auth(
                    action_id,
                    message,
                    icon_name,
                    details,
                    cookie,
                    users,
                    reply,
                    &mut sessions,
                    &host_tx,
                    &helper_tx,
                );
            }
            LoopMsg::Auth(AuthEvent::Cancel { cookie }) => {
                if let Some(session) = sessions.remove(&cookie) {
                    session.finish_cancelled(&host_tx, Some("认证已取消".into()));
                }
            }
            LoopMsg::Helper(ev) => {
                handle_helper_event(ev, &mut sessions, &host_tx, &helper_tx);
            }
            LoopMsg::OwnerChange(change) => {
                if let Ok(args) = change.args() {
                    if args.name() == POLKIT_BUS_NAME {
                        if args.new_owner().is_none() {
                            registered.store(false, Ordering::Release);
                        } else {
                            let ok = register_agent(&connection, &locale, &mut active_subject).await;
                            registered.store(ok, Ordering::Release);
                        }
                    }
                }
            }
        }
    }

    if let Some(subject) = active_subject.take() {
        let _ = unregister_agent(&connection, subject).await;
    }
    registered.store(false, Ordering::Release);
}

async fn next_stream_item<S: zbus::export::futures_core::Stream + Unpin>(
    stream: &mut S,
) -> Option<S::Item> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

async fn drain_ctrl_until_shutdown(ctrl_rx: &mut mpsc::UnboundedReceiver<AgentControl>) {
    while let Some(ctrl) = ctrl_rx.recv().await {
        match ctrl {
            AgentControl::EnsureRegistered(reply) => {
                let _ = reply.send(false);
            }
            AgentControl::Shutdown => break,
            AgentControl::HostReply(_) => {}
        }
    }
}

async fn register_agent(
    connection: &Connection,
    locale: &str,
    active_subject: &mut Option<(String, HashMap<String, OwnedValue>)>,
) -> bool {
    cleanup_legacy_polkit_agents();

    let Some(subject) = resolve_polkit_subject(connection).await else {
        warn!("Could not determine Polkit subject for FocusStage authentication agent");
        return false;
    };

    let authority = match Proxy::new(
        connection,
        POLKIT_BUS_NAME,
        POLKIT_AUTHORITY_PATH,
        POLKIT_AUTHORITY_IFACE,
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!(%err, "Failed to create Polkit Authority D-Bus proxy");
            return false;
        }
    };

    if let Err(err) = authority
        .call::<_, _, ()>(
            "RegisterAuthenticationAgent",
            &(subject.clone(), locale, AGENT_OBJECT_PATH),
        )
        .await
    {
        warn!(%err, "Failed to register FocusStage Polkit authentication agent");
        return false;
    }

    *active_subject = Some(subject);
    info!(locale, "Registered native FocusStage Polkit authentication agent");
    true
}

async fn unregister_agent(
    connection: &Connection,
    subject: (String, HashMap<String, OwnedValue>),
) -> zbus::Result<()> {
    let authority = Proxy::new(
        connection,
        POLKIT_BUS_NAME,
        POLKIT_AUTHORITY_PATH,
        POLKIT_AUTHORITY_IFACE,
    )
    .await?;
    authority
        .call::<_, _, ()>("UnregisterAuthenticationAgent", &(subject, AGENT_OBJECT_PATH))
        .await
}

fn cleanup_legacy_polkit_agents() {
    stop_helper_unit("focusstage-polkit-agent");
    for proc_name in LEGACY_AGENT_PROCS {
        stop_existing_user_process(proc_name);
    }
}

async fn resolve_polkit_subject(
    connection: &Connection,
) -> Option<(String, HashMap<String, OwnedValue>)> {
    if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
        let trimmed = session_id.trim();
        if !trimmed.is_empty() {
            return Some(unix_session_subject(trimmed));
        }
    }

    let uid = current_uid();
    if let Some(session_id) = query_logind_display_session(connection, uid).await {
        return Some(unix_session_subject(&session_id));
    }

    if let Some(session_id) = query_logind_pid_session(connection, std::process::id()).await {
        return Some(unix_session_subject(&session_id));
    }

    let mut details = HashMap::new();
    if let Ok(val) = OwnedValue::try_from(Value::from(uid)) {
        details.insert("uid".to_string(), val);
        return Some(("unix-user".to_string(), details));
    }
    None
}

fn unix_session_subject(session_id: &str) -> (String, HashMap<String, OwnedValue>) {
    let mut details = HashMap::new();
    details.insert(
        "session-id".to_string(),
        OwnedValue::from(zbus::zvariant::Str::from(session_id)),
    );
    ("unix-session".to_string(), details)
}

async fn query_logind_display_session(connection: &Connection, uid: u32) -> Option<String> {
    let manager = Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .ok()?;
    let user_path: OwnedObjectPath = manager.call("GetUser", &(uid,)).await.ok()?;
    let user_proxy = Proxy::new(
        connection,
        "org.freedesktop.login1",
        user_path,
        "org.freedesktop.login1.User",
    )
    .await
    .ok()?;
    let (session_id, _): (String, OwnedObjectPath) =
        user_proxy.get_property("Display").await.ok()?;
    let trimmed = session_id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

async fn query_logind_pid_session(connection: &Connection, pid: u32) -> Option<String> {
    let manager = Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .ok()?;
    let session_path: OwnedObjectPath = manager.call("GetSessionByPID", &(pid,)).await.ok()?;
    let session_proxy = Proxy::new(
        connection,
        "org.freedesktop.login1",
        session_path,
        "org.freedesktop.login1.Session",
    )
    .await
    .ok()?;
    let session_id: String = session_proxy.get_property("Id").await.ok()?;
    let trimmed = session_id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn resolve_polkit_locale(host_locale: Option<&str>) -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("C")
                && !trimmed.eq_ignore_ascii_case("C.UTF-8")
                && !trimmed.eq_ignore_ascii_case("POSIX")
            {
                return trimmed.to_string();
            }
        }
    }

    if let Some(locale) = host_locale {
        let lower = locale.trim().to_ascii_lowercase();
        if lower.starts_with("zh-tw")
            || lower.starts_with("zh-hk")
            || lower.starts_with("zh-hant")
        {
            return "zh_TW.UTF-8".to_string();
        }
        if lower.starts_with("zh") {
            return "zh_CN.UTF-8".to_string();
        }
        if lower.starts_with("ja") {
            return "ja_JP.UTF-8".to_string();
        }
        if lower.starts_with("ko") {
            return "ko_KR.UTF-8".to_string();
        }
        let normalized = locale.trim().replace('-', "_");
        if normalized.len() >= 2 && normalized.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return format!("{normalized}.UTF-8");
        }
    }

    "en_US.UTF-8".to_string()
}

fn resolve_identity_users(identities: &[(String, HashMap<String, OwnedValue>)]) -> Vec<String> {
    let preferred_uid = current_uid();
    let mut users = Vec::new();
    let mut preferred_first = Vec::new();

    for (kind, props) in identities {
        if kind != "unix-user" {
            continue;
        }
        let Some(uid_val) = props.get("uid") else {
            continue;
        };
        let uid: Option<u32> = uid_val.downcast_ref::<u32>().ok().or_else(|| {
            uid_val
                .downcast_ref::<i32>()
                .ok()
                .and_then(|v| u32::try_from(v).ok())
        });
        let Some(uid) = uid else {
            continue;
        };
        let Some(account) = UserAccount::lookup(uid) else {
            continue;
        };
        if account.name.is_empty() {
            continue;
        }
        if uid == preferred_uid {
            if !preferred_first.contains(&account.name) {
                preferred_first.push(account.name);
            }
        } else if !users.contains(&account.name) {
            users.push(account.name);
        }
    }

    preferred_first.extend(users);
    preferred_first
}

fn resolve_auth_icon_png(
    action_id: &str,
    icon_name: &str,
    details: &HashMap<String, String>,
) -> Option<String> {
    if action_id == "io.focusstage.launcher.verify" {
        return None;
    }
    let dirs = desktop::launcher_application_dirs();
    let icons = DesktopIcons::new(&dirs);
    if !icon_name.trim().is_empty() {
        if let Some(path) = icons.resolve(icon_name.trim()) {
            if let Some(png) = icons.png(&path) {
                return Some(base64::engine::general_purpose::STANDARD.encode(png));
            }
        }
    }
    if let Some(program) = details.get("program") {
        if let Some(stem) = Path::new(program).file_name().and_then(|s| s.to_str()) {
            if let Some(app) = desktop::app_by_id(&dirs, stem) {
                if let Some(icon) = app.icon.as_deref() {
                    if let Some(path) = icons.resolve(icon) {
                        if let Some(png) = icons.png(&path) {
                            return Some(base64::engine::general_purpose::STANDARD.encode(png));
                        }
                    }
                }
            }
        }
    }
    None
}

enum HelperEvent {
    Output {
        cookie: String,
        attempt: u64,
        line: HelperLine,
    },
    Closed {
        cookie: String,
        attempt: u64,
    },
}

enum HelperLine {
    PromptEchoOff(String),
    PromptEchoOn(String),
    ErrorMsg(String),
    TextInfo(String),
    Success,
    Failure(Option<String>),
}

struct HelperProcess {
    writer: HelperWriter,
    cancel_handle: HelperCancel,
}

enum HelperWriter {
    Socket(UnixStream),
    ChildStdin(ChildStdin),
}

impl HelperWriter {
    fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        match self {
            Self::Socket(stream) => {
                stream.write_all(line.as_bytes())?;
                stream.write_all(b"\n")?;
                stream.flush()
            }
            Self::ChildStdin(stdin) => {
                stdin.write_all(line.as_bytes())?;
                stdin.write_all(b"\n")?;
                stdin.flush()
            }
        }
    }
}

enum HelperCancel {
    Socket(UnixStream),
    Child(Arc<Mutex<Option<Child>>>),
}

impl HelperCancel {
    fn cancel(self) {
        match self {
            Self::Socket(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Self::Child(child) => {
                if let Ok(mut guard) = child.lock() {
                    if let Some(mut c) = guard.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            }
        }
    }
}

impl HelperProcess {
    fn start(
        cookie: &str,
        user: &str,
        attempt: u64,
        tx: mpsc::UnboundedSender<HelperEvent>,
    ) -> Result<Self, String> {
        if Path::new(AGENT_HELPER_SOCKET).exists() {
            if let Ok(mut stream) = UnixStream::connect(AGENT_HELPER_SOCKET) {
                let header = format!("{user}\n{cookie}\n");
                if stream.write_all(header.as_bytes()).is_ok() && stream.flush().is_ok() {
                    let reader_stream = stream
                        .try_clone()
                        .map_err(|e| format!("clone polkit socket: {e}"))?;
                    let cancel_stream = stream
                        .try_clone()
                        .map_err(|e| format!("clone polkit socket cancel handle: {e}"))?;
                    let cookie_owned = cookie.to_string();
                    thread::Builder::new()
                        .name("focusstage-polkit-helper".into())
                        .spawn(move || {
                            read_helper_lines(BufReader::new(reader_stream), cookie_owned, attempt, tx);
                        })
                        .map_err(|e| format!("spawn polkit reader thread: {e}"))?;
                    return Ok(Self {
                        writer: HelperWriter::Socket(stream),
                        cancel_handle: HelperCancel::Socket(cancel_stream),
                    });
                }
            }
        }

        let Some(helper_bin) = AGENT_HELPER_BINS.iter().find(|p| Path::new(p).is_file()) else {
            return Err("polkit-agent-helper-1 is unavailable".into());
        };
        let mut child = Command::new(helper_bin)
            .arg(user)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {helper_bin}: {e}"))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "polkit helper stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "polkit helper stdout unavailable".to_string())?;
        let stderr = child.stderr.take();

        stdin
            .write_all(format!("{cookie}\n").as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("write cookie to polkit helper: {e}"))?;

        let shared_child = Arc::new(Mutex::new(Some(child)));
        let reader_child = Arc::clone(&shared_child);
        let cookie_owned = cookie.to_string();

        if let Some(stderr) = stderr {
            let stderr_cookie = cookie_owned.clone();
            let stderr_tx = tx.clone();
            thread::Builder::new()
                .name("focusstage-polkit-helper-err".into())
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            debug!(cookie = %stderr_cookie, "polkit-agent-helper stderr: {trimmed}");
                            let _ = stderr_tx.send(HelperEvent::Output {
                                cookie: stderr_cookie.clone(),
                                attempt,
                                line: HelperLine::ErrorMsg(clean_helper_error(trimmed)),
                            });
                        }
                    }
                })
                .ok();
        }

        thread::Builder::new()
            .name("focusstage-polkit-helper".into())
            .spawn(move || {
                read_helper_lines(BufReader::new(stdout), cookie_owned, attempt, tx);
                if let Ok(mut guard) = reader_child.lock() {
                    if let Some(mut c) = guard.take() {
                        let _ = c.wait();
                    }
                }
            })
            .map_err(|e| format!("spawn polkit stdout thread: {e}"))?;

        Ok(Self {
            writer: HelperWriter::ChildStdin(stdin),
            cancel_handle: HelperCancel::Child(shared_child),
        })
    }

    fn send_response(&mut self, response: &str) -> Result<(), String> {
        self.writer
            .send_line(response)
            .map_err(|e| format!("write response to polkit helper: {e}"))
    }

    fn cancel(self) {
        self.cancel_handle.cancel();
    }
}

fn read_helper_lines<R: std::io::Read>(
    reader: BufReader<R>,
    cookie: String,
    attempt: u64,
    tx: mpsc::UnboundedSender<HelperEvent>,
) {
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(parsed) = parse_helper_line(trimmed) {
            if tx
                .send(HelperEvent::Output {
                    cookie: cookie.clone(),
                    attempt,
                    line: parsed,
                })
                .is_err()
            {
                return;
            }
        }
    }
    let _ = tx.send(HelperEvent::Closed { cookie, attempt });
}

fn parse_helper_line(line: &str) -> Option<HelperLine> {
    if let Some(rest) = line.strip_prefix("PAM_PROMPT_ECHO_OFF") {
        return Some(HelperLine::PromptEchoOff(rest.trim().to_string()));
    }
    if let Some(rest) = line.strip_prefix("PAM_PROMPT_ECHO_ON") {
        return Some(HelperLine::PromptEchoOn(rest.trim().to_string()));
    }
    if let Some(rest) = line.strip_prefix("PAM_ERROR_MSG") {
        return Some(HelperLine::ErrorMsg(clean_helper_error(rest.trim())));
    }
    if let Some(rest) = line.strip_prefix("PAM_TEXT_INFO") {
        return Some(HelperLine::TextInfo(rest.trim().to_string()));
    }
    if line == "SUCCESS" {
        return Some(HelperLine::Success);
    }
    if line == "FAILURE" {
        return Some(HelperLine::Failure(None));
    }
    if line.starts_with("polkit-agent-helper-1:") {
        return Some(HelperLine::ErrorMsg(clean_helper_error(line)));
    }
    None
}

fn clean_helper_error(raw: &str) -> String {
    let trimmed = raw
        .strip_prefix("polkit-agent-helper-1:")
        .unwrap_or(raw)
        .trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("authentication failure") || lower.contains("incorrect password") {
        "密码不正确，请重试。".to_string()
    } else if lower.contains("max retries") || lower.contains("maximum number of tries") {
        "密码错误次数过多。".to_string()
    } else {
        trimmed.to_string()
    }
}

struct ActiveSession {
    cookie: String,
    action_id: String,
    message: String,
    icon_png: Option<String>,
    program_name: Option<String>,
    command_line: Option<String>,
    current_user: String,
    users: Vec<String>,
    attempt: u64,
    helper: Option<HelperProcess>,
    pending_response: Option<String>,
    ui_shown: bool,
    retry_pending: bool,
    info_buf: Vec<String>,
    error_buf: Vec<String>,
    reply: Option<oneshot::Sender<Result<(), PolkitDbusError>>>,
}

impl ActiveSession {
    fn take_info(&mut self) -> Option<String> {
        if self.info_buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.info_buf).join("\n"))
        }
    }

    fn take_error(&mut self) -> Option<String> {
        if self.error_buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.error_buf).join("\n"))
        }
    }

    fn finish_success(mut self, host_tx: &calloop_channel::Sender<PolkitHostCommand>) {
        if let Some(helper) = self.helper.take() {
            helper.cancel();
        }
        if self.ui_shown {
            let _ = host_tx.send(PolkitHostCommand::End {
                cookie: self.cookie,
                succeeded: true,
                message: None,
            });
        }
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Ok(()));
        }
    }

    fn finish_cancelled(
        mut self,
        host_tx: &calloop_channel::Sender<PolkitHostCommand>,
        message: Option<String>,
    ) {
        if let Some(helper) = self.helper.take() {
            helper.cancel();
        }
        if self.ui_shown {
            let _ = host_tx.send(PolkitHostCommand::End {
                cookie: self.cookie,
                succeeded: false,
                message,
            });
        }
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(PolkitDbusError::Cancelled(
                "Authentication was dismissed by the user".into(),
            )));
        }
    }

    fn finish_failed(
        mut self,
        host_tx: &calloop_channel::Sender<PolkitHostCommand>,
        reason: String,
    ) {
        if let Some(helper) = self.helper.take() {
            helper.cancel();
        }
        if self.ui_shown {
            let _ = host_tx.send(PolkitHostCommand::End {
                cookie: self.cookie,
                succeeded: false,
                message: Some(reason.clone()),
            });
        }
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(PolkitDbusError::Failed(reason)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_begin_auth(
    action_id: String,
    message: String,
    icon_name: String,
    details: HashMap<String, String>,
    cookie: String,
    users: Vec<String>,
    reply: oneshot::Sender<Result<(), PolkitDbusError>>,
    sessions: &mut HashMap<String, ActiveSession>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    helper_tx: &mpsc::UnboundedSender<HelperEvent>,
) {
    if let Some(old) = sessions.remove(&cookie) {
        old.finish_cancelled(host_tx, None);
    }

    let current_user = users[0].clone();
    let program_name = details
        .get("program")
        .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let command_line = details
        .get("command_line")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let icon_png = resolve_auth_icon_png(&action_id, &icon_name, &details);

    let attempt = 1u64;
    let helper = match HelperProcess::start(&cookie, &current_user, attempt, helper_tx.clone()) {
        Ok(helper) => helper,
        Err(err) => {
            warn!(%err, cookie = %cookie, "Failed to start polkit-agent-helper-1");
            let _ = reply.send(Err(PolkitDbusError::Failed(err)));
            return;
        }
    };

    info!(
        cookie = %cookie,
        action_id = %action_id,
        user = %current_user,
        "Polkit BeginAuthentication started"
    );

    sessions.insert(
        cookie.clone(),
        ActiveSession {
            cookie,
            action_id,
            message,
            icon_png,
            program_name,
            command_line,
            current_user,
            users,
            attempt,
            helper: Some(helper),
            pending_response: None,
            ui_shown: false,
            retry_pending: false,
            info_buf: Vec::new(),
            error_buf: Vec::new(),
            reply: Some(reply),
        },
    );
}

fn handle_host_reply(
    reply: PolkitHostReply,
    sessions: &mut HashMap<String, ActiveSession>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    helper_tx: &mpsc::UnboundedSender<HelperEvent>,
) {
    match reply {
        PolkitHostReply::HostDisconnected => {
            cancel_all_sessions(
                sessions,
                host_tx,
                Some("FocusStage Windows 宿主连接已断开".into()),
            );
        }
        PolkitHostReply::Cancel { cookie } => {
            if let Some(session) = sessions.remove(&cookie) {
                info!(cookie = %cookie, "Polkit authentication cancelled by user");
                session.finish_cancelled(host_tx, None);
            }
        }
        PolkitHostReply::Submit {
            cookie,
            user,
            response,
        } => {
            let Some(session) = sessions.get_mut(&cookie) else {
                return;
            };
            let target_user = if session.users.contains(&user) {
                user
            } else {
                session.current_user.clone()
            };

            let need_new_helper = target_user != session.current_user || session.helper.is_none();
            if need_new_helper {
                if let Some(old) = session.helper.take() {
                    old.cancel();
                }
                session.current_user = target_user;
                session.attempt += 1;
                session.pending_response = Some(response);
                match HelperProcess::start(
                    &cookie,
                    &session.current_user,
                    session.attempt,
                    helper_tx.clone(),
                ) {
                    Ok(helper) => {
                        session.helper = Some(helper);
                    }
                    Err(err) => {
                        if let Some(s) = sessions.remove(&cookie) {
                            s.finish_failed(host_tx, err);
                        }
                    }
                }
            } else if let Some(helper) = session.helper.as_mut() {
                if let Err(err) = helper.send_response(&response) {
                    if let Some(s) = sessions.remove(&cookie) {
                        s.finish_failed(host_tx, err);
                    }
                }
            }
        }
    }
}

fn handle_helper_event(
    event: HelperEvent,
    sessions: &mut HashMap<String, ActiveSession>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    helper_tx: &mpsc::UnboundedSender<HelperEvent>,
) {
    match event {
        HelperEvent::Output {
            cookie,
            attempt,
            line,
        } => {
            let Some(session) = sessions.get_mut(&cookie) else {
                return;
            };
            if session.attempt != attempt {
                return;
            }
            match line {
                HelperLine::TextInfo(msg) => {
                    if !msg.is_empty() {
                        session.info_buf.push(msg);
                    }
                }
                HelperLine::ErrorMsg(msg) => {
                    if !msg.is_empty() && !session.error_buf.contains(&msg) {
                        session.error_buf.push(msg);
                    }
                }
                HelperLine::PromptEchoOff(prompt) => {
                    if let Err(err) = handle_prompt_line(session, host_tx, prompt, false) {
                        if let Some(s) = sessions.remove(&cookie) {
                            s.finish_failed(host_tx, err);
                        }
                    }
                }
                HelperLine::PromptEchoOn(prompt) => {
                    if let Err(err) = handle_prompt_line(session, host_tx, prompt, true) {
                        if let Some(s) = sessions.remove(&cookie) {
                            s.finish_failed(host_tx, err);
                        }
                    }
                }
                HelperLine::Success => {
                    if let Some(s) = sessions.remove(&cookie) {
                        info!(cookie = %cookie, "Polkit authentication succeeded");
                        s.finish_success(host_tx);
                    }
                }
                HelperLine::Failure(msg) => {
                    if let Some(m) = msg {
                        if !m.is_empty() && !session.error_buf.contains(&m) {
                            session.error_buf.push(m);
                        }
                    }
                    if session.error_buf.is_empty() {
                        session.error_buf.push("密码不正确，请重试。".to_string());
                    }
                    restart_helper_for_retry(&cookie, sessions, host_tx, helper_tx);
                }
            }
        }
        HelperEvent::Closed { cookie, attempt } => {
            let Some(session) = sessions.get_mut(&cookie) else {
                return;
            };
            if session.attempt != attempt {
                return;
            }
            if session.error_buf.is_empty() {
                session.error_buf.push("认证未通过，请重试。".to_string());
            }
            restart_helper_for_retry(&cookie, sessions, host_tx, helper_tx);
        }
    }
}

fn handle_prompt_line(
    session: &mut ActiveSession,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    prompt: String,
    echo_on: bool,
) -> Result<(), String> {
    if let Some(pending) = session.pending_response.take() {
        if let Some(helper) = session.helper.as_mut() {
            helper.send_response(&pending)?;
        }
        return Ok(());
    }

    let prompt_text = if prompt.is_empty() {
        "Password:".to_string()
    } else {
        prompt
    };
    let info = session.take_info();
    let error = session.take_error();
    let retry = std::mem::take(&mut session.retry_pending);

    if !session.ui_shown {
        session.ui_shown = true;
        let _ = host_tx.send(PolkitHostCommand::Begin {
            cookie: session.cookie.clone(),
            action_id: session.action_id.clone(),
            message: session.message.clone(),
            icon_png: session.icon_png.clone(),
            program_name: session.program_name.clone(),
            command_line: session.command_line.clone(),
            user: session.current_user.clone(),
            users: session.users.clone(),
            prompt: prompt_text,
            echo_on,
            info,
            error,
        });
    } else {
        let _ = host_tx.send(PolkitHostCommand::Prompt {
            cookie: session.cookie.clone(),
            prompt: prompt_text,
            echo_on,
            info,
            error,
            retry,
        });
    }
    Ok(())
}

fn restart_helper_for_retry(
    cookie: &str,
    sessions: &mut HashMap<String, ActiveSession>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    helper_tx: &mpsc::UnboundedSender<HelperEvent>,
) {
    let Some(session) = sessions.get_mut(cookie) else {
        return;
    };
    if !session.ui_shown {
        let err = session
            .take_error()
            .unwrap_or_else(|| "Polkit authentication helper exited unexpectedly".into());
        if let Some(s) = sessions.remove(cookie) {
            s.finish_failed(host_tx, err);
        }
        return;
    }

    if let Some(old) = session.helper.take() {
        old.cancel();
    }
    session.pending_response = None;
    session.retry_pending = true;
    session.attempt += 1;

    match HelperProcess::start(
        cookie,
        &session.current_user,
        session.attempt,
        helper_tx.clone(),
    ) {
        Ok(helper) => {
            session.helper = Some(helper);
        }
        Err(err) => {
            if let Some(s) = sessions.remove(cookie) {
                s.finish_failed(host_tx, err);
            }
        }
    }
}

fn cancel_all_sessions(
    sessions: &mut HashMap<String, ActiveSession>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    reason: Option<String>,
) {
    let cookies: Vec<String> = sessions.keys().cloned().collect();
    for cookie in cookies {
        if let Some(session) = sessions.remove(&cookie) {
            session.finish_cancelled(host_tx, reason.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_polkit_helper_protocol_lines() {
        assert!(matches!(
            parse_helper_line("PAM_PROMPT_ECHO_OFF Password: "),
            Some(HelperLine::PromptEchoOff(p)) if p == "Password:"
        ));
        assert!(matches!(
            parse_helper_line("PAM_PROMPT_ECHO_ON Verification code:"),
            Some(HelperLine::PromptEchoOn(p)) if p == "Verification code:"
        ));
        assert!(matches!(
            parse_helper_line("SUCCESS"),
            Some(HelperLine::Success)
        ));
        assert!(matches!(
            parse_helper_line("FAILURE"),
            Some(HelperLine::Failure(None))
        ));
        assert!(matches!(
            parse_helper_line("polkit-agent-helper-1: pam_authenticate failed: Authentication failure"),
            Some(HelperLine::ErrorMsg(msg)) if msg.contains("密码不正确")
        ));
    }

    #[test]
    fn resolves_locale_from_host_when_wsl_is_c_utf8() {
        assert_eq!(resolve_polkit_locale(Some("zh-CN")), "zh_CN.UTF-8");
        assert_eq!(resolve_polkit_locale(Some("zh-TW")), "zh_TW.UTF-8");
        assert_eq!(resolve_polkit_locale(Some("en-US")), "en_US.UTF-8");
    }
}
