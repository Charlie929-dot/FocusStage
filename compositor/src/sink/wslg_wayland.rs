use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smithay::backend::input::AxisSource;
use tracing::{debug, info, warn};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingConfigure {
    serial: u32,
    width: u32,
    height: u32,
    strict: bool,
}

#[derive(Default, Debug)]
struct XdgConfigureTracker {
    pending_toplevel_size: (u32, u32),
    pending_toplevel_strict: bool,
    unacked_configure: Option<PendingConfigure>,
    acked_strict_size: Option<(u32, u32)>,
}

impl XdgConfigureTracker {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_toplevel_configure(&mut self, width: i32, height: i32, strict: bool) {
        self.pending_toplevel_size = (width.max(0) as u32, height.max(0) as u32);
        self.pending_toplevel_strict = strict;
    }

    fn on_surface_configure(&mut self, serial: u32) {
        self.unacked_configure = Some(PendingConfigure {
            serial,
            width: self.pending_toplevel_size.0,
            height: self.pending_toplevel_size.1,
            strict: self.pending_toplevel_strict,
        });
    }

    fn prepare_commit(&mut self, width: u32, height: u32) -> Result<Option<u32>, ()> {
        if let Some(cfg) = self.unacked_configure {
            if cfg.strict
                && cfg.width > 0
                && cfg.height > 0
                && (width != cfg.width || height != cfg.height)
            {
                return Err(());
            }
            self.unacked_configure = None;
            self.acked_strict_size = if cfg.strict && cfg.width > 0 && cfg.height > 0 {
                Some((cfg.width, cfg.height))
            } else {
                None
            };
            return Ok(Some(cfg.serial));
        }
        if let Some((cw, ch)) = self.acked_strict_size {
            if width != cw || height != ch {
                return Err(());
            }
        }
        Ok(None)
    }
}

struct WaylandStageWindow {
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    configured: bool,
    mapped: bool,
    cfg_tracker: XdgConfigureTracker,
    rect: SavedRect,
    title: String,
    wm_class: String,
    slots: Vec<ShmSlot>,
    last_frame: Option<(u32, u32, Vec<u8>)>,
    pending_frame_cb: Option<(wl_callback::WlCallback, std::time::Instant)>,
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
    wake_worker_fd: Arc<OwnedFd>,
    pending_offers: HashMap<u32, Vec<String>>,
    dnd_offer: Option<wl_data_offer::WlDataOffer>,
    dnd_stage: Option<String>,
    dnd_pos: (f64, f64),
    just_configured: Vec<String>,
    frame_done_stages: Vec<String>,
    updated_rects: Vec<(String, SavedRect)>,

    events: Vec<SinkInputEvent>,
}

#[derive(Default)]
struct PendingScrollFrame {
    has_event: bool,
    dx: f64,
    dy: f64,
    source: Option<AxisSource>,
    v120: Option<(i32, i32)>,
    saw_discrete: (bool, bool),
    saw_v120: (bool, bool),
    stop: (bool, bool),
    time: u32,
}

impl PendingScrollFrame {
    fn record_axis_discrete(&mut self, axis: wl_pointer::Axis, discrete: i32) {
        self.has_event = true;
        if self.source.is_none() {
            self.source = Some(AxisSource::Wheel);
        }
        match axis {
            wl_pointer::Axis::HorizontalScroll => {
                self.saw_discrete.0 = true;
                if discrete != 0 {
                    let v120 = self.v120.get_or_insert((0, 0));
                    v120.0 += discrete * 120;
                }
            }
            wl_pointer::Axis::VerticalScroll => {
                self.saw_discrete.1 = true;
                if discrete != 0 {
                    let v120 = self.v120.get_or_insert((0, 0));
                    v120.1 += discrete * 120;
                }
            }
            _ => {}
        }
    }

    fn record_axis_value120(&mut self, axis: wl_pointer::Axis, value120: i32) {
        self.has_event = true;
        match axis {
            wl_pointer::Axis::HorizontalScroll => {
                self.saw_v120.0 = true;
                if value120 != 0 {
                    let v120 = self.v120.get_or_insert((0, 0));
                    v120.0 += value120;
                }
            }
            wl_pointer::Axis::VerticalScroll => {
                self.saw_v120.1 = true;
                if value120 != 0 {
                    let v120 = self.v120.get_or_insert((0, 0));
                    v120.1 += value120;
                }
            }
            _ => {}
        }
    }

    fn flush(&mut self, stage_id: &str, out: &mut Vec<SinkInputEvent>) {
        if !self.has_event {
            return;
        }
        let mut dx = self.dx;
        let mut dy = self.dy;
        let (mut vx, mut vy) = self.v120.unwrap_or((0, 0));

        // On wl_pointer v5-v7 (e.g. WSLg Weston), direction reversals and residual
        // accumulator wheel ticks often arrive as axis_discrete(0) + axis(±9.0/±10.0)
        // without axis_value120. Dropping discrete to 0 causes v7 clients (GTK3/Firefox,
        // XWayland) to skip the first reversed notch until a second notch accumulates.
        let is_wheel_source = matches!(self.source, Some(AxisSource::Wheel | AxisSource::WheelTilt));
        if !self.saw_v120.0 && vx == 0 && (self.saw_discrete.0 || is_wheel_source) && dx.abs() >= 5.0 {
            vx = if dx > 0.0 { 120 } else { -120 };
        }
        if !self.saw_v120.1 && vy == 0 && (self.saw_discrete.1 || is_wheel_source) && dy.abs() >= 5.0 {
            vy = if dy > 0.0 { 120 } else { -120 };
        }

        let v120 = if vx != 0 || vy != 0 {
            if dx == 0.0 && vx != 0 {
                dx = (vx as f64 / 120.0) * 15.0;
            }
            if dy == 0.0 && vy != 0 {
                dy = (vy as f64 / 120.0) * 15.0;
            }
            Some((vx, vy))
        } else {
            None
        };

        let emitted = dx != 0.0 || dy != 0.0 || v120.is_some() || self.stop.0 || self.stop.1;
        debug!(
            "[scroll-wslg] flush stage={} raw_d=({:.3},{:.3}) out_d=({:.3},{:.3}) v120={:?} source={:?} stop={:?} time={} emitted={}",
            stage_id, self.dx, self.dy, dx, dy, v120, self.source, self.stop, self.time, emitted
        );
        if emitted {
            out.push(SinkInputEvent::PointerScroll {
                stage_id: stage_id.to_string(),
                dx,
                dy,
                source: self.source,
                v120,
                stop: self.stop,
                time: self.time,
            });
        }
        *self = Self::default();
    }
}

impl WaylandClientState {
    fn new(clipboard_tx: mpsc::Sender<String>, wake_worker_fd: Arc<OwnedFd>) -> Self {
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
            wake_worker_fd,
            pending_offers: HashMap::new(),
            dnd_offer: None,
            dnd_stage: None,
            dnd_pos: (0.0, 0.0),
            just_configured: Vec::new(),
            frame_done_stages: Vec::new(),
            updated_rects: Vec::new(),
            events: Vec::new(),
        }
    }
}

enum SinkCommand {
    Show {
        stage_id: String,
        title: String,
        wm_class: String,
        rect: SavedRect,
    },
    Hide {
        stage_id: String,
    },
    Destroy {
        stage_id: String,
    },
    SetTitle {
        stage_id: String,
        title: String,
    },
    BeginMove {
        stage_id: String,
    },
    BeginResize {
        stage_id: String,
        direction: u32,
    },
    Minimize {
        stage_id: String,
    },
    SetMaximized {
        stage_id: String,
        maximized: bool,
    },
    SetHostClipboard {
        text: String,
    },
    Shutdown,
}

struct PendingPresentFrame {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

#[derive(Default)]
struct SharedSinkState {
    pending_frames: HashMap<String, PendingPresentFrame>,
    free_buffers: Vec<Vec<u8>>,
    frame_pending_since: HashMap<String, Instant>,
    frame_done_stages: Vec<String>,
    events: Vec<SinkInputEvent>,
    window_rects: HashMap<String, SavedRect>,
}

pub struct WslgWaylandSink {
    cmd_tx: mpsc::Sender<SinkCommand>,
    shared: Arc<Mutex<SharedSinkState>>,
    wake_worker_fd: Arc<OwnedFd>,
    notify_main_fd: Arc<OwnedFd>,
    worker: Option<JoinHandle<()>>,
}

struct WorkerCtx {
    conn: Connection,
    queue: EventQueue<WaylandClientState>,
    qh: QueueHandle<WaylandClientState>,
    state: WaylandClientState,
    clipboard_rx: mpsc::Receiver<String>,
}

fn create_eventfd() -> Result<OwnedFd, String> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("eventfd failed: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal_eventfd(fd: &OwnedFd) {
    let val: u64 = 1;
    unsafe {
        libc::write(
            fd.as_raw_fd(),
            &val as *const u64 as *const libc::c_void,
            std::mem::size_of::<u64>(),
        );
    }
}

fn drain_eventfd(fd: &OwnedFd) {
    let mut val: u64 = 0;
    loop {
        let rc = unsafe {
            libc::read(
                fd.as_raw_fd(),
                &mut val as *mut u64 as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        if rc <= 0 {
            break;
        }
    }
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
        let wake_worker_fd = Arc::new(create_eventfd()?);
        let notify_main_fd = Arc::new(create_eventfd()?);
        let shared = Arc::new(Mutex::new(SharedSinkState::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<SinkCommand>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();

        let worker_wake = Arc::clone(&wake_worker_fd);
        let worker_notify = Arc::clone(&notify_main_fd);
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("wslg-wayland-io".to_string())
            .spawn(move || {
                let mut worker = match WorkerCtx::connect(&sock_path, Arc::clone(&worker_wake)) {
                    Ok(w) => {
                        let _ = init_tx.send(Ok(()));
                        w
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                run_worker_loop(&mut worker, cmd_rx, worker_shared, worker_wake, worker_notify);
            })
            .map_err(|e| format!("spawn wslg-wayland-io thread: {e}"))?;

        match init_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(e) => {
                let _ = handle.join();
                return Err(format!("wslg-wayland-io init channel error: {e}"));
            }
        }

        Ok(Self {
            cmd_tx,
            shared,
            wake_worker_fd,
            notify_main_fd,
            worker: Some(handle),
        })
    }

    fn send_cmd(&self, cmd: SinkCommand) {
        if self.cmd_tx.send(cmd).is_ok() {
            signal_eventfd(&self.wake_worker_fd);
        }
    }
}

impl Drop for WslgWaylandSink {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(SinkCommand::Shutdown);
        signal_eventfd(&self.wake_worker_fd);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl WorkerCtx {
    fn connect(sock_path: &PathBuf, wake_worker_fd: Arc<OwnedFd>) -> Result<Self, String> {
        let stream = UnixStream::connect(sock_path)
            .map_err(|e| format!("connect WSLg Wayland {}: {e}", sock_path.display()))?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| format!("wayland Connection::from_socket: {e}"))?;

        let mut queue: EventQueue<WaylandClientState> = conn.new_event_queue();
        let qh = queue.handle();
        let (clipboard_tx, clipboard_rx) = mpsc::channel();
        let mut state = WaylandClientState::new(clipboard_tx, wake_worker_fd);

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
            "Connected WslgWaylandSink (async worker) to {} (csd_decoration_mgr={})",
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
            cfg_tracker: XdgConfigureTracker::default(),
            rect,
            title: clean_title.to_string(),
            wm_class: cls.to_string(),
            slots: Vec::new(),
            last_frame: None,
            pending_frame_cb: None,
        });

        win.surface = Some(surface.clone());
        win.xdg_surface = Some(xdg_surface);
        win.toplevel = Some(toplevel);
        win.decoration = decoration;
        win.configured = false;
        win.mapped = true;
        win.cfg_tracker.reset();
        win.last_frame = None;
        win.pending_frame_cb = None;
        win.rect = rect;
        win.title = clean_title.to_string();
        win.wm_class = cls.to_string();
        self.state.windows.insert(stage_id.to_string(), win);

        surface.commit();
        let _ = self.conn.flush();
    }

    fn unmap_window(&mut self, stage_id: &str) {
        if let Some(win) = self.state.windows.get_mut(stage_id) {
            win.mapped = false;
            win.configured = false;
            win.cfg_tracker.reset();
            win.last_frame = None;
            win.pending_frame_cb = None;
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

    fn present_frame(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]) -> bool {
        if width == 0 || height == 0 || bgra.len() < (width * height * 4) as usize {
            return false;
        }
        let Some(shm) = self.state.shm.clone() else {
            return false;
        };
        let Some(win) = self.state.windows.get_mut(stage_id) else {
            return false;
        };
        if !win.mapped || !win.configured {
            win.last_frame = Some((width, height, bgra.to_vec()));
            return false;
        }
        win.last_frame = None;
        let ack_serial = match win.cfg_tracker.prepare_commit(width, height) {
            Ok(serial) => serial,
            Err(()) => {
                info!(
                    "[wslg-wayland] skipping mismatched frame {}x{} for {} (pending={:?}, acked_strict={:?})",
                    width,
                    height,
                    stage_id,
                    win.cfg_tracker.unacked_configure,
                    win.cfg_tracker.acked_strict_size
                );
                return false;
            }
        };
        if let Some(serial) = ack_serial {
            if let Some(ref xdg_surf) = win.xdg_surface {
                xdg_surf.ack_configure(serial);
            }
        }
        let Some(ref surface) = win.surface else {
            return false;
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
                    return false;
                }
            },
        };

        win.slots[idx].write_bgra(bgra);
        win.slots[idx].busy = true;
        let cb = surface.frame(&self.qh, stage_id.to_string());
        win.pending_frame_cb = Some((cb, Instant::now()));
        surface.attach(Some(&win.slots[idx].buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        let _ = self.conn.flush();
        true
    }

    fn present_just_configured(&mut self, shared: &Arc<Mutex<SharedSinkState>>) {
        let ready_sids = std::mem::take(&mut self.state.just_configured);
        for sid in ready_sids {
            let last = self
                .state
                .windows
                .get_mut(&sid)
                .and_then(|w| w.last_frame.take());
            if let Some((w, h, buf)) = last {
                if self.present_frame(&sid, w, h, &buf) {
                    let mut guard = shared.lock().unwrap();
                    guard.frame_pending_since.insert(sid, Instant::now());
                }
            }
        }
    }

    fn drain_clipboard(&mut self) {
        while let Ok(text) = self.clipboard_rx.try_recv() {
            if !text.is_empty() && text != self.state.host_clipboard_text {
                self.state.host_clipboard_text = text.clone();
                self.state.events.push(SinkInputEvent::HostClipboardChanged { text });
            }
        }
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
}

fn publish_worker_outputs(
    state: &mut WaylandClientState,
    shared: &Arc<Mutex<SharedSinkState>>,
    notify_main_fd: &OwnedFd,
) {
    let has_events = !state.events.is_empty();
    let has_frame_done = !state.frame_done_stages.is_empty();
    let has_resizes = !state.updated_rects.is_empty();
    if !has_events && !has_frame_done && !has_resizes {
        return;
    }
    {
        let mut guard = shared.lock().unwrap();
        for (sid, rect) in state.updated_rects.drain(..) {
            guard.window_rects.insert(sid, rect);
        }
        for sid in &state.frame_done_stages {
            if !guard.pending_frames.contains_key(sid) {
                guard.frame_pending_since.remove(sid);
            }
        }
        if has_frame_done {
            guard.frame_done_stages.append(&mut state.frame_done_stages);
        }
        if has_events {
            guard.events.append(&mut state.events);
        }
    }
    if has_events || has_frame_done {
        signal_eventfd(notify_main_fd);
    }
}

fn run_worker_loop(
    worker: &mut WorkerCtx,
    cmd_rx: mpsc::Receiver<SinkCommand>,
    shared: Arc<Mutex<SharedSinkState>>,
    wake_worker_fd: Arc<OwnedFd>,
    notify_main_fd: Arc<OwnedFd>,
) {
    let mut running = true;
    while running {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                SinkCommand::Show {
                    stage_id,
                    title,
                    wm_class,
                    rect,
                } => {
                    worker.map_window(&stage_id, &title, &wm_class, rect);
                }
                SinkCommand::Hide { stage_id } => {
                    worker.unmap_window(&stage_id);
                }
                SinkCommand::Destroy { stage_id } => {
                    worker.unmap_window(&stage_id);
                    worker.state.windows.remove(&stage_id);
                }
                SinkCommand::SetTitle { stage_id, title } => {
                    worker.set_title(&stage_id, &title);
                }
                SinkCommand::BeginMove { stage_id } => {
                    worker.begin_move(&stage_id);
                }
                SinkCommand::BeginResize {
                    stage_id,
                    direction,
                } => {
                    worker.begin_resize(&stage_id, direction);
                }
                SinkCommand::Minimize { stage_id } => {
                    worker.minimize(&stage_id);
                }
                SinkCommand::SetMaximized {
                    stage_id,
                    maximized,
                } => {
                    worker.set_maximized(&stage_id, maximized);
                }
                SinkCommand::SetHostClipboard { text } => {
                    worker.set_host_clipboard(&text);
                }
                SinkCommand::Shutdown => {
                    running = false;
                    break;
                }
            }
        }
        if !running {
            break;
        }

        let pending_frames = {
            let mut guard = shared.lock().unwrap();
            std::mem::take(&mut guard.pending_frames)
        };
        for (sid, frame) in pending_frames {
            let committed = worker.present_frame(&sid, frame.width, frame.height, &frame.bgra);
            let mut guard = shared.lock().unwrap();
            if !committed && !guard.pending_frames.contains_key(&sid) {
                guard.frame_pending_since.remove(&sid);
            }
            if guard.free_buffers.len() < 4 {
                guard.free_buffers.push(frame.bgra);
            }
        }

        let _ = worker.queue.dispatch_pending(&mut worker.state);
        worker.present_just_configured(&shared);
        worker.drain_clipboard();
        publish_worker_outputs(&mut worker.state, &shared, &notify_main_fd);

        let _ = worker.conn.flush();

        for _ in 0..16 {
            let Some(guard) = worker.conn.prepare_read() else {
                let _ = worker.queue.dispatch_pending(&mut worker.state);
                worker.present_just_configured(&shared);
                worker.drain_clipboard();
                publish_worker_outputs(&mut worker.state, &shared, &notify_main_fd);
                continue;
            };
            let wl_fd = guard.connection_fd().as_raw_fd();
            let mut pfds = [
                libc::pollfd {
                    fd: wl_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_worker_fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                drop(guard);
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                warn!("wslg-wayland-io poll error: {}", err);
                running = false;
                break;
            }
            if (pfds[1].revents & libc::POLLIN) != 0 {
                drain_eventfd(&wake_worker_fd);
            }
            if (pfds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP)) != 0 {
                match guard.read() {
                    Ok(0) => {
                        if (pfds[0].revents & (libc::POLLERR | libc::POLLHUP)) != 0 {
                            warn!("WSLg Wayland socket closed (EOF)");
                            running = false;
                        }
                        break;
                    }
                    Ok(_) => {
                        let _ = worker.queue.dispatch_pending(&mut worker.state);
                        worker.present_just_configured(&shared);
                        worker.drain_clipboard();
                        publish_worker_outputs(&mut worker.state, &shared, &notify_main_fd);
                        break;
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        if !msg.contains("WouldBlock") {
                            warn!("wslg-wayland-io read error: {}", e);
                            running = false;
                        }
                        break;
                    }
                }
            } else {
                drop(guard);
                break;
            }
        }
    }
}

impl DisplaySink for WslgWaylandSink {
    fn show(&mut self, stage_id: &str, title: &str, wm_class: &str, rect: SavedRect) {
        {
            let mut guard = self.shared.lock().unwrap();
            guard.window_rects.insert(stage_id.to_string(), rect);
        }
        self.send_cmd(SinkCommand::Show {
            stage_id: stage_id.to_string(),
            title: title.to_string(),
            wm_class: wm_class.to_string(),
            rect,
        });
    }

    fn hide(&mut self, stage_id: &str) -> Option<SavedRect> {
        let rect = {
            let mut guard = self.shared.lock().unwrap();
            let rect = guard.window_rects.get(stage_id).copied();
            if let Some(old) = guard.pending_frames.remove(stage_id) {
                if guard.free_buffers.len() < 4 {
                    guard.free_buffers.push(old.bgra);
                }
            }
            guard.frame_pending_since.remove(stage_id);
            rect
        };
        if rect.is_some() {
            self.send_cmd(SinkCommand::Hide {
                stage_id: stage_id.to_string(),
            });
        }
        rect
    }

    fn destroy(&mut self, stage_id: &str) {
        {
            let mut guard = self.shared.lock().unwrap();
            guard.window_rects.remove(stage_id);
            if let Some(old) = guard.pending_frames.remove(stage_id) {
                if guard.free_buffers.len() < 4 {
                    guard.free_buffers.push(old.bgra);
                }
            }
            guard.frame_pending_since.remove(stage_id);
        }
        self.send_cmd(SinkCommand::Destroy {
            stage_id: stage_id.to_string(),
        });
    }

    fn take_frame_buffer(&mut self, min_capacity: usize) -> Vec<u8> {
        let mut guard = self.shared.lock().unwrap();
        if let Some(idx) = guard.free_buffers.iter().position(|b| b.capacity() >= min_capacity) {
            let mut buf = guard.free_buffers.swap_remove(idx);
            buf.clear();
            return buf;
        }
        if let Some(mut buf) = guard.free_buffers.pop() {
            buf.clear();
            if buf.capacity() < min_capacity {
                buf.reserve(min_capacity - buf.capacity());
            }
            return buf;
        }
        Vec::with_capacity(min_capacity)
    }

    fn present(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]) {
        let mut buf = self.take_frame_buffer(bgra.len());
        buf.extend_from_slice(bgra);
        self.present_owned(stage_id, width, height, buf);
    }

    fn present_owned(&mut self, stage_id: &str, width: u32, height: u32, bgra: Vec<u8>) {
        if width == 0 || height == 0 || bgra.len() < (width * height * 4) as usize {
            let mut guard = self.shared.lock().unwrap();
            if guard.free_buffers.len() < 4 {
                guard.free_buffers.push(bgra);
            }
            return;
        }
        {
            let mut guard = self.shared.lock().unwrap();
            if let Some(old) = guard.pending_frames.insert(
                stage_id.to_string(),
                PendingPresentFrame {
                    width,
                    height,
                    bgra,
                },
            ) {
                if guard.free_buffers.len() < 4 {
                    guard.free_buffers.push(old.bgra);
                }
            }
            guard
                .frame_pending_since
                .insert(stage_id.to_string(), Instant::now());
        }
        signal_eventfd(&self.wake_worker_fd);
    }

    fn set_title(&mut self, stage_id: &str, title: &str) {
        self.send_cmd(SinkCommand::SetTitle {
            stage_id: stage_id.to_string(),
            title: title.to_string(),
        });
    }

    fn begin_move(&mut self, stage_id: &str) {
        self.send_cmd(SinkCommand::BeginMove {
            stage_id: stage_id.to_string(),
        });
    }

    fn begin_resize(&mut self, stage_id: &str, direction: u32) {
        self.send_cmd(SinkCommand::BeginResize {
            stage_id: stage_id.to_string(),
            direction,
        });
    }

    fn minimize(&mut self, stage_id: &str) {
        self.send_cmd(SinkCommand::Minimize {
            stage_id: stage_id.to_string(),
        });
    }

    fn set_maximized(&mut self, stage_id: &str, maximized: bool) {
        self.send_cmd(SinkCommand::SetMaximized {
            stage_id: stage_id.to_string(),
            maximized,
        });
    }

    fn set_host_clipboard(&mut self, text: &str) {
        self.send_cmd(SinkCommand::SetHostClipboard {
            text: text.to_string(),
        });
    }

    fn event_fd(&self) -> Option<OwnedFd> {
        self.notify_main_fd.as_fd().try_clone_to_owned().ok()
    }

    fn flush(&mut self) {
        // Worker thread flushes its own Wayland socket immediately after processing commands/frames.
    }

    fn is_frame_pending(&self, stage_id: &str) -> bool {
        const FRAME_TIMEOUT: Duration = Duration::from_millis(50);
        let guard = self.shared.lock().unwrap();
        guard
            .frame_pending_since
            .get(stage_id)
            .is_some_and(|since| since.elapsed() < FRAME_TIMEOUT)
    }

    fn take_frame_done_stages(&mut self) -> Vec<String> {
        let mut guard = self.shared.lock().unwrap();
        std::mem::take(&mut guard.frame_done_stages)
    }

    fn poll_events(&mut self) -> Vec<SinkInputEvent> {
        drain_eventfd(&self.notify_main_fd);
        let mut guard = self.shared.lock().unwrap();
        std::mem::take(&mut guard.events)
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

impl Dispatch<wl_callback::WlCallback, String> for WaylandClientState {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        stage_id: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if let Some(win) = state.windows.get_mut(stage_id) {
                if win
                    .pending_frame_cb
                    .as_ref()
                    .is_some_and(|(cb, _)| cb.id() == callback.id())
                {
                    win.pending_frame_cb = None;
                    state.frame_done_stages.push(stage_id.clone());
                }
            }
        }
    }
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
            if let Some(sid) = state.xdg_to_stage.get(&xdg_surf.id().protocol_id()).cloned() {
                if let Some(win) = state.windows.get_mut(&sid) {
                    let first = !win.configured;
                    win.configured = true;
                    win.cfg_tracker.on_surface_configure(serial);
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
            xdg_toplevel::Event::Configure { width, height, states } => {
                let strict = states.chunks_exact(4).any(|chunk| {
                    let val = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    val == xdg_toplevel::State::Maximized as u32
                        || val == xdg_toplevel::State::Fullscreen as u32
                });
                if let Some(win) = state.windows.get_mut(&sid) {
                    win.cfg_tracker.on_toplevel_configure(width, height, strict);
                    if width > 0 && height > 0 {
                        let w = width as u32;
                        let h = height as u32;
                        if win.rect.width != w || win.rect.height != h {
                            win.rect.width = w;
                            win.rect.height = h;
                            state.updated_rects.push((sid.clone(), win.rect));
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
                debug!(
                    "[scroll-wslg] axis_source={:?} stage={:?} ptr_v={}",
                    src, state.pointer_stage, pointer.version()
                );
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
                debug!(
                    "[scroll-wslg] axis_discrete axis={:?} discrete={} stage={:?} ptr_v={}",
                    axis, discrete, state.pointer_stage, pointer.version()
                );
                state.pending_scroll.record_axis_discrete(axis, discrete);
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                debug!(
                    "[scroll-wslg] axis_value120 axis={:?} value120={} stage={:?} ptr_v={}",
                    axis, value120, state.pointer_stage, pointer.version()
                );
                state.pending_scroll.record_axis_value120(axis, value120);
            }
            wl_pointer::Event::Axis {
                time,
                axis: WEnum::Value(axis),
                value,
            } => {
                debug!(
                    "[scroll-wslg] axis axis={:?} value={:.4} time={} stage={:?} ptr_v={}",
                    axis, value, time, state.pointer_stage, pointer.version()
                );
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
                debug!(
                    "[scroll-wslg] axis_stop axis={:?} time={} stage={:?}",
                    axis, time, state.pointer_stage
                );
                state.pending_scroll.has_event = true;
                state.pending_scroll.time = time;
                match axis {
                    wl_pointer::Axis::HorizontalScroll => state.pending_scroll.stop.0 = true,
                    wl_pointer::Axis::VerticalScroll => state.pending_scroll.stop.1 = true,
                    _ => {}
                }
            }
            wl_pointer::Event::Frame => {
                if state.pending_scroll.has_event {
                    debug!(
                        "[scroll-wslg] pointer_frame stage={:?}",
                        state.pointer_stage
                    );
                }
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
                            let wake = Arc::clone(&state.wake_worker_fd);
                            std::thread::spawn(move || {
                                if let Some(text) = read_fd_utf8_with_timeout(read_fd, 1500) {
                                    let _ = tx.send(text);
                                    signal_eventfd(&wake);
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
            saw_discrete: (false, true),
            saw_v120: (false, false),
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
            saw_discrete: (false, false),
            saw_v120: (false, true),
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

    #[test]
    fn promotes_wslg_zero_discrete_wheel_reversal_to_full_v120() {
        let mut frame = PendingScrollFrame::default();
        frame.record_axis_discrete(wl_pointer::Axis::VerticalScroll, 0);
        frame.dy = -9.0;
        frame.time = 100;

        let mut out = Vec::new();
        frame.flush("stage-1", &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            SinkInputEvent::PointerScroll {
                dy,
                source,
                v120,
                ..
            } => {
                assert_eq!(*dy, -9.0);
                assert_eq!(*source, Some(AxisSource::Wheel));
                assert_eq!(*v120, Some((0, -120)));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn keeps_subnotch_finger_and_value120_scrolls_without_inflating() {
        // Touchpad / Finger smooth scroll must not synthesize v120
        let mut finger = PendingScrollFrame {
            has_event: true,
            dy: 9.0,
            source: Some(AxisSource::Finger),
            time: 10,
            ..Default::default()
        };
        let mut out = Vec::new();
        finger.flush("stage-1", &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            SinkInputEvent::PointerScroll {
                dy,
                source,
                v120,
                ..
            } => {
                assert_eq!(*dy, 9.0);
                assert_eq!(*source, Some(AxisSource::Finger));
                assert_eq!(*v120, None);
            }
            other => panic!("unexpected event: {:?}", other),
        }

        // True high-res v8 axis_value120 sub-step must preserve exact value120
        let mut hires = PendingScrollFrame::default();
        hires.record_axis_value120(wl_pointer::Axis::VerticalScroll, 30);
        hires.dy = 2.5;
        hires.source = Some(AxisSource::Wheel);
        hires.flush("stage-1", &mut out);
        assert_eq!(out.len(), 2);
        match &out[1] {
            SinkInputEvent::PointerScroll { v120, .. } => {
                assert_eq!(*v120, Some((0, 30)));
            }
            other => panic!("unexpected event: {:?}", other),
        }

        // Tiny sub-notch axis_discrete(0) micro-step must not inflate to 120
        let mut micro = PendingScrollFrame::default();
        micro.record_axis_discrete(wl_pointer::Axis::VerticalScroll, 0);
        micro.dy = 1.5;
        micro.flush("stage-1", &mut out);
        assert_eq!(out.len(), 3);
        match &out[2] {
            SinkInputEvent::PointerScroll { dy, v120, .. } => {
                assert_eq!(*dy, 1.5);
                assert_eq!(*v120, None);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn xdg_configure_tracker_blocks_mismatched_maximized_commits_until_resized() {
        let mut tracker = XdgConfigureTracker::default();

        // Weston sends maximized configure (1920x1020)
        tracker.on_toplevel_configure(1920, 1020, true);
        tracker.on_surface_configure(10);

        // Stale 1280x800 frame must NOT ack serial 10 or commit
        assert_eq!(tracker.prepare_commit(1280, 800), Err(()));
        assert!(tracker.unacked_configure.is_some());

        // Resized 1920x1020 frame acks serial 10 and locks strict size
        assert_eq!(tracker.prepare_commit(1920, 1020), Ok(Some(10)));
        assert_eq!(tracker.unacked_configure, None);
        assert_eq!(tracker.acked_strict_size, Some((1920, 1020)));

        // Subsequent matching frames commit without re-acking; mismatched frames stay blocked
        assert_eq!(tracker.prepare_commit(1920, 1020), Ok(None));
        assert_eq!(tracker.prepare_commit(1280, 800), Err(()));

        // Unmaximizing clears strict enforcement once the new configure is acked
        tracker.on_toplevel_configure(1280, 800, false);
        tracker.on_surface_configure(11);
        assert_eq!(tracker.prepare_commit(1280, 800), Ok(Some(11)));
        assert_eq!(tracker.acked_strict_size, None);
        assert_eq!(tracker.prepare_commit(1280, 800), Ok(None));
    }
}

