use std::collections::HashMap;

use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, ImageFormat, PropMode, SelectionNotifyEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

use crate::protocol::SavedRect;
use crate::sink::{DisplaySink, SinkInputEvent};

x11rb::atom_manager! {
    pub WslgAtoms: WslgAtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_NORMAL_HINTS,
        WM_CHANGE_STATE,
        _MOTIF_WM_HINTS,
        _NET_WM_MOVERESIZE,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        UTF8_STRING,
        CLIPBOARD,
        TARGETS,
        TEXT,
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        TEXT_URI_LIST: b"text/uri-list",
        _NET_WM_NAME,
        XdndAware,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndActionCopy,
        FOCUSSTAGE_SEL_PROP,
    }
}

struct ProxyWindow {
    window: Window,
    gc: u32,
    mapped: bool,
    rect: SavedRect,
    last_frame: Option<(u32, u32, Vec<u8>)>,
}

pub struct WslgSink {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    depth: u8,
    visual: u32,
    atoms: WslgAtoms,
    windows: HashMap<String, ProxyWindow>,
    win_to_stage: HashMap<Window, String>,
    clipboard_owner_win: Window,
    host_clipboard_text: String,
    xdnd_source_win: Option<Window>,
    xdnd_target_stage: Option<String>,
    xdnd_drop_pos: (f64, f64),
}

impl WslgSink {
    pub fn connect(display_name: Option<&str>) -> Result<Self, String> {
        let (conn, screen_num) = RustConnection::connect(display_name)
            .map_err(|e| format!("connect WSLg X11 ({:?}): {e}", display_name))?;
        let atoms = WslgAtoms::new(&conn)
            .map_err(|e| format!("intern atoms: {e}"))?
            .reply()
            .map_err(|e| format!("intern atoms reply: {e}"))?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let visual = screen.root_visual;

        // Hidden helper window for clipboard ownership and selection retrieval.
        let clipboard_owner_win = conn
            .generate_id()
            .map_err(|e| format!("generate clipboard window id: {e}"))?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            clipboard_owner_win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| format!("create clipboard window: {e}"))?;

        // Enable XFixes clipboard monitoring if available
        if let Ok(ver) = conn.xfixes_query_version(5, 0) {
            if ver.reply().is_ok() {
                let _ = conn.xfixes_select_selection_input(
                    clipboard_owner_win,
                    atoms.CLIPBOARD,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                );
            }
        }
        let _ = conn.flush();

        info!("Connected WslgSink to X11 display {:?}", display_name);
        Ok(Self {
            conn,
            screen_num,
            root,
            depth,
            visual,
            atoms,
            windows: HashMap::new(),
            win_to_stage: HashMap::new(),
            clipboard_owner_win,
            host_clipboard_text: String::new(),
            xdnd_source_win: None,
            xdnd_target_stage: None,
            xdnd_drop_pos: (0.0, 0.0),
        })
    }

    pub fn set_host_clipboard(&mut self, text: &str) {
        if self.host_clipboard_text == text {
            return;
        }
        self.host_clipboard_text = text.to_string();
        let _ = self.conn.set_selection_owner(
            self.clipboard_owner_win,
            self.atoms.CLIPBOARD,
            x11rb::CURRENT_TIME,
        );
        let _ = self.conn.flush();
    }

    fn send_net_wm_moveresize(&self, stage_id: &str, direction: u32) {
        let Some(pw) = self.windows.get(stage_id) else {
            return;
        };
        let (root_x, root_y) = match self.conn.query_pointer(self.root).ok().and_then(|c| c.reply().ok()) {
            Some(r) => (r.root_x as i32 as u32, r.root_y as i32 as u32),
            None => (
                (pw.rect.x + (pw.rect.width as i32 / 2)) as u32,
                (pw.rect.y + 12) as u32,
            ),
        };
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let msg = ClientMessageEvent::new(
            32,
            pw.window,
            self.atoms._NET_WM_MOVERESIZE,
            [root_x, root_y, direction, 1, 1],
        );
        let _ = self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            msg,
        );
        let _ = self.conn.flush();
    }

    fn ensure_proxy_window(
        &mut self,
        stage_id: &str,
        title: &str,
        wm_class: &str,
        rect: SavedRect,
    ) -> Result<Window, String> {
        if let Some(existing) = self.windows.get(stage_id) {
            return Ok(existing.window);
        }

        let win = self.conn.generate_id().map_err(|e| format!("gen id: {e}"))?;
        let gc = self.conn.generate_id().map_err(|e| format!("gen gc: {e}"))?;
        let mask = EventMask::EXPOSURE
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::FOCUS_CHANGE;

        self.conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                win,
                self.root,
                rect.x as i16,
                rect.y as i16,
                rect.width.max(1) as u16,
                rect.height.max(1) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new()
                    .background_pixel(0x16181d)
                    .event_mask(mask),
            )
            .map_err(|e| format!("create_window: {e}"))?;

        self.conn
            .create_gc(gc, win, &CreateGCAux::new())
            .map_err(|e| format!("create_gc: {e}"))?;

        // Set WM_PROTOCOLS = [WM_DELETE_WINDOW]
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &[self.atoms.WM_DELETE_WINDOW],
        );

        // Set WM_CLASS = "<instance>\0<class>\0" so WSLg RAIL binds per-app icon, grouping, and pinning
        let cls = if wm_class.trim().is_empty() {
            "FocusStage"
        } else {
            wm_class.trim()
        };
        let wm_class_prop = format!("{cls}\0{cls}\0");
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            wm_class_prop.as_bytes(),
        );

        // Advertise XdndAware version 5
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.XdndAware,
            AtomEnum::ATOM,
            &[5],
        );

        // Request borderless/undecorated window from WSLg Weston XWM so only the app's own content renders
        let motif_hints: [u32; 5] = [2, 0, 0, 0, 0];
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms._MOTIF_WM_HINTS,
            self.atoms._MOTIF_WM_HINTS,
            &motif_hints,
        );

        self.apply_title_and_hints(win, title, rect);
        let _ = self.conn.flush();

        self.win_to_stage.insert(win, stage_id.to_string());
        self.windows.insert(
            stage_id.to_string(),
            ProxyWindow {
                window: win,
                gc,
                mapped: false,
                rect,
                last_frame: None,
            },
        );
        Ok(win)
    }

    fn apply_title_and_hints(&self, win: Window, title: &str, rect: SavedRect) {
        let full_title = if title.trim().is_empty() {
            "FocusStage"
        } else {
            title.trim()
        };
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            full_title.as_bytes(),
        );
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            win,
            self.atoms._NET_WM_NAME,
            self.atoms.UTF8_STRING,
            full_title.as_bytes(),
        );

        // XSizeHints: flags = USPosition (1) | USSize (2) | PPosition (4) | PSize (8) = 15
        let mut hints = [0u32; 18];
        hints[0] = 1 | 2 | 4 | 8;
        hints[1] = rect.x as u32;
        hints[2] = rect.y as u32;
        hints[3] = rect.width.max(1);
        hints[4] = rect.height.max(1);
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        );
    }

    fn draw_bgra_to_window(
        &self,
        win: Window,
        gc: u32,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) {
        if width == 0 || height == 0 || bgra.len() < (width * height * 4) as usize {
            return;
        }
        let max_req_bytes = ((self.conn.setup().maximum_request_length as usize) * 4)
            .saturating_sub(256)
            .max(4096);
        let row_bytes = (width * 4) as usize;
        let rows_per_chunk = (max_req_bytes / row_bytes).max(1) as u32;

        let mut y = 0u32;
        while y < height {
            let chunk_h = rows_per_chunk.min(height - y);
            let start = (y as usize) * row_bytes;
            let end = start + (chunk_h as usize) * row_bytes;
            let _ = self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                win,
                gc,
                width as u16,
                chunk_h as u16,
                0,
                y as i16,
                0,
                self.depth,
                &bgra[start..end],
            );
            y += chunk_h;
        }
        let _ = self.conn.flush();
    }

    fn handle_selection_request(&self, req: &xproto::SelectionRequestEvent) {
        let mut prop = if req.property == x11rb::NONE {
            req.target
        } else {
            req.property
        };

        let ok = if req.target == self.atoms.TARGETS {
            let targets = [
                self.atoms.TARGETS,
                self.atoms.UTF8_STRING,
                self.atoms.TEXT_PLAIN_UTF8,
                AtomEnum::STRING.into(),
            ];
            self.conn
                .change_property32(PropMode::REPLACE, req.requestor, prop, AtomEnum::ATOM, &targets)
                .is_ok()
        } else if req.target == self.atoms.UTF8_STRING
            || req.target == self.atoms.TEXT_PLAIN_UTF8
            || req.target == Atom::from(AtomEnum::STRING)
            || req.target == self.atoms.TEXT
        {
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    req.target,
                    self.host_clipboard_text.as_bytes(),
                )
                .is_ok()
        } else {
            prop = x11rb::NONE;
            false
        };

        let _ = ok;
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: prop,
        };
        let _ = self.conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
    }
}

impl DisplaySink for WslgSink {
    fn show(&mut self, stage_id: &str, title: &str, wm_class: &str, rect: SavedRect) {
        let win = match self.ensure_proxy_window(stage_id, title, wm_class, rect) {
            Ok(w) => w,
            Err(e) => {
                warn!("WslgSink show failed for {}: {}", stage_id, e);
                return;
            }
        };
        self.apply_title_and_hints(win, title, rect);
        let _ = self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(rect.x)
                .y(rect.y)
                .width(rect.width.max(1))
                .height(rect.height.max(1)),
        );
        let _ = self.conn.map_window(win);
        let _ = self.conn.configure_window(
            win,
            &ConfigureWindowAux::new().x(rect.x).y(rect.y),
        );
        let _ = self.conn.flush();

        if let Some(pw) = self.windows.get_mut(stage_id) {
            pw.mapped = true;
            pw.rect = rect;
            if let Some((w, h, ref buf)) = pw.last_frame {
                let buf_clone = buf.clone();
                let gc = pw.gc;
                self.draw_bgra_to_window(win, gc, w, h, &buf_clone);
            }
        }
    }

    fn hide(&mut self, stage_id: &str) -> Option<SavedRect> {
        let pw = self.windows.get_mut(stage_id)?;
        let win = pw.window;
        if let (Ok(geo_c), Ok(trans_c)) = (
            self.conn.get_geometry(win),
            self.conn.translate_coordinates(win, self.root, 0, 0),
        ) {
            if let (Ok(geo), Ok(trans)) = (geo_c.reply(), trans_c.reply()) {
                pw.rect = SavedRect {
                    x: trans.dst_x as i32,
                    y: trans.dst_y as i32,
                    width: geo.width as u32,
                    height: geo.height as u32,
                };
            }
        }
        pw.mapped = false;
        let saved = pw.rect;
        let _ = self.conn.unmap_window(win);
        let _ = self.conn.flush();
        Some(saved)
    }

    fn destroy(&mut self, stage_id: &str) {
        if let Some(pw) = self.windows.remove(stage_id) {
            self.win_to_stage.remove(&pw.window);
            let _ = self.conn.free_gc(pw.gc);
            let _ = self.conn.destroy_window(pw.window);
            let _ = self.conn.flush();
        }
    }

    fn present(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]) {
        let Some(pw) = self.windows.get_mut(stage_id) else {
            return;
        };
        let win = pw.window;
        let gc = pw.gc;
        let mapped = pw.mapped;
        pw.last_frame = Some((width, height, bgra.to_vec()));
        if mapped {
            self.draw_bgra_to_window(win, gc, width, height, bgra);
        }
    }

    fn set_title(&mut self, stage_id: &str, title: &str) {
        if let Some(pw) = self.windows.get(stage_id) {
            self.apply_title_and_hints(pw.window, title, pw.rect);
            let _ = self.conn.flush();
        }
    }

    fn begin_move(&mut self, stage_id: &str) {
        self.send_net_wm_moveresize(stage_id, 8);
    }

    fn begin_resize(&mut self, stage_id: &str, direction: u32) {
        self.send_net_wm_moveresize(stage_id, direction);
    }

    fn minimize(&mut self, stage_id: &str) {
        let Some(pw) = self.windows.get(stage_id) else {
            return;
        };
        let msg = ClientMessageEvent::new(
            32,
            pw.window,
            self.atoms.WM_CHANGE_STATE,
            [3 /* IconicState */, 0, 0, 0, 0],
        );
        let _ = self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            msg,
        );
        let _ = self.conn.flush();
    }

    fn set_maximized(&mut self, stage_id: &str, maximized: bool) {
        let Some(pw) = self.windows.get(stage_id) else {
            return;
        };
        let action = if maximized { 1u32 } else { 0u32 };
        let msg = ClientMessageEvent::new(
            32,
            pw.window,
            self.atoms._NET_WM_STATE,
            [
                action,
                self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                1,
                0,
            ],
        );
        let _ = self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            msg,
        );
        let _ = self.conn.flush();
    }

    fn set_host_clipboard(&mut self, text: &str) {
        WslgSink::set_host_clipboard(self, text);
    }

    fn poll_events(&mut self) -> Vec<SinkInputEvent> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                Event::Expose(e) => {
                    if e.count == 0 {
                        if let Some(stage_id) = self.win_to_stage.get(&e.window).cloned() {
                            if let Some(pw) = self.windows.get(&stage_id) {
                                if pw.mapped {
                                    if let Some((w, h, ref buf)) = pw.last_frame {
                                        self.draw_bgra_to_window(pw.window, pw.gc, w, h, buf);
                                    }
                                }
                            }
                        }
                    }
                }
                Event::MotionNotify(e) => {
                    if let Some(stage_id) = self.win_to_stage.get(&e.event).cloned() {
                        out.push(SinkInputEvent::PointerMotion {
                            stage_id,
                            x: e.event_x as f64,
                            y: e.event_y as f64,
                            time: e.time,
                        });
                    }
                }
                Event::ButtonPress(_) | Event::ButtonRelease(_) => {
                    let pressed = matches!(ev, Event::ButtonPress(_));
                    let (win, detail, x, y, time) = match ev {
                        Event::ButtonPress(b) | Event::ButtonRelease(b) => {
                            (b.event, b.detail, b.event_x as f64, b.event_y as f64, b.time)
                        }
                        _ => unreachable!(),
                    };
                    if let Some(stage_id) = self.win_to_stage.get(&win).cloned() {
                        match detail {
                            1 => out.push(SinkInputEvent::PointerButton {
                                stage_id,
                                button: 0x110, // BTN_LEFT
                                pressed,
                                x,
                                y,
                                time,
                            }),
                            2 => out.push(SinkInputEvent::PointerButton {
                                stage_id,
                                button: 0x112, // BTN_MIDDLE
                                pressed,
                                x,
                                y,
                                time,
                            }),
                            3 => out.push(SinkInputEvent::PointerButton {
                                stage_id,
                                button: 0x111, // BTN_RIGHT
                                pressed,
                                x,
                                y,
                                time,
                            }),
                            4 if pressed => out.push(SinkInputEvent::PointerScroll {
                                stage_id,
                                dx: 0.0,
                                dy: -15.0,
                                time,
                            }),
                            5 if pressed => out.push(SinkInputEvent::PointerScroll {
                                stage_id,
                                dx: 0.0,
                                dy: 15.0,
                                time,
                            }),
                            6 if pressed => out.push(SinkInputEvent::PointerScroll {
                                stage_id,
                                dx: -15.0,
                                dy: 0.0,
                                time,
                            }),
                            7 if pressed => out.push(SinkInputEvent::PointerScroll {
                                stage_id,
                                dx: 15.0,
                                dy: 0.0,
                                time,
                            }),
                            _ => {}
                        }
                    }
                }
                Event::KeyPress(_) | Event::KeyRelease(_) => {
                    let pressed = matches!(ev, Event::KeyPress(_));
                    let (win, detail, time) = match ev {
                        Event::KeyPress(k) | Event::KeyRelease(k) => (k.event, k.detail, k.time),
                        _ => unreachable!(),
                    };
                    if let Some(stage_id) = self.win_to_stage.get(&win).cloned() {
                        if detail >= 8 {
                            out.push(SinkInputEvent::Key {
                                stage_id,
                                evdev_keycode: (detail as u32) - 8,
                                pressed,
                                time,
                            });
                        }
                    }
                }
                Event::ConfigureNotify(e) => {
                    if let Some(stage_id) = self.win_to_stage.get(&e.window).cloned() {
                        let rect = SavedRect {
                            x: e.x as i32,
                            y: e.y as i32,
                            width: (e.width as u32).max(1),
                            height: (e.height as u32).max(1),
                        };
                        if let Some(pw) = self.windows.get_mut(&stage_id) {
                            let size_changed =
                                pw.rect.width != rect.width || pw.rect.height != rect.height;
                            pw.rect = rect;
                            if size_changed {
                                out.push(SinkInputEvent::Resized { stage_id, rect });
                            }
                        }
                    }
                }
                Event::ClientMessage(e) => {
                    if e.type_ == self.atoms.WM_PROTOCOLS {
                        let data = e.data.as_data32();
                        if data[0] == self.atoms.WM_DELETE_WINDOW {
                            if let Some(stage_id) = self.win_to_stage.get(&e.window).cloned() {
                                out.push(SinkInputEvent::CloseRequested { stage_id });
                            }
                        }
                    } else if e.type_ == self.atoms.XdndEnter {
                        let data = e.data.as_data32();
                        self.xdnd_source_win = Some(data[0]);
                        self.xdnd_target_stage = self.win_to_stage.get(&e.window).cloned();
                    } else if e.type_ == self.atoms.XdndPosition {
                        let data = e.data.as_data32();
                        let src = data[0];
                        let packed = data[2];
                        let root_x = ((packed >> 16) & 0xffff) as i16 as f64;
                        let root_y = (packed & 0xffff) as i16 as f64;
                        if let Some(stage_id) = self.win_to_stage.get(&e.window) {
                            if let Some(pw) = self.windows.get(stage_id) {
                                self.xdnd_drop_pos = (
                                    (root_x - pw.rect.x as f64).max(0.0),
                                    (root_y - pw.rect.y as f64).max(0.0),
                                );
                            }
                        }
                        let status = ClientMessageEvent::new(
                            32,
                            src,
                            self.atoms.XdndStatus,
                            [e.window, 1, 0, 0, self.atoms.XdndActionCopy],
                        );
                        let _ = self.conn.send_event(false, src, EventMask::NO_EVENT, status);
                        let _ = self.conn.flush();
                    } else if e.type_ == self.atoms.XdndDrop {
                        let data = e.data.as_data32();
                        let src = data[0];
                        let time = data[2];
                        let _ = self.conn.convert_selection(
                            e.window,
                            self.atoms.XdndSelection,
                            self.atoms.TEXT_URI_LIST,
                            self.atoms.FOCUSSTAGE_SEL_PROP,
                            time,
                        );
                        let _ = self.conn.flush();
                        let _ = src;
                    }
                }
                Event::SelectionRequest(req) => {
                    self.handle_selection_request(&req);
                }
                Event::XfixesSelectionNotify(e) => {
                    if e.selection == self.atoms.CLIPBOARD && e.owner != self.clipboard_owner_win && e.owner != x11rb::NONE {
                        let _ = self.conn.convert_selection(
                            self.clipboard_owner_win,
                            self.atoms.CLIPBOARD,
                            self.atoms.UTF8_STRING,
                            self.atoms.FOCUSSTAGE_SEL_PROP,
                            e.timestamp,
                        );
                        let _ = self.conn.flush();
                    }
                }
                Event::SelectionNotify(e) => {
                    if e.property != x11rb::NONE {
                        if let Ok(cookie) = self.conn.get_property(
                            true,
                            e.requestor,
                            e.property,
                            AtomEnum::ANY,
                            0,
                            1024 * 256,
                        ) {
                            if let Ok(reply) = cookie.reply() {
                                if let Ok(text) = String::from_utf8(reply.value) {
                                    if e.selection == self.atoms.CLIPBOARD {
                                        if text != self.host_clipboard_text {
                                            self.host_clipboard_text = text.clone();
                                            out.push(SinkInputEvent::HostClipboardChanged { text });
                                        }
                                    } else if e.selection == self.atoms.XdndSelection {
                                        if let Some(stage_id) = self.xdnd_target_stage.take() {
                                            let paths: Vec<String> = text
                                                .lines()
                                                .map(|l| l.trim())
                                                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                                                .map(|l| l.strip_prefix("file://").unwrap_or(l).to_string())
                                                .collect();
                                            if !paths.is_empty() {
                                                out.push(SinkInputEvent::FilesDropped {
                                                    stage_id,
                                                    paths,
                                                    x: self.xdnd_drop_pos.0,
                                                    y: self.xdnd_drop_pos.1,
                                                });
                                            }
                                        }
                                        if let Some(src) = self.xdnd_source_win.take() {
                                            let fin = ClientMessageEvent::new(
                                                32,
                                                src,
                                                self.atoms.XdndFinished,
                                                [e.requestor, 1, self.atoms.XdndActionCopy, 0, 0],
                                            );
                                            let _ = self.conn.send_event(false, src, EventMask::NO_EVENT, fin);
                                            let _ = self.conn.flush();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    let _ = self.screen_num;
                }
            }
        }
        out
    }
}
