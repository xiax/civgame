use super::animals::{CarriedBy, Deer, Horse, Tamed, Wolf};
use super::combat::{Body, Health};
use super::construction::{Bed, BedMap, ChairMap, LoomMap, TableMap, WorkbenchMap};
use super::faction::{FactionMember, FactionRegistry};
use super::items::GroundItem;
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::person::{AiState, Person, PersonAI};
use super::plants::Plant;
use super::schedule::{BucketSlot, SimClock};
use super::tasks::task_interacts_from_adjacent;
use super::technology::HORSEBACK_RIDING;
use crate::pathfinding::astar::{find_path_in, AStarResult};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::flow_field::FlowFieldCache;
use crate::pathfinding::pool::{AStarPool, AStarScratch};
use crate::pathfinding::tile_cost::{furniture_speed_factor, tile_speed_multiplier};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use ahash::AHashSet;
use bevy::prelude::*;
use rand::Rng;

const MOVE_SPEED: f32 = 48.0; // pixels per second
const MOUNTED_SPEED: f32 = 80.0; // speed when riding a horse
const IDLE_WANDER_INTERVAL: f32 = 2.5; // seconds between random moves
/// Max nodes A* will expand per call when the cheap flow field can't help.
/// ~500 covers ~10–20 tiles of detour around obstacles; nearby ramps can
/// be found, far ones can't. The result is cached on `MovementState` so
/// A* only runs when the path is consumed or the target changes.
const ASTAR_BUDGET: usize = 500;

#[derive(Component, Default)]
pub struct MovementState {
    pub wander_timer: f32,
    /// Cached A* path: each entry is a tile step, in order. Empty when no
    /// path is cached. Reused across ticks so A* only fires when consumed.
    pub astar_path: Vec<(i16, i16, i8)>,
    /// The (target_tile, target_z) the cached path was built for. If the
    /// agent's target changes, the cache is invalidated and recomputed.
    pub astar_target: (i16, i16, i8),
    /// Index of the next step in `astar_path` the agent should head toward.
    pub astar_cursor: u16,
}

/// Returns the next tile step the agent should walk toward to follow an
/// A* path to `(target.0, target.1, target_z)`. Reuses the cached path on
/// `mv` when valid; otherwise recomputes (capped by `ASTAR_BUDGET`).
/// `None` means A* couldn't find any path within budget.
fn astar_next_step(
    mv: &mut MovementState,
    scratch: &mut AStarScratch,
    chunk_map: &ChunkMap,
    cur: (i32, i32, i8),
    target: (i32, i32, i8),
) -> Option<(i32, i32)> {
    let target_i16 = (target.0 as i16, target.1 as i16, target.2);

    // Reuse the cached path if it's for this target. Advance the cursor
    // past steps the agent has already crossed (a single tick can pass
    // multiple short steps if speeds are high or sim_dt is large).
    if mv.astar_target == target_i16 && !mv.astar_path.is_empty() {
        while (mv.astar_cursor as usize) < mv.astar_path.len() {
            let (sx, sy, _) = mv.astar_path[mv.astar_cursor as usize];
            if (sx as i32, sy as i32) == (cur.0, cur.1) {
                mv.astar_cursor += 1;
                continue;
            }
            return Some((sx as i32, sy as i32));
        }
        // Path consumed — fall through to recompute.
    }

    let path = match find_path_in(scratch, chunk_map, cur, target, ASTAR_BUDGET) {
        AStarResult::Found(p) if !p.is_empty() => p,
        _ => return None,
    };
    mv.astar_path = path.iter().map(|&(x, y, z)| (x as i16, y as i16, z)).collect();
    mv.astar_target = target_i16;
    mv.astar_cursor = 0;
    let &(sx, sy, _) = mv.astar_path.first()?;
    Some((sx as i32, sy as i32))
}

/// Placed on a person while they are mounted on a horse.
#[derive(Component, Clone, Copy)]
pub struct MountedOn(pub Entity);

/// Release an agent stuck at a tile that the movement system cannot proceed
/// from (wall block, no standable Z) so the dispatch and plan systems will
/// pick them up next tick. Without clearing `task_id` and `target_tile`, the
/// agent stays Idle but every tick re-walks toward the unreachable target,
/// hits the same obstacle, and drops to Idle again forever.
fn release_to_idle(ai: &mut PersonAI, here: (i32, i32)) {
    ai.state = AiState::Idle;
    ai.task_id = PersonAI::UNEMPLOYED;
    ai.target_tile = (here.0 as i16, here.1 as i16);
    ai.dest_tile = ai.target_tile;
    ai.target_entity = None;
}

pub fn movement_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    spatial_index: Res<SpatialIndex>,
    bed_map: Res<BedMap>,
    chair_map: Res<ChairMap>,
    table_map: Res<TableMap>,
    workbench_map: Res<WorkbenchMap>,
    loom_map: Res<LoomMap>,
    mut flow_field_cache: ResMut<FlowFieldCache>,
    mut astar_pool: ResMut<AStarPool>,
    mut claimed_this_tick: Local<AHashSet<(i32, i32, i32)>>,
    mut query: Query<(
        Entity,
        &mut Transform,
        &mut PersonAI,
        &LodLevel,
        &mut MovementState,
        &BucketSlot,
        Option<&RelationshipMemory>,
        Option<&MountedOn>,
    )>,
) {
    let dt = time.delta_secs();
    let speed = clock.speed;
    let sim_dt = dt * clock.scale_factor();

    claimed_this_tick.clear();

    // Borrow one A* scratch buffer for the duration of the tick. Reused
    // across all agents in the loop — each call to find_path_in resets it.
    let scratch = astar_pool.scratch(0);

    // Movement can't be fully parallel because it writes Transform (position sync)
    // and can read ChunkMap for passability. Run sequentially.
    for (_entity, mut transform, mut ai, lod, mut mv, slot, rel_opt, mounted_opt) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }

        let pos = transform.translation.truncate();
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let to_target = target_world - pos;
        let dist = to_target.length();

        if dist > 2.0 {
            // Working agent stopped adjacent to resource — stay put and accumulate progress.
            if ai.state == AiState::Working {
                if clock.is_active(slot.0) {
                    let progress = (sim_dt * 20.0).max(0.0) as u8;
                    ai.work_progress = ai.work_progress.saturating_add(progress);
                }
                continue;
            }

            // Interaction tasks: switch to Working when ≤1 tile (Chebyshev) from dest_tile
            // and within the correct Z range (same level or one above — agents can reach
            // down but not up through a ceiling).
            if ai.state == AiState::Seeking && task_interacts_from_adjacent(ai.task_id) {
                let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
                let cheb = (cur_tx - ai.dest_tile.0 as i32)
                    .abs()
                    .max((cur_ty - ai.dest_tile.1 as i32).abs());
                // Compare against the resource's Z, not target_z (which is now
                // the route tile's Z). Allow ±1 so dig/terraform/build can
                // reach a tile slightly below the agent's foot, not just above.
                let dest_z = chunk_map.nearest_standable_z(
                    ai.dest_tile.0 as i32,
                    ai.dest_tile.1 as i32,
                    ai.current_z as i32,
                ) as i32;
                let dz = dest_z - ai.current_z as i32;
                if cheb <= 1 && (-1..=1).contains(&dz) {
                    ai.state = AiState::Working;
                    continue;
                }
            }

            // Pick the immediate step target. When the agent is in the same
            // chunk as `target_tile`, follow the cached flow field one tile
            // at a time (so they route around walls / slow tiles / furniture
            // instead of walking straight through). Cross-chunk waypoints
            // (Routing) and same-tile finals fall back to straight-line.
            let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
            let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
            let target_tx = ai.target_tile.0 as i32;
            let target_ty = ai.target_tile.1 as i32;
            let csz = CHUNK_SIZE as i32;
            let target_chunk = ChunkCoord(target_tx.div_euclid(csz), target_ty.div_euclid(csz));
            let cur_chunk = ChunkCoord(cur_tx.div_euclid(csz), cur_ty.div_euclid(csz));
            let step_world = if cur_chunk == target_chunk
                && (cur_tx, cur_ty) != (target_tx, target_ty)
            {
                let goal_local = (
                    (target_tx - target_chunk.0 * csz) as u8,
                    (target_ty - target_chunk.1 * csz) as u8,
                );
                let cur_local = (
                    (cur_tx - cur_chunk.0 * csz) as u8,
                    (cur_ty - cur_chunk.1 * csz) as u8,
                );
                let bm = &*bed_map;
                let cm = &*chair_map;
                let tm = &*table_map;
                let wm = &*workbench_map;
                let lm = &*loom_map;
                let field = flow_field_cache.get_or_build(
                    &chunk_map,
                    target_chunk,
                    goal_local,
                    ai.target_z,
                    |(gtx, gty)| {
                        let p = (gtx as i16, gty as i16);
                        if bm.0.contains_key(&p)
                            || cm.0.contains_key(&p)
                            || tm.0.contains_key(&p)
                            || wm.0.contains_key(&p)
                            || lm.0.contains_key(&p)
                        {
                            100
                        } else {
                            0
                        }
                    },
                );
                let idx = cur_local.1 as usize * CHUNK_SIZE + cur_local.0 as usize;
                let dir_byte = field.directions[idx];
                if dir_byte == 0xFF {
                    // In-chunk BFS couldn't reach this cell — typically a
                    // 2-Z cliff with no ramp inside this chunk. Fall back to
                    // bounded A* over tiles, which CAN route via ramps in
                    // neighbouring chunks. If A* also fails, release.
                    match astar_next_step(
                        &mut mv,
                        scratch,
                        &chunk_map,
                        (cur_tx, cur_ty, ai.current_z),
                        (target_tx, target_ty, ai.target_z),
                    ) {
                        Some((sx, sy)) => tile_to_world(sx, sy),
                        None => {
                            release_to_idle(&mut ai, (cur_tx, cur_ty));
                            continue;
                        }
                    }
                } else {
                    let (dx, dy) = match dir_byte {
                        0 => (0, 1),
                        1 => (1, 1),
                        2 => (1, 0),
                        3 => (1, -1),
                        4 => (0, -1),
                        5 => (-1, -1),
                        6 => (-1, 0),
                        7 => (-1, 1),
                        _ => (0, 0),
                    };
                    tile_to_world(cur_tx + dx, cur_ty + dy)
                }
            } else if (cur_tx, cur_ty) == (target_tx, target_ty) {
                // Already at target tile — straight-line "step" is a no-op.
                target_world
            } else {
                // Cross-chunk routing: target_tile is in a different chunk
                // (typically a chunk-graph waypoint). Use A* so we route
                // around in-chunk obstacles (e.g. 2-Z cliff between us and
                // the border tile) rather than straight-lining into them.
                match astar_next_step(
                    &mut mv,
                    scratch,
                    &chunk_map,
                    (cur_tx, cur_ty, ai.current_z),
                    (target_tx, target_ty, ai.target_z),
                ) {
                    Some((sx, sy)) => tile_to_world(sx, sy),
                    None => {
                        release_to_idle(&mut ai, (cur_tx, cur_ty));
                        continue;
                    }
                }
            };

            let to_step = step_world - pos;
            let step_len = to_step.length();
            if step_len < 0.001 {
                continue;
            }
            let dir = to_step / step_len;
            let mut effective_speed = if mounted_opt.is_some() { MOUNTED_SPEED } else { MOVE_SPEED };
            // Per-tile terrain multiplier (Road 1.4×, Forest 0.7×, etc.).
            if let Some(kind) = chunk_map.tile_kind_at(cur_tx, cur_ty) {
                let m = tile_speed_multiplier(kind);
                if m > 0.0 {
                    effective_speed *= m;
                }
            }
            // Furniture slowdown (Bed/Chair/Table/Workbench/Loom).
            effective_speed *= furniture_speed_factor(
                (cur_tx as i16, cur_ty as i16),
                &bed_map,
                &chair_map,
                &table_map,
                &workbench_map,
                &loom_map,
            );
            let step = dir * effective_speed * dt * speed;
            let new_pos = pos + step;

            // Hard wall block: if stepping into a different tile would land us
            // on an impassable cell at every reachable Z, refuse the step and
            // drop back to Idle so a fresh goal/route can be picked. Catches
            // the case where a wall is built across an agent's straight-line
            // path within a chunk, or the flow-field cache is one frame stale.
            let prev_tx = cur_tx;
            let prev_ty = cur_ty;
            let new_tx = (new_pos.x / TILE_SIZE).floor() as i32;
            let new_ty = (new_pos.y / TILE_SIZE).floor() as i32;
            let crossing_boundary = new_tx != prev_tx || new_ty != prev_ty;
            let cz = ai.current_z as i32;
            if crossing_boundary
                && !chunk_map.passable_at(new_tx, new_ty, cz)
                && !chunk_map.passable_at(new_tx, new_ty, cz + 1)
                && !chunk_map.passable_at(new_tx, new_ty, cz - 1)
            {
                release_to_idle(&mut ai, (cur_tx, cur_ty));
                continue;
            }

            transform.translation.x = new_pos.x;
            transform.translation.y = new_pos.y;

            // Eagerly sync current_z when crossing a tile boundary so that
            // update_entity_z_visibility_system (entity_z == surf_z) never
            // sees a stale Z during the transit window. If no neighbouring
            // Z slice is standable, leave current_z alone and drop to Idle
            // — snapping to surface_z would teleport an underground agent
            // up onto the surface mid-walk.
            if crossing_boundary {
                if chunk_map.passable_at(new_tx, new_ty, cz) {
                    ai.current_z = cz as i8;
                } else if chunk_map.passable_at(new_tx, new_ty, cz + 1) {
                    ai.current_z = (cz + 1) as i8;
                } else if chunk_map.passable_at(new_tx, new_ty, cz - 1) {
                    ai.current_z = (cz - 1) as i8;
                } else {
                    release_to_idle(&mut ai, (cur_tx, cur_ty));
                    continue;
                }
            }
        } else {
            // Arrived at target
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;

            // Update foot Z: prefer staying at current_z; otherwise step ±1
            // (e.g. crossing a ramp). If no neighbouring Z is standable,
            // keep current_z and drop to Idle — snapping to surface_z
            // would warp an underground agent up out of their tunnel.
            let arrived_tx = (target_world.x / TILE_SIZE).floor() as i32;
            let arrived_ty = (target_world.y / TILE_SIZE).floor() as i32;
            let cz = ai.current_z as i32;
            if chunk_map.passable_at(arrived_tx, arrived_ty, cz) {
                ai.current_z = cz as i8;
            } else if chunk_map.passable_at(arrived_tx, arrived_ty, cz + 1) {
                ai.current_z = (cz + 1) as i8;
            } else if chunk_map.passable_at(arrived_tx, arrived_ty, cz - 1) {
                ai.current_z = (cz - 1) as i8;
            } else {
                let prev_tx = (pos.x / TILE_SIZE).floor() as i32;
                let prev_ty = (pos.y / TILE_SIZE).floor() as i32;
                release_to_idle(&mut ai, (prev_tx, prev_ty));
                continue;
            }

            match ai.state {
                AiState::Seeking => {
                    // Arrived at task target — start working, unless another agent is here.
                    let tx = (target_world.x / TILE_SIZE).floor() as i32;
                    let ty = (target_world.y / TILE_SIZE).floor() as i32;
                    let cz = ai.current_z as i32;

                    // Was this agent already here at the start of the frame?
                    // (prevents self-nudging from the static spatial index)
                    let was_here = (pos.x / TILE_SIZE).floor() as i32 == tx
                        && (pos.y / TILE_SIZE).floor() as i32 == ty;
                    let already_taken = claimed_this_tick.contains(&(tx, ty, cz));
                    let count_limit = if was_here { 1 } else { 0 };

                    if already_taken || spatial_index.agent_count(tx, ty, cz) > count_limit {
                        // Nudge to an adjacent free tile and stay Seeking.
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                        ];
                        let mut rng = rand::thread_rng();
                        let start = rng.gen_range(0..8);
                        let mut bumped = false;
                        for i in 0..8usize {
                            let (dx, dy) = dirs[(start + i) % 8];
                            let (ntx, nty) = (tx + dx, ty + dy);
                            // Try same-Z, then Z+1 (ramp up), then Z-1 (ramp down).
                            for &dz in &[0, 1, -1] {
                                let ntz = cz + dz;
                                if chunk_map.passable_step_3d((tx, ty, cz), (ntx, nty, ntz))
                                    && !spatial_index.agent_occupied(ntx, nty, ntz)
                                    && !claimed_this_tick.contains(&(ntx, nty, ntz))
                                {
                                    ai.target_tile = (ntx as i16, nty as i16);
                                    bumped = true;
                                    break;
                                }
                            }
                            if bumped {
                                break;
                            }
                        }
                        if !bumped {
                            ai.state = AiState::Working;
                        }
                        // else: stays Seeking toward the adjacent tile
                    } else {
                        claimed_this_tick.insert((tx, ty, cz));
                        ai.state = AiState::Working;
                    }
                }
                AiState::Working => {
                    // Production system handles output; only accumulate progress when bucket is active.
                    if clock.is_active(slot.0) {
                        let progress = (sim_dt * 20.0).max(0.0) as u8;
                        ai.work_progress = ai.work_progress.saturating_add(progress);
                    }
                }
                AiState::Idle => {
                    // Random wander, with 35% chance to drift toward the most-liked nearby friend.
                    mv.wander_timer -= dt * speed;
                    if mv.wander_timer <= 0.0 {
                        mv.wander_timer = IDLE_WANDER_INTERVAL;

                        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
                        let cur_z = ai.current_z as i32;

                        // Try to step toward a liked friend (35% chance per wander tick).
                        let mut drifted = false;
                        if let Some(rel) = rel_opt {
                            if fastrand::f32() < 0.35 {
                                let mut best_aff: i8 = 0;
                                let mut best_dir: Option<(i32, i32)> = None;
                                for slot in &rel.entries {
                                    if let Some(entry) = slot {
                                        if entry.affinity <= 0 {
                                            continue;
                                        }
                                        'scan: for dy in -10i32..=10 {
                                            for dx in -10i32..=10 {
                                                for &cand in
                                                    spatial_index.get(cur_tx + dx, cur_ty + dy)
                                                {
                                                    if cand == entry.entity
                                                        && entry.affinity > best_aff
                                                    {
                                                        best_aff = entry.affinity;
                                                        best_dir =
                                                            Some((dx.signum(), dy.signum()));
                                                        break 'scan;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some((dx, dy)) = best_dir {
                                    let ntx = cur_tx + dx;
                                    let nty = cur_ty + dy;
                                    for &dz in &[0, 1, -1] {
                                        let ntz = cur_z + dz;
                                        if chunk_map.passable_step_3d(
                                            (cur_tx, cur_ty, cur_z),
                                            (ntx, nty, ntz),
                                        ) && !spatial_index.agent_occupied(ntx, nty, ntz)
                                        {
                                            ai.target_tile = (ntx as i16, nty as i16);
                                            ai.dest_tile = ai.target_tile;
                                            drifted = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }

                        if !drifted {
                            let mut rng = rand::thread_rng();
                            let dirs: [(i32, i32); 8] = [
                                (-1, 0),
                                (1, 0),
                                (0, -1),
                                (0, 1),
                                (-1, -1),
                                (1, -1),
                                (-1, 1),
                                (1, 1),
                            ];
                            let candidates: Vec<_> = dirs.iter().collect();
                            let start = rng.gen_range(0..8);
                            let (left, right) = candidates.split_at(start);
                            let shuffled: Vec<_> = right.iter().chain(left.iter()).collect();

                            'outer: for &&(dx, dy) in &shuffled {
                                let ntx = cur_tx + dx;
                                let nty = cur_ty + dy;
                                for &dz in &[0, 1, -1] {
                                    let ntz = cur_z + dz;
                                    if chunk_map.passable_step_3d(
                                        (cur_tx, cur_ty, cur_z),
                                        (ntx, nty, ntz),
                                    ) && !spatial_index.agent_occupied(ntx, nty, ntz)
                                    {
                                        ai.target_tile = (ntx as i16, nty as i16);
                                        ai.dest_tile = ai.target_tile;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                }
                AiState::Sleeping | AiState::Attacking => {}
                AiState::Routing => {
                    // Arrived at a chunk-border waypoint; advance to next waypoint
                    // or switch to Seeking once we're in the destination chunk.
                    let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                    let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                    let dest_chunk = ChunkCoord(
                        (ai.dest_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
                        (ai.dest_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
                    );
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );

                    if cur_chunk == dest_chunk {
                        ai.state = AiState::Seeking;
                        ai.target_tile = ai.dest_tile;
                    } else if let Some(next_wp) = chunk_router.first_waypoint(
                        &chunk_graph,
                        cur_chunk,
                        dest_chunk,
                        ai.current_z,
                    ) {
                        ai.target_tile = next_wp;
                    } else {
                        // No route found — try to head toward destination anyway
                        ai.state = AiState::Seeking;
                        ai.target_tile = ai.dest_tile;
                    }
                }
            }
        }
    }
}

pub fn update_spatial_index_system(
    mut index: ResMut<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    query: Query<
        (
            Entity,
            &Transform,
            Option<&Health>,
            Option<&Body>,
            Option<&PersonAI>,
            Has<Person>,
            Has<Wolf>,
            Has<Deer>,
            Has<Horse>,
        ),
        Or<(
            With<Person>,
            With<Wolf>,
            With<Deer>,
            With<Horse>,
            With<Plant>,
            With<GroundItem>,
            With<Bed>,
        )>,
    >,
) {
    index.map.clear();
    index.agent_counts.clear();
    for (entity, transform, health, body, person_ai, is_person, is_wolf, is_deer, is_horse) in &query {
        let is_dead = health.map_or(false, |h| h.is_dead()) || body.map_or(false, |b| b.is_dead());
        if is_dead {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        index.insert(tx, ty, entity);

        if is_person || is_wolf || is_deer || is_horse {
            // Persons track their own Z (may be in a tunnel below surface);
            // animals always live at surface_z.
            let tz = match person_ai {
                Some(ai) if is_person => ai.current_z as i32,
                _ => chunk_map.surface_z_at(tx, ty),
            };
            *index.agent_counts.entry((tx, ty, tz)).or_insert(0) += 1;
        }
    }
}

/// Remove MountedOn/CarriedBy when a rider arrives, idles, or their horse is gone.
pub fn dismount_system(
    mut commands: Commands,
    query: Query<(Entity, &PersonAI, &MountedOn), With<Person>>,
    horse_exists: Query<(), With<Horse>>,
) {
    for (person_entity, ai, mounted_on) in query.iter() {
        let should_dismount =
            matches!(ai.state, AiState::Working | AiState::Sleeping | AiState::Idle)
                || horse_exists.get(mounted_on.0).is_err();

        if should_dismount {
            commands.entity(person_entity).remove::<MountedOn>();
            if horse_exists.get(mounted_on.0).is_ok() {
                commands.entity(mounted_on.0).remove::<CarriedBy>();
            }
        }
    }
}

/// Automatically mount a nearby tamed faction horse when traveling a long distance.
/// Requires HORSEBACK_RIDING tech. Runs after dismount_system and update_spatial_index_system.
pub fn mount_check_system(
    mut commands: Commands,
    faction_registry: Res<FactionRegistry>,
    spatial: Res<SpatialIndex>,
    person_query: Query<
        (Entity, &Transform, &PersonAI, &FactionMember, &LodLevel),
        (With<Person>, Without<MountedOn>),
    >,
    horse_query: Query<(Entity, &Tamed), (With<Horse>, Without<CarriedBy>)>,
) {
    const MOUNT_SCAN_RADIUS: i32 = 2;
    const MOUNT_MIN_DIST: i32 = 8;

    for (person_entity, transform, ai, member, lod) in person_query.iter() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(ai.state, AiState::Seeking | AiState::Routing) {
            continue;
        }

        let has_riding = faction_registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.techs.has(HORSEBACK_RIDING))
            .unwrap_or(false);
        if !has_riding {
            continue;
        }

        let person_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let person_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let dest_dist = (ai.dest_tile.0 as i32 - person_tx).abs()
            + (ai.dest_tile.1 as i32 - person_ty).abs();
        if dest_dist < MOUNT_MIN_DIST {
            continue;
        }

        let mut found_horse = None;
        'outer: for dy in -MOUNT_SCAN_RADIUS..=MOUNT_SCAN_RADIUS {
            for dx in -MOUNT_SCAN_RADIUS..=MOUNT_SCAN_RADIUS {
                for &candidate in spatial.get(person_tx + dx, person_ty + dy) {
                    if let Ok((horse_entity, tamed)) = horse_query.get(candidate) {
                        if tamed.owner_faction == member.faction_id {
                            found_horse = Some(horse_entity);
                            break 'outer;
                        }
                    }
                }
            }
        }

        if let Some(horse_entity) = found_horse {
            commands.entity(person_entity).insert(MountedOn(horse_entity));
            commands.entity(horse_entity).insert(CarriedBy(person_entity));
        }
    }
}

/// Sync the horse's position to the rider each frame while mounted.
pub fn horse_position_sync_system(
    rider_query: Query<(&Transform, &MountedOn), With<Person>>,
    mut horse_query: Query<&mut Transform, (With<Horse>, Without<Person>)>,
) {
    for (rider_transform, mounted_on) in rider_query.iter() {
        if let Ok(mut horse_transform) = horse_query.get_mut(mounted_on.0) {
            horse_transform.translation.x = rider_transform.translation.x;
            horse_transform.translation.y = rider_transform.translation.y;
        }
    }
}
