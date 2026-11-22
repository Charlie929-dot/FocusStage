use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Mutex;

use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{self, DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
    },
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use tracing::{debug, warn};

use crate::protocol::{windows_path_to_wsl, InputOwner};
use crate::state::{ClientState, FocusStageState};

#[derive(Debug, Default)]
pub struct DataSourceUserData {
    pub mime_types: Mutex<Vec<String>>,
    pub stage_id: Mutex<Option<String>>,
}

#[derive(Debug)]
pub enum OfferBackend {
    StaticText(String),
    StaticUriList(String),
    ClientSource(WlDataSource),
}

#[derive(Debug)]
pub struct DataOfferUserData {
    pub backend: OfferBackend,
    pub mime_types: Vec<String>,
}

#[derive(Debug, Default)]
pub struct StageDataDeviceManagerState {
    pub global: Option<GlobalId>,
    pub devices: Vec<(String, Weak<WlDataDevice>)>,
    pub latest_host_clipboard: Option<String>,
    pub host_clipboard_seq: u64,
}

impl StageDataDeviceManagerState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<FocusStageState, WlDataDeviceManager, ()>(3, ());
        Self {
            global: Some(global),
            devices: Vec::new(),
            latest_host_clipboard: None,
            host_clipboard_seq: 0,
        }
    }
}

impl GlobalDispatch<WlDataDeviceManager, ()> for FocusStageState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WlDataDeviceManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlDataDeviceManager, ()> for FocusStageState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                let stage_id = state.stage_id_for_client(&client.id());
                data_init.init(
                    id,
                    DataSourceUserData {
                        mime_types: Mutex::new(Vec::new()),
                        stage_id: Mutex::new(stage_id),
                    },
                );
            }
            wl_data_device_manager::Request::GetDataDevice { id, seat } => {
                let stage_id = state
                    .stage_id_for_client(&client.id())
                    .unwrap_or_else(|| "default".to_string());
                let device = data_init.init(id, stage_id.clone());
                state
                    .clipboard_state
                    .devices
                    .retain(|(_, w)| w.upgrade().is_ok());
                state
                    .clipboard_state
                    .devices
                    .push((stage_id.clone(), device.downgrade()));
                let _ = seat;

                // If the Stage already has server-side clipboard text, advertise it immediately.
                if let Some(stage) = state.stages.get(&stage_id) {
                    if let Some(ref text) = stage.clipboard.text {
                        send_text_offer_to_device(dh, &device, text.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, DataSourceUserData> for FocusStageState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlDataSource,
        request: wl_data_source::Request,
        data: &DataSourceUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_source::Request::Offer { mime_type } => {
                let mut mimes = data.mime_types.lock().unwrap();
                if !mimes.contains(&mime_type) {
                    mimes.push(mime_type);
                }
            }
            wl_data_source::Request::SetActions { .. } | wl_data_source::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, DataOfferUserData> for FocusStageState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlDataOffer,
        request: wl_data_offer::Request,
        data: &DataOfferUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_offer::Request::Receive { mime_type, fd } => match &data.backend {
                OfferBackend::StaticText(text) | OfferBackend::StaticUriList(text) => {
                    let bytes = text.as_bytes().to_vec();
                    std::thread::spawn(move || {
                        let mut file = unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) };
                        std::mem::forget(fd);
                        let _ = file.write_all(&bytes);
                    });
                }
                OfferBackend::ClientSource(src) => {
                    if src.is_alive() {
                        src.send(mime_type, fd.as_fd());
                    }
                }
            },
            wl_data_offer::Request::Accept { .. }
            | wl_data_offer::Request::Finish
            | wl_data_offer::Request::SetActions { .. }
            | wl_data_offer::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, String> for FocusStageState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &WlDataDevice,
        request: wl_data_device::Request,
        device_stage_id: &String,
        dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let stage_id = state
            .stage_id_for_client(&client.id())
            .unwrap_or_else(|| device_stage_id.clone());

        match request {
            wl_data_device::Request::SetSelection { source, serial: _ } => {
                debug!("Stage {} set Wayland clipboard selection", stage_id);
                let mut mimes = Vec::new();
                if let Some(ref src) = source {
                    if let Some(ud) = src.data::<DataSourceUserData>() {
                        mimes = ud.mime_types.lock().unwrap().clone();
                    }
                }

                // Read text asynchronously from the client's WlDataSource if it offers text.
                let text_mime = pick_text_mime(&mimes);
                if let (Some(src), Some(mime)) = (&source, text_mime) {
                    if let Ok((read_fd, write_fd)) = create_pipe() {
                        src.send(mime, write_fd.as_fd());
                        // Spawn a short helper thread to read clipboard payload without blocking compositor loop.
                        let tx = state.clipboard_update_tx.clone();
                        let sid = stage_id.clone();
                        std::thread::spawn(move || {
                            let mut file = std::fs::File::from(read_fd);
                            let mut buf = Vec::new();
                            let _ = file.read_to_end(&mut buf);
                            if let Ok(text) = String::from_utf8(buf) {
                                let _ = tx.send((sid, text));
                            }
                        });
                    }
                }

                if let Some(stage) = state.stages.get_mut(&stage_id) {
                    if let Some(old) = stage.clipboard.client_source.take() {
                        if Some(&old) != source.as_ref() && old.is_alive() {
                            old.cancelled();
                        }
                    }
                    stage.clipboard.client_source = source.clone();
                    stage.clipboard.mime_types = mimes.clone();
                    stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
                    if source.is_none() {
                        stage.clipboard.text = None;
                    }
                }

                // Advertise ONLY to other WlDataDevices within the SAME Stage (never across Stages!).
                if let Some(ref src) = source {
                    for (dev_sid, weak_dev) in &state.clipboard_state.devices {
                        if dev_sid == &stage_id {
                            if let Ok(dev) = weak_dev.upgrade() {
                                if dev.id() != resource.id() {
                                    send_source_offer_to_device(dh, &dev, src.clone(), &mimes);
                                }
                            }
                        }
                    }
                }
            }
            wl_data_device::Request::StartDrag { .. } | wl_data_device::Request::Release => {}
            _ => {}
        }
    }
}

pub fn pick_text_mime(mimes: &[String]) -> Option<String> {
    for preferred in [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
        "TEXT",
    ] {
        if mimes.iter().any(|m| m == preferred) {
            return Some(preferred.to_string());
        }
    }
    mimes.iter().find(|m| m.starts_with("text/")).cloned()
}

fn create_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn send_text_offer_to_device(dh: &DisplayHandle, device: &WlDataDevice, text: String) {
    let Some(client) = device.client() else {
        return;
    };
    let mimes = vec![
        "text/plain;charset=utf-8".to_string(),
        "UTF8_STRING".to_string(),
        "text/plain".to_string(),
    ];
    let offer = match client.create_resource::<WlDataOffer, _, FocusStageState>(
        dh,
        device.version(),
        DataOfferUserData {
            backend: OfferBackend::StaticText(text),
            mime_types: mimes.clone(),
        },
    ) {
        Ok(o) => o,
        Err(err) => {
            warn!("Failed to create WlDataOffer: {}", err);
            return;
        }
    };
    device.data_offer(&offer);
    for m in mimes {
        offer.offer(m);
    }
    device.selection(Some(&offer));
}

fn send_source_offer_to_device(
    dh: &DisplayHandle,
    device: &WlDataDevice,
    source: WlDataSource,
    mimes: &[String],
) {
    let Some(client) = device.client() else {
        return;
    };
    let offer = match client.create_resource::<WlDataOffer, _, FocusStageState>(
        dh,
        device.version(),
        DataOfferUserData {
            backend: OfferBackend::ClientSource(source),
            mime_types: mimes.to_vec(),
        },
    ) {
        Ok(o) => o,
        Err(_) => return,
    };
    device.data_offer(&offer);
    for m in mimes {
        offer.offer(m.clone());
    }
    device.selection(Some(&offer));
}

impl FocusStageState {
    pub fn stage_id_for_client(&self, client_id: &ClientId) -> Option<String> {
        for (sid, stage) in &self.stages {
            if let Some(client) = stage.wayland_clients.get(client_id) {
                if let Some(cs) = client.get_data::<ClientState>() {
                    if let Some(ref s) = cs.stage_id {
                        return Some(s.clone());
                    }
                }
                return Some(sid.clone());
            }
            if stage
                .xwayland_client
                .as_ref()
                .map(|c| &c.id() == client_id)
                .unwrap_or(false)
            {
                return Some(sid.clone());
            }
        }
        None
    }

    pub fn set_stage_clipboard_text(&mut self, stage_id: &str, text: String, from_host: bool) {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        if from_host && stage.input_owner != InputOwner::Human {
            // Agent/Frozen stage is isolated from host clipboard.
            return;
        }
        stage.clipboard.text = Some(text.clone());
        stage.clipboard.mime_types = vec![
            "text/plain;charset=utf-8".to_string(),
            "UTF8_STRING".to_string(),
            "text/plain".to_string(),
        ];
        stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
        stage.clipboard.client_source = None;

        // Push to Wayland devices of this stage
        for (dev_sid, weak_dev) in &self.clipboard_state.devices {
            if dev_sid == stage_id {
                if let Ok(dev) = weak_dev.upgrade() {
                    send_text_offer_to_device(&self.display_handle, &dev, text.clone());
                }
            }
        }

        // Push to XWayland WM if this is an X11 stage
        if let Some(xwm_id) = stage.xwm_id {
            if let Some(wm) = self.xwms.get_mut(&xwm_id) {
                let _ = wm.new_selection(
                    smithay::wayland::selection::SelectionTarget::Clipboard,
                    Some(vec![
                        "UTF8_STRING".to_string(),
                        "text/plain;charset=utf-8".to_string(),
                        "STRING".to_string(),
                    ]),
                );
            }
        }
    }

    pub fn handle_clipboard_update_from_stage(&mut self, stage_id: &str, text: String) {
        let is_human = if let Some(stage) = self.stages.get_mut(stage_id) {
            stage.clipboard.text = Some(text.clone());
            stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
            stage.input_owner == InputOwner::Human
        } else {
            false
        };

        if is_human {
            if self.clipboard_state.latest_host_clipboard.as_deref() != Some(&text) {
                self.clipboard_state.latest_host_clipboard = Some(text.clone());
                self.clipboard_state.host_clipboard_seq += 1;
                // Push the selection to the desktop backend when one is connected.
                if let Some(ref mut sink) = self.display_sink {
                    sink.set_host_clipboard(&text);
                }
            }
        }
    }

    pub fn sync_from_host_clipboard(&mut self, text: String) {
        if self.clipboard_state.latest_host_clipboard.as_deref() == Some(&text) {
            return;
        }
        self.clipboard_state.latest_host_clipboard = Some(text.clone());
        self.clipboard_state.host_clipboard_seq += 1;

        let human_ids: Vec<String> = self
            .stages
            .iter()
            .filter(|(_, s)| s.input_owner == InputOwner::Human)
            .map(|(id, _)| id.clone())
            .collect();

        for sid in human_ids {
            self.set_stage_clipboard_text(&sid, text.clone(), true);
        }
    }

    /// Deliver a Windows or WSL file drag-and-drop to a Stage via `wl_data_device` (`text/uri-list`).
    pub fn drop_files_to_stage(
        &mut self,
        stage_id: &str,
        paths: &[String],
        pos: Option<Point<f64, Logical>>,
    ) -> Result<(), String> {
        self.drop_files_to_window(stage_id, None, paths, pos)
    }

    pub fn drop_files_to_window(
        &mut self, stage_id: &str, window_id: Option<&str>, paths: &[String],
        pos: Option<Point<f64, Logical>>,
    ) -> Result<(), String> {
        let stage = self
            .stages
            .get(stage_id)
            .ok_or_else(|| format!("Stage '{}' not found", stage_id))?;

        let surface = stage
            .input_toplevel(window_id)
            .and_then(|t| t.wl_surface())
            .ok_or_else(|| format!("Stage '{}' has no live surface", stage_id))?;

        let drop_pos = pos.unwrap_or(stage.input.pointer_pos);
        let uri_list = paths
            .iter()
            .map(|p| {
                let wsl = windows_path_to_wsl(p);
                if wsl.starts_with("file://") {
                    wsl
                } else {
                    format!("file://{}", wsl.replace(' ', "%20"))
                }
            })
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n";

        let mut delivered = false;
        for (dev_sid, weak_dev) in &self.clipboard_state.devices {
            if dev_sid == stage_id {
                if let Ok(dev) = weak_dev.upgrade() {
                    if let Some(client) = dev.client().filter(|c| surface.client().is_some_and(|owner| owner.id() == c.id())) {
                        let mimes = vec!["text/uri-list".to_string()];
                        if let Ok(offer) = client.create_resource::<WlDataOffer, _, FocusStageState>(
                            &self.display_handle,
                            dev.version(),
                            DataOfferUserData {
                                backend: OfferBackend::StaticUriList(uri_list.clone()),
                                mime_types: mimes,
                            },
                        ) {
                            let serial: u32 = SERIAL_COUNTER.next_serial().into();
                            dev.data_offer(&offer);
                            offer.offer("text/uri-list".to_string());
                            if offer.version() >= 3 {
                                offer.source_actions(DndAction::Copy);
                            }
                            dev.enter(serial, &surface, drop_pos.x, drop_pos.y, Some(&offer));
                            if offer.version() >= 3 {
                                offer.action(DndAction::Copy);
                            }
                            dev.motion(0, drop_pos.x, drop_pos.y);
                            dev.drop();
                            dev.leave();
                            delivered = true;
                        }
                    }
                }
            }
        }

        if delivered {
            Ok(())
        } else {
            Err(format!("Stage '{}' has no active wl_data_device", stage_id))
        }
    }
}
