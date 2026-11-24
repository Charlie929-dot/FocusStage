pub mod native;

use crate::protocol::SavedRect;

#[derive(Debug, Clone)]
pub enum SinkInputEvent {
    Disconnected,
    PointerLeave { window_id: String },
    PointerMotion {
        window_id: String,
        x: f64,
        y: f64,
    },
    PointerButton {
        window_id: String,
        button: u32,
        pressed: bool,
        x: f64,
        y: f64,
    },
    PointerScroll {
        window_id: String,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        v120: (i32, i32),
    },
    Key {
        window_id: String,
        evdev_keycode: u32,
        pressed: bool,
    },
    WindowChanged {
        window_id: String,
        state: WindowState,
    },
    CloseRequested {
        window_id: String,
    },
    HostClipboardChanged {
        text: Option<String>,
        image_png: Option<Vec<u8>>,
    },
    FilesDropped {
        window_id: String,
        paths: Vec<String>,
        x: f64,
        y: f64,
    },
    CancelTakeover {
        window_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentOverlay<'a> {
    pub window_id: &'a str,
    pub active: bool,
    pub agent_name: Option<&'a str>,
    pub pointer_x: f64,
    pub pointer_y: f64,
    pub pointer_visible: bool,
    pub pointer_state: &'a str,
}

pub struct WindowInfo<'a> {
    pub window_id: &'a str,
    pub stage_id: &'a str,
    pub title: &'a str,
    pub wm_class: &'a str,
    pub icon_png: Option<&'a str>,
    pub rect: SavedRect,
    pub scale: f64,
    pub parent_window_id: Option<&'a str>,
    pub maximized: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct WindowState {
    pub rect: SavedRect,
    pub scale: f64,
    pub monitor: Option<SavedRect>,
    pub pixel_size: Option<[u32; 2]>,
    pub activated: bool,
    pub maximized: bool,
    pub resizing: bool,
    pub tiled: [bool; 4],
}

pub trait DisplaySink {
    fn show(&mut self, window: WindowInfo<'_>);
    fn hide(&mut self, window_id: &str);
    fn destroy(&mut self, window_id: &str);
    fn take_frame_buffer(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn present_owned(&mut self, window_id: &str, width: u32, height: u32, bgra: Vec<u8>);
    fn set_title(&mut self, window_id: &str, title: &str);
    fn set_parent(&mut self, _window_id: &str, _parent_window_id: Option<&str>) {}
    fn begin_move(&mut self, _window_id: &str) {}
    fn begin_resize(&mut self, _window_id: &str, _direction: u32) {}
    fn minimize(&mut self, _window_id: &str) {}
    fn set_maximized(&mut self, _window_id: &str, _maximized: bool) {}
    fn set_host_clipboard(&mut self, _text: &str) {}
    fn set_host_clipboard_image(&mut self, _png_bytes: &[u8]) {}
    fn activate(&mut self, _window_id: &str) {}
    fn set_agent_overlay(&mut self, _overlay: AgentOverlay<'_>) {}
    fn open_web_url(&mut self, _url: &str) -> bool { false }
    fn event_fd(&self) -> Option<std::os::fd::OwnedFd> {
        None
    }
    fn is_frame_pending(&self, _window_id: &str) -> bool {
        false
    }
    fn poll_events(&mut self) -> Vec<SinkInputEvent>;
}
