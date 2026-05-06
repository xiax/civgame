//! HTN (Hierarchical Task Network) domain — Phase 5 of the Plan/Task System
//! Redesign.
//!
//! Today every goal flows through `plan_execution_system` (linear scoring over
//! a static plan registry) or the residual `goal_dispatch_system` arms (Sleep
//! only, since Phase 4c). Phase 5 stands up a parallel decomposition path:
//! abstract tasks expand via the highest-utility applicable `Method` into a
//! sequence of typed `Task`s that the existing `ActionQueue` already runs.
//!
//! **Phase 5a-ii (current state):** `htn_dispatch_system` (ParallelB, after
//! `goal_dispatch_system`) now consumes the registry for `AgentGoal::Sleep`.
//! For each tired agent it builds a `PlannerCtx` from live ECS queries, asks
//! `MethodRegistry::methods_for(AbstractTaskKind::Sleep)` for the
//! argmax-utility-applicable method, calls `expand`, and dispatches the
//! resulting `Task::Sleep { bed }` via `aq.dispatch` while `assign_task_with_routing`
//! handles the legacy `task_id` channel. The three-branch routing decision
//! (own-bed / faction-home / in-place) reads the same context the method
//! used, so the observable behaviour matches the legacy Sleep arm that this
//! PR deletes. Only one method is registered today (`SleepMethod`) and only
//! one abstract task is consumed (`Sleep`); the dispatch loop is shaped so a
//! second method or kind lands as a registry entry plus a routing branch
//! match arm — no new system per goal.
//!
//! Design notes:
//! - `PlannerCtx` is a *borrowed* snapshot built per-decision rather than a
//!   long-lived component. Methods read the fields they need; that keeps
//!   feature extraction local to each method (the post-Phase-6 shape) instead
//!   of routing through a 42-dim state vector.
//! - `expand` returns `Vec<Task>` for now. The hot path will eventually want a
//!   stack-allocated buffer (matching `ActionQueue::queued`'s `[Task; 4]`),
//!   but a single `Sleep` method that produces one task isn't the right place
//!   to optimise — bench once Phase 5 has 5+ methods running.
//! - `MethodFlags` is a plain `u8` bitmask (no `bitflags` crate per the
//!   no-new-deps rule). Mirrors `PlanFlags` in `plan/mod.rs`.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::construction::{Bed, HomeBed};
use crate::simulation::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::MemoryKind;
use crate::simulation::needs::{Needs, EAT_TRIGGER_HUNGER};
use crate::simulation::person::{AiState, Drafted, PersonAI, PlayerOrder, Profession};
use crate::simulation::plan::ActivePlan;
use crate::simulation::production::total_edible;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::TechId;
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;

/// Abstract goals the planner can decompose. Each variant carries any
/// parameters the methods need to discriminate (none for the three current
/// kinds; future variants like `AcquireGood { good, qty }` will carry their
/// args).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbstractTask {
    Sleep,
    /// Cover the agent's hunger right now using whatever they're already
    /// carrying. Decomposes into a single `Task::Eat`. The "spend what you
    /// have" leaf of the hunger arc — see `AcquireFood` for the "go get
    /// more" branch.
    Eat,
    /// Acquire food the agent doesn't yet have, to be eaten on arrival.
    /// Methods under this kind walk to a food source (storage / forage tile /
    /// scavenge target / hunt) and chain a final `Task::Eat` so the agent
    /// transitions from hunger → action → satiation in a single decomposition.
    /// 5b-iii-i registers `WithdrawFromStorageMethod` as the first method;
    /// future Forage/Scavenge/Hunt methods land here too.
    AcquireFood,
    /// Acquire one unit of an arbitrary material (Wood / Stone / Iron / …)
    /// the agent doesn't yet have. Phase 5c collapses the per-good legacy
    /// plans (`GatherWood` / `GatherStone` / `WithdrawClaimedHaul…` / …) into
    /// a single parameterised abstract task; the `good` payload threads the
    /// target through to the methods so one method can serve every material
    /// (a contrast to the 5b-iii-i `AcquireFood` shape where "food" was the
    /// fixed implicit category).
    ///
    /// Scaffolding only at 5c-i: `WithdrawMaterialFromStorageMethod` is
    /// registered, but no dispatcher consumes `AbstractTaskKind::AcquireGood`
    /// yet. 5c-ii adds the dispatcher and starts deleting per-good plans.
    AcquireGood { good: Good },
}

/// Discriminant-only key for `MethodRegistry` lookups. `AbstractTask` itself
/// can't be a hash key once variants carry payloads, so the registry indexes
/// on this kind enum and methods read their parameters from the full
/// `AbstractTask` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AbstractTaskKind {
    Sleep,
    Eat,
    AcquireFood,
    AcquireGood,
}

impl AbstractTask {
    pub fn kind(self) -> AbstractTaskKind {
        match self {
            AbstractTask::Sleep => AbstractTaskKind::Sleep,
            AbstractTask::Eat => AbstractTaskKind::Eat,
            AbstractTask::AcquireFood => AbstractTaskKind::AcquireFood,
            AbstractTask::AcquireGood { .. } => AbstractTaskKind::AcquireGood,
        }
    }
}

/// Per-method bitflags. Mirrors `PlanFlags` in `plan/mod.rs`. Empty for
/// 5a-i's lone Sleep method.
pub type MethodFlags = u8;
pub const MF_UNINTERRUPTIBLE: MethodFlags = 1 << 0;

/// Tile-level chebyshev (king's-move) distance, the same metric `SpatialIndex`
/// scans use. Used by method `utility()` bodies to bias toward closer targets.
fn chebyshev_dist(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Per-tile utility penalty for a method's target distance. Phase 5c-ii-d-v
/// ("distance-weighted utility"): closer targets win when two methods would
/// otherwise tie on base utility. Total penalty is capped at
/// `MAX_DIST_PENALTY` so a far target can't undercut a method that
/// outranked it on base utility — `ScavengeFromGround` (1.5) beats
/// `GatherFromKnown` (1.0) by at least 0.20 even at the worst-case 15-tile
/// scavenge target paired with a zero-distance gather target. Likewise
/// `WithdrawAndHaulToBlueprint` (2.0) keeps a 0.70+ margin over any sibling
/// at any distance.
const DIST_DISCOUNT_PER_TILE: f32 = 0.02;
const MAX_DIST_PENALTY: f32 = 0.30;

/// Compute the distance-weighted discount for a method whose target tile is
/// `target`. Returns 0 when `target.is_none()` so methods that haven't been
/// populated by the dispatcher (or unit tests with `ctx_empty()`) score at
/// their flat base utility.
fn dist_penalty(agent: (i32, i32), target: Option<(i32, i32)>) -> f32 {
    match target {
        Some(t) => {
            let d = chebyshev_dist(agent, t) as f32;
            (d * DIST_DISCOUNT_PER_TILE).min(MAX_DIST_PENALTY)
        }
        None => 0.0,
    }
}

/// Snapshot of the agent + world state a `Method` needs to make a decision.
/// Constructed per-agent per-decision-tick by the (future) HTN dispatch
/// system; methods borrow it immutably.
///
/// Phase 5a-i populates only the fields the `SleepMethod` actually reads.
/// New fields land on demand as methods are added — no speculative coverage.
#[derive(Clone, Copy, Debug)]
pub struct PlannerCtx {
    /// The agent's current tile (x, y).
    pub tile: (i32, i32),
    /// The agent's faction id. `SOLO=0` if ungrouped.
    pub faction_id: u32,
    /// The faction's `home_tile`, if any. `None` for SOLO or unsettled
    /// factions.
    pub faction_home: Option<(i32, i32)>,
    /// The bed entity claimed by `HomeBed`, if any.
    pub home_bed: Option<Entity>,
    /// World position of the claimed bed (looked up from `bed_query`), if the
    /// claim is still live. `None` if `home_bed` is `None` or the claim is
    /// stale.
    pub home_bed_tile: Option<(i32, i32)>,
    /// Total edible quantity the agent is carrying across inventory + hands.
    /// Read by `EatFromInventoryMethod`. Sleep methods ignore the field; the
    /// dispatcher leaves it at zero in PlannerCtx snapshots they consume.
    pub edible_count: u32,
    /// Current `Needs.hunger` (range 0..=255 conceptually, stored as f32).
    /// Read by `EatFromInventoryMethod` to gate on `EAT_TRIGGER_HUNGER`.
    /// Sleep methods ignore the field.
    pub hunger: f32,
    /// Nearest faction-owned storage tile that holds at least one edible.
    /// `None` when the agent has no faction (`SOLO`), the faction has no
    /// storage tiles, or none of them currently stock food. Read by
    /// `WithdrawFromStorageMethod` (5b-iii-i) to seed the head of an
    /// `AcquireFood` chain. Eat / Sleep dispatchers leave it `None`.
    pub nearest_storage_tile: Option<(i32, i32)>,
    /// Total edible-units summed across the faction's storage tiles. Read by
    /// `WithdrawFromStorageMethod`'s precondition + utility — the gate on
    /// `>0` is what distinguishes "go withdraw food" from "explore for
    /// food" when the agent is hungry but has nothing in hand. Eat / Sleep
    /// dispatchers leave it at zero.
    pub faction_food_stock: u32,
    /// Nearest faction storage tile that holds at least one unit of the
    /// `AcquireGood`'s target material. Read by
    /// `WithdrawMaterialFromStorageMethod` (5c-i) to seed the head of an
    /// `AcquireGood` decomposition. Sleep / Eat / AcquireFood dispatchers
    /// leave it at `None`. Unlike `nearest_storage_tile` (food-specific) the
    /// 5c-ii dispatcher will populate this from a per-good lookup, since
    /// storage tiles aren't food-specific in the underlying map and the
    /// "stock here for THIS good" question can't be answered by
    /// `StorageTileMap::nearest_for_faction` alone.
    pub material_storage_tile: Option<(i32, i32)>,
    /// Total stock of the `AcquireGood` target material across the faction's
    /// storage. Read by `WithdrawMaterialFromStorageMethod`'s precondition.
    /// Sleep / Eat / AcquireFood dispatchers leave it at zero.
    pub material_stock_for_target: u32,
    /// The blueprint entity the agent is currently committed to delivering
    /// material into, if any. Populated by `htn_acquire_good_dispatch_system`
    /// from the `JobClaim::Haul` companion `ClaimTarget`. Read by
    /// `WithdrawAndHaulToBlueprintMethod` (5c-ii-b) so the chain's terminal
    /// `Task::HaulToBlueprint` carries the blueprint without re-querying.
    /// Sleep / Eat / AcquireFood / single-task AcquireGood dispatchers leave
    /// it at `None`.
    pub claimed_blueprint: Option<Entity>,
    /// A known harvest tile for the `AcquireGood` target material — a tree
    /// for Wood, a stone tile for Stone, a berry bush for Fruit, etc. Read
    /// by `GatherFromKnownMethod` (Phase 5c-ii-c) to seed the head of a
    /// gather chain. Populated from the agent's `Memory` (or `SpatialIndex`
    /// when in vis range) by the future `htn_acquire_good_dispatch_system`
    /// extension that fires under `AgentGoal::GatherWood` / `GatherStone`.
    /// Sleep / Eat / AcquireFood / haul-claim AcquireGood dispatchers leave
    /// it at `None`.
    pub gather_target_tile: Option<(i32, i32)>,
    /// A known loose `GroundItem` of the `AcquireGood` target material —
    /// fallen wood / surface stone / dropped fruit, etc. Paired with
    /// `scavenge_target_tile` (the entity's current tile) so the dispatcher
    /// can route there before the chain runs. Read by
    /// `ScavengeFromGroundMethod` (Phase 5c-ii-d-i) to seed the head of a
    /// scavenge chain. Populated from the agent's vision / memory by the
    /// future `htn_acquire_good_dispatch_system` scavenge branch (Phase
    /// 5c-ii-d-ii) that replaces the legacy `ScavengeWood` / `ScavengeStone`
    /// / `ScavengeFood` plans (PlanId 38 / 39 / 6).
    /// Sleep / Eat / AcquireFood / haul-claim / gather AcquireGood
    /// dispatchers leave it at `None`.
    pub scavenge_target_entity: Option<Entity>,
    /// World tile of `scavenge_target_entity`, snapshot at decision time.
    /// Required for routing because `ScavengeFromGroundMethod`'s expansion
    /// terminates in a `Task::DepositToFactionStorage`, and the dispatcher
    /// needs the tile to dispatch the head `Task::Scavenge { target }` via
    /// `assign_task_with_routing`. Same `None` semantics as
    /// `scavenge_target_entity`.
    pub scavenge_target_tile: Option<(i32, i32)>,
}

/// A single decomposition rule for an `AbstractTask`. Scoring (`utility`) and
/// gating (`precondition`) are decoupled so the dispatcher can short-circuit
/// when no method is applicable.
pub trait Method: Send + Sync + 'static {
    /// Hard gate. Methods that fail `precondition` are never selected,
    /// regardless of utility.
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool;

    /// Soft score; higher is better. The dispatcher picks the
    /// argmax-applicable method (with ε-greedy injected at the dispatch
    /// layer, not here).
    fn utility(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32;

    /// Decompose into a sequence of typed tasks. The first task becomes
    /// `aq.current`; the rest get pushed onto the prefetched queue. May
    /// return an empty vec, in which case the dispatcher treats this method
    /// as inapplicable (defensive — ideally `precondition` covered it).
    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task>;

    fn flags(&self) -> MethodFlags {
        0
    }

    fn tech_gate(&self) -> Option<TechId> {
        None
    }

    fn profession_gate(&self) -> Option<Profession> {
        None
    }

    /// Static name for debug / inspector display. Keep these short and
    /// human-recognisable.
    fn name(&self) -> &'static str;
}

/// Registry of methods keyed by abstract-task kind. Populated once at startup
/// (`register_builtin_methods`) and read-only thereafter. Held as a Bevy
/// `Resource` so dispatch systems can borrow it immutably in parallel.
#[derive(Resource, Default)]
pub struct MethodRegistry {
    by_kind: AHashMap<AbstractTaskKind, Vec<Box<dyn Method>>>,
}

impl MethodRegistry {
    pub fn register(&mut self, kind: AbstractTaskKind, method: Box<dyn Method>) {
        self.by_kind.entry(kind).or_default().push(method);
    }

    pub fn methods_for(&self, kind: AbstractTaskKind) -> &[Box<dyn Method>] {
        self.by_kind
            .get(&kind)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn method_count(&self, kind: AbstractTaskKind) -> usize {
        self.methods_for(kind).len()
    }
}

/// Wire up the built-in method library. Called from `SimulationPlugin::build`.
pub fn register_builtin_methods(reg: &mut MethodRegistry) {
    reg.register(AbstractTaskKind::Sleep, Box::new(SleepMethod));
    reg.register(AbstractTaskKind::Eat, Box::new(EatFromInventoryMethod));
    reg.register(
        AbstractTaskKind::AcquireFood,
        Box::new(WithdrawFromStorageMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireFood,
        Box::new(ScavengeFoodFromGroundMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireGood,
        Box::new(WithdrawMaterialFromStorageMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireGood,
        Box::new(WithdrawAndHaulToBlueprintMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireGood,
        Box::new(GatherFromKnownMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireGood,
        Box::new(ScavengeFromGroundMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireFood,
        Box::new(ExploreForFoodMethod),
    );
    reg.register(
        AbstractTaskKind::AcquireGood,
        Box::new(ExploreForMaterialMethod),
    );
}

/// Sole method for `AbstractTask::Sleep`. Mirrors the three-branch decision
/// tree in `goal_dispatch_system`'s Sleep arm:
///
/// 1. If we have a live `HomeBed` claim and know the bed's tile, route there
///    (`Task::Sleep { bed: Some(_) }`).
/// 2. Else if the faction has a `home_tile` and we're outside the 5-tile
///    home disc, route home (`Task::Sleep { bed: None }`).
/// 3. Else sleep in place (`Task::Sleep { bed: None }`, with the dispatcher
///    setting `AiState::Sleeping` directly — handled at the system level,
///    not here).
///
/// All three branches expand to a single `Task::Sleep` because routing /
/// state-transition is downstream of the typed task. The variant exists to
/// make Sleep visible in the typed channel and to carry the bed claim across
/// the `Working → Sleeping` transition.
pub struct SleepMethod;

impl Method for SleepMethod {
    fn precondition(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> bool {
        true
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        1.0
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let bed = ctx.home_bed.filter(|_| ctx.home_bed_tile.is_some());
        vec![Task::Sleep { bed }]
    }

    fn name(&self) -> &'static str {
        "Sleep"
    }
}

/// Sole pre-Phase-5b-ii method for `AbstractTask::Eat`. Mirrors the legacy
/// `EatFromInventory` plan (PlanId 25, single step `Eat` with
/// `eat_when_hungry(EAT_TRIGGER_HUNGER)` precondition): the agent must be
/// holding an edible *and* be at or above the trigger hunger. Expansion is a
/// single in-place `Task::Eat` because the Eat executor inspects inventory +
/// hands itself; there are no parameters to thread.
///
/// The method exists at 5b-i as scaffolding — `register_builtin_methods` adds
/// it to the registry but no dispatcher consumes `AbstractTaskKind::Eat` yet,
/// so behaviour is unchanged. 5b-ii will wire it into the live runtime
/// alongside (or in place of) the legacy plan-execution candidate that fires
/// today under `AgentGoal::Survive`.
pub struct EatFromInventoryMethod;

impl Method for EatFromInventoryMethod {
    fn precondition(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        // Legacy parity: `eat_when_hungry` requires `requires_any_edible` AND
        // `hunger >= EAT_TRIGGER_HUNGER`. The plan registry triggers at 180.
        ctx.edible_count > 0 && ctx.hunger >= EAT_TRIGGER_HUNGER as f32
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Single-method registry today, so any positive value wins. 1.0
        // matches `SleepMethod`; future Eat methods (e.g. EatFromCarriedFood
        // with a freshness preference) will discriminate here.
        1.0
    }

    fn expand(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        vec![Task::Eat]
    }

    fn name(&self) -> &'static str {
        "EatFromInventory"
    }
}

/// Sole pre-Phase-5b-iii-ii method for `AbstractTask::AcquireFood`. Mirrors the
/// legacy `WithdrawAndEat` plan (PlanId 9): walk to the nearest faction storage
/// tile that holds an edible, pick one up, eat it. Expansion is a two-task
/// chain — `[Task::WithdrawFood { tile }, Task::Eat]` — which is the first
/// place in the runtime where a method body produces more than one task.
/// `htn_acquire_food_dispatch_system` (lands in 5b-iii-ii) will route the head
/// `WithdrawFood` via `assign_task_with_routing` and `enqueue` the trailing
/// `Eat` onto the prefetch ring; on the executor's `advance()` after the
/// withdraw finishes, the `Eat` task promotes into `aq.current` without
/// re-entering plan selection.
///
/// Precondition gates on:
/// - `faction_food_stock > 0` and `nearest_storage_tile.is_some()` — there
///   must be food to withdraw and a tile to walk to;
/// - `hunger >= EAT_TRIGGER_HUNGER` — same hunger bar as
///   `EatFromInventoryMethod` so the agent only commits to a withdraw trip
///   when actually hungry.
///
/// Note: the precondition does *not* gate on `edible_count == 0`. In practice
/// the dispatcher will defer to `htn_eat_dispatch_system` (which fires first
/// in ParallelB ordering) when the agent already has food on hand — but if
/// both methods become applicable (e.g. the agent has one edible but more
/// stock at home) this method's utility just needs to score lower than
/// `EatFromInventoryMethod`'s. 5b-iii-i keeps both at `1.0`; the distinction
/// becomes meaningful when the dispatcher and ε-greedy land in 5b-iii-ii.
pub struct WithdrawFromStorageMethod;

impl Method for WithdrawFromStorageMethod {
    fn precondition(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        ctx.faction_food_stock > 0
            && ctx.nearest_storage_tile.is_some()
            && ctx.hunger >= EAT_TRIGGER_HUNGER as f32
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Phase 5c-ii-d-v: base 1.0 minus chebyshev distance to the storage
        // tile (capped at MAX_DIST_PENALTY). When two methods both apply, the
        // closer target wins. Sibling `ScavengeFoodFromGroundMethod` (base
        // 1.5) keeps a >=0.20 margin even at the worst-case dist-spread
        // because both methods clamp at the same MAX_DIST_PENALTY.
        1.0 - dist_penalty(ctx.tile, ctx.nearest_storage_tile)
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        // Defensive: the precondition guarantees Some(_), but a method body
        // shouldn't unwrap on a ctx field. If a future caller skips the
        // precondition, an empty expansion makes the dispatcher treat this
        // method as inapplicable.
        let Some(tile) = ctx.nearest_storage_tile else {
            return Vec::new();
        };
        vec![Task::WithdrawFood { tile }, Task::Eat]
    }

    fn name(&self) -> &'static str {
        "WithdrawFromStorage"
    }
}

/// Sole pre-Phase-5c-ii-d-iii-ii method for `AbstractTask::AcquireFood`'s
/// scavenge branch. Mirrors the legacy `ScavengeFood` plan (PlanId 6, two-step
/// `[CollectFood, DepositGoods]`) but reshapes the chain for the
/// hunger-driven `AcquireFood` flow: instead of depositing the picked-up food
/// at faction storage and then re-walking back to withdraw + eat, the agent
/// scavenges and eats in place. The legacy plan's deposit-then-withdraw
/// round-trip was wasted motion — `AcquireFood` only fires under hunger, so
/// the food the agent just picked up is exactly what they want to eat now.
///
/// Reuses the existing `scavenge_target_entity` / `scavenge_target_tile` ctx
/// fields populated by the future `htn_acquire_food_dispatch_system`
/// scavenge branch (5c-ii-d-iii-ii). The dispatcher will scan `SpatialIndex`
/// within `VIEW_RADIUS=15` for matching edible `GroundItem`s (analogous to
/// the 5c-ii-d-ii-a Wood/Stone scan), populate `scavenge_target_*` per
/// decision, and route the head `Task::Scavenge { target }` via
/// `assign_task_with_routing`. The trailing `Task::Eat` rides the prefetch
/// ring; on `item_pickup_system`'s `finish_scavenge` exit it promotes into
/// `aq.current` and the legacy channel primes (`task_id = TaskKind::Eat`,
/// `state = Working`, `work_progress = 0`) so `eat_task_system` picks up on
/// the next tick. **The chain shape `[Scavenge, Eat]` is the first
/// `AcquireFood` chain that doesn't end in storage withdraw** — it
/// short-circuits the legacy plan's deposit-then-withdraw round trip when
/// the agent is hungry and finds food already on the ground.
///
/// Precondition gates on:
/// - `scavenge_target_entity.is_some() && scavenge_target_tile.is_some()` —
///   paired-field requirement matching `ScavengeFromGroundMethod` (entity is
///   the executor's input; tile is the dispatcher's input);
/// - `hunger >= EAT_TRIGGER_HUNGER` — defence in depth even though the
///   `htn_acquire_food_dispatch_system` already pre-filters on this. Mirrors
///   `WithdrawFromStorageMethod`'s hunger gate so the two AcquireFood
///   methods are symmetric.
///
/// Utility `1.5` — bias-on-visibility above `WithdrawFromStorageMethod`'s
/// `1.0`. Parity with `ScavengeFromGroundMethod`'s 1.5 under AcquireGood:
/// when both AcquireFood methods are applicable (loose food on the ground
/// AND faction storage stocked), the closer scavenge target wins. Real
/// utility-tuning (dist-weighted scoring) is a Phase 6 question.
///
/// **GatherFood goal not handled here.** The legacy `ScavengeFood` plan
/// (PlanId 6) also serves `AgentGoal::GatherFood` — the chief-driven "fill
/// storage" path that doesn't gate on hunger. That path's ideal expansion
/// is `[Scavenge, DepositToFactionStorage { food_good }]`, which needs the
/// food good to thread through the deposit task — a per-good ctx field
/// (e.g. `scavenge_food_good: Option<Good>`) the dispatcher would populate
/// from the picked-up `GroundItem`. 5c-ii-d-iii-ii will decide between
/// (a) extending this method with conditional expansion based on goal +
/// hunger, (b) adding a sibling `ScavengeFoodForStorageMethod`, or (c)
/// keeping PlanId 6 around just for the GatherFood goal. The scaffold here
/// commits to the hunger-driven `[Scavenge, Eat]` shape only.
///
/// Scaffolding only at 5c-ii-d-iii-i: `register_builtin_methods` wires the
/// method into the registry but no dispatcher consumes the AcquireFood
/// scavenge branch yet. The legacy `ScavengeFood` plan remains
/// authoritative; 5c-ii-d-iii-ii will add the dispatch system extension and
/// the PlanId 6 deletion (or the GatherFood-only retention).
pub struct ScavengeFoodFromGroundMethod;

impl Method for ScavengeFoodFromGroundMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        // Defensive: this method is only registered under `AcquireFood`, but
        // a future caller that mis-routes the wrong abstract-task variant
        // gets a clean `false` rather than a wrong-shape expansion.
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return false;
        }
        ctx.scavenge_target_entity.is_some()
            && ctx.scavenge_target_tile.is_some()
            && ctx.hunger >= EAT_TRIGGER_HUNGER as f32
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Bias-on-visibility (base 1.5): outranks `WithdrawFromStorageMethod`
        // (base 1.0). Phase 5c-ii-d-v adds a distance discount on top so the
        // closer of two visible loose-food piles wins. Capped at
        // `MAX_DIST_PENALTY` so a far visible item never falls below
        // `WithdrawFromStorageMethod` at zero distance (1.5 - 0.30 = 1.20 >
        // 1.0).
        1.5 - dist_penalty(ctx.tile, ctx.scavenge_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        // Defensive: precondition guarantees both, but a wrong-variant
        // caller or a partially-populated ctx still gets a sane empty vec.
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return Vec::new();
        }
        let Some(target) = ctx.scavenge_target_entity else {
            return Vec::new();
        };
        if ctx.scavenge_target_tile.is_none() {
            return Vec::new();
        }
        vec![Task::Scavenge { target }, Task::Eat]
    }

    fn name(&self) -> &'static str {
        "ScavengeFoodFromGround"
    }
}

/// Sole pre-Phase-5c-ii method for `AbstractTask::AcquireGood { good }`. The
/// material analogue of `WithdrawFromStorageMethod`: reads the target good from
/// the abstract task, gates on the per-good ctx fields the dispatcher will
/// populate, and expands to a single `Task::WithdrawMaterial { good, qty: 1 }`.
///
/// Three things to flag for the 5c-ii dispatcher PR:
///
/// 1. **Single-task expansion.** Unlike `WithdrawFromStorageMethod`'s
///    `[WithdrawFood, Eat]` two-task chain, withdrawing a *material* doesn't
///    have an automatic terminal step — the agent fetches the good and stops.
///    Whatever consumes the material (a blueprint, a craft order, a deposit)
///    is its own decomposition; chaining belongs there, not here. If 5c-ii
///    wants a "withdraw → deposit at construction site" pattern, that's a
///    separate `AbstractTask` (e.g. `DeliverGood`) whose method emits the
///    full chain — not a tail on this method.
///
/// 2. **`qty: 1` is the simplest contract.** The legacy
///    `WithdrawClaimedHaul…` plans bake in claim-based qty; that plumbing
///    arrives with `AbstractTask::FulfillClaim` (post-5c). For now,
///    "acquire one of X" is the unit decomposition; chained calls handle
///    larger needs.
///
/// 3. **The good lives on the abstract task, not the ctx.** The 5c-ii
///    dispatcher will iterate over outstanding material needs and call
///    `expand(AbstractTask::AcquireGood { good }, &ctx)` per need; the ctx's
///    `material_stock_for_target` / `material_storage_tile` are the
///    per-decision snapshot for that one good, not a map.
pub struct WithdrawMaterialFromStorageMethod;

impl Method for WithdrawMaterialFromStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        // Defensive: this method is only registered under `AcquireGood`, but
        // a future caller that mis-routes the wrong abstract-task variant
        // gets a clean `false` rather than a wrong-good expansion.
        if !matches!(abstract_task, AbstractTask::AcquireGood { .. }) {
            return false;
        }
        ctx.material_stock_for_target > 0 && ctx.material_storage_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Phase 5c-ii-d-v: base 1.0 minus chebyshev distance to the material
        // storage tile (capped at MAX_DIST_PENALTY). Mirrors
        // `WithdrawFromStorageMethod`'s shape — same base, same penalty
        // schedule, different ctx field.
        1.0 - dist_penalty(ctx.tile, ctx.material_storage_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return Vec::new();
        };
        // Defensive: precondition guarantees Some(_), but a method body
        // shouldn't unwrap on a ctx field.
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![Task::WithdrawMaterial { good, qty: 1 }]
    }

    fn name(&self) -> &'static str {
        "WithdrawMaterialFromStorage"
    }
}

/// Phase 5c-ii-b method for `AbstractTask::AcquireGood { good }` when the
/// dispatcher has a concrete delivery blueprint in hand (today: a
/// `JobClaim::Haul` companion `ClaimTarget`). Replaces the legacy
/// `ClaimedHaul` plan (PlanId 33), which encoded the same shape as a two-step
/// plan: `WithdrawClaimedHaulMaterial → HaulToClaimedBlueprint`.
///
/// The expansion is the second multi-task chain in the registry (after
/// `WithdrawFromStorageMethod`'s `[WithdrawFood, Eat]`) — and the first whose
/// trailing leg requires its own routing decision (Eat is in-place; the haul
/// leg has to walk from storage to the blueprint). The routing handoff lives
/// in `withdraw_material_task_system`'s exit (`finish_withdraw_material`),
/// which advances the prefetch ring, looks up the blueprint's tile, and calls
/// `assign_task_with_routing` with `TaskKind::HaulMaterials`. From there
/// `construction_system`'s hauler branch is the executor — it already knows
/// how to deposit-on-arrival via `target_entity = Some(blueprint)`, so no new
/// per-tick task system is needed for the haul leg.
///
/// Utility-vs-`WithdrawMaterialFromStorageMethod`: both sit under
/// `AbstractTaskKind::AcquireGood`, but their preconditions don't overlap —
/// the haul method requires `claimed_blueprint.is_some()`, the bare-withdraw
/// method requires nothing beyond stock+tile. The 5c-ii-b dispatcher only
/// populates `claimed_blueprint` for agents under `AgentGoal::Haul` with a
/// live claim, so the bare-withdraw method never wins on a hauler.
pub struct WithdrawAndHaulToBlueprintMethod;

impl Method for WithdrawAndHaulToBlueprintMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::AcquireGood { .. }) {
            return false;
        }
        ctx.material_stock_for_target > 0
            && ctx.material_storage_tile.is_some()
            && ctx.claimed_blueprint.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Phase 5c-ii-d-v: base 2.0 minus chebyshev distance to the
        // *storage* tile (the haul method's own first hop — the blueprint
        // tile isn't in ctx today, so the storage hop is the only routable
        // distance signal). Stays strictly above
        // `WithdrawMaterialFromStorageMethod`'s base 1.0 even at max
        // penalty (2.0 - 0.30 = 1.70 > 1.0), so a hauler with both methods
        // applicable always picks the chain that actually delivers.
        2.0 - dist_penalty(ctx.tile, ctx.material_storage_tile)
    }

    fn flags(&self) -> MethodFlags {
        // Mirrors the legacy `ClaimedHaul` plan's `PF_UNINTERRUPTIBLE` — once
        // the agent commits to the chain it shouldn't drop it on a routine
        // goal flip. The dispatcher doesn't yet read flags (5a-ii pattern), so
        // this is documentation-of-intent today.
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return Vec::new();
        };
        let Some(blueprint) = ctx.claimed_blueprint else {
            return Vec::new();
        };
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![
            Task::WithdrawMaterial { good, qty: 1 },
            Task::HaulToBlueprint { blueprint },
        ]
    }

    fn name(&self) -> &'static str {
        "WithdrawAndHaulToBlueprint"
    }
}

/// Phase 5c-ii-c method for `AbstractTask::AcquireGood { good }` when the
/// agent has a known harvest tile in memory or visibility (a tree for Wood, a
/// stone tile for Stone, etc.) and faction storage is *not* the cheap answer.
/// Replaces the legacy `GatherWood` / `GatherStone` plans (PlanId 2/3),
/// which encoded the same shape as a two-step plan: `Gather → DepositGoods`.
///
/// The expansion is the third multi-task chain in the registry (after
/// `WithdrawFromStorageMethod`'s `[WithdrawFood, Eat]` and
/// `WithdrawAndHaulToBlueprintMethod`'s `[WithdrawMaterial,
/// HaulToBlueprint]`). Like the haul chain, the trailing leg requires its
/// own routing decision — gather happens at a tree/stone tile somewhere out
/// in the world, deposit happens back at faction storage. The dispatcher
/// (5c-ii-c-ii) will route the head `Task::Gather { tile }`; the chain
/// handoff in `gather_system`'s exit will route to the nearest faction
/// storage tile and prime `TaskKind::DepositResource`. Today that handoff
/// is wired only for plan-driven `StepId(12)` callers — 5c-ii-c-ii adds the
/// HTN-driven path.
///
/// Utility-vs-`WithdrawMaterialFromStorageMethod`: both sit under
/// `AbstractTaskKind::AcquireGood`, but their preconditions are
/// near-disjoint. The bare-withdraw method needs storage stock + tile; this
/// gather method needs a known harvest tile. When *both* fire (rare — the
/// agent both has stock at home and knows where a tree is), the dispatcher
/// will argmax on utility. The legacy plan registry weighted GatherWood
/// against WithdrawAndHaulToBlueprint via a state-vector dot product
/// involving `SI_VIS_TREE` / `SI_MEM_WOOD` / `SI_HAS_WOOD` /
/// `SI_STORAGE_WOOD`; this method uses a flat `1.0` for parity with the
/// other methods and lets the dispatcher's per-good ε-greedy mix keep the
/// behaviour from collapsing to a fixed priority. Real utility-tuning is a
/// post-5c question once Phase 6 method-scoring lands.
///
/// Scaffolding only at 5c-ii-c-i: `register_builtin_methods` wires the
/// method into the registry but no dispatcher consumes the gather chain
/// yet. The legacy `GatherWood` (PlanId 2) and `GatherStone` (PlanId 3)
/// plans remain authoritative; 5c-ii-c-ii adds the dispatch system, the
/// gather-exit handoff into `Task::DepositToFactionStorage`, and the
/// PlanId 2/3 deletion.
pub struct GatherFromKnownMethod;

impl Method for GatherFromKnownMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        // Defensive: this method is only registered under `AcquireGood`, but
        // a future caller that mis-routes the wrong abstract-task variant
        // gets a clean `false` rather than a wrong-good expansion.
        if !matches!(abstract_task, AbstractTask::AcquireGood { .. }) {
            return false;
        }
        ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Phase 5c-ii-d-v: base 1.0 minus chebyshev distance to the gather
        // target tile (capped at MAX_DIST_PENALTY). Parity with the other
        // base-1.0 AcquireGood methods (`WithdrawMaterialFromStorageMethod`),
        // discriminated by target distance when both apply.
        1.0 - dist_penalty(ctx.tile, ctx.gather_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return Vec::new();
        };
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![
            Task::Gather { tile },
            Task::DepositToFactionStorage { good },
        ]
    }

    fn name(&self) -> &'static str {
        "GatherFromKnown"
    }
}

/// Phase 5c-ii-d-i method for `AbstractTask::AcquireGood { good }` when the
/// agent has a known loose `GroundItem` of the target material in vision or
/// memory — fallen wood, surface stone, dropped fruit, etc. Replaces (in
/// 5c-ii-d-ii) the legacy `ScavengeWood` / `ScavengeStone` / `ScavengeFood`
/// plans (PlanId 38 / 39 / 6), each a two-step `[CollectX, DepositGoods]`
/// chain that flagged `PF_SCAVENGE | PF_TARGETS_X`.
///
/// The expansion is the fourth multi-task chain in the registry (after
/// `WithdrawFromStorageMethod`'s `[WithdrawFood, Eat]`,
/// `WithdrawAndHaulToBlueprintMethod`'s `[WithdrawMaterial,
/// HaulToBlueprint]`, and `GatherFromKnownMethod`'s `[Gather,
/// DepositToFactionStorage]`). Like the gather chain, the trailing leg
/// requires its own routing decision — the loose item lives somewhere out in
/// the world (close to the agent if visible, distant if memory-only), and
/// the deposit happens back at faction storage. The future
/// `htn_acquire_good_dispatch_system` scavenge branch (5c-ii-d-ii) will
/// route the head `Task::Scavenge { target }` via `assign_task_with_routing`;
/// the chain handoff in `item_pickup_system`'s exit (mirroring
/// `gather.rs::finish_gather`) will route to the nearest faction storage tile
/// and prime `TaskKind::DepositResource`.
///
/// Utility-vs-`GatherFromKnownMethod` and `WithdrawMaterialFromStorageMethod`:
/// all three sit under `AbstractTaskKind::AcquireGood`, but their
/// preconditions are near-disjoint — the bare-withdraw method gates on
/// `material_storage_tile.is_some()`, the gather method on
/// `gather_target_tile.is_some()`, and this scavenge method on
/// `scavenge_target_entity.is_some()` (paired with the entity's tile). When
/// more than one fires (rare — the agent both has stock at home, knows where
/// a tree is, *and* sees a loose log), the dispatcher will argmax on
/// utility. The legacy plan registry weighted ScavengeWood against GatherWood
/// via a state-vector dot product involving `SI_VIS_GROUND_WOOD` /
/// `SI_HAS_WOOD` / `SI_STORAGE_WOOD`; this method uses a flat `1.0` for
/// parity with the other AcquireGood methods. Real utility-tuning is a
/// post-5c question once Phase 6 method-scoring lands — the Phase 5c-ii-d
/// follow-ups (bias-on-storage / bias-on-visibility) will start
/// differentiating these flat utilities.
///
/// Scaffolding only at 5c-ii-d-i: `register_builtin_methods` wires the method
/// into the registry but no dispatcher consumes the scavenge chain yet. The
/// legacy `ScavengeWood` / `ScavengeStone` / `ScavengeFood` plans remain
/// authoritative; 5c-ii-d-ii adds the dispatch system, the scavenge-exit
/// handoff into `Task::DepositToFactionStorage`, and the PlanId 38/39/6
/// deletion.
pub struct ScavengeFromGroundMethod;

impl Method for ScavengeFromGroundMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        // Defensive: this method is only registered under `AcquireGood`, but
        // a future caller that mis-routes the wrong abstract-task variant
        // gets a clean `false` rather than a wrong-good expansion.
        if !matches!(abstract_task, AbstractTask::AcquireGood { .. }) {
            return false;
        }
        // Both fields must be populated — the entity is the executor's
        // input (`Task::Scavenge { target: Entity }`), the tile is the
        // dispatcher's input (`assign_task_with_routing` needs somewhere to
        // route to). A populated entity without a tile would mean the
        // dispatcher couldn't route the agent there; a populated tile
        // without an entity would mean the executor has nothing to pick up.
        ctx.scavenge_target_entity.is_some() && ctx.scavenge_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Bias-on-visibility (base 1.5): outranks `GatherFromKnownMethod`
        // (base 1.0) so a worker who can see a loose log scavenges it
        // instead of walking past to chop a fresh tree. Phase 5c-ii-d-v adds
        // a distance discount on top so the closer of two visible loose
        // piles wins. Capped at `MAX_DIST_PENALTY` so a far visible item
        // never falls below `GatherFromKnownMethod` at zero distance
        // (1.5 - 0.30 = 1.20 > 1.0). Stays below
        // `WithdrawAndHaulToBlueprintMethod` (base 2.0) on hauler context
        // by the same margin.
        1.5 - dist_penalty(ctx.tile, ctx.scavenge_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return Vec::new();
        };
        let Some(target) = ctx.scavenge_target_entity else {
            return Vec::new();
        };
        // Defensive: precondition requires both, but a method body shouldn't
        // unwrap on a ctx field.
        if ctx.scavenge_target_tile.is_none() {
            return Vec::new();
        }
        vec![
            Task::Scavenge { target },
            Task::DepositToFactionStorage { good },
        ]
    }

    fn name(&self) -> &'static str {
        "ScavengeFromGround"
    }
}

/// Phase 5c-ii-d-iv-i fallback method for `AbstractTask::AcquireFood`. Mirrors
/// the legacy `ExploreForFood` plan (PlanId 35, single step `Explore`,
/// `serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS`, `bias: 0.3`,
/// `flags: PF_EXPLORE | PF_TARGETS_FOOD`). Fires when the dispatcher's ctx
/// shows no concrete food source — no storage stock, no visible scavenge
/// target — but the agent is still hungry. The expansion is a single
/// `Task::Explore { kind: MemoryKind::Food }`; the legacy `TaskKind::Explore`
/// path drives random-tile selection + walk + vision pickup, and the
/// pre-existing `explore_satisfaction_system` aborts the moment matching
/// memory is recorded (so under HTN the next dispatch tick re-evaluates with
/// the new ctx).
///
/// Utility `0.3` matches the legacy plan's `bias` field exactly. With concrete
/// methods at `1.0` (`WithdrawFromStorageMethod`) and `1.5`
/// (`ScavengeFoodFromGroundMethod`), Explore loses to either when applicable —
/// it only wins when no concrete method's precondition fires, which is
/// behaviourally identical to the legacy plan registry where the Explore
/// plan's flat-bias score was beaten by any concrete plan whose state-vector
/// dot product produced a positive score. The utility-based fallback
/// semantics replace the legacy candidate filter's flag inversion (the
/// `PF_EXPLORE` plans were specifically gated on "no source vis AND no good
/// vis AND no memory" in `plan_execution_system`'s candidate filter).
///
/// Precondition gates on `hunger >= EAT_TRIGGER_HUNGER` to mirror the other
/// AcquireFood methods' hunger gates and the dispatcher's pre-filter (which
/// already short-circuits before walking the registry on under-hungry
/// agents). Defence in depth.
///
/// **GatherFood goal not handled here.** The legacy `ExploreForFood` plan
/// also serves `AgentGoal::GatherFood` — the chief-driven "go look for food
/// to put in storage" path that doesn't gate on hunger. That path needs a
/// sibling `ExploreForFoodForStorageMethod` (or this method's precondition
/// relaxed for the GatherFood case once the dispatcher distinguishes goals)
/// to fully retire the legacy plan; deferred to 5c-ii-d-iv-ii.
///
/// Scaffolding only at 5c-ii-d-iv-i: `register_builtin_methods` wires the
/// method into the registry but no dispatcher consumes the AcquireFood
/// fallback branch yet. The legacy `ExploreForFood` plan remains
/// authoritative; 5c-ii-d-iv-ii will land the dispatcher extension that
/// builds a `PlannerCtx` with empty storage / scavenge fields and routes
/// the head `Task::Explore`, plus the PlanId 35 deletion (or GatherFood-only
/// retention).
pub struct ExploreForFoodMethod;

impl Method for ExploreForFoodMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return false;
        }
        ctx.hunger >= EAT_TRIGGER_HUNGER as f32
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        0.3
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return Vec::new();
        }
        vec![Task::Explore {
            kind: MemoryKind::Food,
        }]
    }

    fn name(&self) -> &'static str {
        "ExploreForFood"
    }
}

/// Phase 5c-ii-d-iv-i fallback method for `AbstractTask::AcquireGood { good }`.
/// Mirrors the legacy `ExploreForWood` / `ExploreForStone` plans (PlanId
/// 36/37, single step `Explore`, `bias: 0.3`,
/// `flags: PF_EXPLORE | PF_TARGETS_{WOOD,STONE}`). Fires when the dispatcher's
/// ctx shows no concrete material source — no storage stock, no visible
/// scavenge target, no known harvest tile, no claimed blueprint. The
/// expansion is a single `Task::Explore { kind: MemoryKind::Wood/Stone }` —
/// the kind is derived from the abstract task's `good` payload, so one method
/// body serves both Wood and Stone (and any future material whose
/// `Good → MemoryKind` mapping is added).
///
/// Utility `0.3` matches the legacy plans' `bias` field exactly. Loses to any
/// concrete AcquireGood method (`WithdrawMaterialFromStorageMethod` at 1.0,
/// `WithdrawAndHaulToBlueprintMethod` at 2.0, `GatherFromKnownMethod` at 1.0,
/// `ScavengeFromGroundMethod` at 1.5). Wins only when no concrete ctx is
/// populated, which is the behaviour the legacy candidate-filter inversion
/// (`PF_EXPLORE` only available with no memory + no vis) enforced.
///
/// Precondition gates on the `good` payload mapping cleanly to a `MemoryKind`
/// — only `Good::Wood` and `Good::Stone` are gather goals today. Other goods
/// (Iron, Fruit, etc.) fail the precondition and the dispatcher falls back to
/// whatever other methods are applicable. The legacy plan registry handled
/// this implicitly: only `ExploreForWood` (gated on `GATHER_WOOD_GOALS`) and
/// `ExploreForStone` (gated on `GATHER_STONE_GOALS`) existed; iron/fruit had
/// no `ExploreForX` plan because the corresponding gather goals don't exist.
///
/// Scaffolding only at 5c-ii-d-iv-i: `register_builtin_methods` wires the
/// method into the registry but no dispatcher consumes the AcquireGood
/// fallback branch yet. The legacy `ExploreForWood` / `ExploreForStone`
/// plans remain authoritative; 5c-ii-d-iv-ii will land the dispatcher
/// extension that recognises the empty-ctx case under `AgentGoal::GatherWood`
/// / `GatherStone` and routes a head `Task::Explore`, plus the PlanId 36/37
/// deletion.
pub struct ExploreForMaterialMethod;

impl ExploreForMaterialMethod {
    /// Map a target good to the `MemoryKind` the agent records when they
    /// spot a source of that good. Only Wood / Stone today — other goods
    /// have no corresponding `MemoryKind` because no gather-goal targets
    /// them. Returns `None` for unsupported goods so the method can opt out
    /// cleanly.
    fn memory_kind_for(good: Good) -> Option<MemoryKind> {
        match good {
            Good::Wood => Some(MemoryKind::Wood),
            Good::Stone => Some(MemoryKind::Stone),
            _ => None,
        }
    }
}

impl Method for ExploreForMaterialMethod {
    fn precondition(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> bool {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return false;
        };
        Self::memory_kind_for(good).is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        0.3
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { good } = abstract_task else {
            return Vec::new();
        };
        let Some(kind) = Self::memory_kind_for(good) else {
            return Vec::new();
        };
        vec![Task::Explore { kind }]
    }

    fn name(&self) -> &'static str {
        "ExploreForMaterial"
    }
}

/// Pick a random reachable explore destination near the agent's faction home.
/// Mirrors the legacy `StepTarget::ExploreTile` resolver in `plan/mod.rs`:
/// roll up to 8 random offsets in `[-96, 96]` from `home`, return the first
/// candidate whose surface tile shares a connectivity component with the
/// agent's current `(chunk, z)` pair. Returns `None` if no candidate is
/// reachable — the dispatcher drops the chain and the next tick re-evaluates
/// (legacy plan registry's underground recovery via
/// `nearest_reachable_higher_tile` is intentionally not replicated here; that
/// fallback is rare enough that re-rolling next tick is cheaper than
/// duplicating the helper).
fn pick_explore_tile(
    home: (i32, i32),
    cur_chunk: ChunkCoord,
    cur_z: i8,
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
) -> Option<(i32, i32)> {
    for _ in 0..8 {
        let dx = fastrand::i32(-96..=96);
        let dy = fastrand::i32(-96..=96);
        let tx = (home.0 + dx).max(0);
        let ty = (home.1 + dy).max(0);
        let to_chunk = ChunkCoord(
            tx.div_euclid(CHUNK_SIZE as i32),
            ty.div_euclid(CHUNK_SIZE as i32),
        );
        let to_z = chunk_map.surface_z_at(tx, ty) as i8;
        if chunk_connectivity.is_reachable((cur_chunk, cur_z), (to_chunk, to_z)) {
            return Some((tx, ty));
        }
    }
    None
}

/// Phase 5a-ii dispatcher. Owns `AgentGoal::Sleep` end-to-end — the legacy
/// match arm in `goal_dispatch_system` is gone. For each non-Drafted,
/// non-PlayerOrder agent whose goal is Sleep this system:
///
/// 1. Short-circuits the in-progress states (already `Sleeping`, just arrived
///    `Working` on the Sleep tile, or still `Seeking`/`Routing` toward one).
/// 2. Snapshots the agent into a `PlannerCtx` (tile, faction, faction home,
///    home-bed claim + the bed's tile if the claim is still live).
/// 3. Picks the highest-utility applicable `Method` from the Sleep registry.
///    Today that is always `SleepMethod`; the loop is in place for 5b+ where
///    multiple methods will compete on utility.
/// 4. Reads the expansion's first `Task::Sleep { bed }` and routes the legacy
///    channel accordingly: route to bed tile (`Some(_)`), route to faction
///    home (within 5-tile disc check), or sleep in place. Any further tasks
///    in the expansion are pushed onto the prefetch ring.
///
/// Behaviour parity with the deleted arm is the migration's only contract —
/// `sleep_goal_dispatches_typed_sleep_task` in `test_fixture` is the
/// regression test.
pub fn htn_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    bed_query: Query<&Transform, With<Bed>>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            Option<&HomeBed>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, goal, member, transform, lod, home_bed_opt)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::Sleep) {
                return;
            }

            // Already asleep — nothing to do until the goal flips off Sleep.
            if ai.state == AiState::Sleeping {
                return;
            }

            // Arrived at the Sleep destination — flip the state. The typed
            // `Task::Sleep` variant carries the bed claim (if any) and stays
            // set across the Working→Sleeping transition; it gets cleared
            // when the goal flips off Sleep via the `aq.cancel()` stale-reset
            // path in `goal_dispatch_system`.
            if ai.state == AiState::Working && ai.task_id == TaskKind::Sleep as u16 {
                ai.state = AiState::Sleeping;
                return;
            }

            // In flight on a Sleep task — wait for arrival.
            let is_active = matches!(
                ai.state,
                AiState::Working | AiState::Seeking | AiState::Routing
            );
            if is_active && ai.task_id == TaskKind::Sleep as u16 {
                return;
            }

            // Build the PlannerCtx. `home_bed_tile` reads the bed's Transform;
            // if the bed entity has been despawned or unloaded the lookup
            // fails and we drop to `None`, which the SleepMethod translates
            // into `Task::Sleep { bed: None }` (faction-home / in-place
            // fallback path).
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            let home_bed = home_bed_opt.and_then(|h| h.0);
            let home_bed_tile = home_bed.and_then(|b| bed_query.get(b).ok()).map(|t| {
                (
                    (t.translation.x / TILE_SIZE).floor() as i32,
                    (t.translation.y / TILE_SIZE).floor() as i32,
                )
            });
            let faction_home = if member.faction_id != SOLO {
                faction_registry.home_tile(member.faction_id)
            } else {
                None
            };

            let ctx = PlannerCtx {
                tile: (cur_tx, cur_ty),
                faction_id: member.faction_id,
                faction_home,
                home_bed,
                home_bed_tile,
                // Sleep dispatch path doesn't read the hunger fields; leave
                // them at zero. The future `htn_eat_dispatch_system` (5b-ii)
                // will populate them from `EconomicAgent` + `Carrier` +
                // `Needs` when it lands.
                edible_count: 0,
                hunger: 0.0,
                // Sleep dispatch path doesn't read the storage fields either.
                // The future `htn_acquire_food_dispatch_system` (5b-iii-ii)
                // will populate them from `StorageTileMap` + `FactionStorage`.
                nearest_storage_tile: None,
                faction_food_stock: 0,
                // 5c-i material-storage fields. Sleep doesn't consume them.
                material_storage_tile: None,
                material_stock_for_target: 0,
                claimed_blueprint: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
            };

            // Argmax over applicable methods. f32 has no total order; ties
            // break on declaration order via `partial_cmp(...).unwrap_or(Equal)`.
            let abstract_task = AbstractTask::Sleep;
            let methods = method_registry.methods_for(AbstractTaskKind::Sleep);
            let chosen = methods
                .iter()
                .filter(|m| m.precondition(abstract_task, &ctx))
                .max_by(|a, b| {
                    let ua = a.utility(abstract_task, &ctx);
                    let ub = b.utility(abstract_task, &ctx);
                    ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                });

            let Some(method) = chosen else {
                return;
            };
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                return;
            }
            let head = tasks.remove(0);

            // Route the legacy channel based on the typed task. Future
            // methods that return non-Sleep heads (e.g. a `WalkTo` chain
            // ahead of a Sleep) will land as new arms here.
            match head {
                Task::Sleep { bed: Some(bed_entity) } => {
                    if let Some(bed_tile) = home_bed_tile {
                        assign_task_with_routing(
                            &mut ai,
                            (cur_tx, cur_ty),
                            cur_chunk,
                            bed_tile,
                            TaskKind::Sleep,
                            Some(bed_entity),
                            &chunk_graph,
                            &chunk_router,
                            &chunk_map,
                            &chunk_connectivity,
                        );
                        aq.dispatch(Task::Sleep {
                            bed: Some(bed_entity),
                        });
                    } else {
                        // Defensive: the method already filters bed by
                        // home_bed_tile.is_some(), so this branch shouldn't
                        // fire. If it ever does (e.g. a future method that
                        // skips the filter), drop to in-place to avoid a
                        // null-route panic.
                        ai.state = AiState::Sleeping;
                        ai.task_id = TaskKind::Sleep as u16;
                        aq.dispatch(Task::Sleep { bed: None });
                    }
                }
                Task::Sleep { bed: None } => {
                    // Faction-home branch: route home if we're outside the
                    // 5-tile disc; once at home, the in-place branch fires.
                    if let Some(home) = faction_home {
                        let dx = cur_tx - home.0;
                        let dy = cur_ty - home.1;
                        if dx * dx + dy * dy > 5 * 5 {
                            assign_task_with_routing(
                                &mut ai,
                                (cur_tx, cur_ty),
                                cur_chunk,
                                home,
                                TaskKind::Sleep,
                                None,
                                &chunk_graph,
                                &chunk_router,
                                &chunk_map,
                                &chunk_connectivity,
                            );
                            aq.dispatch(Task::Sleep { bed: None });
                            return;
                        }
                    }
                    // Solo, no home, or already at home with no bed: sleep
                    // here.
                    ai.state = AiState::Sleeping;
                    ai.task_id = TaskKind::Sleep as u16;
                    aq.dispatch(Task::Sleep { bed: None });
                }
                _ => {
                    // No registered Sleep method returns a non-Sleep head
                    // today. Leave the agent untouched so the next tick
                    // re-runs dispatch.
                }
            }

            // Push any remaining tasks onto the prefetch ring. Today the
            // Sleep method returns a single-element vec, so this is a no-op,
            // but the path is here so multi-step Sleep expansions (e.g.
            // future "drink water → sleep" chains) flow without code change.
            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// Phase 5b-ii dispatcher. Owns `AgentGoal::Survive` end-to-end *only* for the
/// in-place Eat case — a hungry agent already carrying food. For each
/// non-Drafted, non-PlayerOrder Survive agent without an `ActivePlan` and idle
/// task slot this system:
///
/// 1. Snapshots the agent into a `PlannerCtx` (tile + faction stub for parity
///    with Sleep, plus the new `edible_count` (inventory + hands) and `hunger`).
/// 2. Argmaxes utility over `methods_for(AbstractTaskKind::Eat)` filtered by
///    `precondition`. Today only `EatFromInventoryMethod` is registered; the
///    loop shape lets future Eat methods (e.g. `EatFromCarriedFoodPreferringFresh`)
///    compete on utility.
/// 3. Reads the expansion's first `Task::Eat` and primes the legacy channel:
///    `state = Working`, `task_id = Eat`, `work_progress = 0`. The existing
///    `eat_task_system` (driven by `task_id == TaskKind::Eat`) consumes it.
///
/// Why a separate system from `htn_dispatch_system`: the Eat path needs three
/// extra components (`EconomicAgent`, `Carrier`, `Needs`) and reads
/// `Option<&ActivePlan>` so it can decline to preempt agents already running a
/// food-acquisition plan (Forage/Scavenge/WithdrawAndEat). Splitting keeps the
/// Sleep query small. Both systems serialise on `&mut PersonAI` / `&mut
/// ActionQueue` anyway, so the split costs no parallelism.
///
/// The legacy `EatFromInventory` plan (PlanId 25) was removed from the
/// registry in this same PR — the only path that produces a `TaskKind::Eat`
/// dispatch under `AgentGoal::Survive` for a food-bearing agent is now this
/// system. The Eat-as-final-step path inside Forage/Scavenge/WithdrawAndEat
/// plans still flows through `plan_execution_system` because those plans
/// haven't been migrated yet.
pub fn htn_eat_dispatch_system(
    method_registry: Res<MethodRegistry>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Needs,
            &EconomicAgent,
            &Carrier,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&ActivePlan>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, goal, needs, agent, carrier, transform, member, lod, active_plan_opt)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::Survive) {
                return;
            }

            // Don't preempt an in-flight plan. Survive plans like Forage,
            // ScavengeFood, WithdrawAndEat all end with an Eat step; let those
            // run to completion and dispatch their own Eat through
            // `plan_execution_system`. We only fire when the agent has no
            // plan and an idle task slot — the same gate
            // `plan_execution_system` uses to start a fresh plan.
            if active_plan_opt.is_some() {
                return;
            }
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

            let edible_count = total_edible(agent, carrier);
            // Quick reject before iterating methods — same gate
            // EatFromInventoryMethod uses, but cheaper than building the ctx
            // and walking the registry just to short-circuit.
            if edible_count == 0 || needs.hunger < EAT_TRIGGER_HUNGER as f32 {
                return;
            }

            let ctx = PlannerCtx {
                tile: (cur_tx, cur_ty),
                faction_id: member.faction_id,
                faction_home: None,
                home_bed: None,
                home_bed_tile: None,
                edible_count,
                hunger: needs.hunger,
                // Eat-in-place dispatch doesn't consider the faction storage
                // tile — the agent already has food in hand. The future
                // `htn_acquire_food_dispatch_system` (5b-iii-ii) will populate
                // these fields when it routes a hungry, empty-handed agent
                // toward storage.
                nearest_storage_tile: None,
                faction_food_stock: 0,
                // 5c-i material-storage fields. Eat doesn't consume them.
                material_storage_tile: None,
                material_stock_for_target: 0,
                claimed_blueprint: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
            };

            let abstract_task = AbstractTask::Eat;
            let methods = method_registry.methods_for(AbstractTaskKind::Eat);
            let chosen = methods
                .iter()
                .filter(|m| m.precondition(abstract_task, &ctx))
                .max_by(|a, b| {
                    let ua = a.utility(abstract_task, &ctx);
                    let ub = b.utility(abstract_task, &ctx);
                    ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                });

            let Some(method) = chosen else {
                return;
            };
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                return;
            }
            let head = tasks.remove(0);

            match head {
                Task::Eat => {
                    // Prime the legacy channel: eat_task_system needs Working
                    // state to start accumulating work_progress, and task_id
                    // discriminates the executor branch. The typed dispatch
                    // mirrors the legacy state.
                    ai.state = AiState::Working;
                    ai.task_id = TaskKind::Eat as u16;
                    ai.work_progress = 0;
                    aq.dispatch(Task::Eat);
                }
                _ => {
                    // No registered Eat method returns a non-Eat head today.
                    // Defensive: leave the agent untouched so the next tick
                    // re-runs dispatch.
                }
            }

            // Push any remaining tasks onto the prefetch ring. Today the Eat
            // method returns a single-element vec, so this is a no-op.
            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// Phase 5b-iii-ii dispatcher. Owns the "agent has no food on hand, faction
/// storage has food, agent is hungry" branch of `AgentGoal::Survive`. For each
/// non-Drafted, non-PlayerOrder Survive agent without an `ActivePlan`, idle
/// task slot, and an empty larder this system:
///
/// 1. Snapshots the agent into a `PlannerCtx` (`hunger`, `nearest_storage_tile`
///    from `StorageTileMap::nearest_for_faction`, `faction_food_stock` from
///    `FactionRegistry::food_stock` rounded down).
/// 2. Argmaxes utility over `methods_for(AbstractTaskKind::AcquireFood)`
///    filtered by `precondition`. Today only `WithdrawFromStorageMethod` is
///    registered.
/// 3. Reads the expansion's first `Task::WithdrawFood { tile }`, routes the
///    agent to the storage tile via `assign_task_with_routing`, and `aq.dispatch`s
///    the typed task.
/// 4. Pushes any remaining tasks (today: a single trailing `Task::Eat`) onto
///    the prefetch ring via `aq.enqueue`. The chained `Eat` is what makes this
///    the first method in the registry that actually exercises the ring at
///    runtime.
///
/// The withdraw → eat handoff lives in `withdraw_food_task_system`: when the
/// withdraw finishes it calls `aq.advance()` (promoting the queued `Task::Eat`
/// into `current`) and primes the legacy channel (`task_id = TaskKind::Eat`,
/// `state = Working`, `work_progress = 0`) so `eat_task_system` picks up
/// immediately on the next tick without re-entering dispatch.
///
/// Why a separate system from `htn_eat_dispatch_system`: AcquireFood needs the
/// `StorageTileMap` + `FactionRegistry` + the four pathfinder resources for
/// routing, while the in-place Eat dispatcher only reads `Needs` + `Carrier` +
/// `EconomicAgent`. Both serialise on `&mut PersonAI` / `&mut ActionQueue`, so
/// the split costs no parallelism. The pre-filter `total_edible(...) > 0` —
/// "agent already has food, defer to the in-place Eat path" — is enforced here
/// so the AcquireFood method's precondition can stay symmetric with the
/// EatFromInventory method's gate without a hand-tuned tie-breaker.
///
/// This is the third HTN dispatcher (after Sleep and Eat); each follows the
/// same shape: `goal_dispatch_system` → ParallelB chain → per-goal dispatcher
/// builds its own `PlannerCtx` and matches on the typed-task variant the
/// expansion's head produces.
pub fn htn_acquire_food_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Needs,
            &EconomicAgent,
            &Carrier,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&ActivePlan>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    // The food-scavenge scan reuses the AcquireGood scavenge branch's radius
    // so the two HTN scavenge paths search out to the same vis range.
    const VIEW_RADIUS: i32 = 15;
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, goal, needs, agent, carrier, transform, member, lod, active_plan_opt)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::Survive) {
                return;
            }

            // Same gating as `htn_eat_dispatch_system`: don't preempt an
            // in-flight plan, only fire on a clean (Idle, UNEMPLOYED) slot.
            if active_plan_opt.is_some() {
                return;
            }
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                return;
            }

            // Solo agents have no faction storage to draw from.
            if member.faction_id == SOLO {
                return;
            }

            // If the agent already has food on hand, the in-place Eat path
            // (htn_eat_dispatch_system) is the right answer — leaving us a
            // free precondition split between "eat what you have" and "go get
            // more." This gate also prevents a hungry agent from walking past
            // food in their own pocket to reach storage.
            if total_edible(agent, carrier) > 0 {
                return;
            }

            // Cheap pre-filter on hunger before we touch the StorageTileMap or
            // walk the registry.
            if needs.hunger < EAT_TRIGGER_HUNGER as f32 {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            let nearest_storage_tile =
                storage_tile_map.nearest_for_faction(member.faction_id, (cur_tx, cur_ty));
            // `food_stock` returns f32 because it sums Fruit/Meat/Grain at
            // floating-point granularity in some legacy code; for ctx purposes
            // we want a u32 tally. Floor the value — under-counting is the
            // safer side for the precondition gate.
            let faction_food_stock = faction_registry.food_stock(member.faction_id) as u32;

            // Phase 5c-ii-d-iii-ii: scan SpatialIndex for visible loose edible
            // GroundItems within VIEW_RADIUS, excluding faction storage tiles
            // (mirrors the legacy `StepTarget::NearestEdible` resolver, which
            // also excludes storage so the agent doesn't try to "scavenge"
            // their own deposit). Same scan pattern as the AcquireGood
            // scavenge branch in `htn_acquire_good_dispatch_system`, but
            // filters on `is_edible()` instead of a specific good.
            let mut scavenge_target_entity: Option<Entity> = None;
            let mut scavenge_target_tile: Option<(i32, i32)> = None;
            {
                let mut best_dist_sq = i32::MAX;
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                        let d2 = dx * dx + dy * dy;
                        if d2 > VIEW_RADIUS * VIEW_RADIUS {
                            continue;
                        }
                        let tx = cur_tx + dx;
                        let ty = cur_ty + dy;
                        if storage_tile_map.tiles.contains_key(&(tx, ty)) {
                            continue;
                        }
                        for &gi_entity in spatial.get(tx, ty) {
                            if let Ok(gi) = item_query.get(gi_entity) {
                                if gi.item.good().is_edible() && gi.qty > 0 && d2 < best_dist_sq {
                                    best_dist_sq = d2;
                                    scavenge_target_entity = Some(gi_entity);
                                    scavenge_target_tile = Some((tx, ty));
                                }
                            }
                        }
                    }
                }
            }

            let ctx = PlannerCtx {
                tile: (cur_tx, cur_ty),
                faction_id: member.faction_id,
                faction_home: faction_registry.home_tile(member.faction_id),
                home_bed: None,
                home_bed_tile: None,
                edible_count: 0,
                hunger: needs.hunger,
                nearest_storage_tile,
                faction_food_stock,
                // 5c-i material-storage fields. AcquireFood doesn't consume them.
                material_storage_tile: None,
                material_stock_for_target: 0,
                claimed_blueprint: None,
                gather_target_tile: None,
                scavenge_target_entity,
                scavenge_target_tile,
            };

            let abstract_task = AbstractTask::AcquireFood;
            let methods = method_registry.methods_for(AbstractTaskKind::AcquireFood);
            let chosen = methods
                .iter()
                .filter(|m| m.precondition(abstract_task, &ctx))
                .max_by(|a, b| {
                    let ua = a.utility(abstract_task, &ctx);
                    let ub = b.utility(abstract_task, &ctx);
                    ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                });

            let Some(method) = chosen else {
                return;
            };
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                return;
            }
            let head = tasks.remove(0);

            match head {
                Task::WithdrawFood { tile } => {
                    let dispatched = assign_task_with_routing(
                        &mut ai,
                        (cur_tx, cur_ty),
                        cur_chunk,
                        tile,
                        TaskKind::WithdrawFood,
                        None,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if !dispatched {
                        // Routing rejected the storage tile (no reachable
                        // adjacent standable). Drop the chain and let the next
                        // tick re-evaluate; method preconditions will likely
                        // re-fire and pick a different tile if storage layout
                        // changes.
                        return;
                    }
                    aq.dispatch(Task::WithdrawFood { tile });
                }
                Task::Scavenge { target } => {
                    // Phase 5c-ii-d-iii-ii: scavenge dispatch under
                    // AcquireFood. Mirrors the AcquireGood scavenge branch
                    // in `htn_acquire_good_dispatch_system` — route to the
                    // entity's tile via `assign_task_with_routing`, then
                    // `dispatch` the typed task. The entity-target lives on
                    // the typed variant; `item_pickup_system` reads it via
                    // `aq.current.as_scavenge()`.
                    //
                    // Pass `target_entity = Some(target)` so the legacy
                    // `ai.target_entity` field tracks the GroundItem.
                    // `goal_update_system`'s Scavenge target validation
                    // (`goals.rs:286-293`) flags the task invalid and resets
                    // state when this is `None` — under Survive (no JobClaim
                    // bypass) the next tick's dispatcher would re-fire and
                    // pile a duplicate chain onto the prefetch ring. The
                    // AcquireGood scavenge branch (5c-ii-d-ii-a) gets away
                    // with `None` because its goal is GatherWood/Stone +
                    // JobClaim::Stockpile, which `goal_update_system` skips
                    // entirely (line 237).
                    let Some(scav_tile) = scavenge_target_tile else {
                        return;
                    };
                    let dispatched = assign_task_with_routing(
                        &mut ai,
                        (cur_tx, cur_ty),
                        cur_chunk,
                        scav_tile,
                        TaskKind::Scavenge,
                        Some(target),
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if !dispatched {
                        return;
                    }
                    aq.dispatch(Task::Scavenge { target });
                }
                Task::Explore { kind } => {
                    // Phase 5c-ii-d-iv-ii: explore dispatch under AcquireFood.
                    // Replaces the legacy `ExploreForFood` plan path under
                    // `AgentGoal::Survive`. Pick a random reachable tile near
                    // the faction home (or the agent's current position if
                    // unsettled), route via `assign_task_with_routing(...
                    // TaskKind::Explore, None, ...)`, dispatch. The legacy
                    // `TaskKind::Explore` executor handles the walk + vision
                    // pickup; when matching memory is recorded en route,
                    // `vision_system` populates `AgentMemory` and the next
                    // dispatch tick will see a populated ctx and pick a
                    // concrete method instead.
                    let home = faction_registry
                        .home_tile(member.faction_id)
                        .unwrap_or((cur_tx, cur_ty));
                    let Some(dest) = pick_explore_tile(
                        home,
                        cur_chunk,
                        ai.current_z,
                        &chunk_map,
                        &chunk_connectivity,
                    ) else {
                        // No reachable random tile in 8 rolls. Drop the
                        // chain; next tick re-rolls. Same fallback shape as
                        // the routing-failure path above — agent stays
                        // (Idle, UNEMPLOYED) and re-evaluates.
                        return;
                    };
                    let dispatched = assign_task_with_routing(
                        &mut ai,
                        (cur_tx, cur_ty),
                        cur_chunk,
                        dest,
                        TaskKind::Explore,
                        None,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if !dispatched {
                        return;
                    }
                    aq.dispatch(Task::Explore { kind });
                }
                _ => {
                    // No registered AcquireFood method returns a non-WithdrawFood,
                    // non-Scavenge, non-Explore head today. Defensive
                    // fallthrough; future Forage / Hunt methods will land as
                    // new arms here.
                    return;
                }
            }

            // Push the trailing tasks onto the prefetch ring. Both AcquireFood
            // chain shapes terminate in `Task::Eat`:
            // - WithdrawFromStorage → [WithdrawFood, Eat]: handoff in
            //   `withdraw_food_task_system::finish_withdraw_food`.
            // - ScavengeFoodFromGround → [Scavenge, Eat]: handoff in
            //   `item_pickup_system::finish_scavenge` (5c-ii-d-iii-ii: the
            //   helper learned to prime the legacy Eat channel here).
            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// Phase 5c-ii-b/-c dispatcher for `AbstractTask::AcquireGood { good }` under
/// `AgentGoal::Haul` (5c-ii-b — replaces the legacy `ClaimedHaul` plan
/// PlanId 33) *and* `AgentGoal::GatherWood` / `AgentGoal::GatherStone`
/// (5c-ii-c-ii — replaces the legacy `GatherWood` / `GatherStone` plans
/// PlanId 2/3).
///
/// **Haul branch.** For each non-Drafted, non-PlayerOrder Haul-goal agent
/// without an `ActivePlan`, an idle task slot, and a live `JobClaim::Haul` /
/// `ClaimTarget` pair this system:
///
/// 1. Reads the `ClaimTarget`'s `good` and `blueprint`. Both are required —
///    Haul claims always carry both per `posting_claim_target`. Skips when
///    either is missing (defensive against partially-populated targets).
/// 2. Walks the faction's storage tiles to find the nearest one holding the
///    target good (effective stock after reservations > 0).
/// 3. Builds a `PlannerCtx { material_storage_tile, material_stock_for_target,
///    claimed_blueprint, .. }` and argmaxes the `AcquireGood` methods.
///    `WithdrawAndHaulToBlueprintMethod` (utility 2.0, gated on
///    `claimed_blueprint.is_some()`) wins over the bare
///    `WithdrawMaterialFromStorageMethod` (utility 1.0) for haulers.
/// 4. Reads the expansion's two-task chain `[WithdrawMaterial, HaulToBlueprint]`,
///    routes the head via `assign_task_with_routing(... TaskKind::WithdrawMaterial,
///    None, ...)` to the storage tile, adds a `StorageReservations` entry, and
///    dispatches the typed task. Pushes the trailing `HaulToBlueprint` onto the
///    prefetch ring. The handoff lives in `finish_withdraw_material`.
///
/// **Gather branch (5c-ii-c-ii).** For each non-Drafted, non-PlayerOrder
/// `GatherWood`/`GatherStone`-goal agent without an `ActivePlan`, an idle
/// task slot, and a populated `AgentMemory::best_for(MemoryKind::Wood|Stone)`:
///
/// 1. Maps the goal to a `(Good, MemoryKind)` pair.
/// 2. Reads `AgentMemory::best_for(memory_kind)` for the gather target tile.
///    Skips when memory is empty — the legacy plan path's `Explore` plans
///    handle the no-knowledge case via `goal_update_system`'s plan churn.
/// 3. Builds a `PlannerCtx { gather_target_tile: Some(tile), .. }` (leaving
///    `material_storage_tile` and `claimed_blueprint` at `None` so the
///    bare-withdraw and haul methods' preconditions fail — the gather method
///    is the only applicable one in this branch today).
/// 4. Reads the expansion's two-task chain `[Gather, DepositToFactionStorage]`,
///    routes the head via `assign_task_with_routing(... TaskKind::Gather,
///    None, ...)` to the gather tile, dispatches the typed task. Pushes the
///    trailing `DepositToFactionStorage` onto the prefetch ring. The handoff
///    lives in `finish_gather` in `gather.rs`: it advances the ring, looks up
///    the nearest faction storage tile via `StorageTileMap::nearest_for_faction`,
///    and routes the agent with `TaskKind::DepositResource`. From there
///    `drop_items_at_destination_system` is the executor — it dumps everything
///    in hands at `dest_tile` and credits any `JobClaim::Stockpile` with
///    `record_progress_filtered`.
pub fn htn_acquire_good_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            Option<&ActivePlan>,
            Option<&crate::simulation::jobs::ClaimTarget>,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::memory::AgentMemory>,
        ),
        (Without<PlayerOrder>, Without<Drafted>),
    >,
) {
    use crate::simulation::jobs::JobKind;

    for (
        mut ai,
        mut aq,
        goal,
        member,
        transform,
        lod,
        active_plan_opt,
        claim_target_opt,
        job_claim_opt,
        memory_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if active_plan_opt.is_some() {
            continue;
        }
        if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        // Branch on goal: each branch builds its own ctx + routes its own
        // expansion head. The argmax happens in both branches but the ctx
        // shape is what makes a different method win — the bare-withdraw,
        // haul, and gather methods all sit under `AcquireGood` and gate on
        // disjoint ctx fields (`material_storage_tile`, `claimed_blueprint`,
        // `gather_target_tile` respectively).
        match *goal {
            AgentGoal::Haul => {
                // existing haul logic below
            }
            AgentGoal::GatherWood | AgentGoal::GatherStone => {
                const VIEW_RADIUS: i32 = 15;
                let (good, memory_kind) = match *goal {
                    AgentGoal::GatherWood => (Good::Wood, MemoryKind::Wood),
                    AgentGoal::GatherStone => (Good::Stone, MemoryKind::Stone),
                    _ => unreachable!(),
                };

                let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                let cur_chunk = ChunkCoord(
                    cur_tx.div_euclid(CHUNK_SIZE as i32),
                    cur_ty.div_euclid(CHUNK_SIZE as i32),
                );

                // Memory-based gather target (Phase 5c-ii-c-ii). Empty memory
                // doesn't kill the dispatch — a visible scavenge target may
                // still drive a chain.
                let gather_target_tile =
                    memory_opt.and_then(|m| m.best_for(memory_kind));

                // Vision-based scavenge target (Phase 5c-ii-d-ii-a). Scan
                // SpatialIndex for the nearest matching `GroundItem` within
                // VIEW_RADIUS, excluding faction storage tiles (matches the
                // legacy `StepTarget::NearestItem` resolver in `plan/mod.rs`,
                // which also excludes storage so an agent doesn't try to
                // "scavenge" their own deposit).
                let mut scavenge_target_entity: Option<Entity> = None;
                let mut scavenge_target_tile: Option<(i32, i32)> = None;
                let mut best_dist_sq = i32::MAX;
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                        let d2 = dx * dx + dy * dy;
                        if d2 > VIEW_RADIUS * VIEW_RADIUS {
                            continue;
                        }
                        let tx = cur_tx + dx;
                        let ty = cur_ty + dy;
                        if storage_tile_map.tiles.contains_key(&(tx, ty)) {
                            continue;
                        }
                        for &gi_entity in spatial.get(tx, ty) {
                            if let Ok(gi) = item_query.get(gi_entity) {
                                if gi.item.good() == good && gi.qty > 0 && d2 < best_dist_sq {
                                    best_dist_sq = d2;
                                    scavenge_target_entity = Some(gi_entity);
                                    scavenge_target_tile = Some((tx, ty));
                                }
                            }
                        }
                    }
                }

                // Phase 5c-ii-d-iv-ii: no early-return when both targets are
                // None. The argmax now picks `ExploreForMaterialMethod`
                // (utility 0.3) as the fallback when no concrete method's
                // precondition fires — replaces the legacy
                // `ExploreForWood`/`ExploreForStone` plan path that this PR
                // deletes from the registry.

                let ctx = PlannerCtx {
                    tile: (cur_tx, cur_ty),
                    faction_id: member.faction_id,
                    faction_home: faction_registry.home_tile(member.faction_id),
                    home_bed: None,
                    home_bed_tile: None,
                    edible_count: 0,
                    hunger: 0.0,
                    nearest_storage_tile: None,
                    faction_food_stock: 0,
                    material_storage_tile: None,
                    material_stock_for_target: 0,
                    claimed_blueprint: None,
                    gather_target_tile,
                    scavenge_target_entity,
                    scavenge_target_tile,
                };

                let abstract_task = AbstractTask::AcquireGood { good };
                let methods = method_registry.methods_for(AbstractTaskKind::AcquireGood);
                let chosen = methods
                    .iter()
                    .filter(|m| m.precondition(abstract_task, &ctx))
                    .max_by(|a, b| {
                        let ua = a.utility(abstract_task, &ctx);
                        let ub = b.utility(abstract_task, &ctx);
                        ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                    });
                let Some(method) = chosen else { continue };
                let mut tasks = method.expand(abstract_task, &ctx);
                if tasks.is_empty() {
                    continue;
                }
                let head = tasks.remove(0);

                match head {
                    Task::Gather { tile: gather_tile } => {
                        let dispatched = assign_task_with_routing(
                            &mut ai,
                            (cur_tx, cur_ty),
                            cur_chunk,
                            gather_tile,
                            TaskKind::Gather,
                            None,
                            &chunk_graph,
                            &chunk_router,
                            &chunk_map,
                            &chunk_connectivity,
                        );
                        if !dispatched {
                            // No reachable adjacency to the gather tile — abandon
                            // the chain. The legacy plan registry's `Explore`
                            // plans handle the no-knowledge / unreachable case
                            // via `goal_update_system`'s plan churn.
                            continue;
                        }
                        aq.dispatch(Task::Gather { tile: gather_tile });
                    }
                    Task::Scavenge { target } => {
                        // Phase 5c-ii-d-ii-a: scavenge dispatch. Routing is
                        // tile-based; the entity-target lives on the typed
                        // task and `item_pickup_system` reads it via
                        // `aq.current.as_scavenge()`.
                        let Some(scav_tile) = scavenge_target_tile else {
                            // Defensive: precondition required tile, but
                            // method was selected and head is Scavenge.
                            continue;
                        };
                        let dispatched = assign_task_with_routing(
                            &mut ai,
                            (cur_tx, cur_ty),
                            cur_chunk,
                            scav_tile,
                            TaskKind::Scavenge,
                            None,
                            &chunk_graph,
                            &chunk_router,
                            &chunk_map,
                            &chunk_connectivity,
                        );
                        if !dispatched {
                            continue;
                        }
                        aq.dispatch(Task::Scavenge { target });
                    }
                    Task::Explore { kind } => {
                        // Phase 5c-ii-d-iv-ii: explore dispatch under
                        // AcquireGood (gather branch). Replaces the legacy
                        // `ExploreForWood`/`ExploreForStone` plan path. Same
                        // shape as the AcquireFood Explore arm — pick a
                        // random reachable tile near the faction home,
                        // route, dispatch. The next dispatch tick will see
                        // a populated `gather_target_tile` once
                        // `vision_system` records a tree/stone sighting and
                        // `GatherFromKnownMethod` (utility 1.0) will outrank
                        // this fallback.
                        let home = faction_registry
                            .home_tile(member.faction_id)
                            .unwrap_or((cur_tx, cur_ty));
                        let Some(dest) = pick_explore_tile(
                            home,
                            cur_chunk,
                            ai.current_z,
                            &chunk_map,
                            &chunk_connectivity,
                        ) else {
                            continue;
                        };
                        let dispatched = assign_task_with_routing(
                            &mut ai,
                            (cur_tx, cur_ty),
                            cur_chunk,
                            dest,
                            TaskKind::Explore,
                            None,
                            &chunk_graph,
                            &chunk_router,
                            &chunk_map,
                            &chunk_connectivity,
                        );
                        if !dispatched {
                            continue;
                        }
                        aq.dispatch(Task::Explore { kind });
                    }
                    _ => {
                        // No registered AcquireGood method returns a
                        // non-Gather, non-Scavenge, non-Explore head under
                        // the gather branch today. Defensive fallthrough.
                        continue;
                    }
                }

                // Push the trailing `Task::DepositToFactionStorage { good }`
                // (and any future tail) onto the prefetch ring. After
                // `gather_system` (or `item_pickup_system` for the scavenge
                // chain) finishes the head, its exit handoff promotes the
                // next task into `current` and primes the legacy channel for
                // `drop_items_at_destination_system`.
                for task in tasks {
                    let _ = aq.enqueue(task);
                }
                continue;
            }
            _ => continue,
        }

        // ── Haul branch ────────────────────────────────────────────────────
        // Need both a Haul claim and its companion ClaimTarget — the target
        // carries the (good, blueprint) pair the chain decomposes around.
        let Some(claim) = job_claim_opt else { continue };
        if claim.kind != JobKind::Haul {
            continue;
        }
        let Some(target) = claim_target_opt else {
            continue;
        };
        let (Some(good), Some(blueprint)) = (target.good, target.blueprint) else {
            continue;
        };

        // Faction-level stock check — mirrors `WithdrawAndHaulToBlueprintMethod`'s
        // precondition gate. Skipping early when the faction has no stock at
        // all avoids touching `SpatialIndex` for every tile on a dry larder.
        let stock = faction_registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.storage.stock_of(good))
            .unwrap_or(0);
        if stock == 0 {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Walk the faction's storage tiles to find the nearest one with the
        // target good in stock (effective stock after reservations > 0).
        // `StorageTileMap::nearest_for_faction` ignores good-specificity, so
        // we need the explicit per-tile scan here.
        let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) else {
            continue;
        };
        let mut best_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        let mut best_tile_stock: u32 = 0;
        for &(tx, ty) in tiles {
            let mut tile_stock: u32 = 0;
            for &gi_entity in spatial.get(tx as i32, ty as i32) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.item.good() == good && gi.qty > 0 {
                        tile_stock = tile_stock.saturating_add(gi.qty);
                    }
                }
            }
            let reserved = storage_reservations.get((tx, ty), good);
            let effective = tile_stock.saturating_sub(reserved);
            if effective == 0 {
                continue;
            }
            let dist = (tx as i32 - cur_tx).abs() + (ty as i32 - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best_tile = Some((tx, ty));
                best_tile_stock = effective;
            }
        }
        let Some(storage_tile) = best_tile else {
            continue;
        };

        let ctx = PlannerCtx {
            tile: (cur_tx, cur_ty),
            faction_id: member.faction_id,
            faction_home: faction_registry.home_tile(member.faction_id),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: Some(storage_tile),
            material_stock_for_target: best_tile_stock,
            claimed_blueprint: Some(blueprint),
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        };

        let abstract_task = AbstractTask::AcquireGood { good };
        let methods = method_registry.methods_for(AbstractTaskKind::AcquireGood);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = a.utility(abstract_task, &ctx);
                let ub = b.utility(abstract_task, &ctx);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else { continue };
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            continue;
        }
        let head = tasks.remove(0);

        match head {
            Task::WithdrawMaterial { good: head_good, qty } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    storage_tile,
                    TaskKind::WithdrawMaterial,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    // No reachable adjacency to the storage tile — abandon
                    // the chain and let next tick re-evaluate (storage layout
                    // may change, or another tile may become reachable).
                    continue;
                }
                // Reserve the qty against the chosen tile so a parallel
                // dispatch in the same tick sees a smaller effective stock.
                // Mirrors `plan_execution_system`'s WithdrawMaterial dispatch
                // site (`plan/mod.rs:2724`).
                let reserved_tile = (storage_tile.0, storage_tile.1);
                storage_reservations.add(reserved_tile, head_good, qty as u32);
                ai.reserved_tile = reserved_tile;
                ai.reserved_good = Some(head_good);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial { good: head_good, qty });
            }
            _ => {
                // No registered AcquireGood method returns a non-WithdrawMaterial
                // head today. Defensive fallthrough.
                continue;
            }
        }

        // Push the trailing `Task::HaulToBlueprint { blueprint }` (and any
        // future tail) onto the prefetch ring. After
        // `withdraw_material_task_system` finishes the head, its
        // `finish_withdraw_material` exit promotes the next task into
        // `current` and primes the legacy channel for the haul leg.
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_solo_in_place() -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 0,
            faction_home: None,
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    fn ctx_with_bed(bed: Entity, bed_tile: (i32, i32)) -> PlannerCtx {
        PlannerCtx {
            tile: (10, 10),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: Some(bed),
            home_bed_tile: Some(bed_tile),
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    fn ctx_with_food(edible_count: u32, hunger: f32) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 0,
            faction_home: None,
            home_bed: None,
            home_bed_tile: None,
            edible_count,
            hunger,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    fn ctx_with_storage(
        storage_tile: Option<(i32, i32)>,
        food_stock: u32,
        hunger: f32,
    ) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger,
            nearest_storage_tile: storage_tile,
            faction_food_stock: food_stock,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    fn ctx_with_material_storage(
        storage_tile: Option<(i32, i32)>,
        material_stock: u32,
    ) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: storage_tile,
            material_stock_for_target: material_stock,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    fn ctx_with_haul_claim(
        storage_tile: Option<(i32, i32)>,
        material_stock: u32,
        blueprint: Option<Entity>,
    ) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: storage_tile,
            material_stock_for_target: material_stock,
            claimed_blueprint: blueprint,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    #[test]
    fn registry_reports_one_sleep_method() {
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::Sleep), 1);
    }

    #[test]
    fn sleep_method_in_place_expands_to_unbedded_sleep() {
        let m = SleepMethod;
        let ctx = ctx_solo_in_place();
        let tasks = m.expand(AbstractTask::Sleep, &ctx);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0], Task::Sleep { bed: None });
    }

    #[test]
    fn sleep_method_with_live_bed_carries_entity() {
        let bed = Entity::from_raw(42);
        let m = SleepMethod;
        let ctx = ctx_with_bed(bed, (3, 3));
        let tasks = m.expand(AbstractTask::Sleep, &ctx);
        assert_eq!(tasks, vec![Task::Sleep { bed: Some(bed) }]);
    }

    #[test]
    fn sleep_method_with_stale_bed_claim_falls_back_to_unbedded() {
        // home_bed: Some(_) but home_bed_tile: None means the bed claim is
        // pointing at an entity whose Transform we couldn't read (despawned
        // or unloaded). Method must drop to bed: None.
        let bed = Entity::from_raw(7);
        let mut ctx = ctx_with_bed(bed, (0, 0));
        ctx.home_bed_tile = None;
        let m = SleepMethod;
        let tasks = m.expand(AbstractTask::Sleep, &ctx);
        assert_eq!(tasks, vec![Task::Sleep { bed: None }]);
    }

    #[test]
    fn sleep_method_precondition_always_true() {
        let m = SleepMethod;
        assert!(m.precondition(AbstractTask::Sleep, &ctx_solo_in_place()));
    }

    #[test]
    fn registry_returns_empty_slice_for_unregistered_kind() {
        // Defensive: an empty registry must not panic on miss.
        let reg = MethodRegistry::default();
        assert!(reg.methods_for(AbstractTaskKind::Sleep).is_empty());
    }

    #[test]
    fn registry_reports_one_eat_method() {
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::Eat), 1);
    }

    #[test]
    fn eat_method_precondition_true_when_food_and_hungry() {
        let m = EatFromInventoryMethod;
        let ctx = ctx_with_food(1, EAT_TRIGGER_HUNGER as f32);
        assert!(m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn eat_method_precondition_false_when_not_hungry_enough() {
        let m = EatFromInventoryMethod;
        let ctx = ctx_with_food(5, (EAT_TRIGGER_HUNGER - 1) as f32);
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn eat_method_precondition_false_when_no_food() {
        let m = EatFromInventoryMethod;
        let ctx = ctx_with_food(0, 250.0);
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn eat_method_expands_to_single_eat_task() {
        let m = EatFromInventoryMethod;
        let ctx = ctx_with_food(3, 220.0);
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert_eq!(tasks, vec![Task::Eat]);
    }

    #[test]
    fn registry_reports_three_acquire_food_methods() {
        // 5c-ii-d-iv-i: `ExploreForFoodMethod` registered alongside
        // `WithdrawFromStorageMethod` (1.0) and `ScavengeFoodFromGroundMethod`
        // (1.5) as the fallback method (utility 0.3). Renamed from
        // `registry_reports_two_acquire_food_methods`.
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::AcquireFood), 3);
    }

    #[test]
    fn withdraw_from_storage_precondition_true_when_stock_storage_and_hunger() {
        let m = WithdrawFromStorageMethod;
        let ctx = ctx_with_storage(Some((4, 7)), 3, EAT_TRIGGER_HUNGER as f32);
        assert!(m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn withdraw_from_storage_precondition_false_without_storage_tile() {
        let m = WithdrawFromStorageMethod;
        // Stock > 0 but no known tile to walk to (e.g. the faction has stocks
        // recorded but every storage tile is unloaded / unreachable).
        let ctx = ctx_with_storage(None, 5, 220.0);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn withdraw_from_storage_precondition_false_without_stock() {
        let m = WithdrawFromStorageMethod;
        // Tile is known but the stock counter is zero — nothing to withdraw.
        let ctx = ctx_with_storage(Some((1, 1)), 0, 220.0);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn withdraw_from_storage_precondition_false_when_not_hungry() {
        let m = WithdrawFromStorageMethod;
        let ctx = ctx_with_storage(Some((1, 1)), 5, (EAT_TRIGGER_HUNGER - 1) as f32);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn withdraw_from_storage_expands_to_withdraw_then_eat() {
        let m = WithdrawFromStorageMethod;
        let ctx = ctx_with_storage(Some((4, 7)), 3, 220.0);
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        assert_eq!(
            tasks,
            vec![Task::WithdrawFood { tile: (4, 7) }, Task::Eat]
        );
    }

    #[test]
    fn withdraw_from_storage_expand_returns_empty_without_tile() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = WithdrawFromStorageMethod;
        let ctx = ctx_with_storage(None, 5, 220.0);
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        assert!(tasks.is_empty());
    }

    #[test]
    fn abstract_task_kind_round_trips() {
        // Sanity: every variant maps to its discriminant key. If a new
        // AbstractTask variant is added without updating `kind()`, the
        // registry lookup silently returns an empty slice — this test
        // surfaces the omission at compile-test time.
        assert_eq!(AbstractTask::Sleep.kind(), AbstractTaskKind::Sleep);
        assert_eq!(AbstractTask::Eat.kind(), AbstractTaskKind::Eat);
        assert_eq!(
            AbstractTask::AcquireFood.kind(),
            AbstractTaskKind::AcquireFood
        );
        assert_eq!(
            AbstractTask::AcquireGood { good: Good::Wood }.kind(),
            AbstractTaskKind::AcquireGood
        );
    }

    #[test]
    fn registry_reports_five_acquire_good_methods() {
        // Phase 5c-ii-d-iv-i: ExploreForMaterialMethod registered as the
        // utility-0.3 fallback, alongside WithdrawMaterialFromStorageMethod
        // (single-task, bare withdraw), WithdrawAndHaulToBlueprintMethod
        // (two-task chain for `JobClaim::Haul` agents), GatherFromKnownMethod
        // (two-task chain for `AgentGoal::GatherWood` / `GatherStone`), and
        // ScavengeFromGroundMethod (two-task chain for visible/known loose
        // ground items). Renamed from `registry_reports_four_acquire_good_methods`.
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::AcquireGood), 5);
    }

    #[test]
    fn withdraw_material_precondition_true_when_stock_and_storage() {
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((2, 3)), 4);
        assert!(m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn withdraw_material_precondition_false_without_storage_tile() {
        let m = WithdrawMaterialFromStorageMethod;
        // Stock recorded but no reachable tile.
        let ctx = ctx_with_material_storage(None, 5);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn withdraw_material_precondition_false_without_stock() {
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((1, 1)), 0);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Stone }, &ctx));
    }

    #[test]
    fn withdraw_material_precondition_false_for_wrong_abstract_task() {
        // Defensive: if a future caller mis-routes the wrong abstract-task
        // variant (e.g. AcquireFood) into this method, `precondition` declines
        // rather than expanding with a defaulted good.
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((1, 1)), 5);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn withdraw_material_expands_to_single_withdraw_task_carrying_good() {
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((6, 9)), 3);
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Stone }, &ctx);
        // qty: 1 — the single-unit acquisition contract; larger needs come
        // from chained calls or a future `FulfillClaim` abstract task.
        assert_eq!(
            tasks,
            vec![Task::WithdrawMaterial {
                good: Good::Stone,
                qty: 1
            }]
        );
    }

    #[test]
    fn withdraw_material_threads_good_through_to_expansion() {
        // The good payload on `AbstractTask::AcquireGood` flows through to
        // the typed task — collapsing per-good legacy plans into one
        // parameterised method is the whole point of 5c.
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((0, 0)), 1);
        let wood = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let iron = m.expand(AbstractTask::AcquireGood { good: Good::Iron }, &ctx);
        assert_eq!(
            wood,
            vec![Task::WithdrawMaterial {
                good: Good::Wood,
                qty: 1
            }]
        );
        assert_eq!(
            iron,
            vec![Task::WithdrawMaterial {
                good: Good::Iron,
                qty: 1
            }]
        );
    }

    #[test]
    fn withdraw_material_expand_returns_empty_without_tile() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(None, 5);
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(tasks.is_empty());
    }

    #[test]
    fn withdraw_material_expand_returns_empty_for_wrong_abstract_task() {
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((1, 1)), 5);
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
    }

    fn ctx_with_gather_target(tile: Option<(i32, i32)>) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: tile,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    #[test]
    fn gather_from_known_precondition_true_when_target_tile_known() {
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((4, 7)));
        assert!(m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn gather_from_known_precondition_false_without_target_tile() {
        let m = GatherFromKnownMethod;
        // No memory of trees / stone tiles for this agent — falls back to
        // the bare-withdraw method or `ExploreFor*`.
        let ctx = ctx_with_gather_target(None);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn gather_from_known_precondition_false_for_wrong_abstract_task() {
        // Defensive: the wrong abstract-task variant gets a clean false even
        // when the gather-target ctx field is populated.
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((1, 1)));
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn gather_from_known_expands_to_gather_then_deposit_chain() {
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((6, 9)));
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        // Two-task chain: gather at the known tile, then deposit at faction
        // storage. The deposit's `good` mirrors the abstract-task payload so
        // chain integrity can be inspected at runtime.
        assert_eq!(
            tasks,
            vec![
                Task::Gather { tile: (6, 9) },
                Task::DepositToFactionStorage { good: Good::Wood },
            ]
        );
    }

    #[test]
    fn gather_from_known_threads_good_through_to_deposit() {
        // The good payload on `AbstractTask::AcquireGood` flows through to
        // the trailing `DepositToFactionStorage` — same parameterisation
        // contract as `WithdrawMaterialFromStorageMethod`'s
        // `threads_good_through_to_expansion` test, but exercises the
        // multi-task chain rather than the single-task expansion.
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((0, 0)));
        let wood = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let stone = m.expand(AbstractTask::AcquireGood { good: Good::Stone }, &ctx);
        assert_eq!(
            wood,
            vec![
                Task::Gather { tile: (0, 0) },
                Task::DepositToFactionStorage { good: Good::Wood },
            ]
        );
        assert_eq!(
            stone,
            vec![
                Task::Gather { tile: (0, 0) },
                Task::DepositToFactionStorage { good: Good::Stone },
            ]
        );
    }

    #[test]
    fn gather_from_known_expand_returns_empty_without_tile() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(None);
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(tasks.is_empty());
    }

    #[test]
    fn gather_from_known_expand_returns_empty_for_wrong_abstract_task() {
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((1, 1)));
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
    }

    fn ctx_with_scavenge_target(
        target: Option<Entity>,
        tile: Option<(i32, i32)>,
    ) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: target,
            scavenge_target_tile: tile,
        }
    }

    #[test]
    fn scavenge_from_ground_precondition_true_when_target_known() {
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(11)), Some((4, 7)));
        assert!(m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn scavenge_from_ground_precondition_false_without_entity() {
        let m = ScavengeFromGroundMethod;
        // Tile populated but no live ground-item entity — falls back to the
        // gather / bare-withdraw / explore methods.
        let ctx = ctx_with_scavenge_target(None, Some((4, 7)));
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn scavenge_from_ground_precondition_false_without_tile() {
        let m = ScavengeFromGroundMethod;
        // Entity recorded but no tile — the dispatcher couldn't route the
        // agent there, so the method must opt out cleanly.
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(11)), None);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn scavenge_from_ground_precondition_false_for_wrong_abstract_task() {
        // Defensive: a wrong abstract-task variant gets a clean false even
        // when both scavenge ctx fields are populated.
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(11)), Some((1, 1)));
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn scavenge_from_ground_expands_to_scavenge_then_deposit_chain() {
        let m = ScavengeFromGroundMethod;
        let target = Entity::from_raw(13);
        let ctx = ctx_with_scavenge_target(Some(target), Some((6, 9)));
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        // Two-task chain: pick up the loose item, then deposit at faction
        // storage. The deposit's `good` mirrors the abstract-task payload so
        // chain integrity can be inspected at runtime.
        assert_eq!(
            tasks,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage { good: Good::Wood },
            ]
        );
    }

    #[test]
    fn scavenge_from_ground_threads_good_through_to_deposit() {
        // The good payload on `AbstractTask::AcquireGood` flows through to
        // the trailing `DepositToFactionStorage` — same parameterisation
        // contract as `WithdrawMaterialFromStorageMethod` and
        // `GatherFromKnownMethod`.
        let m = ScavengeFromGroundMethod;
        let target = Entity::from_raw(21);
        let ctx = ctx_with_scavenge_target(Some(target), Some((0, 0)));
        let wood = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let stone = m.expand(AbstractTask::AcquireGood { good: Good::Stone }, &ctx);
        assert_eq!(
            wood,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage { good: Good::Wood },
            ]
        );
        assert_eq!(
            stone,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage { good: Good::Stone },
            ]
        );
    }

    #[test]
    fn scavenge_from_ground_expand_returns_empty_without_target() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(None, Some((1, 1)));
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(tasks.is_empty());

        // Also defensive: target entity present but tile missing.
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(7)), None);
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(tasks.is_empty());
    }

    #[test]
    fn scavenge_from_ground_expand_returns_empty_for_wrong_abstract_task() {
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(7)), Some((1, 1)));
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
    }

    // ── ScavengeFoodFromGroundMethod (Phase 5c-ii-d-iii-i) ────────────────
    //
    // Mirrors the `ScavengeFromGroundMethod` test pattern but under
    // `AbstractTask::AcquireFood`. The precondition adds a hunger gate
    // (parity with `WithdrawFromStorageMethod`); the expansion is `[Scavenge,
    // Eat]` rather than `[Scavenge, DepositToFactionStorage]`.

    fn ctx_with_food_scavenge_target(
        target: Option<Entity>,
        tile: Option<(i32, i32)>,
        hunger: f32,
    ) -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: target,
            scavenge_target_tile: tile,
        }
    }

    #[test]
    fn scavenge_food_from_ground_precondition_true_when_target_known_and_hungry() {
        let m = ScavengeFoodFromGroundMethod;
        let ctx = ctx_with_food_scavenge_target(
            Some(Entity::from_raw(11)),
            Some((4, 7)),
            EAT_TRIGGER_HUNGER as f32,
        );
        assert!(m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn scavenge_food_from_ground_precondition_false_without_entity() {
        let m = ScavengeFoodFromGroundMethod;
        let ctx = ctx_with_food_scavenge_target(None, Some((4, 7)), 220.0);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn scavenge_food_from_ground_precondition_false_without_tile() {
        let m = ScavengeFoodFromGroundMethod;
        let ctx = ctx_with_food_scavenge_target(Some(Entity::from_raw(11)), None, 220.0);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn scavenge_food_from_ground_precondition_false_when_not_hungry() {
        // Defence in depth: the `htn_acquire_food_dispatch_system` already
        // pre-filters on hunger, but the method gate is symmetric with
        // `WithdrawFromStorageMethod`'s precondition so a future caller that
        // skips the dispatcher pre-filter still gets the right answer.
        let m = ScavengeFoodFromGroundMethod;
        let ctx = ctx_with_food_scavenge_target(
            Some(Entity::from_raw(11)),
            Some((4, 7)),
            (EAT_TRIGGER_HUNGER - 1) as f32,
        );
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn scavenge_food_from_ground_precondition_false_for_wrong_abstract_task() {
        // Defensive: AcquireGood / Sleep / Eat all rejected even when both
        // scavenge fields are populated and hunger is high.
        let m = ScavengeFoodFromGroundMethod;
        let ctx =
            ctx_with_food_scavenge_target(Some(Entity::from_raw(11)), Some((1, 1)), 220.0);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn scavenge_food_from_ground_expands_to_scavenge_then_eat() {
        let m = ScavengeFoodFromGroundMethod;
        let target = Entity::from_raw(13);
        let ctx = ctx_with_food_scavenge_target(Some(target), Some((6, 9)), 220.0);
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        // `[Scavenge, Eat]` — first AcquireFood chain that doesn't end in
        // storage withdraw. The agent picks up the food and eats it on the
        // spot.
        assert_eq!(tasks, vec![Task::Scavenge { target }, Task::Eat]);
    }

    #[test]
    fn scavenge_food_from_ground_expand_returns_empty_without_target() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic (covers both entity-missing
        // and tile-missing).
        let m = ScavengeFoodFromGroundMethod;
        let ctx = ctx_with_food_scavenge_target(None, Some((1, 1)), 220.0);
        assert!(m.expand(AbstractTask::AcquireFood, &ctx).is_empty());

        let ctx = ctx_with_food_scavenge_target(Some(Entity::from_raw(7)), None, 220.0);
        assert!(m.expand(AbstractTask::AcquireFood, &ctx).is_empty());
    }

    #[test]
    fn scavenge_food_from_ground_expand_returns_empty_for_wrong_abstract_task() {
        let m = ScavengeFoodFromGroundMethod;
        let ctx =
            ctx_with_food_scavenge_target(Some(Entity::from_raw(7)), Some((1, 1)), 220.0);
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(tasks.is_empty());
    }

    // ── ExploreForFoodMethod (Phase 5c-ii-d-iv-i) ─────────────────────────
    //
    // Fallback method registered under `AcquireFood` with utility 0.3 (loses
    // to any concrete method). Precondition gates only on hunger so the
    // method is applicable even when storage / scavenge ctx fields are
    // unpopulated — that's the whole point of "fallback when no concrete
    // target." Reuses the existing `ctx_with_storage` helper for hunger-only
    // ctxes (storage tile + stock left at None / 0 model the no-target case).

    #[test]
    fn explore_for_food_precondition_true_when_hungry() {
        let m = ExploreForFoodMethod;
        // Empty storage ctx (`None`, 0) + hungry: no concrete method's
        // precondition fires, so Explore is the only applicable method.
        let ctx = ctx_with_storage(None, 0, EAT_TRIGGER_HUNGER as f32);
        assert!(m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn explore_for_food_precondition_false_when_not_hungry() {
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, (EAT_TRIGGER_HUNGER - 1) as f32);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn explore_for_food_precondition_false_for_wrong_abstract_task() {
        // Defensive: AcquireGood / Sleep / Eat all rejected even when
        // hunger is high.
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn explore_for_food_utility_below_concrete_methods() {
        // Documents the intent that utility ranking is the fallback
        // mechanism: `ExploreForFoodMethod` (0.3) must lose to
        // `WithdrawFromStorageMethod` (1.0) and `ScavengeFoodFromGroundMethod`
        // (1.5) whenever both apply. Pin the literal so a future tuning PR
        // can't silently flip the ordering.
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        let u = m.utility(AbstractTask::AcquireFood, &ctx);
        assert!(u < 1.0, "ExploreForFood utility {} should be below WithdrawFromStorage's 1.0", u);
        assert!(u < 1.5, "ExploreForFood utility {} should be below ScavengeFoodFromGround's 1.5", u);
        assert!(u > 0.0, "ExploreForFood utility {} should be positive (the fallback still beats no method)", u);
    }

    #[test]
    fn explore_for_food_expands_to_single_explore_task_for_food() {
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        assert_eq!(
            tasks,
            vec![Task::Explore {
                kind: MemoryKind::Food
            }]
        );
    }

    #[test]
    fn explore_for_food_expand_returns_empty_for_wrong_abstract_task() {
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        assert!(m
            .expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx)
            .is_empty());
        assert!(m.expand(AbstractTask::Sleep, &ctx).is_empty());
        assert!(m.expand(AbstractTask::Eat, &ctx).is_empty());
    }

    // ── ExploreForMaterialMethod (Phase 5c-ii-d-iv-i) ─────────────────────
    //
    // Fallback method registered under `AcquireGood` with utility 0.3.
    // Precondition gates only on the `good` payload mapping cleanly to a
    // `MemoryKind` (Wood / Stone supported, Iron / Fruit / etc. rejected).
    // The expansion threads the matching `MemoryKind` through to the typed
    // task so one method body serves every supported material.

    fn ctx_empty() -> PlannerCtx {
        PlannerCtx {
            tile: (0, 0),
            faction_id: 1,
            faction_home: Some((0, 0)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
        }
    }

    #[test]
    fn explore_for_material_precondition_true_for_wood() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m.precondition(AbstractTask::AcquireGood { good: Good::Wood }, &ctx));
    }

    #[test]
    fn explore_for_material_precondition_true_for_stone() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m.precondition(AbstractTask::AcquireGood { good: Good::Stone }, &ctx));
    }

    #[test]
    fn explore_for_material_precondition_false_for_unsupported_good() {
        // Iron / Fruit / etc. don't have a corresponding gather goal in the
        // legacy registry, so there's no `MemoryKind` mapping and Explore
        // doesn't apply. The method opts out cleanly rather than expanding
        // with a default kind.
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Iron }, &ctx));
        assert!(!m.precondition(AbstractTask::AcquireGood { good: Good::Fruit }, &ctx));
    }

    #[test]
    fn explore_for_material_precondition_false_for_wrong_abstract_task() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn explore_for_material_utility_below_concrete_methods() {
        // Same intent as `explore_for_food_utility_below_concrete_methods`:
        // pin the fallback ranking so future tuning can't silently invert it.
        // Concrete AcquireGood methods are 1.0 (bare withdraw, gather), 1.5
        // (scavenge), and 2.0 (haul) — Explore must lose to all four.
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        let u = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(u < 1.0, "ExploreForMaterial utility {} should be below 1.0", u);
        assert!(u > 0.0, "ExploreForMaterial utility {} should be positive", u);
    }

    #[test]
    fn explore_for_material_expands_to_single_explore_task_for_wood() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        let tasks = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert_eq!(
            tasks,
            vec![Task::Explore {
                kind: MemoryKind::Wood
            }]
        );
    }

    #[test]
    fn explore_for_material_threads_kind_through_for_stone() {
        // Cross-good test (parallel to `withdraw_material_threads_good_through_to_expansion`
        // and `gather_from_known_threads_good_through_to_deposit`) — proves
        // the parameterisation isn't accidentally short-circuiting on a
        // hardcoded MemoryKind.
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        let wood = m.expand(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let stone = m.expand(AbstractTask::AcquireGood { good: Good::Stone }, &ctx);
        assert_eq!(
            wood,
            vec![Task::Explore {
                kind: MemoryKind::Wood
            }]
        );
        assert_eq!(
            stone,
            vec![Task::Explore {
                kind: MemoryKind::Stone
            }]
        );
    }

    #[test]
    fn explore_for_material_expand_returns_empty_for_unsupported_good() {
        // Defensive: the precondition rejects Iron, but a caller that skips
        // it still gets an empty vec rather than a default-MemoryKind
        // expansion.
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m
            .expand(AbstractTask::AcquireGood { good: Good::Iron }, &ctx)
            .is_empty());
    }

    #[test]
    fn explore_for_material_expand_returns_empty_for_wrong_abstract_task() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m.expand(AbstractTask::AcquireFood, &ctx).is_empty());
        assert!(m.expand(AbstractTask::Sleep, &ctx).is_empty());
        assert!(m.expand(AbstractTask::Eat, &ctx).is_empty());
    }

    // ── Distance-weighted utility (Phase 5c-ii-d-v) ────────────────────────
    //
    // Each `Method` whose ctx carries a target tile subtracts a per-tile
    // penalty from its base utility (capped at `MAX_DIST_PENALTY`). The
    // tests below pin: (a) the helpers themselves; (b) that closer targets
    // outscore farther ones for the same method; (c) that the cap preserves
    // the inter-method ranking established by the flat utilities (1.0 / 1.5
    // / 2.0). Future tuning PRs that re-tune `DIST_DISCOUNT_PER_TILE` /
    // `MAX_DIST_PENALTY` must keep the cap-preserves-ranking invariant.

    #[test]
    fn chebyshev_dist_uses_max_axis() {
        assert_eq!(chebyshev_dist((0, 0), (3, 4)), 4);
        assert_eq!(chebyshev_dist((0, 0), (-7, 2)), 7);
        assert_eq!(chebyshev_dist((5, 5), (5, 5)), 0);
    }

    #[test]
    fn dist_penalty_caps_at_max() {
        // 30 tiles * 0.02/tile = 0.60 raw, but capped at MAX_DIST_PENALTY.
        let p = dist_penalty((0, 0), Some((30, 0)));
        assert!((p - MAX_DIST_PENALTY).abs() < 1e-6);
    }

    #[test]
    fn dist_penalty_zero_for_no_target() {
        // ctx fields default to None when the dispatcher hasn't populated
        // them — methods read at base utility in that case.
        assert_eq!(dist_penalty((0, 0), None), 0.0);
    }

    #[test]
    fn withdraw_from_storage_utility_decreases_with_distance() {
        let m = WithdrawFromStorageMethod;
        let near = ctx_with_storage(Some((1, 0)), 5, 220.0);
        let far = ctx_with_storage(Some((10, 0)), 5, 220.0);
        let u_near = m.utility(AbstractTask::AcquireFood, &near);
        let u_far = m.utility(AbstractTask::AcquireFood, &far);
        assert!(u_near > u_far, "near {} should outscore far {}", u_near, u_far);
    }

    #[test]
    fn scavenge_food_outranks_withdraw_even_at_max_distance() {
        // Cap-preserves-ranking invariant: 1.5 - 0.30 = 1.20 > 1.0 - 0 = 1.0.
        // A far visible food pile still beats a near-zero-distance storage
        // tile because the bias-on-visibility margin is wider than
        // MAX_DIST_PENALTY.
        let scav = ScavengeFoodFromGroundMethod;
        let wd = WithdrawFromStorageMethod;
        let mut ctx = ctx_with_storage(Some((0, 0)), 5, 220.0);
        ctx.scavenge_target_entity = Some(Entity::from_raw(1));
        ctx.scavenge_target_tile = Some((30, 0)); // beyond MAX_DIST_PENALTY
        let u_scav = scav.utility(AbstractTask::AcquireFood, &ctx);
        let u_wd = wd.utility(AbstractTask::AcquireFood, &ctx);
        assert!(u_scav > u_wd, "scavenge {} should still beat withdraw {}", u_scav, u_wd);
    }

    #[test]
    fn scavenge_food_closer_target_wins_over_farther() {
        let m = ScavengeFoodFromGroundMethod;
        let near = ctx_with_food_scavenge_target(Some(Entity::from_raw(1)), Some((2, 0)), 220.0);
        let far = ctx_with_food_scavenge_target(Some(Entity::from_raw(2)), Some((10, 0)), 220.0);
        let u_near = m.utility(AbstractTask::AcquireFood, &near);
        let u_far = m.utility(AbstractTask::AcquireFood, &far);
        assert!(u_near > u_far);
    }

    #[test]
    fn withdraw_material_utility_decreases_with_distance() {
        let m = WithdrawMaterialFromStorageMethod;
        let near = ctx_with_material_storage(Some((1, 1)), 5);
        let far = ctx_with_material_storage(Some((12, 12)), 5);
        let u_near = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &near);
        let u_far = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &far);
        assert!(u_near > u_far);
    }

    #[test]
    fn haul_outranks_bare_withdraw_at_any_distance() {
        // Cap-preserves-ranking: 2.0 - 0.30 = 1.70 > 1.0 - 0 = 1.0. Even with
        // the haul method's storage tile at max-penalty distance and the
        // bare-withdraw method at zero distance (a degenerate ctx), haul
        // still wins by 0.70+.
        let haul = WithdrawAndHaulToBlueprintMethod;
        let bp = Entity::from_raw(99);
        let ctx = ctx_with_haul_claim(Some((30, 30)), 5, Some(bp));
        let bare = WithdrawMaterialFromStorageMethod;
        // Bare-withdraw on a degenerate ctx with storage at zero distance:
        let bare_ctx = ctx_with_material_storage(Some((0, 0)), 5);
        let u_haul = haul.utility(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let u_bare = bare.utility(AbstractTask::AcquireGood { good: Good::Wood }, &bare_ctx);
        assert!(u_haul > u_bare, "haul {} should beat bare-withdraw {}", u_haul, u_bare);
    }

    #[test]
    fn gather_from_known_utility_decreases_with_distance() {
        let m = GatherFromKnownMethod;
        let near = ctx_with_gather_target(Some((2, 0)));
        let far = ctx_with_gather_target(Some((12, 0)));
        let u_near = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &near);
        let u_far = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &far);
        assert!(u_near > u_far);
    }

    #[test]
    fn scavenge_outranks_gather_even_at_max_distance() {
        // AcquireGood analogue of `scavenge_food_outranks_withdraw_even_at_max_distance`.
        // 1.5 - 0.30 (far scavenge) = 1.20 > 1.0 - 0 (zero-distance gather).
        // A worker who sees a faraway loose log still picks scavenge over a
        // tree at their feet.
        let scav = ScavengeFromGroundMethod;
        let gath = GatherFromKnownMethod;
        let mut ctx = ctx_with_gather_target(Some((0, 0)));
        ctx.scavenge_target_entity = Some(Entity::from_raw(5));
        ctx.scavenge_target_tile = Some((30, 0));
        let u_scav = scav.utility(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        let u_gath = gath.utility(AbstractTask::AcquireGood { good: Good::Wood }, &ctx);
        assert!(u_scav > u_gath);
    }

    #[test]
    fn scavenge_from_ground_closer_target_wins_over_farther() {
        let m = ScavengeFromGroundMethod;
        let near = ctx_with_scavenge_target(Some(Entity::from_raw(1)), Some((2, 0)));
        let far = ctx_with_scavenge_target(Some(Entity::from_raw(2)), Some((12, 0)));
        let u_near = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &near);
        let u_far = m.utility(AbstractTask::AcquireGood { good: Good::Wood }, &far);
        assert!(u_near > u_far);
    }

    #[test]
    fn explore_loses_to_any_concrete_method_at_any_distance() {
        // Concrete methods at max-distance penalty (1.0 - 0.30 = 0.70 for
        // bare-withdraw; 1.5 - 0.30 = 1.20 for scavenge; 2.0 - 0.30 = 1.70
        // for haul) all stay strictly above Explore's 0.3.
        let exp_food = ExploreForFoodMethod;
        let wd = WithdrawFromStorageMethod;
        let scav = ScavengeFoodFromGroundMethod;
        let mut ctx = ctx_with_storage(Some((30, 30)), 5, 220.0);
        ctx.scavenge_target_entity = Some(Entity::from_raw(1));
        ctx.scavenge_target_tile = Some((30, 30));
        let u_exp = exp_food.utility(AbstractTask::AcquireFood, &ctx);
        let u_wd = wd.utility(AbstractTask::AcquireFood, &ctx);
        let u_scav = scav.utility(AbstractTask::AcquireFood, &ctx);
        assert!(u_exp < u_wd);
        assert!(u_exp < u_scav);
    }
}
