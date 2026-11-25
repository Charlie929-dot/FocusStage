use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use smithay::backend::input::AxisSource;
use smithay::desktop::PopupManager;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{channel as calloop_channel, EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use tracing::{info, warn};

use focusstage_compositor::control::{local_peer_pid, spawn_control_server};
use focusstage_compositor::offscreen::send_frames_surface_tree;
use focusstage_compositor::protocol::{default_socket_path, DisplayState, SavedRect};
use focusstage_compositor::router::InputSource;
use focusstage_compositor::session_env;
use focusstage_compositor::sink::native::NativeSink;
use focusstage_compositor::sink::{DisplaySink, SinkInputEvent};
use focusstage_compositor::stage::ClipboardContent;
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
    #[arg(long, default_value_t = 60)]
    frame_hz: u32,

    /// Default HiDPI scale factor for offscreen mode.
    #[arg(long, default_value_t = 1.0)]
    scale: f64,

    /// Run offscreen without a desktop host.
    #[arg(long, conflicts_with = "native_host")]
    headless: bool,

    /// Start the native Windows host at this WSL path instead of the installed executable.
    #[arg(long, value_name = "FOCUSSTAGE_EXE")]
    native_host: Option<PathBuf>,
}

fn installed_native_host() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))?;
    let path = std::fs::read_to_string(config.join("focusstage/native-host-path")).ok()?;
    let path = path.trim_end_matches(['\r', '\n']);
    if path.is_empty() { None } else { Some(PathBuf::from(path)) }
}

fn connect_desktop(args: &Args) -> (Option<Box<dyn DisplaySink>>, SavedRect, f64) {
    let default_monitor = SavedRect { x: 0, y: 0, width: 1920, height: 1080 };
    let default_scale = if args.scale.is_finite() { args.scale.max(1.0) } else { 1.0 };
    if args.headless {
        info!("Headless mode requested; running offscreen");
    } else if let Some(path) = args.native_host.clone().or_else(installed_native_host) {
        match NativeSink::connect(&path) {
            Ok((host, monitor, scale)) => {
                info!(scale, "Connected to native Win32 display host");
                return (Some(Box::new(host)), monitor, scale);
            }
            Err(err) => warn!("Native Windows host unavailable at {} ({err}); running offscreen", path.display()),
        }
    } else {
        info!("No native Windows host configured; running offscreen");
    }
    (None, default_monitor, default_scale)
}

fn main() -> anyhow::Result<()> {
    session_env::init_daemon_environment();
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter("info,smithay=warn")
            .init();
    }

    let args = Args::parse();
    let control_socket = args.socket.clone().unwrap_or_else(default_socket_path);
    // Child processes and D-Bus activated helpers (focusstage-open) must return to this daemon.
    std::env::set_var("FOCUSSTAGE_SOCKET", &control_socket);
    if !args.headless {
        session_env::sync_desktop_session_environment();
    }
    let (display_sink, monitor, scale) = connect_desktop(&args);

    let mut event_loop: EventLoop<FocusStageState> = EventLoop::try_new()?;
    let display: Display<FocusStageState> = Display::new()?;
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
    let tick_dur = Duration::from_micros((1_000_000 / frame_hz) as u64);

    let mut state = FocusStageState::new(
        &dh,
        event_loop.handle(),
        wayland_name.clone(),
        scale,
        frame_hz,
        monitor,
        display_sink,
    );
    if !args.headless && state.display_sink.is_some() {
        state.ensure_session_helper_stages();
    }

    info!(
        "focusstaged started: WAYLAND_DISPLAY={}, control_socket={}, desktop={}",
        wayland_name,
        control_socket.display(),
        state.display_sink.is_some()
    );

    event_loop
        .handle()
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state| {
                while let Ok(Some(stream)) = listener.accept() {
                    let pid = local_peer_pid(&stream);
                    let stage_id = pid.and_then(|p| state.resolve_stage_for_pid(p));
                    let cdata = Arc::new(ClientState {
                        compositor_state: Default::default(),
                        stage_id: stage_id.clone(),
                        pid,
                    });
                    match state.display_handle.insert_client(stream, cdata) {
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
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert listening socket source: {e}"))?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: dispatch_clients does not drop/replace `Display` or alter its poll_fd.
                let disp = unsafe { display.get_mut() };
                if let Err(e) = disp.dispatch_clients(state) {
                    warn!("Wayland dispatch_clients error: {}", e);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert wayland display source: {e}"))?;

    event_loop
        .handle()
        .insert_source(control_rx, |event, _, state| {
            if let calloop_channel::Event::Msg(msg) = event {
                state.handle_control_message(msg);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert control channel source: {e}"))?;

    if let Some(sink_fd) = state.display_sink.as_ref().and_then(|s| s.event_fd()) {
        event_loop
            .handle()
            .insert_source(
                Generic::new(sink_fd, Interest::READ, Mode::Level),
                |readiness, _, state| {
                    if readiness.error && !readiness.readable {
                        return Ok(PostAction::Remove);
                    }
                    if dispatch_sink_events(state) {
                        let _ = state.display_handle.backend_handle().flush(None);
                    }
                    Ok(if state.display_sink.is_some() { PostAction::Continue } else { PostAction::Remove })
                },
            )
            .map_err(|e| anyhow::anyhow!("insert display sink source: {e}"))?;
    }

    let tick_due = Rc::new(Cell::new(false));
    let tick_flag = tick_due.clone();
    let timer = Timer::from_duration(tick_dur);
    event_loop
        .handle()
        .insert_source(timer, move |_, _, _| {
            tick_flag.set(true);
            TimeoutAction::ToDuration(tick_dur)
        })
        .map_err(|e| anyhow::anyhow!("insert timer source: {e}"))?;

    loop {
        if state.should_quit {
            break;
        }

        dispatch_sink_events(&mut state);

        // Advance pending control requests without blocking the compositor loop.
        state.tick_pending_act_jobs();
        state.expire_leases();
        state.reap_exited_stages();
        state.tick_pending_launches();
        state.popups.cleanup();
        state.virtual_outputs.cleanup();

        let mut desktop_dirty = Vec::new();
        if let Some(sink) = state.display_sink.as_ref() {
            for stage in state.stages.values().filter(|s| s.display == DisplayState::Desktop) {
                for window in &stage.windows {
                    if !window.mapped || !window.dirty || sink.is_frame_pending(&window.id) {
                        continue;
                    }
                    // Render after collecting IDs so we can also update each window's dirty flag.
                    desktop_dirty.push(window.id.clone());
                }
            }
        }
        for id in desktop_dirty {
            let (stage_id, _) = id.rsplit_once('/').unwrap();
            let window = state.stages[stage_id].window(&id).unwrap();
            let overrides = state.x11_override_windows_for(&window.toplevel);
            let sink = state.display_sink.as_mut().unwrap();
            let mut pixels = sink.take_frame_buffer();
            if let Ok((width, height)) = state.offscreen.render_window_bgra_into(
                Some(window), window.logical_size, window.scale, &overrides, &mut pixels,
            ) {
                sink.present_owned(&id, width, height, pixels);
                state.stages.get_mut(stage_id).unwrap().window_mut(&id).unwrap().dirty = false;
            }
        }

        // Let clients prepare the next frame while the host receives the previous one.
        // The pending check above still bounds transport to one frame per window.
        if tick_due.replace(false) {
            let now_ms = state.now_ms();
            for window in state.stages.values().flat_map(|stage| &stage.windows) {
                if let Some(surface) = window.toplevel.wl_surface() {
                    send_frames_surface_tree(&surface, now_ms);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        send_frames_surface_tree(popup.wl_surface(), now_ms);
                    }
                }
            }
        }

        // Flush Wayland replies before blocking on clients, the host, or the frame timer.
        let _ = state.display_handle.backend_handle().flush(None);
        event_loop.dispatch(None, &mut state)?;
    }

    let stage_ids: Vec<String> = state.stages.keys().cloned().collect();
    for id in stage_ids {
        let _ = state.close_stage(&id);
    }
    let _ = std::fs::remove_file(&control_socket);
    info!("focusstaged shut down cleanly");
    Ok(())
}

fn dispatch_sink_events(state: &mut FocusStageState) -> bool {
    let sink_events = if let Some(ref mut sink) = state.display_sink {
        sink.poll_events()
    } else {
        Vec::new()
    };
    let had_events = !sink_events.is_empty();
    for ev in sink_events {
        match ev {
            SinkInputEvent::Disconnected => {
                warn!("Native Windows host disconnected; existing stages remain available offscreen");
                state.send_polkit_reply(focusstage_compositor::polkit::PolkitHostReply::HostDisconnected);
                state.display_sink = None;
            }
            SinkInputEvent::PointerLeave { window_id } => {
                if let Some(stage_id) = human_input_stage(state, &window_id) {
                    state.leave_window_pointer(&stage_id, &window_id);
                }
            }
            SinkInputEvent::PointerMotion { window_id, x, y } => {
                if let Some(stage_id) = human_input_stage(state, &window_id) {
                    state.send_window_pointer_motion_at(&stage_id, Some(&window_id), x, y, state.now_ms());
                }
            }
            SinkInputEvent::PointerButton { window_id, button, pressed, x, y } => {
                if let Some(stage_id) = human_input_stage(state, &window_id) {
                    if pressed {
                        state.activate_sink_window(&stage_id, &window_id);
                    }
                    state.send_window_button_at(&stage_id, Some(&window_id), x, y, button, pressed, state.now_ms());
                }
            }
            SinkInputEvent::PointerScroll { window_id, x, y, dx, dy, v120 } => {
                if let Some(stage_id) = human_input_stage(state, &window_id) {
                    state.send_window_scroll_frame(
                        &stage_id, Some(&window_id), x, y, dx, dy,
                        Some(AxisSource::Wheel), Some(v120), (false, false), None,
                    );
                }
            }
            SinkInputEvent::Key { window_id, evdev_keycode, pressed } => {
                state.handle_sink_key(&window_id, evdev_keycode, pressed);
            }
            SinkInputEvent::TextInput { window_id, text } => {
                state.handle_sink_text_input(&window_id, &text);
            }
            SinkInputEvent::CancelTakeover { window_id } => {
                state.handle_sink_cancel_takeover(&window_id);
            }
            SinkInputEvent::WindowChanged { window_id, state: host_state } => {
                state.update_window_from_sink(&window_id, host_state);
            }
            SinkInputEvent::CloseRequested { window_id } => {
                state.close_window_from_sink(&window_id);
            }
            SinkInputEvent::HostClipboardChanged { text, image_png } => {
                let content = if let Some(png) = image_png {
                    Some(ClipboardContent::ImagePng(png))
                } else {
                    text.map(ClipboardContent::Text)
                };
                state.sync_from_host_clipboard_content(content);
            }
            SinkInputEvent::FilesDropped { window_id, paths, x, y } => {
                if let Some(stage_id) = human_input_stage(state, &window_id) {
                    let _ = state.drop_files_to_window(&stage_id, Some(&window_id), &paths, Some((x, y).into()));
                }
            }
            SinkInputEvent::AuthSubmit {
                cookie,
                user,
                response,
            } => {
                state.send_polkit_reply(focusstage_compositor::polkit::PolkitHostReply::Submit {
                    cookie,
                    user,
                    response,
                });
            }
            SinkInputEvent::AuthCancel { cookie } => {
                state.send_polkit_reply(focusstage_compositor::polkit::PolkitHostReply::Cancel {
                    cookie,
                });
            }
        }
    }
    had_events
}

fn human_input_stage(state: &FocusStageState, window_id: &str) -> Option<String> {
    state.desktop_stage_for_window(window_id)
        .filter(|id| state.is_input_allowed(id, InputSource::Human, None))
}
