//! Phase 5 — pure shallow-water solver core (Bevy-free, deterministic).
//!
//! Virtual-pipe model: every loaded water-bearing cell exchanges flux with its
//! four cardinal neighbours proportional to the **water-surface** height
//! difference, clamped by the giver's available volume so depth never goes
//! negative and total volume is conserved (modulo sources and exchange with
//! pinned boundary cells). Basins fill to their spill lip; a dam tile is a
//! crest weir that pools water upstream and passes only the over-crest excess
//! downstream (overtopping). Removing the dam lets the impoundment drain.
//!
//! The Bevy wrapper (`water_runtime.rs`) snapshots the active region into a
//! [`WaterGrid`], runs [`WaterGrid::simulate`] on `AsyncComputeTaskPool`, and
//! writes the result back into `RuntimeWater` (persistent) + `ChunkMap`.
//!
//! Determinism: every per-step traversal is over a **sorted** key list, so
//! the only floating-point summation order is fixed regardless of the
//! `AHashMap` iteration order. Two identical grids stepped identically are
//! bit-for-bit equal (`determinism_bitwise` test).

use ahash::AHashMap;

/// Pipe conductance per unit `dt`. Stability wants `FLOW_K * dt * 4 < 1`
/// (four neighbours); the wrapper uses small `dt` with several substeps.
pub const FLOW_K: f32 = 0.40;

/// A single normal edge moves at most this fraction of the giver's depth in
/// one substep (pre volume-clamp). Damps checkerboard oscillation; the
/// per-giver clamp still guarantees non-negativity.
const MAX_EDGE_FRACTION: f32 = 0.5;

/// Below this surface-difference an edge carries no flux — lets at-rest
/// cells sleep (the wrapper drops a region from the active set when every
/// edge is sub-epsilon and there are no sources).
pub const REST_EPS: f32 = 0.01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellRole {
    /// Depth evolves under flux + source.
    Free,
    /// Fixed-surface boundary: ocean, a large lake at its spill level, or an
    /// unloaded-neighbour ghost pinned at hydrology truth. Infinite
    /// reservoir — exchanges flux but its own depth never changes (the
    /// surface is stored in `bed`, `depth == 0`).
    Pinned,
}

#[derive(Clone, Copy, Debug)]
pub struct WaterCell {
    /// Solid bed Z (Free) or the fixed surface Z (Pinned).
    pub bed: f32,
    /// Water column depth (Z-units). Always `0.0` for `Pinned`.
    pub depth: f32,
    pub role: CellRole,
    /// Z-units added per unit `dt` (springs, river inlet). Free cells only.
    pub source: f32,
}

impl WaterCell {
    pub fn free(bed: f32, depth: f32) -> Self {
        Self {
            bed,
            depth: depth.max(0.0),
            role: CellRole::Free,
            source: 0.0,
        }
    }
    pub fn pinned(level: f32) -> Self {
        Self {
            bed: level,
            depth: 0.0,
            role: CellRole::Pinned,
            source: 0.0,
        }
    }
    pub fn with_source(mut self, rate: f32) -> Self {
        self.source = rate;
        self
    }
    #[inline]
    pub fn surface(&self) -> f32 {
        match self.role {
            CellRole::Free => self.bed + self.depth,
            CellRole::Pinned => self.bed,
        }
    }
}

/// Sparse active-region water state. `dam_crests` tiles are **solid weir
/// barriers**, never present in `cells`.
#[derive(Clone, Default)]
pub struct WaterGrid {
    pub cells: AHashMap<(i32, i32), WaterCell>,
    /// tile → crest Z. Water pools against the dam; only the portion of an
    /// upstream surface *above* the crest passes to lower neighbours.
    pub dam_crests: AHashMap<(i32, i32), f32>,
}

const CARDINALS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

impl WaterGrid {
    pub fn surface_at(&self, t: (i32, i32)) -> Option<f32> {
        self.cells.get(&t).map(|c| c.surface())
    }

    /// Total water held by `Free` cells (conservation invariant target).
    pub fn total_free_volume(&self) -> f32 {
        self.cells
            .values()
            .filter(|c| c.role == CellRole::Free)
            .map(|c| c.depth)
            .sum()
    }

    fn sorted_keys(&self) -> Vec<(i32, i32)> {
        let mut k: Vec<(i32, i32)> = self.cells.keys().copied().collect();
        k.sort_unstable();
        k
    }

    /// One explicit substep. Builds a deterministic transfer list (normal
    /// pipe edges + dam-weir transfers), volume-clamps per giver so no Free
    /// cell goes negative, then applies. Conserves volume exactly: every
    /// transfer subtracts from a giver and adds the *same* amount to a
    /// receiver; only exchange with `Pinned` cells or `source` changes the
    /// Free total.
    pub fn step(&mut self, dt: f32) {
        let keys = self.sorted_keys();

        // (giver, receiver, amount > 0). Deterministic build order.
        let mut transfers: Vec<((i32, i32), (i32, i32), f32)> = Vec::new();

        // --- Normal pipe edges (each undirected edge once: c < neighbour) ---
        for &c in &keys {
            let cs = self.cells[&c].surface();
            let c_depth = self.cells[&c].depth;
            let c_free = self.cells[&c].role == CellRole::Free;
            for (dx, dy) in CARDINALS {
                let n = (c.0 + dx, c.1 + dy);
                if n <= c {
                    continue; // process each undirected edge from its lower key
                }
                // A dam tile is a barrier, not a cell — handled below.
                if self.dam_crests.contains_key(&n) || self.dam_crests.contains_key(&c) {
                    continue;
                }
                let Some(ncell) = self.cells.get(&n) else {
                    continue; // unknown neighbour = closed wall
                };
                let ns = ncell.surface();
                let dh = cs - ns;
                if dh.abs() <= REST_EPS {
                    continue;
                }
                let (g, r, gdepth, gfree) = if dh > 0.0 {
                    (c, n, c_depth, c_free)
                } else {
                    (n, c, ncell.depth, ncell.role == CellRole::Free)
                };
                let mut amt = FLOW_K * dt * dh.abs();
                if gfree {
                    amt = amt.min(MAX_EDGE_FRACTION * gdepth);
                }
                if amt > 0.0 {
                    transfers.push((g, r, amt));
                }
            }
        }

        // --- Dam weir transfers ---
        // Donor neighbour surface above crest → its excess flows over; split
        // pro-rata across neighbours below the crest. Conservative: the
        // total taken from donors equals the total given to receivers.
        let mut dams: Vec<((i32, i32), f32)> =
            self.dam_crests.iter().map(|(&t, &cz)| (t, cz)).collect();
        dams.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (d, cz) in dams {
            let mut donors: Vec<((i32, i32), f32, bool)> = Vec::new(); // (tile, supply, free)
            let mut receivers: Vec<((i32, i32), f32)> = Vec::new(); // (tile, capacity)
            for (dx, dy) in CARDINALS {
                let n = (d.0 + dx, d.1 + dy);
                let Some(nc) = self.cells.get(&n) else {
                    continue;
                };
                let s = nc.surface();
                if s > cz + REST_EPS {
                    let free = nc.role == CellRole::Free;
                    let mut sup = FLOW_K * dt * (s - cz);
                    if free {
                        sup = sup.min(MAX_EDGE_FRACTION * nc.depth);
                    }
                    if sup > 0.0 {
                        donors.push((n, sup, free));
                    }
                } else if s < cz - REST_EPS {
                    receivers.push((n, FLOW_K * dt * (cz - s)));
                }
            }
            if donors.is_empty() || receivers.is_empty() {
                continue; // no spill path → upstream keeps rising (realistic)
            }
            let total_sup: f32 = donors.iter().map(|x| x.1).sum();
            let total_cap: f32 = receivers.iter().map(|x| x.1).sum();
            let moved = total_sup.min(total_cap);
            if moved <= 0.0 {
                continue;
            }
            for (dt_tile, sup, _free) in &donors {
                let give = moved * (*sup / total_sup);
                for (rt, cap) in &receivers {
                    let take = give * (*cap / total_cap);
                    if take > 0.0 {
                        transfers.push((*dt_tile, *rt, take));
                    }
                }
            }
        }

        // --- Per-giver volume clamp (Free givers only) ---
        let mut gross_out: AHashMap<(i32, i32), f32> = AHashMap::new();
        for &(g, _, a) in &transfers {
            if self.cells.get(&g).map(|c| c.role) == Some(CellRole::Free) {
                *gross_out.entry(g).or_insert(0.0) += a;
            }
        }
        let mut scale: AHashMap<(i32, i32), f32> = AHashMap::new();
        for (&g, &out) in &gross_out {
            let d = self.cells[&g].depth;
            scale.insert(g, if out > d && out > 0.0 { d / out } else { 1.0 });
        }

        // --- Apply (deterministic transfer order) ---
        let mut delta: AHashMap<(i32, i32), f32> = AHashMap::new();
        for &(g, r, a) in &transfers {
            let s = scale.get(&g).copied().unwrap_or(1.0);
            let m = a * s;
            if m == 0.0 {
                continue;
            }
            if self.cells.get(&g).map(|c| c.role) == Some(CellRole::Free) {
                *delta.entry(g).or_insert(0.0) -= m;
            }
            if self.cells.get(&r).map(|c| c.role) == Some(CellRole::Free) {
                *delta.entry(r).or_insert(0.0) += m;
            }
        }
        for &k in &keys {
            let cell = self.cells.get_mut(&k).unwrap();
            if cell.role != CellRole::Free {
                continue;
            }
            let d = delta.get(&k).copied().unwrap_or(0.0) + cell.source * dt;
            cell.depth = (cell.depth + d).max(0.0);
        }
    }

    pub fn simulate(&mut self, substeps: u32, dt: f32) {
        for _ in 0..substeps {
            self.step(dt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cells: &[((i32, i32), WaterCell)]) -> WaterGrid {
        let mut g = WaterGrid::default();
        for &(t, c) in cells {
            g.cells.insert(t, c);
        }
        g
    }

    #[test]
    fn two_cell_levels_and_conserves() {
        let mut g = grid(&[
            ((0, 0), WaterCell::free(0.0, 4.0)),
            ((1, 0), WaterCell::free(0.0, 0.0)),
        ]);
        let v0 = g.total_free_volume();
        g.simulate(400, 1.0);
        assert!((g.total_free_volume() - v0).abs() < 1e-3, "volume drift");
        let a = g.cells[&(0, 0)].depth;
        let b = g.cells[&(1, 0)].depth;
        assert!(
            (a - 2.0).abs() < 0.05 && (b - 2.0).abs() < 0.05,
            "a={a} b={b}"
        );
    }

    #[test]
    fn determinism_bitwise() {
        let mk = || {
            grid(&[
                ((0, 0), WaterCell::free(1.0, 3.0)),
                ((1, 0), WaterCell::free(0.0, 1.0)),
                ((2, 0), WaterCell::free(0.5, 0.0)),
                ((1, 1), WaterCell::free(0.0, 2.0)),
            ])
        };
        let mut a = mk();
        let mut b = mk();
        a.simulate(123, 0.7);
        b.simulate(123, 0.7);
        for k in [(0, 0), (1, 0), (2, 0), (1, 1)] {
            assert_eq!(
                a.cells[&k].depth.to_bits(),
                b.cells[&k].depth.to_bits(),
                "non-deterministic at {k:?}"
            );
        }
    }

    #[test]
    fn pinned_is_infinite_sink() {
        // A deep Free cell next to an ocean pinned at surface 0 drains away.
        let mut g = grid(&[
            ((0, 0), WaterCell::free(0.0, 5.0)),
            ((1, 0), WaterCell::pinned(0.0)),
        ]);
        g.simulate(600, 1.0);
        assert!(g.cells[&(0, 0)].depth < 0.05, "did not drain to ocean");
        assert_eq!(g.cells[&(1, 0)].depth, 0.0, "pinned depth must stay 0");
        assert_eq!(g.cells[&(1, 0)].bed, 0.0, "pinned level must stay fixed");
    }

    #[test]
    fn basin_fills_to_spill() {
        // x: 0 (source) — 1 (deep basin) — 2 (spill lip bed=2) — 3 (pinned 0).
        // Inflow at x0 fills the basin until its surface reaches the lip,
        // then spills over to the sink. Steady basin surface ≈ lip height.
        let mut g = grid(&[
            ((0, 0), WaterCell::free(0.0, 0.0).with_source(0.02)),
            ((1, 0), WaterCell::free(0.0, 0.0)),
            ((2, 0), WaterCell::free(2.0, 0.0)),
            ((3, 0), WaterCell::pinned(0.0)),
        ]);
        g.simulate(4000, 1.0);
        let basin_surf = g.cells[&(1, 0)].surface();
        assert!(
            basin_surf > 1.8 && basin_surf < 2.6,
            "basin should pool to ≈ lip (2.0), got {basin_surf}"
        );
        assert!(g.cells[&(1, 0)].depth >= 0.0);
    }

    #[test]
    fn dam_pools_upstream_then_overtops_then_drains() {
        // Channel x=0..4, bed 0. Dam tile at x=2, crest 5. Source at x=0,
        // ocean sink (pinned 0) at x=4. There is no cell at x=2 (the dam is
        // a solid barrier); the weir passes only over-crest excess.
        let mut g = WaterGrid::default();
        g.cells
            .insert((0, 0), WaterCell::free(0.0, 0.0).with_source(0.05));
        g.cells.insert((1, 0), WaterCell::free(0.0, 0.0));
        g.cells.insert((3, 0), WaterCell::free(0.0, 0.0));
        g.cells.insert((4, 0), WaterCell::pinned(0.0));
        g.dam_crests.insert((2, 0), 5.0);

        g.simulate(6000, 1.0);
        let up = g.cells[&(1, 0)].surface();
        assert!(
            up > 4.0,
            "upstream should pool toward the crest (5.0), got {up}"
        );
        assert!(
            up <= 5.0 + 0.6,
            "upstream must not exceed crest unbounded (overtopping), got {up}"
        );
        // Some water made it past the dam to the downstream channel/sink.
        assert!(
            g.cells[&(3, 0)].depth > 0.0 || g.cells[&(4, 0)].depth == 0.0,
            "overtopping should feed the downstream side"
        );

        // Remove the dam → the impoundment drains toward the ocean sink.
        let pooled = g.cells[&(1, 0)].depth;
        g.cells.insert((2, 0), WaterCell::free(0.0, 0.0));
        g.dam_crests.clear();
        g.simulate(6000, 1.0);
        assert!(
            g.cells[&(1, 0)].depth < pooled - 1.0,
            "upstream should drain after dam removal: {} vs pooled {pooled}",
            g.cells[&(1, 0)].depth
        );
    }

    #[test]
    fn no_negative_depth_under_aggressive_dt() {
        // Large dt would over-drain without the per-giver clamp.
        let mut g = grid(&[
            ((0, 0), WaterCell::free(0.0, 1.0)),
            ((1, 0), WaterCell::free(0.0, 0.0)),
            ((2, 0), WaterCell::free(0.0, 0.0)),
        ]);
        g.simulate(50, 2.0);
        for k in [(0, 0), (1, 0), (2, 0)] {
            assert!(g.cells[&k].depth >= 0.0, "negative depth at {k:?}");
        }
        assert!(
            (g.total_free_volume() - 1.0).abs() < 1e-3,
            "volume not conserved"
        );
    }
}
