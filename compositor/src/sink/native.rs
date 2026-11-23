//! WSL interop transports a private duplex connection to a child Windows process.
//! JSON lines carry metadata and input; frame payloads are raw, packed BGRA pixels.
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{DisplaySink, SinkInputEvent, WindowInfo, WindowState};
use crate::protocol::SavedRect;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostCommand<'a> {
    Show {
        window_id: &'a str,
        stage_id: &'a str,
        title: &'a str,
        wm_class: &'a str,
        rect: SavedRect,
        scale: f64,
        parent_window_id: Option<&'a str>,
        maximized: bool,
    },
    Hide {
        window_id: &'a str,
    },
    Destroy {
        window_id: &'a str,
    },
    Title {
        window_id: &'a str,
        title: &'a str,
    },
    Parent {
        window_id: &'a str,
        parent_window_id: Option<&'a str>,
    },
    Move {
        window_id: &'a str,
    },
    Resize {
        window_id: &'a str,
        direction: u32,
    },
    Minimize {
        window_id: &'a str,
    },
    Maximize {
        window_id: &'a str,
        maximized: bool,
    },
    Clipboard {
        text: &'a str,
    },
    Activate {
        window_id: &'a str,
    },
    OpenUrl {
        url: &'a str,
    },
    Frame {
        window_id: &'a str,
        width: u32,
        height: u32,
        seq: u64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostEvent {
    Ready {
        scale: f64,
        width: u32,
        height: u32,
    },
    State {
        window_id: String,
        #[serde(flatten)]
        state: WindowState,
    },
    PointerMotion {
        window_id: String,
        x: f64,
        y: f64,
    },
    PointerLeave {
        window_id: String,
    },
    PointerButton {
        window_id: String,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
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
    CloseRequested {
        window_id: String,
    },
    HostClipboardChanged {
        text: String,
    },
    FilesDropped {
        window_id: String,
        paths: Vec<String>,
        x: f64,
        y: f64,
    },
    FrameDone {
        window_id: String,
        seq: u64,
    },
}

impl HostEvent {
    fn into_sink_event(self) -> Option<SinkInputEvent> {
        use SinkInputEvent as Input;
        Some(match self {
            Self::Ready { .. } | Self::FrameDone { .. } => return None,
            Self::State { window_id, state } => Input::WindowChanged { window_id, state },
            Self::PointerMotion { window_id, x, y } => Input::PointerMotion {
                window_id,
                x,
                y,
            },
            Self::PointerLeave { window_id } => Input::PointerLeave { window_id },
            Self::PointerButton {
                window_id,
                x,
                y,
                button,
                pressed,
            } => Input::PointerButton {
                window_id,
                x,
                y,
                button,
                pressed,
            },
            Self::PointerScroll {
                window_id,
                x,
                y,
                dx,
                dy,
                v120,
            } => Input::PointerScroll {
                window_id,
                x,
                y,
                dx,
                dy,
                v120,
            },
            Self::Key {
                window_id,
                evdev_keycode,
                pressed,
            } => Input::Key {
                window_id,
                evdev_keycode,
                pressed,
            },
            Self::CloseRequested { window_id } => Input::CloseRequested { window_id },
            Self::HostClipboardChanged { text } => Input::HostClipboardChanged { text },
            Self::FilesDropped {
                window_id,
                paths,
                x,
                y,
            } => Input::FilesDropped {
                window_id,
                paths,
                x,
                y,
            },
        })
    }
}

struct Packet {
    header: Vec<u8>,
    pixels: Vec<u8>,
}

pub struct NativeSink {
    child: Child,
    commands: Sender<Packet>,
    events: Receiver<Option<HostEvent>>,
    wake: UnixStream,
    pending: HashMap<String, u64>,
    frame_done: Vec<String>,
    recycled: Receiver<Vec<u8>>,
    seq: u64,
}

impl NativeSink {
    pub fn connect(path: &Path) -> anyhow::Result<(Self, SavedRect, f64)> {
        let (wake, notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let (tx, events) = mpsc::channel();
        let (commands, rx) = mpsc::channel::<Packet>();
        let (recycle, recycled) = mpsc::sync_channel(4);
        let mut host = Command::new(path);
        host.arg("--window-host")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // A wsl.exe invocation supplies its own relay, which closes when that command exits.
        // Use WSL's instance-wide init relay so the host stays connected to the daemon.
        let init_interop = "/run/WSL/1_interop";
        if std::fs::metadata(init_interop).is_ok_and(|entry| entry.file_type().is_socket()) {
            host.env("WSL_INTEROP", init_interop);
        }
        let child = host.spawn()?;
        let mut sink = Self {
            child,
            commands,
            events,
            wake,
            pending: HashMap::new(),
            frame_done: Vec::new(),
            recycled,
            seq: 0,
        };

        let output = sink.child.stdout.take().unwrap();
        thread::Builder::new()
            .name("focusstage-native-events".into())
            .spawn(move || {
                let mut output = BufReader::new(output);
                let mut notify = notify;
                let mut line = String::new();
                loop {
                    line.clear();
                    match output.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => match serde_json::from_str(&line) {
                            Ok(event) => {
                                if tx.send(Some(event)).is_err() || notify.write_all(&[1]).is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => warn!("Invalid Windows host event: {err}"),
                        },
                        Err(err) => {
                            warn!("Read Windows host: {err}");
                            break;
                        }
                    }
                }
                let _ = tx.send(None);
                let _ = notify.write_all(&[1]);
            })?;

        let (rect, scale) = match sink.events.recv_timeout(Duration::from_secs(3))? {
            Some(HostEvent::Ready {
                scale,
                width,
                height,
            }) if width > 0 && height > 0 && scale > 0.0 => (
                SavedRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                scale,
            ),
            _ => anyhow::bail!("Windows host did not send a ready event"),
        };

        let input = sink.child.stdin.take().unwrap();
        thread::Builder::new()
            .name("focusstage-native-frames".into())
            .spawn(move || {
                let mut input = BufWriter::new(input);
                for packet in rx {
                    let sent = input
                        .write_all(&packet.header)
                        .and_then(|()| input.write_all(&packet.pixels))
                        .and_then(|()| input.flush());
                    if let Err(err) = sent {
                        warn!("Write Windows host: {err}");
                        break;
                    }
                    if !packet.pixels.is_empty() {
                        let _ = recycle.try_send(packet.pixels);
                    }
                }
            })?;
        Ok((sink, rect, scale))
    }

    fn send(&self, command: HostCommand<'_>, pixels: Vec<u8>) -> bool {
        let Ok(mut header) = serde_json::to_vec(&command) else {
            return false;
        };
        header.push(b'\n');
        self.commands.send(Packet { header, pixels }).is_ok()
    }

    fn command(&self, command: HostCommand<'_>) {
        self.send(command, Vec::new());
    }
}

impl Drop for NativeSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl DisplaySink for NativeSink {
    fn show(&mut self, window: WindowInfo<'_>) {
        let WindowInfo {
            window_id,
            stage_id,
            title,
            wm_class,
            rect,
            scale,
            parent_window_id,
            maximized,
        } = window;
        self.command(HostCommand::Show {
            window_id,
            stage_id,
            title,
            wm_class,
            rect,
            scale,
            parent_window_id,
            maximized,
        });
    }

    fn hide(&mut self, window_id: &str) {
        self.command(HostCommand::Hide { window_id });
    }

    fn destroy(&mut self, window_id: &str) {
        self.pending.remove(window_id);
        self.command(HostCommand::Destroy { window_id });
    }

    fn take_frame_buffer(&mut self) -> Vec<u8> {
        let mut pixels = self.recycled.try_recv().unwrap_or_default();
        pixels.clear();
        pixels
    }

    fn present_owned(&mut self, window_id: &str, width: u32, height: u32, bgra: Vec<u8>) {
        self.seq += 1;
        if self.send(
            HostCommand::Frame {
                window_id,
                width,
                height,
                seq: self.seq,
            },
            bgra,
        ) {
            self.pending.insert(window_id.to_string(), self.seq);
        }
    }

    fn set_title(&mut self, window_id: &str, title: &str) {
        self.command(HostCommand::Title { window_id, title });
    }

    fn set_parent(&mut self, window_id: &str, parent_window_id: Option<&str>) {
        self.command(HostCommand::Parent {
            window_id,
            parent_window_id,
        });
    }

    fn begin_move(&mut self, window_id: &str) {
        self.command(HostCommand::Move { window_id });
    }

    fn begin_resize(&mut self, window_id: &str, direction: u32) {
        self.command(HostCommand::Resize {
            window_id,
            direction,
        });
    }

    fn minimize(&mut self, window_id: &str) {
        self.command(HostCommand::Minimize { window_id });
    }

    fn set_maximized(&mut self, window_id: &str, maximized: bool) {
        self.command(HostCommand::Maximize {
            window_id,
            maximized,
        });
    }

    fn set_host_clipboard(&mut self, text: &str) {
        self.command(HostCommand::Clipboard { text });
    }

    fn activate(&mut self, window_id: &str) {
        self.command(HostCommand::Activate { window_id });
    }

    fn open_web_url(&mut self, url: &str) -> bool {
        crate::xdg::valid_web_url(url) && self.send(HostCommand::OpenUrl { url }, Vec::new())
    }

    fn event_fd(&self) -> Option<OwnedFd> {
        self.wake.try_clone().ok().map(Into::into)
    }

    fn is_frame_pending(&self, window_id: &str) -> bool {
        self.pending.contains_key(window_id)
    }

    fn take_frame_done_windows(&mut self) -> Vec<String> {
        std::mem::take(&mut self.frame_done)
    }

    fn poll_events(&mut self) -> Vec<SinkInputEvent> {
        let mut wake = [0; 1024];
        loop {
            match self.wake.read(&mut wake) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            match event {
                Some(HostEvent::FrameDone { window_id, seq }) => {
                    if self.pending.get(&window_id) == Some(&seq) {
                        self.pending.remove(&window_id);
                        self.frame_done.push(window_id);
                    }
                }
                Some(event) => events.extend(event.into_sink_event()),
                None => events.push(SinkInputEvent::Disconnected),
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_state_deserializes_geometry_scale_and_snap_edges() {
        let event = serde_json::from_str::<HostEvent>(r#"{
            "type":"state","window_id":"stage-5/2","rect":{"x":-1440,"y":0,"width":768,"height":592},
            "scale":1.25,"activated":true,"maximized":false,"resizing":false,"tiled":[true,false,true,true],
            "monitor":{"x":-1440,"y":0,"width":1440,"height":900},"pixel_size":[960,740]
        }"#).unwrap();
        let Some(SinkInputEvent::WindowChanged { window_id, state }) = event.into_sink_event()
        else {
            panic!("expected a native window state event");
        };
        assert_eq!(window_id, "stage-5/2");
        assert_eq!(state.scale, 1.25);
        assert_eq!(state.rect.width, 768);
        assert_eq!(state.tiled, [true, false, true, true]);
        assert_eq!(state.pixel_size, Some([960, 740]));
    }
}
