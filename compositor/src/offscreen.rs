use std::collections::HashSet;

use drm_fourcc::DrmFourcc;
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, Offscreen, Renderer,
};
use smithay::desktop::PopupManager;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, with_states, SurfaceAttributes, TraversalAction,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;

use crate::stage::{Stage, StageToplevel};

/// Return the Wayland toplevel's `xdg_surface.set_window_geometry` top-left offset
/// (non-zero for CSD clients with drop shadows around the main buffer).
pub fn wayland_window_geometry_loc(surface: &WlSurface) -> Point<i32, smithay::utils::Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map(|geo| geo.loc)
    })
    .unwrap_or_default()
}

pub struct OffscreenCore {
    pub renderer: PixmanRenderer,
    pub virtual_output: Output,
    pub output_size: Size<i32, Physical>,
    pub frame_hz: u32,
}

impl OffscreenCore {
    pub fn new(dh: &DisplayHandle, width: i32, height: i32, scale: f64, frame_hz: u32) -> Self {
        let renderer = PixmanRenderer::new().expect("create PixmanRenderer");
        let output = Output::new(
            "focusstage-0".to_string(),
            PhysicalProperties {
                size: Size::from((508, 286)),
                subpixel: Subpixel::Unknown,
                make: "FocusStage".to_string(),
                model: "VirtualStage".to_string(),
            },
        );
        let mode = Mode {
            size: Size::from((width, height)),
            refresh: (frame_hz * 1000) as i32,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(smithay::output::Scale::Fractional(scale)),
            Some(Point::from((0, 0))),
        );
        output.set_preferred(mode);
        let _global = output.create_global::<crate::state::FocusStageState>(dh);

        Self {
            renderer,
            virtual_output: output,
            output_size: Size::from((width, height)),
            frame_hz,
        }
    }

    pub fn ensure_surface_entered(&self, surface: &WlSurface) {
        self.virtual_output.enter(surface);
    }

    /// Render a Stage's main toplevel + all popups + subsurfaces + X11 override-redirect menus
    /// into BGRA8888 (little-endian ARGB8888 premultiplied) buffer, returning `(width, height, bgra_bytes)`.
    pub fn render_stage_bgra(
        &mut self,
        stage: &Stage,
        x11_override_windows: &[smithay::xwayland::X11Surface],
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let scale = stage.scale.max(1.0);
        let logical_w = stage.logical_size.w.max(1);
        let logical_h = stage.logical_size.h.max(1);
        let phys_w = ((logical_w as f64) * scale).round().max(1.0) as i32;
        let phys_h = ((logical_h as f64) * scale).round().max(1.0) as i32;
        let phys_size: Size<i32, Physical> = Size::from((phys_w, phys_h));
        let buf_size: Size<i32, BufferCoords> = Size::from((phys_w, phys_h));
        let damage = Rectangle::from_size(phys_size);

        let mut elements: Vec<WaylandSurfaceRenderElement<PixmanRenderer>> = Vec::new();

        // Collect all alive toplevels + popups; front-to-back order for draw_render_elements
        // Note: Smithay's draw_render_elements draws front-to-back? Actually in Smithay `draw_render_elements`
        // renders elements in REVERSE order (last = bottom, first = top), so push popups FIRST, then main window!
        // Wait: let's verify! In Smithay `WaylandSurfaceRenderElement`, elements[0] is topmost.
        let main_toplevel = stage.active_toplevel().cloned();

        if let Some(ref tl) = main_toplevel {
            match tl {
                StageToplevel::Wayland(wl_tl) => {
                    let root = wl_tl.wl_surface();
                    let popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
                    // PopupManager yields topmost child popups before their parents, matching
                    // draw_render_elements' front-to-back order (elements[0] is topmost).
                    for (popup, popup_loc) in popups {
                        let psurf = popup.wl_surface();
                        let p_geo_loc = popup_loc - popup.geometry().loc;
                        let loc_phys = Point::<i32, Physical>::from((
                            ((p_geo_loc.x as f64) * scale).round() as i32,
                            ((p_geo_loc.y as f64) * scale).round() as i32,
                        ));
                        let mut popup_elems = render_elements_from_surface_tree(
                            &mut self.renderer,
                            psurf,
                            loc_phys,
                            Scale::from(scale),
                            1.0,
                            Kind::Unspecified,
                        );
                        elements.append(&mut popup_elems);
                    }

                    let geo_loc = wayland_window_geometry_loc(root);
                    let root_loc_phys = Point::<i32, Physical>::from((
                        (-(geo_loc.x as f64) * scale).round() as i32,
                        (-(geo_loc.y as f64) * scale).round() as i32,
                    ));
                    let mut main_elems = render_elements_from_surface_tree(
                        &mut self.renderer,
                        root,
                        root_loc_phys,
                        Scale::from(scale),
                        1.0,
                        Kind::Unspecified,
                    );
                    elements.append(&mut main_elems);
                }
                StageToplevel::X11(x11_tl) => {
                    let main_geo = x11_tl.geometry();
                    // Override-redirect X11 windows (context menus, dropdowns, tooltips) go on top
                    for ovr in x11_override_windows.iter().rev() {
                        if let Some(osurf) = ovr.wl_surface() {
                            let ogeo = ovr.geometry();
                            let rel_x = ogeo.loc.x - main_geo.loc.x;
                            let rel_y = ogeo.loc.y - main_geo.loc.y;
                            let loc_phys = Point::<i32, Physical>::from((
                                ((rel_x as f64) * scale).round() as i32,
                                ((rel_y as f64) * scale).round() as i32,
                            ));
                            let mut ovr_elems = render_elements_from_surface_tree(
                                &mut self.renderer,
                                &osurf,
                                loc_phys,
                                Scale::from(scale),
                                1.0,
                                Kind::Unspecified,
                            );
                            elements.append(&mut ovr_elems);
                        }
                    }

                    if let Some(root) = x11_tl.wl_surface() {
                        let mut main_elems = render_elements_from_surface_tree(
                            &mut self.renderer,
                            &root,
                            (0, 0),
                            Scale::from(scale),
                            1.0,
                            Kind::Unspecified,
                        );
                        elements.append(&mut main_elems);
                    }
                }
            }
        }

        let mut img = self
            .renderer
            .create_buffer(DrmFourcc::Argb8888, buf_size)
            .map_err(|e| format!("create pixman buffer: {e}"))?;

        let mut target = self
            .renderer
            .bind(&mut img)
            .map_err(|e| format!("bind pixman buffer: {e}"))?;

        {
            let mut frame = self
                .renderer
                .render(&mut target, phys_size, Transform::Normal)
                .map_err(|e| format!("begin pixman frame: {e}"))?;

            frame
                .clear(Color32F::new(0.08, 0.09, 0.11, 1.0), &[damage])
                .map_err(|e| format!("clear frame: {e}"))?;

            draw_render_elements(&mut frame, scale, &elements, &[damage])
                .map_err(|e| format!("draw elements: {e}"))?;

            let _sync = frame
                .finish()
                .map_err(|e| format!("finish pixman frame: {e}"))?;
        }

        let mapping = self
            .renderer
            .copy_framebuffer(
                &target,
                Rectangle::from_size(buf_size),
                DrmFourcc::Argb8888,
            )
            .map_err(|e| format!("copy framebuffer: {e}"))?;

        let bytes = self
            .renderer
            .map_texture(&mapping)
            .map_err(|e| format!("map texture: {e}"))?
            .to_vec();

        Ok((phys_w as u32, phys_h as u32, bytes))
    }

    pub fn render_stage_png(
        &mut self,
        stage: &Stage,
        x11_override_windows: &[smithay::xwayland::X11Surface],
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let (w, h, bgra) = self.render_stage_bgra(stage, x11_override_windows)?;
        let rgba = bgra_premul_to_rgba(&bgra);

        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("png header: {e}"))?;
            writer
                .write_image_data(&rgba)
                .map_err(|e| format!("png encode: {e}"))?;
        }
        Ok((w, h, png_bytes))
    }
}

pub fn bgra_premul_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        let b = px[0];
        let g = px[1];
        let r = px[2];
        let a = px[3];
        if a == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            rgba.extend_from_slice(&[r, g, b, 255]);
        } else {
            let af = a as f32 / 255.0;
            let ur = ((r as f32 / af).min(255.0)) as u8;
            let ug = ((g as f32 / af).min(255.0)) as u8;
            let ub = ((b as f32 / af).min(255.0)) as u8;
            rgba.extend_from_slice(&[ur, ug, ub, a]);
        }
    }
    rgba
}

pub fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    let mut visited = HashSet::new();
    with_surface_tree_downward(
        surface,
        (),
        |s, _, &()| {
            if visited.insert(s.clone()) {
                TraversalAction::DoChildren(())
            } else {
                TraversalAction::SkipChildren
            }
        },
        |_surf, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}
