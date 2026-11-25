use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use smithay::input::Seat;
use smithay::reexports::wayland_server::{
    backend::ClientId,
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{self, DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::WlDataSource,
    },
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::selection::{
    data_device::{register_data_device, with_source_metadata, ClientDndGrabHandler, DataDeviceHandler,
        DataDeviceState, DataSourceUserData, ServerDndGrabHandler},
    SelectionHandler,
};
use tracing::{debug, warn};

use crate::protocol::{windows_path_to_wsl, InputOwner};
use crate::stage::ClipboardContent;
use crate::state::{ClientState, FocusStageState};

pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardReadKind {
    Text,
    ImagePng,
}

#[derive(Debug)]
pub enum OfferBackend {
    StaticContent(ClipboardContent),
    StaticUriList(String),
    ClientSource(WlDataSource),
}

#[derive(Debug)]
pub struct DataOfferUserData {
    pub backend: OfferBackend,
    pub mime_types: Vec<String>,
}

#[derive(Debug)]
pub struct StageDataDeviceManagerState {
    data_device: DataDeviceState,
    pub devices: Vec<(String, Weak<WlDataDevice>)>,
    pub latest_host_clipboard: Option<ClipboardContent>,
    pub host_clipboard_seq: u64,
}

impl StageDataDeviceManagerState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            data_device: DataDeviceState::new::<FocusStageState>(dh),
            devices: Vec::new(),
            latest_host_clipboard: None,
            host_clipboard_seq: 0,
        }
    }

    pub fn latest_host_text(&self) -> Option<String> {
        self.latest_host_clipboard
            .as_ref()
            .and_then(|c| c.text().map(str::to_owned))
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
                data_init.init(id, DataSourceUserData::default());
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
                if let Some(seat) = Seat::<FocusStageState>::from_resource(&seat) {
                    register_data_device(&seat, device.clone());
                }

                // Inherit the latest host clipboard if this Human stage has no selection yet.
                if let Some(host_content) = state.clipboard_state.latest_host_clipboard.clone() {
                    if let Some(stage) = state.stages.get_mut(&stage_id) {
                        if stage.clipboard.content.is_none()
                            && stage.clipboard.client_source.is_none()
                            && stage.clipboard.seq == 0
                            && stage.input_owner == InputOwner::Human
                        {
                            stage.clipboard.content = Some(host_content.clone());
                            stage.clipboard.mime_types = host_content.mime_types();
                            stage.clipboard.seq = 1;
                        }
                    }
                }

                // If the Stage already has a clipboard selection, advertise it immediately.
                if let Some(stage) = state.stages.get(&stage_id) {
                    if let Some(ref content) = stage.clipboard.content {
                        send_content_offer_to_device(dh, &device, content.clone());
                    } else if let Some(ref src) = stage.clipboard.client_source {
                        if src.is_alive() {
                            send_source_offer_to_device(dh, &device, src.clone(), &stage.clipboard.mime_types);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

smithay::reexports::wayland_server::delegate_dispatch!(FocusStageState: [WlDataSource: DataSourceUserData] => DataDeviceState);

// FocusStage owns clipboard policy; Smithay handles negotiation for client drags.
impl SelectionHandler for FocusStageState {
    type SelectionUserData = ();
}
impl ClientDndGrabHandler for FocusStageState {}
impl ServerDndGrabHandler for FocusStageState {}
impl DataDeviceHandler for FocusStageState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.clipboard_state.data_device
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
                OfferBackend::StaticContent(content) => {
                    let bytes = match content {
                        ClipboardContent::Text(text) => text.as_bytes().to_vec(),
                        ClipboardContent::ImagePng(png) => png.clone(),
                    };
                    std::thread::spawn(move || {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&bytes);
                    });
                }
                OfferBackend::StaticUriList(text) => {
                    let bytes = text.as_bytes().to_vec();
                    std::thread::spawn(move || {
                        let mut file = std::fs::File::from(fd);
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
                    mimes = with_source_metadata(src, |meta| meta.mime_types.clone()).unwrap_or_default();
                }

                let picked = pick_clipboard_mime(&mimes);
                let (expected_seq, xwm_id) = if let Some(stage) = state.stages.get_mut(&stage_id) {
                    if let Some(old) = stage.clipboard.client_source.take() {
                        if Some(&old) != source.as_ref() && old.is_alive() {
                            old.cancelled();
                        }
                    }
                    stage.clipboard.client_source = source.clone();
                    stage.clipboard.mime_types = mimes.clone();
                    stage.clipboard.content = None;
                    stage.clipboard.x11_has_selection = false;
                    stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
                    (stage.clipboard.seq, stage.xwm_id)
                } else {
                    return;
                };

                // Read supported clipboard payload asynchronously from the client's WlDataSource.
                if let (Some(src), Some((mime, kind))) = (&source, picked) {
                    if let Ok((read_fd, write_fd)) = create_pipe() {
                        src.send(mime, write_fd.as_fd());
                        let tx = state.clipboard_update_tx.clone();
                        let sid = stage_id.clone();
                        std::thread::spawn(move || {
                            let file = std::fs::File::from(read_fd);
                            let mut buf = Vec::new();
                            if file
                                .take((MAX_CLIPBOARD_BYTES + 1) as u64)
                                .read_to_end(&mut buf)
                                .is_ok()
                                && buf.len() <= MAX_CLIPBOARD_BYTES
                            {
                                let content = match kind {
                                    ClipboardReadKind::Text => {
                                        String::from_utf8(buf).ok().map(ClipboardContent::Text)
                                    }
                                    ClipboardReadKind::ImagePng => {
                                        (!buf.is_empty()).then_some(ClipboardContent::ImagePng(buf))
                                    }
                                };
                                if let Some(content) = content {
                                    let _ = tx.send((sid, expected_seq, content));
                                }
                            }
                        });
                    }
                }

                // Advertise to other WlDataDevices within the SAME Stage immediately.
                // Cross-stage and Windows propagation happen once the payload read finishes.
                for (dev_sid, weak_dev) in &state.clipboard_state.devices {
                    if dev_sid == &stage_id {
                        if let Ok(dev) = weak_dev.upgrade() {
                            if dev.id() != resource.id() {
                                if let Some(ref src) = source {
                                    send_source_offer_to_device(dh, &dev, src.clone(), &mimes);
                                } else {
                                    dev.selection(None);
                                }
                            }
                        }
                    }
                }

                if let Some(xwm_id) = xwm_id {
                    if let Some(wm) = state.xwms.get_mut(&xwm_id) {
                        let x11_mimes = source.as_ref().map(|_| mimes.clone());
                        let _ = wm.new_selection(
                            smithay::wayland::selection::SelectionTarget::Clipboard,
                            x11_mimes,
                        );
                    }
                }
            }
            wl_data_device::Request::StartDrag { source, origin, icon, serial } => {
                state.start_stage_drag(&stage_id, resource, source, origin, icon, serial.into());
            }
            wl_data_device::Request::Release => {}
            _ => {}
        }
    }
}

pub fn pick_clipboard_mime(mimes: &[String]) -> Option<(String, ClipboardReadKind)> {
    for preferred in [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
        "TEXT",
    ] {
        if mimes.iter().any(|m| m == preferred) {
            return Some((preferred.to_string(), ClipboardReadKind::Text));
        }
    }
    if mimes.iter().any(|m| m == "image/png") {
        return Some(("image/png".to_string(), ClipboardReadKind::ImagePng));
    }
    mimes
        .iter()
        .find(|m| m.starts_with("text/"))
        .cloned()
        .map(|m| (m, ClipboardReadKind::Text))
}

pub fn pick_text_mime(mimes: &[String]) -> Option<String> {
    pick_clipboard_mime(mimes).and_then(|(m, kind)| {
        if kind == ClipboardReadKind::Text {
            Some(m)
        } else {
            None
        }
    })
}

pub(crate) fn create_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn send_content_offer_to_device(
    dh: &DisplayHandle,
    device: &WlDataDevice,
    content: ClipboardContent,
) {
    let Some(client) = device.client() else {
        return;
    };
    let mimes = content.mime_types();
    let offer = match client.create_resource::<WlDataOffer, _, FocusStageState>(
        dh,
        device.version(),
        DataOfferUserData {
            backend: OfferBackend::StaticContent(content),
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
        self.set_stage_clipboard_content(stage_id, Some(ClipboardContent::Text(text)), from_host);
    }

    pub fn set_stage_clipboard_content(
        &mut self,
        stage_id: &str,
        content: Option<ClipboardContent>,
        from_host: bool,
    ) {
        let Some(stage) = self.stages.get_mut(stage_id) else {
            return;
        };
        if from_host && stage.input_owner != InputOwner::Human {
            // Agent/Frozen stage is isolated from host clipboard.
            return;
        }
        if let Some(old) = stage.clipboard.client_source.take() {
            if old.is_alive() {
                old.cancelled();
            }
        }
        stage.clipboard.content = content.clone();
        stage.clipboard.mime_types = content
            .as_ref()
            .map(ClipboardContent::mime_types)
            .unwrap_or_default();
        stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
        stage.clipboard.x11_has_selection = false;

        // Push to Wayland devices of this stage
        for (dev_sid, weak_dev) in &self.clipboard_state.devices {
            if dev_sid == stage_id {
                if let Ok(dev) = weak_dev.upgrade() {
                    match &content {
                        Some(c) => send_content_offer_to_device(&self.display_handle, &dev, c.clone()),
                        None => dev.selection(None),
                    }
                }
            }
        }

        // Push to XWayland WM if this is an X11 stage
        if let Some(xwm_id) = stage.xwm_id {
            if let Some(wm) = self.xwms.get_mut(&xwm_id) {
                let x11_mimes = content.as_ref().map(ClipboardContent::x11_mime_types);
                let _ = wm.new_selection(
                    smithay::wayland::selection::SelectionTarget::Clipboard,
                    x11_mimes,
                );
            }
        }
    }

    pub fn handle_clipboard_update_from_stage(
        &mut self,
        stage_id: &str,
        expected_seq: u64,
        content: ClipboardContent,
    ) {
        let is_human = {
            let Some(stage) = self.stages.get_mut(stage_id) else {
                return;
            };
            if stage.clipboard.seq != expected_seq {
                // Ignore stale async pipe read if a newer selection already replaced it.
                return;
            }
            stage.clipboard.content = Some(content.clone());
            stage.clipboard.seq = stage.clipboard.seq.saturating_add(1);
            stage.input_owner == InputOwner::Human
        };

        if is_human {
            // Propagate across other Human stages so Ctrl+C in one window pastes into another.
            let other_human_ids: Vec<String> = self
                .stages
                .iter()
                .filter(|(id, s)| id.as_str() != stage_id && s.input_owner == InputOwner::Human)
                .map(|(id, _)| id.clone())
                .collect();
            for sid in other_human_ids {
                self.set_stage_clipboard_content(&sid, Some(content.clone()), true);
            }

            if self.clipboard_state.latest_host_clipboard.as_ref() != Some(&content) {
                self.clipboard_state.latest_host_clipboard = Some(content.clone());
                self.clipboard_state.host_clipboard_seq += 1;
                // Push the selection to the desktop backend when one is connected.
                if let Some(ref mut sink) = self.display_sink {
                    match &content {
                        ClipboardContent::Text(text) => sink.set_host_clipboard(text),
                        ClipboardContent::ImagePng(png) => sink.set_host_clipboard_image(png),
                    }
                }
            }
        }
    }

    pub fn sync_from_host_clipboard(&mut self, text: String) {
        self.sync_from_host_clipboard_content(Some(ClipboardContent::Text(text)));
    }

    pub fn sync_from_host_clipboard_content(&mut self, content: Option<ClipboardContent>) {
        if self.clipboard_state.latest_host_clipboard == content {
            return;
        }
        self.clipboard_state.latest_host_clipboard = content.clone();
        self.clipboard_state.host_clipboard_seq += 1;

        let human_ids: Vec<String> = self
            .stages
            .iter()
            .filter(|(_, s)| s.input_owner == InputOwner::Human)
            .map(|(id, _)| id.clone())
            .collect();

        for sid in human_ids {
            self.set_stage_clipboard_content(&sid, content.clone(), true);
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

        let toplevel = stage.input_toplevel(window_id);
        let surface = toplevel.and_then(|window| window.wl_surface())
            .ok_or_else(|| format!("Stage '{}' has no live surface", stage_id))?;

        let drop_pos = pos.unwrap_or(stage.input.pointer_pos);
        let (surface, drop_pos) = match toplevel {
            Some(crate::stage::StageToplevel::Wayland(_)) => {
                let (target, origin) = self.hit_test_wayland_surface(&surface, drop_pos);
                (target, drop_pos - origin)
            }
            _ => (surface, drop_pos),
        };
        let uri_list = paths
            .iter()
            .map(|p| {
                let wsl = windows_path_to_wsl(p);
                if wsl.starts_with("file://") {
                    wsl
                } else {
                    crate::xdg::file_uri(&wsl)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_clipboard_mime_prefers_plain_text_then_png_over_html() {
        assert_eq!(
            pick_clipboard_mime(&["text/html".into(), "image/png".into()]),
            Some(("image/png".into(), ClipboardReadKind::ImagePng))
        );
        assert_eq!(
            pick_clipboard_mime(&[
                "text/html".into(),
                "text/plain;charset=utf-8".into(),
                "image/png".into(),
            ]),
            Some(("text/plain;charset=utf-8".into(), ClipboardReadKind::Text))
        );
        assert_eq!(
            pick_clipboard_mime(&["text/plain".into(), "UTF8_STRING".into()]),
            Some(("UTF8_STRING".into(), ClipboardReadKind::Text))
        );
        assert_eq!(pick_clipboard_mime(&["application/octet-stream".into()]), None);
    }
}
