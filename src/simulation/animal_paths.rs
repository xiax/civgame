//! Animal pathfinding glue.
//!
//! Two layers wired up here, on top of the per-tile passability gate in
//! `animal_movement_system`:
//!
//! - **A\* for solo + PACK species** (Wolf/Fox/Cat/Rabbit/Pig): inline
//!   short-horizon A\* via `pathfinding::astar::find_path_in`, called on
//!   demand from `animal_movement_system` and cached on `AnimalAI.path`.
//!   No interaction with the persons' `PathRequestQueue` worker.
//!
//! - **Flow fields for HERD species** (Deer/Horse/Cow): a per-herd
//!   cohesion field (goal = herd centre) and an optional repulsion field
//!   (active while a predator sits within `HERD_THREAT_RADIUS`). HERD
//!   members in `Wander` follow cohesion; members in `Flee` follow
//!   repulsion. Anything not handled by either field falls through to A\*.
//!
//! Per project convention: per-agent local nav is A\*, flow fields
//! reserved for many-agent shared goals.

use crate::collections::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::pathfinding::astar::{find_path_in, AStarResult};
use crate::pathfinding::flow_field::{build_flow_field, walk_to_goal, FlowField};
use crate::pathfinding::pool::AStarScratch;
use crate::simulation::animals::{AnimalAI, AnimalState, Fox, HerdMember, Wolf};
use crate::simulation::lod::LodLevel;
use crate::simulation::perf::{BackgroundWorkDiagnostics, PerfWorkBudget};
use crate::simulation::schedule::SimClock;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

/// A* node budget per animal replan. Far below the person worker's 1500.
pub const ANIMAL_PATH_BUDGET: usize = 256;

/// Ticks an animal suppresses inline A\* replanning after a fruitless search
/// (`Unreachable` / `BudgetExhausted`). Without this, an animal whose goal sits
/// beyond `ANIMAL_PATH_BUDGET` nodes re-runs a full 256-node A\* every single
/// tick (the partial path is consumed in one step). One game-second at 20 Hz.
pub const ANIMAL_REPLAN_COOLDOWN_TICKS: u64 = 20;

/// Outcome of [`replan_astar`], so the caller can tell a clean path from a
/// fruitless search and apply [`ANIMAL_REPLAN_COOLDOWN_TICKS`] only to the
/// expensive cases.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnimalReplanOutcome {
    /// Full path to the goal was found.
    Planned,
    /// A* ran out of budget; a one-step partial toward `best_so_far` was
    /// stamped. Walkable, but the goal is out of short-horizon range.
    PlannedPartial,
    /// Goal unreachable; caller should drop into Wander.
    Unreachable,
}

/// Round-robin cursor over animal entities for the per-tick inline-A\* replan
/// budget (`PerfWorkBudget::animal_replans_per_tick`). Mirrors `VisionCursor`:
/// eligible animals needing a replan are sorted by entity bits, the slice
/// starting at `next_bits` consumes the budget, and the cursor advances past
/// the last served entity so every animal is revisited within
/// `ceil(N_needing_replan / cap)` ticks. Prevents starvation under a flat cap.
#[derive(Resource, Default)]
pub struct AnimalReplanCursor {
    pub next_bits: u64,
}

/// Round-robin selection of which animals may run inline A\* this tick.
///
/// `eligible` (the entities that need a replan, off-cooldown) is sorted in
/// place by entity bits. The slice of up to `cap` entities starting at the
/// first whose bits are `>= cursor_bits` (wrapping around the ring) is
/// returned as a set, along with the new cursor value — one past the last
/// served entity, so the next tick begins where this one stopped and every
/// animal is revisited within `ceil(len / cap)` ticks. Empty input → empty
/// set and an unchanged cursor. `cap` is floored at 1.
pub fn select_replan_slice(
    eligible: &mut Vec<Entity>,
    cap: usize,
    cursor_bits: u64,
) -> (AHashSet<Entity>, u64) {
    if eligible.is_empty() {
        return (AHashSet::default(), cursor_bits);
    }
    eligible.sort_unstable_by_key(|e| e.to_bits());
    let pivot = eligible
        .iter()
        .position(|e| e.to_bits() >= cursor_bits)
        .unwrap_or(0);
    let take = cap.max(1).min(eligible.len());
    let slice: AHashSet<Entity> = (0..take)
        .map(|off| eligible[(pivot + off) % eligible.len()])
        .collect();
    let last = eligible[(pivot + take - 1) % eligible.len()];
    (slice, last.to_bits().wrapping_add(1))
}

/// Maximum cells the cached path may be before we force a replan even if
/// the destination hasn't changed (guards against stale partials).
pub const ANIMAL_PATH_MAX_AGE_CURSOR: u16 = 200;

/// Centre-drift tolerance before a cohesion field gets invalidated.
pub const HERD_REBUILD_DRIFT: i32 = 4;

/// Chebyshev radius for "predator near herd" detection.
pub const HERD_THREAT_RADIUS: i32 = 12;

/// Ticks the repulsion field persists after the last threat sighting.
pub const HERD_THREAT_COOLDOWN: u64 = 100;

/// Cohesion-field penalty applied within this chebyshev radius of the
/// active threat tile. Pushes the field's gradient away from the wolf.
pub const HERD_REPULSION_RADIUS: i32 = 4;
pub const HERD_REPULSION_PENALTY: u16 = 800;

/// Per-herd cohesion + optional repulsion fields. Built lazily.
#[derive(Default)]
pub struct HerdCluster {
    pub center_tile: (i32, i32),
    pub center_z: i8,
    /// Last-known bloomed member count. 0 ⇒ entry can be evicted.
    pub members: u16,
    pub cohesion_field: Option<FlowField>,
    /// Anchor the cohesion field was built around. If the running centre
    /// drifts > `HERD_REBUILD_DRIFT` from this, we invalidate.
    pub cohesion_anchor: (i32, i32),
    pub repulsion_field: Option<FlowField>,
    pub repulsion_threat_tile: Option<(i32, i32)>,
    pub repulsion_last_sighting_tick: u64,
}

#[derive(Resource, Default)]
pub struct HerdClusterRegistry {
    pub by_id: AHashMap<u32, HerdCluster>,
}

/// Round-robin cursor over cluster ids for the per-tick herd-threat scan.
/// Each tick advances `PerfWorkBudget::herd_repulsion_rebuilds_per_tick`
/// clusters; the rest of the work amortises across the remaining ticks.
#[derive(Resource, Default)]
pub struct HerdClusterCursor {
    pub next_idx: u32,
}

impl HerdClusterRegistry {
    pub fn get(&self, cluster_id: u32) -> Option<&HerdCluster> {
        self.by_id.get(&cluster_id)
    }
}

/// Daily Economy pass: walk every `HerdMember` and recompute each
/// cluster's centre (mean of bloomed member tiles). Invalidates the
/// cohesion field when the centre drifts more than `HERD_REBUILD_DRIFT`
/// from the field's anchor, and evicts entries with zero bloomed members.
pub fn herd_cluster_update_system(
    clock: Res<SimClock>,
    mut registry: ResMut<HerdClusterRegistry>,
    chunk_map: Res<ChunkMap>,
    members: Query<(&HerdMember, &Transform, &LodLevel)>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }

    // Aggregate per cluster: count + sum of tile coords.
    let mut agg: AHashMap<u32, (i64, i64, u32)> = AHashMap::default();
    for (hm, transform, lod) in members.iter() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let entry = agg.entry(hm.cluster_id).or_insert((0, 0, 0));
        entry.0 += tx as i64;
        entry.1 += ty as i64;
        entry.2 += 1;
    }

    // Update existing clusters; insert new.
    for (cid, (sx, sy, n)) in agg.iter() {
        let avg_tx = (sx / *n as i64) as i32;
        let avg_ty = (sy / *n as i64) as i32;
        let z = chunk_map.surface_z_at(avg_tx, avg_ty) as i8;
        let entry = registry.by_id.entry(*cid).or_default();
        entry.center_tile = (avg_tx, avg_ty);
        entry.center_z = z;
        entry.members = (*n as u16).min(u16::MAX);
        let drift = (entry.center_tile.0 - entry.cohesion_anchor.0)
            .abs()
            .max((entry.center_tile.1 - entry.cohesion_anchor.1).abs());
        if drift > HERD_REBUILD_DRIFT {
            entry.cohesion_field = None;
        }
    }

    // Evict clusters with no bloomed members today.
    registry.by_id.retain(|cid, entry| {
        if agg.contains_key(cid) {
            true
        } else {
            entry.members = 0;
            // Keep the slot one day so a transient un-bloom doesn't lose
            // the field; rebuild from scratch on next sighting.
            entry.cohesion_field = None;
            entry.repulsion_field = None;
            false
        }
    });
}

/// Lazy field builder. Runs each tick but cheap-skips when nothing needs
/// rebuilding. Builds at most one cohesion field per tick to spread cost.
pub fn herd_cohesion_field_system(
    chunk_map: Res<ChunkMap>,
    mut registry: ResMut<HerdClusterRegistry>,
) {
    // Pick the first cluster that needs a build.
    let mut target: Option<u32> = None;
    for (cid, entry) in registry.by_id.iter() {
        if entry.members > 0 && entry.cohesion_field.is_none() {
            target = Some(*cid);
            break;
        }
    }
    let Some(cid) = target else { return };
    let entry = registry.by_id.get_mut(&cid).unwrap();
    let (cx, cy) = entry.center_tile;
    let cz = entry.center_z;
    let chunk = ChunkCoord(
        cx.div_euclid(CHUNK_SIZE as i32),
        cy.div_euclid(CHUNK_SIZE as i32),
    );
    let local_x = cx.rem_euclid(CHUNK_SIZE as i32) as u8;
    let local_y = cy.rem_euclid(CHUNK_SIZE as i32) as u8;
    let field = build_flow_field(&chunk_map, chunk, (local_x, local_y), cz, &|_| 0u16);
    entry.cohesion_field = Some(field);
    entry.cohesion_anchor = (cx, cy);
}

/// Per-tick predator-vs-herd scan. Replaces the legacy 20-tick burst
/// (which did a 25×25 spatial scan per bloomed member) with:
///
/// 1. **Predator snapshot** built every tick from `Wolf`/`Fox` queries.
/// 2. **Per-cluster bounds** aggregated from bloomed `HerdMember`s
///    (one pass, not per-member).
/// 3. **Cursor-driven cluster scan** — advance `PerfWorkBudget::
///    herd_repulsion_rebuilds_per_tick` clusters per tick, search the
///    predator snapshot against each cluster's expanded bounds.
///
/// Aggregate revisit rate per cluster is preserved (matches the legacy
/// 20-tick cadence at default budget), but no tick does the full sweep.
pub fn herd_threat_detect_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    budget: Res<PerfWorkBudget>,
    mut registry: ResMut<HerdClusterRegistry>,
    mut cursor: ResMut<HerdClusterCursor>,
    mut bg: ResMut<BackgroundWorkDiagnostics>,
    members: Query<(&HerdMember, &Transform, &LodLevel)>,
    wolf_xf: Query<&Transform, With<Wolf>>,
    fox_xf: Query<&Transform, With<Fox>>,
) {
    let now = clock.tick;
    let t_start = std::time::Instant::now();

    // (1) Predator snapshot — one cheap pass, all predator tiles.
    let mut predator_tiles: AHashSet<(i32, i32)> = AHashSet::default();
    for xf in wolf_xf.iter() {
        let tx = (xf.translation.x / TILE_SIZE).floor() as i32;
        let ty = (xf.translation.y / TILE_SIZE).floor() as i32;
        predator_tiles.insert((tx, ty));
    }
    for xf in fox_xf.iter() {
        let tx = (xf.translation.x / TILE_SIZE).floor() as i32;
        let ty = (xf.translation.y / TILE_SIZE).floor() as i32;
        predator_tiles.insert((tx, ty));
    }
    bg.herd_predators_indexed = predator_tiles.len() as u32;

    // (2) Per-cluster bounds from bloomed members.
    struct ClusterBounds {
        x_min: i32,
        y_min: i32,
        x_max: i32,
        y_max: i32,
    }
    let mut bounds: AHashMap<u32, ClusterBounds> = AHashMap::default();
    for (hm, transform, lod) in members.iter() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let mx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let my = (transform.translation.y / TILE_SIZE).floor() as i32;
        bounds
            .entry(hm.cluster_id)
            .and_modify(|b| {
                b.x_min = b.x_min.min(mx);
                b.y_min = b.y_min.min(my);
                b.x_max = b.x_max.max(mx);
                b.y_max = b.y_max.max(my);
            })
            .or_insert(ClusterBounds {
                x_min: mx,
                y_min: my,
                x_max: mx,
                y_max: my,
            });
    }

    // (3) Cursor over cluster ids. Sort for deterministic round-robin.
    let mut cluster_ids: Vec<u32> = registry.by_id.keys().copied().collect();
    cluster_ids.sort_unstable();
    if cluster_ids.is_empty() {
        return;
    }
    // `cap` is the floor for the per-tick scan window; revisit rate per
    // cluster ≈ N_clusters / scan_cap ticks.
    let cap = budget
        .herd_repulsion_rebuilds_per_tick
        .max(1)
        .min(cluster_ids.len());
    // NOTE: the budget bounds how many clusters we SCAN per tick, not how many
    // flow-field rebuilds fire — a scanned cluster whose nearest-threat tile
    // changed rebuilds immediately, so up to `scan_cap` rebuilds can happen in
    // one tick if many clusters' threats shift together. Acceptable because the
    // predator check is O(small) and live herd-cluster counts are low (a
    // handful); if cluster counts ever grow large, add a separate per-tick
    // rebuild cap + deferral queue here.
    let scan_cap = cap.max((cluster_ids.len() + 19) / 20).min(cluster_ids.len());

    // Locate cursor start.
    let pivot = cluster_ids
        .iter()
        .position(|&c| c >= cursor.next_idx)
        .unwrap_or(0);
    let mut scanned: u32 = 0;
    let mut rebuilt: u32 = 0;

    for offset in 0..scan_cap {
        let i = (pivot + offset) % cluster_ids.len();
        let cid = cluster_ids[i];
        scanned += 1;
        let entry = match registry.by_id.get_mut(&cid) {
            Some(e) => e,
            None => continue,
        };
        if entry.members == 0 {
            entry.repulsion_field = None;
            entry.repulsion_threat_tile = None;
            continue;
        }

        // Find nearest predator within cluster bounds expanded by HERD_THREAT_RADIUS.
        let nearest = bounds.get(&cid).and_then(|b| {
            nearest_predator_in_bounds(
                &predator_tiles,
                (b.x_min, b.x_max, b.y_min, b.y_max),
                entry.center_tile,
            )
        });

        match nearest {
            Some((threat_tile, _)) => {
                let rebuild = entry.repulsion_threat_tile != Some(threat_tile)
                    || entry.repulsion_field.is_none();
                entry.repulsion_threat_tile = Some(threat_tile);
                entry.repulsion_last_sighting_tick = now;
                if rebuild {
                    let field = build_repulsion_field(
                        &chunk_map,
                        entry.center_tile,
                        entry.center_z,
                        threat_tile,
                    );
                    entry.repulsion_field = field;
                    rebuilt += 1;
                }
            }
            None => {
                if entry.repulsion_threat_tile.is_some()
                    && now.saturating_sub(entry.repulsion_last_sighting_tick) > HERD_THREAT_COOLDOWN
                {
                    entry.repulsion_threat_tile = None;
                    entry.repulsion_field = None;
                }
            }
        }
    }

    // Advance cursor past the last scanned cluster id.
    let last_idx = (pivot + scan_cap.saturating_sub(1)) % cluster_ids.len();
    cursor.next_idx = cluster_ids[last_idx].saturating_add(1);

    bg.herd_clusters_scanned = scanned;
    bg.herd_repulsion_built_last_tick = rebuilt;
    bg.herd_threat_scan_us = crate::simulation::perf::micros_u32(t_start.elapsed());
}

/// Nearest predator tile to a herd cluster, used by `herd_threat_detect_system`.
///
/// A predator counts when it lies inside the cluster's bounding box
/// (`bounds = (x_min, x_max, y_min, y_max)`) expanded by `HERD_THREAT_RADIUS`
/// on every side. Among those, returns the one nearest `center` by chebyshev
/// distance, with a deterministic `(distance, x, y)` tie-break — `predators`
/// is a process-seeded `AHashSet`, so a bare distance `min` would pick a
/// run-dependent tile on ties and make the herd's flee direction
/// non-deterministic. Returns `(threat_tile, chebyshev_distance)`.
pub(crate) fn nearest_predator_in_bounds(
    predators: &AHashSet<(i32, i32)>,
    bounds: (i32, i32, i32, i32),
    center: (i32, i32),
) -> Option<((i32, i32), i32)> {
    let (x_min, x_max, y_min, y_max) = bounds;
    let lo_x = x_min - HERD_THREAT_RADIUS;
    let hi_x = x_max + HERD_THREAT_RADIUS;
    let lo_y = y_min - HERD_THREAT_RADIUS;
    let hi_y = y_max + HERD_THREAT_RADIUS;
    let (cx, cy) = center;
    predators
        .iter()
        .filter(|&&(tx, ty)| tx >= lo_x && tx <= hi_x && ty >= lo_y && ty <= hi_y)
        .map(|&(tx, ty)| ((tx, ty), (tx - cx).abs().max((ty - cy).abs())))
        .min_by_key(|&((tx, ty), d)| (d, tx, ty))
}

/// Build a single-chunk repulsion field: goal = a "safe anchor" projected
/// away from the threat through the herd centre, plus an `extra_cost`
/// penalty in a 4-tile radius around the threat tile. Returns None if
/// the centre and the safe anchor don't share a chunk (next tick the
/// caller will retry).
fn build_repulsion_field(
    chunk_map: &ChunkMap,
    center: (i32, i32),
    center_z: i8,
    threat: (i32, i32),
) -> Option<FlowField> {
    let (cx, cy) = center;
    let (tx, ty) = threat;
    // Vector from threat to center; project center outward to "safe" anchor.
    let mut dx = cx - tx;
    let mut dy = cy - ty;
    let mag = dx.abs().max(dy.abs()).max(1);
    // Normalise to ±1, then walk 6 tiles further outward from center.
    let nx = (dx / mag).clamp(-1, 1);
    let ny = (dy / mag).clamp(-1, 1);
    dx = nx;
    dy = ny;
    let safe_anchor = (cx + dx * 6, cy + dy * 6);

    let chunk = ChunkCoord(
        safe_anchor.0.div_euclid(CHUNK_SIZE as i32),
        safe_anchor.1.div_euclid(CHUNK_SIZE as i32),
    );
    // Build only when the safe anchor sits in a same-or-adjacent chunk as
    // the centre (so most bloomed members are in-chunk to consume the field).
    let center_chunk = ChunkCoord(
        cx.div_euclid(CHUNK_SIZE as i32),
        cy.div_euclid(CHUNK_SIZE as i32),
    );
    if (chunk.0 - center_chunk.0).abs() > 1 || (chunk.1 - center_chunk.1).abs() > 1 {
        return None;
    }
    let local_x = safe_anchor.0.rem_euclid(CHUNK_SIZE as i32) as u8;
    let local_y = safe_anchor.1.rem_euclid(CHUNK_SIZE as i32) as u8;

    let z = chunk_map.surface_z_at(safe_anchor.0, safe_anchor.1) as i8;
    // Penalty closure: large cost near the threat tile.
    let threat_pos = threat;
    let field = build_flow_field(chunk_map, chunk, (local_x, local_y), z, &move |pos| {
        let d = (pos.0 - threat_pos.0)
            .abs()
            .max((pos.1 - threat_pos.1).abs());
        if d <= HERD_REPULSION_RADIUS {
            HERD_REPULSION_PENALTY
        } else {
            0
        }
    });
    // Fall back gracefully if the build did nothing useful (we still
    // store the partial; movement will reject impossible steps).
    let _ = z;
    let _ = center_z;
    Some(field)
}

/// Replan a path for a solo/pack animal using inline A*. Stashes the path
/// on `ai.path` + sets `path_cursor=1` (cursor 0 is the start tile).
/// Returns an [`AnimalReplanOutcome`] so the caller can distinguish a full
/// path from a fruitless search and apply a replan cooldown to the latter.
pub fn replan_astar(
    scratch: &mut AStarScratch,
    chunk_map: &ChunkMap,
    ai: &mut AnimalAI,
    start: (i32, i32, i8),
    goal: (i32, i32, i8),
) -> AnimalReplanOutcome {
    let (result, _) = find_path_in(scratch, chunk_map, start, goal, ANIMAL_PATH_BUDGET);
    ai.path.clear();
    ai.path_cursor = 0;
    match result {
        AStarResult::Found(p) => {
            // `find_path_in` returns path *excluding* start; prepend start
            // so cursor==1 step-toward semantics line up with our loop.
            ai.path.push(start);
            ai.path.extend(p);
            ai.path_cursor = 1;
            ai.path_goal = (goal.0, goal.1);
            AnimalReplanOutcome::Planned
        }
        AStarResult::BudgetExhausted { best_so_far } => {
            // Walk toward the partial. We don't have the chain of waypoints
            // to `best_so_far`, so just plant it as the single next target.
            ai.path.push(start);
            ai.path.push(best_so_far);
            ai.path_cursor = 1;
            ai.path_goal = (goal.0, goal.1);
            AnimalReplanOutcome::PlannedPartial
        }
        AStarResult::Unreachable => AnimalReplanOutcome::Unreachable,
    }
}

/// Try to fill `ai.path` for a HERD-species animal via the appropriate
/// flow field. Returns `true` if the path was stamped from a field;
/// `false` when the caller should fall through to `replan_astar`.
///
/// - `Flee` ⇒ repulsion field (if active).
/// - `Wander` ⇒ cohesion field.
/// - other states ⇒ false (use A\*).
pub fn try_replan_via_flow_field(
    registry: &HerdClusterRegistry,
    ai: &mut AnimalAI,
    cluster_id: u32,
    start_tile: (i32, i32),
    start_z: i8,
) -> bool {
    let Some(cluster) = registry.get(cluster_id) else {
        return false;
    };
    let field = match ai.state {
        AnimalState::Flee => cluster.repulsion_field.as_ref(),
        AnimalState::Wander => cluster.cohesion_field.as_ref(),
        _ => None,
    };
    let Some(field) = field else {
        return false;
    };

    // Member must be in the same chunk as the field for `walk_to_goal`.
    let csz = CHUNK_SIZE as i32;
    let start_chunk = ChunkCoord(start_tile.0.div_euclid(csz), start_tile.1.div_euclid(csz));
    if start_chunk != field.chunk {
        return false;
    }
    let local_x = start_tile.0.rem_euclid(csz) as u8;
    let local_y = start_tile.1.rem_euclid(csz) as u8;
    let Some(path) = walk_to_goal(field, (local_x, local_y)) else {
        return false;
    };
    if path.is_empty() {
        return false;
    }
    ai.path.clear();
    ai.path.push((start_tile.0, start_tile.1, start_z));
    ai.path.extend(path);
    ai.path_cursor = 1;
    ai.path_goal = (field.goal_tile.0 as i32, field.goal_tile.1 as i32);
    true
}

/// Bevy plugin glue: register the resource. Systems are wired into the
/// `SimulationPlugin` directly (Economy set for the daily cluster update,
/// per-tick for the field builders).
pub fn build(app: &mut App) {
    app.init_resource::<HerdClusterRegistry>();
    app.init_resource::<HerdClusterCursor>();
    app.init_resource::<AnimalReplanCursor>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(tiles: &[(i32, i32)]) -> AHashSet<(i32, i32)> {
        tiles.iter().copied().collect()
    }

    #[test]
    fn no_predators_in_range_returns_none() {
        // Cluster box [0,0], radius 12 → window [-12,12]. Predator at (50,50) outside.
        let preds = set(&[(50, 50)]);
        assert_eq!(
            nearest_predator_in_bounds(&preds, (0, 0, 0, 0), (0, 0)),
            None
        );
    }

    #[test]
    fn predator_just_inside_expanded_window_detected() {
        // x_max=0 + HERD_THREAT_RADIUS(12) = 12 → predator at (12,0) is the edge.
        let preds = set(&[(HERD_THREAT_RADIUS, 0)]);
        let got = nearest_predator_in_bounds(&preds, (0, 0, 0, 0), (0, 0));
        assert_eq!(got, Some(((HERD_THREAT_RADIUS, 0), HERD_THREAT_RADIUS)));
        // One tile further out is excluded.
        let preds_out = set(&[(HERD_THREAT_RADIUS + 1, 0)]);
        assert_eq!(
            nearest_predator_in_bounds(&preds_out, (0, 0, 0, 0), (0, 0)),
            None
        );
    }

    #[test]
    fn nearest_by_chebyshev_to_center_wins() {
        let preds = set(&[(10, 0), (3, 0), (7, 7)]);
        let got = nearest_predator_in_bounds(&preds, (0, 0, 0, 0), (0, 0));
        assert_eq!(got, Some(((3, 0), 3)));
    }

    #[test]
    fn distance_ties_broken_deterministically_by_coords() {
        // (2,0),(0,2),(-2,0),(0,-2) all chebyshev 2 from origin. Tie-break
        // is (d, x, y) ascending → smallest x then y → (-2, 0).
        let preds = set(&[(2, 0), (0, 2), (-2, 0), (0, -2)]);
        let got = nearest_predator_in_bounds(&preds, (-4, 4, -4, 4), (0, 0));
        assert_eq!(got, Some(((-2, 0), 2)));
        // Insertion order must not matter (set is process-hashed anyway).
        let preds_rev = set(&[(0, -2), (-2, 0), (0, 2), (2, 0)]);
        assert_eq!(
            nearest_predator_in_bounds(&preds_rev, (-4, 4, -4, 4), (0, 0)),
            got
        );
    }

    fn ents(ids: &[u32]) -> Vec<Entity> {
        ids.iter().map(|&i| Entity::from_raw(i)).collect()
    }

    #[test]
    fn replan_slice_empty_input_keeps_cursor() {
        let mut e: Vec<Entity> = Vec::new();
        let (slice, next) = select_replan_slice(&mut e, 4, 99);
        assert!(slice.is_empty());
        assert_eq!(next, 99, "cursor must not move when nothing is eligible");
    }

    #[test]
    fn replan_slice_cap_ge_len_takes_all() {
        let mut e = ents(&[3, 1, 2]);
        let (slice, next) = select_replan_slice(&mut e, 8, 0);
        assert_eq!(slice.len(), 3);
        for id in [1u32, 2, 3] {
            assert!(slice.contains(&Entity::from_raw(id)));
        }
        // Sorted ascending, served all → cursor past the largest.
        assert_eq!(next, Entity::from_raw(3).to_bits().wrapping_add(1));
    }

    #[test]
    fn replan_slice_caps_and_advances_cursor() {
        let mut e = ents(&[10, 20, 30, 40]);
        // Start of ring, cap 2 → serve {10,20}, cursor past 20.
        let (slice, next) = select_replan_slice(&mut e, 2, 0);
        assert_eq!(slice.len(), 2);
        assert!(slice.contains(&Entity::from_raw(10)));
        assert!(slice.contains(&Entity::from_raw(20)));
        assert_eq!(next, Entity::from_raw(20).to_bits().wrapping_add(1));
    }

    #[test]
    fn replan_slice_two_ticks_cover_everyone_no_starvation() {
        let ids = [10u32, 20, 30, 40];
        let mut cursor = 0u64;
        let mut seen: AHashSet<Entity> = AHashSet::default();
        for _ in 0..2 {
            let mut e = ents(&ids);
            let (slice, next) = select_replan_slice(&mut e, 2, cursor);
            seen.extend(slice);
            cursor = next;
        }
        // ceil(4/2) = 2 ticks revisit everyone.
        assert_eq!(seen.len(), 4, "every animal served within ceil(len/cap) ticks");
    }

    #[test]
    fn replan_slice_cursor_past_all_wraps_to_front() {
        let mut e = ents(&[10, 20, 30, 40]);
        // Cursor beyond every entity's bits → pivot wraps to index 0.
        let big = Entity::from_raw(40).to_bits().wrapping_add(1000);
        let (slice, _next) = select_replan_slice(&mut e, 2, big);
        assert!(slice.contains(&Entity::from_raw(10)));
        assert!(slice.contains(&Entity::from_raw(20)));
    }

    #[test]
    fn window_expands_with_cluster_bounds_not_just_center() {
        // Cluster spans x in [0,20]; center at (10,0). Predator at (31,0) is
        // within x_max(20)+12 = 32, so detected even though it's 21 from center.
        let preds = set(&[(31, 0)]);
        let got = nearest_predator_in_bounds(&preds, (0, 20, 0, 0), (10, 0));
        assert_eq!(got, Some(((31, 0), 21)));
    }
}
