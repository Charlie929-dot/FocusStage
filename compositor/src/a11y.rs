use std::collections::HashSet;
use std::io::ErrorKind;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tokio::time::{sleep, timeout_at, Instant};
use tracing::{debug, warn};
use zbus::address::transport::{Transport, UnixSocket};
use zbus::proxy::{Builder, CacheProperties};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Address, Connection, Proxy};

use crate::protocol::{A11yBounds, A11yNode};

const BUS_NAME: &str = "org.a11y.Bus";
const BUS_PATH: &str = "/org/a11y/bus";
const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const QUERY_TIMEOUT: Duration = Duration::from_millis(800);
const PREPARE_TIMEOUT: Duration = Duration::from_millis(1500);
const REPLY_MARGIN: Duration = Duration::from_millis(40);
const MAX_DEPTH: usize = 12;
const NODE_BUDGET: usize = 320;

type ObjRef = (String, OwnedObjectPath);

struct AtspiBus {
    address: Address,
    connection: Connection,
}

struct A11yQuery {
    pids: HashSet<u32>,
    deadline: Instant,
    reply: mpsc::Sender<Vec<A11yNode>>,
}

#[derive(Clone)]
pub struct A11yCollector {
    tx: mpsc::Sender<A11yQuery>,
}

impl A11yCollector {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<A11yQuery>();
        thread::Builder::new()
            .name("focusstage-a11y".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(err) => {
                        warn!(%err, "Failed to start the AT-SPI collector");
                        return;
                    }
                };
                rt.block_on(async move {
                    let mut bus: Option<AtspiBus> = None;
                    while let Ok(query) = rx.recv() {
                        if query.deadline <= Instant::now() {
                            continue;
                        }
                        if bus.as_ref().is_some_and(|bus| socket_missing(&bus.address)) {
                            bus = None;
                        }
                        if bus.is_none() {
                            bus = match timeout_at(query.deadline, connect_atspi_bus()).await {
                                Ok(Ok(bus)) => Some(bus),
                                Ok(Err(err)) => {
                                    warn!(%err, "Could not connect to the AT-SPI bus");
                                    None
                                }
                                Err(_) => {
                                    warn!("Timed out connecting to the AT-SPI bus");
                                    None
                                }
                            };
                        }
                        let mut nodes = Vec::new();
                        if let Some(bus_ref) = bus.as_ref().filter(|_| !query.pids.is_empty()) {
                            match collect_for_pids(&bus_ref.connection, &query.pids, query.deadline).await {
                                Ok(tree) => nodes = tree,
                                Err(err) => {
                                    warn!(%err, "Could not read the AT-SPI registry; will reconnect on the next request");
                                    bus = None;
                                }
                            }
                        }
                        let _ = query.reply.send(nodes);
                    }
                });
            })
            .expect("spawn focusstage-a11y thread");
        Self { tx }
    }

    /// Set the session's accessibility status before starting applications that read it at startup.
    pub fn prepare_for_launch(&self) {
        self.query(HashSet::new(), PREPARE_TIMEOUT);
    }

    pub fn query_pids(&self, pids: HashSet<u32>) -> Vec<A11yNode> {
        if pids.is_empty() {
            return Vec::new();
        }
        self.query(pids, QUERY_TIMEOUT)
    }

    fn query(&self, pids: HashSet<u32>, timeout: Duration) -> Vec<A11yNode> {
        let (reply, rx) = mpsc::channel();
        let deadline = Instant::now() + timeout - REPLY_MARGIN;
        if self
            .tx
            .send(A11yQuery {
                pids,
                deadline,
                reply,
            })
            .is_err()
        {
            return Vec::new();
        }
        match rx.recv_timeout(timeout) {
            Ok(nodes) => nodes,
            Err(err) => {
                warn!(%err, "The AT-SPI collector did not reply");
                Vec::new()
            }
        }
    }
}

async fn connect_atspi_bus() -> zbus::Result<AtspiBus> {
    let session = Connection::session().await;
    if let Ok(session) = &session {
        enable_accessibility(session).await;
    }
    if let Ok(address) = std::env::var("AT_SPI_BUS_ADDRESS") {
        if !address.is_empty() {
            let connection = match address.as_str().try_into() {
                Ok(address) => connect_address(address).await,
                Err(err) => Err(err),
            };
            match connection {
                Ok(conn) => return Ok(conn),
                Err(err) => {
                    warn!(%err, "Could not connect to AT_SPI_BUS_ADDRESS; checking the session bus")
                }
            }
        }
    }

    let session = session?;
    let address = get_address(&session).await?;
    let result = connect_address(address.clone()).await;
    let Err(err) = result else {
        return result;
    };
    warn!(%err, %address, "Connection to the session's AT-SPI socket failed");
    if is_stale_socket(&address, &err) {
        match session
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "RestartUnit",
                &("at-spi-dbus-bus.service", "replace"),
            )
            .await
        {
            Ok(_) => {
                // RestartUnit queues a job. Wait for the new launcher to publish its socket.
                for _ in 0..12 {
                    sleep(Duration::from_millis(40)).await;
                    if let Ok(address) = get_address(&session).await {
                        if let Ok(conn) = connect_address(address).await {
                            enable_accessibility(&session).await;
                            return Ok(conn);
                        }
                    }
                }
                warn!("Restarted at-spi-dbus-bus.service but could not reconnect");
            }
            Err(restart_err) => warn!(%restart_err, "Could not restart at-spi-dbus-bus.service"),
        }
    }
    // Re-read the address in case the service changed between GetAddress and connect.
    connect_address(get_address(&session).await?).await
}

async fn enable_accessibility(session: &Connection) {
    let status = match proxy(session, BUS_NAME, BUS_PATH, "org.a11y.Status").await {
        Ok(status) => status,
        Err(err) => {
            warn!(%err, "Could not access org.a11y.Status");
            return;
        }
    };
    for property in ["IsEnabled", "ScreenReaderEnabled"] {
        if matches!(status.get_property::<bool>(property).await, Ok(true)) {
            continue;
        }
        if let Err(err) = status.set_property(property, true).await {
            warn!(%err, property, "Could not enable the AT-SPI session status");
        }
    }
}

async fn get_address(session: &Connection) -> zbus::Result<Address> {
    let reply = session
        .call_method(Some(BUS_NAME), BUS_PATH, Some(BUS_NAME), "GetAddress", &())
        .await?;
    let address: String = reply.body().deserialize()?;
    address.as_str().try_into()
}

async fn connect_address(address: Address) -> zbus::Result<AtspiBus> {
    let connection = zbus::connection::Builder::address(address.clone())?
        .build()
        .await?;
    Ok(AtspiBus {
        address,
        connection,
    })
}

fn socket_path(address: &Address) -> Option<&std::path::Path> {
    match address.transport() {
        Transport::Unix(unix) => match unix.path() {
            UnixSocket::File(path) => Some(path),
            _ => None,
        },
        _ => None,
    }
}

fn socket_missing(address: &Address) -> bool {
    socket_path(address).is_some_and(|path| matches!(path.try_exists(), Ok(false)))
}

fn is_stale_socket(address: &Address, err: &zbus::Error) -> bool {
    socket_missing(address)
        || (socket_path(address).is_some()
            && matches!(err, zbus::Error::InputOutput(err) if err.kind() == ErrorKind::ConnectionRefused))
}

async fn proxy<'a>(
    conn: &Connection,
    bus: &'a str,
    path: &'a str,
    interface: &'static str,
) -> zbus::Result<Proxy<'a>> {
    Builder::new(conn)
        .destination(bus)?
        .path(path)?
        .interface(interface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn collect_for_pids(
    conn: &Connection,
    pids: &HashSet<u32>,
    deadline: Instant,
) -> zbus::Result<Vec<A11yNode>> {
    let registry = proxy(conn, "org.a11y.atspi.Registry", ROOT_PATH, ACCESSIBLE).await?;
    let apps: Vec<ObjRef> = timeout_at(deadline, registry.call("GetChildren", &()))
        .await
        .map_err(|_| {
            zbus::Error::Failure("AT-SPI registry did not respond before the deadline".into())
        })??;

    let mut result = Vec::new();
    let mut budget = NODE_BUDGET;
    for (bus, path) in apps {
        if budget == 0 || Instant::now() >= deadline {
            break;
        }
        let pid_args = (&bus,);
        let pid_call = conn.call_method(
            Some(DBUS_NAME),
            DBUS_PATH,
            Some(DBUS_NAME),
            "GetConnectionUnixProcessID",
            &pid_args,
        );
        let Ok(Ok(reply)) = timeout_at(deadline, pid_call).await else {
            continue;
        };
        let Ok(pid) = reply.body().deserialize::<u32>() else {
            continue;
        };
        if pids.contains(&pid) {
            if let Some(node) = read_tree(conn, &bus, path.as_str(), 0, &mut budget, deadline).await
            {
                result.push(node);
            }
        }
    }
    if budget == 0 || Instant::now() >= deadline {
        debug!(
            nodes_read = NODE_BUDGET - budget,
            "Returning partial AT-SPI tree"
        );
    }
    Ok(result)
}

async fn read_tree(
    conn: &Connection,
    bus: &str,
    path: &str,
    depth: usize,
    budget: &mut usize,
    deadline: Instant,
) -> Option<A11yNode> {
    if *budget == 0 || Instant::now() >= deadline {
        return None;
    }
    *budget -= 1;
    let accessible = proxy(conn, bus, path, ACCESSIBLE).await.ok()?;
    let mut node = timeout_at(deadline, read_node(conn, &accessible, bus, path, depth))
        .await
        .ok()??;
    if depth < MAX_DEPTH && *budget > 0 {
        let read_children = accessible.call::<_, _, Vec<ObjRef>>("GetChildren", &());
        if let Ok(Ok(children)) = timeout_at(deadline, read_children).await {
            for (child_bus, child_path) in children {
                if *budget == 0 || Instant::now() >= deadline {
                    break;
                }
                let child_bus = if child_bus.is_empty() {
                    bus
                } else {
                    &child_bus
                };
                if let Some(child) = Box::pin(read_tree(
                    conn,
                    child_bus,
                    child_path.as_str(),
                    depth + 1,
                    budget,
                    deadline,
                ))
                .await
                {
                    node.children.push(child);
                }
            }
        }
    }
    Some(node)
}

async fn read_node(
    conn: &Connection,
    accessible: &Proxy<'_>,
    bus: &str,
    path: &str,
    depth: usize,
) -> Option<A11yNode> {
    let state: Vec<u32> = accessible.call("GetState", &()).await.unwrap_or_default();
    let states = state_names(&state);
    // Application roots often have no geometric state. Some bridges mark onscreen
    // containers VISIBLE or SHOWING, so either is sufficient for traversal.
    if depth > 1
        && (state_has(&state, 6)
            || (state.iter().any(|&word| word != 0)
                && !states.contains(&"visible")
                && !states.contains(&"showing")))
    {
        return None;
    }
    let role = accessible
        .call("GetLocalizedRoleName", &())
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let name: String = accessible.get_property("Name").await.unwrap_or_default();
    let description: String = accessible
        .get_property("Description")
        .await
        .unwrap_or_default();
    let interfaces: Vec<String> = accessible
        .call("GetInterfaces", &())
        .await
        .unwrap_or_default();

    let mut text = None;
    if interfaces
        .iter()
        .any(|interface| interface == "org.a11y.atspi.Text")
    {
        if let Ok(text_proxy) = proxy(conn, bus, path, "org.a11y.atspi.Text").await {
            let count: i32 = text_proxy.get_property("CharacterCount").await.unwrap_or(0);
            if count > 0 {
                text = text_proxy
                    .call("GetText", &(0i32, count.min(512)))
                    .await
                    .ok();
            }
        }
    }

    let mut bounds = None;
    if interfaces
        .iter()
        .any(|interface| interface == "org.a11y.atspi.Component")
    {
        if let Ok(component) = proxy(conn, bus, path, "org.a11y.atspi.Component").await {
            // 1 is the window-relative coordinate space.
            if let Ok((x, y, width, height)) = component
                .call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(1u32,))
                .await
            {
                if width > 0 && height > 0 {
                    bounds = Some(A11yBounds {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
        }
    }

    Some(A11yNode {
        role,
        name,
        text,
        description: (!description.is_empty()).then_some(description),
        states: states.into_iter().map(String::from).collect(),
        bounds,
        children: Vec::new(),
    })
}

fn state_has(words: &[u32], bit: usize) -> bool {
    words
        .get(bit / 32)
        .is_some_and(|word| word & (1 << (bit % 32)) != 0)
}

fn state_names(words: &[u32]) -> Vec<&'static str> {
    // AT-SPI encodes state IDs as bit indices in an array of 32-bit words.
    [
        (4, "checked"),
        (7, "editable"),
        (8, "enabled"),
        (12, "focused"),
        (23, "selected"),
        (25, "showing"),
        (30, "visible"),
    ]
    .into_iter()
    .filter_map(|(bit, name)| state_has(words, bit).then_some(name))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_atspi_state_bits() {
        let words = [(1 << 7) | (1 << 8) | (1 << 12) | (1 << 25) | (1 << 30), 1];
        assert_eq!(
            state_names(&words),
            ["editable", "enabled", "focused", "showing", "visible"]
        );
        assert_eq!(
            state_names(&[(1 << 4) | (1 << 23)]),
            ["checked", "selected"]
        );
        assert!(state_names(&[]).is_empty());
    }
}
