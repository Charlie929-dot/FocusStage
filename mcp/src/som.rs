use std::collections::HashMap;
use std::io::Cursor;

const CODE_ALPHABET: &[u8] = b"ACDEFHJKLMNPRSTVWXYZ";

const SOM_PALETTE: &[(u8, u8, u8)] = &[
    (220, 72, 82),  // Coral red
    (36, 168, 206), // Cyan blue
    (52, 182, 110), // Emerald green
    (226, 164, 24), // Amber yellow
    (168, 96, 232), // Violet
    (232, 118, 52), // Orange
    (218, 76, 148), // Rose
    (68, 132, 236), // Bright blue
];

const PILL_BG: (u8, u8, u8) = (18, 22, 30);
const PILL_FG: (u8, u8, u8) = (255, 255, 255);
const BORDER_ALPHA: f32 = 0.52;
const PILL_BG_ALPHA: f32 = 0.82;
const PILL_RIM_ALPHA: f32 = 0.92;

#[derive(Debug, Clone, Copy)]
pub struct MarkTarget {
    /// Center X in Stage logical pixels
    pub x: f64,
    /// Center Y in Stage logical pixels
    pub y: f64,
    /// Bounding box in physical image pixels [x0, y0, x1, y1]
    pub bbox: [i32; 4],
    pub area: usize,
}

#[derive(Debug, Clone)]
pub struct AnnotatedResult {
    pub png_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub marks: HashMap<String, MarkTarget>,
    pub ordered_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    y: usize,
    x0: usize,
    x1: usize,
}

#[derive(Debug, Clone)]
struct Unit {
    runs: Vec<Run>,
    bbox: [i32; 4], // [x0, y0, x1, y1]
    area: usize,
    #[allow(dead_code)]
    filled: bool,
    probe_x: i32,
    probe_y: i32,
}

pub fn encode_id(mut index: usize) -> String {
    let base = CODE_ALPHABET.len();
    let mut len = 2usize;
    let mut span = base * base;
    while index >= span {
        index -= span;
        len += 1;
        span *= base;
    }
    let mut chars = Vec::with_capacity(len);
    for _ in 0..len {
        chars.push(CODE_ALPHABET[index % base]);
        index /= base;
    }
    chars.reverse();
    String::from_utf8(chars).unwrap()
}

pub fn decode_id(code: &str) -> Option<usize> {
    let text = code.trim().to_ascii_uppercase();
    let bytes = text.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let base = CODE_ALPHABET.len();
    let mut offset = 0usize;
    let mut span = base * base;
    for _ in 2..bytes.len() {
        offset += span;
        span *= base;
    }
    let mut value = 0usize;
    for &b in bytes {
        let pos = CODE_ALPHABET.iter().position(|&c| c == b)?;
        value = value * base + pos;
    }
    Some(offset + value)
}

/// Normalize target code: accepts "AE" / "ae", or legacy "P3" / "p3" / "3".
pub fn normalize_mark_code(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    if decode_id(&upper).is_some() {
        return Some(upper);
    }
    let digits = upper.strip_prefix('P').unwrap_or(&upper);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(idx) = digits.parse::<usize>() {
            return Some(encode_id(idx));
        }
    }
    None
}

pub fn annotate_png(png_bytes: &[u8], scale: f64) -> Result<AnnotatedResult, String> {
    let (mut rgb, w, h) = decode_png_rgb(png_bytes)?;
    let scale = if scale > 0.0 { scale } else { 1.0 };

    let units = build_gray_regions(&rgb, w, h);
    let mut marks = HashMap::with_capacity(units.len());
    let mut ordered_codes = Vec::with_capacity(units.len());

    let mut boxes_with_idx: Vec<&Unit> = units.iter().collect();
    // Same spatial ordering as MarkScreen: top-to-bottom bands of 24px, left-to-right, then area.
    boxes_with_idx.sort_by_key(|u| (u.bbox[1] / 24, u.bbox[0], (u.bbox[2] - u.bbox[0]) * (u.bbox[3] - u.bbox[1])));

    let n_colors = SOM_PALETTE.len();
    let mut placed_colors: Vec<([i32; 4], usize)> = Vec::with_capacity(boxes_with_idx.len());
    let mut colored: Vec<(usize, &Unit, i64, (u8, u8, u8))> = Vec::with_capacity(boxes_with_idx.len());

    for (idx, unit) in boxes_with_idx.into_iter().enumerate() {
        let bx = unit.bbox;
        let mut used = [false; 8];
        for &(ox, c) in &placed_colors {
            if bx[0] - 36 < ox[2] && ox[0] < bx[2] + 36 && bx[1] - 36 < ox[3] && ox[1] < bx[3] + 36 {
                used[c] = true;
            }
        }
        let base = idx % n_colors;
        let c_idx = (0..n_colors)
            .map(|k| (base + k) % n_colors)
            .find(|&c| !used[c])
            .unwrap_or(base);
        placed_colors.push((bx, c_idx));
        let area = ((bx[2] - bx[0]) as i64) * ((bx[3] - bx[1]) as i64);
        colored.push((idx, unit, area, SOM_PALETTE[c_idx]));
    }

    let mut canvas = Canvas::new(&mut rgb, w, h);

    // 1. Draw 1px semi-transparent borders largest-first so small controls sit cleanly on top.
    let mut border_order = colored.clone();
    border_order.sort_by(|a, b| b.2.cmp(&a.2));
    for &(_, unit, _, ring) in &border_order {
        let [bx0, by0, bx1, by1] = unit.bbox;
        let bw = bx1 - bx0;
        let bh = by1 - by0;
        if bw <= 2 && bh <= 2 {
            canvas.hline(unit.probe_x - 4, unit.probe_x + 5, unit.probe_y, ring, 0.8, 1);
            canvas.vline(unit.probe_x, unit.probe_y - 4, unit.probe_y + 5, ring, 0.8, 1);
        } else {
            canvas.rect_border(bx0, by0, bx1, by1, ring, 1, BORDER_ALPHA);
            if !(bx0 + bw / 4 <= unit.probe_x
                && unit.probe_x <= bx1 - bw / 4
                && by0 + bh / 4 <= unit.probe_y
                && unit.probe_y <= by1 - bh / 4)
            {
                canvas.hline(unit.probe_x - 3, unit.probe_x + 4, unit.probe_y, ring, 0.75, 1);
                canvas.vline(unit.probe_x, unit.probe_y - 3, unit.probe_y + 4, ring, 0.75, 1);
            }
        }
    }

    // 2. Draw compact dark-pill 2-letter badges smallest-first.
    let all_boxes: Vec<[i32; 4]> = colored.iter().map(|c| c.1.bbox).collect();
    let mut badge_order = colored;
    badge_order.sort_by_key(|&(idx, unit, area, _)| (area, unit.bbox[1], unit.bbox[0], idx));

    let mut occupied: Vec<[i32; 4]> = Vec::with_capacity(badge_order.len());
    for (idx, unit, _, ring) in badge_order {
        let code = encode_id(idx);
        let (tw, th) = badge_size(&code, 1);
        let bx = unit.bbox;
        let other_boxes: Vec<[i32; 4]> = all_boxes
            .iter()
            .copied()
            .filter(|&ob| {
                ob != bx && !(ob[0] <= bx[0] && bx[2] <= ob[2] && ob[1] <= bx[1] && bx[3] <= ob[3])
            })
            .collect();

        if let Some((lx, ly)) = badge_spot(w as i32, h as i32, bx, &other_boxes, tw, th, &occupied) {
            occupied.push([lx, ly, lx + tw, ly + th]);
            canvas.badge(lx, ly, &code, ring, 1);
            marks.insert(
                code.clone(),
                MarkTarget {
                    x: (unit.probe_x as f64) / scale,
                    y: (unit.probe_y as f64) / scale,
                    bbox: bx,
                    area: unit.area,
                },
            );
            ordered_codes.push(code);
        }
    }

    ordered_codes.sort_by_key(|c| decode_id(c).unwrap_or(usize::MAX));
    let out_png = encode_png_rgb(&rgb, w as u32, h as u32)?;
    Ok(AnnotatedResult {
        png_bytes: out_png,
        width: w as u32,
        height: h as u32,
        marks,
        ordered_codes,
    })
}

fn decode_png_rgb(blob: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let mut decoder = png::Decoder::new(Cursor::new(blob));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("png decode header: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("png decode frame: {e}"))?;
    let bytes = &buf[..info.buffer_size()];
    let w = info.width as usize;
    let h = info.height as usize;

    let mut rgb = Vec::with_capacity(w * h * 3);
    match info.color_type {
        png::ColorType::Rgb => rgb.extend_from_slice(bytes),
        png::ColorType::Rgba => {
            for px in bytes.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in bytes {
                rgb.extend_from_slice(&[g, g, g]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in bytes.chunks_exact(2) {
                rgb.extend_from_slice(&[px[0], px[0], px[0]]);
            }
        }
        other => return Err(format!("unsupported png color type: {other:?}")),
    }
    Ok((rgb, w, h))
}

fn encode_png_rgb(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().map_err(|e| format!("png encode header: {e}"))?;
        writer.write_image_data(rgb).map_err(|e| format!("png encode data: {e}"))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// MarkScreen visual segmentation (ms_segment.py port)
// ---------------------------------------------------------------------------

struct DisjointSet {
    parent: Vec<usize>,
    sizes: Vec<usize>,
}

impl DisjointSet {
    fn new(sizes: Vec<usize>) -> Self {
        let parent = (0..sizes.len()).collect();
        Self { parent, sizes }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return;
        }
        if self.sizes[ra] < self.sizes[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        self.sizes[ra] += self.sizes[rb];
    }
}

fn extract_runs(mask: &[u8], colors: Option<&[u8]>, w: usize, h: usize) -> (Vec<Run>, Vec<u8>) {
    let mut runs = Vec::new();
    let mut run_cols = Vec::new();
    for y in 0..h {
        let row = y * w;
        let mut x = 0;
        while x < w {
            if mask[row + x] == 0 {
                x += 1;
                continue;
            }
            let x0 = x;
            if let Some(cols) = colors {
                let c = cols[row + x];
                x += 1;
                while x < w && mask[row + x] != 0 && cols[row + x] == c {
                    x += 1;
                }
                runs.push(Run { y, x0, x1: x });
                run_cols.push(c);
            } else {
                x += 1;
                while x < w && mask[row + x] != 0 {
                    x += 1;
                }
                runs.push(Run { y, x0, x1: x });
            }
        }
    }
    (runs, run_cols)
}

fn run_components(runs: &[Run], run_cols: Option<&[u8]>, diagonal: bool) -> Vec<Vec<Run>> {
    let n = runs.len();
    if n == 0 {
        return Vec::new();
    }
    let sizes: Vec<usize> = runs.iter().map(|r| r.x1 - r.x0).collect();
    let mut dsu = DisjointSet::new(sizes);
    let diag = usize::from(diagonal);

    let mut prev_start;
    let mut prev_end = 0usize;
    let mut cur_start = 0usize;
    let mut cur_row: isize = -2;
    let mut p = 0usize;

    for i in 0..n {
        let r = runs[i];
        let y = r.y as isize;
        if y != cur_row {
            if y == cur_row + 1 {
                prev_start = cur_start;
                prev_end = i;
            } else {
                prev_start = 0;
                prev_end = 0;
            }
            cur_start = i;
            cur_row = y;
            p = prev_start;
        }
        let left = r.x0.saturating_sub(diag);
        let right = r.x1 + diag;
        while p < prev_end && runs[p].x1 <= left {
            p += 1;
        }
        let mut k = p;
        while k < prev_end && runs[k].x0 < right {
            if let Some(cols) = run_cols {
                if cols[i] == cols[k] {
                    dsu.union(i, k);
                }
            } else {
                dsu.union(i, k);
            }
            k += 1;
        }
    }

    let mut root_to_group: HashMap<usize, usize> = HashMap::new();
    let mut groups: Vec<Vec<Run>> = Vec::new();
    for i in 0..n {
        let root = dsu.find(i);
        let gidx = match root_to_group.get(&root) {
            Some(&idx) => idx,
            None => {
                let idx = groups.len();
                root_to_group.insert(root, idx);
                groups.push(Vec::new());
                idx
            }
        };
        groups[gidx].push(runs[i]);
    }
    groups
}

fn runs_area(runs: &[Run]) -> usize {
    runs.iter().map(|r| r.x1 - r.x0).sum()
}

fn runs_bbox(runs: &[Run]) -> [i32; 4] {
    let mut x0 = usize::MAX;
    let mut x1 = 0usize;
    let y0 = runs.first().map(|r| r.y).unwrap_or(0);
    let y1 = runs.last().map(|r| r.y + 1).unwrap_or(0);
    for r in runs {
        x0 = x0.min(r.x0);
        x1 = x1.max(r.x1);
    }
    [x0 as i32, y0 as i32, x1 as i32, y1 as i32]
}

fn build_gray_regions(rgb: &[u8], w: usize, h: usize) -> Vec<Unit> {
    if w < 8 || h < 8 {
        return Vec::new();
    }
    let shift = 3u8;
    let min_area = 3200usize;
    let boundary = 2usize;
    let text_gap = 12i32;

    let n_px = w * h;
    let mut q = vec![0u8; n_px];
    for i in 0..n_px {
        let r = rgb[i * 3] as u32;
        let g = rgb[i * 3 + 1] as u32;
        let b = rgb[i * 3 + 2] as u32;
        let gray = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
        q[i] = gray >> shift;
    }

    let all_ones = vec![1u8; n_px];
    let (q_runs, q_cols) = extract_runs(&all_ones, Some(&q), w, h);
    let q_comps = run_components(&q_runs, Some(&q_cols), false);

    let mut eligible = vec![0u8; n_px];
    let mut bg_colors = [false; 32];
    for comp in &q_comps {
        if runs_area(comp) >= min_area {
            let first = comp[0];
            bg_colors[q[first.y * w + first.x0] as usize] = true;
            for r in comp {
                let row = r.y * w;
                eligible[row + r.x0..row + r.x1].fill(1);
            }
        }
    }

    let mut foreground: Vec<u8> = eligible.iter().map(|&e| 1 - e).collect();
    let mut ink: Vec<u8> = (0..n_px)
        .map(|i| u8::from(foreground[i] != 0 && !bg_colors[q[i] as usize]))
        .collect();

    suppress_nonflat(rgb, &q, w, h, min_area, shift, &mut eligible, &foreground);
    for i in 0..n_px {
        foreground[i] = 1 - eligible[i];
        ink[i] &= foreground[i];
    }

    erode_separators(&mut eligible, &foreground, w, h, boundary);

    let content: Vec<u8> = eligible.iter().map(|&e| 1 - e).collect();
    let (content_runs, _) = extract_runs(&content, None, w, h);
    let content_comps = run_components(&content_runs, None, false);

    let mut raw_units = Vec::new();
    let mut controls = Vec::new();
    for comp in content_comps {
        for (runs, filled) in split_filled_controls(&comp, &ink, w, h, boundary) {
            if runs.is_empty() {
                continue;
            }
            let unit = make_unit(runs, filled);
            if filled {
                controls.push(unit);
            } else {
                raw_units.push(unit);
            }
        }
    }

    let mut units = merge_gray_text_rows(raw_units, text_gap, boundary as i32);
    units.extend(controls);
    units.sort_by_key(|u| (u.bbox[1], u.bbox[0]));

    let frame_area = (w * h) as i64;
    units
        .into_iter()
        .filter(|u| {
            if u.area < 32 {
                return false;
            }
            let bw = (u.bbox[2] - u.bbox[0]) as usize;
            let bh = (u.bbox[3] - u.bbox[1]) as usize;
            if bw == 0 || bh == 0 {
                return false;
            }
            // Drop giant hollow window borders or full-window backdrop slabs.
            if bw * 10 > w * 9 && bh * 10 > h * 9 && u.area * 10 < bw * bh {
                return false;
            }
            if ((bw * bh) as i64) * 100 > frame_area * 85 {
                return false;
            }
            true
        })
        .collect()
}

fn suppress_nonflat(
    rgb: &[u8],
    q: &[u8],
    w: usize,
    h: usize,
    min_area: usize,
    shift: u8,
    eligible: &mut [u8],
    foreground: &[u8],
) {
    if min_area < 64 || foreground.iter().filter(|&&v| v != 0).count() < min_area {
        return;
    }
    let (fg_runs, _) = extract_runs(foreground, None, w, h);
    let fg_comps = run_components(&fg_runs, None, false);
    let big_comps: Vec<Vec<Run>> = fg_comps
        .into_iter()
        .filter(|c| runs_area(c) >= min_area)
        .collect();
    if big_comps.is_empty() {
        return;
    }

    let n_px = w * h;
    let mut rgb332 = vec![0u8; n_px];
    for i in 0..n_px {
        let r = rgb[i * 3];
        let g = rgb[i * 3 + 1];
        let b = rgb[i * 3 + 2];
        rgb332[i] = (b >> 6) | ((g >> 5) << 2) | ((r >> 5) << 5);
    }

    let inner_tol = ((2usize << shift).max(12)) as i32;
    let smooth_min = min_area.min(256);
    let sub_min = 10usize;
    let min_kept = 12usize;

    let mut cy_min = usize::MAX;
    let mut cy_max = 0usize;
    let mut in_comp = vec![0u8; n_px];
    let mut sm_raw = vec![0u8; n_px];
    let mut sm_mask = vec![0u8; n_px];
    let mut rem_mask = vec![0u8; n_px];
    let mut l2_mask = vec![0u8; n_px];
    let mut kept_raw = vec![0u8; n_px];
    let mut final_kept = vec![0u8; n_px];

    let px_diff_ok = |p1: usize, p2: usize, tol: i32| -> bool {
        let dr = (rgb[p1 * 3] as i32 - rgb[p2 * 3] as i32).abs();
        let dg = (rgb[p1 * 3 + 1] as i32 - rgb[p2 * 3 + 1] as i32).abs();
        let db = (rgb[p1 * 3 + 2] as i32 - rgb[p2 * 3 + 2] as i32).abs();
        dr <= tol && dg <= tol && db <= tol
    };

    for comp in &big_comps {
        let bb = runs_bbox(comp);
        let y0 = bb[1] as usize;
        let y1 = bb[3] as usize;
        cy_min = cy_min.min(y0);
        cy_max = cy_max.max(y1);

        for r in comp {
            let row = r.y * w;
            in_comp[row + r.x0..row + r.x1].fill(1);
        }

        for r in comp {
            let y = r.y;
            let row = y * w;
            for x in r.x0..r.x1 {
                let p = row + x;
                let mut ok = true;
                if y > 0 && in_comp[p - w] != 0 && !px_diff_ok(p, p - w, inner_tol) {
                    ok = false;
                }
                if ok && y + 1 < h && in_comp[p + w] != 0 && !px_diff_ok(p, p + w, inner_tol) {
                    ok = false;
                }
                if ok && x > 0 && in_comp[p - 1] != 0 && !px_diff_ok(p, p - 1, inner_tol) {
                    ok = false;
                }
                if ok && x + 1 < w && in_comp[p + 1] != 0 && !px_diff_ok(p, p + 1, inner_tol) {
                    ok = false;
                }
                if ok {
                    sm_raw[p] = 1;
                }
            }
        }

        let (sm_runs, _) = extract_runs(&sm_raw[y0 * w..y1 * w], None, w, y1 - y0);
        for grp in run_components(&sm_runs, None, false) {
            if runs_area(&grp) < smooth_min {
                continue;
            }
            let first_p = (grp[0].y + y0) * w + grp[0].x0;
            let q0 = q[first_p];
            let c0 = rgb332[first_p];
            let varied = grp.iter().any(|r| {
                let row = (r.y + y0) * w;
                (r.x0..r.x1).any(|x| q[row + x] != q0 || rgb332[row + x] != c0)
            });
            if varied {
                for r in grp {
                    let row = (r.y + y0) * w;
                    sm_mask[row + r.x0..row + r.x1].fill(1);
                }
            }
        }

        for r in comp {
            let row = r.y * w;
            for x in r.x0..r.x1 {
                let p = row + x;
                if sm_mask[p] == 0 {
                    rem_mask[p] = 1;
                }
            }
        }

        let (col_runs, cols) = extract_runs(&rem_mask[y0 * w..y1 * w], Some(&rgb332[y0 * w..y1 * w]), w, y1 - y0);
        for grp in run_components(&col_runs, Some(&cols), true) {
            if runs_area(&grp) >= sub_min {
                for r in grp {
                    let row = (r.y + y0) * w;
                    l2_mask[row + r.x0..row + r.x1].fill(1);
                }
            }
        }

        for r in comp {
            let y = r.y;
            let row = y * w;
            let yu = y.saturating_sub(1) * w;
            let yd = (y + 1).min(h - 1) * w;
            for x in r.x0..r.x1 {
                let p = row + x;
                if rem_mask[p] == 0 {
                    continue;
                }
                if l2_mask[p] != 0 {
                    kept_raw[p] = 1;
                    continue;
                }
                let xl = x.saturating_sub(1);
                let xr = (x + 1).min(w - 1);
                let c0 = rgb332[p];
                let mut c1: Option<u8> = None;
                let mut low_color = true;
                for v in [
                    rgb332[row + xl],
                    rgb332[row + xr],
                    rgb332[yu + x],
                    rgb332[yd + x],
                    rgb332[yu + xl],
                    rgb332[yu + xr],
                    rgb332[yd + xl],
                    rgb332[yd + xr],
                ] {
                    if v != c0 {
                        match c1 {
                            None => c1 = Some(v),
                            Some(prev) if prev == v => {}
                            Some(_) => {
                                low_color = false;
                                break;
                            }
                        }
                    }
                }
                if low_color {
                    kept_raw[p] = 1;
                }
            }
        }

        let (kept_runs, _) = extract_runs(&kept_raw[y0 * w..y1 * w], None, w, y1 - y0);
        for grp in run_components(&kept_runs, None, false) {
            if runs_area(&grp) >= min_kept {
                for r in grp {
                    let row = (r.y + y0) * w;
                    final_kept[row + r.x0..row + r.x1].fill(1);
                }
            }
        }

        for r in comp {
            let row = r.y * w;
            for x in r.x0..r.x1 {
                let p = row + x;
                if final_kept[p] == 0 {
                    eligible[p] = 1;
                }
            }
            in_comp[row + r.x0..row + r.x1].fill(0);
            sm_raw[row + r.x0..row + r.x1].fill(0);
            sm_mask[row + r.x0..row + r.x1].fill(0);
            rem_mask[row + r.x0..row + r.x1].fill(0);
            l2_mask[row + r.x0..row + r.x1].fill(0);
            kept_raw[row + r.x0..row + r.x1].fill(0);
            final_kept[row + r.x0..row + r.x1].fill(0);
        }
    }

    // Gentle gradient strip absorption within big component vertical range.
    let gentle_tol = (1i32 << shift.saturating_sub(1)).max(2);
    let g_span = cy_max.saturating_sub(cy_min);
    if g_span > 0 {
        let mut gentle = vec![0u8; g_span * w];
        let mut combined = vec![0u8; g_span * w];
        let mut any_gentle = false;
        for gy in 0..g_span {
            let y = cy_min + gy;
            let row = y * w;
            for x in 0..w {
                let p = row + x;
                combined[gy * w + x] = eligible[p];
                if eligible[p] != 0 || foreground[p] == 0 {
                    continue;
                }
                let ok = (y == 0 || px_diff_ok(p, p - w, gentle_tol))
                    && (y + 1 >= h || px_diff_ok(p, p + w, gentle_tol))
                    && (x == 0 || px_diff_ok(p, p - 1, gentle_tol))
                    && (x + 1 >= w || px_diff_ok(p, p + 1, gentle_tol));
                if ok {
                    gentle[gy * w + x] = 1;
                    combined[gy * w + x] = 1;
                    any_gentle = true;
                }
            }
        }
        if any_gentle {
            let (g_runs, _) = extract_runs(&combined, None, w, g_span);
            for grp in run_components(&g_runs, None, false) {
                if runs_area(&grp) >= min_area {
                    for r in grp {
                        let y = cy_min + r.y;
                        for x in r.x0..r.x1 {
                            if gentle[r.y * w + x] != 0 {
                                eligible[y * w + x] = 1;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn erode_separators(eligible: &mut [u8], foreground: &[u8], w: usize, h: usize, boundary: usize) {
    if boundary == 0 {
        return;
    }
    let radius = boundary - 1;
    let mut edge = Vec::new();
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let p = row + x;
            if eligible[p] == 0 {
                continue;
            }
            let touches_fg = (y > 0 && foreground[p - w] != 0)
                || (y + 1 < h && foreground[p + w] != 0)
                || (x > 0 && foreground[p - 1] != 0)
                || (x + 1 < w && foreground[p + 1] != 0);
            if touches_fg {
                edge.push((x, y));
            }
        }
    }
    for (ex, ey) in edge {
        let x0 = ex.saturating_sub(radius);
        let x1 = (ex + radius + 1).min(w);
        let y0 = ey.saturating_sub(radius);
        let y1 = (ey + radius + 1).min(h);
        for y in y0..y1 {
            eligible[y * w + x0..y * w + x1].fill(0);
        }
    }
}

fn split_filled_controls(
    comp: &[Run],
    ink: &[u8],
    w: usize,
    _h: usize,
    boundary: usize,
) -> Vec<(Vec<Run>, bool)> {
    const MIN_SIDE: usize = 8;
    let bb = runs_bbox(comp);
    let x0 = bb[0] as usize;
    let y0 = bb[1] as usize;
    let x1 = bb[2] as usize;
    let y1 = bb[3] as usize;
    let cw = x1 - x0;
    let ch = y1 - y0;
    if cw < MIN_SIDE || ch < MIN_SIDE {
        return vec![(comp.to_vec(), false)];
    }

    let mut comp_mask = vec![0u8; cw * ch];
    let mut ink_mask = vec![0u8; cw * ch];
    for r in comp {
        let ly = r.y - y0;
        let row_local = ly * cw;
        let row_global = r.y * w;
        for x in r.x0..r.x1 {
            let lx = x - x0;
            comp_mask[row_local + lx] = 1;
            if ink[row_global + x] != 0 {
                ink_mask[row_local + lx] = 1;
            }
        }
    }

    let (ink_runs, _) = extract_runs(&ink_mask, None, cw, ch);
    let ink_comps = run_components(&ink_runs, None, false);
    let mut out = Vec::new();
    let mut split = false;

    for piece in ink_comps {
        let pb = runs_bbox(&piece);
        let pw = (pb[2] - pb[0]) as usize;
        let ph = (pb[3] - pb[1]) as usize;
        let area = runs_area(&piece);
        if pw.min(ph) < MIN_SIDE || 3 * area < 2 * pw * ph {
            continue;
        }
        let mut spans: HashMap<usize, (usize, usize)> = HashMap::new();
        for r in &piece {
            spans
                .entry(r.y)
                .and_modify(|(lo, hi)| {
                    *lo = (*lo).min(r.x0);
                    *hi = (*hi).max(r.x1);
                })
                .or_insert((r.x0, r.x1));
        }

        let mut body_mask = vec![0u8; cw * ch];
        for (&sy, &(a, b)) in &spans {
            let la = a.saturating_sub(boundary);
            let lb = (b + boundary).min(cw);
            let ya = sy.saturating_sub(boundary);
            let yb = (sy + boundary + 1).min(ch);
            for yy in ya..yb {
                for lx in la..lb {
                    if comp_mask[yy * cw + lx] != 0 {
                        body_mask[yy * cw + lx] = 1;
                    }
                }
            }
        }
        for i in 0..comp_mask.len() {
            if body_mask[i] != 0 {
                comp_mask[i] = 0;
            }
        }
        let (body_runs, _) = extract_runs(&body_mask, None, cw, ch);
        let global_runs: Vec<Run> = body_runs
            .into_iter()
            .map(|r| Run {
                y: r.y + y0,
                x0: r.x0 + x0,
                x1: r.x1 + x0,
            })
            .collect();
        if !global_runs.is_empty() {
            out.push((global_runs, true));
            split = true;
        }
    }

    if split {
        let (rest_runs, _) = extract_runs(&comp_mask, None, cw, ch);
        for grp in run_components(&rest_runs, None, false) {
            let global_runs: Vec<Run> = grp
                .into_iter()
                .map(|r| Run {
                    y: r.y + y0,
                    x0: r.x0 + x0,
                    x1: r.x1 + x0,
                })
                .collect();
            if !global_runs.is_empty() {
                out.push((global_runs, false));
            }
        }
        out
    } else {
        vec![(comp.to_vec(), false)]
    }
}

fn make_unit(runs: Vec<Run>, filled: bool) -> Unit {
    let bbox = runs_bbox(&runs);
    let area = runs_area(&runs).max(1);
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    for r in &runs {
        let len = (r.x1 - r.x0) as f64;
        sum_x += ((r.x0 + r.x1 - 1) as f64) * 0.5 * len;
        sum_y += (r.y as f64) * len;
    }
    let cx = sum_x / (area as f64);
    let cy = sum_y / (area as f64);
    let mut best_x = bbox[0];
    let mut best_y = bbox[1];
    let mut best_d2 = f64::INFINITY;
    for r in &runs {
        let px = (cx.round() as i32).clamp(r.x0 as i32, (r.x1 - 1) as i32);
        let py = r.y as i32;
        let dx = (px as f64) - cx;
        let dy = (py as f64) - cy;
        let d2 = dx * dx + dy * dy;
        if d2 < best_d2 {
            best_d2 = d2;
            best_x = px;
            best_y = py;
        }
    }
    Unit {
        runs,
        bbox,
        area,
        filled,
        probe_x: best_x,
        probe_y: best_y,
    }
}

fn union_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut sorted = runs;
    sorted.sort_by_key(|r| (r.y, r.x0, r.x1));
    let mut out: Vec<Run> = Vec::new();
    for r in sorted {
        if let Some(last) = out.last_mut() {
            if last.y == r.y && r.x0 <= last.x1 {
                last.x1 = last.x1.max(r.x1);
                continue;
            }
        }
        out.push(r);
    }
    out
}

fn merge_gray_text_rows(units: Vec<Unit>, gap: i32, boundary: i32) -> Vec<Unit> {
    if gap <= 0 {
        return units;
    }
    const MAX_HEIGHT: i32 = 64;
    let mut candidates = Vec::new();
    let mut rows = Vec::new();
    for u in units {
        if u.bbox[3] - u.bbox[1] <= MAX_HEIGHT {
            candidates.push(u);
        } else {
            rows.push(u);
        }
    }
    candidates.sort_by_key(|u| u.bbox[0]);

    let fits = |a: [i32; 4], b: [i32; 4]| -> bool {
        let aw = a[2] - a[0];
        let ah = a[3] - a[1];
        let bw = b[2] - b[0];
        let bh = b[3] - b[1];
        let (short, tall) = if ah <= bh { (ah, bh) } else { (bh, ah) };
        let overlap = a[3].min(b[3]) - a[1].max(b[1]);
        if overlap < (2 * short / 3).max(1) {
            return false;
        }
        let limit = if overlap < (3 * tall / 5).max(1) {
            let sw = if ah < bh { aw } else { bw };
            if 2 * short.max(sw) > tall {
                return false;
            }
            (tall + 2) / 3
        } else {
            3 * (tall - 2 * boundary).max(0) / 4
        };
        b[0] - a[2] + 2 * boundary <= gap.min(limit.max(2))
    };

    let n = candidates.len();
    let mut dsu = DisjointSet::new(vec![1; n]);
    let search_gap = gap.min(MAX_HEIGHT / 2);

    for i in 0..n {
        let a = candidates[i].bbox;
        let limit_x = a[2] + search_gap;
        for j in (i + 1)..n {
            let b = candidates[j].bbox;
            if b[0] > limit_x {
                break;
            }
            if fits(a, b) {
                dsu.union(i, j);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<Unit>> = HashMap::new();
    for (i, u) in candidates.into_iter().enumerate() {
        let root = dsu.find(i);
        groups.entry(root).or_default().push(u);
    }

    for (_, mut grp) in groups {
        if grp.len() == 1 {
            rows.push(grp.pop().unwrap());
        } else {
            let all_runs: Vec<Run> = grp.into_iter().flat_map(|u| u.runs).collect();
            rows.push(make_unit(union_runs(all_runs), false));
        }
    }
    rows.sort_by_key(|u| (u.bbox[1], u.bbox[0]));
    rows
}

// ---------------------------------------------------------------------------
// MarkScreen Set-of-Mark Canvas & Badge Placement (ms_grid_image.py port)
// ---------------------------------------------------------------------------

struct Canvas<'a> {
    rgb: &'a mut [u8],
    w: i32,
    h: i32,
}

impl<'a> Canvas<'a> {
    fn new(rgb: &'a mut [u8], w: usize, h: usize) -> Self {
        Self {
            rgb,
            w: w as i32,
            h: h as i32,
        }
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: (u8, u8, u8), alpha: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let p = ((y * self.w + x) * 3) as usize;
        if alpha >= 1.0 {
            self.rgb[p] = color.0;
            self.rgb[p + 1] = color.1;
            self.rgb[p + 2] = color.2;
        } else {
            let k = (alpha * 256.0).round().clamp(0.0, 256.0) as u32;
            let inv = 256 - k;
            self.rgb[p] = (((self.rgb[p] as u32) * inv + (color.0 as u32) * k) >> 8) as u8;
            self.rgb[p + 1] = (((self.rgb[p + 1] as u32) * inv + (color.1 as u32) * k) >> 8) as u8;
            self.rgb[p + 2] = (((self.rgb[p + 2] as u32) * inv + (color.2 as u32) * k) >> 8) as u8;
        }
    }

    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: (u8, u8, u8), alpha: f32, thickness: i32) {
        let xa = x0.max(0);
        let xb = x1.min(self.w);
        if xb <= xa || y < 0 || y >= self.h {
            return;
        }
        let ya = y.max(0);
        let yb = (y + thickness).min(self.h);
        for yy in ya..yb {
            for xx in xa..xb {
                self.blend_pixel(xx, yy, color, alpha);
            }
        }
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, color: (u8, u8, u8), alpha: f32, thickness: i32) {
        let ya = y0.max(0);
        let yb = y1.min(self.h);
        if yb <= ya || x < 0 || x >= self.w {
            return;
        }
        let xa = x.max(0);
        let xb = (x + thickness).min(self.w);
        for xx in xa..xb {
            for yy in ya..yb {
                self.blend_pixel(xx, yy, color, alpha);
            }
        }
    }

    fn fill_box(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: (u8, u8, u8), alpha: f32) {
        let xa = x0.max(0);
        let xb = x1.min(self.w);
        let ya = y0.max(0);
        let yb = y1.min(self.h);
        if xb <= xa || yb <= ya {
            return;
        }
        for yy in ya..yb {
            for xx in xa..xb {
                self.blend_pixel(xx, yy, color, alpha);
            }
        }
    }

    fn rect_border(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: (u8, u8, u8), thickness: i32, alpha: f32) {
        let xa = x0.max(0);
        let xb = x1.min(self.w);
        let ya = y0.max(0);
        let yb = y1.min(self.h);
        if xb <= xa || yb <= ya {
            return;
        }
        let t = thickness.min(((xb - xa) / 2).max(1)).min(((yb - ya) / 2).max(1));
        self.hline(xa, xb, ya, color, alpha, t);
        if yb - t > ya {
            self.hline(xa, xb, yb - t, color, alpha, t);
        }
        let inner_y0 = ya + t;
        let inner_y1 = yb - t;
        if inner_y1 > inner_y0 {
            self.vline(xa, inner_y0, inner_y1, color, alpha, t);
            if xb - t > xa {
                self.vline(xb - t, inner_y0, inner_y1, color, alpha, t);
            }
        }
    }

    fn badge(&mut self, x: i32, y: i32, value: &str, rim: (u8, u8, u8), scale: i32) {
        let (tw, th) = badge_size(value, scale);
        self.fill_box(x + 1, y + 1, x + tw - 1, y + th - 1, PILL_BG, PILL_BG_ALPHA);
        self.rect_border(x, y, x + tw, y + th, rim, 1, PILL_RIM_ALPHA);
        self.draw_text(x + 3, y + 2, value, PILL_FG, scale);
    }

    fn draw_text(&mut self, x: i32, y: i32, value: &str, color: (u8, u8, u8), scale: i32) {
        let mut cx = x;
        for ch in value.to_ascii_uppercase().chars() {
            let (gw, rows) = glyph_for(ch);
            for (r_idx, &bits) in rows.iter().enumerate() {
                for col in 0..gw {
                    if (bits >> (gw - 1 - col)) & 1 == 1 {
                        self.fill_box(
                            cx + (col as i32) * scale,
                            y + (r_idx as i32) * scale,
                            cx + ((col + 1) as i32) * scale,
                            y + ((r_idx + 1) as i32) * scale,
                            color,
                            1.0,
                        );
                    }
                }
            }
            cx += ((gw as i32) + 1) * scale;
        }
    }
}

fn glyph_for(ch: char) -> (usize, [u8; 8]) {
    match ch {
        '0' => (6, [0b011110, 0b110011, 0b110011, 0b110011, 0b110011, 0b110011, 0b110011, 0b011110]),
        '1' => (6, [0b001100, 0b011100, 0b001100, 0b001100, 0b001100, 0b001100, 0b001100, 0b011110]),
        '2' => (6, [0b011110, 0b110011, 0b000011, 0b001110, 0b011000, 0b110000, 0b110000, 0b111111]),
        '3' => (6, [0b111110, 0b000011, 0b000011, 0b011110, 0b000011, 0b000011, 0b000011, 0b111110]),
        '4' => (6, [0b000110, 0b001110, 0b011010, 0b110010, 0b111111, 0b000010, 0b000010, 0b000010]),
        '5' => (6, [0b111111, 0b110000, 0b110000, 0b111110, 0b000011, 0b000011, 0b110011, 0b011110]),
        '6' => (6, [0b011110, 0b110000, 0b110000, 0b111110, 0b110011, 0b110011, 0b110011, 0b011110]),
        '7' => (6, [0b111111, 0b000011, 0b000110, 0b000110, 0b001100, 0b001100, 0b001100, 0b001100]),
        '8' => (6, [0b011110, 0b110011, 0b110011, 0b011110, 0b110011, 0b110011, 0b110011, 0b011110]),
        '9' => (6, [0b011110, 0b110011, 0b110011, 0b011111, 0b000011, 0b000011, 0b110011, 0b011110]),
        'A' => (6, [0b001100, 0b011110, 0b110011, 0b110011, 0b111111, 0b110011, 0b110011, 0b110011]),
        'B' => (6, [0b111110, 0b110011, 0b110011, 0b111110, 0b110011, 0b110011, 0b110011, 0b111110]),
        'C' => (6, [0b011110, 0b110011, 0b110000, 0b110000, 0b110000, 0b110000, 0b110011, 0b011110]),
        'D' => (6, [0b111100, 0b110010, 0b110011, 0b110011, 0b110011, 0b110011, 0b110010, 0b111100]),
        'E' => (6, [0b111111, 0b110000, 0b110000, 0b111110, 0b110000, 0b110000, 0b110000, 0b111111]),
        'F' => (6, [0b111111, 0b110000, 0b110000, 0b111110, 0b110000, 0b110000, 0b110000, 0b110000]),
        'H' => (6, [0b110011, 0b110011, 0b110011, 0b111111, 0b110011, 0b110011, 0b110011, 0b110011]),
        'J' => (6, [0b001111, 0b000011, 0b000011, 0b000011, 0b000011, 0b110011, 0b110011, 0b011110]),
        'K' => (6, [0b110011, 0b110011, 0b110110, 0b111100, 0b111100, 0b110110, 0b110011, 0b110011]),
        'L' => (6, [0b110000, 0b110000, 0b110000, 0b110000, 0b110000, 0b110000, 0b110000, 0b111111]),
        'M' => (7, [0b1100011, 0b1110111, 0b1111111, 0b1101011, 0b1100011, 0b1100011, 0b1100011, 0b1100011]),
        'N' => (7, [0b1100011, 0b1110011, 0b1111011, 0b1101111, 0b1100111, 0b1100011, 0b1100011, 0b1100011]),
        'P' => (6, [0b111110, 0b110011, 0b110011, 0b111110, 0b110000, 0b110000, 0b110000, 0b110000]),
        'R' => (6, [0b111110, 0b110011, 0b110011, 0b111110, 0b110110, 0b110011, 0b110011, 0b110011]),
        'S' => (6, [0b011111, 0b110000, 0b110000, 0b011110, 0b000011, 0b000011, 0b000011, 0b111110]),
        'T' => (6, [0b111111, 0b111111, 0b001100, 0b001100, 0b001100, 0b001100, 0b001100, 0b001100]),
        'V' => (6, [0b110011, 0b110011, 0b110011, 0b110011, 0b011110, 0b011110, 0b001100, 0b001100]),
        'W' => (7, [0b1100011, 0b1100011, 0b1100011, 0b1100011, 0b1101011, 0b1111111, 0b1110111, 0b1100011]),
        'X' => (6, [0b110011, 0b110011, 0b011110, 0b001100, 0b001100, 0b011110, 0b110011, 0b110011]),
        'Y' => (6, [0b110011, 0b110011, 0b011110, 0b001100, 0b001100, 0b001100, 0b001100, 0b001100]),
        'Z' => (6, [0b111111, 0b000011, 0b000110, 0b001100, 0b011000, 0b110000, 0b110000, 0b111111]),
        _ => (4, [0; 8]),
    }
}

fn badge_size(value: &str, scale: i32) -> (i32, i32) {
    let chars: Vec<char> = value.to_ascii_uppercase().chars().collect();
    if chars.is_empty() {
        return (10, 8 * scale + 4);
    }
    let widths: i32 = chars.iter().map(|&c| glyph_for(c).0 as i32).sum();
    let tw = (widths + (chars.len() as i32 - 1)) * scale;
    (tw + 6, 8 * scale + 4)
}

fn intersects(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

fn badge_spot(
    w: i32,
    h: i32,
    bx: [i32; 4],
    other_boxes: &[[i32; 4]],
    tw: i32,
    th: i32,
    occupied: &[[i32; 4]],
) -> Option<(i32, i32)> {
    let [bx0, by0, bx1, by1] = bx;
    let bw = bx1 - bx0;
    let bh = by1 - by0;

    let free = |lx: i32, ly: i32| -> bool {
        if lx < 0 || ly < 0 || lx + tw > w || ly + th > h {
            return false;
        }
        let cand = [lx, ly, lx + tw, ly + th];
        !occupied.iter().any(|&o| intersects(cand, o))
    };

    let intrudes = |lx: i32, ly: i32| -> bool {
        let cand = [lx + 1, ly + 1, lx + tw - 1, ly + th - 1];
        other_boxes.iter().any(|&o| intersects(cand, o))
    };

    let outside = [
        (bx0, by0 - th + 1),
        (bx0 - tw + 1, by0),
        (bx1 - tw, by0 - th + 1),
        (bx0, by1 - 1),
        (bx1 - 1, by0),
        (bx1 - tw, by1 - 1),
    ];
    let inside = [
        (bx0, by0),
        (bx1 - tw, by0),
        (bx0, by1 - th),
        (bx1 - tw, by1 - th),
    ];

    for &(lx, ly) in &outside {
        if free(lx, ly) && !intrudes(lx, ly) {
            return Some((lx, ly));
        }
    }
    if bw >= tw - 4 && bh >= th - 2 {
        for &(lx, ly) in &inside {
            if free(lx, ly) {
                return Some((lx, ly));
            }
        }
    }
    for &(lx, ly) in &outside {
        if free(lx, ly) {
            return Some((lx, ly));
        }
    }
    for &(lx, ly) in &inside {
        if free(lx, ly) {
            return Some((lx, ly));
        }
    }
    let step = (tw / 2).max(8);
    let max_lx = bx0.max(bx1 - tw);
    let mut lx = bx0;
    while lx <= max_lx {
        for ly in [by0 - th + 1, by0, by1 - th, by1 - 1] {
            if free(lx, ly) {
                return Some((lx, ly));
            }
        }
        lx += step;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_canvas(w: usize, h: usize, bg: u8, rects: &[(usize, usize, usize, usize, u8)]) -> Vec<u8> {
        let mut rgb = vec![bg; w * h * 3];
        for &(x0, y0, x1, y1, g) in rects {
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = (y * w + x) * 3;
                    rgb[p] = g;
                    rgb[p + 1] = g;
                    rgb[p + 2] = g;
                }
            }
        }
        rgb
    }

    #[test]
    fn code_alphabet_roundtrip() {
        assert_eq!(encode_id(0), "AA");
        assert_eq!(encode_id(1), "AC");
        assert_eq!(encode_id(399), "ZZ");
        assert_eq!(encode_id(400), "AAA");
        for i in [0, 1, 19, 20, 42, 399, 400, 1024] {
            let code = encode_id(i);
            assert_eq!(decode_id(&code), Some(i));
            assert_eq!(decode_id(&code.to_ascii_lowercase()), Some(i));
        }
        assert_eq!(normalize_mark_code("p3"), Some("AE".to_string()));
        assert_eq!(normalize_mark_code("ae"), Some("AE".to_string()));
    }

    #[test]
    fn keeps_toolbar_icons_separate_and_merges_text_rows() {
        // 3 separate 32x32 toolbar icons should stay 3 separate units.
        let icons = gray_canvas(
            122,
            54,
            40,
            &[(4, 8, 36, 40, 240), (44, 8, 76, 40, 200), (84, 8, 116, 40, 160)],
        );
        let units = build_gray_regions(&icons, 122, 54);
        let bboxes: Vec<[i32; 4]> = units.iter().map(|u| u.bbox).collect();
        assert_eq!(bboxes, vec![[2, 6, 38, 42], [42, 6, 78, 42], [82, 6, 118, 42]]);

        // Thin glyph strokes in one line should merge into one text unit.
        let mut glyphs = vec![(8, 8, 12, 26, 232), (72, 21, 75, 25, 232)];
        for (l, r) in [(17, 29), (32, 49), (52, 69)] {
            glyphs.push((l, 8, l + 3, 26, 232));
            glyphs.push((r - 3, 8, r, 26, 232));
            glyphs.push((l, 15, r, 18, 232));
        }
        let text_img = gray_canvas(100, 50, 32, &glyphs);
        let text_units = build_gray_regions(&text_img, 100, 50);
        assert_eq!(text_units.len(), 1);
        assert_eq!(text_units[0].bbox, [6, 6, 77, 28]);
    }

    #[test]
    fn nonflat_gradient_background_suppressed_keeping_button() {
        let w = 200;
        let h = 150;
        let mut rgb = vec![0u8; w * h * 3];
        // Smooth gradient background
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 3;
                rgb[p] = ((x * 180) / w) as u8;
                rgb[p + 1] = ((y * 180) / h) as u8;
                rgb[p + 2] = 100;
            }
        }
        // Solid bright button in the middle
        for y in 60..90 {
            for x in 70..130 {
                let p = (y * w + x) * 3;
                rgb[p] = 245;
                rgb[p + 1] = 245;
                rgb[p + 2] = 245;
            }
        }
        let png_in = encode_png_rgb(&rgb, w as u32, h as u32).unwrap();
        let res = annotate_png(&png_in, 1.0).unwrap();
        assert_eq!(res.marks.len(), 1);
        let mark = res.marks.get("AA").expect("AA mark");
        assert!(mark.x >= 70.0 && mark.x <= 130.0);
        assert!(mark.y >= 60.0 && mark.y <= 90.0);
        assert_ne!(res.png_bytes, png_in);
    }
}
