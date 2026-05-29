//! Seed-time tile reservations for chunk-streaming-safe stamps.
//!
//! `seed_starting_buildings_system` and friends stamp structures, doormats,
//! and queue road carves in `OnEnter(Playing)`. Plants only spawn inside
//! chunks the camera has streamed in, which may happen well after the seed
//! pass. Without this resource, a plant streaming onto a planned doormat or
//! road tile that doesn't yet carry a `StructureLabel` would slip past
//! `react_obstacle_under_structure_system`.
//!
//! `SeedReservation` is the persistent, additive truth-set of every tile the
//! bootstrap pipeline has stamped as "must remain clear of obstacles" —
//! structures, blueprints, doormats, planned/queued road tiles. Three
//! consumers:
//!
//! - `plants::seed_target_tile_ok` — wild-seed scatter rejects reserved tiles.
//! - `world::chunk_streaming::spawn_chunk_plants` — terrain-driven plant
//!   spawn skips reserved tiles.
//! - `clear_obstacle::react_obstacle_under_structure_system` — late-streamed
//!   obstacles on reserved tiles are despawned/relocated synchronously.
//!
//! Wild-plant exclusion on agricultural plot tiles is **not** routed through
//! `SeedReservation`. Field tiles legitimately host cultivated plants that
//! workers sow there, so they must not be treated as obstacles. The wild-spawn
//! paths consult `PlotIndex.ag_tiles` directly instead; the carve-time cleanup
//! in `land::carve_plots_system` handles pre-existing wild plants.
//!
//! Entries are never removed during a game session — settlements decay and
//! restamp differently, but the bootstrap "this tile is part of a stamped
//! settlement" claim persists. Sized ~256 tiles × ~20 settlements ≈ 5k.

use ahash::AHashSet;
use bevy::prelude::*;

use crate::simulation::construction::{RoadCarveQueue, StructureIndex, WellMap};
use crate::simulation::doormat::DoormatReservations;
use crate::simulation::organic_settlement::SettlementBrains;

#[derive(Resource, Default)]
pub struct SeedReservation(pub AHashSet<(i32, i32)>);

impl SeedReservation {
    pub fn is_reserved(&self, tile: (i32, i32)) -> bool {
        self.0.contains(&tile)
    }

    pub fn reserve(&mut self, tile: (i32, i32)) {
        self.0.insert(tile);
    }

    pub fn reserve_iter<I: IntoIterator<Item = (i32, i32)>>(&mut self, tiles: I) {
        self.0.extend(tiles);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Rasterise a Bresenham line from `from` to `to` (endpoints inclusive) into
/// the reservation set. Matches the **2-tile-wide** carve in
/// `road_carve_system` so queued-but-uncarved roads are reserved across
/// their full footprint, not just the centreline. The widen side routes around
/// `is_blocked` tiles per cell via the shared `road_widen_tile` rule, so the
/// reservation stays lock-step with the carver's structure avoidance. Pass
/// `|_| false` for the unconditional baseline.
pub fn rasterize_line_into(
    reservation: &mut SeedReservation,
    from: (i32, i32),
    to: (i32, i32),
    is_blocked: impl Fn((i32, i32)) -> bool,
) {
    use crate::simulation::organic_settlement::road_widen_tile;
    let mut x0 = from.0;
    let mut y0 = from.1;
    let x1 = to.0;
    let y1 = to.1;
    let dx_abs = (x1 - x0).abs();
    let dy_abs = (y1 - y0).abs();
    let dx = dx_abs;
    let dy = -dy_abs;
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        reservation.reserve((x0, y0));
        reservation.reserve(road_widen_tile((x0, y0), from, to, &is_blocked));
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// One-shot `OnEnter(Playing)` system that consolidates every tile the seed
/// pipeline stamped or queued into `SeedReservation`. Sources:
///
/// 1. `StructureIndex` — every finalized seeded structure (Wall/Door/Bed/
///    Hearth/Workbench/Granary/Shrine/Market/Barracks/Monument/Well/...).
///    The `StructureLabel` add-hook keeps this in sync, so any seeded
///    structure that bypasses the blueprint pipeline is still indexed.
/// 2. `DoormatReservations` — every doormat tile, even ones not yet flipped
///    to `TileKind::Road` by the road-carve drain.
/// 3. `RoadCarveQueue` — Bresenham rasterisation of every queued road
///    segment, so plants can't sprout on a planned road tile before
///    `road_carve_system` reaches it.
/// 4. `SettlementBrains::road_tiles` — the planned-road tile set per
///    settlement (kept current by the runtime survey and `kickoff_initial_
///    survey_system`).
///
/// Agricultural plot tiles are intentionally NOT folded in here — wild-plant
/// exclusion on fields lives on `PlotIndex.ag_tiles` directly (see
/// `plants::seed_target_tile_ok` / `chunk_streaming::spawn_chunk_plants`), and
/// `land::carve_plots_system` handles pre-existing wild plants at carve time.
/// Reserving field tiles here would cause `react_obstacle_under_structure_system`
/// to despawn cultivated seedlings the moment a worker plants them.
///
/// Runs after `clear_obstacles_under_seeded_structures` and
/// `seed_starting_farms_system` so all four inputs are populated.
pub fn populate_seed_reservation_system(
    structure_index: Res<StructureIndex>,
    doormat: Res<DoormatReservations>,
    road_queue: Res<RoadCarveQueue>,
    brains: Res<SettlementBrains>,
    well_map: Res<WellMap>,
    mut reservation: ResMut<SeedReservation>,
) {
    reservation.reserve_iter(structure_index.0.keys().copied());
    reservation.reserve_iter(doormat.0.keys().copied());
    for brain in brains.0.values() {
        // Reserve the full widened corridor, not just the centreline — the
        // carver stamps a 2-tile-wide road and the corridor cache already
        // routes around standing structures.
        reservation.reserve_iter(brain.road_corridor_tiles.iter().copied());
    }
    for &(_faction_id, from, to) in road_queue.0.iter() {
        rasterize_line_into(&mut reservation, from, to, |t| {
            structure_index.0.contains_key(&t)
        });
    }
    // Every well owns a 5×5 stepwell footprint, but only the centre tile sits
    // in `StructureIndex` (the wellhead carries the `StructureLabel`). The
    // remaining 24 tiles — outer-ring lining walls and the inner helix —
    // must still reject wild-plant scatter and late-streamed obstacle
    // clearing. The seed-time stamp inserts these into the reservation
    // inline; this loop is the backstop for restamped / runtime-finalised
    // wells that didn't go through `stamp_seeded_well`.
    for &center in well_map.0.keys() {
        reservation.reserve_iter(crate::simulation::well::well_footprint(center));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_and_query() {
        let mut r = SeedReservation::default();
        assert!(!r.is_reserved((3, 4)));
        r.reserve((3, 4));
        assert!(r.is_reserved((3, 4)));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn reserve_iter_dedupes() {
        let mut r = SeedReservation::default();
        r.reserve_iter([(1, 1), (2, 2), (1, 1)]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn rasterise_horizontal_line_inclusive() {
        let mut r = SeedReservation::default();
        rasterize_line_into(&mut r, (0, 5), (3, 5), |_| false);
        // 2-tile-wide: centre row y=5 + widened row y=6 for each x in 0..=3.
        for x in 0..=3 {
            assert!(r.is_reserved((x, 5)), "missing centre ({x}, 5)");
            assert!(r.is_reserved((x, 6)), "missing widened ({x}, 6)");
        }
        assert_eq!(r.len(), 8);
    }

    #[test]
    fn rasterise_routes_widen_around_blocked_tile() {
        let mut r = SeedReservation::default();
        // Block the default (+Y) widen side at x=1 only.
        rasterize_line_into(&mut r, (0, 5), (3, 5), |p| p == (1, 6));
        assert!(r.is_reserved((1, 5)), "centre still reserved");
        assert!(!r.is_reserved((1, 6)), "blocked default side not reserved");
        assert!(r.is_reserved((1, 4)), "widened to the clear side");
        // Unblocked columns keep the default side.
        assert!(r.is_reserved((2, 6)));
    }

    #[test]
    fn rasterise_diagonal_line_covers_endpoints() {
        let mut r = SeedReservation::default();
        rasterize_line_into(&mut r, (0, 0), (3, 3), |_| false);
        assert!(r.is_reserved((0, 0)));
        assert!(r.is_reserved((3, 3)));
    }
}
