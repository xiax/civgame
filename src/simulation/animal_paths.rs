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

use ahash::AHashMap;
use bevy::prelude::*;

use crate::pathfinding::astar::{find_path_in, AStarResult};
use crate::pathfinding::flow_field::{build_flow_field, walk_to_goal, FlowField};
use crate::pathfinding::pool::AStarScratch;
use crate::simulation::animals::{AnimalAI, AnimalState, Fox, HerdMember, Wolf};
use crate::simulation::lod::LodLevel;
use crate::simulation::schedule::SimClock;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

/// A* node budget per animal replan. Far below the person worker's 1500.
pub const ANIMAL_PATH_BUDGET: usize = 256;

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
    let mut agg: AHashMap<u32, (i64, i64, u32)> = AHashMap::new();
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
    let chunk = ChunkCoord(cx.div_euclid(CHUNK_SIZE as i32), cy.div_euclid(CHUNK_SIZE as i32));
    let local_x = cx.rem_euclid(CHUNK_SIZE as i32) as u8;
    let local_y = cy.rem_euclid(CHUNK_SIZE as i32) as u8;
    let field = build_flow_field(&chunk_map, chunk, (local_x, local_y), cz, &|_| 0u16);
    entry.cohesion_field = Some(field);
    entry.cohesion_anchor = (cx, cy);
}

/// Scan bloomed herd members for nearby predators. Sets / clears each
/// cluster's repulsion field. Cheap: O(herd_members × predators_within_radius)
/// via spatial index, runs every 20 ticks.
pub fn herd_threat_detect_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    spatial: Res<SpatialIndex>,
    mut registry: ResMut<HerdClusterRegistry>,
    members: Query<(&HerdMember, &Transform, &LodLevel)>,
    wolf_q: Query<(), With<Wolf>>,
    fox_q: Query<(), With<Fox>>,
) {
    if clock.tick % 20 != 0 {
        return;
    }
    let now = clock.tick;

    // For each cluster, gather the nearest predator (chebyshev) seen by
    // any bloomed member. One-pass.
    let mut nearest_threat: AHashMap<u32, ((i32, i32), i32)> = AHashMap::new();
    for (hm, transform, lod) in members.iter() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let mx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let my = (transform.translation.y / TILE_SIZE).floor() as i32;

        // Scan neighbours within HERD_THREAT_RADIUS via spatial index.
        for dy in -HERD_THREAT_RADIUS..=HERD_THREAT_RADIUS {
            for dx in -HERD_THREAT_RADIUS..=HERD_THREAT_RADIUS {
                let tx = mx + dx;
                let ty = my + dy;
                for &ent in spatial.get(tx, ty) {
                    if wolf_q.get(ent).is_ok() || fox_q.get(ent).is_ok() {
                        let d = dx.abs().max(dy.abs());
                        let slot = nearest_threat.entry(hm.cluster_id).or_insert(((tx, ty), d));
                        if d < slot.1 {
                            *slot = ((tx, ty), d);
                        }
                    }
                }
            }
        }
    }

    // Apply: rebuild repulsion when threat present + tile changed; expire after cooldown.
    for (cid, entry) in registry.by_id.iter_mut() {
        if entry.members == 0 {
            entry.repulsion_field = None;
            entry.repulsion_threat_tile = None;
            continue;
        }
        match nearest_threat.get(cid) {
            Some((threat_tile, _)) => {
                let rebuild = entry.repulsion_threat_tile != Some(*threat_tile)
                    || entry.repulsion_field.is_none();
                entry.repulsion_threat_tile = Some(*threat_tile);
                entry.repulsion_last_sighting_tick = now;
                if rebuild {
                    let field = build_repulsion_field(
                        &chunk_map,
                        entry.center_tile,
                        entry.center_z,
                        *threat_tile,
                    );
                    entry.repulsion_field = field;
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
/// Returns `true` when a usable path was found (full or partial),
/// `false` when the goal is unreachable and the caller should drop into
/// Wander.
pub fn replan_astar(
    scratch: &mut AStarScratch,
    chunk_map: &ChunkMap,
    ai: &mut AnimalAI,
    start: (i32, i32, i8),
    goal: (i32, i32, i8),
) -> bool {
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
            true
        }
        AStarResult::BudgetExhausted { best_so_far } => {
            // Walk toward the partial. We don't have the chain of waypoints
            // to `best_so_far`, so just plant it as the single next target.
            ai.path.push(start);
            ai.path.push(best_so_far);
            ai.path_cursor = 1;
            ai.path_goal = (goal.0, goal.1);
            true
        }
        AStarResult::Unreachable => false,
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
}

