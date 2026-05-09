//! Typed Task variants — Phases 3 and 4 of the Plan/Task System Redesign.
//!
//! Today the agent's "current action" is encoded as `PersonAI.task_id: u16`
//! plus a smear of loose target fields (`dest_tile`, `target_z`,
//! `target_entity`, `withdraw_good`, ...). Every executor reads its parameters
//! out of those fields, and every plan-end path has to remember to clear
//! them. The redesign moves task parameters onto a single typed `Task`
//! variant so executors get their inputs from one well-known place and
//! teardowns are mechanical (replace the variant with `Idle`).
//!
//! Phase 3 introduced the `Task` enum and migrated each task family one at a
//! time. Phase 4a promoted the typed task off `PersonAI` and onto a dedicated
//! `ActionQueue` component. Phase 4b-i added the `queued` ring (capacity 4) so
//! a future dispatcher can pre-decompose multi-step plans into a sequence of
//! typed tasks and executors can pop the next one without re-entering plan
//! selection. Phase 4b-ii wires the ring into the live runtime:
//!
//! - **Producer.** Plan dispatchers (`plan_execution_system`, the player-order
//!   handlers in `ui/orders.rs`, the Read player-order handler in `teaching.rs`)
//!   route through `ActionQueue::dispatch(task)` instead of writing `current`
//!   directly. `dispatch` enqueues the task and immediately promotes it into
//!   `current` if `current` is `Idle` — so single-task dispatches are
//!   behaviourally identical to the old direct write, while multi-task chains
//!   from a future method library accumulate behind `current` correctly.
//! - **Consumer.** Executors that today wrote `aq.current = Task::Idle` on
//!   completion now call `aq.advance()`, which pops the head of the queue
//!   (or sets `current = Idle` when empty). With no producer pushing chains
//!   yet, this is a no-op behaviourally; once Phase 5 method bodies push
//!   multi-task expansions, executor exit transitions are already wired to
//!   promote the next task without re-entering plan selection.
//! - **External preempts.** Sites that abort an in-flight plan (player muster,
//!   chief hunter demote, goal-flip stale reset in `goal_dispatch_system`) call
//!   `aq.cancel()` instead of `current = Idle`, dropping both `current` and
//!   the prefetched queue so a chained follow-up doesn't outlive its plan.
//!
//! Per-tick "pin" sites that re-assert the current task while an activity
//! component is alive (lecture/teach pin in `teaching.rs`) deliberately stay
//! as direct `current = X` writes — they're idempotent re-assertions of the
//! state, not fresh dispatches, and routing them through `dispatch()` would
//! pile duplicates onto the queue every tick.
//!
//! The legacy `task_id` / `dest_tile` / `target_z` / `target_entity` fields
//! still co-exist on `PersonAI` because not every consumer has migrated to
//! the typed channel yet. They get retired family-by-family as Phase 4
//! progresses.

use bevy::prelude::Component;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::items::EquipmentSlot;
use crate::simulation::memory::MemoryKind;
use crate::simulation::technology::TechId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkReason {
    /// Drafted unit walking to a player-issued rally point. Arrival drops
    /// the unit back to Idle in place.
    MilitaryMove,
    /// Routing to a known gather target (tree / stone tile / berry bush).
    /// The arrival flips into a `Task::Gather { tile }` step. Used by HTN
    /// `GatherFromKnownMethod` (Phase 5c-ii-c) — scaffolding only at
    /// 5c-ii-c-i; no dispatcher emits this reason yet.
    Gather,
}

/// Selector for `Task::WithdrawGood`: which item on the storage tile satisfies
/// the request. Replaces the `ai.craft_recipe_id == ENTERTAINMENT_SENTINEL`
/// (255) overload that used the same u8 channel as the Craft / Equip
/// dispatchers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawGoodFilter {
    /// Pull exactly this resource from the tile (catalog `ResourceId` equality
    /// against `GroundItem.item.resource_id`).
    Specific(ResourceId),
    /// Pull any item whose `entertainment_value() > 0`. Used by `PLAY_WITH_STORED_TOY`
    /// to scavenge whatever toy is on the tile rather than committing to a
    /// specific good at plan-author time.
    AnyEntertainment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Task {
    /// No typed task is active. Either the legacy task_id is still
    /// authoritative (for un-migrated families) or the agent is genuinely
    /// idle.
    Idle,
    /// Walk to `(tile, z)` — pure movement, no work phase on arrival.
    WalkTo {
        tile: (i32, i32),
        z: i8,
        why: WalkReason,
    },
    /// Withdraw one matching item from a faction storage tile. The tile
    /// itself is still routed via the legacy `dest_tile` field; this variant
    /// only owns the *what to take* parameters. Once Phase 3 finishes, the
    /// storage tile entity will live here too.
    WithdrawGood {
        filter: WithdrawGoodFilter,
    },
    /// Withdraw `qty` units of `resource_id` for a faction-blueprint /
    /// craft-order / haul-claim need. Replaces
    /// `PersonAI.{withdraw_good, withdraw_qty}`. The reservation against the
    /// storage tile still lives on the legacy `reserved_*` fields because
    /// every cleanup path goes through `release_reservation` — Phase 3
    /// collapses that into a `Drop` guard once the loose-target fields are
    /// fully retired.
    WithdrawMaterial {
        resource_id: ResourceId,
        qty: u8,
    },
    /// P2b: nomadic / member-pool counterpart to `WithdrawMaterial`. Walks
    /// the actor adjacent to `target` (a fellow faction member), then pulls
    /// `qty` units of `resource_id` out of the target's `EconomicAgent.inventory`
    /// (or `Carrier` hands) into the actor's. No `StorageReservations` — member
    /// pools are claim-light today; collisions are best-effort.
    ///
    /// Today this variant has no dispatcher caller (nomadic factions have
    /// `caps.posting=Disabled` and never reach `AcquireGood`), but the
    /// executor + task are wired so the path is reachable as soon as a
    /// caller emits the variant.
    WalkAndTakeFromMember {
        target: bevy::prelude::Entity,
        resource_id: ResourceId,
        qty: u8,
    },
    /// Move a matching `Item` from inventory or hands into `Equipment[slot]`.
    /// Replaces `PersonAI.equip_slot` (sentinel-encoded `u8`) and the Equip
    /// usage of `craft_recipe_id` (good as u8).
    Equip {
        slot: EquipmentSlot,
        resource_id: ResourceId,
    },
    /// Build a wall / bed / other structure from a `Blueprint` entity. The
    /// `task_id` discriminates `Construct` vs `ConstructBed` for reward
    /// scaling — only the blueprint entity is shared. Replaces
    /// `PersonAI.target_entity` reads in `construction_system`'s worker
    /// branch; the field stays populated for now because Hunt / Attack /
    /// other families haven't migrated yet.
    Construct {
        blueprint: bevy::prelude::Entity,
    },
    /// Harvest from a tile: plant fruits/wood, surface stone, or ore.
    /// `gather_system` inspects the tile contents to decide; the typed task
    /// just owns the *which tile*. Replaces `dest_tile` reads in the gather
    /// executor — the legacy field stays populated because `movement_system`
    /// still drives routing off of it.
    Gather {
        tile: (i32, i32),
    },
    /// Dig down at a tile, lowering its floor by one Z and producing the
    /// carved material as drops. Same shape as `Gather` — `dig_system` reads
    /// the tile from this variant; legacy `dest_tile` is kept for routing.
    Dig {
        tile: (i32, i32),
    },
    /// Pick up a specific `GroundItem` entity. Replaces the `TargetItem.0`
    /// read in `item_pickup_system` for the Scavenge branch — the typed
    /// channel is now the source of truth, falling back to `TargetItem.0`
    /// only for legacy player-dispatch sites.
    Scavenge {
        target: bevy::prelude::Entity,
    },
    /// Solo study of a tablet/book in inventory. Replaces `tech_focus`
    /// (the loose `Option<TechId>` field) for the Read executor.
    Read {
        tech: TechId,
    },
    /// 1-on-1 teaching: teacher stays adjacent to student and transfers
    /// progress on `tech`. The student-side state (`BeingTaught`) is a
    /// separate component; this variant only owns the teacher's params.
    Teach {
        tech: TechId,
    },
    /// Stand at lecture anchor and broadcast progress to nearby attendees.
    HoldLecture {
        tech: TechId,
    },
    /// Stand near a lecturer and accumulate study progress on `tech`.
    AttendLecture {
        tech: TechId,
    },
    /// Walk adjacent to a `Corpse` entity and attach it to
    /// `PersonAI.carried_corpse`. Replaces `target_entity` reads in
    /// `pickup_corpse_task_system`. The downstream HaulCorpse + Butcher tasks
    /// also have typed variants for chain-integrity inspection — they read
    /// the corpse from the `Carrying` component (which spans pickup → haul →
    /// butcher) rather than per-task params, so the typed variants only
    /// document the phase.
    PickUpCorpse {
        corpse: bevy::prelude::Entity,
    },
    /// Walk adjacent to another Person and converse — `social_fill_system`
    /// reduces `needs.social` for any agent with neighbours within
    /// `SOCIAL_RADIUS`, and `needs.rs`'s table+chair bonus reads
    /// `task_id == TaskKind::Socialize` for furniture-assisted recovery.
    /// There is no dedicated executor — the dispatcher routes via
    /// `assign_task_with_routing(... TaskKind::Socialize, Some(partner) ...)`
    /// and the agent simply sits in the task until `goal_update_system`
    /// flips them off `AgentGoal::Socialize`. Produced by HTN
    /// `SocializeWithPartnerMethod` (replaces legacy `Socialize` plan
    /// PlanId 60 + StepId 48).
    Socialize {
        partner: bevy::prelude::Entity,
    },
    /// Walk to the home tile of the faction this agent is raiding (per
    /// `FactionRegistry::raid_target`). On arrival the legacy executor's
    /// task entry, `task_requires_free_hands`, and `task_interacts_from_adjacent`
    /// govern the engagement; `combat_system` picks fights with any enemy
    /// faction member encountered along the way. Produced by HTN
    /// `RaidEnemyHomeMethod` (replaces legacy `Raid` plan PlanId 61 +
    /// StepId 49).
    Raid {
        dest: (i32, i32),
    },
    /// Walk to the agent's faction home tile and stand watch. No dedicated
    /// executor — the agent stays in `TaskKind::Defend` until
    /// `goal_update_system` flips them off `AgentGoal::Defend` (typically
    /// when the faction is no longer `is_under_raid`). Produced by HTN
    /// `DefendCampMethod` (replaces legacy `Defend` plan PlanId 62 +
    /// StepId 50).
    Defend {
        dest: (i32, i32),
    },
    /// Tribal chief in peacetime walks to the faction home tile and runs
    /// `TaskKind::Lead` — used by `chief_*` systems as a "chief is on duty"
    /// signal. No dedicated executor; the chief stays here until
    /// `goal_update_system` peels them off `AgentGoal::Lead` (crisis,
    /// hunger, sleep). Produced by HTN `LeadCampMethod` (replaces legacy
    /// `Lead` plan PlanId 63 + StepId 51).
    Lead {
        dest: (i32, i32),
    },
    /// Distress responder routes to the attacker carried on the agent's
    /// `RescueTarget` component. The dispatcher mirrors the legacy
    /// `StepTarget::RescueAttacker` resolver: writes `CombatTarget(Some(attacker))`
    /// before routing so `combat_system` engages on adjacency. The variant
    /// carries the attacker entity for chain-integrity inspection; the
    /// destination tile lives on `dest`. Produced by HTN
    /// `EngageRescueAttackerMethod` (replaces legacy `RescueAlly` plan
    /// PlanId 23 + StepId 27 EngageRescue).
    RescueAlly {
        attacker: bevy::prelude::Entity,
        dest: (i32, i32),
    },
    /// Walk to the chief's chosen muster hearth tile and register the agent
    /// into the faction's `HuntOrder::Hunt::mustered` list. The executor
    /// (`wait_for_party_task_system`) blocks on arrival until the party fills
    /// (`mustered.len() >= target_party_size`) or the order goes stale. The
    /// `hearth` tile is also written to legacy `dest_tile` for routing.
    /// Produced by the future HTN `MusterAtHearthMethod` and by the legacy
    /// `HuntFood` plan's StepId(57).
    HuntPartyMuster {
        hearth: (i32, i32),
    },
    /// Hunt down the named prey entity. There is no dedicated executor — the
    /// dispatcher routes the agent to the prey via
    /// `assign_task_with_routing(... TaskKind::Hunter, Some(prey) ...)` and
    /// sets `CombatTarget`, then `combat_system` engages the moment the
    /// agent is adjacent. The variant carries the prey entity for chain
    /// inspection (the future HTN `EngagePreyMethod` will read it; the
    /// legacy `HuntFood` plan's StepId(5) writes it for parity).
    Hunt {
        prey: bevy::prelude::Entity,
    },
    /// Drag the carried corpse to the named butcher-site tile. The corpse
    /// itself follows via `corpse_follow_system` (no typed-task input). The
    /// `dest` tile is also written to legacy `dest_tile` for routing.
    /// Produced by the future HTN `HaulCorpseMethod` and by the legacy
    /// `HuntFood` plan's StepId(54).
    HaulCorpse {
        dest: (i32, i32),
    },
    /// Butcher the carried corpse in place. The corpse comes from the
    /// `Carrying` component (set at PickUpCorpse arrival, cleared on butcher
    /// completion). Parameterless because every input is component-level
    /// state. Produced by the future HTN `ButcherCorpseMethod` and by the
    /// legacy `HuntFood` plan's StepId(55).
    Butcher,
    /// Work adjacent to a wild tameable animal (horse / cow / pig / cat) for
    /// `TICKS_TAME` accumulating ticks, then insert `Tamed { owner_faction }`
    /// on the target. Per-species tech gates (`HORSE_TAMING`,
    /// `ANIMAL_HUSBANDRY`, `DOG_DOMESTICATION`) checked inside
    /// `tame_task_system` against the agent's faction. Routing happens via
    /// `assign_task_with_routing` to the target's tile; the legacy executor
    /// reads `target_entity` for backwards compatibility — the typed variant
    /// is what the HTN dispatcher (`htn_tame_horse_dispatch_system`) emits
    /// for chain-integrity inspection. Replaces the legacy `TameHorse` plan
    /// (PlanId 10).
    TameAnimal {
        target: bevy::prelude::Entity,
    },
    /// Plant one seed (Grain / Berry / …) from the agent's inventory or hands
    /// onto an unplanted Farmland tile. The executor (`production_system`'s
    /// Planter branch) walks `PlantKind::ALL` to pick the matching plant for
    /// whichever seed is held, so the variant only needs the destination tile.
    /// Routing happens via `assign_task_with_routing` (set up by the HTN chain
    /// handoff in `production::finish_withdraw_material`); the legacy executor
    /// reads `dest_tile` for backwards compatibility — the typed variant is
    /// what the HTN dispatcher (`htn_plant_from_storage_dispatch_system`)
    /// emits for chain-integrity inspection. Replaces the dead legacy
    /// `PlantFromStorage` / `PlantBerryFromStorage` plans (PlanIds 4, 66).
    Planter {
        tile: (i32, i32),
    },
    /// Agent is tired and is either routing toward a bed / faction home or
    /// already asleep in place. The Sleep "executor" is a state transition
    /// (`AiState::Sleeping`) rather than a per-tick task system, so this
    /// variant is bookkeeping only today: it makes Sleep visible in the typed
    /// channel alongside every other task family and prepares the dispatcher
    /// for Phase 5a, where an HTN method will produce this variant directly.
    /// `bed = None` means "sleep in place" (solo agent, or at-home with no
    /// claimed bed yet).
    Sleep {
        bed: Option<bevy::prelude::Entity>,
    },
    /// Consume edibles from inventory or hands in place. The agent stays in
    /// `AiState::Working` accumulating `work_progress` until `TICKS_EAT`, then
    /// `eat_task_system` consumes one item per loop and reduces hunger. The
    /// variant carries no parameters because the executor inspects
    /// inventory + hands itself (smallest-cover-then-largest selection across
    /// every edible the agent is currently carrying). Bookkeeping only at
    /// Phase 5b-i — `EatFromInventoryMethod` produces this variant but no
    /// dispatcher consumes the typed channel yet (the legacy `task_id ==
    /// TaskKind::Eat` path is still authoritative).
    Eat,
    /// Pull one edible item off a faction storage tile into the agent's
    /// hands or inventory. The agent works from a tile adjacent to `tile`
    /// (routing happens via the legacy `dest_tile` channel in 5b-iii-ii;
    /// today the variant is scaffolding only). Mirrors the legacy
    /// `TaskKind::WithdrawFood` executor, which runs as a single-tick
    /// withdraw — no per-tick work accumulation. Produced by
    /// `WithdrawFromStorageMethod` as the first leg of an
    /// `AcquireFood → WithdrawFood → Eat` chain.
    WithdrawFood {
        tile: (i32, i32),
    },
    /// Carry the agent's hand contents to the named `Blueprint` and drop them
    /// into its deposit slots. Produced by `WithdrawAndHaulToBlueprintMethod`
    /// as the second leg of an `AcquireGood → WithdrawMaterial → HaulToBlueprint`
    /// chain (Phase 5c-ii-b — replaces the legacy `ClaimedHaul` plan).
    /// The "executor" is `construction_system`'s hauler branch, which already
    /// knows how to deposit-on-arrival via `task_id == TaskKind::HaulMaterials`
    /// and `target_entity = Some(blueprint)`. The typed variant carries the
    /// blueprint so the chain handoff in `withdraw_material_task_system`
    /// (`finish_withdraw_material`) has everything it needs to look up the
    /// tile and route the agent without re-entering plan selection.
    HaulToBlueprint {
        blueprint: bevy::prelude::Entity,
    },
    /// Carry the agent's hand contents to the named `CraftOrder` anchor and
    /// drop matching held goods into its deposit slots. Produced by
    /// `WithdrawAndHaulToCraftOrderMethod` (Phase 5e-xi-a — replaces the legacy
    /// `DeliverFromStorageToCraftOrder` plan, PlanId 15) as the second leg of
    /// a `[WithdrawMaterial, HaulToCraftOrder]` chain. The executor is
    /// `craft_order_system`'s hauler branch, which already knows how to
    /// deposit-on-arrival via `task_id == TaskKind::HaulToCraftOrder` and
    /// `target_entity = Some(order)`. The typed variant carries the order so
    /// the chain handoff in `production::finish_withdraw_material` has
    /// everything it needs to route the agent to the order's anchor tile
    /// without re-entering plan selection.
    HaulToCraftOrder {
        order: bevy::prelude::Entity,
    },
    /// Recreational play. `partner = Some(e)` for social play (the agent walks
    /// adjacent to another `Person` and plays together — `play_system` reads
    /// the partner from `ai.target_entity` set up by routing); `partner = None`
    /// for solo play with a held or adjacent entertainment item. Produced by
    /// `PlayWithPartnerMethod` / `PlaySoloMethod` (Phase 5e-xii-a — replaces
    /// the legacy `PlaySocial` plan PlanId 26 + `PlaySolo` plan PlanId 27).
    /// The executor `play_system` already handles both cases via the legacy
    /// `task_id == TaskKind::Play` channel; the typed variant only owns the
    /// partner reference for chain inspection and `aq.advance()` drainage.
    Play {
        partner: Option<bevy::prelude::Entity>,
    },
    /// Recreational rock-throwing in place. Consumes one Stone from inventory
    /// (or hands), awards Combat XP + `ActivityKind::Combat`, bursts willpower.
    /// Parameterless because the executor (`production_system`'s PlayThrow
    /// branch) acts in place at the agent's current tile and reads the stone
    /// from `EconomicAgent`. Produced by `WithdrawAndThrowStonesAsPlayMethod`
    /// (Phase 5e-xii-b — replaces the legacy `PlayByThrowingRocks` plan,
    /// PlanId 31) as the trailing leg of a
    /// `[WithdrawMaterial { stone, 1 }, PlayThrow]` chain. The chain handoff
    /// in `production::finish_withdraw_material` primes the legacy channel
    /// with `task_id = TaskKind::PlayThrow` once the stone is in hand — the
    /// throw is in-place, no routing required.
    PlayThrow,
    /// Recreational seed-planting on an unplanted grass tile. Consumes one
    /// Grain or Berry seed from inventory or hands, spawns the matching
    /// `PlantKind` at `tile`, awards Farming XP + `ActivityKind::Farming`,
    /// bursts willpower. Shares `production_system`'s Planter branch with
    /// `Task::Planter { tile }` — the only difference is `is_play = true`
    /// for the willpower burst on completion. Produced by
    /// `WithdrawAndPlantSeedAsPlayMethod` / `WithdrawAndPlantBerrySeedAsPlayMethod`
    /// (Phase 5e-xii-d — replaces the legacy `PlayByPlanting` plan, PlanId 30,
    /// and `PlayByPlantingBerry` plan, PlanId 67) as the trailing leg of a
    /// `[WithdrawMaterial { seed, 1 }, PlayPlant { tile }]` chain. The chain
    /// handoff in `production::finish_withdraw_material` routes via
    /// `TaskKind::PlayPlant` to the destination grass tile carried by the
    /// typed variant once the seed is in hand.
    PlayPlant {
        tile: (i32, i32),
    },
    /// Work adjacent to a satisfied `CraftOrder` until the recipe completes.
    /// Produced by `WorkOnSatisfiedCraftOrderMethod` (Phase 5e-xi-b — replaces
    /// the legacy `WorkOnCraft` plan, PlanId 16) as the head of a
    /// `[WorkOnCraftOrder, DepositToFactionStorage]` chain. The executor is
    /// `craft_order_system`'s worker branch, which already advances
    /// `work_progress` per tick once the order's deposits are full and pays
    /// out the recipe output to the lead worker on completion. The typed
    /// variant carries the order so dispatch / chain handoff has the right
    /// target without re-querying.
    WorkOnCraftOrder {
        order: bevy::prelude::Entity,
    },
    /// Carry the agent's hand contents to the nearest faction storage tile and
    /// drop them. Produced by `GatherFromKnownMethod` (Phase 5c-ii-c) as the
    /// trailing leg of an `AcquireGood → Gather → DepositToFactionStorage`
    /// chain — the typed analogue of legacy `StepId(12)` "DepositGoods".
    /// The "executor" is the legacy `TaskKind::DepositResource` path
    /// (`faction_dump_at_storage_system`), which is parameterless: it dumps
    /// everything in hands at the current `dest_tile`. The `good` payload is
    /// recorded here for chain-integrity inspection (the dispatcher and the
    /// executor's exit can assert "this chain is depositing Wood, did the
    /// Gather step actually leave Wood in our hands?") and to keep the
    /// AcquireGood-family symmetric with `WithdrawMaterial { good, .. }` and
    /// `HaulToBlueprint { blueprint }` — every variant in the family
    /// documents what the agent is *for*. Scaffolding only at 5c-ii-c-i:
    /// `GatherFromKnownMethod` produces the variant in unit tests, but no
    /// dispatcher consumes the typed channel yet — the legacy `GatherWood` /
    /// `GatherStone` plans (PlanId 2/3) remain authoritative.
    DepositToFactionStorage {
        resource_id: ResourceId,
    },
    /// Walk to a random reachable tile near the agent's faction home, hoping
    /// to record a `MemoryKind::{kind}` sighting along the way. Produced by
    /// `ExploreForFoodMethod` (under `AcquireFood`) and `ExploreForMaterialMethod`
    /// (under `AcquireGood`) as the lone-task expansion when no concrete target
    /// is in ctx — the HTN analogue of the legacy `ExploreForFood` / `ExploreForWood`
    /// / `ExploreForStone` plans (PlanId 35/36/37, all single-step
    /// `[StepId(31)/Explore]`). The "executor" is the legacy `TaskKind::Explore`
    /// path: `StepTarget::ExploreTile` resolver picks a random reachable tile,
    /// `movement_system` walks the agent there, and `vision_system` records any
    /// matching memory entry along the path. Termination is handled the same
    /// way as the legacy plan: `explore_satisfaction_system` aborts the plan
    /// the moment matching memory is recorded; under HTN, the next dispatch
    /// tick will see the populated memory and pick the appropriate concrete
    /// method instead. The `kind` payload mirrors the legacy plan's
    /// `memory_target_kind` field — it documents what the agent is *for* and
    /// lets the future dispatcher (5c-ii-d-iv-ii) verify chain integrity.
    /// **Scaffolding only at 5c-ii-d-iv-i**: the variant is produced in unit
    /// tests but no dispatcher consumes the typed channel yet; the legacy
    /// `ExploreForFood` / `ExploreForWood` / `ExploreForStone` plans remain
    /// authoritative.
    Explore {
        kind: MemoryKind,
    },
}

impl Default for Task {
    fn default() -> Self {
        Task::Idle
    }
}

/// Cap on the prefetched-task queue. Four slots is enough to hold the typed
/// task chains that today are spread across consecutive plan steps (e.g.
/// `WalkTo → WithdrawMaterial → WalkTo → DepositGoods`) without an allocation
/// in the hot path. If a method ever needs more, it should bump this constant
/// rather than spilling to a heap-backed Vec.
pub const ACTION_QUEUE_CAP: usize = 4;

/// Per-agent typed-task slot. Phase 4a introduced the component (`current`
/// only); Phase 4b-i adds the `queued` prefetch ring.
///
/// - `current` is the task the executors run *now*. Defaulting to `Task::Idle`.
/// - `queued` is a fixed-capacity FIFO of tasks scheduled to follow `current`.
///   Empty in Phase 4b-i. Producers will push via `enqueue` once 4b-ii lands;
///   the executor advance will promote via `pop_next()` when `current` becomes
///   `Idle`.
///
/// Held as its own component so:
/// - the queue can grow without churning `PersonAI`'s shape;
/// - executors can request `&mut ActionQueue` independently of the rest of
///   `PersonAI`, leaving room for finer-grained system parallelism later;
/// - teardown sites can clear the task with a single field write rather than
///   reaching into `PersonAI`.
///
/// Every `Person` entity carries one `ActionQueue`, defaulting to
/// `ActionQueue::idle()`. When the legacy `task_id` is `PersonAI::UNEMPLOYED`
/// the typed `current` is always `Task::Idle`; the inverse is *not*
/// guaranteed during dispatch transitions, so executors still validate the
/// pair via the inconsistent-state guard pattern.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionQueue {
    pub current: Task,
    queued: [Task; ACTION_QUEUE_CAP],
    queued_len: u8,
}

impl Default for ActionQueue {
    fn default() -> Self {
        Self::idle()
    }
}

impl ActionQueue {
    pub const fn idle() -> Self {
        Self {
            current: Task::Idle,
            queued: [Task::Idle; ACTION_QUEUE_CAP],
            queued_len: 0,
        }
    }

    /// Number of prefetched tasks waiting behind `current`.
    pub fn queued_len(&self) -> usize {
        self.queued_len as usize
    }

    pub fn queued_is_empty(&self) -> bool {
        self.queued_len == 0
    }

    pub fn queued_is_full(&self) -> bool {
        self.queued_len as usize >= ACTION_QUEUE_CAP
    }

    /// Read the next prefetched task without consuming it. Returns `None` if
    /// the queue is empty.
    pub fn peek_next(&self) -> Option<Task> {
        if self.queued_is_empty() {
            None
        } else {
            Some(self.queued[0])
        }
    }

    /// Push a task onto the back of the prefetched queue. Returns `false`
    /// without modifying the queue if it is at capacity — callers should treat
    /// a full queue as a producer bug (the dispatcher should not be enqueuing
    /// more than four tasks ahead of `current`).
    pub fn enqueue(&mut self, task: Task) -> bool {
        if self.queued_is_full() {
            return false;
        }
        let idx = self.queued_len as usize;
        self.queued[idx] = task;
        self.queued_len += 1;
        true
    }

    /// Pop the front of the queue. Returns `Task::Idle` if empty so callers
    /// can unconditionally promote into `current`.
    pub fn pop_next(&mut self) -> Task {
        if self.queued_is_empty() {
            return Task::Idle;
        }
        let head = self.queued[0];
        let new_len = (self.queued_len - 1) as usize;
        for i in 0..new_len {
            self.queued[i] = self.queued[i + 1];
        }
        self.queued[new_len] = Task::Idle;
        self.queued_len = new_len as u8;
        head
    }

    /// Dispatch a freshly resolved task. Pushes onto the prefetched queue and
    /// — if `current` is `Idle` — immediately promotes the head so the
    /// executor sees the new task this tick. This is the canonical write-path
    /// for plan dispatchers and player-order systems: they never touch
    /// `current` directly. Returns `false` (without modifying anything) when
    /// the queue is at capacity, which is a producer bug — dispatchers must
    /// not try to push more than `ACTION_QUEUE_CAP` ahead of the running task.
    pub fn dispatch(&mut self, task: Task) -> bool {
        if !self.enqueue(task) {
            return false;
        }
        if self.current == Task::Idle {
            self.current = self.pop_next();
        }
        true
    }

    /// Called by an executor when its current task finishes (success, soft
    /// failure, or precondition expiry — anything that is *not* an external
    /// preempt). Promotes the next prefetched task into `current`, or sets
    /// `current = Task::Idle` if the queue is empty. The prefetched queue is
    /// preserved so a chained method can keep flowing without re-entering plan
    /// selection.
    ///
    /// Use `cancel()` instead when the entire plan chain is being aborted
    /// (player order, draft, goal flip, target despawn).
    pub fn advance(&mut self) {
        self.current = self.pop_next();
    }

    /// Drop every prefetched task without touching `current`. Used when the
    /// remaining queue is invalidated (target despawned, plan flipped) but the
    /// in-flight task should still finish on its own terms.
    pub fn clear_queued(&mut self) {
        self.queued = [Task::Idle; ACTION_QUEUE_CAP];
        self.queued_len = 0;
    }

    /// Cancel the current task *and* drop the prefetched queue. Use when a
    /// plan is aborted, the agent is preempted by a player order / draft, or
    /// a goal flip invalidates the entire chain. For ordinary
    /// task-completion transitions (current done → advance), keep using a
    /// plain `current = Task::Idle` so the prefetched queue can advance into
    /// `current` on the next tick.
    pub fn cancel(&mut self) {
        self.current = Task::Idle;
        self.clear_queued();
    }
}

impl Task {
    /// Convenience accessor for the WalkTo variant.
    pub fn as_walk_to(&self) -> Option<((i32, i32), i8, WalkReason)> {
        match *self {
            Task::WalkTo { tile, z, why } => Some((tile, z, why)),
            _ => None,
        }
    }

    /// Convenience accessor for the WithdrawGood variant.
    pub fn as_withdraw_good(&self) -> Option<WithdrawGoodFilter> {
        match *self {
            Task::WithdrawGood { filter } => Some(filter),
            _ => None,
        }
    }

    /// Convenience accessor for the WithdrawMaterial variant.
    pub fn as_withdraw_material(&self) -> Option<(ResourceId, u8)> {
        match *self {
            Task::WithdrawMaterial { resource_id, qty } => Some((resource_id, qty)),
            _ => None,
        }
    }

    /// Convenience accessor for the Equip variant.
    pub fn as_equip(&self) -> Option<(EquipmentSlot, ResourceId)> {
        match *self {
            Task::Equip { slot, resource_id } => Some((slot, resource_id)),
            _ => None,
        }
    }

    /// Convenience accessor for the Construct variant.
    pub fn as_construct(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::Construct { blueprint } => Some(blueprint),
            _ => None,
        }
    }

    /// Convenience accessor for the Gather variant.
    pub fn as_gather(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Gather { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the Dig variant.
    pub fn as_dig(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Dig { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the Scavenge variant.
    pub fn as_scavenge(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::Scavenge { target } => Some(target),
            _ => None,
        }
    }

    /// Returns the tech if the task is one of the four knowledge variants
    /// (Read, Teach, HoldLecture, AttendLecture). Lets shared code paths
    /// (inspector display, teardowns) read the tech without matching all
    /// four arms individually.
    pub fn knowledge_tech(&self) -> Option<TechId> {
        match *self {
            Task::Read { tech }
            | Task::Teach { tech }
            | Task::HoldLecture { tech }
            | Task::AttendLecture { tech } => Some(tech),
            _ => None,
        }
    }

    /// Convenience accessor for the PickUpCorpse variant.
    pub fn as_pickup_corpse(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::PickUpCorpse { corpse } => Some(corpse),
            _ => None,
        }
    }

    /// Convenience accessor for the Socialize variant. Returns the partner
    /// entity the agent should sit adjacent to.
    pub fn as_socialize(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::Socialize { partner } => Some(partner),
            _ => None,
        }
    }

    /// Convenience accessor for the Raid variant.
    pub fn as_raid(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Raid { dest } => Some(dest),
            _ => None,
        }
    }

    /// Convenience accessor for the Defend variant.
    pub fn as_defend(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Defend { dest } => Some(dest),
            _ => None,
        }
    }

    /// Convenience accessor for the Lead variant.
    pub fn as_lead(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Lead { dest } => Some(dest),
            _ => None,
        }
    }

    /// Convenience accessor for the RescueAlly variant. Returns
    /// `(attacker, dest)`.
    pub fn as_rescue_ally(&self) -> Option<(bevy::prelude::Entity, (i32, i32))> {
        match *self {
            Task::RescueAlly { attacker, dest } => Some((attacker, dest)),
            _ => None,
        }
    }

    /// Convenience accessor for the HuntPartyMuster variant.
    pub fn as_hunt_party_muster(&self) -> Option<(i32, i32)> {
        match *self {
            Task::HuntPartyMuster { hearth } => Some(hearth),
            _ => None,
        }
    }

    /// Convenience accessor for the Hunt variant.
    pub fn as_hunt(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::Hunt { prey } => Some(prey),
            _ => None,
        }
    }

    /// Convenience accessor for the HaulCorpse variant.
    pub fn as_haul_corpse(&self) -> Option<(i32, i32)> {
        match *self {
            Task::HaulCorpse { dest } => Some(dest),
            _ => None,
        }
    }

    /// True if this task is the in-place `Butcher` variant. Carries no
    /// parameters so a discriminant check is sufficient.
    pub fn is_butcher(&self) -> bool {
        matches!(*self, Task::Butcher)
    }

    /// Convenience accessor for the TameAnimal variant.
    pub fn as_planter(&self) -> Option<(i32, i32)> {
        match self {
            Task::Planter { tile } => Some(*tile),
            _ => None,
        }
    }

    pub fn as_tame_animal(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::TameAnimal { target } => Some(target),
            _ => None,
        }
    }

    /// Convenience accessor for the Sleep variant. Returns the claimed bed
    /// entity, or `None` for "sleep in place".
    pub fn as_sleep(&self) -> Option<Option<bevy::prelude::Entity>> {
        match *self {
            Task::Sleep { bed } => Some(bed),
            _ => None,
        }
    }

    /// True if this task is the in-place `Eat` variant. Eat carries no
    /// parameters so a discriminant check is sufficient.
    pub fn is_eat(&self) -> bool {
        matches!(*self, Task::Eat)
    }

    /// Convenience accessor for the WithdrawFood variant. Returns the
    /// faction-storage tile the agent should reach over to pick from.
    pub fn as_withdraw_food(&self) -> Option<(i32, i32)> {
        match *self {
            Task::WithdrawFood { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the HaulToBlueprint variant. Returns the
    /// blueprint entity the agent should deliver their hand contents to.
    pub fn as_haul_to_blueprint(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::HaulToBlueprint { blueprint } => Some(blueprint),
            _ => None,
        }
    }

    /// Convenience accessor for the HaulToCraftOrder variant. Returns the
    /// craft order entity the agent should deliver their hand contents to.
    pub fn as_haul_to_craft_order(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::HaulToCraftOrder { order } => Some(order),
            _ => None,
        }
    }

    /// Convenience accessor for the WorkOnCraftOrder variant. Returns the
    /// satisfied craft order entity the agent should labor at.
    pub fn as_work_on_craft_order(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::WorkOnCraftOrder { order } => Some(order),
            _ => None,
        }
    }

    /// Convenience accessor for the Play variant. Returns the partner entity
    /// for social play, or `None` for solo play.
    pub fn as_play(&self) -> Option<Option<bevy::prelude::Entity>> {
        match *self {
            Task::Play { partner } => Some(partner),
            _ => None,
        }
    }

    /// True if this task is the in-place `PlayThrow` variant. Carries no
    /// parameters so a discriminant check is sufficient.
    pub fn is_play_throw(&self) -> bool {
        matches!(*self, Task::PlayThrow)
    }

    /// Convenience accessor for the PlayPlant variant. Returns the destination
    /// grass tile the agent should plant on.
    pub fn as_play_plant(&self) -> Option<(i32, i32)> {
        match *self {
            Task::PlayPlant { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the DepositToFactionStorage variant. Returns
    /// the resource payload — the executor itself is parameterless (the legacy
    /// `TaskKind::DepositResource` path dumps whatever is in hand) but the
    /// typed task records what the chain produced for inspection.
    pub fn as_deposit_to_faction_storage(&self) -> Option<ResourceId> {
        match *self {
            Task::DepositToFactionStorage { resource_id } => Some(resource_id),
            _ => None,
        }
    }

    /// Convenience accessor for the Explore variant. Returns the
    /// `MemoryKind` the agent is exploring for so the chain handoff can
    /// verify integrity (e.g. "this Explore was supposed to find Wood, did
    /// the agent actually record a Wood sighting?").
    pub fn as_explore(&self) -> Option<MemoryKind> {
        match *self {
            Task::Explore { kind } => Some(kind),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dig(x: i32) -> Task {
        Task::Dig { tile: (x, 0) }
    }

    #[test]
    fn idle_default_has_empty_queue() {
        let aq = ActionQueue::idle();
        assert_eq!(aq.current, Task::Idle);
        assert!(aq.queued_is_empty());
        assert_eq!(aq.queued_len(), 0);
        assert_eq!(aq.peek_next(), None);
    }

    #[test]
    fn enqueue_then_pop_preserves_fifo_order() {
        let mut aq = ActionQueue::idle();
        assert!(aq.enqueue(dig(1)));
        assert!(aq.enqueue(dig(2)));
        assert!(aq.enqueue(dig(3)));
        assert_eq!(aq.queued_len(), 3);
        assert_eq!(aq.peek_next(), Some(dig(1)));
        assert_eq!(aq.pop_next(), dig(1));
        assert_eq!(aq.pop_next(), dig(2));
        assert_eq!(aq.pop_next(), dig(3));
        assert_eq!(aq.pop_next(), Task::Idle);
        assert!(aq.queued_is_empty());
    }

    #[test]
    fn enqueue_at_capacity_returns_false_and_does_not_overwrite() {
        let mut aq = ActionQueue::idle();
        for i in 0..ACTION_QUEUE_CAP {
            assert!(aq.enqueue(dig(i as i32)));
        }
        assert!(aq.queued_is_full());
        // Over-cap push must not silently displace older entries.
        assert!(!aq.enqueue(dig(99)));
        assert_eq!(aq.peek_next(), Some(dig(0)));
        assert_eq!(aq.queued_len(), ACTION_QUEUE_CAP);
    }

    #[test]
    fn cancel_resets_both_current_and_queue() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(1);
        aq.enqueue(dig(2));
        aq.enqueue(dig(3));
        aq.cancel();
        assert_eq!(aq.current, Task::Idle);
        assert!(aq.queued_is_empty());
    }

    #[test]
    fn clear_queued_keeps_current() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(1);
        aq.enqueue(dig(2));
        aq.clear_queued();
        assert_eq!(aq.current, dig(1));
        assert!(aq.queued_is_empty());
    }

    #[test]
    fn dispatch_to_idle_promotes_immediately() {
        let mut aq = ActionQueue::idle();
        assert!(aq.dispatch(dig(1)));
        assert_eq!(aq.current, dig(1));
        assert!(aq.queued_is_empty());
    }

    #[test]
    fn dispatch_while_busy_queues_behind_current() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(1);
        assert!(aq.dispatch(dig(2)));
        assert!(aq.dispatch(dig(3)));
        assert_eq!(aq.current, dig(1));
        assert_eq!(aq.queued_len(), 2);
        assert_eq!(aq.peek_next(), Some(dig(2)));
    }

    #[test]
    fn advance_promotes_queued_into_current() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(1);
        assert!(aq.enqueue(dig(2)));
        assert!(aq.enqueue(dig(3)));
        aq.advance();
        assert_eq!(aq.current, dig(2));
        assert_eq!(aq.queued_len(), 1);
        aq.advance();
        assert_eq!(aq.current, dig(3));
        assert!(aq.queued_is_empty());
        aq.advance();
        assert_eq!(aq.current, Task::Idle);
    }

    #[test]
    fn dispatch_at_capacity_returns_false() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(0);
        for i in 0..ACTION_QUEUE_CAP {
            assert!(aq.dispatch(dig(i as i32 + 1)));
        }
        assert!(aq.queued_is_full());
        assert!(!aq.dispatch(dig(99)));
        assert_eq!(aq.queued_len(), ACTION_QUEUE_CAP);
    }

    // Phase 5e-vii: typed-task scaffolding for the HuntFood chain. Variants
    // are produced by `plan_execution_system` in parallel with the legacy
    // `task_id` channel and consumed by future HTN dispatchers. Today they're
    // informational; the accessor tests pin the discriminant + payload shapes
    // so a future migration can refactor the executor reads without losing
    // chain-integrity inspection.
    #[test]
    fn hunt_party_muster_accessor_returns_hearth_tile() {
        let task = Task::HuntPartyMuster { hearth: (3, -7) };
        assert_eq!(task.as_hunt_party_muster(), Some((3, -7)));
        assert_eq!(Task::Idle.as_hunt_party_muster(), None);
    }

    #[test]
    fn hunt_accessor_returns_prey_entity() {
        let prey = bevy::prelude::Entity::from_raw(42);
        let task = Task::Hunt { prey };
        assert_eq!(task.as_hunt(), Some(prey));
        assert_eq!(Task::Idle.as_hunt(), None);
    }

    #[test]
    fn haul_corpse_accessor_returns_dest_tile() {
        let task = Task::HaulCorpse { dest: (12, 4) };
        assert_eq!(task.as_haul_corpse(), Some((12, 4)));
        assert_eq!(Task::Idle.as_haul_corpse(), None);
    }

    #[test]
    fn butcher_is_butcher_discriminates() {
        assert!(Task::Butcher.is_butcher());
        assert!(!Task::Idle.is_butcher());
        assert!(!dig(1).is_butcher());
    }

    #[test]
    fn socialize_accessor_returns_partner_entity() {
        let partner = bevy::prelude::Entity::from_raw(17);
        let task = Task::Socialize { partner };
        assert_eq!(task.as_socialize(), Some(partner));
        assert_eq!(Task::Idle.as_socialize(), None);
    }

    #[test]
    fn raid_defend_lead_accessors_return_dest_tile() {
        assert_eq!(Task::Raid { dest: (5, -3) }.as_raid(), Some((5, -3)));
        assert_eq!(Task::Defend { dest: (0, 0) }.as_defend(), Some((0, 0)));
        assert_eq!(Task::Lead { dest: (-2, 4) }.as_lead(), Some((-2, 4)));
        assert_eq!(Task::Idle.as_raid(), None);
        assert_eq!(Task::Idle.as_defend(), None);
        assert_eq!(Task::Idle.as_lead(), None);
    }

    #[test]
    fn rescue_ally_accessor_returns_attacker_and_dest() {
        let attacker = bevy::prelude::Entity::from_raw(99);
        let task = Task::RescueAlly {
            attacker,
            dest: (7, 8),
        };
        assert_eq!(task.as_rescue_ally(), Some((attacker, (7, 8))));
        assert_eq!(Task::Idle.as_rescue_ally(), None);
    }
}
