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
use super::tasks::{task_interacts_from_adjacent, TaskKind};
use super::technology::HORSEBACK_RIDING;
use crate::pathfinding::path_request::{
    cooldown_for_streak, FollowStatus, PathDebugFlags, PathFollow, PathKind, PathRequestQueue,
    DEFAULT_PATH_BUDGET,
};
use crate::pathfinding::tile_cost::{furniture_speed_factor, tile_speed_multiplier};
use crate::pathfinding::worker::PathfindingDiagnostics;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use ahash::AHashSet;
use bevy::prelude::*;
use rand::Rng;

const MOVE_SPEED: f32 = 48.0; // pixels per second
const MOUNTED_SPEED: f32 = 80.0; // speed when riding a horse
const IDLE_WANDER_INTERVAL: f32 = 2.5; // seconds between random moves

#[derive(Component, Default)]
pub struct MovementState {
    pub wander_timer: f32,
}

/// Placed on a person while they are mounted on a horse.
#[derive(Component, Clone, Copy)]
pub struct MountedOn(pub Entity);

/// Release an agent stuck at a tile that the movement system cannot proceed
/// from (wall block, no standable Z) so the dispatch and plan systems will
/// pick them up next tick. Without clearing `task_id` and `target_tile`, the
/// agent stays Idle but every tick re-walks toward the unreachable target,
/// hits the same obstacle, and drops to Idle again forever.
///
/// Also resets `PathFollow` and snaps the agent's pixel position to the tile
/// center: leaving `pf.status = Following` with a stale `segment_path` makes
/// the debug overlay keep drawing on a frozen agent, and leaving the agent
/// off-center traps them in the `dist > 2.0 && target_tile == current_tile`
/// loop where the worker returns an empty path and the agent never moves.
fn release_to_idle(
    ai: &mut PersonAI,
    pf: &mut PathFollow,
    transform: &mut Transform,
    here: (i32, i32),
) {
    ai.state = AiState::Idle;
    ai.task_id = PersonAI::UNEMPLOYED;
    ai.target_tile = (here.0 as i16, here.1 as i16);
    ai.dest_tile = ai.target_tile;
    ai.target_entity = None;

    pf.status = FollowStatus::Idle;
    pf.segment_path.clear();
    pf.chunk_route.clear();
    pf.segment_cursor = 0;
    pf.route_cursor = 0;
    pf.goal = (here.0, here.1, ai.current_z);

    let center = tile_to_world(here.0, here.1);
    transform.translation.x = center.x;
    transform.translation.y = center.y;
}

pub fn movement_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    spatial_index: Res<SpatialIndex>,
    bed_map: Res<BedMap>,
    chair_map: Res<ChairMap>,
    table_map: Res<TableMap>,
    workbench_map: Res<WorkbenchMap>,
    loom_map: Res<LoomMap>,
    mut path_queue: ResMut<PathRequestQueue>,
    path_flags: Res<PathDebugFlags>,
    mut path_diag: ResMut<PathfindingDiagnostics>,
    mut claimed_this_tick: Local<AHashSet<(i32, i32, i32)>>,
    mut query: Query<(
        Entity,
        &mut Transform,
        &mut PersonAI,
        &LodLevel,
        &mut MovementState,
        &mut PathFollow,
        &BucketSlot,
        Option<&RelationshipMemory>,
        Option<&MountedOn>,
    )>,
) {
    let dt = time.delta_secs();
    let speed = clock.speed;
    let sim_dt = dt * clock.scale_factor();
    let now_ms = time.elapsed().as_millis() as u64;

    claimed_this_tick.clear();

    // Movement can't be fully parallel because it writes Transform (position sync)
    // and can read ChunkMap for passability. Run sequentially.
    for (entity, mut transform, mut ai, lod, mut mv, mut pf, slot, rel_opt, mounted_opt) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }

        // Snap to tile center if pixel position drifted off-center while the
        // agent's `target_tile` is their own tile. Without this the agent
        // re-enters the `dist > 2.0` block, enqueues a path with start==goal,
        // gets back an empty path, clears to Idle, and loops forever — every
        // agent that ever hit `release_to_idle` ends up frozen at a tile edge.
        let cur_tx0 = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty0 = (transform.translation.y / TILE_SIZE).floor() as i32;
        if (ai.target_tile.0 as i32, ai.target_tile.1 as i32) == (cur_tx0, cur_ty0) {
            let center = tile_to_world(cur_tx0, cur_ty0);
            transform.translation.x = center.x;
            transform.translation.y = center.y;
        }

        // PathFollow handles cross-chunk traversal. `target_tile` is the
        // path goal as set by `assign_task_with_routing`: for interact-
        // from-adjacent tasks it's a passable tile next to `dest_tile`,
        // otherwise it's `dest_tile` itself. Don't clobber it here —
        // pathing to an impassable `dest_tile` returns Unreachable from
        // every adjacent tile and starves the agent. Treat Routing
        // identically to Seeking; the Routing arm of the arrival match
        // is a no-op (kept only for callers that still set the variant).

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

            // Pick the immediate step target via PathFollow. The path worker
            // (PreUpdate) populates `pf.segment_path` from PathRequestQueue
            // entries we enqueue here; while the request is in flight the
            // agent stays put rather than twitching toward the goal.
            let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
            let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
            // Re-snap target_z if the goal Z drifted off a standable slice.
            // target_tile can be updated mid-flight (Routing→Seeking, stranded
            // recovery, dispatch fall-throughs) without target_z being kept in
            // sync, leaving goal3 pointing into mid-air. A* would burn budget
            // failing every retry until cooldown. Idempotent when valid; skip
            // for Craft, which overloads target_z as a recipe id.
            if ai.task_id != TaskKind::Craft as u16
                && !chunk_map.passable_at(
                    ai.target_tile.0 as i32,
                    ai.target_tile.1 as i32,
                    ai.target_z as i32,
                )
            {
                ai.target_z = chunk_map.nearest_standable_z(
                    ai.target_tile.0 as i32,
                    ai.target_tile.1 as i32,
                    ai.current_z as i32,
                ) as i8;
            }
            let goal3 = (
                ai.target_tile.0 as i32,
                ai.target_tile.1 as i32,
                ai.target_z,
            );
            let cur3 = (cur_tx, cur_ty, ai.current_z);

            // The match returns both the world-space step target and the
            // planner's intended next tile (x, y, z). The planned Z is
            // carried out so the boundary-cross block below can validate
            // the move with the same `passable_step_3d` rule A* used.
            let (step_world, planned_step): (Vec2, (i32, i32, i32)) = match pf.status {
                FollowStatus::Failed(_) => {
                    // Worker rejected the request; release so dispatch picks
                    // a different goal. Re-requesting the same goal would
                    // just fail again.
                    release_to_idle(&mut ai, &mut pf, &mut transform, (cur_tx, cur_ty));
                    continue;
                }
                FollowStatus::Pending => {
                    // Worker hasn't run yet — hold position to avoid
                    // twitching toward a goal we don't have a path to yet.
                    continue;
                }
                FollowStatus::Idle => {
                    // Cooldown gate: if this exact goal just failed for this
                    // agent and the cooldown hasn't elapsed, drop the task
                    // back to dispatch instead of re-enqueueing the same
                    // request that will fail again. Without this an agent
                    // assigned an unreachable target loops at the per-tick
                    // budget forever.
                    if pf.last_fail_goal == goal3
                        && now_ms.saturating_sub(pf.last_fail_tick)
                            < cooldown_for_streak(pf.last_fail_streak)
                    {
                        path_diag.path_request_skipped_cooldown += 1;
                        release_to_idle(&mut ai, &mut pf, &mut transform, (cur_tx, cur_ty));
                        continue;
                    }
                    path_queue.enqueue(
                        entity,
                        cur3,
                        goal3,
                        PathKind::BestEffort,
                        DEFAULT_PATH_BUDGET,
                        ai.task_id,
                        ai.last_plan_id,
                    );
                    pf.status = FollowStatus::Pending;
                    pf.goal = goal3;
                    continue;
                }
                FollowStatus::Following => {
                    // Stuck-tick heartbeat: if the agent's tile hasn't changed
                    // since last frame, count up. ~30 ticks (1.5 s @ 20 Hz)
                    // means we're wedged — drop the path so the Idle arm
                    // re-enqueues a fresh request next tick. Catches cases the
                    // hard-wall guard below misses (another agent camping the
                    // next tile, sub-tile oscillation, stale segment_path).
                    //
                    // Skip the heartbeat entirely when the game is paused
                    // (`speed == 0.0`): paused agents can't move, so otherwise
                    // every paused tick would count as "stuck" and trip the
                    // limit ~1.5 s after the user clicks ⏸ — destroying the
                    // diagnostic state they paused to inspect.
                    const STUCK_LIMIT: u8 = 30;
                    let here = (cur_tx as i16, cur_ty as i16, ai.current_z);
                    if speed > 0.0 {
                        if pf.recent_tiles[0] == here {
                            pf.stuck_ticks = pf.stuck_ticks.saturating_add(1);
                        } else {
                            pf.recent_tiles[0] = here;
                            pf.stuck_ticks = 0;
                        }
                    }
                    if pf.stuck_ticks >= STUCK_LIMIT {
                        if path_flags.verbose_logs {
                            debug!(
                                "[path] stuck-tick clear agent={:?} at=({},{},{}) goal={:?}",
                                entity, cur_tx, cur_ty, ai.current_z, pf.goal
                            );
                        }
                        pf.segment_path.clear();
                        pf.chunk_route.clear();
                        pf.segment_cursor = 0;
                        pf.route_cursor = 0;
                        pf.status = FollowStatus::Idle;
                        pf.stuck_ticks = 0;
                        pf.recent_tiles[0] = (i16::MIN, i16::MIN, 0);
                        continue;
                    }

                    // If the goal moved (new task / new target), force replan.
                    if pf.goal != goal3 {
                        if pf.last_fail_goal == goal3
                            && now_ms.saturating_sub(pf.last_fail_tick)
                                < cooldown_for_streak(pf.last_fail_streak)
                        {
                            path_diag.path_request_skipped_cooldown += 1;
                            release_to_idle(&mut ai, &mut pf, &mut transform, (cur_tx, cur_ty));
                            continue;
                        }
                        path_queue.enqueue(
                            entity,
                            cur3,
                            goal3,
                            PathKind::BestEffort,
                            DEFAULT_PATH_BUDGET,
                            ai.task_id,
                            ai.last_plan_id,
                        );
                        pf.status = FollowStatus::Pending;
                        pf.goal = goal3;
                        continue;
                    }
                    // Advance the segment cursor past tiles we've already
                    // crossed (sim_dt may consume multiple short steps in
                    // one tick).
                    while (pf.segment_cursor as usize) < pf.segment_path.len() {
                        let (sx, sy, sz) = pf.segment_path[pf.segment_cursor as usize];
                        // Only treat a step as consumed when both xy AND the
                        // planner's Z match — otherwise a planned ramp climb
                        // (same xy, different z) gets skipped.
                        if (sx as i32, sy as i32, sz) == (cur_tx, cur_ty, ai.current_z) {
                            pf.segment_cursor += 1;
                            continue;
                        }
                        break;
                    }
                    if (pf.segment_cursor as usize) >= pf.segment_path.len() {
                        // Segment consumed. If we're at the goal tile, the
                        // arrival branch below will pick this up next tick;
                        // otherwise we need the next segment of the route.
                        if (cur_tx, cur_ty) == (goal3.0, goal3.1) {
                            pf.status = FollowStatus::Idle;
                            pf.segment_path.clear();
                            continue;
                        }
                        path_queue.enqueue(
                            entity,
                            cur3,
                            goal3,
                            PathKind::BestEffort,
                            DEFAULT_PATH_BUDGET,
                            ai.task_id,
                            ai.last_plan_id,
                        );
                        pf.status = FollowStatus::Pending;
                        continue;
                    }
                    let (sx, sy, sz) = pf.segment_path[pf.segment_cursor as usize];
                    // Step-continuity check: the planner emits each path
                    // node as a single passable_step_3d hop from the
                    // previous node. If the agent's live current_z drifted
                    // off the planner's track (e.g. a diagonal-ramp
                    // rounding picked cz±1 on an intermediate cell), the
                    // next planned tile may now be |Δz| ≥ 2 from where we
                    // actually stand. That's the symptom the user reports
                    // as a "2-z wall": the boundary block below would
                    // reject every candidate. Detect it here, drop the
                    // segment, and re-request from the agent's true
                    // (xy, current_z).
                    let cur3i = (cur_tx, cur_ty, ai.current_z as i32);
                    let next3i = (sx as i32, sy as i32, sz as i32);
                    if next3i != cur3i && !chunk_map.passable_step_3d(cur3i, next3i) {
                        path_diag.path_drift_rejections_total += 1;
                        let center = tile_to_world(cur_tx, cur_ty);
                        transform.translation.x = center.x;
                        transform.translation.y = center.y;
                        pf.status = FollowStatus::Idle;
                        pf.segment_path.clear();
                        pf.chunk_route.clear();
                        pf.segment_cursor = 0;
                        pf.route_cursor = 0;
                        continue;
                    }
                    (
                        tile_to_world(sx as i32, sy as i32),
                        (sx as i32, sy as i32, sz as i32),
                    )
                }
            };

            let to_step = step_world - pos;
            let step_len = to_step.length();
            if step_len < 0.001 {
                continue;
            }
            let dir = to_step / step_len;
            let mut effective_speed = if mounted_opt.is_some() {
                MOUNTED_SPEED
            } else {
                MOVE_SPEED
            };
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
            // Overshoot arrival: when the smooth step would reach or pass the
            // final target tile this tick, clamp to target_world and fall
            // through to the arrival branch. Without this, agents within
            // ~2 px of target snap the last pixels in one frame (visible pop).
            // Restricted to the final segment via step_world == target_world
            // so intermediate path tiles never trigger arrival.
            let step_len = step.length();
            let is_final_segment = step_world == target_world;
            let arrived_this_step = is_final_segment && step_len >= dist;
            let new_pos = if arrived_this_step {
                target_world
            } else {
                pos + step
            };

            // Validate boundary crossings with the same Z tolerance A* used
            // when expanding neighbours (astar.rs:90 — dz ∈ {0, +1, −1}).
            // For diagonal+ramp steps, per-frame pixel motion rounds across
            // one axis a frame before the other; the intermediate axis-
            // aligned cell is rarely standable at cz alone, so a strict
            // `target_z = cz` fallback rejects routine ramp-up steps and
            // snaps the agent back to the previous tile.
            let prev_tx = cur_tx;
            let prev_ty = cur_ty;
            let new_tx = (new_pos.x / TILE_SIZE).floor() as i32;
            let new_ty = (new_pos.y / TILE_SIZE).floor() as i32;
            let crossing_boundary = new_tx != prev_tx || new_ty != prev_ty;
            let cz = ai.current_z as i32;
            if crossing_boundary {
                let (px, py, pz) = planned_step;
                // Z candidates in priority order. When entering the planner's
                // intended cell, prefer pz so a planned ramp climb actually
                // updates current_z. When rounding into the off-planned
                // intermediate cell of a diagonal step, prefer cz then ±1.
                let candidates: [i32; 3] = if px == new_tx && py == new_ty {
                    [pz, cz + 1, cz - 1]
                } else {
                    [cz, cz + 1, cz - 1]
                };
                let mut chosen: Option<i32> = None;
                for tz in candidates {
                    if chunk_map.passable_step_3d((cur_tx, cur_ty, cz), (new_tx, new_ty, tz)) {
                        chosen = Some(tz);
                        break;
                    }
                }
                let Some(target_z) = chosen else {
                    // Path is stale (world changed under us, planner/runtime
                    // disagree). Drop the segment and re-request next tick;
                    // the cooldown gate above prevents runaway re-requests
                    // if the goal is genuinely unreachable.
                    path_diag.boundary_rejections_per_tick += 1;
                    path_diag.boundary_rejections_total += 1;
                    let center = tile_to_world(cur_tx, cur_ty);
                    transform.translation.x = center.x;
                    transform.translation.y = center.y;
                    pf.status = FollowStatus::Idle;
                    pf.segment_path.clear();
                    pf.chunk_route.clear();
                    pf.segment_cursor = 0;
                    pf.route_cursor = 0;
                    continue;
                };
                ai.current_z = target_z as i8;
            }

            transform.translation.x = new_pos.x;
            transform.translation.y = new_pos.y;
            if !arrived_this_step {
                continue;
            }
        }
        // Arrived at target — clamp transform exactly to target_world.
        // No-op when a smooth step already landed here; required when
        // dist <= 2.0 falls through without taking a step (e.g. agent
        // teleported or already at goal at frame start).
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
            release_to_idle(&mut ai, &mut pf, &mut transform, (prev_tx, prev_ty));
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
                                            for &cand in spatial_index.get(cur_tx + dx, cur_ty + dy)
                                            {
                                                if cand == entry.entity && entry.affinity > best_aff
                                                {
                                                    best_aff = entry.affinity;
                                                    best_dir = Some((dx.signum(), dy.signum()));
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
                                    if chunk_map
                                        .passable_step_3d((cur_tx, cur_ty, cur_z), (ntx, nty, ntz))
                                        && !spatial_index.agent_occupied(ntx, nty, ntz)
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
                                if chunk_map
                                    .passable_step_3d((cur_tx, cur_ty, cur_z), (ntx, nty, ntz))
                                    && !spatial_index.agent_occupied(ntx, nty, ntz)
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
                // PathFollow handles cross-chunk routing transparently;
                // arriving at target_tile (= dest_tile) means we are at
                // the final destination. Promote to Seeking so the
                // adjacent-tile claim logic runs next tick.
                ai.state = AiState::Seeking;
                ai.target_tile = ai.dest_tile;
                if ai.task_id != TaskKind::Craft as u16 {
                    ai.target_z = chunk_map.nearest_standable_z(
                        ai.target_tile.0 as i32,
                        ai.target_tile.1 as i32,
                        ai.current_z as i32,
                    ) as i8;
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
    for (entity, transform, health, body, person_ai, is_person, is_wolf, is_deer, is_horse) in
        &query
    {
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
        let should_dismount = matches!(
            ai.state,
            AiState::Working | AiState::Sleeping | AiState::Idle
        ) || horse_exists.get(mounted_on.0).is_err();

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
        let dest_dist =
            (ai.dest_tile.0 as i32 - person_tx).abs() + (ai.dest_tile.1 as i32 - person_ty).abs();
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
            commands
                .entity(person_entity)
                .insert(MountedOn(horse_entity));
            commands
                .entity(horse_entity)
                .insert(CarriedBy(person_entity));
        }
    }
}

/// Detect agents whose `current_z` is no longer standable (e.g. a wall was
/// built under them, or a tile was carved out from under their feet) and
/// snap them to `nearest_standable_z`. Without this, an agent stranded
/// more than ±1 z from any standable tile cannot recover via A* (every
/// step requires `|Δz| ≤ 1` and a standable foot tile), goes Idle, then
/// re-requests the same impossible path forever.
///
/// Runs after `movement_system` and before `update_spatial_index_system`
/// so the index sees the corrected coordinates this tick.
pub fn recover_stranded_agents_system(
    chunk_map: Res<ChunkMap>,
    mut query: Query<
        (
            Entity,
            &mut Transform,
            &mut PersonAI,
            &LodLevel,
            &mut PathFollow,
        ),
        (With<Person>, Without<MountedOn>),
    >,
) {
    for (entity, mut transform, mut ai, lod, mut pf) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cz = ai.current_z as i32;
        if chunk_map.passable_at(tx, ty, cz) {
            continue;
        }
        let new_z = chunk_map.nearest_standable_z(tx, ty, cz);
        if new_z == cz || !chunk_map.passable_at(tx, ty, new_z) {
            // Either nothing better was found or the surface_z fallback is
            // also non-standable (column entirely solid). Leave the agent
            // for the next tick — terrain may still be settling.
            continue;
        }
        debug!(
            "[recovery] {:?} at ({},{}) z={} not standable; snapping to z={}",
            entity, tx, ty, cz, new_z
        );
        ai.current_z = new_z as i8;
        ai.target_z = new_z as i8;
        release_to_idle(&mut ai, &mut pf, &mut transform, (tx, ty));
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
