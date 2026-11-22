use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc;

use smithay::backend::input::AxisSource;
use tracing::{info, warn};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
    wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::protocol::SavedRect;
use crate::sink::{DisplaySink, SinkInputEvent};

struct ShmSlot {
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    _fd: OwnedFd,
    ptr: NonNull<u8>,
    map_len: usize,
    width: u32,
    height: u32,
    busy: bool,
}

unsafe impl Send for ShmSlot {}

impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.map_len);
        }
    }
}

impl ShmSlot {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<WaylandClientState>, width: u32, height: u32) -> Result<Self, String> {
        let w = width.max(1);
        let h = height.max(1);
        let stride = (w * 4) as i32;
        let size = (stride as usize) * (h as usize);

        let name = CString::new("focusstage-wslg-shm").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw_fd < 0 {
            return Err(format!("memfd_create failed: {}", std::io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
            return Err(format!("ftruncate failed: {}", std::io::Error::last_os_error()));
        }
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }
        let ptr = NonNull::new(mapped as *mut u8).ok_or("null mmap")?;

        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride, wl_shm::Format::Argb8888, qh, ());
        Ok(Self {
            pool,
            buffer,
            _fd: fd,
            ptr,
            map_len: size,
            width: w,
            height: h,
            busy: false,
        })
    }

    fn write_bgra(&mut self, bgra: &[u8]) {
        let copy_len = bgra.len().min(self.map_len);
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), self.ptr.as_ptr(), copy_len);
        }
    }
}

struct WaylandStageWindow {
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    configured: bool,
    mapped: bool,
    rect: SavedRect,
    title: String,
    wm_class: String,
    slots: Vec<ShmSlot>,
    last_frame: Option<(u32, u32, Vec<u8>)>,
}

struct WaylandClientState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    decoration_mgr: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    data_device_mgr: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,

    windows: HashMap<String, WaylandStageWindow>,
    surface_to_stage: HashMap<u32, String>,
    xdg_to_stage: HashMap<u32, String>,
    toplevel_to_stage: HashMap<u32, String>,

    pointer_stage: Option<String>,
    pointer_pos: (f64, f64),
    pending_scroll: PendingScrollFrame,
    keyboard_stage: Option<String>,
    last_pointer_serial: u32,
    last_input_serial: u32,

    host_clipboard_text: String,
    clipboard_source: Option<wl_data_source::WlDataSource>,
    clipboard_tx: mpsc::Sender<String>,
    pending_offers: HashMap<u32, Vec<String>>,
    dnd_offer: Option<wl_data_offer::WlDataOffer>,
    dnd_stage: Option<String>,
    dnd_pos: (f64, f64),
    just_configured: Vec<String>,

    events: Vec<SinkInputEvent>,
}

#[derive(Default)]
struct PendingScrollFrame {
    has_event: bool,
    dx: f64,
    dy: f64,
    source: Option<AxisSource>,
    v120: Option<(i32, i32)>,
    stop: (bool, bool),
    time: u32,
}

impl PendingScrollFrame {
    fn flush(&mut self, stage_id: &str, out: &mut Vec<SinkInputEvent>) {
        if !self.has_event {
            return;
        }
        let mut dx = self.dx;
        let mut dy = self.dy;
        if let Some((vx, vy)) = self.v120 {
            if dx == 0.0 && vx != 0 {
                dx = (vx as f64 / 120.0) * 15.0;
            }
            if dy == 0.0 && vy != 0 {
                dy = (vy as f64 / 120.0) * 15.0;
            }
        }
        if dx != 0.0 || dy != 0.0 || self.v120.is_some() || self.stop.0 || self.stop.1 {
            out.push(SinkInputEvent::PointerScroll {
                stage_id: stage_id.to_string(),
                dx,
                dy,
                source: self.source,
                v120: self.v120,
                stop: self.stop,
                time: self.time,
            });
        }
        *self = Self::default();
    }
}

impl WaylandClientState {
    fn new(clipboard_tx: mpsc::Sender<String>) -> Self {
        Self {
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            pointer: None,
            keyboard: None,
            decoration_mgr: None,
            data_device_mgr: None,
            data_device: None,
            windows: HashMap::new(),
            surface_to_stage: HashMap::new(),
            xdg_to_stage: HashMap::new(),
            toplevel_to_stage: HashMap::new(),
            pointer_stage: None,
            pointer_pos: (0.0, 0.0),
            pending_scroll: PendingScrollFrame::default(),
            keyboard_stage: None,
            last_pointer_serial: 0,
            last_input_serial: 0,
            host_clipboard_text: String::new(),
            clipboard_source: None,
            clipboard_tx,
            pending_offers: HashMap::new(),
            dnd_offer: None,
            dnd_stage: None,
            dnd_pos: (0.0, 0.0),
            just_configured: Vec::new(),
            events: Vec::new(),
        }
    }
}

pub struct WslgWaylandSink {
    conn: Connection,
    queue: EventQueue<WaylandClientState>,
    qh: QueueHandle<WaylandClientState>,
    state: WaylandClientState,
    clipboard_rx: mpsc::Receiver<String>,
}

fn find_wslg_wayland_socket() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("WSLG_WAYLAND_DISPLAY") {
        if !explicit.is_empty() {
            let p = PathBuf::from(&explicit);
            if p.is_absolute() && p.exists() {
                return Some(p);
            }
            if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
                let cand = PathBuf::from(rt).join(&explicit);
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
    }
    if let Ok(disp) = std::env::var("WAYLAND_DISPLAY") {
        if !disp.is_empty() && !disp.starts_with("focusstage") {
            let p = PathBuf::from(&disp);
            if p.is_absolute() && p.exists() {
                return Some(p);
            }
            if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
                let cand = PathBuf::from(rt).join(&disp);
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        let cand = PathBuf::from(rt).join("wayland-0");
        if cand.exists() {
            return Some(cand);
        }
    }
    let fallback = PathBuf::from("/mnt/wslg/runtime-dir/wayland-0");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

impl WslgWaylandSink {
    pub fn connect() -> Result<Self, String> {
        let sock_path = find_wslg_wayland_socket()
            .ok_or_else(|| "WSLg wayland-0 socket not found".to_string())?;
        let stream = UnixStream::connect(&sock_path)
            .map_err(|e| format!("connect WSLg Wayland {}: {e}", sock_path.display()))?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| format!("wayland Connection::from_socket: {e}"))?;

        let mut queue: EventQueue<WaylandClientState> = conn.new_event_queue();
        let qh = queue.handle();
        let (clipboard_tx, clipboard_rx) = mpsc::channel();
        let mut state = WaylandClientState::new(clipboard_tx);

        let _registry = conn.display().get_registry(&qh, ());
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("wayland roundtrip 1: {e}"))?;
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("wayland roundtrip 2: {e}"))?;

        if state.compositor.is_none() || state.shm.is_none() || state.wm_base.is_none() {
            return Err("WSLg Wayland compositor missing wl_compositor/wl_shm/xdg_wm_base".to_string());
        }

        info!(
            "Connected WslgWaylandSink to {} (csd_decoration_mgr={})",
            sock_path.display(),
            state.decoration_mgr.is_some()
        );
        Ok(Self {
            conn,
            queue,
            qh,
            state,
            clipboard_rx,
        })
    }

    fn map_window(&mut self, stage_id: &str, title: &str, wm_class: &str, rect: SavedRect) {
        let clean_title = if title.trim().is_empty() {
            "FocusStage"
        } else {
            title.trim()
        };
        let cls = if wm_class.trim().is_empty() {
            "FocusStage"
        } else {
            wm_class.trim()
        };

        let existing_mapped = self
            .state
            .windows
            .get(stage_id)
            .map(|w| w.mapped)
            .unwrap_or(false);

        if existing_mapped {
            if let Some(win) = self.state.windows.get_mut(stage_id) {
                win.title = clean_title.to_string();
                win.wm_class = cls.to_string();
                win.rect = rect;
                if let Some(ref tl) = win.toplevel {
                    tl.set_title(clean_title.to_string());
                    tl.set_app_id(cls.to_string());
                }
            }
            let _ = self.conn.flush();
            return;
        }

        let Some(comp) = self.state.compositor.clone() else {
            return;
        };
        let Some(wm_base) = self.state.wm_base.clone() else {
            return;
        };

        let surface = comp.create_surface(&self.qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg_surface.get_toplevel(&self.qh, ());
        toplevel.set_title(clean_title.to_string());
        toplevel.set_app_id(cls.to_string());

        let decoration = self.state.decoration_mgr.as_ref().map(|mgr| {
            let dec = mgr.get_toplevel_decoration(&toplevel, &self.qh, ());
            dec.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
            dec
        });

        let surf_id = surface.id().protocol_id();
        let xdg_id = xdg_surface.id().protocol_id();
        let tl_id = toplevel.id().protocol_id();

        self.state.surface_to_stage.insert(surf_id, stage_id.to_string());
        self.state.xdg_to_stage.insert(xdg_id, stage_id.to_string());
        self.state.toplevel_to_stage.insert(tl_id, stage_id.to_string());

        let mut win = self.state.windows.remove(stage_id).unwrap_or(WaylandStageWindow {
            surface: None,
            xdg_surface: None,
            toplevel: None,
            decoration: None,
            configured: false,
            mapped: false,
            rect,
            title: clean_title.to_string(),
            wm_class: cls.to_string(),
            slots: Vec::new(),
            last_frame: None,
        });

        win.surface = Some(surface.clone());
        win.xdg_surface = Some(xdg_surface);
        win.toplevel = Some(toplevel);
        win.decoration = decoration;
        win.configured = false;
        win.mapped = true;
        win.rect = rect;
        win.title = clean_title.to_string();
        win.wm_class = cls.to_string();
        self.state.windows.insert(stage_id.to_string(), win);

        surface.commit();
        let _ = self.queue.roundtrip(&mut self.state);

        let last = self
            .state
            .windows
            .get(stage_id)
            .and_then(|w| w.last_frame.clone());
        if let Some((w, h, buf)) = last {
            self.present(stage_id, w, h, &buf);
        }
    }

    fn unmap_window(&mut self, stage_id: &str) {
        if let Some(win) = self.state.windows.get_mut(stage_id) {
            win.mapped = false;
            win.configured = false;
            if let Some(dec) = win.decoration.take() {
                dec.destroy();
            }
            if let Some(tl) = win.toplevel.take() {
                self.state.toplevel_to_stage.remove(&tl.id().protocol_id());
                tl.destroy();
            }
            if let Some(xs) = win.xdg_surface.take() {
                self.state.xdg_to_stage.remove(&xs.id().protocol_id());
                xs.destroy();
            }
            if let Some(s) = win.surface.take() {
                self.state.surface_to_stage.remove(&s.id().protocol_id());
                s.destroy();
            }
            win.slots.clear();
            let _ = self.conn.flush();
        }
    }
}

impl DisplaySink for WslgWaylandSink {
    fn show(&mut self, stage_id: &str, title: &str, wm_class: &str, rect: SavedRect) {
        self.map_window(stage_id, title, wm_class, rect);
    }

    fn hide(&mut self, stage_id: &str) -> Option<SavedRect> {
        let rect = self.state.windows.get(stage_id).map(|w| w.rect)?;
        self.unmap_window(stage_id);
        Some(rect)
    }

    fn destroy(&mut self, stage_id: &str) {
        self.unmap_window(stage_id);
        self.state.windows.remove(stage_id);
    }

    fn present(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]) {
        if width == 0 || height == 0 || bgra.len() < (width * height * 4) as usize {
            return;
        }
        let Some(shm) = self.state.shm.clone() else {
            return;
        };
        let Some(win) = self.state.windows.get_mut(stage_id) else {
            return;
        };
        win.last_frame = Some((width, height, bgra.to_vec()));
        if !win.mapped || !win.configured {
            return;
        }
        let Some(ref surface) = win.surface else {
            return;
        };

        // Drop slots whose dimensions no longer match and are free
        win.slots.retain(|s| !s.busy && s.width == width && s.height == height || s.busy);

        let slot_idx = win
            .slots
            .iter()
            .position(|s| !s.busy && s.width == width && s.height == height);

        let idx = match slot_idx {
            Some(i) => i,
            None => match ShmSlot::new(&shm, &self.qh, width, height) {
                Ok(slot) => {
                    win.slots.push(slot);
                    win.slots.len() - 1
                }
                Err(e) => {
                    warn!("ShmSlot::new failed for {}: {}", stage_id, e);
                    return;
                }
            },
        };

        win.slots[idx].write_bgra(bgra);
        win.slots[idx].busy = true;
        surface.attach(Some(&win.slots[idx].buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        let _ = self.conn.flush();
    }

    fn set_title(&mut self, stage_id: &str, title: &str) {
        let clean = if title.trim().is_empty() {
            "FocusStage"
        } else {
            title.trim()
        };
        if let Some(win) = self.state.windows.get_mut(stage_id) {
            win.title = clean.to_string();
            if let Some(ref tl) = win.toplevel {
                tl.set_title(clean.to_string());
                let _ = self.conn.flush();
            }
        }
    }

    fn begin_move(&mut self, stage_id: &str) {
        let (Some(ref seat), Some(win)) = (&self.state.seat, self.state.windows.get(stage_id)) else {
            return;
        };
        if let Some(ref tl) = win.toplevel {
            tl._move(seat, self.state.last_pointer_serial);
            let _ = self.conn.flush();
        }
    }

    fn begin_resize(&mut self, stage_id: &str, direction: u32) {
        let (Some(ref seat), Some(win)) = (&self.state.seat, self.state.windows.get(stage_id)) else {
            return;
        };
        let edge = match direction {
            0 => xdg_toplevel::ResizeEdge::TopLeft,
            1 => xdg_toplevel::ResizeEdge::Top,
            2 => xdg_toplevel::ResizeEdge::TopRight,
            3 => xdg_toplevel::ResizeEdge::Right,
            4 => xdg_toplevel::ResizeEdge::BottomRight,
            5 => xdg_toplevel::ResizeEdge::Bottom,
            6 => xdg_toplevel::ResizeEdge::BottomLeft,
            7 => xdg_toplevel::ResizeEdge::Left,
            _ => return,
        };
        if let Some(ref tl) = win.toplevel {
            tl.resize(seat, self.state.last_pointer_serial, edge);
            let _ = self.conn.flush();
        }
    }

    fn minimize(&mut self, stage_id: &str) {
        if let Some(win) = self.state.windows.get(stage_id) {
            if let Some(ref tl) = win.toplevel {
                tl.set_minimized();
                let _ = self.conn.flush();
            }
        }
    }

    fn set_maximized(&mut self, stage_id: &str, maximized: bool) {
        if let Some(win) = self.state.windows.get(stage_id) {
            if let Some(ref tl) = win.toplevel {
                if maximized {
                    tl.set_maximized();
                } else {
                    tl.unset_maximized();
                }
                let _ = self.conn.flush();
            }
        }
    }

    fn set_host_clipboard(&mut self, text: &str) {
        if self.state.host_clipboard_text == text {
            return;
        }
        self.state.host_clipboard_text = text.to_string();
        if let (Some(ref mgr), Some(ref dev)) = (&self.state.data_device_mgr, &self.state.data_device) {
            let src = mgr.create_data_source(&self.qh, ());
            src.offer("text/plain;charset=utf-8".to_string());
            src.offer("UTF8_STRING".to_string());
            src.offer("text/plain".to_string());
            dev.set_selection(Some(&src), self.state.last_input_serial);
            self.state.clipboard_source = Some(src);
            let _ = self.conn.flush();
        }
    }

    fn poll_events(&mut self) -> Vec<SinkInputEvent> {
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
        if let Some(guard) = self.conn.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
        let ready_sids = std::mem::take(&mut self.state.just_configured);
        for sid in ready_sids {
            let last = self
                .state
                .windows
                .get(&sid)
                .and_then(|w| w.last_frame.clone());
            if let Some((w, h, buf)) = last {
                self.present(&sid, w, h, &buf);
            }
        }
        while let Ok(text) = self.clipboard_rx.try_recv() {
            if !text.is_empty() && text != self.state.host_clipboard_text {
                self.state.host_clipboard_text = text.clone();
                self.state.events.push(SinkInputEvent::HostClipboardChanged { text });
            }
        }
        std::mem::take(&mut self.state.events)
    }
}

fn read_fd_utf8_with_timeout(read_fd: OwnedFd, timeout_ms: i32) -> Option<String> {
    let mut file = std::fs::File::from(read_fd);
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(1) as u64);
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait_ms = remaining.as_millis().min(200) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= 1024 * 1024 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8(buf).ok()
}

fn read_offer_fd_utf8(
    conn: &Connection,
    offer: &wl_data_offer::WlDataOffer,
    mime: &str,
    timeout_ms: i32,
) -> Option<String> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    offer.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd);
    let _ = conn.flush();
    read_fd_utf8_with_timeout(read_fd, timeout_ms)
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        reg: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(reg.bind::<wl_compositor::WlCompositor, _, _>(name, version.min(5), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(reg.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(reg.bind::<xdg_wm_base::XdgWmBase, _, _>(name, version.min(5), qh, ()));
                }
                "wl_seat" => {
                    let seat = reg.bind::<wl_seat::WlSeat, _, _>(name, version.min(8), qh, ());
                    if let Some(ref mgr) = state.data_device_mgr {
                        state.data_device = Some(mgr.get_data_device(&seat, qh, ()));
                    }
                    state.seat = Some(seat);
                }
                "zxdg_decoration_manager_v1" => {
                    state.decoration_mgr = Some(reg.bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _, _>(name, 1, qh, ()));
                }
                "wl_data_device_manager" => {
                    let mgr = reg.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(name, version.min(3), qh, ());
                    if let Some(ref seat) = state.seat {
                        state.data_device = Some(mgr.get_data_device(seat, qh, ()));
                    }
                    state.data_device_mgr = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_buffer::WlBuffer, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            let bid = buffer.id();
            for win in state.windows.values_mut() {
                for slot in &mut win.slots {
                    if slot.buffer.id() == bid {
                        slot.busy = false;
                    }
                }
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandClientState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        xdg_surf: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surf.ack_configure(serial);
            if let Some(sid) = state.xdg_to_stage.get(&xdg_surf.id().protocol_id()).cloned() {
                if let Some(win) = state.windows.get_mut(&sid) {
                    let first = !win.configured;
                    win.configured = true;
                    if first && win.last_frame.is_some() {
                        state.just_configured.push(sid);
                    }
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        tl: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(sid) = state.toplevel_to_stage.get(&tl.id().protocol_id()).cloned() else {
            return;
        };
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    if let Some(win) = state.windows.get_mut(&sid) {
                        let w = width as u32;
                        let h = height as u32;
                        if win.rect.width != w || win.rect.height != h {
                            win.rect.width = w;
                            win.rect.height = h;
                            state.events.push(SinkInputEvent::Resized {
                                stage_id: sid,
                                rect: win.rect,
                            });
                        }
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                state.events.push(SinkInputEvent::CloseRequested { stage_id: sid });
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _: zxdg_decoration_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, _: zxdg_toplevel_decoration_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.last_pointer_serial = serial;
                state.last_input_serial = serial;
                state.pending_scroll = PendingScrollFrame::default();
                if let Some(sid) = state.surface_to_stage.get(&surface.id().protocol_id()).cloned() {
                    state.pointer_stage = Some(sid.clone());
                    state.pointer_pos = (surface_x, surface_y);
                    state.events.push(SinkInputEvent::PointerMotion {
                        stage_id: sid,
                        x: surface_x,
                        y: surface_y,
                        time: 0,
                    });
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(sid) = state.surface_to_stage.get(&surface.id().protocol_id()) {
                    if state.pointer_stage.as_ref() == Some(sid) {
                        state.pointer_stage = None;
                        state.pending_scroll = PendingScrollFrame::default();
                    }
                }
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                state.pointer_pos = (surface_x, surface_y);
                if let Some(sid) = state.pointer_stage.clone() {
                    state.events.push(SinkInputEvent::PointerMotion {
                        stage_id: sid,
                        x: surface_x,
                        y: surface_y,
                        time,
                    });
                }
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: WEnum::Value(btn_state),
            } => {
                state.last_pointer_serial = serial;
                state.last_input_serial = serial;
                if let Some(sid) = state.pointer_stage.clone() {
                    state.events.push(SinkInputEvent::PointerButton {
                        stage_id: sid,
                        button,
                        pressed: btn_state == wl_pointer::ButtonState::Pressed,
                        x: state.pointer_pos.0,
                        y: state.pointer_pos.1,
                        time,
                    });
                }
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(src),
            } => {
                state.pending_scroll.source = Some(match src {
                    wl_pointer::AxisSource::Wheel => AxisSource::Wheel,
                    wl_pointer::AxisSource::Finger => AxisSource::Finger,
                    wl_pointer::AxisSource::Continuous => AxisSource::Continuous,
                    wl_pointer::AxisSource::WheelTilt => AxisSource::WheelTilt,
                    _ => AxisSource::Wheel,
                });
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => {
                state.pending_scroll.has_event = true;
                let v120 = state.pending_scroll.v120.get_or_insert((0, 0));
                match axis {
                    wl_pointer::Axis::HorizontalScroll => v120.0 += discrete * 120,
                    wl_pointer::Axis::VerticalScroll => v120.1 += discrete * 120,
                    _ => {}
                }
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                state.pending_scroll.has_event = true;
                let v120 = state.pending_scroll.v120.get_or_insert((0, 0));
                match axis {
                    wl_pointer::Axis::HorizontalScroll => v120.0 += value120,
                    wl_pointer::Axis::VerticalScroll => v120.1 += value120,
                    _ => {}
                }
            }
            wl_pointer::Event::Axis {
                time,
                axis: WEnum::Value(axis),
                value,
            } => {
                state.pending_scroll.has_event = true;
                state.pending_scroll.time = time;
                match axis {
                    wl_pointer::Axis::VerticalScroll => state.pending_scroll.dy += value,
                    wl_pointer::Axis::HorizontalScroll => state.pending_scroll.dx += value,
                    _ => {}
                }
                if pointer.version() < 5 {
                    if let Some(sid) = state.pointer_stage.as_deref() {
                        state.pending_scroll.flush(sid, &mut state.events);
                    } else {
                        state.pending_scroll = PendingScrollFrame::default();
                    }
                }
            }
            wl_pointer::Event::AxisStop {
                time,
                axis: WEnum::Value(axis),
            } => {
                state.pending_scroll.has_event = true;
                state.pending_scroll.time = time;
                match axis {
                    wl_pointer::Axis::HorizontalScroll => state.pending_scroll.stop.0 = true,
                    wl_pointer::Axis::VerticalScroll => state.pending_scroll.stop.1 = true,
                    _ => {}
                }
            }
            wl_pointer::Event::Frame => {
                if let Some(sid) = state.pointer_stage.as_deref() {
                    state.pending_scroll.flush(sid, &mut state.events);
                } else {
                    state.pending_scroll = PendingScrollFrame::default();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { serial, surface, .. } => {
                state.last_input_serial = serial;
                if let Some(sid) = state.surface_to_stage.get(&surface.id().protocol_id()).cloned() {
                    state.keyboard_stage = Some(sid);
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if let Some(sid) = state.surface_to_stage.get(&surface.id().protocol_id()) {
                    if state.keyboard_stage.as_ref() == Some(sid) {
                        state.keyboard_stage = None;
                    }
                }
            }
            wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: WEnum::Value(kstate),
            } => {
                state.last_input_serial = serial;
                if let Some(sid) = state.keyboard_stage.clone() {
                    if matches!(kstate, wl_keyboard::KeyState::Pressed | wl_keyboard::KeyState::Released) {
                        state.events.push(SinkInputEvent::Key {
                            stage_id: sid,
                            evdev_keycode: key,
                            pressed: kstate == wl_keyboard::KeyState::Pressed,
                            time,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for WaylandClientState {
    fn event(_: &mut Self, _: &wl_data_device_manager::WlDataDeviceManager, _: wl_data_device_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state
                .pending_offers
                .entry(offer.id().protocol_id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { fd, .. } = event {
            let text = state.host_clipboard_text.clone();
            std::thread::spawn(move || {
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(text.as_bytes());
            });
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for WaylandClientState {
    wayland_client::event_created_child!(WaylandClientState, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.pending_offers.insert(id.id().protocol_id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(offer) = id {
                    let oid = offer.id().protocol_id();
                    let mimes = state.pending_offers.remove(&oid).unwrap_or_default();
                    let mime = mimes
                        .iter()
                        .find(|m| *m == "text/plain;charset=utf-8" || *m == "UTF8_STRING" || *m == "text/plain")
                        .cloned();
                    if let Some(m) = mime {
                        let mut fds = [0i32; 2];
                        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0 {
                            let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                            let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
                            offer.receive(m, write_fd.as_fd());
                            drop(write_fd);
                            offer.destroy();
                            let _ = conn.flush();
                            let tx = state.clipboard_tx.clone();
                            std::thread::spawn(move || {
                                if let Some(text) = read_fd_utf8_with_timeout(read_fd, 1500) {
                                    let _ = tx.send(text);
                                }
                            });
                        } else {
                            offer.destroy();
                            let _ = conn.flush();
                        }
                    } else {
                        offer.destroy();
                        let _ = conn.flush();
                    }
                }
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                if let Some(offer) = id {
                    if let Some(sid) = state.surface_to_stage.get(&surface.id().protocol_id()).cloned() {
                        state.dnd_stage = Some(sid);
                        state.dnd_pos = (x, y);
                        offer.accept(serial, Some("text/uri-list".to_string()));
                        offer.set_actions(
                            wl_data_device_manager::DndAction::Copy,
                            wl_data_device_manager::DndAction::Copy,
                        );
                        state.dnd_offer = Some(offer);
                    }
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                state.dnd_pos = (x, y);
            }
            wl_data_device::Event::Drop => {
                if let (Some(offer), Some(sid)) = (state.dnd_offer.take(), state.dnd_stage.take()) {
                    if let Some(text) = read_offer_fd_utf8(conn, &offer, "text/uri-list", 800) {
                        let paths: Vec<String> = text
                            .lines()
                            .map(|l| l.trim())
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .map(|l| l.strip_prefix("file://").unwrap_or(l).to_string())
                            .collect();
                        if !paths.is_empty() {
                            state.events.push(SinkInputEvent::FilesDropped {
                                stage_id: sid,
                                paths,
                                x: state.dnd_pos.0,
                                y: state.dnd_pos.1,
                            });
                        }
                    }
                    offer.finish();
                    offer.destroy();
                    let _ = conn.flush();
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.dnd_offer.take() {
                    offer.destroy();
                }
                state.dnd_stage = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_discrete_wheel_scroll_frame_and_resets() {
        let mut frame = PendingScrollFrame {
            has_event: true,
            dx: 0.0,
            dy: 10.0,
            source: Some(AxisSource::Wheel),
            v120: Some((0, 120)),
            stop: (false, false),
            time: 42,
        };
        let mut out = Vec::new();
        frame.flush("stage-1", &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            SinkInputEvent::PointerScroll {
                stage_id,
                dx,
                dy,
                source,
                v120,
                stop,
                time,
            } => {
                assert_eq!(stage_id, "stage-1");
                assert_eq!((*dx, *dy), (0.0, 10.0));
                assert_eq!(*source, Some(AxisSource::Wheel));
                assert_eq!(*v120, Some((0, 120)));
                assert_eq!(*stop, (false, false));
                assert_eq!(*time, 42);
            }
            other => panic!("unexpected event: {:?}", other),
        }
        // Subsequent empty frame must emit nothing
        frame.flush("stage-1", &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn derives_axis_value_when_only_v120_present() {
        let mut frame = PendingScrollFrame {
            has_event: true,
            dx: 0.0,
            dy: 0.0,
            source: Some(AxisSource::Wheel),
            v120: Some((0, -120)),
            stop: (false, false),
            time: 77,
        };
        let mut out = Vec::new();
        frame.flush("stage-2", &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            SinkInputEvent::PointerScroll { dy, v120, .. } => {
                assert_eq!(*dy, -15.0);
                assert_eq!(*v120, Some((0, -120)));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }
}

