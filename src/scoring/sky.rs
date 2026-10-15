// SPDX-License-Identifier: MIT OR Apache-2.0
//! Sky detection for "Select Sky" masks: a colour and texture heuristic in
//! pure Rust, so it runs on every platform including the browser.
//!
//! Sky touches the top edge, is bright, has little texture, and is blue,
//! white or grey. [`sky_coverage`] finds it in six passes:
//!
//! 1. **Grid.** Average the photo into cells about 1/256 of its long side.
//!    Each cell keeps its mean colour and a texture measure: the luma
//!    spread inside the cell plus the spread of its 3x3 neighbourhood of
//!    cell means. Leaves, brick and stucco are rough at that scale; open
//!    sky is not. Working on ~45k cells keeps the region logic cheap.
//! 2. **Likelihood.** Each cell scores as *blue* (blue above red, not below
//!    green, and saturated, which keeps out shaded white walls the sky
//!    tints a faint blue) or *pale* (bright and near neutral or warm:
//!    clouds, haze, a pale horizon, sunset glow), times a smoothness score.
//! 3. **Grow.** Each smooth blue or pale cell in the top rows seeds a flood
//!    that crosses only to smooth, sky-coloured cells close in colour to
//!    the cell it came from, so a roof line or a tree stops it. Each
//!    flooded region is judged on its own. If any is mostly blue, only the
//!    blue ones are sky. If none is, the photo may be overcast, and a pale
//!    region counts unless it reaches the bottom fifth of the frame: there
//!    it is a wall or a backdrop, the plain wall behind a portrait being
//!    the false positive this guards against.
//! 4. **Extend** clear sky. Blue cells matching the sky's mean blue, such
//!    as sky between branches, seed a second flood. Pale regions anywhere
//!    count as clouds when sky borders more of their rim than non-sky
//!    colours do; a sunlit wall touches the sky along one side only. Then
//!    narrow gaps and enclosed holes of sky colour are filled, and the
//!    border is pushed a few cells into sky-coloured but textured cells
//!    (sky past leaves, a cloud bank's rough underside).
//! 5. **Refine.** The cell mask is upsampled and run through a guided
//!    filter (He et al.) with the photo's luma as the guide. Inside each
//!    small window coverage becomes a linear function of luma, so a
//!    blocky cell edge snaps to the real horizon and a tree line gets
//!    partial coverage that follows the bright gaps between leaves. That
//!    feathering matters because the mask drives a tonal adjustment and a
//!    hard edge would halo.
//! 6. **Subtract and reject.** The subject's coverage is removed, and a
//!    result under 2% of the frame is dropped as noise.
//!
//! Known misses: sky seen only through dense foliage, an all-orange sunset
//! with no pale or blue cell at the top, and overcast sky in a frame shot
//! upward so the sky reaches the bottom fifth. A white backdrop that stops
//! short of the bottom fifth, with no blue sky in the frame, reads as
//! overcast sky.

/// Long side of the analysis grid, in cells.
const GRID_LONG_SIDE: usize = 256;
/// Inputs larger than this many pixels are not previews and return no sky.
/// Mismatched inputs return at most this many zero bytes, since `w * h` is
/// then unbacked by any buffer.
const MAX_PIXELS: usize = 1 << 24;
/// A flood step may cross between cells at most this far apart in RGB (0..1).
const STEP_COLOUR: f32 = 0.09;
/// Coverage below this fraction of the frame is noise, not sky.
const MIN_AREA: f32 = 0.02;
/// Seeds come from this fraction of the grid's top rows (at least one row).
const SEED_BAND: f32 = 0.03;
/// How far, in cells, [`reach_edges`] pushes the sky's border.
const EDGE_REACH: usize = 6;
/// [`reach_edges`] crosses colour steps up to this size (RGB, 0..1).
const EDGE_STEP: f32 = 0.2;
/// Clouds may sit this many cells from the blue sky: their rough edges fail
/// the smoothness test and leave a gap.
const CLOUD_GAP: usize = 3;
/// Guided filter regulariser. Luma steps with a standard deviation well above
/// `sqrt(EPS)` keep their edge; flatter windows just blur.
const GUIDED_EPS: f32 = 0.004;

/// Sky coverage for an upright RGBA8 photo.
///
/// `rgba`: upright RGBA8, `w*h*4` bytes, in source orientation. `subject`:
/// optional coverage at the same w,h (0..255); pixels the subject covers are
/// never sky. Returns sky coverage, `w*h` bytes, 0..255. All zero when the
/// photo has no sky. Mismatched lengths (of `rgba` or `subject`) or zero sizes
/// return all-zero of `w*h` (capped) rather than panic.
pub fn sky_coverage(rgba: &[u8], w: u32, h: u32, subject: Option<&[u8]>) -> Vec<u8> {
    let pixels = (w as u64).saturating_mul(h as u64);
    let n = usize::try_from(pixels).unwrap_or(usize::MAX);
    let rgba_ok = n.checked_mul(4) == Some(rgba.len());
    let subject_ok = subject.is_none_or(|s| s.len() == n);
    if !rgba_ok || !subject_ok {
        return vec![0; n.min(MAX_PIXELS)];
    }
    // From here `rgba` holds 4n bytes, so n more is affordable.
    let zeros = || vec![0u8; n];
    if n == 0 || n > MAX_PIXELS {
        return zeros();
    }
    let (w, h) = (w as usize, h as usize);

    let grid = Grid::build(rgba, w, h);
    let Some(member) = grow_sky(&grid) else {
        return zeros();
    };
    let coarse = upsample(&member, &grid, w, h);
    let refined = guided_filter(&grid.full_luma, &coarse, w, h, (2 * grid.cell).max(3));

    let mut out = Vec::with_capacity(n);
    let mut total = 0.0f64;
    for (i, &q) in refined.iter().enumerate() {
        // Drop the filter's faint tails so far-off pixels get exactly zero.
        let mut c = ((q - 0.04) / 0.92).clamp(0.0, 1.0);
        if let Some(s) = subject.and_then(|s| s.get(i)) {
            c *= 1.0 - *s as f32 / 255.0;
        }
        total += c as f64;
        out.push((c * 255.0 + 0.5) as u8);
    }
    if total < MIN_AREA as f64 * n as f64 {
        return zeros();
    }
    out
}

/// The photo averaged into square cells, plus its full-resolution luma.
struct Grid {
    gw: usize,
    gh: usize,
    /// Cell side in pixels.
    cell: usize,
    rgb: Vec<[f32; 3]>,
    luma: Vec<f32>,
    /// Luma spread at cell scale, see the module doc.
    texture: Vec<f32>,
    /// Luma of every pixel, 0..1, the guided filter's guide.
    full_luma: Vec<f32>,
}

fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.299 * r + 0.587 * g + 0.114 * b
}

impl Grid {
    /// `rgba` holds exactly `w * h * 4` bytes; the caller checked.
    fn build(rgba: &[u8], w: usize, h: usize) -> Grid {
        let cell = w.max(h).div_ceil(GRID_LONG_SIDE).max(1);
        let (gw, gh) = (w.div_ceil(cell), h.div_ceil(cell));
        // r, g, b, luma, luma squared, pixel count.
        let mut acc = vec![[0.0f32; 6]; gw * gh];
        let mut full_luma = Vec::with_capacity(w * h);
        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            let (x, y) = (i % w, i / w);
            let r = px[0] as f32 / 255.0;
            let g = px[1] as f32 / 255.0;
            let b = px[2] as f32 / 255.0;
            let l = luma(r, g, b);
            full_luma.push(l);
            let a = &mut acc[(y / cell) * gw + x / cell];
            a[0] += r;
            a[1] += g;
            a[2] += b;
            a[3] += l;
            a[4] += l * l;
            a[5] += 1.0;
        }

        let mut rgb = Vec::with_capacity(gw * gh);
        let mut luma = Vec::with_capacity(gw * gh);
        let mut inner_var = Vec::with_capacity(gw * gh);
        for a in &acc {
            let k = a[5].max(1.0);
            let l = a[3] / k;
            rgb.push([a[0] / k, a[1] / k, a[2] / k]);
            luma.push(l);
            inner_var.push((a[4] / k - l * l).max(0.0));
        }

        let mut texture = Vec::with_capacity(gw * gh);
        for gy in 0..gh {
            for gx in 0..gw {
                let (mut s, mut s2, mut k) = (0.0f32, 0.0f32, 0.0f32);
                for ny in gy.saturating_sub(1)..(gy + 2).min(gh) {
                    for nx in gx.saturating_sub(1)..(gx + 2).min(gw) {
                        let l = luma[ny * gw + nx];
                        s += l;
                        s2 += l * l;
                        k += 1.0;
                    }
                }
                let m = s / k;
                let outer_var = (s2 / k - m * m).max(0.0);
                texture.push((inner_var[gy * gw + gx] + outer_var).sqrt());
            }
        }

        Grid {
            gw,
            gh,
            cell,
            rgb,
            luma,
            texture,
            full_luma,
        }
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-cell sky likelihoods.
struct Likelihood {
    /// Clear blue sky, times smoothness.
    blue: Vec<f32>,
    /// Bright neutral or warm (cloud, haze, glow), times smoothness.
    pale: Vec<f32>,
    /// Sky-coloured at all, ignoring texture: blue or pale before smoothness.
    tint: Vec<f32>,
}

impl Likelihood {
    fn of(grid: &Grid) -> Likelihood {
        let n = grid.rgb.len();
        let (mut blue, mut pale, mut tint) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        for (i, &[r, g, b]) in grid.rgb.iter().enumerate() {
            let l = grid.luma[i];
            let smooth = 1.0 - smoothstep(0.02, 0.06, grid.texture[i]);
            let hi = r.max(g).max(b);
            let sat = if hi > 0.0 {
                (hi - r.min(g).min(b)) / hi
            } else {
                0.0
            };
            // Saturation keeps out shaded white walls, which the sky tints
            // a faint blue; a pale horizon blue still counts as pale below.
            let is_blue = smoothstep(0.02, 0.07, b - r)
                * smoothstep(-0.08, -0.01, b - g)
                * smoothstep(0.12, 0.22, sat)
                * smoothstep(0.2, 0.35, l);
            let neutral = 1.0 - smoothstep(0.10, 0.22, sat);
            // Sunset glow: red-dominant, still bright, not deeply saturated.
            let warm = smoothstep(0.02, 0.08, r - b) * (1.0 - smoothstep(0.45, 0.65, sat));
            let is_pale = neutral.max(warm) * smoothstep(0.5, 0.72, l);
            blue.push(is_blue * smooth);
            pale.push(is_pale * smooth);
            tint.push(is_blue.max(is_pale));
        }
        Likelihood { blue, pale, tint }
    }
}

fn colour_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dr, dg, db) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dr * dr + dg * dg + db * db).sqrt()
}

/// 4-neighbours of cell `i` on a `gw x gh` grid.
fn neighbours(i: usize, gw: usize, gh: usize) -> impl Iterator<Item = usize> {
    let (x, y) = (i % gw, i / gw);
    [
        (x > 0).then(|| i - 1),
        (x + 1 < gw).then(|| i + 1),
        (y > 0).then(|| i - gw),
        (y + 1 < gh).then(|| i + gw),
    ]
    .into_iter()
    .flatten()
}

/// Give every unlabelled cell reachable from `queue` the label `id`. A step
/// enters only `eligible` cells and only across a colour change under
/// [`STEP_COLOUR`].
fn flood(grid: &Grid, eligible: &[bool], labels: &mut [u32], id: u32, mut queue: Vec<usize>) {
    for &s in &queue {
        labels[s] = id;
    }
    while let Some(i) = queue.pop() {
        for n in neighbours(i, grid.gw, grid.gh) {
            if labels[n] == 0 && eligible[n] && colour_dist(grid.rgb[i], grid.rgb[n]) < STEP_COLOUR
            {
                labels[n] = id;
                queue.push(n);
            }
        }
    }
}

/// Cell counts for one flooded region.
#[derive(Clone, Copy, Default)]
struct Region {
    size: usize,
    /// Cells that are clearly blue.
    blue: usize,
    /// Cells in the bottom fifth of the frame.
    low: usize,
    /// Neighbouring cells outside the region that are within [`CLOUD_GAP`]
    /// cells of the blue sky.
    rim_sky: usize,
    /// Neighbouring cells outside the region, far from the sky, whose colour
    /// can't be sky. Sky-coloured cells far from the sky (a cloud's own
    /// textured folds) and the frame edge count for neither side.
    rim_other: usize,
}

impl Region {
    fn is_blue(&self) -> bool {
        self.size > 0 && self.blue * 10 >= self.size
    }

    /// A pale region that reaches the bottom of the frame is a wall or a
    /// backdrop: sky, clear or overcast, sits above the ground.
    fn clear_of_bottom(&self) -> bool {
        self.size > 0 && self.low as f32 <= 0.02 * self.size as f32
    }

    /// A cloud is mostly ringed by sky (or the frame edge); a sunlit wall
    /// touches the sky along one side only.
    fn is_cloud(&self) -> bool {
        self.clear_of_bottom() && self.rim_sky > 0 && self.rim_sky >= self.rim_other
    }
}

/// Label regions flooded from `seeds` (one label per still-unlabelled seed),
/// and count each one's cells. `near_sky`, when given, fills in the rim counts.
fn label_regions(
    grid: &Grid,
    like: &Likelihood,
    eligible: &[bool],
    labels: &mut [u32],
    seeds: impl Iterator<Item = usize>,
    near_sky: Option<&[bool]>,
) -> Vec<Region> {
    let first = labels.iter().copied().max().unwrap_or(0) + 1;
    let mut next = first;
    for s in seeds {
        if labels[s] == 0 {
            flood(grid, eligible, labels, next, vec![s]);
            next += 1;
        }
    }
    let mut regions = vec![Region::default(); (next - first) as usize];
    let low_row = grid.gh - grid.gh / 5;
    for (i, &l) in labels.iter().enumerate() {
        let Some(r) = l
            .checked_sub(first)
            .and_then(|k| regions.get_mut(k as usize))
        else {
            continue;
        };
        r.size += 1;
        r.blue += usize::from(like.blue[i] > 0.5);
        r.low += usize::from(i / grid.gw >= low_row);
        if let Some(near) = near_sky {
            for n in neighbours(i, grid.gw, grid.gh).filter(|&n| labels[n] != l) {
                r.rim_sky += usize::from(near[n]);
                r.rim_other += usize::from(!near[n] && like.tint[n] < 0.3);
            }
        }
    }
    regions
}

/// The sky's cells, or `None` when the photo has no sky.
fn grow_sky(grid: &Grid) -> Option<Vec<bool>> {
    let like = Likelihood::of(grid);
    let cells = grid.gw * grid.gh;
    let eligible: Vec<bool> = (0..cells)
        .map(|i| like.blue[i].max(like.pale[i]) >= 0.3)
        .collect();
    let band = ((grid.gh as f32 * SEED_BAND).ceil() as usize).clamp(1, grid.gh);

    // Label each region that touches the top band separately, so a white wall
    // at the top is judged apart from the sky beside it. Label 0 is not sky.
    let mut labels = vec![0u32; cells];
    let top_seeds = (0..band * grid.gw).filter(|&s| like.blue[s] > 0.5 || like.pale[s] > 0.6);
    let regions = label_regions(grid, &like, &eligible, &mut labels, top_seeds, None);

    let any_blue = regions.iter().any(Region::is_blue);
    let keep = |r: &Region| {
        if any_blue {
            r.is_blue()
        } else {
            r.clear_of_bottom()
        }
    };
    let mut sky: Vec<bool> = labels
        .iter()
        .map(|&l| {
            l.checked_sub(1)
                .and_then(|k| regions.get(k as usize))
                .is_some_and(keep)
        })
        .collect();

    if any_blue {
        second_chance(grid, &like, &eligible, &mut sky);
        add_clouds(grid, &like, &eligible, &mut sky);
    }
    close_gaps(&like, grid, &mut sky);
    fill_holes(&like, grid, &mut sky);
    reach_edges(&like, grid, &mut sky);

    let area = sky.iter().filter(|&&m| m).count();
    (area as f32 >= MIN_AREA * cells as f32).then_some(sky)
}

/// Re-seed from blue cells the first fill missed (sky between branches) that
/// match its mean blue. They may sit a little below the grown sky, under a
/// cloud bank, but not far below it, where blue is water or a reflection.
fn second_chance(grid: &Grid, like: &Likelihood, eligible: &[bool], sky: &mut [bool]) {
    let (mut sum, mut k, mut lowest) = ([0.0f32; 3], 0.0f32, 0usize);
    for (i, _) in sky.iter().enumerate().filter(|(_, &s)| s) {
        if like.blue[i] > 0.5 {
            for (s, c) in sum.iter_mut().zip(grid.rgb[i]) {
                *s += c;
            }
            k += 1.0;
        }
        lowest = i / grid.gw;
    }
    if k == 0.0 {
        return;
    }
    let mean = sum.map(|s| s / k);
    let mut labels: Vec<u32> = sky.iter().map(|&s| u32::from(s)).collect();
    let limit = (lowest + 1 + grid.gh / 6).min(grid.gh) * grid.gw;
    let seeds: Vec<usize> = (0..limit)
        .filter(|&i| !sky[i] && like.blue[i] > 0.6 && colour_dist(grid.rgb[i], mean) < 0.12)
        .collect();
    flood(grid, eligible, &mut labels, 1, seeds);
    for (s, l) in sky.iter_mut().zip(labels) {
        *s = l != 0;
    }
}

/// Add pale regions anywhere in the frame that the blue sky nearly surrounds.
fn add_clouds(grid: &Grid, like: &Likelihood, eligible: &[bool], sky: &mut [bool]) {
    let near_sky = dilate(sky, grid.gw, grid.gh, CLOUD_GAP);
    // Label 1 is the sky, so its cells never join a cloud.
    let mut labels: Vec<u32> = sky.iter().map(|&s| u32::from(s)).collect();
    let seeds = (0..sky.len()).filter(|&i| like.pale[i] > 0.6);
    let clouds = label_regions(grid, like, eligible, &mut labels, seeds, Some(&near_sky));
    for (s, l) in sky.iter_mut().zip(labels) {
        let cloud = l.checked_sub(2).and_then(|k| clouds.get(k as usize));
        *s |= cloud.is_some_and(Region::is_cloud);
    }
}

/// Fill sky-coloured cells in narrow gaps of the sky (a cloud's rough rim, a
/// thin wire), a morphological closing limited to cells whose colour could be
/// sky so tree lines and roof edges stay out.
fn close_gaps(like: &Likelihood, grid: &Grid, sky: &mut [bool]) {
    let grown = dilate(sky, grid.gw, grid.gh, CLOUD_GAP);
    let outside: Vec<bool> = grown.iter().map(|&g| !g).collect();
    let closed = dilate(&outside, grid.gw, grid.gh, CLOUD_GAP);
    for (i, s) in sky.iter_mut().enumerate() {
        *s |= !closed[i] && like.tint[i] > 0.3;
    }
}

/// Fill holes in the sky: sky-coloured pockets the sky encloses on every side,
/// such as a patch of blue framed by clouds. A dark hole (a bird, a plane)
/// stays out and the guided filter decides its edge.
fn fill_holes(like: &Likelihood, grid: &Grid, sky: &mut [bool]) {
    let (gw, gh) = (grid.gw, grid.gh);
    let mut seen: Vec<bool> = sky.to_vec();
    for start in 0..sky.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let (mut stack, mut hole) = (vec![start], Vec::new());
        let (mut enclosed, mut tint) = (true, 0.0f32);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % gw, i / gw);
            enclosed &= x > 0 && y > 0 && x + 1 < gw && y + 1 < gh;
            tint += like.tint[i];
            hole.push(i);
            for n in neighbours(i, gw, gh) {
                if !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if enclosed && tint >= 0.5 * hole.len() as f32 {
            for i in hole {
                sky[i] = true;
            }
        }
    }
}

/// Push the sky's border up to [`EDGE_REACH`] cells into sky-coloured cells
/// the smoothness test refused: sky seen past leaves and the rough underside
/// of a cloud bank. A dark line (a roof edge, a trunk) still stops it, and
/// the guided filter then decides each pixel.
fn reach_edges(like: &Likelihood, grid: &Grid, sky: &mut [bool]) {
    let mut frontier: Vec<usize> = (0..sky.len()).filter(|&i| sky[i]).collect();
    for _ in 0..EDGE_REACH {
        let mut next = Vec::new();
        for &i in &frontier {
            for n in neighbours(i, grid.gw, grid.gh) {
                if !sky[n]
                    && like.tint[n] >= 0.5
                    && colour_dist(grid.rgb[i], grid.rgb[n]) < EDGE_STEP
                {
                    sky[n] = true;
                    next.push(n);
                }
            }
        }
        frontier = next;
    }
}

/// Square dilation by `r` cells, separable with sliding counts.
fn dilate(mask: &[bool], gw: usize, gh: usize, r: usize) -> Vec<bool> {
    let mut rows = vec![false; mask.len()];
    for (src, dst) in mask.chunks_exact(gw).zip(rows.chunks_exact_mut(gw)) {
        let mut count = src[..r.min(gw)].iter().filter(|&&m| m).count();
        for x in 0..gw {
            if x + r < gw {
                count += usize::from(src[x + r]);
            }
            if x > r {
                count -= usize::from(src[x - r - 1]);
            }
            dst[x] = count > 0;
        }
    }
    let mut out = vec![false; mask.len()];
    for x in 0..gw {
        let col = |y: usize| usize::from(rows[y * gw + x]);
        let mut count: usize = (0..r.min(gh)).map(col).sum();
        for y in 0..gh {
            if y + r < gh {
                count += col(y + r);
            }
            if y > r {
                count -= col(y - r - 1);
            }
            out[y * gw + x] = count > 0;
        }
    }
    out
}

/// Bilinear upsample of the cell mask to `w x h`, sampling at cell centres.
fn upsample(member: &[bool], grid: &Grid, w: usize, h: usize) -> Vec<f32> {
    let cell = grid.cell as f32;
    let taps = |len: usize, cells: usize| -> Vec<(usize, usize, f32)> {
        (0..len)
            .map(|p| {
                let g = ((p as f32 + 0.5) / cell - 0.5).clamp(0.0, (cells - 1) as f32);
                let g0 = g as usize;
                (g0, (g0 + 1).min(cells - 1), g - g0 as f32)
            })
            .collect()
    };
    let xs = taps(w, grid.gw);
    let ys = taps(h, grid.gh);
    let at = |x: usize, y: usize| if member[y * grid.gw + x] { 1.0 } else { 0.0 };
    let mut out = Vec::with_capacity(w * h);
    for &(y0, y1, ty) in &ys {
        for &(x0, x1, tx) in &xs {
            let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
            let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
            out.push(top * (1.0 - ty) + bot * ty);
        }
    }
    out
}

/// Mean over a `(2r+1)^2` window, shrunk at the borders. Separable sliding sums.
fn box_mean(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut horiz = vec![0.0f32; w * h];
    for (row, out) in src.chunks_exact(w).zip(horiz.chunks_exact_mut(w)) {
        let mut sum: f32 = row[..r.min(w)].iter().sum();
        for x in 0..w {
            if x + r < w {
                sum += row[x + r];
            }
            if x > r {
                sum -= row[x - r - 1];
            }
            let count = (x + r).min(w - 1) + 1 - x.saturating_sub(r);
            out[x] = sum / count as f32;
        }
    }
    let mut out = vec![0.0f32; w * h];
    let mut sum = vec![0.0f32; w];
    for row in horiz.chunks_exact(w).take(r) {
        for (s, v) in sum.iter_mut().zip(row) {
            *s += v;
        }
    }
    for y in 0..h {
        if y + r < h {
            for (s, v) in sum.iter_mut().zip(&horiz[(y + r) * w..(y + r + 1) * w]) {
                *s += v;
            }
        }
        if y > r {
            for (s, v) in sum.iter_mut().zip(&horiz[(y - r - 1) * w..(y - r) * w]) {
                *s -= v;
            }
        }
        let count = ((y + r).min(h - 1) + 1 - y.saturating_sub(r)) as f32;
        for (o, s) in out[y * w..(y + 1) * w].iter_mut().zip(&sum) {
            *o = s / count;
        }
    }
    out
}

/// Gray-guide guided filter: `p` filtered with edges taken from `guide`.
fn guided_filter(guide: &[f32], p: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let gp: Vec<f32> = guide.iter().zip(p).map(|(g, p)| g * p).collect();
    let gg: Vec<f32> = guide.iter().map(|g| g * g).collect();
    let mean_g = box_mean(guide, w, h, r);
    let mean_p = box_mean(p, w, h, r);
    let corr_gp = box_mean(&gp, w, h, r);
    let corr_gg = box_mean(&gg, w, h, r);
    let mut a = Vec::with_capacity(w * h);
    let mut b = Vec::with_capacity(w * h);
    for i in 0..w * h {
        let var = corr_gg[i] - mean_g[i] * mean_g[i];
        let cov = corr_gp[i] - mean_g[i] * mean_p[i];
        let ai = cov / (var + GUIDED_EPS);
        a.push(ai);
        b.push(mean_p[i] - ai * mean_g[i]);
    }
    let mean_a = box_mean(&a, w, h, r);
    let mean_b = box_mean(&b, w, h, r);
    guide
        .iter()
        .zip(mean_a.iter().zip(&mean_b))
        .map(|(g, (a, b))| a * g + b)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic per-pixel noise in -1..1.
    fn noise(x: usize, y: usize) -> f32 {
        let mut v = (x as u32).wrapping_mul(0x9E37_79B9) ^ (y as u32).wrapping_mul(0x85EB_CA6B);
        v ^= v >> 15;
        v = v.wrapping_mul(0x2C1B_3C6D);
        v ^= v >> 12;
        (v & 0xFFFF) as f32 / 32768.0 - 1.0
    }

    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                for c in f(x, y) {
                    out.push((c.clamp(0.0, 1.0) * 255.0) as u8);
                }
                out.push(255);
            }
        }
        out
    }

    /// Foliage and earth: green and brown with strong per-pixel texture.
    fn ground(x: usize, y: usize) -> [f32; 3] {
        let n = noise(x, y) * 0.18;
        if (x / 7 + y / 5).is_multiple_of(2) {
            [0.20 + n, 0.38 + n, 0.12 + n]
        } else {
            [0.40 + n, 0.28 + n, 0.16 + n]
        }
    }

    fn row_mean(cov: &[u8], w: usize, y: usize) -> f32 {
        cov[y * w..(y + 1) * w]
            .iter()
            .map(|&c| c as f32)
            .sum::<f32>()
            / w as f32
    }

    const W: usize = 400;
    const H: usize = 300;

    fn blue_over_ground() -> Vec<u8> {
        image(W, H, |x, y| {
            if y < H / 2 {
                let t = y as f32 / H as f32;
                [0.30 + 0.4 * t, 0.50 + 0.3 * t, 0.85 + 0.1 * t]
            } else {
                ground(x, y)
            }
        })
    }

    #[test]
    fn blue_sky_over_textured_ground() {
        let cov = sky_coverage(&blue_over_ground(), W as u32, H as u32, None);
        assert_eq!(cov.len(), W * H);
        let horizon = H / 2;
        for y in 0..horizon - 12 {
            let min = cov[y * W..(y + 1) * W].iter().min().copied().unwrap_or(0);
            assert!(min > 200, "row {y} is sky but min coverage {min}");
        }
        for y in horizon + 12..H {
            let max = cov[y * W..(y + 1) * W].iter().max().copied().unwrap_or(255);
            assert!(max < 15, "row {y} is ground but max coverage {max}");
        }
        // The horizon is feathered, not a hard step.
        let soft = (horizon - 12..horizon + 12)
            .map(|y| row_mean(&cov, W, y))
            .filter(|&m| m > 10.0 && m < 245.0)
            .count();
        assert!(soft >= 1, "no partial coverage at the horizon");
    }

    #[test]
    fn tree_line_coverage_follows_the_sky_between_leaves() {
        let sky = [0.35, 0.55, 0.88];
        let leaf = [0.12, 0.25, 0.08];
        let fringe = H / 2..H / 2 + 12;
        let is_gap = |x: usize, y: usize| noise(x, y) > 0.0;
        let img = image(W, H, |x, y| {
            if y < fringe.start || (fringe.contains(&y) && is_gap(x, y)) {
                sky
            } else if fringe.contains(&y) {
                leaf
            } else {
                ground(x, y)
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        let (mut gaps, mut leaves) = ((0.0, 0.0), (0.0, 0.0));
        for y in fringe.start..fringe.start + 6 {
            for x in 0..W {
                let side = if is_gap(x, y) { &mut gaps } else { &mut leaves };
                side.0 += cov[y * W + x] as f32;
                side.1 += 1.0;
            }
        }
        let (gap_mean, leaf_mean) = (gaps.0 / gaps.1, leaves.0 / leaves.1);
        assert!(
            gap_mean > leaf_mean + 20.0,
            "sky between leaves {gap_mean:.0}, leaves {leaf_mean:.0}"
        );
    }

    #[test]
    fn grey_overcast_with_white_clouds_counts() {
        let img = image(W, H, |x, y| {
            if y < H / 2 {
                let (dx, dy) = (x as f32 - 120.0, y as f32 - 60.0);
                let (ex, ey) = (x as f32 - 290.0, y as f32 - 90.0);
                let cloud = (1.0 - (dx * dx + dy * dy).sqrt() / 60.0)
                    .max(1.0 - (ex * ex + ey * ey).sqrt() / 45.0)
                    .clamp(0.0, 1.0);
                let v = 0.72 + 0.24 * cloud;
                [v, v, v + 0.01]
            } else {
                ground(x, y)
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        assert!(row_mean(&cov, W, 20) > 230.0);
        assert!(row_mean(&cov, W, 60) > 230.0, "the cloud row is sky");
        assert!(row_mean(&cov, W, H - 20) < 5.0);
    }

    #[test]
    fn rough_edged_cloud_at_the_top_counts() {
        let img = image(W, H, |x, y| {
            if y >= H / 2 {
                return ground(x, y);
            }
            // Touching the top edge, so only the cloud pass can claim it.
            let (dx, dy) = (x as f32 - 200.0, y as f32 - 30.0);
            // A ragged rim: the radius wobbles per pixel, so the cloud's edge
            // cells are textured and fail the smoothness test.
            let radius = 50.0 + 6.0 * noise(x, y);
            if (dx * dx + dy * dy).sqrt() < radius {
                [0.93, 0.94, 0.95]
            } else {
                [0.35, 0.55, 0.88]
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        assert!(
            cov[30 * W + 200] > 200,
            "cloud centre {}",
            cov[30 * W + 200]
        );
        assert!(cov[30 * W + 152] > 128, "cloud rim {}", cov[30 * W + 152]);
        assert!(cov[(H - 20) * W + 200] < 5);
    }

    #[test]
    fn shaded_wall_beside_blue_sky_is_not_sky() {
        // The sky tints a shaded white wall a faint blue. It runs from the top
        // of the frame to the bottom, beside blue sky over foliage.
        let img = image(W, H, |x, y| {
            if x < 150 {
                [0.49, 0.50, 0.55]
            } else if y < H / 2 {
                [0.35, 0.55, 0.88]
            } else {
                ground(x, y)
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        assert!(cov[40 * W + 300] > 200, "the sky is still found");
        for y in 0..H {
            for x in 0..130 {
                assert_eq!(cov[y * W + x], 0, "wall pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn indoor_room_has_no_sky() {
        let img = image(W, H, |x, y| {
            let n = noise(x, y) * 0.05;
            if y < H / 3 {
                [0.42 + n, 0.38 + n, 0.33 + n]
            } else {
                ground(x, y)
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        assert!(cov.iter().all(|&c| c == 0));
    }

    #[test]
    fn plain_white_wall_behind_a_portrait_is_not_sky() {
        let img = image(W, H, |x, y| {
            let (dx, dy) = (x as f32 - 200.0, y as f32 - 170.0);
            let head = dx * dx / (90.0 * 90.0) + dy * dy / (110.0 * 110.0) < 1.0;
            let body = y > 230 && (60..340).contains(&x);
            if head || body {
                ground(x, y)
            } else {
                [0.90, 0.89, 0.87]
            }
        });
        let cov = sky_coverage(&img, W as u32, H as u32, None);
        assert!(cov.iter().all(|&c| c == 0));
    }

    #[test]
    fn subject_is_never_sky() {
        let mut subject = vec![0u8; W * H];
        for y in 30..90 {
            for x in 150..250 {
                subject[y * W + x] = 255;
            }
        }
        let cov = sky_coverage(&blue_over_ground(), W as u32, H as u32, Some(&subject));
        for y in 30..90 {
            for x in 150..250 {
                assert_eq!(cov[y * W + x], 0, "subject pixel ({x}, {y})");
            }
        }
        assert!(cov[40 * W + 50] > 200, "sky beside the subject stays sky");
    }

    #[test]
    fn hostile_inputs_return_zeros_of_the_right_length() {
        let cov = sky_coverage(&[], u32::MAX, u32::MAX, None);
        assert_eq!(cov.len(), MAX_PIXELS);
        assert!(cov.iter().all(|&c| c == 0));

        assert_eq!(sky_coverage(&[0; 10], 4, 4, None), vec![0; 16]);
        // Valid but past the preview size: no sky, still one byte per pixel.
        let big = vec![0u8; 4097 * 4097 * 4];
        assert_eq!(sky_coverage(&big, 4097, 4097, None).len(), 4097 * 4097);
        assert!(sky_coverage(&[], 0, 0, None).is_empty());
        assert_eq!(sky_coverage(&[], 0, 7, None), Vec::<u8>::new());

        let img = blue_over_ground();
        let cov = sky_coverage(&img, W as u32, H as u32, Some(&[255; 3]));
        assert_eq!(cov, vec![0; W * H]);
    }

    #[test]
    fn tiny_images_do_not_panic() {
        for (w, h) in [(1, 1), (1, 9), (9, 1), (2, 3)] {
            let img = image(w, h, |_, _| [0.4, 0.6, 0.95]);
            assert_eq!(sky_coverage(&img, w as u32, h as u32, None).len(), w * h);
        }
    }
}
