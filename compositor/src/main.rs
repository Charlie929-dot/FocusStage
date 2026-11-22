use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use tracing::{info, warn};

use focusstage_compositor::control::spawn_control_server;
use focusstage_compositor::offscreen::send_frames_surface_tree;
use focusstage_compositor::protocol::{default_socket_path, DisplayState};
use focusstage_compositor::router::InputSource;
use focusstage_compositor::sink::{DisplaySink, SinkInputEvent};
use focusstage_compositor::state::{ClientState, FocusStageState};

#[derive(Debug, Parser)]
#[command(name = "focusstaged", about = "FocusStage multi-focus Wayland/X11 compositor")]
struct Args {
    /// Wayland socket name (e.g. focusstage-0)
    #[arg(long, default_value = "focusstage-0")]
    wayland_display: String,

    /// Control Unix socket path
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Frame callback frequency in Hz for offscreen stages
    #[arg(long, default_value_t = 30)]
    frame_hz: u32,

    /// Default HiDPI scale factor
    #[arg(long, default_value_t = 1.0)]
    scale: f64,

    /// Run headless without connecting to WSLg X11 display
    #[arg(long, default_value_t = false)]
    headless: bool,
}

fn get_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    let mut ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && ucred.pid > 0 {
        Some(ucred.pid as u32)
    } else {
        None
    }
}

fn main() -> anyhow::Result<()> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter("info,smithay=warn")
            .init();
    }

    let args = Args::parse();
    let control_socket = args.socket.unwrap_or_else(default_socket_path);
    let runtime_base = control_socket
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("focusstage-runtime");
    std::fs::create_dir_all(&runtime_base)?;

    let mut event_loop: EventLoop<FocusStageState> = EventLoop::try_new()?;
    let mut display: Display<FocusStageState> = Display::new()?;
    let dh = display.handle();

    let listener = ListeningSocket::bind(&args.wayland_display)
        .or_else(|_| ListeningSocket::bind_auto("focusstage", 0..32))?;
    let wayland_name = listener
        .socket_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&args.wayland_display)
        .to_string();

    let control_rx = spawn_control_server(&control_socket)?;
    let frame_hz = args.frame_hz.clamp(5, 120);
    let tick_dur = Duration::from_millis((1000 / frame_hz).max(8) as u64);

    let mut state = FocusStageState::new(
        &dh,
        event_loop.handle(),
        wayland_name.clone(),
        runtime_base,
        args.scale,
        frame_hz,
        !args.headless,
    );

    info!(
        "focusstaged started: WAYLAND_DISPLAY={}, control_socket={}, wslg={}",
        wayland_name,
        control_socket.display(),
        state.wslg_sink.is_some()
    );

    let timer = Timer::from_duration(tick_dur);
    event_loop
        .handle()
        .insert_source(timer, move |_, _, _| TimeoutAction::ToDuration(tick_dur))
        .map_err(|e| anyhow::anyhow!("insert timer source: {e}"))?;

    loop {
        if state.should_quit {
            break;
        }

        // 1. Accept any waiting Wayland clients
        while let Ok(Some(stream)) = listener.accept() {
            let pid = get_peer_pid(&stream);
            let stage_id = pid.and_then(|p| state.resolve_stage_for_pid(p));
            let cdata = Arc::new(ClientState {
                compositor_state: Default::default(),
                stage_id: stage_id.clone(),
                pid,
            });
            match display.handle().insert_client(stream, cdata) {
                Ok(client) => {
                    if let Some(ref sid) = stage_id {
                        if let Some(stage) = state.stages.get_mut(sid) {
                            stage.wayland_clients.insert(client.id(), client);
                        }
                    }
                }
                Err(e) => warn!("Failed to insert Wayland client: {}", e),
            }
        }

        // 2. Drain Control API messages
        while let Ok(msg) = control_rx.try_recv() {
            state.handle_control_message(msg);
        }

        // 3. Drain clipboard updates from async client source readers
        while let Ok((sid, text)) = state.clipboard_update_rx.try_recv() {
            state.handle_clipboard_update_from_stage(&sid, text);
        }

        // 4. Poll WslgSink events (Human input, resize, close, host clipboard, Xdnd)
        let sink_events = if let Some(ref mut sink) = state.wslg_sink {
            sink.poll_events()
        } else {
            Vec::new()
        };
        for ev in sink_events {
            match ev {
                SinkInputEvent::PointerMotion {
                    stage_id,
                    x,
                    y,
                    time: _,
                } => {
                    if state.is_input_allowed(&stage_id, InputSource::Human, None) {
                        state.send_stage_pointer_motion(&stage_id, x, y);
                    }
                }
                SinkInputEvent::PointerButton {
                    stage_id,
                    button,
                    pressed,
                    x,
                    y,
                    time: _,
                } => {
                    if state.is_input_allowed(&stage_id, InputSource::Human, None) {
                        state.send_stage_button(&stage_id, x, y, button, pressed);
                    }
                }
                SinkInputEvent::PointerScroll {
                    stage_id,
                    dx,
                    dy,
                    time: _,
                } => {
                    if state.is_input_allowed(&stage_id, InputSource::Human, None) {
                        if let Some(pos) = state.stages.get(&stage_id).map(|s| s.input.pointer_pos) {
                            state.send_stage_scroll(&stage_id, pos.x, pos.y, dx, dy);
                        }
                    }
                }
                SinkInputEvent::Key {
                    stage_id,
                    evdev_keycode,
                    pressed,
                    time: _,
                } => {
                    if state.is_input_allowed(&stage_id, InputSource::Human, None) {
                        state.send_stage_evdev_key(&stage_id, evdev_keycode, pressed);
                    }
                }
                SinkInputEvent::Resized { stage_id, rect } => {
                    state.resize_stage_from_sink(&stage_id, rect);
                }
                SinkInputEvent::CloseRequested { stage_id } => {
                    let _ = state.close_stage(&stage_id);
                }
                SinkInputEvent::HostClipboardChanged { text } => {
                    state.sync_from_host_clipboard(text);
                }
                SinkInputEvent::FilesDropped {
                    stage_id,
                    paths,
                    x,
                    y,
                } => {
                    let _ = state.drop_files_to_stage(
                        &stage_id,
                        &paths,
                        Some((x, y).into()),
                    );
                }
            }
        }

        // 5. Tick pending AI action sequences, expire leases, reap exited processes
        state.tick_pending_act_jobs();
        state.expire_leases();
        state.reap_exited_stages();

        // 6. Send frame callbacks to ALL surfaces across ALL Stages (keeps hidden stages rendering!)
        let now_ms = state.now_ms();
        let surfaces: Vec<_> = state
            .stages
            .values()
            .flat_map(|s| s.toplevels.iter().filter_map(|t| t.wl_surface()))
            .collect();
        for surf in surfaces {
            send_frames_surface_tree(&surf, now_ms);
        }

        // 7. Present dirty Desktop stages to WslgSink
        if state.wslg_sink.is_some() {
            let desktop_dirty: Vec<String> = state
                .stages
                .iter()
                .filter(|(_, s)| {
                    s.display == DisplayState::Desktop && s.dirty_for_sink && s.has_live_surface()
                })
                .map(|(id, _)| id.clone())
                .collect();

            for sid in desktop_dirty {
                let xwm_id = state.stages.get(&sid).and_then(|s| s.xwm_id);
                let ovrs = xwm_id
                    .map(|id| state.x11_override_windows_for(id))
                    .unwrap_or_default();
                let rendered = {
                    let stage_ref = state.stages.get(&sid).unwrap();
                    state.offscreen.render_stage_bgra(stage_ref, &ovrs)
                };
                if let Ok((w, h, bgra)) = rendered {
                    if let Some(ref mut sink) = state.wslg_sink {
                        sink.present(&sid, w, h, &bgra);
                    }
                    if let Some(s) = state.stages.get_mut(&sid) {
                        s.dirty_for_sink = false;
                    }
                }
            }
        }

        // 8. Dispatch Wayland protocol & event loop
        let _ = display.dispatch_clients(&mut state);
        let _ = display.flush_clients();
        event_loop.dispatch(Some(Duration::from_millis(8)), &mut state)?;
    }

    let _ = std::fs::remove_file(&control_socket);
    info!("focusstaged shut down cleanly");
    Ok(())
}
