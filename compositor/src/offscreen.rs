use std::collections::HashSet;

use drm_fourcc::DrmFourcc;
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::{Element, Kind};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{
    Bind, Color32F, Frame, Offscreen, Renderer,
};
use smithay::desktop::PopupManager;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, with_states, SurfaceAttributes, TraversalAction,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;

use crate::stage::{Stage, StageToplevel, StageWindow};

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
    cached_image: Option<(i32, i32, pixman::Image<'static, 'static>)>,
    cursor_cached_image: Option<(i32, i32, pixman::Image<'static, 'static>)>,
}

impl OffscreenCore {
    pub fn new() -> Self {
        Self {
            renderer: PixmanRenderer::new().expect("create PixmanRenderer"),
            cached_image: None,
            cursor_cached_image: None,
        }
    }

    /// Render one toplevel with its popups, subsurfaces and X11 override-redirect menus
    /// into BGRA8888 (little-endian ARGB8888 premultiplied) bytes inside `out`,
    /// returning `(width, height)`.
    pub fn render_window_bgra_into(
        &mut self,
        window: Option<&StageWindow>,
        size: smithay::utils::Size<i32, smithay::utils::Logical>,
        scale: f64,
        x11_override_windows: &[smithay::xwayland::X11Surface],
        out: &mut Vec<u8>,
    ) -> Result<(u32, u32), String> {
        self.render_window(window, size, scale, x11_override_windows, out, false)
            .map(|dimensions| dimensions.expect("background frames are allowed"))
    }

    fn render_window(
        &mut self,
        window: Option<&StageWindow>,
        size: smithay::utils::Size<i32, smithay::utils::Logical>,
        scale: f64,
        x11_override_windows: &[smithay::xwayland::X11Surface],
        out: &mut Vec<u8>,
        require_content: bool,
    ) -> Result<Option<(u32, u32)>, String> {
        let logical_w = size.w.max(1);
        let logical_h = size.h.max(1);
        let [phys_w, phys_h] = window.and_then(|w| w.pixel_size).map(|size| size.map(|v| v as i32))
            .unwrap_or_else(|| [logical_w, logical_h].map(|v| (v as f64 * scale).round().max(1.0) as i32));
        let phys_size: Size<i32, Physical> = Size::from((phys_w, phys_h));
        let damage = Rectangle::from_size(phys_size);

        let mut elements: Vec<WaylandSurfaceRenderElement<PixmanRenderer>> = Vec::new();

        if let Some(tl) = window.map(|w| &w.toplevel) {
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

        if require_content && !elements.iter().any(|element| {
            element.alpha() > 0.0 && element.geometry(Scale::from(scale)).overlaps(damage)
        }) {
            return Ok(None);
        }

        self.rasterize(&elements, phys_size, scale, Color32F::new(0.08, 0.09, 0.11, 1.0), false, out)?;
        Ok(Some((phys_w as u32, phys_h as u32)))
    }

    pub(crate) fn cursor_bitmap(&mut self, surface: &WlSurface, scale: f64) -> crate::sink::NativeCursor {
        use smithay::input::pointer::CursorImageSurfaceData;
        let bounds = smithay::desktop::utils::bbox_from_surface_tree(surface, (0, 0));
        if bounds.size.w <= 0 || bounds.size.h <= 0 {
            return crate::sink::NativeCursor::Hidden;
        }
        let size = bounds.size.to_physical_precise_round(scale);
        if !(1..=1024).contains(&size.w) || !(1..=1024).contains(&size.h) {
            return crate::sink::NativeCursor::Named("default");
        }
        let origin: Point<i32, Physical> = bounds.loc.to_physical_precise_round(scale);
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|attrs| attrs.lock().unwrap().hotspot)
        })
        .unwrap_or_default()
        .to_physical_precise_round(scale)
            - origin;
        let elements = render_elements_from_surface_tree(
            &mut self.renderer,
            surface,
            (-origin.x, -origin.y),
            scale,
            1.0,
            Kind::Unspecified,
        );
        if elements.is_empty() {
            return crate::sink::NativeCursor::Hidden;
        }
        let mut pixels = Vec::new();
        if self
            .rasterize(
                &elements,
                size,
                scale,
                Color32F::new(0.0, 0.0, 0.0, 0.0),
                true,
                &mut pixels,
            )
            .is_err()
        {
            return crate::sink::NativeCursor::Named("default");
        }
        match encode_png(size.w as u32, size.h as u32, &pixels) {
            Ok(png) => crate::sink::NativeCursor::Bitmap {
                png,
                hotspot_x: hotspot.x,
                hotspot_y: hotspot.y,
            },
            Err(_) => crate::sink::NativeCursor::Named("default"),
        }
    }

    fn rasterize(
        &mut self,
        elements: &[WaylandSurfaceRenderElement<PixmanRenderer>],
        phys_size: Size<i32, Physical>,
        scale: f64,
        clear: Color32F,
        use_cursor_cache: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let (phys_w, phys_h) = (phys_size.w, phys_size.h);
        let buf_size: Size<i32, BufferCoords> = Size::from((phys_w, phys_h));
        let damage = Rectangle::from_size(phys_size);
        let cache = if use_cursor_cache {
            &mut self.cursor_cached_image
        } else {
            &mut self.cached_image
        };
        let mut img = match cache.take() {
            Some((cw, ch, img)) if cw == phys_w && ch == phys_h => img,
            _ => self
                .renderer
                .create_buffer(DrmFourcc::Argb8888, buf_size)
                .map_err(|e| format!("create pixman buffer: {e}"))?,
        };

        let render_res = (|| -> Result<(), String> {
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
                    .clear(clear, &[damage])
                    .map_err(|e| format!("clear frame: {e}"))?;

                draw_render_elements(&mut frame, scale, elements, &[damage])
                    .map_err(|e| format!("draw elements: {e}"))?;

                let _sync = frame
                    .finish()
                    .map_err(|e| format!("finish pixman frame: {e}"))?;
            }
            drop(target);

            let stride = img.stride();
            let row_bytes = (phys_w as usize) * 4;
            let height = img.height();
            let raw = unsafe {
                std::slice::from_raw_parts(img.data() as *const u8, stride * height)
            };
            out.clear();
            let needed = row_bytes * (phys_h as usize);
            if out.capacity() < needed {
                out.reserve(needed - out.capacity());
            }
            if stride == row_bytes {
                out.extend_from_slice(&raw[..needed.min(raw.len())]);
            } else {
                for row in 0..(phys_h as usize).min(height) {
                    let start = row * stride;
                    out.extend_from_slice(&raw[start..start + row_bytes]);
                }
            }
            Ok(())
        })();

        let cache = if use_cursor_cache {
            &mut self.cursor_cached_image
        } else {
            &mut self.cached_image
        };
        *cache = Some((phys_w, phys_h, img));
        render_res
    }

    pub fn render_stage_bgra(
        &mut self,
        stage: &Stage,
        x11_override_windows: &[smithay::xwayland::X11Surface],
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let mut bytes = Vec::new();
        let window = stage.active_window();
        let (w, h) = self.render_window_bgra_into(
            window, stage.logical_size(), window.map_or(stage.default_scale, |w| w.scale),
            x11_override_windows, &mut bytes,
        )?;
        Ok((w, h, bytes))
    }

    pub fn render_stage_png(
        &mut self,
        stage: &Stage,
        x11_override_windows: &[smithay::xwayland::X11Surface],
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let (w, h, bgra) = self.render_stage_bgra(stage, x11_override_windows)?;
        Ok((w, h, encode_png(w, h, &bgra)?))
    }

    pub(crate) fn render_launch_png(
        &mut self,
        window: &StageWindow,
        x11_override_windows: &[smithay::xwayland::X11Surface],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, String> {
        let mut bgra = Vec::new();
        let dimensions = self.render_window(
            Some(window), window.logical_size, window.scale, x11_override_windows, &mut bgra, true,
        )?;
        dimensions.map(|(w, h)| Ok((w, h, encode_png(w, h, &bgra)?))).transpose()
    }
}

fn encode_png(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let rgba = bgra_premul_to_rgba(bgra);
    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| format!("png header: {e}"))?;
    writer.write_image_data(&rgba).map_err(|e| format!("png encode: {e}"))?;
    writer.finish().map_err(|e| format!("png finish: {e}"))?;
    Ok(png_bytes)
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
