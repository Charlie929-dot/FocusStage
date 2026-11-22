pub mod wslg;

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
    fn show(&mut self, stage_id: &str, title: &str, rect: SavedRect);
    fn hide(&mut self, stage_id: &str) -> Option<SavedRect>;
    fn destroy(&mut self, stage_id: &str);
    fn present(&mut self, stage_id: &str, width: u32, height: u32, bgra: &[u8]);
    fn set_title(&mut self, stage_id: &str, title: &str);
    fn poll_events(&mut self) -> Vec<SinkInputEvent>;
}
