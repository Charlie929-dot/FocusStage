use std::cell::RefCell;
use std::collections::HashMap;

use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle};
use smithay::utils::Transform;
use smithay::wayland::compositor::{
    send_surface_state, with_surface_tree_downward, SurfaceData, TraversalAction,
};
use smithay::wayland::fractional_scale::with_fractional_scale;

use crate::protocol::SavedRect;
use crate::state::FocusStageState;

pub struct VirtualOutputs {
    primary: (i32, i32),
    outputs: HashMap<(i32, i32), Output>,
    frame_hz: u32,
}

impl VirtualOutputs {
    pub fn new(dh: &DisplayHandle, rect: SavedRect, scale: f64, frame_hz: u32) -> Self {
        let mut outputs = Self {
            primary: (rect.x, rect.y),
            outputs: HashMap::new(),
            frame_hz,
        };
        outputs.update(dh, rect, scale);
        outputs
    }

    pub fn update(&mut self, dh: &DisplayHandle, bounds: SavedRect, scale: f64) {
        let origin = (bounds.x, bounds.y);
        let new = !self.outputs.contains_key(&origin);
        let output = self.outputs.entry(origin).or_insert_with(|| {
            Output::new(
                format!("focusstage-{}-{}", origin.0, origin.1),
                PhysicalProperties {
                    size: (0, 0).into(), // Win32 DPI is user scaling, not a physical panel measurement.
                    subpixel: Subpixel::Unknown,
                    make: "FocusStage".to_string(),
                    model: "Virtual monitor".to_string(),
                },
            )
        });
        let mode = Mode {
            size: (bounds.width as i32, bounds.height as i32).into(),
            refresh: (self.frame_hz * 1000) as i32,
        };
        let scale = Scale::Fractional(scale);
        if output.current_mode() != Some(mode)
            || output.current_scale().fractional_scale() != scale.fractional_scale()
        {
            output.set_preferred(mode);
            output.change_current_state(
                Some(mode),
                Some(Transform::Normal),
                Some(scale),
                Some(origin.into()),
            );
        }
        if new {
            output.create_global::<FocusStageState>(dh);
        }
    }

    pub fn enter_tree(&self, root: &WlSurface, monitor: Option<SavedRect>, scale: f64) {
        let output = monitor
            .and_then(|r| self.outputs.get(&(r.x, r.y)))
            .unwrap_or(&self.outputs[&self.primary]);
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, data, _| enter_surface(surface, data, output, scale),
            |_, _, _| true,
        );
    }

    pub fn cleanup(&self) {
        for output in self.outputs.values() {
            output.cleanup();
        }
    }
}

#[derive(Default)]
struct SurfaceOutput(RefCell<Option<Output>>);

fn enter_surface(surface: &WlSurface, data: &SurfaceData, output: &Output, scale: f64) {
    let mut old = data
        .data_map
        .get_or_insert(SurfaceOutput::default)
        .0
        .borrow_mut();
    if old.as_ref() != Some(output) {
        // Enter first so toolkits never see the window screenless between monitors.
        output.enter(surface);
        if let Some(previous) = old.replace(output.clone()) {
            previous.leave(surface);
        }
    }
    with_fractional_scale(data, |state| state.set_preferred_scale(scale));
    send_surface_state(
        surface,
        data,
        scale.ceil().max(1.0) as i32,
        Transform::Normal,
    );
}
