//! Continental plate tectonics on the climate-cell grid.
//!
//! - Lloyd-relaxed Voronoi assigns each cell to one of `NUM_PLATES` plates.
//! - Each plate carries a (vx, vy) motion vector.
//! - Boundaries are classified as convergent / divergent / transform from the
//!   relative velocity of the two adjacent plates along the boundary normal.
//! - Convergent edges add uplift to a heightmap (mountain ranges); divergent
//!   edges subsidence (rifts / mid-ocean ridges); transform edges nothing.
//!
//! X wraps (cylinder); Y clamps (poles).

use super::globe::{GLOBE_HEIGHT, GLOBE_WIDTH};
use rand::{rngs::StdRng, Rng, SeedableRng};

pub const NUM_PLATES: usize = 8;
const LLOYD_ITERS: u32 = 4;

#[derive(Clone, Copy, Debug)]
pub struct Plate {
    /// Plate centroid in cell-space.
    pub center: (f32, f32),
    /// Motion vector (cells per "geological tick" — magnitude ~1).
    pub velocity: (f32, f32),
}

pub struct PlateField {
    /// Plate id per cell (row-major, GLOBE_WIDTH × GLOBE_HEIGHT).
    pub plate_id: Vec<u8>,
    pub plates: Vec<Plate>,
}

impl PlateField {
    pub fn at(&self, gx: i32, gy: i32) -> u8 {
        let gx = gx.rem_euclid(GLOBE_WIDTH);
        let gy = gy.clamp(0, GLOBE_HEIGHT - 1);
        self.plate_id[(gy * GLOBE_WIDTH + gx) as usize]
    }
}

/// Wrapped horizontal distance on the cylinder. Vertical clamps (no wrap).
fn wrap_dx(dx: f32) -> f32 {
    let w = GLOBE_WIDTH as f32;
    if dx > w * 0.5 {
        dx - w
    } else if dx < -w * 0.5 {
        dx + w
    } else {
        dx
    }
}

fn dist_sq(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = wrap_dx(a.0 - b.0);
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

fn assign_nearest(plates: &[Plate]) -> Vec<u8> {
    let mut out = vec![0u8; (GLOBE_WIDTH * GLOBE_HEIGHT) as usize];
    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let p = (gx as f32 + 0.5, gy as f32 + 0.5);
            let mut best = 0;
            let mut best_d = f32::INFINITY;
            for (i, plate) in plates.iter().enumerate() {
                let d = dist_sq(p, plate.center);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            out[(gy * GLOBE_WIDTH + gx) as usize] = best as u8;
        }
    }
    out
}

fn lloyd_relax(plates: &mut [Plate], assignment: &[u8]) {
    let mut sums: Vec<(f32, f32, u32)> = vec![(0.0, 0.0, 0); plates.len()];
    // For X-wrap-correct centroids, accumulate against the plate's current
    // center (using wrap-corrected dx) and translate at the end.
    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let pid = assignment[(gy * GLOBE_WIDTH + gx) as usize] as usize;
            let cx = (gx as f32 + 0.5) - plates[pid].center.0;
            let cx = wrap_dx(cx);
            let cy = (gy as f32 + 0.5) - plates[pid].center.1;
            sums[pid].0 += cx;
            sums[pid].1 += cy;
            sums[pid].2 += 1;
        }
    }
    for (i, plate) in plates.iter_mut().enumerate() {
        if sums[i].2 == 0 {
            continue;
        }
        let n = sums[i].2 as f32;
        let mut nx = plate.center.0 + sums[i].0 / n;
        let ny = plate.center.1 + sums[i].1 / n;
        nx = nx.rem_euclid(GLOBE_WIDTH as f32);
        let ny = ny.clamp(0.0, GLOBE_HEIGHT as f32);
        plate.center = (nx, ny);
    }
}

pub fn generate(seed: u64) -> PlateField {
    let mut rng = StdRng::seed_from_u64(seed);

    // Seed plate centers and velocities.
    let mut plates: Vec<Plate> = (0..NUM_PLATES)
        .map(|_| {
            let cx = rng.gen_range(0.0..GLOBE_WIDTH as f32);
            let cy = rng.gen_range(0.0..GLOBE_HEIGHT as f32);
            let angle = rng.gen_range(0.0..std::f32::consts::TAU);
            let speed = rng.gen_range(0.5..1.5);
            Plate {
                center: (cx, cy),
                velocity: (angle.cos() * speed, angle.sin() * speed),
            }
        })
        .collect();

    // Lloyd-relax the plate centers so they're roughly equally-spaced.
    let mut assignment = assign_nearest(&plates);
    for _ in 0..LLOYD_ITERS {
        lloyd_relax(&mut plates, &assignment);
        assignment = assign_nearest(&plates);
    }

    PlateField {
        plate_id: assignment,
        plates,
    }
}

/// Build the tectonic uplift field: convergent boundaries push elevation up,
/// divergent push down, transform unchanged. The result is a smoothed
/// per-cell delta to add into the elevation field.
///
/// Output is in normalised units (~ -0.5 .. +0.5) before mixing with noise.
pub fn uplift_field(field: &PlateField) -> Vec<f32> {
    let w = GLOBE_WIDTH as usize;
    let h = GLOBE_HEIGHT as usize;
    let mut raw = vec![0.0f32; w * h];

    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let pid_self = field.at(gx, gy) as usize;
            // Inspect 4-neighbours. If any has a different plate_id, classify
            // the boundary. Use the mean across all differing neighbours so a
            // cell straddling a triple-junction averages cleanly.
            let mut total = 0.0f32;
            let mut count = 0u32;
            let neighbours = [
                (gx - 1, gy),
                (gx + 1, gy),
                (gx, gy - 1),
                (gx, gy + 1),
            ];
            for (nx, ny) in neighbours {
                if ny < 0 || ny >= GLOBE_HEIGHT {
                    continue;
                }
                let pid_n = field.at(nx, ny) as usize;
                if pid_n == pid_self {
                    continue;
                }
                // Boundary normal: pointing from self → neighbour.
                let dx = wrap_dx((nx as f32 + 0.5) - (gx as f32 + 0.5));
                let dy = (ny as f32 + 0.5) - (gy as f32 + 0.5);
                let len = (dx * dx + dy * dy).sqrt().max(1e-6);
                let nx_u = dx / len;
                let ny_u = dy / len;
                // Relative velocity (neighbour - self): if it points back
                // toward self (negative along normal) → convergent → uplift.
                let v_self = field.plates[pid_self].velocity;
                let v_n = field.plates[pid_n].velocity;
                let rvx = v_n.0 - v_self.0;
                let rvy = v_n.1 - v_self.1;
                let rel = rvx * nx_u + rvy * ny_u;
                // Convergent (rel<0) → up, divergent (rel>0) → down.
                total += -rel;
                count += 1;
            }
            if count > 0 {
                raw[(gy * w as i32 + gx) as usize] = total / count as f32;
            }
        }
    }

    // Smooth with a separable 3x3 box filter, applied twice, to widen the
    // mountain ranges from a 1-cell ridge into a band.
    let mut buf = raw.clone();
    for _ in 0..2 {
        // Horizontal pass (X wraps).
        for gy in 0..h {
            for gx in 0..w {
                let xm = (gx + w - 1) % w;
                let xp = (gx + 1) % w;
                buf[gy * w + gx] =
                    (raw[gy * w + xm] + raw[gy * w + gx] + raw[gy * w + xp]) / 3.0;
            }
        }
        std::mem::swap(&mut raw, &mut buf);
        // Vertical pass (Y clamps).
        for gy in 0..h {
            let ym = gy.saturating_sub(1);
            let yp = (gy + 1).min(h - 1);
            for gx in 0..w {
                buf[gy * w + gx] = (raw[ym * w + gx] + raw[gy * w + gx] + raw[yp * w + gx]) / 3.0;
            }
        }
        std::mem::swap(&mut raw, &mut buf);
    }

    // Normalise to roughly [-0.5, 0.5].
    let mut max_abs = 1e-6f32;
    for &v in &raw {
        max_abs = max_abs.max(v.abs());
    }
    for v in raw.iter_mut() {
        *v = (*v / max_abs) * 0.5;
    }
    raw
}
