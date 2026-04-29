use bevy::prelude::*;
use std::time::Instant;

use crate::pathfinding::astar::{find_path_in, AStarResult};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::flow_field::walk_to_goal;
use crate::pathfinding::hotspots::HotspotFlowFields;
use crate::pathfinding::path_request::{
    FailReason, FailureLog, FailureRecord, FollowStatus, PathDebugFlags, PathFailed, PathFollow,
    PathKind, PathReady, PathReadyKind, PathRequest, PathRequestQueue,
};
use crate::pathfinding::pool::AStarPool;
use crate::pathfinding::step::passable_diagonal_step;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};

/// Maximum number of `PathRequest`s the worker drains in one tick. Sized
/// to keep worker time well under 4 ms with 2k agents (target in plan):
/// at ~50 µs per A* segment that's ~3 ms worst case.
pub const PATH_BUDGET_PER_TICK: usize = 64;

/// Cap on chunk-route length. A route longer than this is pathological —
/// the worker reports `NoRoute` rather than spending time fanning out.
const MAX_CHUNK_ROUTE: usize = 64;

/// Per-tick pathfinding telemetry, surfaced in the debug panel.
/// Counters that are listed in the plan as "running totals" stay across
/// ticks; tick-local counters are reset at the start of each drain.
#[derive(Resource, Default, Debug)]
pub struct PathfindingDiagnostics {
    pub paths_dispatched_per_tick: u32,
    pub worker_us_per_tick: u32,
    pub queue_len: u32,
    pub astar_calls_per_tick: u32,
    pub connectivity_rejections_per_tick: u32,
    /// Total `Unreachable` failures, regardless of which gate fired.
    /// Backwards-compatible umbrella counter.
    pub path_failed_unreachable: u64,
    /// Subset of `path_failed_unreachable`: rejected by the chunk-level
    /// connectivity check before any A* ran.
    pub path_failed_unreachable_connectivity: u64,
    /// Subset of `path_failed_unreachable`: A* finished with `Unreachable`
    /// after the connectivity check let the request through.
    pub path_failed_unreachable_astar: u64,
    pub path_failed_budget: u64,
    pub path_failed_no_route: u64,
    /// Times `movement_system` skipped enqueueing a request because the
    /// agent's `last_fail_goal` matches the current goal and the cooldown
    /// hasn't expired yet. Running total — not reset per tick.
    pub path_request_skipped_cooldown: u64,
    pub paths_ready_strict: u64,
    pub paths_ready_best_effort: u64,
    pub flow_field_hits_per_tick: u32,
    pub flow_field_hits_total: u64,
    /// Sum of A* node expansions across all calls in the last tick.
    pub astar_iters_last_tick: u32,
    /// Largest single-call expansion count observed in the last tick.
    pub astar_iters_max_single: u32,
    /// Hotspot field existed for the goal but `walk_to_goal` returned None,
    /// forcing a fallthrough to A*. Running total.
    pub hotspot_fastpath_misses: u64,
    /// `PathReady`/`PathFailed` arrived for an outdated `request_id`.
    /// Running total.
    pub stale_id_discards: u64,
    /// `write_failure`/`write_success` couldn't find a `PathFollow` on the
    /// agent — either despawned or never had the component.
    pub missing_follow_on_event: u64,
    /// `movement_system` rejected a tile-boundary crossing because no Z
    /// in {pz, cz, cz±1} was standable at the destination cell. Indicates
    /// genuine path staleness (wall built mid-walk, dig event invalidated
    /// the route) — non-zero values are worth investigating.
    pub boundary_rejections_per_tick: u32,
    pub boundary_rejections_total: u64,
    /// Producer-side: a segment_path returned by A* / flow-field had a
    /// consecutive pair that failed `passable_step_3d` (e.g. |Δz| > 1).
    /// Should stay 0 in healthy runs — non-zero indicates a planner bug.
    pub path_failed_step_continuity: u64,
    /// Consumer-side: movement saw the next segment tile fail
    /// `passable_step_3d` against the agent's live `(cur_xy, current_z)`.
    /// Indicates the agent's z drifted off the planner's intended track
    /// (e.g. diagonal-ramp rounding) — the path becomes effectively a
    /// "2-z wall" from the agent's actual position.
    pub path_drift_rejections_total: u64,
}

impl PathfindingDiagnostics {
    fn reset_per_tick(&mut self) {
        self.paths_dispatched_per_tick = 0;
        self.worker_us_per_tick = 0;
        self.astar_calls_per_tick = 0;
        self.connectivity_rejections_per_tick = 0;
        self.flow_field_hits_per_tick = 0;
        self.astar_iters_last_tick = 0;
        self.astar_iters_max_single = 0;
        self.boundary_rejections_per_tick = 0;
    }
}

fn chunk_of(tile: (i32, i32, i8)) -> ChunkCoord {
    ChunkCoord(
        tile.0.div_euclid(CHUNK_SIZE as i32),
        tile.1.div_euclid(CHUNK_SIZE as i32),
    )
}

/// Walks the segment path (prefixed by `start`) and verifies every
/// consecutive pair is a single passable step under `passable_step_3d`
/// (|Δxy| ≤ 1 cardinal/diagonal, |Δz| ≤ 1, destination standable). On
/// failure returns `Some(index)` of the offending pair (0 means the
/// `start → path[0]` step). This is the same rule the A* expansion and
/// the movement boundary check use, so a violation here means a planner
/// emitted a path that movement cannot legally execute.
fn first_invalid_step(
    chunk_map: &ChunkMap,
    start: (i32, i32, i8),
    path: &[(i16, i16, i8)],
) -> Option<usize> {
    let mut prev = start;
    for (i, &(x, y, z)) in path.iter().enumerate() {
        let next = (x as i32, y as i32, z);
        if next != prev {
            let from = (prev.0, prev.1, prev.2 as i32);
            let to = (next.0, next.1, next.2 as i32);
            let dx = (to.0 - from.0).abs();
            let dy = (to.1 - from.1).abs();
            let ok = if dx == 1 && dy == 1 {
                passable_diagonal_step(chunk_map, from, to)
            } else {
                chunk_map.passable_step_3d(from, to)
            };
            if !ok {
                return Some(i);
            }
        }
        prev = next;
    }
    None
}

/// Drains up to `PATH_BUDGET_PER_TICK` requests from the queue and writes
/// `PathFollow` for the requesting agents. Runs in `PreUpdate` — that way
/// any system that consumes `PathFollow` later in the same tick (movement
/// after step (f)) sees the freshly-computed result.
pub fn drain_path_requests_system(
    mut queue: ResMut<PathRequestQueue>,
    chunk_map: Res<ChunkMap>,
    graph: Res<ChunkGraph>,
    router: Res<ChunkRouter>,
    conn: Res<ChunkConnectivity>,
    hotspots: Res<HotspotFlowFields>,
    flags: Res<PathDebugFlags>,
    mut pool: ResMut<AStarPool>,
    mut diag: ResMut<PathfindingDiagnostics>,
    mut failure_log: ResMut<FailureLog>,
    tick: Res<bevy::prelude::Time>,
    mut follows: Query<&mut PathFollow>,
    mut ready_w: EventWriter<PathReady>,
    mut failed_w: EventWriter<PathFailed>,
) {
    diag.reset_per_tick();
    diag.queue_len = queue.len() as u32;
    if flags.worker_paused {
        return;
    }
    let started = Instant::now();
    let conn_generation = conn.generation;
    let now_tick = tick.elapsed().as_millis() as u64;

    let mut processed: usize = 0;
    while processed < PATH_BUDGET_PER_TICK {
        let Some(req) = queue.pop() else { break };
        processed += 1;

        process_request(
            &req,
            &chunk_map,
            &graph,
            &router,
            &conn,
            &hotspots,
            &flags,
            conn_generation,
            now_tick,
            &mut pool,
            &mut diag,
            &mut failure_log,
            &mut follows,
            &mut ready_w,
            &mut failed_w,
        );
    }

    diag.paths_dispatched_per_tick = processed as u32;
    diag.queue_len = queue.len() as u32;
    diag.worker_us_per_tick = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
}

#[allow(clippy::too_many_arguments)]
fn process_request(
    req: &PathRequest,
    chunk_map: &ChunkMap,
    graph: &ChunkGraph,
    router: &ChunkRouter,
    conn: &ChunkConnectivity,
    hotspots: &HotspotFlowFields,
    flags: &PathDebugFlags,
    conn_generation: u32,
    now_tick: u64,
    pool: &mut AStarPool,
    diag: &mut PathfindingDiagnostics,
    failure_log: &mut FailureLog,
    follows: &mut Query<&mut PathFollow>,
    ready_w: &mut EventWriter<PathReady>,
    failed_w: &mut EventWriter<PathFailed>,
) {
    let start_chunk = chunk_of(req.start);
    let goal_chunk = chunk_of(req.goal);

    if !conn.is_reachable((start_chunk, req.start.2), (goal_chunk, req.goal.2)) {
        diag.connectivity_rejections_per_tick += 1;
        diag.path_failed_unreachable += 1;
        diag.path_failed_unreachable_connectivity += 1;
        if flags.verbose_logs {
            info!(
                "[path] connectivity reject agent={:?} start={:?} goal={:?}",
                req.agent, req.start, req.goal
            );
        }
        write_failure(req, FailReason::Unreachable, now_tick, follows, failed_w, failure_log, diag);
        return;
    }

    let chunk_route = match build_chunk_route(graph, router, start_chunk, goal_chunk, req.start.2) {
        Ok(r) => r,
        Err(reason) => {
            match reason {
                FailReason::Unreachable => diag.path_failed_unreachable += 1,
                FailReason::BudgetExhausted => diag.path_failed_budget += 1,
                FailReason::NoRoute => diag.path_failed_no_route += 1,
            }
            if flags.verbose_logs {
                info!(
                    "[path] chunk-route fail reason={:?} agent={:?} start_chunk={:?} goal_chunk={:?}",
                    reason, req.agent, start_chunk, goal_chunk
                );
            }
            write_failure(req, reason, now_tick, follows, failed_w, failure_log, diag);
            return;
        }
    };

    // Fast path: if the goal is in the start chunk and is a registered
    // hotspot whose flow field reached the agent's cell at the agent's
    // current Z, walk the precomputed field instead of running A*. The
    // per-cell Z check accepts paths that climb/descend a ramp inside the
    // chunk — the field's `goal_z` no longer needs to match `start.z`.
    // Falls through on miss / Z mismatch / unreachable cell.
    if chunk_route.len() == 1 {
        let goal_tile = (req.goal.0 as i16, req.goal.1 as i16, req.goal.2);
        if let Some(field) = hotspots.lookup_field(goal_tile) {
            let csz = CHUNK_SIZE as i32;
            let lx = (req.start.0 - field.chunk.0 * csz) as u8;
            let ly = (req.start.1 - field.chunk.1 * csz) as u8;
            let cell_idx = ly as usize * CHUNK_SIZE + lx as usize;
            if field.cell_z[cell_idx] == req.start.2 {
                if let Some(path) = walk_to_goal(field, (lx, ly)) {
                    if let Some(bad) = first_invalid_step(chunk_map, req.start, &path) {
                        diag.path_failed_step_continuity += 1;
                        diag.path_failed_no_route += 1;
                        if flags.verbose_logs {
                            let prev = if bad == 0 {
                                req.start
                            } else {
                                let p = path[bad - 1];
                                (p.0 as i32, p.1 as i32, p.2)
                            };
                            let cur = path[bad];
                            warn!(
                                "[path] flow-field emitted bad step agent={:?} prev={:?} -> {:?}",
                                req.agent, prev, cur
                            );
                        }
                        write_failure(req, FailReason::NoRoute, now_tick, follows, failed_w, failure_log, diag);
                        return;
                    }
                    diag.flow_field_hits_per_tick += 1;
                    diag.flow_field_hits_total += 1;
                    write_success(
                        req,
                        chunk_route,
                        path,
                        PathReadyKind::Strict,
                        conn_generation,
                        follows,
                        ready_w,
                        diag,
                    );
                    return;
                } else {
                    diag.hotspot_fastpath_misses += 1;
                    if flags.verbose_logs {
                        info!(
                            "[path] hotspot fastpath miss agent={:?} goal={:?} local=({},{})",
                            req.agent, goal_tile, lx, ly
                        );
                    }
                }
            }
        }
    }

    let segment_target = first_segment_target(graph, router, &chunk_route, req);

    let scratch = pool.scratch(1);
    diag.astar_calls_per_tick += 1;
    let (mut result, mut iters) = find_path_in(
        scratch,
        chunk_map,
        req.start,
        segment_target,
        req.max_budget as usize,
    );
    diag.astar_iters_last_tick = diag.astar_iters_last_tick.saturating_add(iters);
    if iters > diag.astar_iters_max_single {
        diag.astar_iters_max_single = iters;
    }

    if matches!(result, AStarResult::BudgetExhausted { .. }) {
        let scratch = pool.scratch(1);
        diag.astar_calls_per_tick += 1;
        let retry = find_path_in(
            scratch,
            chunk_map,
            req.start,
            segment_target,
            req.max_budget.saturating_mul(4) as usize,
        );
        result = retry.0;
        iters = retry.1;
        diag.astar_iters_last_tick = diag.astar_iters_last_tick.saturating_add(iters);
        if iters > diag.astar_iters_max_single {
            diag.astar_iters_max_single = iters;
        }
    }

    let (segment_path, ready_kind) = match result {
        AStarResult::Found(path) => {
            let converted: Vec<(i16, i16, i8)> = path
                .into_iter()
                .map(|(x, y, z)| (x as i16, y as i16, z))
                .collect();
            (converted, PathReadyKind::Strict)
        }
        AStarResult::BudgetExhausted { best_so_far } => {
            if matches!(req.kind, PathKind::Strict) {
                diag.path_failed_budget += 1;
                if flags.verbose_logs {
                    info!(
                        "[path] A* budget exhausted (strict) agent={:?} start={:?} target={:?}",
                        req.agent, req.start, segment_target
                    );
                }
                write_failure(req, FailReason::BudgetExhausted, now_tick, follows, failed_w, failure_log, diag);
                return;
            }
            // BestEffort: walk one tile toward `best_so_far`. We don't have
            // the full reverse-walk here (scratch is private to the call)
            // so the partial is just the single tile — caller re-requests
            // on arrival, exactly per the failure-recovery spec.
            (vec![(best_so_far.0 as i16, best_so_far.1 as i16, best_so_far.2)], PathReadyKind::BestEffort)
        }
        AStarResult::Unreachable => {
            diag.path_failed_unreachable += 1;
            diag.path_failed_unreachable_astar += 1;
            if flags.verbose_logs {
                info!(
                    "[path] A* unreachable agent={:?} start={:?} target={:?}",
                    req.agent, req.start, segment_target
                );
            }
            write_failure(req, FailReason::Unreachable, now_tick, follows, failed_w, failure_log, diag);
            return;
        }
    };

    if let Some(bad) = first_invalid_step(chunk_map, req.start, &segment_path) {
        diag.path_failed_step_continuity += 1;
        diag.path_failed_no_route += 1;
        if flags.verbose_logs {
            let prev = if bad == 0 {
                req.start
            } else {
                let p = segment_path[bad - 1];
                (p.0 as i32, p.1 as i32, p.2)
            };
            let cur = segment_path[bad];
            warn!(
                "[path] A* emitted bad step agent={:?} prev={:?} -> {:?}",
                req.agent, prev, cur
            );
        }
        write_failure(req, FailReason::NoRoute, now_tick, follows, failed_w, failure_log, diag);
        return;
    }

    write_success(
        req,
        chunk_route,
        segment_path,
        ready_kind,
        conn_generation,
        follows,
        ready_w,
        diag,
    );
}

fn build_chunk_route(
    graph: &ChunkGraph,
    router: &ChunkRouter,
    start_chunk: ChunkCoord,
    goal_chunk: ChunkCoord,
    start_z: i8,
) -> Result<Vec<ChunkCoord>, FailReason> {
    let mut route: Vec<ChunkCoord> = vec![start_chunk];
    if start_chunk == goal_chunk {
        return Ok(route);
    }
    let mut cur = start_chunk;
    let mut cur_z = start_z;
    while cur != goal_chunk {
        if route.len() >= MAX_CHUNK_ROUTE {
            return Err(FailReason::NoRoute);
        }
        let waypoint = router.first_waypoint(graph, cur, goal_chunk, cur_z);
        let Some((wx, wy)) = waypoint else {
            return Err(FailReason::NoRoute);
        };
        let next = ChunkCoord(
            (wx as i32).div_euclid(CHUNK_SIZE as i32),
            (wy as i32).div_euclid(CHUNK_SIZE as i32),
        );
        if next == cur {
            // Defensive: router returned a waypoint inside the current
            // chunk. Treat as no progress to avoid an infinite loop.
            return Err(FailReason::NoRoute);
        }
        route.push(next);
        cur = next;
        // We don't track precise entry-tile Z here; the router's Z bias
        // tends to keep us on the agent's current band, and A* picks the
        // exact ramp on the next segment. Leaving cur_z unchanged works
        // adequately for shadow-mode comparison; step (f) refines this.
        let _ = cur_z;
    }
    Ok(route)
}

fn first_segment_target(
    graph: &ChunkGraph,
    router: &ChunkRouter,
    chunk_route: &[ChunkCoord],
    req: &PathRequest,
) -> (i32, i32, i8) {
    if chunk_route.len() < 2 {
        return req.goal;
    }
    let start_chunk = chunk_route[0];
    let goal_chunk = *chunk_route.last().expect("len >= 2");
    let Some((wx, wy)) = router.first_waypoint(graph, start_chunk, goal_chunk, req.start.2) else {
        return req.goal;
    };
    // Use the chunk-graph edge's `entry_z` so A* searches for a tile that
    // actually exists on the right Z. Forcing the segment target's Z to the
    // agent's current Z (the previous behaviour) produced false `Unreachable`
    // failures whenever the waypoint was on a ramp or different Z slice.
    let next_chunk = chunk_route[1];
    let entry_z = graph
        .edges
        .get(&start_chunk)
        .and_then(|edges| {
            edges
                .iter()
                .find(|e| {
                    e.neighbor == next_chunk
                        && (e.entry_local.0 as i32 + next_chunk.0 * CHUNK_SIZE as i32) == wx as i32
                        && (e.entry_local.1 as i32 + next_chunk.1 * CHUNK_SIZE as i32) == wy as i32
                })
                .map(|e| e.entry_z)
        })
        .unwrap_or(req.start.2);
    (wx as i32, wy as i32, entry_z)
}

#[allow(clippy::too_many_arguments)]
fn write_failure(
    req: &PathRequest,
    reason: FailReason,
    now_tick: u64,
    follows: &mut Query<&mut PathFollow>,
    failed_w: &mut EventWriter<PathFailed>,
    failure_log: &mut FailureLog,
    diag: &mut PathfindingDiagnostics,
) {
    match follows.get_mut(req.agent) {
        Ok(mut follow) => {
            follow.status = FollowStatus::Failed(reason);
            follow.goal = req.goal;
            follow.chunk_route.clear();
            follow.route_cursor = 0;
            follow.segment_path.clear();
            follow.segment_cursor = 0;
            follow.request_id = req.id;
            follow.last_fail_reason = Some(reason);
            follow.last_fail_tick = now_tick;
            if follow.last_fail_goal == req.goal {
                follow.last_fail_streak = follow.last_fail_streak.saturating_add(1);
            } else {
                follow.last_fail_streak = 1;
                follow.last_fail_goal = req.goal;
            }
        }
        Err(_) => {
            diag.missing_follow_on_event += 1;
            warn!(
                "[path] PathFailed but no PathFollow on agent={:?} (despawned?) reason={:?}",
                req.agent, reason
            );
        }
    }
    failure_log.push(FailureRecord {
        tick: now_tick,
        agent: req.agent,
        start: req.start,
        goal: req.goal,
        reason,
    });
    failed_w.send(PathFailed {
        agent: req.agent,
        request_id: req.id,
        reason,
    });
}

#[allow(clippy::too_many_arguments)]
fn write_success(
    req: &PathRequest,
    chunk_route: Vec<ChunkCoord>,
    segment_path: Vec<(i16, i16, i8)>,
    ready_kind: PathReadyKind,
    conn_generation: u32,
    follows: &mut Query<&mut PathFollow>,
    ready_w: &mut EventWriter<PathReady>,
    diag: &mut PathfindingDiagnostics,
) {
    match follows.get_mut(req.agent) {
        Ok(mut follow) => {
            follow.status = FollowStatus::Following;
            follow.goal = req.goal;
            follow.chunk_route = chunk_route;
            follow.route_cursor = 0;
            follow.segment_path = segment_path;
            follow.segment_cursor = 0;
            follow.recent_tiles = [(i16::MIN, i16::MIN, 0); 4];
            follow.recent_idx = 0;
            follow.stuck_ticks = 0;
            follow.last_replan_tick = 0;
            follow.planning_generation = conn_generation;
            follow.request_id = req.id;
            follow.last_fail_streak = 0;
        }
        Err(_) => {
            diag.missing_follow_on_event += 1;
            warn!(
                "[path] PathReady but no PathFollow on agent={:?} (despawned?)",
                req.agent
            );
        }
    }
    match ready_kind {
        PathReadyKind::Strict => diag.paths_ready_strict += 1,
        PathReadyKind::BestEffort => diag.paths_ready_best_effort += 1,
    }
    ready_w.send(PathReady {
        agent: req.agent,
        request_id: req.id,
        kind: ready_kind,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk(surf_z: i8) -> Chunk {
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    #[test]
    fn first_invalid_step_flags_corner_cut_diagonal() {
        // Path tries a single diagonal (5,5,0) → (6,6,0) where (6,5) is
        // walled. The validator must reject so worker drops the path
        // before movement_system snap-backs at runtime.
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        for z in 0..=1i32 {
            map.set_tile(
                6,
                5,
                z,
                TileData { kind: TileKind::Wall, ..Default::default() },
            );
        }
        let path = vec![(6i16, 6i16, 0i8)];
        assert_eq!(first_invalid_step(&map, (5, 5, 0), &path), Some(0));
    }

    #[test]
    fn first_invalid_step_accepts_clean_diagonal() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let path = vec![(6i16, 6i16, 0i8)];
        assert_eq!(first_invalid_step(&map, (5, 5, 0), &path), None);
    }
}
