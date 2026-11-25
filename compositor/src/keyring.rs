use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use smithay::reexports::calloop::channel as calloop_channel;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::proxy::{Builder as ProxyBuilder, CacheProperties};
use zbus::{interface, Connection, Proxy};

use crate::manager::{stop_existing_user_process, stop_helper_unit};
use crate::polkit::{PolkitHostCommand, PolkitHostReply};
use crate::session_env::{current_uid, UserAccount};

const PROMPTER_BUS_NAME: &str = "org.gnome.keyring.SystemPrompter";
const PROMPTER_OBJECT_PATH: &str = "/org/gnome/keyring/Prompter";
const CALLBACK_IFACE: &str = "org.gnome.keyring.internal.Prompter.Callback";

const SECRETS_BUS_NAME: &str = "org.freedesktop.secrets";
const SECRETS_SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SECRETS_SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const SECRETS_COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const GNOME_KEYRING_INTERNAL_IFACE: &str = "org.gnome.keyring.InternalUnsupportedGuiltRiddenInterface";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyringStatus {
    pub available: bool,
    pub default_collection: Option<String>,
    pub label: Option<String>,
    pub locked: Option<bool>,
}

impl Default for KeyringStatus {
    fn default() -> Self {
        Self {
            available: false,
            default_collection: None,
            label: None,
            locked: None,
        }
    }
}

pub fn query_keyring_status() -> KeyringStatus {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return KeyringStatus::default();
    };
    rt.block_on(query_keyring_status_async())
}

pub fn unlock_default_keyring(password: &str) -> Result<bool, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("初始化运行时失败: {e}"))?;
    rt.block_on(unlock_default_keyring_async(password))
}

pub fn lock_default_keyring() -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("初始化运行时失败: {e}"))?;
    rt.block_on(lock_default_keyring_async())
}

async fn uncached_collection_proxy(
    connection: &Connection,
    collection_path: OwnedObjectPath,
) -> zbus::Result<Proxy<'static>> {
    ProxyBuilder::new(connection)
        .destination(SECRETS_BUS_NAME)?
        .path(collection_path)?
        .interface(SECRETS_COLLECTION_IFACE)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn query_keyring_status_async() -> KeyringStatus {
    let Ok(connection) = Connection::session().await else {
        return KeyringStatus::default();
    };
    let Ok(service) = Proxy::new(
        &connection,
        SECRETS_BUS_NAME,
        SECRETS_SERVICE_PATH,
        SECRETS_SERVICE_IFACE,
    )
    .await
    else {
        return KeyringStatus::default();
    };

    let Ok(collection_path) = service.call::<_, _, OwnedObjectPath>("ReadAlias", &("default",)).await else {
        return KeyringStatus::default();
    };

    let path_str = collection_path.as_str().to_string();
    if path_str == "/" || path_str.is_empty() {
        return KeyringStatus {
            available: true,
            default_collection: None,
            label: None,
            locked: None,
        };
    }

    let Ok(col_proxy) = uncached_collection_proxy(&connection, collection_path).await else {
        return KeyringStatus {
            available: true,
            default_collection: Some(path_str),
            label: None,
            locked: None,
        };
    };

    let locked = col_proxy.get_property::<bool>("Locked").await.ok();
    let label = col_proxy.get_property::<String>("Label").await.ok();

    KeyringStatus {
        available: true,
        default_collection: Some(path_str),
        label,
        locked,
    }
}

async fn unlock_default_keyring_async(password: &str) -> Result<bool, String> {
    let connection = Connection::session()
        .await
        .map_err(|e| format!("无法连接会话 D-Bus: {e}"))?;
    let service = Proxy::new(
        &connection,
        SECRETS_BUS_NAME,
        SECRETS_SERVICE_PATH,
        SECRETS_SERVICE_IFACE,
    )
    .await
    .map_err(|e| format!("无法访问 Secret Service: {e}"))?;

    let collection_path: OwnedObjectPath = service
        .call("ReadAlias", &("default",))
        .await
        .map_err(|e| format!("读取默认密钥环失败: {e}"))?;
    if collection_path.as_str() == "/" || collection_path.as_str().is_empty() {
        return Err("未找到默认 GNOME 密钥环（Default keyring）。".into());
    }

    let col_proxy = uncached_collection_proxy(&connection, collection_path.clone())
        .await
        .map_err(|e| format!("访问密钥环集合失败: {e}"))?;

    if matches!(col_proxy.get_property::<bool>("Locked").await, Ok(false)) {
        return Ok(true);
    }

    let (_output, session_path): (OwnedValue, OwnedObjectPath) = service
        .call("OpenSession", &("plain", Value::from("")))
        .await
        .map_err(|e| format!("打开 Secret Service 会话失败: {e}"))?;

    let internal = Proxy::new(
        &connection,
        SECRETS_BUS_NAME,
        SECRETS_SERVICE_PATH,
        GNOME_KEYRING_INTERNAL_IFACE,
    )
    .await
    .map_err(|e| format!("无法访问 GNOME Keyring 解锁接口: {e}"))?;

    let secret = (
        session_path,
        Vec::<u8>::new(),
        password.as_bytes().to_vec(),
        "text/plain".to_string(),
    );

    let call_res: zbus::Result<()> = internal
        .call("UnlockWithMasterPassword", &(collection_path, secret))
        .await;
    if let Err(err) = call_res {
        let msg = err.to_string();
        if msg.contains("invalid") || msg.contains("password") || msg.contains("incorrect") {
            return Ok(false);
        }
        return Err(format!("解锁密钥环失败: {err}"));
    }

    let locked = col_proxy
        .get_property::<bool>("Locked")
        .await
        .unwrap_or(true);
    Ok(!locked)
}

async fn lock_default_keyring_async() -> Result<(), String> {
    let connection = Connection::session()
        .await
        .map_err(|e| format!("无法连接会话 D-Bus: {e}"))?;
    let service = Proxy::new(
        &connection,
        SECRETS_BUS_NAME,
        SECRETS_SERVICE_PATH,
        SECRETS_SERVICE_IFACE,
    )
    .await
    .map_err(|e| format!("无法访问 Secret Service: {e}"))?;

    let collection_path: OwnedObjectPath = service
        .call("ReadAlias", &("default",))
        .await
        .map_err(|e| format!("读取默认密钥环失败: {e}"))?;
    if collection_path.as_str() == "/" || collection_path.as_str().is_empty() {
        return Err("未找到默认 GNOME 密钥环。".into());
    }

    let (locked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = service
        .call("Lock", &(vec![collection_path],))
        .await
        .map_err(|e| format!("锁定密钥环失败: {e}"))?;
    if locked.is_empty() && prompt.as_str() != "/" {
        return Err("锁定密钥环需要交互确认。".into());
    }
    Ok(())
}

struct GcrLib {
    _gcr: libloading::Library,
    _glib: libloading::Library,
    _gobject: libloading::Library,
    exchange_new: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    exchange_receive: unsafe extern "C" fn(*mut c_void, *const c_char, isize) -> c_int,
    exchange_send: unsafe extern "C" fn(*mut c_void, *const c_char, isize) -> *mut c_char,
    object_unref: unsafe extern "C" fn(*mut c_void),
    g_free: unsafe extern "C" fn(*mut c_void),
}

impl GcrLib {
    fn load() -> Result<Self, String> {
        let gcr_candidates = [
            "libgcr-base-3.so.1",
            "/usr/lib/x86_64-linux-gnu/libgcr-base-3.so.1",
            "/usr/lib/aarch64-linux-gnu/libgcr-base-3.so.1",
        ];
        let gcr_path = gcr_candidates
            .iter()
            .find(|p| !p.starts_with('/') || Path::new(p).exists())
            .copied()
            .unwrap_or("libgcr-base-3.so.1");

        unsafe {
            let gcr = libloading::Library::new(gcr_path)
                .map_err(|e| format!("load {gcr_path}: {e}"))?;
            let glib = libloading::Library::new("libglib-2.0.so.0")
                .map_err(|e| format!("load libglib-2.0.so.0: {e}"))?;
            let gobject = libloading::Library::new("libgobject-2.0.so.0")
                .map_err(|e| format!("load libgobject-2.0.so.0: {e}"))?;

            let exchange_new = *gcr
                .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"gcr_secret_exchange_new\0")
                .map_err(|e| format!("symbol gcr_secret_exchange_new: {e}"))?;
            let exchange_receive = *gcr
                .get::<unsafe extern "C" fn(*mut c_void, *const c_char, isize) -> c_int>(
                    b"gcr_secret_exchange_receive\0",
                )
                .map_err(|e| format!("symbol gcr_secret_exchange_receive: {e}"))?;
            let exchange_send = *gcr
                .get::<unsafe extern "C" fn(*mut c_void, *const c_char, isize) -> *mut c_char>(
                    b"gcr_secret_exchange_send\0",
                )
                .map_err(|e| format!("symbol gcr_secret_exchange_send: {e}"))?;
            let object_unref = *gobject
                .get::<unsafe extern "C" fn(*mut c_void)>(b"g_object_unref\0")
                .map_err(|e| format!("symbol g_object_unref: {e}"))?;
            let g_free = *glib
                .get::<unsafe extern "C" fn(*mut c_void)>(b"g_free\0")
                .map_err(|e| format!("symbol g_free: {e}"))?;

            Ok(Self {
                _gcr: gcr,
                _glib: glib,
                _gobject: gobject,
                exchange_new,
                exchange_receive,
                exchange_send,
                object_unref,
                g_free,
            })
        }
    }

    fn new_exchange(&self) -> Option<SecretExchange> {
        let ptr = unsafe { (self.exchange_new)(std::ptr::null()) };
        if ptr.is_null() {
            None
        } else {
            Some(SecretExchange { ptr })
        }
    }

    fn receive(&self, ex: &SecretExchange, data: &str) -> bool {
        let Ok(cstr) = CString::new(data) else {
            return false;
        };
        unsafe { (self.exchange_receive)(ex.ptr, cstr.as_ptr(), -1) != 0 }
    }

    fn send(&self, ex: &SecretExchange, secret: &str) -> Option<String> {
        let bytes = secret.as_bytes();
        let len = isize::try_from(bytes.len()).ok()?;
        let ptr = unsafe { (self.exchange_send)(ex.ptr, bytes.as_ptr().cast(), len) };
        if ptr.is_null() {
            return None;
        }
        let out = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        unsafe { (self.g_free)(ptr.cast()) };
        Some(out)
    }

    fn unref(&self, ex: SecretExchange) {
        unsafe { (self.object_unref)(ex.ptr) };
    }
}

struct SecretExchange {
    ptr: *mut c_void,
}

// GcrSecretExchange is only used on the dedicated single-threaded keyring runtime thread.
unsafe impl Send for SecretExchange {}

enum PrompterControl {
    EnsureRegistered(std_mpsc::Sender<bool>),
    HostReply(PolkitHostReply),
    Shutdown,
}

enum PrompterDbusEvent {
    Begin {
        sender: String,
        callback: OwnedObjectPath,
    },
    Perform {
        sender: String,
        callback: OwnedObjectPath,
        prompt_type: String,
        properties: HashMap<String, OwnedValue>,
        exchange: String,
    },
    Stop {
        sender: String,
        callback: OwnedObjectPath,
    },
}

#[derive(Clone)]
struct DbusPrompterObject {
    tx: mpsc::UnboundedSender<PrompterDbusEvent>,
}

#[interface(name = "org.gnome.keyring.internal.Prompter")]
impl DbusPrompterObject {
    #[zbus(name = "BeginPrompting")]
    async fn begin_prompting(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        callback: OwnedObjectPath,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let _ = self.tx.send(PrompterDbusEvent::Begin { sender, callback });
        Ok(())
    }

    #[zbus(name = "PerformPrompt")]
    async fn perform_prompt(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        callback: OwnedObjectPath,
        prompt_type: String,
        properties: HashMap<String, OwnedValue>,
        exchange: String,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let _ = self.tx.send(PrompterDbusEvent::Perform {
            sender,
            callback,
            prompt_type,
            properties,
            exchange,
        });
        Ok(())
    }

    #[zbus(name = "StopPrompting")]
    async fn stop_prompting(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        callback: OwnedObjectPath,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let _ = self.tx.send(PrompterDbusEvent::Stop { sender, callback });
        Ok(())
    }
}

pub struct KeyringPrompterHandle {
    ctrl_tx: mpsc::UnboundedSender<PrompterControl>,
    registered: Arc<AtomicBool>,
}

impl KeyringPrompterHandle {
    pub fn spawn(host_tx: calloop_channel::Sender<PolkitHostCommand>) -> Result<Self, String> {
        let gcr = GcrLib::load()?;
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let registered = Arc::new(AtomicBool::new(false));
        let registered_flag = Arc::clone(&registered);

        thread::Builder::new()
            .name("focusstage-keyring".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        warn!(%err, "Failed to initialize FocusStage Keyring prompter runtime");
                        return;
                    }
                };
                rt.block_on(run_prompter_loop(ctrl_rx, host_tx, registered_flag, gcr));
            })
            .map_err(|e| format!("spawn focusstage-keyring thread: {e}"))?;

        Ok(Self {
            ctrl_tx,
            registered,
        })
    }

    pub fn ensure_registered(&self) -> bool {
        if self.registered.load(Ordering::Acquire) {
            return true;
        }
        let (tx, rx) = std_mpsc::channel();
        if self.ctrl_tx.send(PrompterControl::EnsureRegistered(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(Duration::from_millis(1500)).unwrap_or(false)
    }

    pub fn send_host_reply(&self, reply: PolkitHostReply) {
        let _ = self.ctrl_tx.send(PrompterControl::HostReply(reply));
    }
}

impl Drop for KeyringPrompterHandle {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(PrompterControl::Shutdown);
    }
}

struct ActivePrompt {
    cookie: String,
    sender: String,
    callback: OwnedObjectPath,
    prompt_type: String,
    exchange: Option<SecretExchange>,
    dialog_open: bool,
    awaiting_reply: bool,
}

enum LoopMsg {
    Ctrl(PrompterControl),
    Dbus(PrompterDbusEvent),
}

async fn run_prompter_loop(
    mut ctrl_rx: mpsc::UnboundedReceiver<PrompterControl>,
    host_tx: calloop_channel::Sender<PolkitHostCommand>,
    registered: Arc<AtomicBool>,
    gcr: GcrLib,
) {
    stop_legacy_keyring_prompters();

    let connection = match Connection::session().await {
        Ok(conn) => conn,
        Err(err) => {
            warn!(%err, "Failed to connect to session D-Bus for FocusStage Keyring prompter");
            drain_until_shutdown(&mut ctrl_rx).await;
            return;
        }
    };

    let (dbus_tx, mut dbus_rx) = mpsc::unbounded_channel::<PrompterDbusEvent>();
    if let Err(err) = connection
        .object_server()
        .at(PROMPTER_OBJECT_PATH, DbusPrompterObject { tx: dbus_tx })
        .await
    {
        warn!(%err, "Failed to export FocusStage Keyring SystemPrompter interface");
        drain_until_shutdown(&mut ctrl_rx).await;
        return;
    }

    let ok = claim_prompter_name(&connection).await;
    registered.store(ok, Ordering::Release);

    let (loop_tx, mut loop_rx) = mpsc::unbounded_channel::<LoopMsg>();
    {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                let is_shutdown = matches!(msg, PrompterControl::Shutdown);
                if tx.send(LoopMsg::Ctrl(msg)).is_err() || is_shutdown {
                    break;
                }
            }
        });
    }
    {
        let tx = loop_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = dbus_rx.recv().await {
                if tx.send(LoopMsg::Dbus(msg)).is_err() {
                    break;
                }
            }
        });
    }
    drop(loop_tx);

    let mut sessions: HashMap<String, ActivePrompt> = HashMap::new();
    let seq = AtomicU64::new(1);

    while let Some(msg) = loop_rx.recv().await {
        match msg {
            LoopMsg::Ctrl(PrompterControl::EnsureRegistered(reply)) => {
                let ok = if registered.load(Ordering::Acquire) {
                    true
                } else {
                    stop_legacy_keyring_prompters();
                    let ok = claim_prompter_name(&connection).await;
                    registered.store(ok, Ordering::Release);
                    ok
                };
                let _ = reply.send(ok);
            }
            LoopMsg::Ctrl(PrompterControl::Shutdown) => {
                let cookies: Vec<String> = sessions.keys().cloned().collect();
                for cookie in cookies {
                    if let Some(mut s) = sessions.remove(&cookie) {
                        let _ = call_prompt_ready(&connection, &s.sender, &s.callback, "no", "").await;
                        if let Some(ex) = s.exchange.take() {
                            gcr.unref(ex);
                        }
                        if s.dialog_open {
                            let _ = host_tx.send(PolkitHostCommand::End {
                                cookie,
                                succeeded: false,
                                message: None,
                            });
                        }
                    }
                }
                break;
            }
            LoopMsg::Ctrl(PrompterControl::HostReply(reply)) => {
                handle_host_reply(reply, &connection, &mut sessions, &host_tx, &gcr).await;
            }
            LoopMsg::Dbus(ev) => {
                handle_dbus_event(ev, &connection, &mut sessions, &host_tx, &gcr, &seq).await;
            }
        }
    }

    registered.store(false, Ordering::Release);
}

async fn drain_until_shutdown(ctrl_rx: &mut mpsc::UnboundedReceiver<PrompterControl>) {
    while let Some(ctrl) = ctrl_rx.recv().await {
        match ctrl {
            PrompterControl::EnsureRegistered(reply) => {
                let _ = reply.send(false);
            }
            PrompterControl::Shutdown => break,
            PrompterControl::HostReply(_) => {}
        }
    }
}

fn stop_legacy_keyring_prompters() {
    stop_helper_unit("focusstage-gcr-prompter");
    stop_existing_user_process("gcr-prompter");
}

async fn claim_prompter_name(connection: &Connection) -> bool {
    let Ok(dbus) = DBusProxy::new(connection).await else {
        return false;
    };
    let flags = zbus::fdo::RequestNameFlags::ReplaceExisting
        | zbus::fdo::RequestNameFlags::AllowReplacement;
    match dbus
        .request_name(PROMPTER_BUS_NAME.try_into().unwrap(), flags)
        .await
    {
        Ok(reply) => {
            info!(?reply, "Registered native FocusStage Keyring SystemPrompter");
            true
        }
        Err(err) => {
            warn!(%err, "Failed to claim {PROMPTER_BUS_NAME}");
            false
        }
    }
}

async fn handle_dbus_event(
    ev: PrompterDbusEvent,
    connection: &Connection,
    sessions: &mut HashMap<String, ActivePrompt>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    gcr: &GcrLib,
    seq: &AtomicU64,
) {
    match ev {
        PrompterDbusEvent::Begin { sender, callback } => {
            let key = session_key(&sender, &callback);
            if let Some(old_cookie) = find_cookie_by_key(sessions, &key) {
                if let Some(mut old) = sessions.remove(&old_cookie) {
                    if let Some(ex) = old.exchange.take() {
                        gcr.unref(ex);
                    }
                    if old.dialog_open {
                        let _ = host_tx.send(PolkitHostCommand::End {
                            cookie: old_cookie,
                            succeeded: false,
                            message: None,
                        });
                    }
                }
            }

            let cookie = format!("keyring-{}", seq.fetch_add(1, Ordering::Relaxed));
            sessions.insert(
                cookie,
                ActivePrompt {
                    cookie: String::new(),
                    sender: sender.clone(),
                    callback: callback.clone(),
                    prompt_type: String::new(),
                    exchange: gcr.new_exchange(),
                    dialog_open: false,
                    awaiting_reply: false,
                },
            );
            // Fix cookie field inside entry
            if let Some(entry) = sessions.values_mut().find(|p| p.sender == sender && p.callback == callback) {
                // Re-key isn't needed; find_cookie_by_key fills it.
                let _ = entry;
            }
            for (c, p) in sessions.iter_mut() {
                if p.cookie.is_empty() {
                    p.cookie = c.clone();
                }
            }

            if let Err(err) = call_prompt_ready(connection, &sender, &callback, "", "").await {
                debug!(%err, "Failed initial PromptReady callback");
            }
        }
        PrompterDbusEvent::Perform {
            sender,
            callback,
            prompt_type,
            properties,
            exchange,
        } => {
            let key = session_key(&sender, &callback);
            let cookie = find_cookie_by_key(sessions, &key).unwrap_or_else(|| {
                let c = format!("keyring-{}", seq.fetch_add(1, Ordering::Relaxed));
                sessions.insert(
                    c.clone(),
                    ActivePrompt {
                        cookie: c.clone(),
                        sender: sender.clone(),
                        callback: callback.clone(),
                        prompt_type: prompt_type.clone(),
                        exchange: gcr.new_exchange(),
                        dialog_open: false,
                        awaiting_reply: false,
                    },
                );
                c
            });

            let Some(prompt) = sessions.get_mut(&cookie) else {
                return;
            };
            prompt.prompt_type = prompt_type.clone();
            if !exchange.is_empty() {
                if prompt.exchange.is_none() {
                    prompt.exchange = gcr.new_exchange();
                }
                if let Some(ref ex) = prompt.exchange {
                    let _ = gcr.receive(ex, &exchange);
                }
            }

            let msg = prop_str(&properties, "message")
                .or_else(|| prop_str(&properties, "description"))
                .unwrap_or_else(|| "应用请求访问 GNOME 密钥环，需要验证密码以解锁。".to_string());
            let translated_msg = translate_keyring_message(&msg);
            let warning = prop_str(&properties, "warning").map(|w| translate_keyring_warning(&w));
            let is_confirm = prompt_type == "confirm";
            let is_new_pw = prop_bool(&properties, "password-new").unwrap_or(false);
            let prompt_label = if is_confirm {
                String::new()
            } else if is_new_pw {
                "设置新密钥环密码:".to_string()
            } else {
                "密钥环密码:".to_string()
            };

            let username = UserAccount::lookup(current_uid())
                .map(|u| u.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "ubuntu".to_string()));
            let distro = std::env::var("WSL_DISTRO_NAME").ok().filter(|s| !s.trim().is_empty());

            prompt.awaiting_reply = true;
            if !prompt.dialog_open {
                prompt.dialog_open = true;
                let action_id = if is_confirm {
                    "org.gnome.keyring.confirm".to_string()
                } else {
                    "org.gnome.keyring.unlock".to_string()
                };
                let _ = host_tx.send(PolkitHostCommand::Begin {
                    cookie,
                    distro,
                    action_id,
                    message: translated_msg,
                    icon_png: None,
                    program_name: Some("gnome-keyring-daemon".to_string()),
                    command_line: prop_str(&properties, "description"),
                    user: username.clone(),
                    users: vec![username],
                    prompt: prompt_label,
                    echo_on: false,
                    info: None,
                    error: warning,
                });
            } else {
                let _ = host_tx.send(PolkitHostCommand::Prompt {
                    cookie,
                    prompt: prompt_label,
                    echo_on: false,
                    info: None,
                    error: warning.or_else(|| Some("密钥环密码不正确，请重试。".to_string())),
                    retry: true,
                });
            }
        }
        PrompterDbusEvent::Stop { sender, callback } => {
            let key = session_key(&sender, &callback);
            if let Some(cookie) = find_cookie_by_key(sessions, &key) {
                if let Some(mut s) = sessions.remove(&cookie) {
                    if let Some(ex) = s.exchange.take() {
                        gcr.unref(ex);
                    }
                    if s.dialog_open {
                        let _ = host_tx.send(PolkitHostCommand::End {
                            cookie,
                            succeeded: true,
                            message: None,
                        });
                    }
                }
            }
            let _ = call_prompt_done(connection, &sender, &callback).await;
        }
    }
}

async fn handle_host_reply(
    reply: PolkitHostReply,
    connection: &Connection,
    sessions: &mut HashMap<String, ActivePrompt>,
    host_tx: &calloop_channel::Sender<PolkitHostCommand>,
    gcr: &GcrLib,
) {
    match reply {
        PolkitHostReply::Submit {
            cookie,
            user: _,
            response,
        } => {
            let Some(prompt) = sessions.get_mut(&cookie) else {
                return;
            };
            if !prompt.awaiting_reply {
                return;
            }
            prompt.awaiting_reply = false;
            let exchange_out = if prompt.prompt_type == "confirm" {
                String::new()
            } else if let Some(ref ex) = prompt.exchange {
                match gcr.send(ex, &response) {
                    Some(encoded) => encoded,
                    None => {
                        let _ = call_prompt_ready(
                            connection,
                            &prompt.sender,
                            &prompt.callback,
                            "no",
                            "",
                        )
                        .await;
                        return;
                    }
                }
            } else {
                String::new()
            };

            if let Err(err) = call_prompt_ready(
                connection,
                &prompt.sender,
                &prompt.callback,
                "yes",
                &exchange_out,
            )
            .await
            {
                warn!(%err, "Failed to send Keyring PromptReady reply");
                if let Some(mut s) = sessions.remove(&cookie) {
                    if let Some(ex) = s.exchange.take() {
                        gcr.unref(ex);
                    }
                    if s.dialog_open {
                        let _ = host_tx.send(PolkitHostCommand::End {
                            cookie,
                            succeeded: false,
                            message: Some("无法将密钥环凭据发回 Linux 服务。".into()),
                        });
                    }
                }
            }
        }
        PolkitHostReply::Cancel { cookie } => {
            if let Some(mut s) = sessions.remove(&cookie) {
                let _ = call_prompt_ready(connection, &s.sender, &s.callback, "no", "").await;
                if let Some(ex) = s.exchange.take() {
                    gcr.unref(ex);
                }
                if s.dialog_open {
                    let _ = host_tx.send(PolkitHostCommand::End {
                        cookie,
                        succeeded: false,
                        message: None,
                    });
                }
            }
        }
        PolkitHostReply::HostDisconnected => {
            let cookies: Vec<String> = sessions.keys().cloned().collect();
            for cookie in cookies {
                if let Some(mut s) = sessions.remove(&cookie) {
                    let _ = call_prompt_ready(connection, &s.sender, &s.callback, "no", "").await;
                    if let Some(ex) = s.exchange.take() {
                        gcr.unref(ex);
                    }
                }
            }
        }
    }
}

async fn call_prompt_ready(
    connection: &Connection,
    destination: &str,
    callback: &OwnedObjectPath,
    reply: &str,
    exchange: &str,
) -> zbus::Result<()> {
    let proxy = Proxy::new(connection, destination, callback.as_str(), CALLBACK_IFACE).await?;
    let props: HashMap<&str, Value<'_>> = HashMap::new();
    proxy.call::<_, _, ()>("PromptReady", &(reply, props, exchange)).await
}

async fn call_prompt_done(
    connection: &Connection,
    destination: &str,
    callback: &OwnedObjectPath,
) -> zbus::Result<()> {
    let proxy = Proxy::new(connection, destination, callback.as_str(), CALLBACK_IFACE).await?;
    proxy.call::<_, _, ()>("PromptDone", &()).await
}

fn session_key(sender: &str, callback: &OwnedObjectPath) -> String {
    format!("{sender}::{}", callback.as_str())
}

fn find_cookie_by_key(sessions: &HashMap<String, ActivePrompt>, key: &str) -> Option<String> {
    sessions
        .iter()
        .find(|(_, p)| session_key(&p.sender, &p.callback) == key)
        .map(|(c, _)| c.clone())
}

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<String>().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| v.downcast_ref::<bool>().ok())
}

fn translate_keyring_message(raw: &str) -> String {
    if raw.contains("wants access to the keyring") && raw.contains("but it is locked") {
        if let Some(start) = raw.find('“').or_else(|| raw.find('"')) {
            let after = &raw[start..];
            let quote_len = after.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            let rest = &after[quote_len..];
            if let Some(end) = rest.find('”').or_else(|| rest.find('"')) {
                let name = &rest[..end];
                return format!("应用请求访问密钥环 “{name}”，但该密钥环当前已锁定，需要输入密码解锁。");
            }
        }
        return "应用请求访问 GNOME 密钥环，但该密钥环当前已锁定，需要输入密码解锁。".to_string();
    }
    if raw.contains("Enter password to unlock") {
        return "请输入密钥环密码以解锁加密凭据库。".to_string();
    }
    raw.to_string()
}

fn translate_keyring_warning(raw: &str) -> String {
    if raw.eq_ignore_ascii_case("The password was incorrect")
        || raw.contains("password was incorrect")
    {
        return "密钥环密码不正确，请重试。".to_string();
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_standard_gnome_keyring_messages() {
        let msg = translate_keyring_message(
            "An application wants access to the keyring “Default keyring”, but it is locked",
        );
        assert!(msg.contains("Default keyring"));
        assert!(msg.contains("已锁定"));

        assert_eq!(
            translate_keyring_warning("The password was incorrect"),
            "密钥环密码不正确，请重试。"
        );
    }

    #[test]
    fn secret_service_lock_reply_signature_matches_dbus_spec() {
        use zbus::zvariant::Type;
        assert_eq!(
            <(Vec<OwnedObjectPath>, OwnedObjectPath) as Type>::SIGNATURE.to_string(),
            "(aoo)"
        );
    }
}
