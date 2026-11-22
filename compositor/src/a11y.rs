use std::collections::HashSet;
use std::sync::mpsc;
use std::thread;
use tracing::{debug, warn};
use zbus::zvariant::OwnedObjectPath;

use crate::protocol::{A11yBounds, A11yNode};

#[derive(Debug)]
struct A11yQuery {
    pids: HashSet<u32>,
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
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        warn!("Failed to build tokio runtime for AT-SPI collector: {}", err);
                        return;
                    }
                };
                rt.block_on(async move {
                    let mut conn: Option<zbus::Connection> = None;
                    while let Ok(query) = rx.recv() {
                        if conn.is_none() {
                            conn = connect_atspi_bus().await;
                        }
                        let nodes = if let Some(ref c) = conn {
                            match collect_for_pids(c, &query.pids).await {
                                Ok(n) => n,
                                Err(err) => {
                                    debug!("AT-SPI collection error (reconnecting next call): {}", err);
                                    conn = None;
                                    Vec::new()
                                }
                            }
                        } else {
                            Vec::new()
                        };
                        let _ = query.reply.send(nodes);
                    }
                });
            })
            .expect("spawn focusstage-a11y thread");

        Self { tx }
    }

    pub fn query_pids(&self, pids: HashSet<u32>) -> Vec<A11yNode> {
        if pids.is_empty() {
            return Vec::new();
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(A11yQuery { pids, reply: reply_tx }).is_err() {
            return Vec::new();
        }
        reply_rx
            .recv_timeout(std::time::Duration::from_millis(800))
            .unwrap_or_default()
    }
}

async fn connect_atspi_bus() -> Option<zbus::Connection> {
    if let Ok(addr) = std::env::var("AT_SPI_BUS_ADDRESS") {
        if !addr.is_empty() {
            if let Ok(c) = zbus::connection::Builder::address(addr.as_str())
                .ok()?
                .build()
                .await
            {
                return Some(c);
            }
        }
    }

    let session = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            debug!("No D-Bus session bus for AT-SPI: {}", err);
            return None;
        }
    };

    let reply = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .await;

    let addr: String = match reply {
        Ok(msg) => match msg.body().deserialize() {
            Ok(s) => s,
            Err(_) => return None,
        },
        Err(err) => {
            debug!("org.a11y.Bus GetAddress unavailable: {}", err);
            return None;
        }
    };

    match zbus::connection::Builder::address(addr.as_str())
        .ok()?
        .build()
        .await
    {
        Ok(c) => Some(c),
        Err(err) => {
            debug!("Failed connecting to AT-SPI bus {}: {}", addr, err);
            None
        }
    }
}

type ObjRef = (String, OwnedObjectPath);

async fn collect_for_pids(conn: &zbus::Connection, pids: &HashSet<u32>) -> zbus::Result<Vec<A11yNode>> {
    let root_path = "/org/a11y/atspi/accessible/root";
    let registry_bus = "org.a11y.atspi.Registry";

    let children_msg = conn
        .call_method(
            Some(registry_bus),
            root_path,
            Some("org.a11y.atspi.Accessible"),
            "GetChildren",
            &(),
        )
        .await?;
    let apps: Vec<ObjRef> = children_msg.body().deserialize()?;

    let mut result = Vec::new();
    for (bus_name, obj_path) in apps {
        let pid_msg = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionUnixProcessID",
                &(&bus_name,),
            )
            .await;

        let app_pid: u32 = match pid_msg {
            Ok(m) => m.body().deserialize().unwrap_or(0),
            Err(_) => 0,
        };

        if !pids.contains(&app_pid) {
            continue;
        }

        if let Some(node) = read_node_recursive(conn, &bus_name, obj_path.as_str(), 0, 5, 160).await {
            result.push(node);
        }
    }
    Ok(result)
}

async fn read_node_recursive(
    conn: &zbus::Connection,
    bus_name: &str,
    path: &str,
    depth: usize,
    max_depth: usize,
    max_children: usize,
) -> Option<A11yNode> {
    if depth > max_depth {
        return None;
    }

    let proxy = zbus::Proxy::new(
        conn,
        bus_name,
        path,
        "org.a11y.atspi.Accessible",
    )
    .await
    .ok()?;

    let name: String = proxy.get_property("Name").await.unwrap_or_default();
    let description: String = proxy.get_property("Description").await.unwrap_or_default();
    let role: String = proxy
        .call("GetLocalizedRoleName", &())
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    let interfaces: Vec<String> = proxy.call("GetInterfaces", &()).await.unwrap_or_default();
    let has_text = interfaces.iter().any(|i| i == "org.a11y.atspi.Text");
    let has_component = interfaces.iter().any(|i| i == "org.a11y.atspi.Component");

    let text = if has_text {
        let text_proxy = zbus::Proxy::new(conn, bus_name, path, "org.a11y.atspi.Text")
            .await
            .ok();
        if let Some(tp) = text_proxy {
            let count: i32 = tp.get_property("CharacterCount").await.unwrap_or(0);
            if count > 0 {
                let end = count.min(512);
                tp.call::<_, _, String>("GetText", &(0i32, end)).await.ok()
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let bounds = if has_component {
        let comp_proxy = zbus::Proxy::new(conn, bus_name, path, "org.a11y.atspi.Component")
            .await
            .ok();
        if let Some(cp) = comp_proxy {
            // coord_type 1 = window-relative
            if let Ok((x, y, w, h)) = cp.call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(1u32,)).await {
                if w > 0 || h > 0 {
                    Some(A11yBounds { x, y, width: w, height: h })
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut children = Vec::new();
    if depth < max_depth {
        if let Ok(raw_children) = proxy.call::<_, _, Vec<ObjRef>>("GetChildren", &()).await {
            for (cbus, cpath) in raw_children.into_iter().take(max_children) {
                let target_bus = if cbus.is_empty() { bus_name } else { cbus.as_str() };
                if let Some(child) =
                    Box::pin(read_node_recursive(conn, target_bus, cpath.as_str(), depth + 1, max_depth, max_children))
                        .await
                {
                    children.push(child);
                }
            }
        }
    }

    Some(A11yNode {
        role,
        name,
        text,
        description: if description.is_empty() { None } else { Some(description) },
        states: Vec::new(),
        bounds,
        children,
    })
}
