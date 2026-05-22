//! Typed Task variants — `ActionQueue.current` is canonical "what task is
//! running right now" for every agent.
//!
//! - **Producer.** HTN dispatchers, player-order handlers in `ui/orders.rs`
//!   and `player_command::dispatch_one`, the Read player-order handler in
//!   `teaching.rs`, and the legacy-only producers (`building_upgrade_system`,
//!   `terraform_dispatch_system`, `military_task_system` reroute) all route
//!   through `ActionQueue::dispatch(task)`. `dispatch` enqueues the task and
//!   immediately promotes it into `current` if `current` is `Idle`.
//! - **Consumer.** Executors call `aq.finish_task(&mut ai)` on success
//!   (Idle + work_progress=0 + advance) and `aq.cancel_chain(&mut ai)` on
//!   chain abort (same fields + cancel). Both leave the typed channel as
//!   the sole signal of intent.
//! - **External preempts.** Player commands, hunter demote, stale reset, and
//!   goal-flip preempts call `aq.cancel()`, dropping both `current` and the
//!   queue so a chained follow-up doesn't outlive its plan.
//!
//! Per-tick "pin" sites that re-assert the current task while an activity
//! component is alive (lecture/teach pin in `teaching.rs`) deliberately stay
//! as direct `current = X` writes — they're idempotent re-assertions of the
//! state, not fresh dispatches, and routing them through `dispatch()` would
//! pile duplicates onto the queue every tick.
//!
//! `PersonAI.task_id` is gone (Phase 4 step 3). Sites that previously read
//! the legacy `u16` discriminant now read `aq.current_task_kind()`, which
//! derives the same `TaskKind` projection from the typed channel via
//! `task_kind_for(...)`. `UNEMPLOYED_TASK_KIND` is the sentinel returned for
//! `Task::Idle`.

use bevy::prelude::Component;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::EquipmentSlot;
use crate::simulation::memory::MemoryKind;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::tasks::TaskKind;
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
    /// Nomadic-band member walking with the caravan to the final camp
    /// target. Route-waypoint arrival clears only `MigrationTarget.route_tile`;
    /// final arrival removes the marker.
    Migration,
    /// Heal-3: patient walking to the nearest same-faction Healer to
    /// receive care, or Healer walking to a patient.
    SeekCare,
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
    WithdrawGood { filter: WithdrawGoodFilter },
    /// Withdraw `qty` units of `resource_id` for a faction-blueprint /
    /// craft-order / haul-claim need. Replaces
    /// `PersonAI.{withdraw_good, withdraw_qty}`. The reservation against the
    /// storage tile still lives on the legacy `reserved_*` fields because
    /// every cleanup path goes through `release_reservation` — Phase 3
    /// collapses that into a `Drop` guard once the loose-target fields are
    /// fully retired.
    WithdrawMaterial { resource_id: ResourceId, qty: u8 },
    /// Market-procurement counterpart of `WithdrawMaterial` (Step 5). The
    /// agent walks to the faction's market `node` (Settlement/Camp entity)
    /// and buys `qty` of `resource_id` with treasury-funded escrow capital,
    /// then the chain continues into the shared `HaulToBlueprint` deposit
    /// leg. The price ceiling lives on the claim's `ClaimTarget.haul_source`
    /// (kept off `Task` so `Task` stays `Eq` — no `f32` field). Produced by
    /// the Market-haul direct-dispatch branch in
    /// `htn_acquire_good_dispatch_system`; the executor is the exclusive
    /// `production::buy_material_task_system`.
    BuyMaterialAtMarket {
        resource_id: ResourceId,
        qty: u8,
        node: bevy::prelude::Entity,
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
    Construct { blueprint: bevy::prelude::Entity },
    /// Harvest from a tile: plant fruits/wood, surface stone, or ore.
    /// `gather_system` inspects the tile contents to decide; the typed task
    /// just owns the *which tile*. Replaces `dest_tile` reads in the gather
    /// executor — the legacy field stays populated because `movement_system`
    /// still drives routing off of it.
    Gather { tile: (i32, i32) },
    /// Dig down at a tile, lowering its floor by one Z and producing the
    /// carved material as drops. Same shape as `Gather` — `dig_system` reads
    /// the tile from this variant; legacy `dest_tile` is kept for routing.
    Dig { tile: (i32, i32) },
    /// Pick up a specific `GroundItem` entity. Replaces the `TargetItem.0`
    /// read in `item_pickup_system` for the Scavenge branch — the typed
    /// channel is now the source of truth, falling back to `TargetItem.0`
    /// only for legacy player-dispatch sites.
    Scavenge { target: bevy::prelude::Entity },
    /// Solo study of a tablet/book in inventory. Replaces `tech_focus`
    /// (the loose `Option<TechId>` field) for the Read executor.
    Read { tech: TechId },
    /// 1-on-1 teaching: teacher stays adjacent to student and transfers
    /// progress on `tech`. The student-side state (`BeingTaught`) is a
    /// separate component; this variant only owns the teacher's params.
    Teach { tech: TechId },
    /// Stand at lecture anchor and broadcast progress to nearby attendees.
    HoldLecture { tech: TechId },
    /// Stand near a lecturer and accumulate study progress on `tech`.
    AttendLecture { tech: TechId },
    /// Walk adjacent to a `Corpse` entity and attach it to
    /// `PersonAI.carried_corpse`. Replaces `target_entity` reads in
    /// `pickup_corpse_task_system`. The downstream HaulCorpse + Butcher tasks
    /// also have typed variants for chain-integrity inspection — they read
    /// the corpse from the `Carrying` component (which spans pickup → haul →
    /// butcher) rather than per-task params, so the typed variants only
    /// document the phase.
    PickUpCorpse { corpse: bevy::prelude::Entity },
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
    Socialize { partner: bevy::prelude::Entity },
    /// Walk to the home tile of the faction this agent is raiding (per
    /// `FactionRegistry::raid_target`). On arrival the legacy executor's
    /// task entry, `task_requires_free_hands`, and `task_interacts_from_adjacent`
    /// govern the engagement; `combat_system` picks fights with any enemy
    /// faction member encountered along the way. Produced by HTN
    /// `RaidEnemyHomeMethod` (replaces legacy `Raid` plan PlanId 61 +
    /// StepId 49).
    Raid { dest: (i32, i32) },
    /// Walk to the agent's faction home tile and stand watch. No dedicated
    /// executor — the agent stays in `TaskKind::Defend` until
    /// `goal_update_system` flips them off `AgentGoal::Defend` (typically
    /// when the faction is no longer `is_under_raid`). Produced by HTN
    /// `DefendCampMethod` (replaces legacy `Defend` plan PlanId 62 +
    /// StepId 50).
    Defend { dest: (i32, i32) },
    /// Tribal chief in peacetime walks to the faction home tile and runs
    /// `TaskKind::Lead` — used by `chief_*` systems as a "chief is on duty"
    /// signal. No dedicated executor; the chief stays here until
    /// `goal_update_system` peels them off `AgentGoal::Lead` (crisis,
    /// hunger, sleep). Produced by HTN `LeadCampMethod` (replaces legacy
    /// `Lead` plan PlanId 63 + StepId 51).
    Lead { dest: (i32, i32) },
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
    HuntPartyMuster { hearth: (i32, i32) },
    /// Hunt down the named prey entity. There is no dedicated executor — the
    /// dispatcher routes the agent to the prey via
    /// `assign_task_with_routing(... TaskKind::Hunter, Some(prey) ...)` and
    /// sets `CombatTarget`, then `combat_system` engages the moment the
    /// agent is adjacent. The variant carries the prey entity for chain
    /// inspection (the future HTN `EngagePreyMethod` will read it; the
    /// legacy `HuntFood` plan's StepId(5) writes it for parity).
    Hunt { prey: bevy::prelude::Entity },
    /// Drag the carried corpse to the named butcher-site tile. The corpse
    /// itself follows via `corpse_follow_system` (no typed-task input). The
    /// `dest` tile is also written to legacy `dest_tile` for routing.
    /// Produced by the future HTN `HaulCorpseMethod` and by the legacy
    /// `HuntFood` plan's StepId(54).
    HaulCorpse { dest: (i32, i32) },
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
    TameAnimal { target: bevy::prelude::Entity },
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
    Planter { tile: (i32, i32) },
    /// Agent is tired and is either routing toward a bed / faction home or
    /// already asleep in place. The Sleep "executor" is a state transition
    /// (`AiState::Sleeping`) rather than a per-tick task system, so this
    /// variant is bookkeeping only today: it makes Sleep visible in the typed
    /// channel alongside every other task family and prepares the dispatcher
    /// for Phase 5a, where an HTN method will produce this variant directly.
    /// `bed = None` means "sleep in place" (solo agent, or at-home with no
    /// claimed bed yet).
    Sleep { bed: Option<bevy::prelude::Entity> },
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
    WithdrawFood { tile: (i32, i32) },
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
    HaulToBlueprint { blueprint: bevy::prelude::Entity },
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
    HaulToCraftOrder { order: bevy::prelude::Entity },
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
    PlayPlant { tile: (i32, i32) },
    /// Work adjacent to a satisfied `CraftOrder` until the recipe completes.
    /// Produced by `WorkOnSatisfiedCraftOrderMethod` (Phase 5e-xi-b — replaces
    /// the legacy `WorkOnCraft` plan, PlanId 16) as the head of a
    /// `[WorkOnCraftOrder, DepositToFactionStorage]` chain. The executor is
    /// `craft_order_system`'s worker branch, which already advances
    /// `work_progress` per tick once the order's deposits are full and pays
    /// out the recipe output to the lead worker on completion. The typed
    /// variant carries the order so dispatch / chain handoff has the right
    /// target without re-querying.
    WorkOnCraftOrder { order: bevy::prelude::Entity },
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
    /// Optional `target_faction_id` overrides the default routing (which uses
    /// the actor's own faction's storage map). Set to `Some(household_id)` for
    /// private farm harvest so crops land in the household's storage tile
    /// rather than village storage. `None` preserves legacy behaviour.
    DepositToFactionStorage {
        resource_id: ResourceId,
        target_faction_id: Option<u32>,
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
    Explore { kind: MemoryKind },
    /// Worker walks adjacent to a `ConstructionObstacle`-tagged entity in
    /// `blueprint`'s footprint, accumulates work_progress against its
    /// `WorkerClear { work_ticks, .. }` resolution, then despawns it
    /// (dropping any yields on the ground) and pops it from
    /// `Blueprint.pending_clear`. Distinct from `Gather` because the
    /// activity is a structure-prerequisite, not resource acquisition;
    /// loot drops on ground for haulers.
    ClearObstacle {
        entity: bevy::prelude::Entity,
        blueprint: bevy::prelude::Entity,
    },
    /// Part B: dismantle a Deployable nomadic structure. Worker walks
    /// adjacent to the `structure` tile, accumulates work_progress for
    /// `UNPITCH_WORK_TICKS`, then the executor despawns the entity and
    /// transfers the packed good into the worker's inventory / hands
    /// / a nearby pack animal / GroundItem (in that preference).
    UnpitchStructure { structure: bevy::prelude::Entity },
    /// Part B: drop `qty` units of `resource_id` from the worker's
    /// hands or inventory at `tile`. Used to pre-stage cargo at the
    /// new camp before pitching.
    UnloadCampCargo {
        resource_id: crate::economy::resource_catalog::ResourceId,
        qty: u8,
        tile: (i32, i32),
    },
    /// Part B: pitch a nomadic structure at `anchor`. Worker walks
    /// adjacent, the executor consumes the matching packed good
    /// (from inventory / hands / a co-located GroundItem) and spawns
    /// the structure.
    PitchStructureAt {
        kind: crate::simulation::construction::BuildSiteKind,
        anchor: (i32, i32),
    },
    /// Heal-3: Healer walks adjacent to `patient` and treats the
    /// patient's `Injury` over time. Decrements `Injury.severity`
    /// each tick while in range; despawns the `Injury` component
    /// when severity hits zero. Patient-side has no typed task —
    /// patients use `Task::WalkTo` to reach the Healer.
    Heal { patient: bevy::prelude::Entity },
    /// Thirst pipeline: drink one unit of water.
    ///
    /// `source = DrinkSource::Inventory` — agent consumes one `clean_water`
    /// from inventory/hands in-place.
    ///
    /// `source = DrinkSource::Tile { tile }` — agent stands adjacent to a
    /// fresh-water tile (`River` / `Marsh` / inland `Water`) and sips
    /// directly. Raw (non-River) sources roll a small sickness chance.
    /// Salt-water tiles never produce this variant — the dispatcher rejects
    /// them.
    Drink { source: DrinkSource },
    /// Build a Bed blueprint (kept distinct from `Construct` only for the
    /// reward-scaling / labor-tracking branch in `construction_system`).
    /// The executor reads the blueprint from this variant; the legacy
    /// `target_entity` is still populated by `assign_task_with_routing`'s
    /// caller for routing fall-back.
    ConstructBed { blueprint: bevy::prelude::Entity },
    /// Worker walks adjacent to a placed structure tile and dismantles it,
    /// then chains into `DepositResource` to carry refunds to storage.
    /// The destination tile lives on this variant; legacy `dest_tile` is
    /// kept for routing during `assign_task_with_routing`.
    Deconstruct { tile: (i32, i32) },
    /// Worker level-shifts the given footprint tile toward
    /// `TerraformSite.target_z` — one Z step per `TERRAFORM_WORK_TICKS`.
    /// Mirrors the shape of `Dig` / `Gather` — the variant only owns the
    /// destination tile; the `TerraformSite` entity is looked up out of
    /// `TerraformMap` by the executor.
    Terraform { tile: (i32, i32) },
    /// Drafted unit chases the named foe to attack adjacent. The dispatcher
    /// also writes `target_entity` for `combat_system` engagement.
    MilitaryAttack { foe: bevy::prelude::Entity },
    /// Seasonal farming Phase 1: worker turns an Agricultural plot tile into
    /// `TileKind::Cropland`. Executor (`farm::prepare_field_task_system`)
    /// accumulates `FIELD_PREP_WORK_TICKS` then stamps Cropland (preserving
    /// fertility), emits `TileChangedEvent`, increments the Farm posting's
    /// `JobProgress::FieldWork { phase: Prepare, completed, .. }`, grants
    /// Farming XP, and ensures `FieldTileIndex[tile].nutrients >= 30`.
    PrepareField { tile: (i32, i32) },
    /// Draftwork v2: worker plows one tile of an Agricultural plot, either
    /// with a draft animal (Cattle / Horse) — `animal: Some(e)` — or
    /// human-drawn — `animal: None`. Executor `draftwork::plow_task_system`
    /// accumulates `plow_work_ticks(animal)` ticks per tile (6 with an ox,
    /// 12 by hand), credits the posting's `plowed_tiles` counter, and on
    /// the final tile stamps `Plot.plowed_year`, releases the
    /// `AnimalWorkClaim` (only when `animal.is_some()`), and `finish_task`.
    /// Produced by `draftwork::htn_plow_dispatch_system`. The animal entity
    /// rides on the variant for chain-integrity inspection and so the
    /// executor knows which (if any) claim to drop.
    Plow {
        plot_entity: bevy::prelude::Entity,
        animal: Option<bevy::prelude::Entity>,
    },
    /// Vehicle system (Phase 4): a worker drives a `Vehicle` to ferry bulk
    /// `resource_id` from faction storage into a construction `blueprint`'s
    /// deposit slots. Re-dispatched per phase by
    /// `vehicle::htn_vehicle_haul_dispatch_system`: empty vehicle → routed to
    /// the source storage tile (load phase); loaded vehicle → routed to the
    /// blueprint (deliver phase). Executor `vehicle::vehicle_cargo_haul_task_system`
    /// releases the draft `AnimalWorkClaim`s and re-parks the vehicle once the
    /// haul completes. Draft animals live on the vehicle's `VehicleDraft`.
    VehicleCargoHaul {
        vehicle: bevy::prelude::Entity,
        blueprint: bevy::prelude::Entity,
        resource_id: ResourceId,
    },
    /// Fishing system: harvest `fish` from the `FishStock` of a water
    /// `spot_tile`. The worker stands on a passable chebyshev-adjacent
    /// tile (the routing layer picks it — interacts-from-adjacent), works
    /// `FISH_WORK_TICKS`, and `fish_task_system` deposits the catch into a
    /// free hand (overflow spills as a `GroundItem`). `output_resource` is
    /// always `core_ids::fish()`; carried on the variant so the trailing
    /// `Eat` / `DepositToFactionStorage` leg can assert chain integrity.
    /// Produced by `FishForImmediateFood` (→ `[Fish, Eat]`) and
    /// `FishForStorage` (→ `[Fish, DepositToFactionStorage]`).
    Fish {
        spot_tile: (i32, i32),
        method: crate::simulation::fishing::FishingMethod,
        output_resource: ResourceId,
    },
}

/// Source for a `Task::Drink`. Inventory drinks consume one `clean_water`
/// unit; tile drinks read the adjacency tile and gate on freshness via
/// `world::biome::water_kind_at`; well drinks read the adjacency tile against
/// `WellMap` and treat the result as clean unless `SanitationMap` flags it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrinkSource {
    Inventory,
    Tile { tile: (i32, i32) },
    Well { tile: (i32, i32) },
}

impl Default for Task {
    fn default() -> Self {
        Task::Idle
    }
}

/// Legacy sentinel returned by `task_kind_for(Task::Idle)` and historically by
/// `PersonAI.task_id` when the agent was not running any task. The typed
/// `ActionQueue::current` channel is now the source of truth for "what is the
/// agent doing right now"; this constant exists only because
/// `task_kind_for(...)` still projects the typed task back to the legacy
/// `TaskKind` discriminant for the handful of consumers
/// (`task_kind_label` / `task_requires_free_hands` / `task_is_labor` / etc.)
/// that read it via `ActionQueue::current_task_kind()`.
pub const UNEMPLOYED_TASK_KIND: u16 = u16::MAX;

/// Cap on the prefetched-task queue. Four slots is enough to hold the typed
/// task chains that today are spread across consecutive plan steps (e.g.
/// `WalkTo → WithdrawMaterial → WalkTo → DepositGoods`) without an allocation
/// in the hot path. If a method ever needs more, it should bump this constant
/// rather than spilling to a heap-backed Vec.
pub const ACTION_QUEUE_CAP: usize = 4;

/// Lifecycle metadata for autonomous dispatchers that install a concrete
/// task directly instead of going through an HTN `MethodId`.
///
/// The metadata lives beside `current`, so normal `finish_task` / `advance` /
/// `cancel` paths clear it with the task it describes. This keeps direct
/// job-driven dispatchers from needing a second, hand-maintained preserve-arm
/// in `goal_dispatch_system`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutonomousTaskLifecycle {
    pub owner_goal: AgentGoal,
    pub task_kind: TaskKind,
    pub job_id: Option<u32>,
    pub preserve_across_goal_dispatch: bool,
}

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
/// `ActionQueue::idle()`. The legacy `PersonAI.task_id` mirror is gone — the
/// typed channel is the sole source of truth.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionQueue {
    pub current: Task,
    queued: [Task; ACTION_QUEUE_CAP],
    queued_len: u8,
    autonomous_lifecycle: Option<AutonomousTaskLifecycle>,
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
            autonomous_lifecycle: None,
        }
    }

    /// Derived `TaskKind` discriminant of the running task. Returns
    /// `UNEMPLOYED_TASK_KIND` (`u16::MAX`) when idle. Replaces the legacy
    /// `PersonAI.task_id` field — the typed queue is the only source of truth.
    pub fn current_task_kind(&self) -> u16 {
        task_kind_for(self.current)
    }

    /// `true` when the queue is idle and nothing is prefetched. Used at every
    /// HTN dispatcher's preflight gate; replaces the legacy
    /// `ai.task_id == UNEMPLOYED_TASK_KIND && aq.current == Task::Idle` pair.
    pub fn is_idle(&self) -> bool {
        self.current == Task::Idle
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

    /// Metadata for the currently-running autonomous direct-dispatch task, if
    /// one owns `current`.
    pub fn autonomous_lifecycle(&self) -> Option<AutonomousTaskLifecycle> {
        self.autonomous_lifecycle
    }

    /// Stamp lifecycle metadata for the currently-running task. Callers should
    /// only use this immediately after a dispatch promoted into `current`.
    pub fn set_autonomous_lifecycle(&mut self, lifecycle: AutonomousTaskLifecycle) {
        self.autonomous_lifecycle = Some(lifecycle);
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
    /// `current` directly.
    ///
    /// Returns a `DispatchOutcome` distinguishing `Promoted` (current was Idle,
    /// task is now running), `Queued` (task is parked behind a running one),
    /// and `Rejected` (queue full — producer bug). The previous bool-returning
    /// shape buried `Queued` inside the `true` branch, so callers that expected
    /// the task to actually start had no signal when it silently parked behind
    /// a stale `current`. In debug builds, `Rejected` fires a
    /// `debug_assert!` — every production caller is expected to size its
    /// chains within `ACTION_QUEUE_CAP`.
    pub fn dispatch(&mut self, task: Task) -> DispatchOutcome {
        // Desync guard: the dispatcher believed the agent was idle (queue
        // empty), but `current` is still holding a task of the same kind.
        // This is the canonical "stale typed-channel" pattern — e.g. an
        // executor exit forgot to advance/cancel, or an external mutation
        // wrote `PersonAI.state = Idle` without touching `aq`. We don't
        // panic on the legit chain-prefetch case where the queue carries a
        // tail of the same kind (`WalkTo → Withdraw → WalkTo → Deposit`),
        // because that path always has `queued_len > 0` at the moment of
        // the second dispatch.
        debug_assert!(
            !(self.queued_len == 0
                && self.current != Task::Idle
                && task_kind_for(self.current) == task_kind_for(task)),
            "ActionQueue::dispatch desync — pushing {:?} while current is still {:?} and queue is empty",
            task,
            self.current,
        );
        if !self.enqueue(task) {
            debug_assert!(
                false,
                "ActionQueue::dispatch rejected — queue full (current={:?})",
                self.current
            );
            return DispatchOutcome::Rejected;
        }
        if self.current == Task::Idle {
            self.current = self.pop_next();
            DispatchOutcome::Promoted
        } else {
            DispatchOutcome::Queued
        }
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
        self.autonomous_lifecycle = None;
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
        self.autonomous_lifecycle = None;
        self.current = Task::Idle;
        self.clear_queued();
    }

    /// Canonical executor-success exit: reset `PersonAI.state` to `Idle`,
    /// zero `work_progress`, and `advance()` to the next prefetched task.
    pub fn finish_task(&mut self, ai: &mut PersonAI) {
        ai.state = AiState::Idle;
        ai.work_progress = 0;
        self.advance();
    }

    /// Canonical chain-abort exit: same field writes as `finish_task` but
    /// drops the prefetched queue too. Use when a target despawned, the plan
    /// was invalidated, or defence-in-depth code recovered from a desync.
    pub fn cancel_chain(&mut self, ai: &mut PersonAI) {
        ai.state = AiState::Idle;
        ai.work_progress = 0;
        self.cancel();
    }
}

/// Outcome of `ActionQueue::dispatch`. Replaces the prior `bool` return so
/// callers can distinguish "task is running now" (`Promoted`) from "task is
/// parked behind something" (`Queued`), and `Rejected` (queue full) is no
/// longer collapsed into the same `false` as legitimate failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// `current` was `Idle`; the new task was promoted into it and will run
    /// this tick. The canonical outcome for every HTN dispatcher's leading
    /// task and for player-order dispatch (which always cancels first).
    Promoted,
    /// `current` was already running; the task is parked in the prefetched
    /// queue and will promote when `advance()` runs. Legitimate for chain
    /// prefetch (`WalkTo → WithdrawMaterial → WalkTo → Deposit`).
    Queued,
    /// The queue was at capacity. The task was *not* enqueued. Producer bug —
    /// dispatchers must not try to push more than `ACTION_QUEUE_CAP` ahead of
    /// `current`. Fires `debug_assert!` inside `dispatch`.
    Rejected,
}

impl DispatchOutcome {
    /// `true` when the dispatched task is now `current` (i.e. running this
    /// tick, not parked).
    pub fn is_promoted(self) -> bool {
        matches!(self, DispatchOutcome::Promoted)
    }

    /// `true` when the queue accepted the task — `Promoted` or `Queued`.
    /// `false` only on `Rejected`. Use at sites that don't care whether the
    /// task ran this tick or the next.
    pub fn is_accepted(self) -> bool {
        !matches!(self, DispatchOutcome::Rejected)
    }
}

/// Map a typed `Task` variant back to the legacy `TaskKind` `u16`. Backs
/// `ActionQueue::current_task_kind()`, which is how consumers read the
/// current task family in terms of the (still-extant) `TaskKind` enum used
/// by `task_requires_free_hands` / `task_is_labor` / `task_interacts_from_adjacent`
/// / `task_kind_label` and the handful of executor-gate checks that still
/// compare against `TaskKind::X as u16`.
pub fn task_kind_for(task: Task) -> u16 {
    use TaskKind as TK;
    let kind = match task {
        Task::Idle => return UNEMPLOYED_TASK_KIND,
        Task::WalkTo { why, .. } => match why {
            WalkReason::MilitaryMove => TK::MilitaryMove,
            WalkReason::Gather => TK::Gather,
            WalkReason::Migration => TK::Migrate,
            WalkReason::SeekCare => TK::SeekCare,
        },
        Task::WithdrawGood { .. } => TK::WithdrawGood,
        Task::WithdrawMaterial { .. } => TK::WithdrawMaterial,
        Task::BuyMaterialAtMarket { .. } => TK::BuyMaterialAtMarket,
        Task::WalkAndTakeFromMember { .. } => TK::TakeFromMember,
        Task::Equip { .. } => TK::Equip,
        Task::Construct { .. } => TK::Construct,
        Task::Gather { .. } => TK::Gather,
        Task::Dig { .. } => TK::Dig,
        Task::Scavenge { .. } => TK::Scavenge,
        Task::Read { .. } => TK::Read,
        Task::Teach { .. } => TK::Teach,
        Task::HoldLecture { .. } => TK::HoldLecture,
        Task::AttendLecture { .. } => TK::AttendLecture,
        Task::PickUpCorpse { .. } => TK::PickUpCorpse,
        Task::Socialize { .. } => TK::Socialize,
        Task::Raid { .. } => TK::Raid,
        Task::Defend { .. } => TK::Defend,
        Task::Lead { .. } => TK::Lead,
        Task::RescueAlly { .. } => TK::Defend,
        Task::HuntPartyMuster { .. } => TK::HuntPartyMuster,
        Task::Hunt { .. } => TK::Hunter,
        Task::HaulCorpse { .. } => TK::HaulCorpse,
        Task::Butcher => TK::Butcher,
        Task::TameAnimal { .. } => TK::TameAnimal,
        Task::Planter { .. } => TK::Planter,
        Task::Sleep { .. } => TK::Sleep,
        Task::Eat => TK::Eat,
        Task::WithdrawFood { .. } => TK::WithdrawFood,
        Task::HaulToBlueprint { .. } => TK::HaulMaterials,
        Task::HaulToCraftOrder { .. } => TK::HaulToCraftOrder,
        Task::Play { .. } => TK::Play,
        Task::PlayThrow => TK::PlayThrow,
        Task::PlayPlant { .. } => TK::PlayPlant,
        Task::WorkOnCraftOrder { .. } => TK::WorkOnCraftOrder,
        Task::DepositToFactionStorage { .. } => TK::DepositResource,
        Task::Explore { .. } => TK::Explore,
        Task::ClearObstacle { .. } => TK::ClearObstacle,
        Task::UnpitchStructure { .. } => TK::UnpitchStructure,
        Task::UnloadCampCargo { .. } => TK::UnloadCampCargo,
        Task::PitchStructureAt { .. } => TK::PitchStructureAt,
        Task::Heal { .. } => TK::Heal,
        Task::Drink { .. } => TK::Drink,
        Task::ConstructBed { .. } => TK::ConstructBed,
        Task::Deconstruct { .. } => TK::Deconstruct,
        Task::Terraform { .. } => TK::Terraform,
        Task::MilitaryAttack { .. } => TK::MilitaryAttack,
        Task::PrepareField { .. } => TK::PrepareField,
        Task::Plow { .. } => TK::Plow,
        Task::VehicleCargoHaul { .. } => TK::VehicleCargoHaul,
        Task::Fish { .. } => TK::Fishing,
    };
    kind as u16
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

    /// Convenience accessor for the BuyMaterialAtMarket variant.
    pub fn as_buy_material_at_market(&self) -> Option<(ResourceId, u8, bevy::prelude::Entity)> {
        match *self {
            Task::BuyMaterialAtMarket {
                resource_id,
                qty,
                node,
            } => Some((resource_id, qty, node)),
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

    /// Convenience accessor for the ClearObstacle variant. Returns
    /// `(obstacle_entity, blueprint_entity)`.
    pub fn as_clear_obstacle(&self) -> Option<(bevy::prelude::Entity, bevy::prelude::Entity)> {
        match *self {
            Task::ClearObstacle { entity, blueprint } => Some((entity, blueprint)),
            _ => None,
        }
    }

    /// Part B: accessor for `UnpitchStructure`.
    pub fn as_unpitch_structure(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::UnpitchStructure { structure } => Some(structure),
            _ => None,
        }
    }

    /// Part B: accessor for `UnloadCampCargo`.
    pub fn as_unload_camp_cargo(
        &self,
    ) -> Option<(crate::economy::resource_catalog::ResourceId, u8, (i32, i32))> {
        match *self {
            Task::UnloadCampCargo {
                resource_id,
                qty,
                tile,
            } => Some((resource_id, qty, tile)),
            _ => None,
        }
    }

    /// Part B: accessor for `PitchStructureAt`.
    pub fn as_pitch_structure_at(
        &self,
    ) -> Option<(crate::simulation::construction::BuildSiteKind, (i32, i32))> {
        match *self {
            Task::PitchStructureAt { kind, anchor } => Some((kind, anchor)),
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

    /// Convenience accessor for the Drink variant.
    pub fn as_drink(&self) -> Option<DrinkSource> {
        match *self {
            Task::Drink { source } => Some(source),
            _ => None,
        }
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
            Task::DepositToFactionStorage { resource_id, .. } => Some(resource_id),
            _ => None,
        }
    }

    /// Like `as_deposit_to_faction_storage` but also returns the override
    /// faction id (private farm harvests route to the household sub-faction's
    /// storage tile when this is `Some`).
    pub fn as_deposit_to_faction_storage_full(&self) -> Option<(ResourceId, Option<u32>)> {
        match *self {
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id,
            } => Some((resource_id, target_faction_id)),
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

    /// Convenience accessor for the ConstructBed variant.
    pub fn as_construct_bed(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::ConstructBed { blueprint } => Some(blueprint),
            _ => None,
        }
    }

    /// Convenience accessor for the Deconstruct variant.
    pub fn as_deconstruct(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Deconstruct { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the Terraform variant.
    pub fn as_terraform(&self) -> Option<(i32, i32)> {
        match *self {
            Task::Terraform { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the MilitaryAttack variant.
    pub fn as_military_attack(&self) -> Option<bevy::prelude::Entity> {
        match *self {
            Task::MilitaryAttack { foe } => Some(foe),
            _ => None,
        }
    }

    /// Convenience accessor for the PrepareField variant.
    pub fn as_prepare_field(&self) -> Option<(i32, i32)> {
        match *self {
            Task::PrepareField { tile } => Some(tile),
            _ => None,
        }
    }

    /// Convenience accessor for the Plow variant. Returns
    /// `(plot_entity, animal)` where `animal` is `Some(e)` for ox-drawn
    /// plowing and `None` for human-drawn.
    pub fn as_plow(&self) -> Option<(bevy::prelude::Entity, Option<bevy::prelude::Entity>)> {
        match *self {
            Task::Plow {
                plot_entity,
                animal,
            } => Some((plot_entity, animal)),
            _ => None,
        }
    }

    /// Convenience accessor for the Fish variant. Returns
    /// `(spot_tile, method, output_resource)`.
    pub fn as_fish(
        &self,
    ) -> Option<((i32, i32), crate::simulation::fishing::FishingMethod, ResourceId)> {
        match *self {
            Task::Fish {
                spot_tile,
                method,
                output_resource,
            } => Some((spot_tile, method, output_resource)),
            _ => None,
        }
    }

    /// Convenience accessor for the VehicleCargoHaul variant. Returns
    /// `(vehicle, blueprint, resource_id)`.
    pub fn as_vehicle_cargo_haul(
        &self,
    ) -> Option<(
        bevy::prelude::Entity,
        bevy::prelude::Entity,
        ResourceId,
    )> {
        match *self {
            Task::VehicleCargoHaul {
                vehicle,
                blueprint,
                resource_id,
            } => Some((vehicle, blueprint, resource_id)),
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

    /// A non-`Dig` task used to seed `current` in tests that exercise
    /// `dispatch_while_busy` paths. The dispatch-desync `debug_assert!`
    /// fires when `current` and the incoming task share a `TaskKind` and the
    /// queue is empty — so synthetic "current is busy" setups must seed with
    /// a different kind, mirroring how real chains interleave variants.
    fn sleep_task() -> Task {
        Task::Sleep { bed: None }
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
        aq.set_autonomous_lifecycle(AutonomousTaskLifecycle {
            owner_goal: AgentGoal::Farm,
            task_kind: TaskKind::Dig,
            job_id: Some(7),
            preserve_across_goal_dispatch: true,
        });
        aq.cancel();
        assert_eq!(aq.current, Task::Idle);
        assert!(aq.queued_is_empty());
        assert_eq!(aq.autonomous_lifecycle(), None);
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
        assert_eq!(aq.dispatch(dig(1)), DispatchOutcome::Promoted);
        assert_eq!(aq.current, dig(1));
        assert!(aq.queued_is_empty());
    }

    #[test]
    fn dispatch_while_busy_queues_behind_current() {
        let mut aq = ActionQueue::idle();
        aq.current = sleep_task();
        assert_eq!(aq.dispatch(dig(2)), DispatchOutcome::Queued);
        assert_eq!(aq.dispatch(dig(3)), DispatchOutcome::Queued);
        assert_eq!(aq.current, sleep_task());
        assert_eq!(aq.queued_len(), 2);
        assert_eq!(aq.peek_next(), Some(dig(2)));
    }

    #[test]
    fn advance_promotes_queued_into_current() {
        let mut aq = ActionQueue::idle();
        aq.current = dig(1);
        assert!(aq.enqueue(dig(2)));
        assert!(aq.enqueue(dig(3)));
        aq.set_autonomous_lifecycle(AutonomousTaskLifecycle {
            owner_goal: AgentGoal::Farm,
            task_kind: TaskKind::Dig,
            job_id: Some(7),
            preserve_across_goal_dispatch: true,
        });
        aq.advance();
        assert_eq!(aq.current, dig(2));
        assert_eq!(aq.queued_len(), 1);
        assert_eq!(aq.autonomous_lifecycle(), None);
        aq.advance();
        assert_eq!(aq.current, dig(3));
        assert!(aq.queued_is_empty());
        aq.advance();
        assert_eq!(aq.current, Task::Idle);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "ActionQueue::dispatch rejected")]
    fn dispatch_at_capacity_panics_in_debug() {
        let mut aq = ActionQueue::idle();
        aq.current = sleep_task();
        for i in 0..ACTION_QUEUE_CAP {
            assert_eq!(aq.dispatch(dig(i as i32 + 1)), DispatchOutcome::Queued);
        }
        assert!(aq.queued_is_full());
        let _ = aq.dispatch(dig(99));
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn dispatch_at_capacity_returns_rejected_in_release() {
        let mut aq = ActionQueue::idle();
        aq.current = sleep_task();
        for i in 0..ACTION_QUEUE_CAP {
            assert_eq!(aq.dispatch(dig(i as i32 + 1)), DispatchOutcome::Queued);
        }
        assert!(aq.queued_is_full());
        assert_eq!(aq.dispatch(dig(99)), DispatchOutcome::Rejected);
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
