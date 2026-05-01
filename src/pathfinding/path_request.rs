use ahash::AHashSet;
use bevy::prelude::*;
use std::collections::VecDeque;

use crate::world::chunk::ChunkCoord;

/// Default node budget for the first A* attempt on a single segment.
/// Segments span at most one chunk plus a tile or two of slop, so this is
/// generous; a retry quadruples it before giving up.
pub const DEFAULT_PATH_BUDGET: u32 = 1500;

/// Base cooldown after a single path-request failure. Breaks the
/// Failed→Idle→re-enqueue→Failed loop that otherwise pegs the request queue
/// at the per-tick budget. Compared against `last_fail_tick` which is
/// sourced from `Time::elapsed().as_millis()`. Use `cooldown_for_streak`
/// rather than this constant directly so persistent failures back off.
pub const PATH_FAIL_COOLDOWN_MS: u64 = 3000;

/// Cap on how long the cooldown can grow when a goal keeps failing. After
/// `last_fail_streak` reaches the level that produces this duration, the
/// agent stops re-trying for ~30 s — enough that something else (terrain
/// change, plan timeout, goal flip) has a chance to break the loop.
pub const PATH_FAIL_COOLDOWN_MAX_MS: u64 = 30_000;

/// Effective cooldown for an agent whose last `streak` consecutive failures
/// all hit the same goal. Streak 1 returns the base; each subsequent strike
/// roughly squares the wait, capped at `PATH_FAIL_COOLDOWN_MAX_MS`. A streak
/// of 0 (no failure recorded) returns 0.
pub fn cooldown_for_streak(streak: u8) -> u64 {
    if streak == 0 {
        return 0;
    }
    let s = streak as u64;
    PATH_FAIL_COOLDOWN_MS
        .saturating_mul(s.saturating_mul(s))
        .min(PATH_FAIL_COOLDOWN_MAX_MS)
}

/// What kind of path the requester will accept.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PathKind {
    /// Only a complete path is acceptable. Partial / best-effort fails.
    Strict,
    /// A best-effort partial path is acceptable when the search ran out of
    /// budget — the agent walks toward `best_so_far` and re-requests on
    /// arrival.
    BestEffort,
}

/// Why the worker rejected a path request.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FailReason {
    /// `ChunkConnectivity` proved start and goal are in different
    /// connected components.
    Unreachable,
    /// A* ran past `max_budget` even after the retry. Goal is theoretically
    /// reachable but too far for the budget allotted.
    BudgetExhausted,
    /// `ChunkRouter` could not produce a chunk-graph route, e.g. when the
    /// chunk graph hasn't been built for one of the endpoints yet.
    NoRoute,
}

/// Finer-grained failure classification carried in the per-agent inspector
/// counters and the `FailureLog`. Each variant maps to exactly one
/// `FailReason` via `to_reason()`, but tells you which gate inside the
/// worker actually fired.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FailSubReason {
    /// `ChunkConnectivity` rejected the request before A* ran.
    UnreachableConnectivity,
    /// A* exhausted the open set without reaching the segment target.
    UnreachableAstar,
    /// A* exceeded `max_budget` even after the 4× retry (Strict only).
    BudgetExhausted,
    /// `build_chunk_route` could not produce a waypoint, or the router
    /// returned a self-waypoint (defensive `next == cur` check).
    NoRouteRouter,
    /// A*/flow-field emitted a path with a step that fails
    /// `passable_step_3d` — caught by `first_invalid_step`.
    NoRouteStepContinuity,
}

impl FailSubReason {
    pub fn to_reason(self) -> FailReason {
        match self {
            Self::UnreachableConnectivity | Self::UnreachableAstar => FailReason::Unreachable,
            Self::BudgetExhausted => FailReason::BudgetExhausted,
            Self::NoRouteRouter | Self::NoRouteStepContinuity => FailReason::NoRoute,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::UnreachableConnectivity => "Unreachable (connectivity)",
            Self::UnreachableAstar => "Unreachable (A*)",
            Self::BudgetExhausted => "BudgetExhausted",
            Self::NoRouteRouter => "NoRoute (router)",
            Self::NoRouteStepContinuity => "NoRoute (continuity)",
        }
    }
}

/// Why the worker considered a path complete.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PathReadyKind {
    /// `segment_path` reaches the requested goal exactly.
    Strict,
    /// `segment_path` ends short of the goal at the closest point reached
    /// before the budget ran out. Caller should re-request on arrival.
    BestEffort,
}

/// One queued request, owned by the request queue until the worker pops it.
///
/// `task_id` and `plan_id` are diagnostic-only: snapshots of the requesting
/// agent's `PersonAI.task_id` / `last_plan_id` at enqueue time. They flow into
/// the `UnreachableAstar` dump so the user can see which task/plan asked for
/// the failing goal even after dispatch has moved on.
#[derive(Clone, Debug)]
pub struct PathRequest {
    pub id: u64,
    pub agent: Entity,
    pub start: (i32, i32, i8),
    pub goal: (i32, i32, i8),
    pub kind: PathKind,
    pub max_budget: u32,
    pub task_id: u16,
    pub plan_id: u16,
}

/// Lifecycle of an agent's `PathFollow`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FollowStatus {
    /// No active goal — `movement_system` should idle/wander.
    Idle,
    /// Request enqueued, worker hasn't run yet. `movement_system` must NOT
    /// move the agent — eliminates twitching toward unreachable targets.
    Pending,
    /// `segment_path` is populated; agent is walking it tile-by-tile.
    Following,
    /// Most recent attempt failed. Caller layer reads the reason and
    /// either re-requests with a different goal or releases to idle.
    Failed(FailReason),
}

/// Per-agent state describing the active path. Written by the worker,
/// consumed by `movement_system`. The fields not listed in the plan
/// (`planning_generation`) record which `ChunkConnectivity` generation we
/// planned against so a graph rebuild can force a replan.
#[derive(Component, Debug)]
pub struct PathFollow {
    pub status: FollowStatus,
    pub goal: (i32, i32, i8),
    /// Sequence of chunks the agent will traverse, including the start
    /// chunk at index 0 and the goal chunk at the back. One A* segment per
    /// chunk hop.
    pub chunk_route: Vec<ChunkCoord>,
    pub route_cursor: u8,
    /// A* output for the *current* chunk segment — list of tiles to step
    /// onto, target inclusive.
    pub segment_path: Vec<(i16, i16, i8)>,
    pub segment_cursor: u16,
    /// `recent_tiles[0]` doubles as the "last tile observed by movement_system"
    /// slot used by the stuck-tick heartbeat. Sentinel `(i16::MIN, i16::MIN, 0)`
    /// means "no observation yet" so the first post-plan tick can't false-match.
    pub recent_tiles: [(i16, i16, i8); 4],
    pub recent_idx: u8,
    pub stuck_ticks: u8,
    pub last_replan_tick: u64,
    /// `ChunkConnectivity::generation` when this path was planned. If the
    /// graph rebuilds (generation bumps) the path may be stale.
    pub planning_generation: u32,
    /// Request id this follow was last populated by — lets late-arriving
    /// `PathReady`/`PathFailed` events ignore stale requests.
    pub request_id: u64,
    /// Subreason of the most recent failed request for this agent, if any.
    /// Survives the `Failed → Idle` transition so the inspector can still
    /// show what went wrong (with the granular variant — connectivity vs A*
    /// for Unreachable, router vs continuity for NoRoute).
    pub last_fail_subreason: Option<FailSubReason>,
    /// Tick at which `last_fail_subreason` was recorded.
    pub last_fail_tick: u64,
    /// Lifetime per-agent counters, one per `FailSubReason` variant. Never
    /// reset on success or goal change — answer the inspector's "has this
    /// agent ever hit X?" question across the agent's whole lifetime.
    pub fail_count_unreachable_conn: u32,
    pub fail_count_unreachable_astar: u32,
    pub fail_count_budget: u32,
    pub fail_count_no_route_router: u32,
    pub fail_count_no_route_continuity: u32,
    /// Goal of the most recent failed request, used by `movement_system`
    /// to suppress re-enqueueing the same goal for `PATH_FAIL_COOLDOWN_TICKS`.
    pub last_fail_goal: (i32, i32, i8),
    /// Consecutive failures against `last_fail_goal`. Cleared (set to 1)
    /// when a different goal fails. Read by the cooldown gate via
    /// `cooldown_for_streak` so persistent failures back off exponentially
    /// rather than re-issuing every 3 s forever.
    pub last_fail_streak: u8,
    /// Multi-line ASCII dump of terrain around start/goal at the moment of
    /// the most recent `UnreachableAstar` failure. Populated by `worker.rs`
    /// and rendered by the inspector. Cleared on successful path build.
    pub last_astar_dump: Option<String>,
    /// Tick when `plan_execution_system` last fired the `ReturnToSurface`
    /// recovery for this agent. Stops the recovery from firing every tick
    /// once it's already underway.
    pub last_recovery_tick: u64,
    /// Value of `fail_count_unreachable_conn` at the last recovery attempt.
    /// Recovery re-fires only after at least 3 *new* connectivity failures
    /// have accumulated since the previous attempt — single transient
    /// connectivity misses don't yank the agent off-task.
    pub last_recovery_conn_count: u32,
}

impl Default for PathFollow {
    fn default() -> Self {
        Self {
            status: FollowStatus::Idle,
            goal: (0, 0, 0),
            chunk_route: Vec::new(),
            route_cursor: 0,
            segment_path: Vec::new(),
            segment_cursor: 0,
            recent_tiles: [(i16::MIN, i16::MIN, 0); 4],
            recent_idx: 0,
            stuck_ticks: 0,
            last_replan_tick: 0,
            planning_generation: 0,
            request_id: 0,
            last_fail_subreason: None,
            last_fail_tick: 0,
            fail_count_unreachable_conn: 0,
            fail_count_unreachable_astar: 0,
            fail_count_budget: 0,
            fail_count_no_route_router: 0,
            fail_count_no_route_continuity: 0,
            last_fail_goal: (i32::MIN, i32::MIN, 0),
            last_fail_streak: 0,
            last_astar_dump: None,
            last_recovery_tick: 0,
            last_recovery_conn_count: 0,
        }
    }
}

/// Pending path requests, drained at most `PATH_BUDGET_PER_TICK` per tick
/// by `drain_path_requests_system`. Dedupes per agent: enqueueing for an
/// agent that already has a pending request replaces the old one in place
/// (most recent goal wins, no stale work in the queue).
#[derive(Resource, Default)]
pub struct PathRequestQueue {
    queue: VecDeque<PathRequest>,
    pending: AHashSet<Entity>,
    next_id: u64,
}

impl PathRequestQueue {
    /// Enqueue (or replace) a request. Returns the request id assigned.
    pub fn enqueue(
        &mut self,
        agent: Entity,
        start: (i32, i32, i8),
        goal: (i32, i32, i8),
        kind: PathKind,
        max_budget: u32,
        task_id: u16,
        plan_id: u16,
    ) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let req = PathRequest {
            id,
            agent,
            start,
            goal,
            kind,
            max_budget,
            task_id,
            plan_id,
        };
        if self.pending.contains(&agent) {
            for slot in self.queue.iter_mut() {
                if slot.agent == agent {
                    *slot = req;
                    return id;
                }
            }
        }
        self.pending.insert(agent);
        self.queue.push_back(req);
        id
    }

    pub fn pop(&mut self) -> Option<PathRequest> {
        let req = self.queue.pop_front()?;
        self.pending.remove(&req.agent);
        Some(req)
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn is_pending(&self, agent: Entity) -> bool {
        self.pending.contains(&agent)
    }

    /// Remove any queued request for `agent` and clear its pending flag.
    /// Used by the inspector's "Force replan" button so a stuck request
    /// can be torn down without waiting for the worker to drain it.
    pub fn cancel_for_agent(&mut self, agent: Entity) {
        if !self.pending.remove(&agent) {
            return;
        }
        self.queue.retain(|r| r.agent != agent);
    }
}

/// Emitted when the worker successfully writes a `PathFollow`.
#[derive(Event, Debug)]
pub struct PathReady {
    pub agent: Entity,
    pub request_id: u64,
    pub kind: PathReadyKind,
}

/// Emitted when the worker rejects or abandons a path request.
#[derive(Event, Debug)]
pub struct PathFailed {
    pub agent: Entity,
    pub request_id: u64,
    pub reason: FailReason,
}

/// Runtime-toggleable knobs for debugging the worker. Lives as a Bevy
/// resource and is read by the worker, the gizmo systems, and a handful of
/// gated `info!`/`debug!` calls.
#[derive(Resource, Default, Debug)]
pub struct PathDebugFlags {
    /// When true, otherwise-silent failure paths emit `info!` lines.
    pub verbose_logs: bool,
    /// When true, the worker drains zero requests per tick. Queue grows.
    pub worker_paused: bool,
}

/// One entry in the `FailureLog` ring buffer.
#[derive(Clone, Debug)]
pub struct FailureRecord {
    pub tick: u64,
    pub agent: Entity,
    pub start: (i32, i32, i8),
    pub goal: (i32, i32, i8),
    pub subreason: FailSubReason,
}

/// Maximum number of failure records retained globally.
pub const FAILURE_LOG_CAP: usize = 128;

/// Bounded ring buffer of recent `PathFailed` events. The debug overlay
/// reads this for the recent-failures markers and per-agent failure history.
#[derive(Resource, Default)]
pub struct FailureLog {
    pub recent: VecDeque<FailureRecord>,
}

impl FailureLog {
    pub fn push(&mut self, rec: FailureRecord) {
        if self.recent.len() >= FAILURE_LOG_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(rec);
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }

    /// Iterate failures for one agent, newest first.
    pub fn for_agent(&self, agent: Entity) -> impl Iterator<Item = &FailureRecord> {
        self.recent.iter().rev().filter(move |r| r.agent == agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy(id: u32) -> Entity {
        Entity::from_raw(id)
    }

    #[test]
    fn enqueue_then_pop_round_trip() {
        let mut q = PathRequestQueue::default();
        let a = dummy(1);
        let id = q.enqueue(
            a,
            (0, 0, 0),
            (5, 5, 0),
            PathKind::Strict,
            DEFAULT_PATH_BUDGET,
            0,
            0,
        );
        assert_eq!(q.len(), 1);
        assert!(q.is_pending(a));
        let r = q.pop().unwrap();
        assert_eq!(r.id, id);
        assert!(!q.is_pending(a));
        assert!(q.is_empty());
    }

    #[test]
    fn dedupe_replaces_request_for_same_agent() {
        let mut q = PathRequestQueue::default();
        let a = dummy(1);
        q.enqueue(
            a,
            (0, 0, 0),
            (5, 5, 0),
            PathKind::Strict,
            DEFAULT_PATH_BUDGET,
            0,
            0,
        );
        q.enqueue(
            a,
            (0, 0, 0),
            (9, 9, 0),
            PathKind::Strict,
            DEFAULT_PATH_BUDGET,
            0,
            0,
        );
        assert_eq!(q.len(), 1, "dedupe should keep one entry per agent");
        let r = q.pop().unwrap();
        assert_eq!(r.goal, (9, 9, 0), "newest enqueue should win");
    }

    #[test]
    fn distinct_agents_keep_distinct_requests() {
        let mut q = PathRequestQueue::default();
        q.enqueue(dummy(1), (0, 0, 0), (1, 0, 0), PathKind::Strict, 1000, 0, 0);
        q.enqueue(dummy(2), (0, 0, 0), (2, 0, 0), PathKind::Strict, 1000, 0, 0);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn cooldown_grows_with_streak_and_caps() {
        assert_eq!(cooldown_for_streak(0), 0);
        assert_eq!(cooldown_for_streak(1), PATH_FAIL_COOLDOWN_MS);
        assert!(cooldown_for_streak(2) > cooldown_for_streak(1));
        assert!(cooldown_for_streak(3) > cooldown_for_streak(2));
        assert_eq!(cooldown_for_streak(255), PATH_FAIL_COOLDOWN_MAX_MS);
    }
}
