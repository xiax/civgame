use bevy::prelude::*;
use std::time::Instant;

use crate::pathfinding::astar::{find_path_in, find_path_profile, AStarResult};
use crate::pathfinding::chunk_graph::{ChunkGraph, ComponentId};
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::flow_field::walk_to_goal;
use crate::pathfinding::hotspots::HotspotFlowFields;
use crate::pathfinding::path_request::{
    FailSubReason, FailureLog, FailureRecord, FollowStatus, PathDebugFlags, PathFailed, PathFollow,
    PathKind, PathReady, PathReadyKind, PathRequest, PathRequestQueue,
};
use crate::pathfinding::pool::{AStarPool, AStarScratch};
use crate::pathfinding::step::{passable_diagonal_step, passable_diagonal_step_for};
use crate::pathfinding::tile_cost::{TraversalProfile, BASE_STEP_COST};
use crate::simulation::tasks::task_kind_label;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};

/// Default for `PerfWorkBudget::path_requests_per_tick` — the maximum number
/// of `PathRequest`s the worker drains in one tick. Sized for the
/// FixedUpdate-resident drain — at 20 Hz baseline this is 192 × 20 ≈ 3840
/// req/s. Higher game speeds run FixedUpdate more often via
/// `Time<Virtual>::set_relative_speed`, so the budget scales with sim demand
/// without further tuning. At ~50 µs per A* segment worst case that's
/// ~10 ms/tick, at the edge of the 5× speed budget of 10 ms. This is a
/// *drain capacity*, not a CPU cap — the runtime cap reads
/// `PerfWorkBudget::path_requests_per_tick` (this value is its default).
pub const PATH_BUDGET_PER_TICK: usize = 192;

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
    /// Hotspot field returned a path, but `first_invalid_step` rejected it
    /// against live `ChunkMap` state (stale `cell_z` after terrain
    /// mutation). Treated as a recoverable cache miss — falls through to
    /// A* instead of returning `NoRouteStepContinuity`. Non-zero values
    /// indicate hotspot-field invalidation drift; see
    /// `plans/fix-sleep-stalls.md` Deferred follow-up.
    pub hotspot_fastpath_bad_steps: u64,
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
    /// The agent's start tile (or the goal tile) didn't classify into
    /// any chunk-graph component. Either the chunk hasn't been built
    /// yet, or terrain mutated mid-tick and the agent is on a
    /// freshly-modified cell. Indicates a transient race; should stay
    /// near zero in healthy runs.
    pub component_lookup_failed_at_start: u64,
    pub component_lookup_failed_at_goal: u64,
    /// Largest `chunk_route.len()` produced by a successful path build
    /// in the last tick. Watch this for regressions: post-overhaul a
    /// well-routed path is no longer than the chunk-Manhattan distance
    /// between endpoints.
    pub chunk_route_len_max_last_tick: u32,
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
        self.chunk_route_len_max_last_tick = 0;
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
    path: &[(i32, i32, i8)],
    profile: TraversalProfile,
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
                passable_diagonal_step_for(chunk_map, from, to, profile)
            } else {
                chunk_map.passable_step_for(from, to, profile)
            };
            if !ok {
                return Some(i);
            }
        }
        prev = next;
    }
    None
}

/// Per-request outcome produced by the parallel A* worker tasks. Held in
/// a flat `Vec` between the parallel compute phase and the serial apply
/// phase so the apply phase can borrow `Query<&mut PathFollow>`, the
/// event writers, and the diagnostics resource without contention.
struct ComputeOutcome {
    req: PathRequest,
    body: OutcomeBody,
    astar_calls: u32,
    astar_iters: u32,
    astar_iters_max_single: u32,
    /// Goal tile whose hotspot flow field the worker detected as stale
    /// (fast-path emitted a path that failed `first_invalid_step`). The
    /// serial post-pass evicts the field so subsequent requests this same
    /// Update don't re-walk it before PostUpdate's invalidator runs.
    evict_hotspot_goal: Option<(i32, i32, i8)>,
}

enum OutcomeBody {
    Success {
        chunk_route: Vec<ChunkCoord>,
        segment_path: Vec<(i32, i32, i8)>,
        ready_kind: PathReadyKind,
        flow_field_hit: bool,
        hotspot_fastpath_bad_step: bool,
    },
    Failure(FailureBody),
}

struct FailureBody {
    sub: FailSubReason,
    segment_target: (i32, i32, i8),
    connectivity_reject: bool,
    hotspot_fastpath_miss: bool,
    /// Hotspot fast path produced a path that failed `first_invalid_step`
    /// against live chunk state. Bumped even when A* later recovers — it
    /// counts cache-drift events, not request failures.
    hotspot_fastpath_bad_step: bool,
    /// Set when the failure is specifically "agent's start tile has no
    /// component classification" — separately tracked because it's a
    /// transient race (terrain mutated between request enqueue and
    /// worker drain).
    component_lookup_failed_start: bool,
    component_lookup_failed_goal: bool,
}

impl FailureBody {
    fn basic(sub: FailSubReason, segment_target: (i32, i32, i8)) -> Self {
        Self {
            sub,
            segment_target,
            connectivity_reject: false,
            hotspot_fastpath_miss: false,
            hotspot_fastpath_bad_step: false,
            component_lookup_failed_start: false,
            component_lookup_failed_goal: false,
        }
    }
    fn connectivity(sub: FailSubReason, segment_target: (i32, i32, i8)) -> Self {
        Self {
            sub,
            segment_target,
            connectivity_reject: true,
            hotspot_fastpath_miss: false,
            hotspot_fastpath_bad_step: false,
            component_lookup_failed_start: false,
            component_lookup_failed_goal: false,
        }
    }
}

fn fail_outcome(req: PathRequest, body: FailureBody) -> ComputeOutcome {
    ComputeOutcome {
        body: OutcomeBody::Failure(body),
        req,
        astar_calls: 0,
        astar_iters: 0,
        astar_iters_max_single: 0,
        evict_hotspot_goal: None,
    }
}

fn fail_outcome_with_metrics(
    req: PathRequest,
    body: FailureBody,
    astar_calls: u32,
    astar_iters: u32,
    astar_iters_max_single: u32,
) -> ComputeOutcome {
    ComputeOutcome {
        body: OutcomeBody::Failure(body),
        req,
        astar_calls,
        astar_iters,
        astar_iters_max_single,
        evict_hotspot_goal: None,
    }
}

/// Drains up to `PerfWorkBudget::path_requests_per_tick` requests from the
/// queue and writes `PathFollow` for the requesting agents. Runs on
/// FixedUpdate before `SimulationSet::Sequential` (see `pathfinding/mod.rs`)
/// so movement consuming `PathFollow` later in the same tick sees the
/// freshly-computed result.
///
/// A* searches run in parallel on the Bevy compute task pool. Component
/// writes, event emission, and diagnostic counters are folded back in
/// serially on the main schedule thread once the parallel scope returns.
pub fn drain_path_requests_system(
    mut queue: ResMut<PathRequestQueue>,
    chunk_map: Res<ChunkMap>,
    graph: Res<ChunkGraph>,
    router: Res<ChunkRouter>,
    conn: Res<ChunkConnectivity>,
    mut hotspots: ResMut<HotspotFlowFields>,
    flags: Res<PathDebugFlags>,
    budget: Res<crate::simulation::perf::PerfWorkBudget>,
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

    // Drain a batch from the queue (tunable knob; default == PATH_BUDGET_PER_TICK).
    let drain_cap = budget.path_requests_per_tick.max(1);
    let mut requests: Vec<PathRequest> = Vec::with_capacity(drain_cap);
    while requests.len() < drain_cap {
        let Some(req) = queue.pop() else { break };
        requests.push(req);
    }

    if requests.is_empty() {
        diag.queue_len = queue.len() as u32;
        return;
    }

    // Allocate one scratch per task so the parallel A* runs are
    // contention-free. The pool keeps capacity across ticks.
    pool.ensure(requests.len());
    let scratches = pool.slice_mut(requests.len());

    let chunk_map_ref: &ChunkMap = &chunk_map;
    let graph_ref: &ChunkGraph = &graph;
    let router_ref: &ChunkRouter = &router;
    let conn_ref: &ChunkConnectivity = &conn;
    let hotspots_ref: &HotspotFlowFields = &hotspots;
    let flags_ref: &PathDebugFlags = &flags;

    let outcomes: Vec<ComputeOutcome> = bevy::tasks::ComputeTaskPool::get().scope(|s| {
        for (req, scratch) in requests.iter().zip(scratches.iter_mut()) {
            let req_clone = req.clone();
            s.spawn(async move {
                compute_outcome(
                    req_clone,
                    chunk_map_ref,
                    graph_ref,
                    router_ref,
                    conn_ref,
                    hotspots_ref,
                    flags_ref,
                    scratch,
                )
            });
        }
    });

    let dispatched = outcomes.len() as u32;
    // Self-heal stale hotspot fields detected during the parallel walk.
    // PostUpdate's invalidator only fires once per Update; without this,
    // subsequent FixedUpdate iterations in the same Update would re-walk
    // the same stale field. See `HotspotFlowFields::evict_field_for_goal`.
    for outcome in &outcomes {
        if let Some(goal) = outcome.evict_hotspot_goal {
            hotspots.evict_field_for_goal(goal);
        }
    }
    for outcome in outcomes {
        apply_outcome(
            outcome,
            &chunk_map,
            now_tick,
            conn_generation,
            &mut follows,
            &mut ready_w,
            &mut failed_w,
            &mut diag,
            &mut failure_log,
        );
    }

    diag.paths_dispatched_per_tick = dispatched;
    diag.queue_len = queue.len() as u32;
    diag.worker_us_per_tick = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
}

/// Top-level path build. A `Land` request routes through the chunk
/// graph (`compute_land`). An `Amphibious` request **also tries the land
/// route first** — when a dry route exists nothing swims, and the fast
/// hierarchical pathfinder is preserved with zero regression. Only when
/// land routing fails as `Unreachable` / `NoRoute` (banks split by
/// water) does it fall back to `compute_amphibious`'s bounded swim A*.
#[allow(clippy::too_many_arguments)]
fn compute_outcome(
    req: PathRequest,
    chunk_map: &ChunkMap,
    graph: &ChunkGraph,
    router: &ChunkRouter,
    conn: &ChunkConnectivity,
    hotspots: &HotspotFlowFields,
    flags: &PathDebugFlags,
    scratch: &mut AStarScratch,
) -> ComputeOutcome {
    let land = compute_land(
        req.clone(),
        chunk_map,
        graph,
        router,
        conn,
        hotspots,
        flags,
        scratch,
    );
    if req.profile == TraversalProfile::Amphibious {
        if let OutcomeBody::Failure(ref fb) = land.body {
            use crate::pathfinding::path_request::FailReason;
            if matches!(
                fb.sub.to_reason(),
                FailReason::Unreachable | FailReason::NoRoute
            ) {
                return compute_amphibious(req, chunk_map, scratch, flags);
            }
        }
    }
    land
}

#[allow(clippy::too_many_arguments)]
fn compute_land(
    req: PathRequest,
    chunk_map: &ChunkMap,
    graph: &ChunkGraph,
    router: &ChunkRouter,
    conn: &ChunkConnectivity,
    hotspots: &HotspotFlowFields,
    flags: &PathDebugFlags,
    scratch: &mut AStarScratch,
) -> ComputeOutcome {
    let start_chunk = chunk_of(req.start);
    let goal_chunk = chunk_of(req.goal);

    // Reject goals whose Z slice isn't standable. A* would otherwise burn
    // budget exploring from start until it gives up, then we'd retry on
    // every cooldown elapse. The dispatch path is supposed to snap goal Z
    // via `nearest_standable_z` but stale `target_z` can sneak through
    // (Routing→Seeking, stranded recovery, etc.).
    if !chunk_map.passable_for(req.goal.0, req.goal.1, req.goal.2 as i32, req.profile) {
        if flags.verbose_logs {
            info!(
                "[path] goal not standable agent={:?} goal={:?}",
                req.agent, req.goal
            );
        }
        let goal = req.goal;
        return fail_outcome(
            req,
            FailureBody::connectivity(FailSubReason::UnreachableConnectivity, goal),
        );
    }


    // Look up start/goal components in the new component-typed graph.
    // The agent's exact (x, y, z) classifies into a single component;
    // an A→B→A oscillation is impossible because the cache is keyed by
    // the agent's specific component, not just by chunk + z.
    let start_component = match graph.component_for_tile(req.start.0, req.start.1, req.start.2) {
        Some(c) => c,
        None => {
            if flags.verbose_logs {
                info!(
                    "[path] start tile not classified agent={:?} start={:?}",
                    req.agent, req.start
                );
            }
            let goal = req.goal;
            let mut body = FailureBody::connectivity(FailSubReason::UnreachableConnectivity, goal);
            body.component_lookup_failed_start = true;
            return fail_outcome(req, body);
        }
    };
    let goal_component = match graph.component_for_tile(req.goal.0, req.goal.1, req.goal.2) {
        Some(c) => c,
        None => {
            if flags.verbose_logs {
                info!(
                    "[path] goal tile not classified agent={:?} goal={:?}",
                    req.agent, req.goal
                );
            }
            let goal = req.goal;
            let mut body = FailureBody::connectivity(FailSubReason::UnreachableConnectivity, goal);
            body.component_lookup_failed_goal = true;
            return fail_outcome(req, body);
        }
    };
    // Reachability check uses the same router cache the route does, so
    // the component-graph CC and the route are guaranteed in sync.
    let _ = conn; // legacy resource retained for backwards-compatible APIs
    if !router.is_reachable(
        graph,
        (start_chunk, start_component),
        (goal_chunk, goal_component),
    ) {
        if flags.verbose_logs {
            info!(
                "[path] component reject agent={:?} start={:?} goal={:?}",
                req.agent, req.start, req.goal
            );
        }
        let goal = req.goal;
        return fail_outcome(
            req,
            FailureBody::connectivity(FailSubReason::UnreachableConnectivity, goal),
        );
    }

    let chunk_route = match router.compute_route(
        graph,
        (start_chunk, start_component),
        (goal_chunk, goal_component),
    ) {
        Some(r) => r,
        None => {
            if flags.verbose_logs {
                info!(
                    "[path] chunk-route fail agent={:?} start_chunk={:?}/{:?} goal_chunk={:?}/{:?}",
                    req.agent, start_chunk, start_component, goal_chunk, goal_component
                );
            }
            let goal = req.goal;
            return fail_outcome(req, FailureBody::basic(FailSubReason::NoRouteRouter, goal));
        }
    };

    // Fast path: hotspot flow-field walk if the goal is in the start
    // chunk and the agent's cell reached the goal at the agent's Z.
    let mut hotspot_miss = false;
    let mut hotspot_bad_step = false;
    // Populated when the fast path detects cache drift; consumed by the
    // serial post-pass to evict the stale field. See
    // `HotspotFlowFields::evict_field_for_goal` for the rationale.
    let mut evict_hotspot_goal: Option<(i32, i32, i8)> = None;
    if chunk_route.len() == 1 {
        let goal_tile = (req.goal.0 as i32, req.goal.1 as i32, req.goal.2);
        if let Some(field) = hotspots.lookup_field(goal_tile) {
            let csz = CHUNK_SIZE as i32;
            let lx = (req.start.0 - field.chunk.0 * csz) as u8;
            let ly = (req.start.1 - field.chunk.1 * csz) as u8;
            let cell_idx = ly as usize * CHUNK_SIZE + lx as usize;
            if field.cell_z[cell_idx] == req.start.2 {
                if let Some(path) = walk_to_goal(field, (lx, ly)) {
                    if let Some(bad) =
                        first_invalid_step(chunk_map, req.start, &path, TraversalProfile::Land)
                    {
                        // Stale hotspot cache: the cached `cell_z` field
                        // matched live z at the start cell, but somewhere
                        // along the emitted path a per-step continuity
                        // check failed against live `ChunkMap` state. Treat
                        // as a recoverable cache miss and fall through to
                        // A* — A* re-routing against live state is the
                        // authoritative fallback. Distinct counter from
                        // `hotspot_fastpath_misses` so cache-drift events
                        // can be diagnosed in telemetry independent of
                        // `walk_to_goal` returning `None`.
                        hotspot_bad_step = true;
                        hotspot_miss = true;
                        evict_hotspot_goal = Some(goal_tile);
                        if flags.verbose_logs {
                            let prev = if bad == 0 {
                                req.start
                            } else {
                                let p = path[bad - 1];
                                (p.0 as i32, p.1 as i32, p.2)
                            };
                            let cur = path[bad];
                            debug!(
                                "[path] hotspot fastpath bad step agent={:?} prev={:?} -> {:?} (falling through to A*)",
                                req.agent, prev, cur
                            );
                        }
                    } else {
                        return ComputeOutcome {
                            body: OutcomeBody::Success {
                                chunk_route,
                                segment_path: path,
                                ready_kind: PathReadyKind::Strict,
                                flow_field_hit: true,
                                hotspot_fastpath_bad_step: false,
                            },
                            req,
                            astar_calls: 0,
                            astar_iters: 0,
                            astar_iters_max_single: 0,
                            evict_hotspot_goal: None,
                        };
                    }
                } else {
                    hotspot_miss = true;
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

    let segment_target = first_segment_target(
        graph,
        router,
        &chunk_route,
        &req,
        start_component,
        goal_component,
    );
    let mut astar_calls: u32 = 0;
    let mut astar_iters_total: u32 = 0;
    let mut astar_iters_max: u32 = 0;

    astar_calls += 1;
    let (mut result, mut iters) = find_path_in(
        scratch,
        chunk_map,
        req.start,
        segment_target,
        req.max_budget as usize,
    );
    astar_iters_total = astar_iters_total.saturating_add(iters);
    if iters > astar_iters_max {
        astar_iters_max = iters;
    }

    if matches!(result, AStarResult::BudgetExhausted { .. }) {
        astar_calls += 1;
        let retry = find_path_in(
            scratch,
            chunk_map,
            req.start,
            segment_target,
            req.max_budget.saturating_mul(4) as usize,
        );
        result = retry.0;
        iters = retry.1;
        astar_iters_total = astar_iters_total.saturating_add(iters);
        if iters > astar_iters_max {
            astar_iters_max = iters;
        }
    }

    let (segment_path, ready_kind) = match result {
        AStarResult::Found(path) => {
            let converted: Vec<(i32, i32, i8)> = path
                .into_iter()
                .map(|(x, y, z)| (x as i32, y as i32, z))
                .collect();
            (converted, PathReadyKind::Strict)
        }
        AStarResult::BudgetExhausted { best_so_far } => {
            if matches!(req.kind, PathKind::Strict) {
                if flags.verbose_logs {
                    info!(
                        "[path] A* budget exhausted (strict) agent={:?} start={:?} target={:?}",
                        req.agent, req.start, segment_target
                    );
                }
                let mut body = FailureBody::basic(FailSubReason::BudgetExhausted, segment_target);
                body.hotspot_fastpath_miss = hotspot_miss;
                body.hotspot_fastpath_bad_step = hotspot_bad_step;
                let mut outcome = fail_outcome_with_metrics(
                    req,
                    body,
                    astar_calls,
                    astar_iters_total,
                    astar_iters_max,
                );
                outcome.evict_hotspot_goal = evict_hotspot_goal;
                return outcome;
            }
            // BestEffort: walk one tile toward best_so_far.
            (
                vec![(best_so_far.0 as i32, best_so_far.1 as i32, best_so_far.2)],
                PathReadyKind::BestEffort,
            )
        }
        AStarResult::Unreachable => {
            if flags.verbose_logs {
                info!(
                    "[path] A* unreachable agent={:?} start={:?} target={:?}",
                    req.agent, req.start, segment_target
                );
            }
            let mut body = FailureBody::basic(FailSubReason::UnreachableAstar, segment_target);
            body.hotspot_fastpath_miss = hotspot_miss;
            body.hotspot_fastpath_bad_step = hotspot_bad_step;
            let mut outcome = fail_outcome_with_metrics(
                req,
                body,
                astar_calls,
                astar_iters_total,
                astar_iters_max,
            );
            outcome.evict_hotspot_goal = evict_hotspot_goal;
            return outcome;
        }
    };

    if let Some(bad) =
        first_invalid_step(chunk_map, req.start, &segment_path, TraversalProfile::Land)
    {
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
        let mut body = FailureBody::basic(FailSubReason::NoRouteStepContinuity, segment_target);
        body.hotspot_fastpath_miss = hotspot_miss;
        body.hotspot_fastpath_bad_step = hotspot_bad_step;
        let mut outcome = fail_outcome_with_metrics(
            req,
            body,
            astar_calls,
            astar_iters_total,
            astar_iters_max,
        );
        outcome.evict_hotspot_goal = evict_hotspot_goal;
        return outcome;
    }

    ComputeOutcome {
        body: OutcomeBody::Success {
            chunk_route,
            segment_path,
            ready_kind,
            flow_field_hit: false,
            hotspot_fastpath_bad_step: hotspot_bad_step,
        },
        req,
        astar_calls,
        astar_iters: astar_iters_total,
        astar_iters_max_single: astar_iters_max,
        evict_hotspot_goal,
    }
}

/// Amphibious path build: one bounded full-route A* over
/// `passable_for(Amphibious)`. The whole tile path is stuffed into a
/// single segment with a length-1 `chunk_route` (just the start chunk)
/// so `movement_system` walks `segment_path` to completion and then
/// reports arrival without requesting a next segment.
fn compute_amphibious(
    req: PathRequest,
    chunk_map: &ChunkMap,
    scratch: &mut AStarScratch,
    flags: &PathDebugFlags,
) -> ComputeOutcome {
    let mut astar_calls: u32 = 1;
    let (mut result, mut iters) = find_path_profile(
        scratch,
        chunk_map,
        req.start,
        req.goal,
        req.max_budget as usize,
        TraversalProfile::Amphibious,
    );
    let mut iters_total = iters;
    let mut iters_max = iters;
    if matches!(result, AStarResult::BudgetExhausted { .. }) {
        astar_calls += 1;
        let retry = find_path_profile(
            scratch,
            chunk_map,
            req.start,
            req.goal,
            req.max_budget.saturating_mul(4) as usize,
            TraversalProfile::Amphibious,
        );
        result = retry.0;
        iters = retry.1;
        iters_total = iters_total.saturating_add(iters);
        iters_max = iters_max.max(iters);
    }

    let (segment_path, ready_kind) = match result {
        AStarResult::Found(path) => (path, PathReadyKind::Strict),
        AStarResult::BudgetExhausted { best_so_far } => {
            if matches!(req.kind, PathKind::Strict) {
                if flags.verbose_logs {
                    info!(
                        "[path] amphibious A* budget exhausted agent={:?} start={:?} goal={:?}",
                        req.agent, req.start, req.goal
                    );
                }
                let goal = req.goal;
                return fail_outcome_with_metrics(
                    req,
                    FailureBody::basic(FailSubReason::BudgetExhausted, goal),
                    astar_calls,
                    iters_total,
                    iters_max,
                );
            }
            (vec![best_so_far], PathReadyKind::BestEffort)
        }
        AStarResult::Unreachable => {
            if flags.verbose_logs {
                info!(
                    "[path] amphibious A* unreachable agent={:?} start={:?} goal={:?}",
                    req.agent, req.start, req.goal
                );
            }
            let goal = req.goal;
            return fail_outcome_with_metrics(
                req,
                FailureBody::basic(FailSubReason::UnreachableAstar, goal),
                astar_calls,
                iters_total,
                iters_max,
            );
        }
    };

    if let Some(_bad) =
        first_invalid_step(chunk_map, req.start, &segment_path, TraversalProfile::Amphibious)
    {
        let goal = req.goal;
        return fail_outcome_with_metrics(
            req,
            FailureBody::basic(FailSubReason::NoRouteStepContinuity, goal),
            astar_calls,
            iters_total,
            iters_max,
        );
    }

    let chunk_route = vec![chunk_of(req.start)];
    ComputeOutcome {
        body: OutcomeBody::Success {
            chunk_route,
            segment_path,
            ready_kind,
            flow_field_hit: false,
            hotspot_fastpath_bad_step: false,
        },
        req,
        astar_calls,
        astar_iters: iters_total,
        astar_iters_max_single: iters_max,
        evict_hotspot_goal: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    outcome: ComputeOutcome,
    chunk_map: &ChunkMap,
    now_tick: u64,
    conn_generation: u32,
    follows: &mut Query<&mut PathFollow>,
    ready_w: &mut EventWriter<PathReady>,
    failed_w: &mut EventWriter<PathFailed>,
    diag: &mut PathfindingDiagnostics,
    failure_log: &mut FailureLog,
) {
    diag.astar_calls_per_tick = diag
        .astar_calls_per_tick
        .saturating_add(outcome.astar_calls);
    diag.astar_iters_last_tick = diag
        .astar_iters_last_tick
        .saturating_add(outcome.astar_iters);
    if outcome.astar_iters_max_single > diag.astar_iters_max_single {
        diag.astar_iters_max_single = outcome.astar_iters_max_single;
    }

    let req = outcome.req;
    match outcome.body {
        OutcomeBody::Success {
            chunk_route,
            segment_path,
            ready_kind,
            flow_field_hit,
            hotspot_fastpath_bad_step,
        } => {
            let route_len = chunk_route.len() as u32;
            if route_len > diag.chunk_route_len_max_last_tick {
                diag.chunk_route_len_max_last_tick = route_len;
            }
            if flow_field_hit {
                diag.flow_field_hits_per_tick += 1;
                diag.flow_field_hits_total += 1;
            }
            if hotspot_fastpath_bad_step {
                diag.hotspot_fastpath_bad_steps += 1;
            }
            write_success(
                &req,
                chunk_route,
                segment_path,
                ready_kind,
                conn_generation,
                follows,
                ready_w,
                diag,
            );
        }
        OutcomeBody::Failure(FailureBody {
            sub,
            segment_target,
            connectivity_reject,
            hotspot_fastpath_miss,
            hotspot_fastpath_bad_step,
            component_lookup_failed_start,
            component_lookup_failed_goal,
        }) => {
            if connectivity_reject {
                diag.connectivity_rejections_per_tick += 1;
            }
            if hotspot_fastpath_miss {
                diag.hotspot_fastpath_misses += 1;
            }
            if hotspot_fastpath_bad_step {
                diag.hotspot_fastpath_bad_steps += 1;
            }
            if component_lookup_failed_start {
                diag.component_lookup_failed_at_start += 1;
            }
            if component_lookup_failed_goal {
                diag.component_lookup_failed_at_goal += 1;
            }
            match sub {
                FailSubReason::UnreachableConnectivity => {
                    diag.path_failed_unreachable += 1;
                    diag.path_failed_unreachable_connectivity += 1;
                }
                FailSubReason::UnreachableAstar => {
                    diag.path_failed_unreachable += 1;
                    diag.path_failed_unreachable_astar += 1;
                }
                FailSubReason::BudgetExhausted => {
                    diag.path_failed_budget += 1;
                }
                FailSubReason::NoRouteRouter => {
                    diag.path_failed_no_route += 1;
                }
                FailSubReason::NoRouteStepContinuity => {
                    diag.path_failed_step_continuity += 1;
                    diag.path_failed_no_route += 1;
                }
            }
            write_failure(
                &req,
                sub,
                segment_target,
                now_tick,
                chunk_map,
                follows,
                failed_w,
                failure_log,
                diag,
            );
        }
    }
}

#[inline]
fn cheb(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Pick the concrete first-hop portal for the current segment.
///
/// The router (`first_waypoint_full`) decides only the next chunk + component
/// to enter. The chunk graph stores one edge per passable border tile, so on a
/// uniform border the router's Dijkstra `next_hop` is just the first-relaxed
/// (scan-order) edge — a corner, independent of where the agent is or where it's
/// going. Here we enumerate every edge from the start chunk into that exact next
/// chunk/component and pick the one nearest the straight line between `req.start`
/// and `req.goal`, breaking ties on traversal cost (rescaled to tile units).
fn first_segment_target(
    graph: &ChunkGraph,
    router: &ChunkRouter,
    chunk_route: &[ChunkCoord],
    req: &PathRequest,
    start_component: ComponentId,
    goal_component: ComponentId,
) -> (i32, i32, i8) {
    if chunk_route.len() < 2 {
        return req.goal;
    }
    let start_chunk = chunk_route[0];
    let goal_chunk = *chunk_route.last().expect("len >= 2");
    let Some(wp) = router.first_waypoint_full(
        graph,
        (start_chunk, start_component),
        (goal_chunk, goal_component),
    ) else {
        return req.goal;
    };

    // Router decides identity; geometry decides the concrete portal.
    let next_chunk = wp.neighbor;
    let next_component = wp.neighbor_component;
    let start_xy = (req.start.0, req.start.1);
    let goal_xy = (req.goal.0, req.goal.1);
    let size = CHUNK_SIZE as i32;

    let best = graph
        .edges
        .get(&start_chunk)
        .into_iter()
        .flatten()
        .filter(|e| {
            e.neighbor == next_chunk
                && e.from_component == start_component
                && e.to_component == next_component
        })
        .map(|e| {
            let exit = (
                start_chunk.0 * size + e.exit_local.0 as i32,
                start_chunk.1 * size + e.exit_local.1 as i32,
            );
            let entry = (
                next_chunk.0 * size + e.entry_local.0 as i32,
                next_chunk.1 * size + e.entry_local.1 as i32,
            );
            // Primary: minimise detour off the straight line start→goal.
            // Secondary: cheaper terrain / flat-Z portal (cost rescaled to
            // tile units so it only breaks ties between equidistant portals).
            let cost_tiles =
                (e.traverse_cost as f32 / BASE_STEP_COST as f32).round() as i32;
            let score = cheb(start_xy, exit) + cheb(entry, goal_xy) + cost_tiles;
            (score, entry, e.entry_z)
        })
        .min_by_key(|(score, _, _)| *score);

    match best {
        Some((_, (ex, ey), ez)) => (ex, ey, ez),
        // Router said this hop exists but no matching edge was enumerable
        // (shouldn't happen) — fall back to the router's own pick.
        None => (wp.entry_x, wp.entry_y, wp.entry_z),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_failure(
    req: &PathRequest,
    sub: FailSubReason,
    segment_target: (i32, i32, i8),
    now_tick: u64,
    chunk_map: &ChunkMap,
    follows: &mut Query<&mut PathFollow>,
    failed_w: &mut EventWriter<PathFailed>,
    failure_log: &mut FailureLog,
    diag: &mut PathfindingDiagnostics,
) {
    let reason = sub.to_reason();
    match follows.get_mut(req.agent) {
        Ok(mut follow) => {
            follow.status = FollowStatus::Failed(reason);
            follow.goal = req.goal;
            follow.chunk_route.clear();
            follow.route_cursor = 0;
            follow.segment_path.clear();
            follow.segment_cursor = 0;
            follow.request_id = req.id;
            follow.last_fail_subreason = Some(sub);
            follow.last_fail_tick = now_tick;
            match sub {
                FailSubReason::UnreachableConnectivity => {
                    follow.fail_count_unreachable_conn =
                        follow.fail_count_unreachable_conn.saturating_add(1);
                }
                FailSubReason::UnreachableAstar => {
                    follow.fail_count_unreachable_astar =
                        follow.fail_count_unreachable_astar.saturating_add(1);
                    follow.last_astar_dump = Some(build_astar_diagnostic(
                        chunk_map,
                        req.start,
                        segment_target,
                        req.goal,
                        req.task_id,
                    ));
                }
                FailSubReason::BudgetExhausted => {
                    follow.fail_count_budget = follow.fail_count_budget.saturating_add(1);
                }
                FailSubReason::NoRouteRouter => {
                    follow.fail_count_no_route_router =
                        follow.fail_count_no_route_router.saturating_add(1);
                }
                FailSubReason::NoRouteStepContinuity => {
                    follow.fail_count_no_route_continuity =
                        follow.fail_count_no_route_continuity.saturating_add(1);
                }
            }
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
                "[path] PathFailed but no PathFollow on agent={:?} (despawned?) sub={:?}",
                req.agent, sub
            );
        }
    }
    failure_log.push(FailureRecord {
        tick: now_tick,
        agent: req.agent,
        start: req.start,
        goal: req.goal,
        subreason: sub,
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
    segment_path: Vec<(i32, i32, i8)>,
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
            follow.recent_tiles = [(i32::MIN, i32::MIN, 0); 4];
            follow.recent_idx = 0;
            follow.stuck_ticks = 0;
            follow.last_replan_tick = 0;
            follow.planning_generation = conn_generation;
            follow.request_id = req.id;
            follow.last_fail_streak = 0;
            follow.profile = req.profile;
            // Intentionally do NOT clear `last_astar_dump`: a successful
            // path between failures would wipe the dump before the user
            // can read it. The dump is overwritten on the next
            // UnreachableAstar or manually via "Force replan".
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

/// Build a compact terrain dump for an A* `Unreachable` failure. Lists
/// `surface_z` and step3d/diagonal results for each of the 8 neighbors of
/// `start`, plus per-tile passability of the segment target. Surfaces in
/// the inspector so the user can paste it back without re-running with
/// debug logs enabled.
fn build_astar_diagnostic(
    chunk_map: &ChunkMap,
    start: (i32, i32, i8),
    segment_target: (i32, i32, i8),
    goal: (i32, i32, i8),
    task_id: u16,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(900);
    let (sx, sy, sz) = start;
    let (tx, ty, tz) = segment_target;
    let start3 = (sx, sy, sz as i32);
    let task_label = task_kind_label(task_id);
    let _ = writeln!(out, "A* Unreachable");
    let _ = writeln!(out, "origin task={}({})", task_label, task_id);
    let _ = writeln!(
        out,
        "start ({},{},{})  segtgt ({},{},{})  goal ({},{},{})",
        sx, sy, sz, tx, ty, tz, goal.0, goal.1, goal.2
    );
    let _ = writeln!(
        out,
        "start surface_z={}  passable_at({})={}",
        chunk_map.surface_z_at(sx, sy),
        sz,
        bchar(chunk_map.passable_at(sx, sy, sz as i32)),
    );
    let _ = writeln!(
        out,
        "segtgt surface_z={}  passable_at({})={}  nearest_standable_z={}",
        chunk_map.surface_z_at(tx, ty),
        tz,
        bchar(chunk_map.passable_at(tx, ty, tz as i32)),
        chunk_map.nearest_standable_z(tx, ty, tz as i32),
    );
    let _ = writeln!(out, "neighbors of start (dx,dy):");
    let dirs: [((i32, i32), &str); 8] = [
        ((-1, 1), "NW"),
        ((0, 1), "N "),
        ((1, 1), "NE"),
        ((-1, 0), "W "),
        ((1, 0), "E "),
        ((-1, -1), "SW"),
        ((0, -1), "S "),
        ((1, -1), "SE"),
    ];
    for ((dx, dy), label) in dirs {
        let nx = sx + dx;
        let ny = sy + dy;
        let nsurf = chunk_map.surface_z_at(nx, ny);
        let tile_kind = chunk_map.tile_at(nx, ny, sz as i32).kind;
        let head_kind = chunk_map.tile_at(nx, ny, sz as i32 + 1).kind;
        let p_at = chunk_map.passable_at(nx, ny, sz as i32);
        let s_dn = chunk_map.passable_step_3d(start3, (nx, ny, sz as i32 - 1));
        let s_eq = chunk_map.passable_step_3d(start3, (nx, ny, sz as i32));
        let s_up = chunk_map.passable_step_3d(start3, (nx, ny, sz as i32 + 1));
        let is_diag = dx != 0 && dy != 0;
        let diag_str = if is_diag {
            let ok = passable_diagonal_step(chunk_map, start3, (nx, ny, sz as i32));
            format!("  diag={}", bchar(ok))
        } else {
            String::new()
        };
        let goal_marker = if (nx, ny) == (tx, ty) {
            "  ← SEGTGT"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  {} ({},{}) sz={:>3} kind={:?}/{:?} p@{}={} step3d@{}={} @{}={} @{}={}{}{}",
            label,
            nx,
            ny,
            nsurf,
            tile_kind,
            head_kind,
            sz,
            bchar(p_at),
            sz as i32 - 1,
            bchar(s_dn),
            sz,
            bchar(s_eq),
            sz as i32 + 1,
            bchar(s_up),
            diag_str,
            goal_marker,
        );
    }
    out
}

fn bchar(b: bool) -> char {
    if b {
        'T'
    } else {
        'F'
    }
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
                TileData {
                    kind: TileKind::Wall,
                    ..Default::default()
                },
            );
        }
        let path = vec![(6i32, 6i32, 0i8)];
        assert_eq!(first_invalid_step(&map, (5, 5, 0), &path, TraversalProfile::Land), Some(0));
    }

    #[test]
    fn first_invalid_step_accepts_clean_diagonal() {
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        let path = vec![(6i32, 6i32, 0i8)];
        assert_eq!(first_invalid_step(&map, (5, 5, 0), &path, TraversalProfile::Land), None);
    }

    /// Regression for `plans/fix-illogical-chunk-pathing.md`: the worker must
    /// pick the first-hop portal nearest the straight line between start and
    /// goal, not the scan-order border corner the router's Dijkstra `next_hop`
    /// happens to return on a uniform border.
    #[test]
    fn first_segment_target_picks_portal_near_straight_line() {
        use crate::pathfinding::chunk_graph::{rebuild_chunk_graph_sync, ChunkGraph};
        use crate::pathfinding::chunk_router::ChunkRouter;
        use crate::pathfinding::path_request::{PathKind, PathRequest};
        use crate::simulation::typed_task::UNEMPLOYED_TASK_KIND;

        // Two adjacent flat chunks sharing an east/west border at x=31|32.
        let mut chunk_map = ChunkMap::default();
        chunk_map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        chunk_map.0.insert(ChunkCoord(1, 0), flat_chunk(0));
        let mut graph = ChunkGraph::default();
        rebuild_chunk_graph_sync(&chunk_map, &mut graph);
        let router = ChunkRouter::default();

        // Start near the top-left of chunk(0,0); goal well into chunk(1,0) and
        // offset down on Y. The straight line crosses the x=32 border around
        // y ≈ 20; the scan-order corner would be y = 0.
        let start = (5i32, 5i32, 0i8);
        let goal = (40i32, 25i32, 0i8);
        let start_component = graph
            .component_for_tile(start.0, start.1, start.2)
            .expect("start standable");
        let goal_component = graph
            .component_for_tile(goal.0, goal.1, goal.2)
            .expect("goal standable");

        let chunk_route = router
            .compute_route(
                &graph,
                (ChunkCoord(0, 0), start_component),
                (ChunkCoord(1, 0), goal_component),
            )
            .expect("route exists");
        assert_eq!(chunk_route.len(), 2, "single cross-chunk hop");

        let req = PathRequest {
            id: 1,
            agent: Entity::from_raw(0),
            start,
            goal,
            kind: PathKind::BestEffort,
            max_budget: 4096,
            task_id: UNEMPLOYED_TASK_KIND,
            profile: TraversalProfile::Land,
        };

        let (ex, ey, _ez) =
            first_segment_target(&graph, &router, &chunk_route, &req, start_component, goal_component);

        // Entry tile must be just across the border (x = 32) and track the
        // line toward the goal, NOT the scan-order corner (y = 0). Under the
        // chebyshev metric the portals y∈[17,25] are co-optimal (all within
        // the goal's chebyshev shadow), so the straight-line crossing (~20)
        // falls inside the selected band; the tie-break returns its low edge.
        assert_eq!(ex, 32, "entry on the neighbour side of the shared border");
        assert!(
            (15..=25).contains(&ey),
            "portal y={ey} should track the goal direction, not corner 0",
        );
    }

    /// Regression for `plans/fix-sleep-stalls.md`: the hotspot fast path
    /// must not return `NoRouteStepContinuity` when its cached `cell_z`
    /// has drifted from live chunk state. It must set
    /// `hotspot_fastpath_bad_step` and fall through to the A* path, which
    /// re-routes against live state and succeeds.
    #[test]
    fn hotspot_bad_step_falls_through_to_astar() {
        use crate::pathfinding::chunk_graph::{rebuild_chunk_graph_sync, ChunkGraph};
        use crate::pathfinding::chunk_router::ChunkRouter;
        use crate::pathfinding::connectivity::{
            populate_connectivity_from_graph, ChunkConnectivity,
        };
        use crate::pathfinding::flow_field::{build_flow_field, FlowField};
        use crate::pathfinding::hotspots::{HotspotEntry, HotspotFlowFields, HotspotKey, HotspotKind};
        use crate::pathfinding::path_request::{PathDebugFlags, PathKind, PathRequest};
        use crate::pathfinding::pool::AStarScratch;
        use crate::simulation::typed_task::UNEMPLOYED_TASK_KIND;

        let mut chunk_map = ChunkMap::default();
        chunk_map.0.insert(ChunkCoord(0, 0), flat_chunk(0));
        // Build the live planner state against the clean flat chunk.
        let mut graph = ChunkGraph::default();
        rebuild_chunk_graph_sync(&chunk_map, &mut graph);
        let mut conn = ChunkConnectivity::default();
        populate_connectivity_from_graph(&graph, &mut conn);
        let router = ChunkRouter::default();

        // Build a real flow field whose path is valid against the current
        // chunk_map, then poison `cell_z` at the start cell to z=5. The
        // chunk's only standable layer is z=0, so emitting a step at z=5
        // fails `first_invalid_step` — exactly the cache-drift symptom.
        let goal_local = (10u8, 5u8);
        let start_local = (5u8, 5u8);
        let mut field: FlowField = build_flow_field(
            &chunk_map,
            ChunkCoord(0, 0),
            goal_local,
            0,
            &|_: (i32, i32)| 0u16,
        );
        // Poison the cell immediately stepped onto by `walk_to_goal` —
        // start is (5,5), first step lands on (6,5). MUST leave the
        // start cell's `cell_z` matching live `req.start.2` (the worker
        // gates entry to the fast path on that equality), and MUST keep
        // `cell_z[goal_idx] == goal_z` so the field is internally
        // consistent. Only the intermediate `(6,5)` cell — the first
        // step in the emitted path — is corrupted; `first_invalid_step`
        // catches it on step 0.
        let bad_idx = goal_local.1 as usize * CHUNK_SIZE + (start_local.0 as usize + 1);
        field.cell_z[bad_idx] = 5;

        let mut hotspots = HotspotFlowFields::default();
        let goal_tile = (goal_local.0 as i32, goal_local.1 as i32, 0i8);
        hotspots.entries.insert(
            HotspotKey {
                tile: goal_tile,
                kind: HotspotKind::FactionCenter,
            },
            HotspotEntry { field },
        );
        hotspots.field_count = 1;

        let flags = PathDebugFlags::default();
        let mut scratch = AStarScratch::default();
        let req = PathRequest {
            id: 1,
            agent: Entity::from_raw(0),
            start: (start_local.0 as i32, start_local.1 as i32, 0i8),
            goal: goal_tile,
            kind: PathKind::Strict,
            max_budget: 4096,
            task_id: UNEMPLOYED_TASK_KIND,
            profile: TraversalProfile::Land,
        };

        let outcome = compute_outcome(
            req,
            &chunk_map,
            &graph,
            &router,
            &conn,
            &hotspots,
            &flags,
            &mut scratch,
        );

        // Worker must report success — A* recovers against live state.
        // The bad-step flag must propagate so diagnostics can count
        // cache-drift events.
        assert_eq!(
            outcome.evict_hotspot_goal,
            Some(goal_tile),
            "expected the worker to flag the goal tile for hotspot eviction on bad-step",
        );
        match outcome.body {
            OutcomeBody::Success {
                hotspot_fastpath_bad_step,
                flow_field_hit,
                ..
            } => {
                assert!(
                    hotspot_fastpath_bad_step,
                    "expected the hotspot bad-step flag to be set when the cached field emits an invalid step",
                );
                assert!(
                    !flow_field_hit,
                    "expected fallthrough to A* (no flow-field hit) on bad-step",
                );
            }
            OutcomeBody::Failure(body) => {
                panic!(
                    "expected A* fallback Success, got Failure(sub={:?}, bad_step={})",
                    body.sub, body.hotspot_fastpath_bad_step,
                );
            }
        }
    }
}
