pub mod wslg;
pub mod wslg_wayland;

use smithay::backend::input::AxisSource;

use crate::protocol::SavedRect;

#[derive(Debug, Clone)]
pub enum SinkInputEvent {
    PointerMotion {
        stage_id: String,
        x: f64,
        y: f64,
        time: u32,
    },
    PointerButton {
        stage_id: String,
        button: u32,
        pressed: bool,
        x: f64,
        y: f64,
        time: u32,
    },
    PointerScroll {
        stage_id: String,
        dx: f64,
        dy: f64,
        source: Option<AxisSource>,
        v120: Option<(i32, i32)>,
        stop: (bool, bool),
        time: u32,
    },
    Key {
        stage_id: String,
        evdev_keycode: u32,
        pressed: bool,
        time: u32,
    },
    Resized {
        stage_id: String,
        rect: SavedRect,
    },
    CloseRequested {
        stage_id: String,
    },
    HostClipboardChanged {
        text: String,
    },
    FilesDropped {
        stage_id: String,
        paths: Vec<String>,
        x: f64,
        y: f64,
    },
}

pub trait DisplaySink {
    fn show(&mut self, stage_id: &str, title: &str, wm_class: &str, rect: SavedRect);
    fn hide(&mut self, stage_id: &str) -> Option<SavedRect>;
    fn destroy(&mut self, stage_id: &str);
    fn present(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]);
    fn set_title(&mut self, stage_id: &str, title: &str);
    fn begin_move(&mut self, _stage_id: &str) {}
    fn begin_resize(&mut self, _stage_id: &str, _direction: u32) {}
    fn minimize(&mut self, _stage_id: &str) {}
    fn set_maximized(&mut self, _stage_id: &str, _maximized: bool) {}
    fn set_host_clipboard(&mut self, _text: &str) {}
    fn event_fd(&self) -> Option<std::os::fd::OwnedFd> {
        None
    }
    fn flush(&mut self) {}
    fn is_frame_pending(&self, _stage_id: &str) -> bool {
        false
    }
    fn take_frame_done_stages(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn poll_events(&mut self) -> Vec<SinkInputEvent>;
}
