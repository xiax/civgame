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

use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::construction::{Bed, HomeBed};
use crate::simulation::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use crate::simulation::gather_claims::{suggested_expiry, GatherClaims};
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::MemoryKind;
use crate::simulation::needs::{Needs, EAT_TRIGGER_HUNGER};
use crate::simulation::person::{AiState, Drafted, PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::plants::{
    nearest_mature_plant_under_agent, GrowthStage, Plant, PlantKind, PlantMap,
};
use crate::simulation::production::total_edible;
use crate::simulation::schedule::SimClock;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::TechId;
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, TimePhase};
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
    AcquireGood {
        resource_id: ResourceId,
    },
    /// Fill faction food storage. The chief-driven counterpart to
    /// `AcquireFood`: instead of "agent is hungry, get food into mouth," this
    /// expresses "faction wants more food in storage, regardless of who is
    /// hungry." Methods under this kind chain pick-up tasks with a trailing
    /// `DepositToFactionStorage` (parallel to `AcquireFood`'s trailing `Eat`).
    /// Phase 5c-ii-d-vi adds two methods: `ScavengeFoodForStorageMethod` and
    /// `ExploreForFoodForStorageMethod` — the GatherFood-goal replacements
    /// for the legacy `ScavengeFood` (PlanId 6) and `ExploreForFood`
    /// (PlanId 35) plans, both now retired.
    StockpileFood,
    /// Walk somewhere new in the hope of recording a `MemoryKind::Prey`
    /// sighting. Chief-driven: only applicable when the agent's faction
    /// holds a `HuntOrder::Scout` (the chief flips to `Hunt` the moment any
    /// hunter records prey memory). One method (`ScoutForPreyMethod`) — the
    /// abstract task is parameterless because the memory kind is fixed.
    /// Replaces the legacy `ScoutForPrey` plan (PlanId 65).
    Scout,
    /// Hunter who is missing a Weapon (in inventory / hands / MainHand) walks
    /// to faction storage, withdraws one Spear, and equips it. Single
    /// expansion `[WithdrawMaterial { weapon, 1 }, Equip { MainHand, weapon }]`.
    /// `MF_UNINTERRUPTIBLE` so a hungry hunter mid-fetch doesn't peel off to
    /// eat before they're armed (mirrors the legacy plan's bias 5.0 +
    /// `PF_UNINTERRUPTIBLE`). Replaces the legacy `AcquireHuntingSpear` plan
    /// (PlanId 64).
    EquipHuntingSpear,
    /// Agent carrying surplus food walks to the nearest faction storage tile
    /// and dumps everything in hands + inventory. Single-method registry —
    /// `DepositSurplusAtStorageMethod` always wins. Replaces the legacy
    /// `ReturnSurplusFood` plan (PlanId 24).
    ReturnSurplus,
    /// Agent walks to the nearest visible wild Horse and works adjacent until
    /// it accepts a `Tamed` marker. Single-method registry —
    /// `TameWildAnimalMethod` always wins. Species candidate is resolved at
    /// dispatch time (Horse / Cow / Pig / Cat) against the faction's tech
    /// bitset (HORSE_TAMING / ANIMAL_HUSBANDRY / DOG_DOMESTICATION). The
    /// executor (`tame_task_system`) re-validates per-species at every tick.
    TameWildAnimal,
    /// Withdraw one of `resource_id` (a seed) from faction storage and plant it
    /// on the nearest unplanted Farmland tile. Single expansion
    /// `[WithdrawMaterial { seed, 1 }, Planter { tile }]`. `MF_UNINTERRUPTIBLE`
    /// so a hungry farmer mid-fetch doesn't peel off before the seed is in the
    /// ground. Replaces the legacy (and dead — never seeded into KnownPlans)
    /// `PlantFromStorage` (PlanId 4) and `PlantBerryFromStorage` (PlanId 66)
    /// plans, restoring chief-driven planting under the Farm goal.
    PlantFromStorage {
        resource_id: ResourceId,
    },
    /// Agent holding a `JobClaim::Build` walks to the claimed blueprint and
    /// labors at it. Single expansion `[Task::Construct { blueprint }]`.
    /// `MF_UNINTERRUPTIBLE` so a goal flip mid-walk doesn't drop the claim.
    /// Dispatcher gates on `bp.is_satisfied()` so the agent only commits when
    /// every deposit slot is full — until then the legacy build plans
    /// (`BuildBlueprint`, `HaulFromStorageAndBuild`) own the gather/haul work.
    /// Replaces the legacy `ClaimedBuild` plan (PlanId 34).
    ConstructBlueprint,
    /// Hunter carrying a fresh corpse hauls it to the nearest butcher site
    /// (campfire or faction home) and butchers it in place. Single expansion
    /// `[HaulCorpse { dest }, Butcher]`. `MF_UNINTERRUPTIBLE` so a hunger
    /// spike mid-haul doesn't peel the agent off — the corpse decays in
    /// `CORPSE_FRESHNESS_TICKS=600` and the carrier is the only one who can
    /// finish the job. Replaces the trailing two steps of the legacy
    /// `HuntFood` plan (PlanId 5): `[StepId(54) HaulCorpse, StepId(55) Butcher]`.
    /// The remaining four steps (Muster, Travel, Hunt, PickUp) still run
    /// through the legacy plan; once PickUp ends, the plan completes and the
    /// agent's `Carrying` component triggers this method on the next dispatch
    /// tick.
    DeliverHuntKill,
    /// Hunter at the chief's chosen hunt area engages prey or picks up a
    /// fresh kill. Two methods compete via argmax: `HuntPreyMethod` fires
    /// when a live prey entity is in vision (or memory) and emits
    /// `[Task::Hunt { prey }]`; `PickUpFreshCorpseMethod` fires when a fresh
    /// corpse is nearby and emits `[Task::PickUpCorpse { corpse }]`. The
    /// dispatcher re-fires between phases — there's no chain handoff because
    /// each method emits a single task, and the world-state transition (prey
    /// alive → prey dead → corpse) drives method selection. Replaces the
    /// middle two steps of the legacy `HuntFood` plan (PlanId 5):
    /// `[StepId(5) Hunt, StepId(53) PickUpCorpse]`. The remaining two steps
    /// (Muster, Travel) still run through the legacy plan; once Travel ends,
    /// the plan completes and this dispatcher takes over.
    EngagePrey,
    /// Hunter joining the chief's hunt party — first walks to the muster
    /// hearth and waits for the party to fill (`MusterAtHearthMethod`), then
    /// travels to the chief's chosen `area_tile` (`TravelToHuntAreaMethod`).
    /// Two methods gated on the `HuntOrder::Hunt`'s `deployed_tick` state:
    /// muster fires while the party hasn't deployed (and isn't stale),
    /// travel fires once deployed or stale. Replaces the leading two steps
    /// of the legacy `HuntFood` plan (PlanId 5): `[StepId(57)
    /// HuntPartyMuster, StepId(58) TravelToHuntArea]`. Together with
    /// `EngagePrey` and `DeliverHuntKill` this retires PlanId 5 entirely —
    /// the full hunting pipeline runs through HTN.
    JoinHuntParty,
    /// Agent under `AgentGoal::Socialize` walks to the nearest other Person
    /// and converses. Single expansion `[Task::Socialize { partner }]`.
    /// Single-method registry — `SocializeWithPartnerMethod` always wins.
    /// Replaces the legacy `Socialize` plan (PlanId 60) and its single step
    /// (StepId 48 NearestPlayPartner). Not `MF_UNINTERRUPTIBLE`: a sudden
    /// hunger spike or external preempt should be free to take precedence.
    Socialize,
    /// Agent under `AgentGoal::Raid` walks to the home tile of their
    /// faction's `raid_target` faction. Single-method registry —
    /// `RaidEnemyHomeMethod` always wins. Replaces legacy `Raid` plan
    /// PlanId 61 + StepId 49 (`StepTarget::FactionRaidTarget`).
    Raid,
    /// Agent under `AgentGoal::Defend` walks to faction home and stands
    /// watch. Single-method registry — `DefendCampMethod` always wins.
    /// Replaces legacy `Defend` plan PlanId 62 + StepId 50 (FactionCamp).
    Defend,
    /// Tribal chief under `AgentGoal::Lead` walks to faction home.
    /// Single-method registry — `LeadCampMethod` always wins. Replaces
    /// legacy `Lead` plan PlanId 63 + StepId 51 (FactionCamp).
    Lead,
    /// Distress responder under `AgentGoal::Rescue` engages the attacker
    /// stored on their `RescueTarget` component. Single expansion
    /// `[Task::RescueAlly { attacker, dest }]`. The dispatcher writes
    /// `CombatTarget(Some(attacker))` so `combat_system` engages on
    /// adjacency. Replaces legacy `RescueAlly` plan PlanId 23 + StepId 27
    /// (`StepTarget::RescueAttacker`).
    RescueAlly,
    /// Worker under `AgentGoal::Craft` walks to faction storage, withdraws one
    /// unit of `resource_id`, then carries it to a faction `CraftOrder` whose
    /// deposit slots still need that resource. Single expansion
    /// `[WithdrawMaterial { resource_id, qty: 1 }, HaulToCraftOrder { order }]`.
    /// `MF_UNINTERRUPTIBLE` so a goal flip mid-fetch doesn't strand the
    /// reservation — mirrors the legacy plan's `PF_UNINTERRUPTIBLE`. Replaces
    /// the legacy `DeliverFromStorageToCraftOrder` plan (PlanId 15) +
    /// `[StepId(40) FetchCraftOrderMaterialFromStorage, StepId(38)
    /// HaulToCraftOrder]`.
    DeliverMaterialToCraftOrder {
        resource_id: ResourceId,
    },
    /// Worker under `AgentGoal::Craft` walks adjacent to a satisfied faction
    /// `CraftOrder` and labors at it until `craft_order_system` produces the
    /// output and despawns the order. Single expansion
    /// `[Task::WorkOnCraftOrder { order }, Task::DepositToFactionStorage
    /// { resource_id: output }]`. `MF_UNINTERRUPTIBLE` so a goal flip mid-
    /// labor doesn't drop the worker (mirrors the legacy plan's
    /// `PF_UNINTERRUPTIBLE`). Replaces the legacy `WorkOnCraft` plan
    /// (PlanId 16) + `[StepId(39) WorkOnCraftOrder, StepId(12) DepositGoods]`.
    WorkOnCraftOrder,
    /// Worker under `AgentGoal::Craft` harvests a mature grain plant (in
    /// memory) and hauls the harvested grain to a faction `CraftOrder` whose
    /// deposits still need it. Single expansion `[Task::Gather { tile },
    /// Task::HaulToCraftOrder { order }]`. `MF_UNINTERRUPTIBLE` so a goal flip
    /// mid-harvest doesn't drop the chain (mirrors the legacy plan's
    /// `PF_UNINTERRUPTIBLE`). Replaces the legacy `DeliverGrainToCraftOrder`
    /// plan (PlanId 14) + `[StepId(1) FarmFood, StepId(38) HaulToCraftOrder]`.
    HarvestGrainForCraftOrder,
    /// Farmer under `AgentGoal::Farm` harvests a remembered mature edible
    /// plant (Grain / BerryBush) and deposits the harvest at faction storage.
    /// Single expansion `[Task::Gather { tile },
    /// Task::DepositToFactionStorage { resource_id, target_faction_id: None }]`.
    /// Replaces the legacy
    /// `FarmFood` plan (PlanId 1) + `[StepId(1) FarmFarmland, StepId(12)
    /// DepositGoods]`. The companion `htn_plant_from_storage_dispatch_system`
    /// owns the planting half of `AgentGoal::Farm`; together they retire the
    /// last plan-driven flow.
    HarvestPlant,
    /// Agent under `AgentGoal::Play` recreates — either with a nearby Person
    /// (`PlayWithPartnerMethod`, social play) or solo with an entertainment-
    /// valued item held or adjacent (`PlaySoloMethod`). Single expansion
    /// `[Task::Play { partner }]` from each method; `play_system` reads the
    /// partner from `ai.target_entity` (set by routing) and accumulates
    /// willpower until `PLAY_DURATION_TICKS` or `PLAY_FULL_WILLPOWER`. Not
    /// `MF_UNINTERRUPTIBLE` — Play is the lowest-priority need-driven activity
    /// and freely yields to hunger / sleep / external preempts. Replaces
    /// legacy `PlaySocial` plan PlanId 26 + `PlaySolo` plan PlanId 27.
    Play,
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
    StockpileFood,
    Scout,
    EquipHuntingSpear,
    ReturnSurplus,
    TameWildAnimal,
    PlantFromStorage,
    ConstructBlueprint,
    DeliverHuntKill,
    EngagePrey,
    JoinHuntParty,
    Socialize,
    Raid,
    Defend,
    Lead,
    RescueAlly,
    DeliverMaterialToCraftOrder,
    WorkOnCraftOrder,
    HarvestGrainForCraftOrder,
    HarvestPlant,
    Play,
}

impl AbstractTask {
    pub fn kind(self) -> AbstractTaskKind {
        match self {
            AbstractTask::Sleep => AbstractTaskKind::Sleep,
            AbstractTask::Eat => AbstractTaskKind::Eat,
            AbstractTask::AcquireFood => AbstractTaskKind::AcquireFood,
            AbstractTask::AcquireGood { .. } => AbstractTaskKind::AcquireGood,
            AbstractTask::StockpileFood => AbstractTaskKind::StockpileFood,
            AbstractTask::Scout => AbstractTaskKind::Scout,
            AbstractTask::EquipHuntingSpear => AbstractTaskKind::EquipHuntingSpear,
            AbstractTask::ReturnSurplus => AbstractTaskKind::ReturnSurplus,
            AbstractTask::TameWildAnimal => AbstractTaskKind::TameWildAnimal,
            AbstractTask::PlantFromStorage { .. } => AbstractTaskKind::PlantFromStorage,
            AbstractTask::ConstructBlueprint => AbstractTaskKind::ConstructBlueprint,
            AbstractTask::DeliverHuntKill => AbstractTaskKind::DeliverHuntKill,
            AbstractTask::EngagePrey => AbstractTaskKind::EngagePrey,
            AbstractTask::JoinHuntParty => AbstractTaskKind::JoinHuntParty,
            AbstractTask::Socialize => AbstractTaskKind::Socialize,
            AbstractTask::Raid => AbstractTaskKind::Raid,
            AbstractTask::Defend => AbstractTaskKind::Defend,
            AbstractTask::Lead => AbstractTaskKind::Lead,
            AbstractTask::RescueAlly => AbstractTaskKind::RescueAlly,
            AbstractTask::DeliverMaterialToCraftOrder { .. } => {
                AbstractTaskKind::DeliverMaterialToCraftOrder
            }
            AbstractTask::WorkOnCraftOrder => AbstractTaskKind::WorkOnCraftOrder,
            AbstractTask::HarvestGrainForCraftOrder => AbstractTaskKind::HarvestGrainForCraftOrder,
            AbstractTask::HarvestPlant => AbstractTaskKind::HarvestPlant,
            AbstractTask::Play => AbstractTaskKind::Play,
        }
    }
}

/// Per-method bitflags. Mirrors `PlanFlags` in `plan/mod.rs`. Empty for
/// 5a-i's lone Sleep method.
pub type MethodFlags = u8;
pub const MF_UNINTERRUPTIBLE: MethodFlags = 1 << 0;

/// Stable per-method identity. Mirrors `PlanId` in `plan/mod.rs` — newtype
/// over `u16` with one `pub const` per registered method. Method dispatchers
/// will use this to key per-agent recency-failure history (`MethodHistory`,
/// Phase 6a) so a method that just routing-failed scores lower next tick
/// than one that hasn't been tried.
#[derive(Copy, Clone, Eq, Hash, PartialEq, Debug)]
pub struct MethodId(pub u16);

impl MethodId {
    pub const SLEEP: MethodId = MethodId(0);
    pub const EAT_FROM_INVENTORY: MethodId = MethodId(1);
    pub const WITHDRAW_FROM_STORAGE: MethodId = MethodId(2);
    pub const SCAVENGE_FOOD_FROM_GROUND: MethodId = MethodId(3);
    pub const EXPLORE_FOR_FOOD: MethodId = MethodId(4);
    pub const WITHDRAW_MATERIAL_FROM_STORAGE: MethodId = MethodId(5);
    pub const WITHDRAW_AND_HAUL_TO_BLUEPRINT: MethodId = MethodId(6);
    pub const GATHER_FROM_KNOWN: MethodId = MethodId(7);
    pub const SCAVENGE_FROM_GROUND: MethodId = MethodId(8);
    pub const EXPLORE_FOR_MATERIAL: MethodId = MethodId(9);
    pub const SCAVENGE_FOOD_FOR_STORAGE: MethodId = MethodId(10);
    pub const EXPLORE_FOR_FOOD_FOR_STORAGE: MethodId = MethodId(11);
    pub const FORAGE_FROM_KNOWN: MethodId = MethodId(12);
    pub const FORAGE_FROM_KNOWN_FOR_STORAGE: MethodId = MethodId(13);
    pub const SCOUT_FOR_PREY: MethodId = MethodId(14);
    pub const WITHDRAW_AND_EQUIP_HUNTING_SPEAR: MethodId = MethodId(15);
    pub const DEPOSIT_SURPLUS_AT_STORAGE: MethodId = MethodId(16);
    pub const TAME_WILD_ANIMAL: MethodId = MethodId(17);
    pub const WITHDRAW_AND_PLANT_SEED: MethodId = MethodId(18);
    pub const BUILD_CLAIMED_BLUEPRINT: MethodId = MethodId(19);
    pub const DELIVER_HUNT_KILL: MethodId = MethodId(20);
    pub const HUNT_PREY: MethodId = MethodId(21);
    pub const PICK_UP_FRESH_CORPSE: MethodId = MethodId(22);
    pub const MUSTER_AT_HEARTH: MethodId = MethodId(23);
    pub const TRAVEL_TO_HUNT_AREA: MethodId = MethodId(24);
    pub const SOCIALIZE_WITH_PARTNER: MethodId = MethodId(25);
    pub const RAID_ENEMY_HOME: MethodId = MethodId(26);
    pub const DEFEND_CAMP: MethodId = MethodId(27);
    pub const LEAD_CAMP: MethodId = MethodId(28);
    pub const ENGAGE_RESCUE_ATTACKER: MethodId = MethodId(29);
    pub const WITHDRAW_AND_HAUL_TO_CRAFT_ORDER: MethodId = MethodId(30);
    pub const WORK_ON_SATISFIED_CRAFT_ORDER: MethodId = MethodId(31);
    pub const HARVEST_AND_HAUL_GRAIN_TO_CRAFT_ORDER: MethodId = MethodId(32);
    pub const PLAY_WITH_PARTNER: MethodId = MethodId(33);
    pub const PLAY_SOLO_WITH_ITEM: MethodId = MethodId(34);
    pub const WITHDRAW_AND_THROW_STONES_AS_PLAY: MethodId = MethodId(35);
    pub const WITHDRAW_AND_PLAY_WITH_TOY: MethodId = MethodId(36);
    pub const WITHDRAW_AND_PLANT_GRAIN_SEED_AS_PLAY: MethodId = MethodId(37);
    pub const WITHDRAW_AND_PLANT_BERRY_SEED_AS_PLAY: MethodId = MethodId(38);
    pub const WITHDRAW_AND_HAUL_TO_PERSONAL_BLUEPRINT: MethodId = MethodId(39);
    pub const GATHER_AND_HAUL_TO_PERSONAL_BLUEPRINT: MethodId = MethodId(40);
    pub const HARVEST_MATURE_PLANT_FOR_STORAGE: MethodId = MethodId(41);
    /// Synthetic id for terminal `Task::Explore` fallback (Phase 3) when no
    /// registered method routes. Pushed onto `MethodHistory` so repeated
    /// terminal-explore failures escalate to Phase 6B force-reevaluate.
    pub const TERMINAL_EXPLORE: MethodId = MethodId(42);
    /// Sentinel used when an executor cancels but `ai.active_method` was
    /// `None` (e.g. a chain leg that never stamped a method). `recently_failed_count`
    /// only matches concrete method ids, so UNKNOWN entries are harmless
    /// padding but they keep the cancel paths panic-free.
    pub const UNKNOWN: MethodId = MethodId(u16::MAX);

    pub fn raw(self) -> u16 {
        self.0
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::SLEEP => "Sleep",
            Self::EAT_FROM_INVENTORY => "EatFromInventory",
            Self::WITHDRAW_FROM_STORAGE => "WithdrawFromStorage",
            Self::SCAVENGE_FOOD_FROM_GROUND => "ScavengeFoodFromGround",
            Self::EXPLORE_FOR_FOOD => "ExploreForFood",
            Self::WITHDRAW_MATERIAL_FROM_STORAGE => "WithdrawMaterialFromStorage",
            Self::WITHDRAW_AND_HAUL_TO_BLUEPRINT => "WithdrawAndHaulToBlueprint",
            Self::GATHER_FROM_KNOWN => "GatherFromKnown",
            Self::SCAVENGE_FROM_GROUND => "ScavengeFromGround",
            Self::EXPLORE_FOR_MATERIAL => "ExploreForMaterial",
            Self::SCAVENGE_FOOD_FOR_STORAGE => "ScavengeFoodForStorage",
            Self::EXPLORE_FOR_FOOD_FOR_STORAGE => "ExploreForFoodForStorage",
            Self::FORAGE_FROM_KNOWN => "ForageFromKnown",
            Self::FORAGE_FROM_KNOWN_FOR_STORAGE => "ForageFromKnownForStorage",
            Self::SCOUT_FOR_PREY => "ScoutForPrey",
            Self::WITHDRAW_AND_EQUIP_HUNTING_SPEAR => "WithdrawAndEquipHuntingSpear",
            Self::DEPOSIT_SURPLUS_AT_STORAGE => "DepositSurplusAtStorage",
            Self::TAME_WILD_ANIMAL => "TameWildAnimal",
            Self::WITHDRAW_AND_PLANT_SEED => "WithdrawAndPlantSeed",
            Self::BUILD_CLAIMED_BLUEPRINT => "BuildClaimedBlueprint",
            Self::DELIVER_HUNT_KILL => "DeliverHuntKill",
            Self::HUNT_PREY => "HuntPrey",
            Self::PICK_UP_FRESH_CORPSE => "PickUpFreshCorpse",
            Self::MUSTER_AT_HEARTH => "MusterAtHearth",
            Self::TRAVEL_TO_HUNT_AREA => "TravelToHuntArea",
            Self::SOCIALIZE_WITH_PARTNER => "SocializeWithPartner",
            Self::RAID_ENEMY_HOME => "RaidEnemyHome",
            Self::DEFEND_CAMP => "DefendCamp",
            Self::LEAD_CAMP => "LeadCamp",
            Self::ENGAGE_RESCUE_ATTACKER => "EngageRescueAttacker",
            Self::WITHDRAW_AND_HAUL_TO_CRAFT_ORDER => "WithdrawAndHaulToCraftOrder",
            Self::WORK_ON_SATISFIED_CRAFT_ORDER => "WorkOnSatisfiedCraftOrder",
            Self::HARVEST_AND_HAUL_GRAIN_TO_CRAFT_ORDER => "HarvestAndHaulGrainToCraftOrder",
            Self::PLAY_WITH_PARTNER => "PlayWithPartner",
            Self::PLAY_SOLO_WITH_ITEM => "PlaySoloWithItem",
            Self::WITHDRAW_AND_THROW_STONES_AS_PLAY => "WithdrawAndThrowStonesAsPlay",
            Self::WITHDRAW_AND_PLAY_WITH_TOY => "WithdrawAndPlayWithToy",
            Self::WITHDRAW_AND_PLANT_GRAIN_SEED_AS_PLAY => "WithdrawAndPlantGrainSeedAsPlay",
            Self::WITHDRAW_AND_PLANT_BERRY_SEED_AS_PLAY => "WithdrawAndPlantBerrySeedAsPlay",
            Self::WITHDRAW_AND_HAUL_TO_PERSONAL_BLUEPRINT => "WithdrawAndHaulToPersonalBlueprint",
            Self::GATHER_AND_HAUL_TO_PERSONAL_BLUEPRINT => "GatherAndHaulToPersonalBlueprint",
            Self::HARVEST_MATURE_PLANT_FOR_STORAGE => "HarvestMaturePlantForStorage",
            Self::TERMINAL_EXPLORE => "TerminalExplore",
            Self::UNKNOWN => "Unknown",
            _ => "Unknown",
        }
    }
}

/// Outcome of a method expansion once it stops running. Pushed onto
/// `MethodHistory` so `utility()` can apply a soft recency penalty to a
/// method that just failed. Mirrors `PlanOutcome` in `plan/mod.rs`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MethodOutcome {
    Success,
    /// The dispatcher could not route the head task (no path / unreachable
    /// target / SOLO with no storage).
    FailedRouting,
    /// The executor ran but its target became invalid (despawned plant,
    /// reservation lost, etc.).
    FailedTarget,
    /// External preempt: chain dropped via `aq.cancel()` from a goal flip,
    /// muster, or distress response.
    Interrupted,
    /// Goal flipped before the method's chain completed and the method was
    /// *not* shielded by `MF_UNINTERRUPTIBLE`. Treated like `FailedTarget`
    /// for scoring (penalises the abandoned method) but tagged separately
    /// so the inspector can show why a method's bias accumulated. Without
    /// this, an agent who keeps switching goals leaves phantom Success-
    /// eligible state behind that fires on the next idle and never biases
    /// away from anything.
    Abandoned,
}

impl MethodOutcome {
    pub const fn is_failure(self) -> bool {
        !matches!(self, MethodOutcome::Success)
    }
}

/// Soft utility penalty applied per recent failure in `MethodHistory` when
/// the dispatcher scores a method.
///
/// Tuned 0.5 → 0.2 → 0.4: 0.2 + ring-of-2 was mathematically too weak to
/// overcome a 0.5 utility-tier gap (4 stacked failures needed but only 2
/// fit in the ring). 0.4 with a ring of 6 lets three failures comfortably
/// shift winner→runner-up while still leaving a once-failed method as a
/// candidate. Distance penalty cap (0.30) is preserved so a single failure
/// (-0.4) plus distance (-0.30 max) still keeps inter-tier ranking unless
/// the agent has bounced repeatedly.
pub const METHOD_FAILURE_PENALTY: f32 = 0.4;

// ── Method utility tiers (Phase 6c) ────────────────────────────────────
//
// Centralised so every `Method::utility()` body pulls its base from one
// place; tuning the inter-tier ranking touches one block instead of a
// dozen scattered literals.
//
// Tier semantics:
//
// - `UTIL_CLAIMED_HAUL` (2.0): the agent holds an active `JobClaim`
//   naming a specific blueprint+material. Outranks every opportunistic
//   alternative; survives goal flips via `MF_UNINTERRUPTIBLE`.
// - `UTIL_VISIBLE_GROUND` (1.5): a concrete loose `GroundItem` is
//   visible right now (or freshly remembered). Bias-on-visibility —
//   outranks the storage / known-gather baseline so a seen pile is
//   preferred over walking to a known tree or a known stockpile.
// - `UTIL_BASELINE` (1.0): the ordinary "do the obvious thing" tier
//   for sleep, eat, withdraw-from-storage, and gather-from-known.
// - `UTIL_EXPLORE_FALLBACK` (0.3): no concrete option fires; walk
//   somewhere new in the hope of recording a memory. Loses to any
//   concrete method whose precondition holds.
//
// Cap-preserving invariant (with `MAX_DIST_PENALTY = 0.30`):
//
//   CLAIMED_HAUL      − 0.30 = 1.70   (max-distance haul)
//   VISIBLE_GROUND    − 0.30 = 1.20   (max-distance scavenge)
//   BASELINE          − 0.00 = 1.00   (zero-distance baseline)
//   BASELINE          − 0.30 = 0.70   (max-distance baseline)
//   EXPLORE_FALLBACK         = 0.30
//
// Distance discounts can move a method *within* its tier but never
// across tiers — pinned by tests like
// `haul_full_trip_cap_preserves_ranking_over_bare_withdraw`.
pub const UTIL_BASELINE: f32 = 1.0;
pub const UTIL_VISIBLE_GROUND: f32 = 1.5;
pub const UTIL_CLAIMED_HAUL: f32 = 2.0;
pub const UTIL_EXPLORE_FALLBACK: f32 = 0.3;

/// Apply the recency-failure penalty to a method's raw utility. Centralised
/// so every dispatcher applies the same bias schedule and unit tests can
/// pin the contract. `now` is the current `SimClock.tick`.
pub fn score_method_with_history(
    method: &dyn Method,
    abstract_task: AbstractTask,
    ctx: &PlannerCtx,
    history: &MethodHistory,
    now: u64,
) -> f32 {
    let raw = method.utility(abstract_task, ctx);
    let failures = history.recently_failed_count(method.id(), now) as f32;
    raw - failures * METHOD_FAILURE_PENALTY
}

/// Phase E: disposition-aware variant of `score_method_with_history`.
/// Multiplies the method's `utility()` by `disposition_lift(...)` (default
/// 1.0 — no lift) before applying the failure penalty. Used by the
/// 3 migrated dispatchers (Socialize / Hunt / Play); the other ~21
/// stay on the legacy `score_method_with_history` until the
/// dispatch_for_goal consolidation lands (see
/// `civgame/plans/htn-dispatcher-consolidation.md`).
pub fn score_method_with_history_and_disposition(
    method: &dyn Method,
    abstract_task: AbstractTask,
    ctx: &PlannerCtx,
    disposition: crate::simulation::goal_scorers::Disposition,
    history: &MethodHistory,
    now: u64,
) -> f32 {
    let raw = method.utility(abstract_task, ctx) * method.disposition_lift(disposition);
    let failures = history.recently_failed_count(method.id(), now) as f32;
    raw - failures * METHOD_FAILURE_PENALTY
}

pub struct DispatchForGoalPick<'a> {
    pub method: &'a dyn Method,
    pub method_id: MethodId,
    pub score: f32,
}

/// Shared method argmax for goal-specific HTN wrappers. The wrapper still
/// owns context construction and task routing; this helper owns the common
/// "methods for abstract task → precondition → history-aware utility" shape.
pub fn dispatch_for_goal<'a>(
    method_registry: &'a MethodRegistry,
    abstract_task: AbstractTask,
    ctx: &PlannerCtx,
    history: &MethodHistory,
    now: u64,
    disposition: Option<crate::simulation::goal_scorers::Disposition>,
) -> Option<DispatchForGoalPick<'a>> {
    let mut best: Option<DispatchForGoalPick<'a>> = None;
    for method in method_registry.methods_for(abstract_task.kind()) {
        let method_ref = method.as_ref();
        if !method_ref.precondition(abstract_task, ctx) {
            continue;
        }
        let score = if let Some(disposition) = disposition {
            score_method_with_history_and_disposition(
                method_ref,
                abstract_task,
                ctx,
                disposition,
                history,
                now,
            )
        } else {
            score_method_with_history(method_ref, abstract_task, ctx, history, now)
        };
        // Match the legacy per-dispatcher `Iterator::max_by` behavior: when
        // utilities tie, the later registered method wins. Several Play
        // methods intentionally sit at UTIL_BASELINE and rely on registration
        // order for parity with the old plan ranking.
        if best.as_ref().map_or(true, |b| score >= b.score) {
            best = Some(DispatchForGoalPick {
                method: method_ref,
                method_id: method_ref.id(),
                score,
            });
        }
    }
    best
}

/// Pluralist Economy R4: check whether a method's `policy_gate` is
/// satisfied by the agent's effective faction. Returns `true` when:
///
/// - the method declares no gate (today: every existing method), or
/// - every gate entry's `RequiredFlag` is satisfied by the faction's
///   `policy_for(resource)`.
///
/// `faction_data` is `None` for SOLO / unsettled agents, in which case
/// only methods with empty gates pass — SOLO agents have no
/// policy table, so any policy-gated method is filtered out. R6+
/// dispatchers call this alongside `precondition` before scoring.
pub fn method_passes_policy_gate(
    method: &dyn Method,
    faction_data: Option<&crate::simulation::faction::FactionData>,
) -> bool {
    let gate = method.policy_gate();
    if gate.is_empty() {
        return true;
    }
    let Some(data) = faction_data else {
        // Method declares a non-empty gate; SOLO agent has no
        // policy table to satisfy it. Reject.
        return false;
    };
    gate.iter()
        .all(|(rid, flag)| data.policy_for(*rid).satisfies(*flag))
}

/// Per-agent failure ring. Tuned 2 → 6 entries / 100 → 600 ticks:
/// the prior ring overflowed after two parallel failure modes (e.g. food
/// pacing + wood pacing) and the 5-second TTL evaporated bias before the
/// agent re-considered the method on its next dispatch tick. Six slots cover
/// the typical concurrent-method count an agent juggles; 30 seconds keeps
/// the bias alive across a typical walk-and-arrive cycle.
pub const METHOD_HISTORY_LEN: usize = 6;
pub const METHOD_HISTORY_TTL_TICKS: u64 = 600;

/// Per-agent ring buffer of the last few method outcomes. Phase 6a writes the
/// component on every Person spawn site and exposes `recently_failed_count`;
/// Phase 6b instruments the chain-teardown sites that push outcomes here, and
/// extends `PlannerCtx` so method `utility()` bodies can read the count and
/// apply a soft penalty.
#[derive(Component, Default)]
pub struct MethodHistory {
    pub entries: [Option<(MethodId, MethodOutcome, u64)>; METHOD_HISTORY_LEN],
    pub head: u8,
}

impl MethodHistory {
    pub fn push(&mut self, method_id: MethodId, outcome: MethodOutcome, tick: u64) {
        let i = (self.head as usize) % METHOD_HISTORY_LEN;
        self.entries[i] = Some((method_id, outcome, tick));
        self.head = ((self.head as usize + 1) % METHOD_HISTORY_LEN) as u8;
    }

    /// Number of non-expired failure entries for `method_id`. Soft penalty
    /// only — the dispatcher still considers the method, just at lower
    /// utility.
    pub fn recently_failed_count(&self, method_id: MethodId, now: u64) -> u32 {
        self.entries
            .iter()
            .filter(|slot| {
                matches!(
                    slot,
                    Some((id, outcome, tick))
                        if *id == method_id
                            && outcome.is_failure()
                            && now.saturating_sub(*tick) <= METHOD_HISTORY_TTL_TICKS
                )
            })
            .count() as u32
    }
}

/// Record a target-loss failure on the agent's currently-active method and
/// clear `active_method`. Use this at executor cancel paths when a *target*
/// became invalid (despawned plant, snatched ground item, blueprint gone).
/// Caller is also responsible for any cluster `report_depleted` (when the
/// target was tied to a `SharedKnowledge` cluster) and for any
/// `release_reservation` / `release_gather_claim` on the cancelled chain leg.
///
/// Safe to call when `active_method` is `None` — pushes a `MethodId::UNKNOWN`
/// entry which `recently_failed_count` filters out (it only matches concrete
/// method ids), so the entry is harmless padding rather than a panic.
pub fn record_target_failure(
    method_history: &mut MethodHistory,
    ai: &mut crate::simulation::person::PersonAI,
    now: u64,
) {
    let mid = ai.active_method.take().unwrap_or(MethodId::UNKNOWN);
    method_history.push(mid, MethodOutcome::FailedTarget, now);
}

/// Same as `record_target_failure` but stamps `FailedRouting` instead — use
/// at chain-handoff sites where the trailing leg of a multi-step expansion
/// could not route (e.g. `finish_gather`'s DepositToFactionStorage handoff
/// finds no reachable storage tile). The first leg succeeded; the chain
/// dies because the second leg is unroutable, which is structurally a
/// routing failure, not a target failure.
pub fn record_routing_failure(
    method_history: &mut MethodHistory,
    ai: &mut crate::simulation::person::PersonAI,
    now: u64,
) {
    let mid = ai.active_method.take().unwrap_or(MethodId::UNKNOWN);
    method_history.push(mid, MethodOutcome::FailedRouting, now);
}

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

/// Hard penalty cap when scoring under `ScoringScope::ContextAware` at night.
/// Distinct from `MAX_DIST_PENALTY` so a hard-night penalty can drop a
/// gather method below the explore-fallback floor — terminal Explore picks
/// up, the agent quickly returns home, and Maslow-driven Sleep wins the next
/// goal flip naturally.
const MAX_DIST_PENALTY_NIGHT: f32 = 1.50;

/// Geometric distance penalty: `chebyshev × 0.02`, capped at
/// `MAX_DIST_PENALTY`. Used internally by `dist_penalty` /
/// `full_trip_penalty` and directly by tests.
fn dist_penalty_raw(agent: (i32, i32), target: Option<(i32, i32)>) -> f32 {
    match target {
        Some(t) => {
            let d = chebyshev_dist(agent, t) as f32;
            (d * DIST_DISCOUNT_PER_TILE).min(MAX_DIST_PENALTY)
        }
        None => 0.0,
    }
}

fn full_trip_penalty_raw(
    agent: (i32, i32),
    target: Option<(i32, i32)>,
    deposit: Option<(i32, i32)>,
) -> f32 {
    match (target, deposit) {
        (Some(t), Some(d)) => {
            let total = (chebyshev_dist(agent, t) + chebyshev_dist(t, d)) as f32;
            (total * DIST_DISCOUNT_PER_TILE).min(MAX_DIST_PENALTY)
        }
        _ => dist_penalty_raw(agent, target),
    }
}

/// Build a `ScoringScope::ContextAware` from a calendar snapshot and the
/// agent's `Needs`. Fatigue blends sleep need (0..255, weight 0.6) and
/// drained willpower (0..255 inverted, weight 0.4) so a sleep-deprived
/// worker treats every tile as more expensive even before willpower
/// crashes. Sleep weighted heavier because it's the harder cap — a
/// sleeping agent is uncontrollable.
pub fn context_aware_scope(calendar: &Calendar, needs: &Needs) -> ScoringScope {
    let sleep_norm = (needs.sleep / 255.0).clamp(0.0, 1.0);
    let willpower_drain_norm = ((255.0 - needs.willpower) / 255.0).clamp(0.0, 1.0);
    let fatigue = (sleep_norm * 0.6 + willpower_drain_norm * 0.4).clamp(0.0, 1.0);
    ScoringScope::ContextAware {
        time_phase: calendar.time_phase(),
        dusk_remaining: calendar.dusk_fraction_remaining(),
        fatigue,
    }
}

/// Multiplier on the geometric penalty driven by time-of-day and worker
/// fatigue. Returns `(time_mul, fatigue_mul, cap)`.
///
/// - Day:   `time_mul = 1.0`. Cap = `MAX_DIST_PENALTY`.
/// - Dawn:  `time_mul = 1.10` (slight cold-start bias to closer work).
/// - Dusk:  `time_mul = 1.0..2.0` ramp on `dusk_remaining` (1 → 0).
/// - Night: `time_mul = 4.0`, cap raised to `MAX_DIST_PENALTY_NIGHT` so
///          gather methods can drop below the 0.3 explore fallback at any
///          non-trivial distance.
/// - Fatigue scales linearly: `fatigue_mul = 1.0 + fatigue` (up to 2× at
///   `fatigue == 1.0`, doubling the effective penalty).
fn ctx_penalty_factors(scope: ScoringScope) -> (f32, f32, f32) {
    match scope {
        ScoringScope::Geometric => (1.0, 1.0, MAX_DIST_PENALTY),
        ScoringScope::ContextAware {
            time_phase,
            dusk_remaining,
            fatigue,
        } => {
            let fatigue_mul = 1.0 + fatigue.clamp(0.0, 1.0);
            let (time_mul, cap) = match time_phase {
                TimePhase::Day => (1.0, MAX_DIST_PENALTY),
                TimePhase::Dawn => (1.10, MAX_DIST_PENALTY),
                TimePhase::Dusk => {
                    let remaining = dusk_remaining.clamp(0.0, 1.0);
                    (1.0 + (1.0 - remaining), MAX_DIST_PENALTY)
                }
                TimePhase::Night => (4.0, MAX_DIST_PENALTY_NIGHT),
            };
            (time_mul, fatigue_mul, cap)
        }
    }
}

/// Compute the distance-weighted discount for a method whose target tile is
/// `target`. Returns 0 when `target.is_none()` so methods that haven't been
/// populated by the dispatcher (or unit tests with `ctx_empty()`) score at
/// their flat base utility.
///
/// When `ctx.scope == Geometric` (the default) this is the legacy
/// `chebyshev × 0.02` penalty capped at `MAX_DIST_PENALTY`. When
/// `ctx.scope == ContextAware` the penalty is multiplied by a time-of-day
/// factor and a fatigue factor, with the cap raised at night so distant
/// methods can lose to the explore fallback.
fn dist_penalty(ctx: &PlannerCtx, target: Option<(i32, i32)>) -> f32 {
    let Some(t) = target else { return 0.0 };
    let d = chebyshev_dist(ctx.tile, t) as f32;
    let (time_mul, fatigue_mul, cap) = ctx_penalty_factors(ctx.scope);
    (d * DIST_DISCOUNT_PER_TILE * time_mul * fatigue_mul).min(cap)
}

/// Two-leg distance discount for chains shaped agent → target → deposit
/// (gather/scavenge methods whose expansion ends in `DepositToFactionStorage`,
/// or the haul method's storage→blueprint pair). Total penalty caps at
/// `MAX_DIST_PENALTY` (or `MAX_DIST_PENALTY_NIGHT` under hard-night context).
/// Falls back to the agent→target single-leg signal when `deposit` is `None`.
fn full_trip_penalty(
    ctx: &PlannerCtx,
    target: Option<(i32, i32)>,
    deposit: Option<(i32, i32)>,
) -> f32 {
    match (target, deposit) {
        (Some(t), Some(d)) => {
            let total = (chebyshev_dist(ctx.tile, t) + chebyshev_dist(t, d)) as f32;
            let (time_mul, fatigue_mul, cap) = ctx_penalty_factors(ctx.scope);
            (total * DIST_DISCOUNT_PER_TILE * time_mul * fatigue_mul).min(cap)
        }
        _ => dist_penalty(ctx, target),
    }
}

/// Whether method `utility()` bodies should fold time-of-day and worker
/// fatigue into their distance penalty (`ContextAware`) or stick to the
/// simple geometric chebyshev penalty (`Geometric`).
///
/// Defaults to `Geometric` so dispatchers that haven't been migrated keep
/// their existing behaviour. The `htn_acquire_food_dispatch_system` and
/// the wood/stone branches of `htn_acquire_good_dispatch_system` set
/// `ContextAware` after sampling `Calendar` + the agent's `Needs`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ScoringScope {
    #[default]
    Geometric,
    ContextAware {
        time_phase: TimePhase,
        /// Within dusk, fraction of dusk daylight remaining (1.0 → 0.0).
        /// 1.0 outside dusk; consumed only when `time_phase == Dusk`.
        dusk_remaining: f32,
        /// Composite tiredness in `[0.0, 1.0]`. Higher = more tired.
        fatigue: f32,
    },
}

/// Snapshot of the agent + world state a `Method` needs to make a decision.
/// Constructed per-agent per-decision-tick by the (future) HTN dispatch
/// system; methods borrow it immutably.
///
/// Phase 5a-i populates only the fields the `SleepMethod` actually reads.
/// New fields land on demand as methods are added — no speculative coverage.
#[derive(Clone, Copy, Debug)]
pub struct PlannerCtx {
    /// Distance-penalty scoring scope. See `ScoringScope`.
    pub scope: ScoringScope,
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
    /// World tile of `claimed_blueprint`, snapshot at decision time. Read by
    /// `WithdrawAndHaulToBlueprintMethod`'s `utility()` to discount on the
    /// *full* storage→blueprint trip rather than just the storage hop. The
    /// haul dispatcher populates this from `Blueprint.tile` whenever
    /// `claimed_blueprint` is set; siblings leave both at `None`.
    pub claimed_blueprint_tile: Option<(i32, i32)>,
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
    /// Specific food good the picked-up `scavenge_target_entity` will yield
    /// (Fruit / Meat / Grain / etc.). Read by `ScavengeFoodForStorageMethod`
    /// (Phase 5c-ii-d-vi) so the trailing `Task::DepositToFactionStorage` can
    /// carry the right `good` payload. The legacy `ScavengeFood` plan (PlanId
    /// 6) didn't need this field because the deposit step was parameterless;
    /// the typed task makes the good explicit for chain-integrity inspection.
    /// `None` for non-food scavenge dispatches and dispatcher ctx-build sites
    /// that don't populate it.
    pub scavenge_food_good: Option<crate::economy::resource_catalog::ResourceId>,
    /// Nearest faction storage tile from `gather_target_tile`. Used by
    /// `GatherFromKnownMethod` to discount on the full gather→deposit trip
    /// (matches the haul method's full-trip discount from 5c-ii-d-vii).
    /// `None` when no gather target is set or the faction has no storage.
    pub gather_deposit_tile: Option<(i32, i32)>,
    /// Nearest faction storage tile from `scavenge_target_tile`. Used by the
    /// AcquireGood/StockpileFood scavenge methods (whose chains end in
    /// `DepositToFactionStorage`) for full-trip discount. `None` when no
    /// scavenge target is set, the faction has no storage, or the chain ends
    /// in `Eat` (AcquireFood case — no second hop to discount).
    pub scavenge_deposit_tile: Option<(i32, i32)>,
    /// Specific food good the picked-up plant at `gather_target_tile` will
    /// yield (Fruit / Grain / etc.). Read by `ForageFromKnownForStorageMethod`
    /// so the trailing `Task::DepositToFactionStorage` carries the right
    /// `good` payload. Mirrors `scavenge_food_good`'s role for the scavenge
    /// chain. The `Task::DepositToFactionStorage` payload is informational
    /// (the deposit executor dumps everything in hand regardless), but the
    /// typed task makes the good explicit for chain-integrity inspection.
    /// `None` for non-forage dispatches and AcquireFood (whose chain ends in
    /// `Eat`, not `DepositToFactionStorage`).
    pub forage_food_good: Option<crate::economy::resource_catalog::ResourceId>,
    /// Nearest butcher-site tile (campfire / faction home) for the agent's
    /// faction. Read by `DeliverHuntKillMethod` (Phase 5e-viii-a) to seed
    /// the head `Task::HaulCorpse { dest }` of the haul → butcher chain.
    /// Mirrors the legacy `StepTarget::NearestButcherSite` resolver. `None`
    /// when the faction has no campfires and no `home_tile`. Other dispatchers
    /// leave it at `None`.
    pub butcher_site_tile: Option<(i32, i32)>,
    /// Nearest live prey entity within vision (LOS-checked) or memory. Read
    /// by `HuntPreyMethod` (Phase 5e-viii-b). Mirrors the legacy
    /// `StepTarget::HuntPrey` resolver — vision first, memory fallback. `None`
    /// when no prey is reachable. Other dispatchers leave it at `None`.
    pub prey_target_entity: Option<Entity>,
    /// World tile of `prey_target_entity`, snapshot at decision time. Used
    /// by `HuntPreyMethod`'s utility for distance discount and by the
    /// dispatcher for `assign_task_with_routing` destination.
    pub prey_target_tile: Option<(i32, i32)>,
    /// Nearest fresh `Corpse` entity within `VIEW_RADIUS` of the agent. Read
    /// by `PickUpFreshCorpseMethod` (Phase 5e-viii-b). Mirrors the legacy
    /// `StepTarget::NearestFreshCorpse` resolver — direct `CorpseMap` scan,
    /// no LOS check. `None` when no fresh corpse is in range. Other
    /// dispatchers leave it at `None`.
    pub fresh_corpse_entity: Option<Entity>,
    /// World tile of `fresh_corpse_entity`. Used by
    /// `PickUpFreshCorpseMethod`'s utility for distance discount and by the
    /// dispatcher for routing.
    pub fresh_corpse_tile: Option<(i32, i32)>,
    /// Muster hearth tile for `JoinHuntParty` (Phase 5e-viii-c). Mirrors the
    /// legacy `StepTarget::HearthForHunt` resolver — nearest campfire to
    /// the chief's `area_tile` with `home_tile` fallback. Read by
    /// `MusterAtHearthMethod`. `None` if no campfires and no faction home.
    pub hunt_hearth_tile: Option<(i32, i32)>,
    /// Hunt area tile from the faction's `HuntOrder::Hunt`. Read by
    /// `TravelToHuntAreaMethod`. `None` outside the JoinHuntParty
    /// dispatcher's scope.
    pub hunt_area_tile: Option<(i32, i32)>,
    /// `true` when the faction's `HuntOrder::Hunt` has its `deployed_tick`
    /// set — the party has reached `target_party_size` and may travel.
    /// `MusterAtHearthMethod` requires this to be `false`;
    /// `TravelToHuntAreaMethod` accepts it `true` (or `hunt_party_stale`).
    pub hunt_party_deployed: bool,
    /// `true` when the faction's hunt order has been posted longer than
    /// `HUNT_PARTY_TIMEOUT` ticks without filling. Triggers travel even on
    /// an under-strength party — mirrors `wait_for_party_task_system`'s
    /// staleness exit.
    pub hunt_party_stale: bool,
    /// Open faction `CraftOrder` entity. Read by
    /// `WithdrawAndHaulToCraftOrderMethod` (Phase 5e-xi-a) for the
    /// haul-to-order chain (precondition: deposits unmet) and by
    /// `WorkOnSatisfiedCraftOrderMethod` (Phase 5e-xi-b) for the work-on-order
    /// chain (precondition: deposits satisfied). The dispatcher picks the
    /// nearest applicable order at decision time. `None` for non-Craft
    /// dispatchers and ctx-build sites that don't populate it.
    pub target_craft_order: Option<Entity>,
    /// Output `ResourceId` of the recipe attached to `target_craft_order`,
    /// snapshot by `htn_work_on_craft_order_dispatch_system` so
    /// `WorkOnSatisfiedCraftOrderMethod`'s expansion can carry it on the
    /// trailing `Task::DepositToFactionStorage`. Other dispatchers leave it
    /// at `None`.
    pub craft_output_resource: Option<ResourceId>,
    /// Nearest other Person within play radius (12 tiles), filtered to
    /// exclude blueprints/items/animals. Read by `PlayWithPartnerMethod`
    /// (Phase 5e-xii-a). `None` if no partner is in range. Other dispatchers
    /// leave it at `None`.
    pub play_partner_entity: Option<Entity>,
    /// `true` when the agent has an entertainment-valued item in hand or
    /// adjacent on the ground. Read by `PlaySoloMethod` (Phase 5e-xii-a).
    pub play_solo_eligible: bool,
    /// Nearest faction storage tile holding at least one Stone. Read by
    /// `WithdrawAndThrowStonesAsPlayMethod` (Phase 5e-xii-b) to seed the
    /// `WithdrawMaterial` head of a `[WithdrawMaterial, PlayThrow]` chain.
    /// `None` for SOLO agents, factions without storage, or storage without
    /// stone (after `StorageReservations` deduction). Other dispatchers
    /// leave it at `None`.
    pub play_stone_storage_tile: Option<(i32, i32)>,
    /// Nearest faction storage tile holding at least one entertainment-valued
    /// resource (`entertainment_value() > 0`). Read by
    /// `WithdrawAndPlayWithToyMethod` (Phase 5e-xii-c) to seed the
    /// `WithdrawMaterial` head of a `[WithdrawMaterial, Play { None }]` chain.
    /// Pairs with `play_toy_resource` (the specific resource picked at
    /// decision time). `None` for SOLO agents, factions without storage, or
    /// storage without any entertainment-valued resource. Other dispatchers
    /// leave it at `None`.
    pub play_toy_storage_tile: Option<(i32, i32)>,
    /// Specific entertainment-valued resource picked from the storage tile
    /// snapshot. Read by `WithdrawAndPlayWithToyMethod` so the typed
    /// `WithdrawMaterial { resource_id }` head carries the right resource
    /// payload. `None` when no toy was found. Other dispatchers leave it at
    /// `None`.
    pub play_toy_resource: Option<ResourceId>,
    /// Nearest faction storage tile holding at least one Grain seed. Read by
    /// `WithdrawAndPlantGrainSeedAsPlayMethod` (Phase 5e-xii-d) to seed the
    /// `WithdrawMaterial` head of a `[WithdrawMaterial, PlayPlant { tile }]`
    /// chain. `None` when SOLO / no storage / no grain seeds.
    pub play_grain_seed_storage_tile: Option<(i32, i32)>,
    /// Nearest faction storage tile holding at least one Berry seed. Read by
    /// `WithdrawAndPlantBerrySeedAsPlayMethod` (Phase 5e-xii-d). `None` when
    /// SOLO / no storage / no berry seeds.
    pub play_berry_seed_storage_tile: Option<(i32, i32)>,
    /// Nearest unplanted Grass tile within `VIEW_RADIUS=15` of the agent.
    /// Read by `WithdrawAndPlantGrainSeedAsPlayMethod` /
    /// `WithdrawAndPlantBerrySeedAsPlayMethod` (Phase 5e-xii-d) as the
    /// destination of the trailing `Task::PlayPlant { tile }` leg. `None`
    /// when no unplanted grass is in range.
    pub play_plant_destination_tile: Option<(i32, i32)>,
    /// Phase 5e-xiii-a: when the agent is committed to building a personal
    /// blueprint (`bp.personal_owner == Some(self)`) whose deposits are
    /// *not* yet satisfied, the dispatcher snapshots the most-deficient
    /// resource the bp still needs here. Read by
    /// `WithdrawAndHaulToPersonalBlueprintMethod` so the typed
    /// `WithdrawMaterial { resource_id }` head carries the right resource
    /// payload. Set only on the personal-build path; left `None` on the
    /// JobClaim::Build path (which only fires once `bp.is_satisfied()`) so
    /// the existing `BuildClaimedBlueprintMethod` wins as before.
    pub personal_bp_resource: Option<ResourceId>,
    /// True when the agent has a Weapon resource in any of: `Equipment`
    /// MainHand, `Carrier` hands, `EconomicAgent.inventory`. Gates
    /// `HuntPreyMethod` / `MusterAtHearthMethod` / `TravelToHuntAreaMethod`
    /// so an unarmed hunter falls through to `EquipHuntingSpear` instead
    /// of marching into combat with bare hands. Only populated by the
    /// engage-prey and join-hunt-party dispatchers; everywhere else
    /// defaults to `false` (the hunting methods are the only readers).
    pub agent_has_weapon: bool,
    /// Farm-planner follow-up: when a `AgentGoal::Farm` harvest should route
    /// its trailing `Task::DepositToFactionStorage` to a household-specific
    /// `FactionStorageTile` instead of the actor's faction storage, the
    /// harvest dispatcher snapshots `Some(household_id)` here. Read by
    /// `HarvestMaturePlantForStorageMethod::expand`. `None` everywhere else
    /// preserves the actor-faction default.
    pub deposit_target_faction_override: Option<u32>,
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

    /// Pluralist Economy R4: per-resource policy preconditions. Each
    /// entry says "to fire, the agent's effective faction must have
    /// `flag` set on its `economic_policy` for `resource`". Methods
    /// that don't care about per-resource policy (today: every
    /// existing method) leave this as the default empty slice.
    /// New methods registered in R6+ (Trader, household-driven
    /// posting paths, P2P contract producers) declare non-empty
    /// gates so they only fire under the right policy mix.
    fn policy_gate(&self) -> &'static [crate::economy::policy::PolicyGateEntry] {
        &[]
    }

    /// Static name for debug / inspector display. Keep these short and
    /// human-recognisable.
    fn name(&self) -> &'static str;

    /// Stable identity for `MethodHistory` keying. Phase 6a: every method
    /// returns a hardcoded `MethodId::*` const; the registry doesn't yet
    /// consume the value, but the trait surface lets 6b's outcome-recording
    /// sites stamp the right id without re-deriving it from `name()`.
    fn id(&self) -> MethodId;

    /// Phase E: personality-driven multiplier on the method's
    /// `utility()`. Returns `1.0` by default (no lift). Methods that
    /// want disposition-driven behaviour override and read the agent's
    /// `Disposition` axes (e.g. `gregariousness` for socialize/play,
    /// `martial` for combat, `curiosity` for explore/learn). Lift is
    /// applied only by `score_method_with_history_and_disposition`;
    /// the legacy `score_method_with_history` (used by ~21 unmigrated
    /// dispatchers) ignores it for backwards compatibility.
    ///
    /// Lifts should stay sub-tier (recommended range ~`[1.0, 1.3]`)
    /// so they don't cross `UTIL_BASELINE` → `UTIL_VISIBLE_GROUND`
    /// breakpoints. Method-ranking tests pin those tier boundaries.
    fn disposition_lift(&self, _disposition: crate::simulation::goal_scorers::Disposition) -> f32 {
        1.0
    }
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
        self.by_kind.get(&kind).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn method_count(&self, kind: AbstractTaskKind) -> usize {
        self.methods_for(kind).len()
    }

    /// Look up a registered method's `flags()` by its stable `MethodId`.
    /// O(N) over every registered method (~50 entries) — only called from
    /// `goal_dispatch_system`'s goal-flip path which fires on a tiny subset
    /// of agents per tick. Returns `None` for `MethodId::UNKNOWN` /
    /// `TERMINAL_EXPLORE` and any synthetic id without a registered owner.
    pub fn flags_by_id(&self, id: MethodId) -> Option<MethodFlags> {
        for methods in self.by_kind.values() {
            for m in methods {
                if m.id() == id {
                    return Some(m.flags());
                }
            }
        }
        None
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
        AbstractTaskKind::AcquireFood,
        Box::new(ForageFromKnownMethod),
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
    reg.register(
        AbstractTaskKind::StockpileFood,
        Box::new(ScavengeFoodForStorageMethod),
    );
    reg.register(
        AbstractTaskKind::StockpileFood,
        Box::new(ForageFromKnownForStorageMethod),
    );
    reg.register(
        AbstractTaskKind::StockpileFood,
        Box::new(ExploreForFoodForStorageMethod),
    );
    reg.register(AbstractTaskKind::Scout, Box::new(ScoutForPreyMethod));
    reg.register(
        AbstractTaskKind::EquipHuntingSpear,
        Box::new(WithdrawAndEquipHuntingSpearMethod),
    );
    reg.register(
        AbstractTaskKind::ReturnSurplus,
        Box::new(DepositSurplusAtStorageMethod),
    );
    reg.register(
        AbstractTaskKind::TameWildAnimal,
        Box::new(TameWildAnimalMethod),
    );
    reg.register(
        AbstractTaskKind::PlantFromStorage,
        Box::new(WithdrawAndPlantSeedMethod),
    );
    reg.register(
        AbstractTaskKind::ConstructBlueprint,
        Box::new(BuildClaimedBlueprintMethod),
    );
    reg.register(
        AbstractTaskKind::ConstructBlueprint,
        Box::new(WithdrawAndHaulToPersonalBlueprintMethod),
    );
    reg.register(
        AbstractTaskKind::ConstructBlueprint,
        Box::new(GatherAndHaulToPersonalBlueprintMethod),
    );
    reg.register(
        AbstractTaskKind::DeliverHuntKill,
        Box::new(DeliverHuntKillMethod),
    );
    reg.register(AbstractTaskKind::EngagePrey, Box::new(HuntPreyMethod));
    reg.register(
        AbstractTaskKind::EngagePrey,
        Box::new(PickUpFreshCorpseMethod),
    );
    reg.register(
        AbstractTaskKind::JoinHuntParty,
        Box::new(MusterAtHearthMethod),
    );
    reg.register(
        AbstractTaskKind::JoinHuntParty,
        Box::new(TravelToHuntAreaMethod),
    );
    reg.register(
        AbstractTaskKind::Socialize,
        Box::new(SocializeWithPartnerMethod),
    );
    reg.register(AbstractTaskKind::Raid, Box::new(RaidEnemyHomeMethod));
    reg.register(AbstractTaskKind::Defend, Box::new(DefendCampMethod));
    reg.register(AbstractTaskKind::Lead, Box::new(LeadCampMethod));
    reg.register(
        AbstractTaskKind::RescueAlly,
        Box::new(EngageRescueAttackerMethod),
    );
    reg.register(
        AbstractTaskKind::DeliverMaterialToCraftOrder,
        Box::new(WithdrawAndHaulToCraftOrderMethod),
    );
    reg.register(
        AbstractTaskKind::WorkOnCraftOrder,
        Box::new(WorkOnSatisfiedCraftOrderMethod),
    );
    reg.register(
        AbstractTaskKind::HarvestGrainForCraftOrder,
        Box::new(HarvestAndHaulGrainToCraftOrderMethod),
    );
    reg.register(
        AbstractTaskKind::HarvestPlant,
        Box::new(HarvestMaturePlantForStorageMethod),
    );
    reg.register(AbstractTaskKind::Play, Box::new(PlayWithPartnerMethod));
    reg.register(AbstractTaskKind::Play, Box::new(PlaySoloMethod));
    reg.register(
        AbstractTaskKind::Play,
        Box::new(WithdrawAndThrowStonesAsPlayMethod),
    );
    reg.register(
        AbstractTaskKind::Play,
        Box::new(WithdrawAndPlayWithToyMethod),
    );
    reg.register(
        AbstractTaskKind::Play,
        Box::new(WithdrawAndPlantGrainSeedAsPlayMethod),
    );
    reg.register(
        AbstractTaskKind::Play,
        Box::new(WithdrawAndPlantBerrySeedAsPlayMethod),
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
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let bed = ctx.home_bed.filter(|_| ctx.home_bed_tile.is_some());
        vec![Task::Sleep { bed }]
    }

    fn name(&self) -> &'static str {
        "Sleep"
    }

    fn id(&self) -> MethodId {
        MethodId::SLEEP
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
        // Single-method registry today, so any positive value wins.
        // `UTIL_BASELINE` matches `SleepMethod`; future Eat methods (e.g.
        // EatFromCarriedFood with a freshness preference) will
        // discriminate here.
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        vec![Task::Eat]
    }

    fn name(&self) -> &'static str {
        "EatFromInventory"
    }

    fn id(&self) -> MethodId {
        MethodId::EAT_FROM_INVENTORY
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
        // Baseline tier minus chebyshev distance to the storage tile
        // (capped at `MAX_DIST_PENALTY`). Sibling
        // `ScavengeFoodFromGroundMethod` (`UTIL_VISIBLE_GROUND`) keeps a
        // ≥0.20 margin even at the worst-case dist-spread because both
        // methods clamp at the same cap.
        UTIL_BASELINE - dist_penalty(ctx, ctx.nearest_storage_tile)
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

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_FROM_STORAGE
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
        // Visible-ground tier: outranks `WithdrawFromStorageMethod`
        // (`UTIL_BASELINE`). Distance discount picks the closer of two
        // visible piles; cap (`MAX_DIST_PENALTY`) preserves the
        // inter-tier ranking against the baseline-tier sibling.
        UTIL_VISIBLE_GROUND - dist_penalty(ctx, ctx.scavenge_target_tile)
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

    fn id(&self) -> MethodId {
        MethodId::SCAVENGE_FOOD_FROM_GROUND
    }
}

/// Sole pre-Phase-5c-ii method for `AbstractTask::AcquireGood { good }`. The
/// material analogue of `WithdrawFromStorageMethod`: reads the target good from
/// the abstract task, gates on the per-good ctx fields the dispatcher will
/// populate, and expands to a single `Task::WithdrawMaterial { resource_id: good.into(), qty: 1 }`.
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
        // Baseline tier minus chebyshev distance to the material storage
        // tile (capped at `MAX_DIST_PENALTY`). Mirrors
        // `WithdrawFromStorageMethod`'s shape — same tier, same penalty
        // schedule, different ctx field.
        UTIL_BASELINE - dist_penalty(ctx, ctx.material_storage_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
            return Vec::new();
        };
        // Defensive: precondition guarantees Some(_), but a method body
        // shouldn't unwrap on a ctx field.
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![Task::WithdrawMaterial {
            resource_id,
            qty: 1,
        }]
    }

    fn name(&self) -> &'static str {
        "WithdrawMaterialFromStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_MATERIAL_FROM_STORAGE
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
        // Claimed-haul tier minus the full agent→storage→blueprint trip,
        // capped at `MAX_DIST_PENALTY`. Stays strictly above
        // `WithdrawMaterialFromStorageMethod` (`UTIL_BASELINE`) even at
        // max penalty (`UTIL_CLAIMED_HAUL - MAX_DIST_PENALTY = 1.70 >
        // UTIL_BASELINE`). Falls back to the storage-only signal when the
        // blueprint tile is missing.
        UTIL_CLAIMED_HAUL
            - full_trip_penalty(ctx, ctx.material_storage_tile, ctx.claimed_blueprint_tile)
    }

    fn flags(&self) -> MethodFlags {
        // Mirrors the legacy `ClaimedHaul` plan's `PF_UNINTERRUPTIBLE` — once
        // the agent commits to the chain it shouldn't drop it on a routine
        // goal flip. The dispatcher doesn't yet read flags (5a-ii pattern), so
        // this is documentation-of-intent today.
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
            return Vec::new();
        };
        let Some(blueprint) = ctx.claimed_blueprint else {
            return Vec::new();
        };
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![
            Task::WithdrawMaterial {
                resource_id,
                qty: 1,
            },
            Task::HaulToBlueprint { blueprint },
        ]
    }

    fn name(&self) -> &'static str {
        "WithdrawAndHaulToBlueprint"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_HAUL_TO_BLUEPRINT
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
        // Baseline tier minus the full agent→gather→deposit trip when
        // both legs are populated, capped at `MAX_DIST_PENALTY`. Falls
        // back to the gather-only signal when no deposit anchor is set
        // (SOLO / no storage / faction without storage tiles).
        UTIL_BASELINE - full_trip_penalty(ctx, ctx.gather_target_tile, ctx.gather_deposit_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
            return Vec::new();
        };
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![
            Task::Gather { tile },
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id: None,
            },
        ]
    }

    fn name(&self) -> &'static str {
        "GatherFromKnown"
    }

    fn id(&self) -> MethodId {
        MethodId::GATHER_FROM_KNOWN
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
        // Visible-ground tier minus the full agent→scavenge→deposit trip
        // when both legs are populated, capped at `MAX_DIST_PENALTY`.
        // Falls back to the scavenge-only signal when no deposit anchor
        // is set.
        UTIL_VISIBLE_GROUND
            - full_trip_penalty(ctx, ctx.scavenge_target_tile, ctx.scavenge_deposit_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
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
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id: None,
            },
        ]
    }

    fn name(&self) -> &'static str {
        "ScavengeFromGround"
    }

    fn id(&self) -> MethodId {
        MethodId::SCAVENGE_FROM_GROUND
    }
}

/// Phase 5c-ii-d-iv-i fallback method for `AbstractTask::AcquireFood`. Mirrors
/// the legacy `ExploreForFood` plan (PlanId 35, single step `Explore`,
/// `serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS`, `bias: 0.3`,
/// `flags: PF_EXPLORE | PF_TARGETS_FOOD`). Fires when the dispatcher's ctx
/// shows no concrete food source — no storage stock, no visible scavenge
/// target — but the agent is still hungry. The expansion is a single
/// `Task::Explore { kind: MemoryKind::AnyEdible }`; the legacy `TaskKind::Explore`
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
        UTIL_EXPLORE_FALLBACK
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return Vec::new();
        }
        vec![Task::Explore {
            kind: MemoryKind::AnyEdible,
        }]
    }

    fn name(&self) -> &'static str {
        "ExploreForFood"
    }

    fn id(&self) -> MethodId {
        MethodId::EXPLORE_FOR_FOOD
    }
}

/// Phase 5c-ii-d-iv-i fallback method for `AbstractTask::AcquireGood { good }`.
/// Mirrors the legacy `ExploreForWood` / `ExploreForStone` plans (PlanId
/// 36/37, single step `Explore`, `bias: 0.3`,
/// `flags: PF_EXPLORE | PF_TARGETS_{WOOD,STONE}`). Fires when the dispatcher's
/// ctx shows no concrete material source — no storage stock, no visible
/// scavenge target, no known harvest tile, no claimed blueprint. The
/// expansion is a single `Task::Explore { kind: MemoryKind::Resource(WOOD/STONE) }` —
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
/// Precondition gates on the `resource_id` payload mapping cleanly to a
/// `MemoryKind` — only Wood and Stone are gather goals today. Other resources
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
    /// Map a target resource to the `MemoryKind` the agent records when they
    /// spot a source of it. Only Wood / Stone today — other resources have
    /// no corresponding `MemoryKind` because no gather-goal targets them.
    /// Returns `None` for unsupported resources so the method can opt out
    /// cleanly.
    fn memory_kind_for(resource_id: ResourceId) -> Option<MemoryKind> {
        if Some(resource_id) == crate::economy::core_ids::Wood.get().copied() {
            Some(MemoryKind::wood())
        } else if Some(resource_id) == crate::economy::core_ids::Stone.get().copied() {
            Some(MemoryKind::stone())
        } else {
            None
        }
    }
}

impl Method for ExploreForMaterialMethod {
    fn precondition(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> bool {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
            return false;
        };
        Self::memory_kind_for(resource_id).is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_EXPLORE_FALLBACK
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::AcquireGood { resource_id } = abstract_task else {
            return Vec::new();
        };
        let Some(kind) = Self::memory_kind_for(resource_id) else {
            return Vec::new();
        };
        vec![Task::Explore { kind }]
    }

    fn name(&self) -> &'static str {
        "ExploreForMaterial"
    }

    fn id(&self) -> MethodId {
        MethodId::EXPLORE_FOR_MATERIAL
    }
}

/// Phase 5c-ii-d-vi method for `AbstractTask::StockpileFood` — the
/// chief-driven counterpart to `ScavengeFoodFromGroundMethod`. Replaces the
/// `AgentGoal::GatherFood` half of the legacy `ScavengeFood` plan (PlanId 6,
/// which served `SURVIVE_AND_GATHER_FOOD_GOALS` until 5c-ii-d-iii-ii
/// retargeted it to `GATHER_FOOD_GOALS` only). The legacy plan's two-step
/// `[CollectFood, DepositGoods]` chain becomes the typed-task chain
/// `[Scavenge, DepositToFactionStorage { good }]`.
///
/// Where this differs from `ScavengeFoodFromGroundMethod`:
/// - **No hunger gate.** Chief-driven storage-fill fires regardless of the
///   agent's hunger; an agent with full hands of fruit is exactly who the
///   chief wants walking to storage.
/// - **Trailing task is Deposit, not Eat.** GatherFood's intent is "fill
///   storage," so the chain ends in `DepositToFactionStorage { good }`. The
///   `good` payload threads through from `ctx.scavenge_food_good`, which the
///   dispatcher populates by inspecting the picked GroundItem.
/// - **Different abstract task kind.** `AcquireFood` and `StockpileFood` are
///   sibling abstract tasks, mirroring the AcquireFood/AcquireGood split.
///   Methods can't share a `precondition`/`expand` body across both intents
///   because the chain shape diverges fundamentally — Eat-in-place vs walk-
///   to-storage-and-deposit.
///
/// Utility `1.5` matches `ScavengeFoodFromGroundMethod` and
/// `ScavengeFromGroundMethod` (bias-on-visibility — outranks the explore
/// fallback's `0.3`). Distance-weighted via `dist_penalty` so two visible
/// piles tie-break on closer-target.
pub struct ScavengeFoodForStorageMethod;

impl Method for ScavengeFoodForStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::StockpileFood) {
            return false;
        }
        // Both fields must be populated — the entity is the executor's input
        // (`Task::Scavenge { target }`); the tile is the dispatcher's input
        // (`assign_task_with_routing` needs somewhere to route to). The good
        // is the deposit's payload — without it the chain can't know what to
        // record on the deposit, so opt out cleanly.
        ctx.scavenge_target_entity.is_some()
            && ctx.scavenge_target_tile.is_some()
            && ctx.scavenge_food_good.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Visible-ground tier — same shape as `ScavengeFromGroundMethod`
        // (the AcquireGood sibling); chief-driven storage-fill rather
        // than personal-hunger drive.
        UTIL_VISIBLE_GROUND
            - full_trip_penalty(ctx, ctx.scavenge_target_tile, ctx.scavenge_deposit_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::StockpileFood) {
            return Vec::new();
        }
        let Some(target) = ctx.scavenge_target_entity else {
            return Vec::new();
        };
        if ctx.scavenge_target_tile.is_none() {
            return Vec::new();
        }
        let Some(resource_id) = ctx.scavenge_food_good else {
            return Vec::new();
        };
        vec![
            Task::Scavenge { target },
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id: None,
            },
        ]
    }

    fn name(&self) -> &'static str {
        "ScavengeFoodForStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::SCAVENGE_FOOD_FOR_STORAGE
    }
}

/// Phase 5c-ii-d-vi fallback method for `AbstractTask::StockpileFood`.
/// Replaces the `AgentGoal::GatherFood` half of the legacy `ExploreForFood`
/// plan (PlanId 35, retired by this PR). Mirrors `ExploreForFoodMethod` but
/// without the hunger gate — chief-driven storage-fill explores even when no
/// agent is hungry, because the goal is sustaining stockpile depth rather
/// than satisfying an immediate need.
///
/// Utility `0.3` matches the legacy plan's `bias` and `ExploreForFoodMethod`.
/// Loses to `ScavengeFoodForStorageMethod` (1.5) when a visible target is
/// available. The legacy plan's candidate-filter inversion ("only fires with
/// no source vis AND no good vis AND no memory") collapses into the
/// utility-ranking model: at 0.3, this method only wins when no concrete
/// method's precondition fires.
pub struct ExploreForFoodForStorageMethod;

impl Method for ExploreForFoodForStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::StockpileFood)
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_EXPLORE_FALLBACK
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::StockpileFood) {
            return Vec::new();
        }
        vec![Task::Explore {
            kind: MemoryKind::AnyEdible,
        }]
    }

    fn name(&self) -> &'static str {
        "ExploreForFoodForStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::EXPLORE_FOR_FOOD_FOR_STORAGE
    }
}

/// Method for `AbstractTask::AcquireFood`: harvest a known mature
/// food-bearing plant (berry bush, grain) and eat in place. Mirrors
/// `GatherFromKnownMethod`'s shape under AcquireGood (Wood/Stone) but the
/// trailing leg is `Eat` instead of `DepositToFactionStorage` — the agent's
/// own hunger drove the dispatch, so the harvest goes straight to mouth.
///
/// Replaces the AcquireFood half of the legacy `ForageFood` plan (PlanId 0,
/// `[ForageGrass, DepositGoods]`). The legacy plan always deposited at
/// faction storage; the HTN split intentionally skips storage when the agent
/// is hungry — `htn_eat_dispatch_system` would just walk the food back out
/// next tick.
///
/// Utility `UTIL_BASELINE` (1.0) — same tier as `WithdrawFromStorageMethod`
/// and `GatherFromKnownMethod`. Single-leg distance discount on the
/// agent→plant hop (no second hop to discount, since Eat is in-place).
pub struct ForageFromKnownMethod;

impl Method for ForageFromKnownMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return false;
        }
        ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.gather_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::AcquireFood) {
            return Vec::new();
        }
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![Task::Gather { tile }, Task::Eat]
    }

    fn name(&self) -> &'static str {
        "ForageFromKnown"
    }

    fn id(&self) -> MethodId {
        MethodId::FORAGE_FROM_KNOWN
    }
}

/// Method for `AbstractTask::StockpileFood`: harvest a known mature
/// food-bearing plant and deposit at faction storage. Chief-driven
/// counterpart to `ForageFromKnownMethod` — fires regardless of the agent's
/// personal hunger, because the goal is sustaining the storage stockpile.
///
/// Replaces the GatherFood half of the legacy `ForageFood` plan (PlanId 0).
/// The trailing `DepositToFactionStorage { good }` carries the food good the
/// plant at `gather_target_tile` will yield (`forage_food_good` ctx field),
/// for chain-integrity inspection — the deposit executor itself is
/// parameterless and dumps everything in hand.
///
/// Utility `UTIL_BASELINE` (1.0) — same tier as `ForageFromKnownMethod`.
/// Full-trip distance discount on agent→plant→storage when both
/// `gather_deposit_tile` and `gather_target_tile` are populated; falls back
/// to single-leg when the faction has no storage.
pub struct ForageFromKnownForStorageMethod;

impl Method for ForageFromKnownForStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::StockpileFood) {
            return false;
        }
        // Need both the harvest tile (head Task::Gather { tile }) and the
        // good (trailing Task::DepositToFactionStorage { good, target_faction_id: None }).
        // Without the good the chain can't be expressed in typed form, even
        // though the deposit executor itself is parameterless.
        ctx.gather_target_tile.is_some() && ctx.forage_food_good.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - full_trip_penalty(ctx, ctx.gather_target_tile, ctx.gather_deposit_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::StockpileFood) {
            return Vec::new();
        }
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        let Some(resource_id) = ctx.forage_food_good else {
            return Vec::new();
        };
        vec![
            Task::Gather { tile },
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id: None,
            },
        ]
    }

    fn name(&self) -> &'static str {
        "ForageFromKnownForStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::FORAGE_FROM_KNOWN_FOR_STORAGE
    }
}

/// Sole method for `AbstractTask::Scout`. Single-step expansion to
/// `Task::Explore { kind: MemoryKind::Prey }` — the agent walks toward a
/// random reachable tile near faction home and `vision_system` writes prey
/// memory along the way. Hunter-only, gated by the chief flipping
/// `HuntOrder::Scout` (gating is enforced by `htn_scout_dispatch_system`,
/// not the precondition, because the order lives on `FactionData` and isn't
/// part of `PlannerCtx`).
///
/// Replaces the legacy `ScoutForPrey` plan (PlanId 65) and its `WanderForPrey`
/// step (StepId 59) + `StepTarget::ScoutForPrey` resolver. Single-method
/// registry — no scoring competition. `UTIL_BASELINE` is arbitrary; the
/// dispatcher always picks the sole applicable method.
pub struct ScoutForPreyMethod;

impl Method for ScoutForPreyMethod {
    fn precondition(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Scout)
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::Scout) {
            return Vec::new();
        }
        vec![Task::Explore {
            kind: MemoryKind::Prey,
        }]
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "ScoutForPrey"
    }

    fn id(&self) -> MethodId {
        MethodId::SCOUT_FOR_PREY
    }
}

/// Sole method for `AbstractTask::EquipHuntingSpear`. Two-leg expansion:
/// `[Task::WithdrawMaterial { weapon, 1 }, Task::Equip { MainHand, weapon }]`.
/// `MF_UNINTERRUPTIBLE` so a hungry hunter mid-fetch doesn't peel off mid-trip
/// (mirrors the legacy plan's bias 5.0 + `PF_UNINTERRUPTIBLE`).
///
/// Replaces the legacy `AcquireHuntingSpear` plan (PlanId 64) and its two
/// step defs (StepId 52 WithdrawSpear, StepId 56 EquipMainHand). Single-method
/// registry — no scoring competition; the dispatcher's gating (agent unarmed
/// + faction has spear stock) is what governs whether the chain fires.
///
/// Distance discount on `material_storage_tile` so a hunter near storage
/// arms slightly faster than one on the far side of camp; the cap-preserving
/// invariant keeps the relative ranking inside the tier.
pub struct WithdrawAndEquipHuntingSpearMethod;

impl Method for WithdrawAndEquipHuntingSpearMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::EquipHuntingSpear) {
            return false;
        }
        // Need a faction storage tile that holds at least one Weapon. The
        // dispatcher populates `material_storage_tile` + `material_stock_for_target`
        // from a per-good lookup over `StorageTileMap.by_faction`.
        ctx.material_storage_tile.is_some() && ctx.material_stock_for_target > 0
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.material_storage_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::EquipHuntingSpear) {
            return Vec::new();
        }
        let weapon = crate::economy::core_ids::weapon();
        vec![
            Task::WithdrawMaterial {
                resource_id: weapon,
                qty: 1,
            },
            Task::Equip {
                slot: crate::simulation::items::EquipmentSlot::MainHand,
                resource_id: weapon,
            },
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "WithdrawAndEquipHuntingSpear"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_EQUIP_HUNTING_SPEAR
    }
}

/// Sole method for `AbstractTask::ReturnSurplus`. Single-leg expansion
/// `[Task::DepositToFactionStorage { resource_id: <picked food>, target_faction_id: None }]` — the
/// agent is holding food from a foraging trip and walks back to faction
/// storage to drop it off. The `resource_id` payload is informational (the
/// `drop_items_at_destination_system` executor dumps everything in hands +
/// surplus inventory regardless of payload); threading the actual carried
/// food good through keeps the chain inspectable in the same shape as
/// `ScavengeFoodForStorageMethod`.
///
/// Replaces the legacy `ReturnSurplusFood` plan (PlanId 24) and its single
/// step (StepId 12 DepositGoods). Distance discount on `nearest_storage_tile`.
pub struct DepositSurplusAtStorageMethod;

impl Method for DepositSurplusAtStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::ReturnSurplus) {
            return false;
        }
        // Need a target tile and *something* to deposit. The dispatcher only
        // builds a valid ctx when the agent actually has surplus food — the
        // `scavenge_food_good` ctx field doubles as the deposit good for this
        // method (mirrors `ScavengeFoodForStorageMethod`'s usage).
        ctx.nearest_storage_tile.is_some() && ctx.scavenge_food_good.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.nearest_storage_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::ReturnSurplus) {
            return Vec::new();
        }
        let Some(resource_id) = ctx.scavenge_food_good else {
            return Vec::new();
        };
        vec![Task::DepositToFactionStorage {
            resource_id,
            target_faction_id: None,
        }]
    }

    fn name(&self) -> &'static str {
        "DepositSurplusAtStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::DEPOSIT_SURPLUS_AT_STORAGE
    }
}

/// Sole method for `AbstractTask::TameWildAnimal`. Single-leg expansion
/// `[Task::TameAnimal { target }]` — agent walks to the candidate's tile and
/// works adjacent until `tame_task_system` fires. Reuses the
/// `scavenge_target_entity`/`scavenge_target_tile` ctx fields to carry the
/// target entity + tile (semantically "an entity the agent walks to and
/// interacts with"). The dispatcher does the per-species tech gate at scan
/// time (Horse → HORSE_TAMING, Cow/Pig → ANIMAL_HUSBANDRY, Cat →
/// DOG_DOMESTICATION), so this method has no static `tech_gate`.
pub struct TameWildAnimalMethod;

impl Method for TameWildAnimalMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::TameWildAnimal) {
            return false;
        }
        ctx.scavenge_target_entity.is_some() && ctx.scavenge_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.scavenge_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::TameWildAnimal) {
            return Vec::new();
        }
        let Some(target) = ctx.scavenge_target_entity else {
            return Vec::new();
        };
        vec![Task::TameAnimal { target }]
    }

    fn name(&self) -> &'static str {
        "TameWildAnimal"
    }

    fn id(&self) -> MethodId {
        MethodId::TAME_WILD_ANIMAL
    }
}

/// Sole method for `AbstractTask::PlantFromStorage`. Two-leg expansion:
/// `[Task::WithdrawMaterial { seed, 1 }, Task::Planter { tile }]`.
/// `MF_UNINTERRUPTIBLE` so a hungry farmer mid-fetch doesn't peel off before
/// the seed is in the ground (mirrors `WithdrawAndEquipHuntingSpearMethod`'s
/// chain integrity).
///
/// Replaces the dead legacy `PlantFromStorage` (PlanId 4, `[StepId(33)
/// WithdrawGrainSeed, StepId(4) PlantGrainSeed]`) and `PlantBerryFromStorage`
/// (PlanId 66, `[StepId(60) WithdrawBerrySeed, StepId(61) PlantBerrySeed]`)
/// plans. Both were registered but never seeded into any `KnownPlans` —
/// chiefs posting `JobKind::Farm` could only drive harvesting via `FarmFood`
/// (PlanId 1). This method restores the planting half of the Farm goal.
///
/// Distance discount on `material_storage_tile` so the farmer prefers the
/// storage tile closer to them; the `gather_target_tile` is the destination
/// farmland tile threaded through `Task::Planter { tile }`.
pub struct WithdrawAndPlantSeedMethod;

impl Method for WithdrawAndPlantSeedMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::PlantFromStorage { .. }) {
            return false;
        }
        ctx.material_storage_tile.is_some()
            && ctx.material_stock_for_target > 0
            && ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.material_storage_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::PlantFromStorage { resource_id } = abstract_task else {
            return Vec::new();
        };
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![
            Task::WithdrawMaterial {
                resource_id,
                qty: 1,
            },
            Task::Planter { tile },
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::CROP_CULTIVATION)
    }

    fn name(&self) -> &'static str {
        "WithdrawAndPlantSeed"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_PLANT_SEED
    }
}

/// Sole method for `AbstractTask::ConstructBlueprint`. The dispatcher gates on
/// `JobClaim::Build` + `ClaimTarget.blueprint = Some(_)` + `bp.is_satisfied()`,
/// snapshots the blueprint entity into `ctx.claimed_blueprint` (re-using the
/// existing slot from the haul branch — semantically "the blueprint this agent
/// is committed to"), and the method emits the single-task expansion
/// `[Task::Construct { blueprint }]`. `MF_UNINTERRUPTIBLE` mirrors the legacy
/// `PF_UNINTERRUPTIBLE` on PlanId 34 so a transient goal flip mid-walk doesn't
/// drop the claim. Replaces the legacy `ClaimedBuild` plan (PlanId 34) and its
/// `BuildClaimedBlueprint` step (StepId 43).
pub struct BuildClaimedBlueprintMethod;

impl Method for BuildClaimedBlueprintMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return false;
        }
        // The bp must exist *and* its deposits must be satisfied. The
        // Path A (JobClaim::Build) dispatcher gate already guarantees
        // satisfaction (it `continue`s on `!bp.is_satisfied()`). The Path B
        // (personal-build) dispatcher only populates `personal_bp_resource`
        // when deposits are unmet — so `personal_bp_resource.is_none()`
        // means either Path A or Path B with `bp.is_satisfied()`. This keeps
        // BuildClaimed from firing on an unsatisfied personal bp where
        // `WithdrawAndHaulToPersonalBlueprintMethod` /
        // `GatherAndHaulToPersonalBlueprintMethod` should win.
        ctx.claimed_blueprint.is_some() && ctx.personal_bp_resource.is_none()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.claimed_blueprint_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return Vec::new();
        }
        let Some(blueprint) = ctx.claimed_blueprint else {
            return Vec::new();
        };
        vec![Task::Construct { blueprint }]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "BuildClaimedBlueprint"
    }

    fn id(&self) -> MethodId {
        MethodId::BUILD_CLAIMED_BLUEPRINT
    }
}

/// Phase 5e-xiii-a method for `AbstractTask::ConstructBlueprint`. Fires when
/// the agent owns a personal blueprint (`bp.personal_owner == Some(self)`) whose
/// deposits are *not* yet satisfied and the faction's storage holds at least
/// one unit of the most-deficient resource the bp still needs. Replaces the
/// storage-fed legacy `HaulFromStorageAndBuild` plan (PlanId 29) for the
/// personal-blueprint path.
///
/// The expansion is a 2-task chain `[WithdrawMaterial, HaulToBlueprint]`,
/// matching the AcquireGood haul method's shape but routed off the
/// `personal_blueprint`/`personal_bp_resource` ctx slots instead of the
/// JobClaim::Haul `ClaimTarget`. The chain handoff lives in
/// `production::finish_withdraw_material`'s existing
/// `Task::HaulToBlueprint { blueprint }` arm — once the resource is in hand,
/// the agent routes to the bp's tile via `TaskKind::HaulMaterials` and the
/// hauler branch of `construction_system` deposits-on-arrival. After the
/// deposit, the agent returns to Idle; the next dispatch tick re-evaluates
/// (either dispatching a fresh withdraw chain for the next deficit slot, or
/// the existing `BuildClaimedBlueprintMethod` if the bp is now satisfied).
///
/// `MF_UNINTERRUPTIBLE` mirrors the legacy `PF_UNINTERRUPTIBLE` on PlanId 29 —
/// once committed to a haul leg, a transient goal flip mid-trip shouldn't
/// strand the agent. The dispatcher only populates `personal_bp_resource` on
/// the personal-build path with deposits unmet; the JobClaim::Build path
/// leaves it `None`, so this method never wins on a chief-driven build.
pub struct WithdrawAndHaulToPersonalBlueprintMethod;

impl Method for WithdrawAndHaulToPersonalBlueprintMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return false;
        }
        ctx.personal_bp_resource.is_some()
            && ctx.material_storage_tile.is_some()
            && ctx.material_stock_for_target > 0
            && ctx.claimed_blueprint.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Claimed-haul tier minus the full agent→storage→blueprint trip,
        // capped at `MAX_DIST_PENALTY`. Mirrors `WithdrawAndHaulToBlueprintMethod`.
        UTIL_CLAIMED_HAUL
            - full_trip_penalty(ctx, ctx.material_storage_tile, ctx.claimed_blueprint_tile)
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return Vec::new();
        }
        let Some(blueprint) = ctx.claimed_blueprint else {
            return Vec::new();
        };
        let Some(resource_id) = ctx.personal_bp_resource else {
            return Vec::new();
        };
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![
            Task::WithdrawMaterial {
                resource_id,
                qty: 1,
            },
            Task::HaulToBlueprint { blueprint },
        ]
    }

    fn name(&self) -> &'static str {
        "WithdrawAndHaulToPersonalBlueprint"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_HAUL_TO_PERSONAL_BLUEPRINT
    }
}

/// Phase 5e-xiii-b method for `AbstractTask::ConstructBlueprint`. Mirrors
/// `WithdrawAndHaulToPersonalBlueprintMethod` but harvests from a memory-known
/// gather source instead of pulling from faction storage. Replaces the legacy
/// `BuildBlueprint` plan (PlanId 7) which encoded
/// `[GatherWood, HaulToBlueprint, BuildAnyBlueprint]` end-to-end.
///
/// Personal blueprints today need wood (Bed = 3 wood), but the method is
/// resource-agnostic: the dispatcher derives the gather memory key from
/// `personal_bp_resource` via `MemoryKind::Resource(rid)`, so any future
/// gather-able material (stone/etc.) added as a personal-bp ingredient flows
/// through automatically.
///
/// The expansion is a 2-task chain `[Gather { tile }, HaulToBlueprint { bp }]`.
/// The chain handoff lives in `gather::finish_gather`'s `Task::HaulToBlueprint`
/// arm — once the resource is in hand, the agent routes to the bp's tile via
/// `TaskKind::HaulMaterials` and the hauler branch of `construction_system`
/// deposits-on-arrival.
///
/// Utility-vs-`WithdrawAndHaulToPersonalBlueprintMethod`: the withdraw method
/// sits at `UTIL_CLAIMED_HAUL=2.0` (cheap haul from settled stock); this
/// method sits at `UTIL_BASELINE=1.0` (full chop-then-haul). When both fire
/// (storage holds wood AND the agent remembers a tree), the withdraw method
/// wins by ranking. Only when storage is dry does the gather method take
/// over. `MF_UNINTERRUPTIBLE` mirrors the legacy `PF_UNINTERRUPTIBLE` on
/// PlanId 7.
pub struct GatherAndHaulToPersonalBlueprintMethod;

impl Method for GatherAndHaulToPersonalBlueprintMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return false;
        }
        ctx.personal_bp_resource.is_some()
            && ctx.gather_target_tile.is_some()
            && ctx.claimed_blueprint.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - full_trip_penalty(ctx, ctx.gather_target_tile, ctx.claimed_blueprint_tile)
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::ConstructBlueprint) {
            return Vec::new();
        }
        let Some(blueprint) = ctx.claimed_blueprint else {
            return Vec::new();
        };
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        if ctx.personal_bp_resource.is_none() {
            return Vec::new();
        }
        vec![Task::Gather { tile }, Task::HaulToBlueprint { blueprint }]
    }

    fn name(&self) -> &'static str {
        "GatherAndHaulToPersonalBlueprint"
    }

    fn id(&self) -> MethodId {
        MethodId::GATHER_AND_HAUL_TO_PERSONAL_BLUEPRINT
    }
}

/// Sole method for `AbstractTask::DeliverHuntKill`. The dispatcher gates on
/// the agent holding a `Carrying` component (set by `pickup_corpse_task_system`
/// on arrival at a fresh corpse). Emits the two-leg expansion
/// `[Task::HaulCorpse { dest }, Task::Butcher]`. `MF_UNINTERRUPTIBLE` so a
/// hunger spike mid-haul doesn't peel the agent off — the corpse decays at
/// `CORPSE_FRESHNESS_TICKS=600` and the carrier is the only one who can
/// finish the job. Replaces the trailing two steps of the legacy `HuntFood`
/// plan (PlanId 5): `[StepId(54) HaulCorpse, StepId(55) Butcher]`. The plan
/// is truncated to `[Muster, Travel, Hunt, PickUp]` in the same sub-PR; once
/// PickUp ends, the plan completes and this method picks up via the
/// `Carrying` gate on the next dispatch tick.
pub struct DeliverHuntKillMethod;

impl Method for DeliverHuntKillMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::DeliverHuntKill) {
            return false;
        }
        ctx.butcher_site_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        // Tier: obligation. Once the agent picks up a corpse the only sensible
        // next step is to deliver it; baseline (1.0) suffices because the
        // dispatcher gates on `Carrying` and there are no sibling methods
        // competing for the slot. Distance discount on the haul leg.
        UTIL_BASELINE - dist_penalty(ctx, ctx.butcher_site_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::DeliverHuntKill) {
            return Vec::new();
        }
        let Some(dest) = ctx.butcher_site_tile else {
            return Vec::new();
        };
        vec![Task::HaulCorpse { dest }, Task::Butcher]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "DeliverHuntKill"
    }

    fn id(&self) -> MethodId {
        MethodId::DELIVER_HUNT_KILL
    }
}

/// First method for `AbstractTask::EngagePrey`. Fires when the dispatcher
/// finds a live prey entity within vision (LOS) or memory and emits the
/// single-task expansion `[Task::Hunt { prey }]`. The dispatcher pre-routes
/// the agent toward the prey's tile and sets `CombatTarget`; once adjacent,
/// `combat_system` engages and resolves the kill (after which it calls
/// `aq.advance()` to drain the typed channel — the dispatcher then re-fires
/// next tick, and `PickUpFreshCorpseMethod` typically wins because the new
/// corpse is right at the agent's feet). `MF_UNINTERRUPTIBLE` so a hunger
/// spike mid-combat doesn't peel the agent off; the legacy plan's
/// `PF_UNINTERRUPTIBLE` flag did the same.
pub struct HuntPreyMethod;

impl Method for HuntPreyMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::EngagePrey) {
            return false;
        }
        ctx.prey_target_entity.is_some() && ctx.agent_has_weapon
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.prey_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::EngagePrey) {
            return Vec::new();
        }
        let Some(prey) = ctx.prey_target_entity else {
            return Vec::new();
        };
        vec![Task::Hunt { prey }]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "HuntPrey"
    }

    fn id(&self) -> MethodId {
        MethodId::HUNT_PREY
    }

    /// Martial agents press the hunt harder. Lift capped at 1.3
    /// (martial=255) so HuntPrey's `UTIL_BASELINE` tier ranking
    /// against `PickUpFreshCorpseMethod` (`UTIL_VISIBLE_GROUND`) is
    /// preserved.
    fn disposition_lift(&self, d: crate::simulation::goal_scorers::Disposition) -> f32 {
        crate::simulation::utility_curves::disposition_lift(d.martial, 0.3)
    }
}

/// Second method for `AbstractTask::EngagePrey`. Fires when a fresh `Corpse`
/// is within `VIEW_RADIUS` of the agent (set by `combat_system`'s death path
/// just moments earlier, ideally at the agent's own tile after their kill).
/// Single-task expansion `[Task::PickUpCorpse { corpse }]`. `pickup_corpse_task_system`
/// inserts `Carrying(corpse)` on arrival, after which the next dispatch tick's
/// `htn_deliver_hunt_kill_dispatch_system` (5e-viii-a) takes over for the
/// haul → butcher tail. `MF_UNINTERRUPTIBLE` so a transient goal flip doesn't
/// peel the hunter off a kill they just made (the corpse decays in
/// `CORPSE_FRESHNESS_TICKS=600` and they're closest).
///
/// Utility tier: `UTIL_VISIBLE_GROUND=1.5` — once a kill is on the ground,
/// picking it up beats starting a new hunt. Mirrors the legacy reward_scale
/// hierarchy where the corpse-pickup step (0.4) sat at the same priority
/// tier as the hunt step (0.4) but the actual game-time priority came from
/// the plan being committed (`PF_UNINTERRUPTIBLE`); under HTN the explicit
/// utility lift makes the priority legible.
pub struct PickUpFreshCorpseMethod;

impl Method for PickUpFreshCorpseMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::EngagePrey) {
            return false;
        }
        ctx.fresh_corpse_entity.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_VISIBLE_GROUND - dist_penalty(ctx, ctx.fresh_corpse_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::EngagePrey) {
            return Vec::new();
        }
        let Some(corpse) = ctx.fresh_corpse_entity else {
            return Vec::new();
        };
        vec![Task::PickUpCorpse { corpse }]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "PickUpFreshCorpse"
    }

    fn id(&self) -> MethodId {
        MethodId::PICK_UP_FRESH_CORPSE
    }
}

/// First method for `AbstractTask::JoinHuntParty`. Fires while the chief's
/// hunt party hasn't yet deployed (`!hunt_party_deployed`) and the order
/// isn't stale. Emits `[Task::HuntPartyMuster { hearth }]` — agent walks to
/// the muster hearth and `wait_for_party_task_system` registers them in the
/// `HuntOrder::Hunt::mustered` Vec, blocking until the party fills or stales.
/// `MF_UNINTERRUPTIBLE` mirrors the legacy plan's `PF_UNINTERRUPTIBLE`. The
/// dispatcher resolves the hearth via `CampfireMap` (nearest to area_tile,
/// faction `home_tile` fallback), mirroring `StepTarget::HearthForHunt`.
pub struct MusterAtHearthMethod;

impl Method for MusterAtHearthMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::JoinHuntParty) {
            return false;
        }
        ctx.hunt_hearth_tile.is_some()
            && !ctx.hunt_party_deployed
            && !ctx.hunt_party_stale
            && ctx.agent_has_weapon
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.hunt_hearth_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::JoinHuntParty) {
            return Vec::new();
        }
        let Some(hearth) = ctx.hunt_hearth_tile else {
            return Vec::new();
        };
        vec![Task::HuntPartyMuster { hearth }]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "MusterAtHearth"
    }

    fn id(&self) -> MethodId {
        MethodId::MUSTER_AT_HEARTH
    }
}

/// Second method for `AbstractTask::JoinHuntParty`. Fires once the chief's
/// hunt party has deployed (`hunt_party_deployed`) or the order has gone
/// stale (`hunt_party_stale`). Emits `[Task::Explore { kind: Prey }]` — the
/// agent walks toward the chief's `area_tile` while `vision_system` records
/// any prey memory along the path. The semantically-overloaded use of
/// `Task::Explore` is intentional: this leg combines "walk to specific
/// tile" + "scan for prey memory en route," which is exactly what the
/// `Explore` typed task does (the dispatcher routes to the chief's tile
/// rather than a random one). On arrival, `goal_dispatch_system`'s
/// catch-all flips the typed channel back to Idle and the next dispatch
/// tick lets `htn_engage_prey_dispatch_system` take over for engagement.
/// `MF_UNINTERRUPTIBLE` mirrors the legacy plan's `PF_UNINTERRUPTIBLE`.
pub struct TravelToHuntAreaMethod;

impl Method for TravelToHuntAreaMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::JoinHuntParty) {
            return false;
        }
        ctx.hunt_area_tile.is_some()
            && (ctx.hunt_party_deployed || ctx.hunt_party_stale)
            && ctx.agent_has_weapon
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.hunt_area_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::JoinHuntParty) {
            return Vec::new();
        }
        vec![Task::Explore {
            kind: MemoryKind::Prey,
        }]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::HUNTING_SPEAR)
    }

    fn profession_gate(&self) -> Option<Profession> {
        Some(Profession::Hunter)
    }

    fn name(&self) -> &'static str {
        "TravelToHuntArea"
    }

    fn id(&self) -> MethodId {
        MethodId::TRAVEL_TO_HUNT_AREA
    }
}

/// Sole method for `AbstractTask::Socialize`. Single-leg expansion
/// `[Task::Socialize { partner }]`. Replaces the legacy `Socialize` plan
/// (PlanId 60) and its single step (StepId 48 NearestPlayPartner).
///
/// The dispatcher scans `SpatialIndex` within 12 tiles for the nearest other
/// Person (filtering out blueprints / ground items / animals), populates
/// `scavenge_target_entity` + `scavenge_target_tile` with the partner, and
/// the method's `expand` reads the entity. Distance discount on the
/// scavenge-target tile keeps "nearest partner" semantics inside the
/// argmax — though there is only one method, the dist penalty makes the
/// inspector readout reflect proximity.
///
/// Not `MF_UNINTERRUPTIBLE`: a sudden hunger spike or external preempt
/// (player order, distress response) is free to take precedence —
/// socialization is the lowest-priority need-driven activity.
pub struct SocializeWithPartnerMethod;

impl Method for SocializeWithPartnerMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::Socialize) {
            return false;
        }
        ctx.scavenge_target_entity.is_some() && ctx.scavenge_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.scavenge_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::Socialize) {
            return Vec::new();
        }
        let Some(partner) = ctx.scavenge_target_entity else {
            return Vec::new();
        };
        vec![Task::Socialize { partner }]
    }

    fn name(&self) -> &'static str {
        "SocializeWithPartner"
    }

    fn id(&self) -> MethodId {
        MethodId::SOCIALIZE_WITH_PARTNER
    }

    /// Gregarious agents lift the socialize utility — they pursue
    /// conversation harder than equidistant loners. Lift capped at
    /// 1.3 (gregariousness=255) so it stays under
    /// `UTIL_VISIBLE_GROUND=1.5` and the method's tier ranking holds.
    fn disposition_lift(&self, d: crate::simulation::goal_scorers::Disposition) -> f32 {
        crate::simulation::utility_curves::disposition_lift(d.gregariousness, 0.3)
    }
}

/// Sole method for `AbstractTask::Raid`. Single-leg expansion
/// `[Task::Raid { dest }]`. Replaces the legacy `Raid` plan (PlanId 61) and
/// its single step (StepId 49 FactionRaidTarget). The dispatcher reads
/// `FactionRegistry::raid_target` and threads the target faction's
/// `home_tile` through `gather_target_tile`. Solo agents and peacetime
/// factions resolve to `None` and the method's precondition fails — the
/// agent stays Idle and the next dispatch tick re-evaluates.
pub struct RaidEnemyHomeMethod;

impl Method for RaidEnemyHomeMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Raid) && ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Distance discount intentionally omitted: the raid target is one
        // fixed tile per faction-tick, so any inter-method ordering would
        // be vacuous (single-method registry).
        UTIL_BASELINE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::Raid) {
            return Vec::new();
        }
        let Some(dest) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![Task::Raid { dest }]
    }

    fn name(&self) -> &'static str {
        "RaidEnemyHome"
    }

    fn id(&self) -> MethodId {
        MethodId::RAID_ENEMY_HOME
    }
}

/// Sole method for `AbstractTask::Defend`. Single-leg expansion
/// `[Task::Defend { dest }]`. Replaces legacy `Defend` plan (PlanId 62) +
/// StepId 50 (FactionCamp). The dispatcher threads the faction's
/// `home_tile` through `gather_target_tile`.
pub struct DefendCampMethod;

impl Method for DefendCampMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Defend) && ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::Defend) {
            return Vec::new();
        }
        let Some(dest) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![Task::Defend { dest }]
    }

    fn name(&self) -> &'static str {
        "DefendCamp"
    }

    fn id(&self) -> MethodId {
        MethodId::DEFEND_CAMP
    }
}

/// Sole method for `AbstractTask::Lead`. Single-leg expansion
/// `[Task::Lead { dest }]`. Replaces legacy `Lead` plan (PlanId 63) +
/// StepId 51 (FactionCamp).
pub struct LeadCampMethod;

impl Method for LeadCampMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Lead) && ctx.gather_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::Lead) {
            return Vec::new();
        }
        let Some(dest) = ctx.gather_target_tile else {
            return Vec::new();
        };
        vec![Task::Lead { dest }]
    }

    fn name(&self) -> &'static str {
        "LeadCamp"
    }

    fn id(&self) -> MethodId {
        MethodId::LEAD_CAMP
    }
}

/// Sole method for `AbstractTask::RescueAlly`. Single-leg expansion
/// `[Task::RescueAlly { attacker, dest }]`. The dispatcher reads the
/// agent's `RescueTarget` (`(attacker, attacker_tile)`), populates
/// `scavenge_target_entity` / `scavenge_target_tile`, and writes
/// `CombatTarget(Some(attacker))` so `combat_system` engages on adjacency.
/// Replaces legacy `RescueAlly` plan (PlanId 23) + StepId 27 EngageRescue.
pub struct EngageRescueAttackerMethod;

impl Method for EngageRescueAttackerMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::RescueAlly)
            && ctx.scavenge_target_entity.is_some()
            && ctx.scavenge_target_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - dist_penalty(ctx, ctx.scavenge_target_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::RescueAlly) {
            return Vec::new();
        }
        let (Some(attacker), Some(dest)) = (ctx.scavenge_target_entity, ctx.scavenge_target_tile)
        else {
            return Vec::new();
        };
        vec![Task::RescueAlly { attacker, dest }]
    }

    fn name(&self) -> &'static str {
        "EngageRescueAttacker"
    }

    fn id(&self) -> MethodId {
        MethodId::ENGAGE_RESCUE_ATTACKER
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
    agent_tile: (i32, i32, i8),
    chunk_map: &ChunkMap,
    chunk_graph: &ChunkGraph,
    chunk_connectivity: &ChunkConnectivity,
) -> Option<(i32, i32)> {
    for _ in 0..8 {
        let dx = fastrand::i32(-96..=96);
        let dy = fastrand::i32(-96..=96);
        let tx = (home.0 + dx).max(0);
        let ty = (home.1 + dy).max(0);
        let to_z = chunk_map.surface_z_at(tx, ty) as i8;
        if chunk_connectivity.tile_reachable(chunk_graph, agent_tile, (tx, ty, to_z)) {
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
        Without<Drafted>,
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

            // A Sleep task is already live — this dispatcher only does the
            // *initial* plan+route+dispatch. Its entire downstream lifecycle
            // (arrival `Working`→`Sleeping` flip, recovery, retirement, and
            // orphan recovery when an external preempt resets `ai.state`) is
            // owned by `sleep::sleep_task_system` (Sequential), keyed on the
            // typed `Task::Sleep` rather than on `ai.state`. Re-planning here
            // while `current == Sleep` is exactly the desync the
            // `ActionQueue::dispatch` assert guards against (an external
            // resetter could leave `state == Idle` with `current == Sleep`
            // mid-flight), so never touch an agent whose Sleep task is live —
            // subsumes the old Sleeping / in-flight / arrival guards.
            if aq.current_task_kind() == TaskKind::Sleep as u16 {
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
                scope: ScoringScope::Geometric,
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
                claimed_blueprint_tile: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
                scavenge_food_good: None,
                gather_deposit_tile: None,
                scavenge_deposit_tile: None,
                forage_food_good: None,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
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
                Task::Sleep {
                    bed: Some(bed_entity),
                } => {
                    if let Some(bed_tile) = home_bed_tile {
                        let routed = assign_task_with_routing(
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
                        if routed {
                            aq.dispatch(Task::Sleep {
                                bed: Some(bed_entity),
                            });
                        } else {
                            // Bed unreachable (walled in, sealed by blueprints,
                            // wrong connectivity component). Falling back to
                            // in-place sleep keeps the agent's AiState in sync
                            // with the dispatched task — without this, the
                            // routing failure leaves AiState::Idle and the
                            // next tick re-dispatches Task::Sleep, piling up
                            // in the prefetch ring until it overflows.
                            ai.state = AiState::Sleeping;
                            aq.dispatch(Task::Sleep { bed: None });
                        }
                    } else {
                        // Defensive: the method already filters bed by
                        // home_bed_tile.is_some(), so this branch shouldn't
                        // fire. If it ever does (e.g. a future method that
                        // skips the filter), drop to in-place to avoid a
                        // null-route panic.
                        ai.state = AiState::Sleeping;
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
                            let routed = assign_task_with_routing(
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
                            if routed {
                                aq.dispatch(Task::Sleep { bed: None });
                            } else {
                                // Home unreachable from here. Sleep in place
                                // so AiState matches the dispatched task and
                                // the next tick doesn't re-dispatch Sleep.
                                ai.state = AiState::Sleeping;
                                aq.dispatch(Task::Sleep { bed: None });
                            }
                            return;
                        }
                    }
                    // Solo, no home, or already at home with no bed: sleep
                    // here.
                    ai.state = AiState::Sleeping;
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
        ),
        Without<Drafted>,
    >,
) {
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, goal, needs, agent, carrier, transform, member, lod)| {
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
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
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
                scope: ScoringScope::Geometric,
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
                claimed_blueprint_tile: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
                scavenge_food_good: None,
                gather_deposit_tile: None,
                scavenge_deposit_tile: None,
                forage_food_good: None,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
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
            let chosen_id = method.id();
            // Phase 6b-ii: stamp active method for chain-completion success
            // recording. Eat is a single-task chain with no failure paths
            // beyond the empty-expansion edge case.
            ai.active_method = Some(chosen_id);
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                ai.active_method = None;
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
                    ai.work_progress = 0;
                    aq.dispatch(Task::Eat);
                }
                _ => {
                    // No registered Eat method returns a non-Eat head today.
                    // Defensive: leave the agent untouched so the next tick
                    // re-runs dispatch.
                    ai.active_method = None;
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
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    plant_map: Res<PlantMap>,
    gather_claims: Res<GatherClaims>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    item_query: Query<&crate::simulation::items::GroundItem>,
    plant_query: Query<&Plant>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Needs,
            &EconomicAgent,
            &Carrier,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            &crate::simulation::memory::CurrentVision,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    query.par_iter_mut().for_each(
        |(
            actor,
            mut ai,
            mut aq,
            mut history,
            goal,
            needs,
            agent,
            carrier,
            transform,
            member,
            lod,
            household_member,
            current_vision,
        )| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::Survive) {
                return;
            }

            // Same gating as `htn_eat_dispatch_system`: don't preempt an
            // in-flight plan, only fire on a clean (Idle, UNEMPLOYED) slot.
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
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

            // Phase 2a: reachability-aware storage pick. Skips storage tiles
            // whose chunk isn't reachable from the agent's chunk so the
            // dispatcher doesn't burn a tick on an unroutable target before
            // the failure path biases the method.
            // **Correction:** Filter storage tiles to ensure they actually
            // contain edible items, preventing a loop where agents walk to
            // a seed-only tile under Survive.
            let nearest_storage_tile = if let Some(tiles) =
                storage_tile_map.by_faction.get(&member.faction_id)
            {
                let agent_tile_3d = (cur_tx, cur_ty, ai.current_z);
                let pick = |reachable_only: bool| {
                    tiles
                        .iter()
                        .filter(|&&(tx, ty)| {
                            if reachable_only {
                                let tz = chunk_map.nearest_standable_z(tx, ty, ai.current_z as i32)
                                    as i8;
                                if !chunk_connectivity.tile_reachable(
                                    &chunk_graph,
                                    agent_tile_3d,
                                    (tx, ty, tz),
                                ) {
                                    return false;
                                }
                            }

                            // Ensure at least one edible item exists on this tile
                            spatial.get(tx, ty).iter().any(|&e| {
                                if let Ok(gi) = item_query.get(e) {
                                    gi.item.resource_id.is_edible() && gi.qty > 0
                                } else {
                                    false
                                }
                            })
                        })
                        .min_by_key(|&&(tx, ty)| (tx - cur_tx).abs() + (ty - cur_ty).abs())
                        .copied()
                };
                pick(true).or_else(|| pick(false))
            } else {
                None
            };
            // `food_stock` returns f32 because it sums Fruit/Meat/Grain at
            // floating-point granularity in some legacy code; for ctx purposes
            // we want a u32 tally. Floor the value — under-counting is the
            // safer side for the precondition gate.
            let faction_food_stock = faction_registry.food_stock(member.faction_id) as u32;

            // Vision-first scavenge target: nearest visible loose edible
            // GroundItem within the agent's current vision (LOS-checked by
            // `vision_system`), excluding storage tiles so the agent doesn't
            // try to "scavenge" their own deposit.
            // Phase 2a: build a tile-reachability closure once and pass it to
            // every vision-picker so we don't pin a target in a disconnected
            // chunk only to fail at routing time. Two-pass design inside the
            // pickers falls back to the connectivity-blind result if every
            // candidate is disconnected.
            let reach_from_agent = |t: (i32, i32)| -> bool {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                chunk_connectivity.tile_reachable(
                    &chunk_graph,
                    (cur_tx, cur_ty, ai.current_z),
                    (t.0, t.1, tz),
                )
            };
            // Detour-aware (river-aware) distance from the agent: a target
            // across a river costs the walk-around, not the straight line.
            let detour_est =
                crate::pathfinding::detour::DetourEstimator::new(&chunk_router, &chunk_graph);
            let detour_dist = |t: (i32, i32)| -> i32 {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                detour_est.tiles((cur_tx, cur_ty), ai.current_z, t, tz)
            };
            let scavenge = current_vision.nearest_scavenge_target(
                MemoryKind::AnyEdible,
                detour_dist,
                |t| storage_tile_map.tiles.contains_key(&t),
                reach_from_agent,
            );
            let scavenge_target_entity = scavenge.map(|(e, _)| e);
            let scavenge_target_tile = scavenge.map(|(_, t)| t);

            // Vision-first forage candidate: nearest visible mature edible
            // plant. Vision_system only writes mature-stage edible plants, so
            // no extra stage filter needed. Falls back to SharedKnowledge
            // when vision shows nothing.
            let viewer_household = household_member.map(|h| h.household_id);
            let viewer_settlement = gk.settlement_map.first_for_faction(member.faction_id);
            // P6a: live `PlantMap` fast path. Probes for a mature
            // edible plant within chebyshev radius 2 *before* vision /
            // SharedKnowledge — vision runs once per ~20-tick bucket
            // and SharedKnowledge requires a reported sighting, so an
            // agent that walked onto a wheat tile this tick may not
            // see it via either lookup yet. Skips the probe hit if
            // another agent already pressured the tile so the cluster
            // mutex still spreads workers.
            let underfoot = nearest_mature_plant_under_agent(
                &plant_map,
                &plant_query,
                |k| matches!(k, PlantKind::Grain | PlantKind::BerryBush),
                (cur_tx, cur_ty),
                2,
            )
            .filter(|(t, _)| gather_claims.pressure(*t, now, actor) == 0)
            .map(|(t, _)| t);
            let visible_forage = current_vision.nearest_gather_target(
                MemoryKind::AnyEdible,
                detour_dist,
                actor,
                viewer_household,
                viewer_settlement,
                member.faction_id,
                |t| gather_claims.pressure(t, now, actor) * 4,
                reach_from_agent,
            );
            let gather_target_tile = underfoot.or(visible_forage).or_else(|| {
                gk.nearest_target_tile(
                    actor,
                    member.faction_id,
                    viewer_household,
                    MemoryKind::AnyEdible,
                    (cur_tx, cur_ty),
                    ai.current_z,
                    now,
                )
                .and_then(|tile| {
                    let entity = plant_map.0.get(&tile).copied()?;
                    let plant = plant_query.get(entity).ok()?;
                    if plant.stage != GrowthStage::Mature {
                        return None;
                    }
                    Some(tile)
                })
            });

            let ctx = PlannerCtx {
                scope: context_aware_scope(&calendar, needs),
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
                claimed_blueprint_tile: None,
                gather_target_tile,
                scavenge_target_entity,
                scavenge_target_tile,
                scavenge_food_good: None,
                gather_deposit_tile: None,
                scavenge_deposit_tile: None,
                // AcquireFood's forage chain ends in `Eat`, not
                // `DepositToFactionStorage`, so no good payload is needed.
                forage_food_good: None,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
            };

            let abstract_task = AbstractTask::AcquireFood;
            let methods = method_registry.methods_for(AbstractTaskKind::AcquireFood);
            // Phase 6b: scoring goes through `score_method_with_history` so
            // recent routing failures bias the argmax. Sibling methods that
            // haven't failed get a free pass; a method with two recent
            // failures eats a `2 * METHOD_FAILURE_PENALTY` discount.
            let chosen = methods
                .iter()
                .filter(|m| m.precondition(abstract_task, &ctx))
                .max_by(|a, b| {
                    let ua =
                        score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                    let ub =
                        score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                    ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                });

            let Some(method) = chosen else {
                // Phase 3 terminal Explore fallback: every AcquireFood
                // method's precondition failed (no inventory, no storage,
                // no scavenge target, no known forage tile). Rather than
                // standing idle for another goal-update cycle, walk
                // somewhere new in the hope of recording a fresh
                // `AnyEdible` sighting. Stamps `MethodId::TERMINAL_EXPLORE`
                // so chronic terminal-fallback failures are observable in
                // `MethodHistory` and later phases can escalate.
                let home = faction_registry
                    .home_tile(member.faction_id)
                    .unwrap_or((cur_tx, cur_ty));
                if let Some(dest) = pick_explore_tile(
                    home,
                    (cur_tx, cur_ty, ai.current_z),
                    &chunk_map,
                    &chunk_graph,
                    &chunk_connectivity,
                ) {
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
                    if dispatched {
                        ai.active_method = Some(MethodId::TERMINAL_EXPLORE);
                        aq.dispatch(Task::Explore {
                            kind: MemoryKind::AnyEdible,
                        });
                    } else {
                        history.push(
                            MethodId::TERMINAL_EXPLORE,
                            MethodOutcome::FailedRouting,
                            now,
                        );
                    }
                }
                return;
            };
            let chosen_id = method.id();
            // Phase 6b-ii: stamp the active method so `htn_method_completion_system`
            // can record `MethodOutcome::Success` when the chain naturally
            // drains to `Task::Idle`. Failure paths below clear it before
            // pushing the explicit failure outcome.
            ai.active_method = Some(chosen_id);
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                ai.active_method = None;
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
                        // adjacent standable). Record the failure so the next
                        // tick's argmax biases away from this method (Phase
                        // 6b: `score_method_with_history` reads `history` and
                        // applies `METHOD_FAILURE_PENALTY` per recent miss).
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedTarget, now);
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                        (cur_tx, cur_ty, ai.current_z),
                        &chunk_map,
                        &chunk_graph,
                        &chunk_connectivity,
                    ) else {
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    aq.dispatch(Task::Explore { kind });
                }
                Task::Gather { tile: gather_tile } => {
                    // Forage dispatch under AcquireFood. The trailing leg is
                    // `Task::Eat` (in-place); `finish_gather` primes the
                    // legacy `task_id = Eat` channel when the prefetch ring
                    // promotes it, mirroring `finish_withdraw_food`.
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    let kind = MemoryKind::AnyEdible;
                    gather_claims.add(
                        gather_tile,
                        kind,
                        actor,
                        suggested_expiry(now, (cur_tx, cur_ty), gather_tile),
                    );
                    ai.active_gather_claim = Some((gather_tile, kind));
                    aq.dispatch(Task::Gather { tile: gather_tile });
                }
                _ => {
                    // No registered AcquireFood method returns a non-WithdrawFood,
                    // non-Scavenge, non-Explore, non-Gather head today.
                    // Defensive fallthrough; future Hunt methods will land
                    // as new arms here.
                    ai.active_method = None;
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
/// task slot, and a populated `AgentMemory::best_for(MemoryKind::Resource(WOOD|STONE))`:
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
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    gather_claims: Res<GatherClaims>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    item_query: Query<&crate::simulation::items::GroundItem>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            Option<&crate::simulation::jobs::ClaimTarget>,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            &crate::simulation::memory::CurrentVision,
            &crate::simulation::carry::Carrier,
            &crate::economy::agent::EconomicAgent,
            &Needs,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::jobs::JobKind;

    let now = clock.tick;
    for (
        actor,
        mut ai,
        mut aq,
        mut history,
        goal,
        member,
        transform,
        lod,
        claim_target_opt,
        job_claim_opt,
        household_member,
        current_vision,
        carrier,
        agent_econ,
        needs,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
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
            AgentGoal::GatherWood | AgentGoal::GatherStone | AgentGoal::Stockpile => {
                // Phase 5e-xiv: `Stockpile` is the generalized counterpart to
                // GatherWood/GatherStone — the specific resource lives on the
                // `ClaimTarget.resource_id` companion of the agent's
                // `JobKind::Stockpile` claim. Without a claim target the
                // dispatcher silently skips (the agent stays Idle and
                // `goal_update_system` will reassign the goal next tick).
                let (target_rid, memory_kind): (
                    crate::economy::resource_catalog::ResourceId,
                    MemoryKind,
                ) = match *goal {
                    AgentGoal::GatherWood => (crate::economy::core_ids::wood(), MemoryKind::wood()),
                    AgentGoal::GatherStone => {
                        (crate::economy::core_ids::stone(), MemoryKind::stone())
                    }
                    AgentGoal::Stockpile => {
                        let Some(rid) = claim_target_opt.and_then(|c| c.resource_id()) else {
                            continue;
                        };
                        (rid, MemoryKind::Resource(rid))
                    }
                    _ => unreachable!(),
                };

                let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                let cur_chunk = ChunkCoord(
                    cur_tx.div_euclid(CHUNK_SIZE as i32),
                    cur_ty.div_euclid(CHUNK_SIZE as i32),
                );

                // Vision-first: prefer a target the agent can see right now
                // over a remembered one. Vision is refreshed by `vision_system`
                // once per agent's bucket (~1s), with LOS already enforced.
                // Memory is only consulted when vision shows nothing of the
                // requested kind.
                let viewer_household = household_member.map(|h| h.household_id);
                let viewer_settlement = gk.settlement_map.first_for_faction(member.faction_id);
                // Phase 2a: tile-reachability closure for vision pickers.
                let reach_from_agent = |t: (i32, i32)| -> bool {
                    let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                    chunk_connectivity.tile_reachable(
                        &chunk_graph,
                        (cur_tx, cur_ty, ai.current_z),
                        (t.0, t.1, tz),
                    )
                };
                let detour_est = crate::pathfinding::detour::DetourEstimator::new(
                    &chunk_router,
                    &chunk_graph,
                );
                let detour_dist = |t: (i32, i32)| -> i32 {
                    let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                    detour_est.tiles((cur_tx, cur_ty), ai.current_z, t, tz)
                };
                let visible_gather = current_vision.nearest_gather_target(
                    memory_kind,
                    detour_dist,
                    actor,
                    viewer_household,
                    viewer_settlement,
                    member.faction_id,
                    |t| gather_claims.pressure(t, now, actor) * 4,
                    reach_from_agent,
                );
                let gather_target_tile = visible_gather.or_else(|| {
                    gk.nearest_target_tile(
                        actor,
                        member.faction_id,
                        viewer_household,
                        memory_kind,
                        (cur_tx, cur_ty),
                        ai.current_z,
                        now,
                    )
                });

                // Visible loose-item scavenge target — the buffer already
                // mirrors what the previous in-dispatcher SpatialIndex scan
                // produced (LOS-checked sightings of GroundItems), with the
                // storage-tile exclusion applied at read time.
                let scavenge = current_vision.nearest_scavenge_target(
                    memory_kind,
                    detour_dist,
                    |t| storage_tile_map.tiles.contains_key(&t),
                    reach_from_agent,
                );
                let scavenge_target_entity = scavenge.map(|(e, _)| e);
                let scavenge_target_tile = scavenge.map(|(_, t)| t);

                // Phase 5c-ii-d-iv-ii: no early-return when both targets are
                // None. The argmax now picks `ExploreForMaterialMethod`
                // (utility 0.3) as the fallback when no concrete method's
                // precondition fires — replaces the legacy
                // `ExploreForWood`/`ExploreForStone` plan path that this PR
                // deletes from the registry.

                // Phase 2a: reachability-aware deposit picks for AcquireGood —
                // pick a storage tile reachable from the gather/scavenge tile
                // so the dispatcher doesn't bake an unroutable trailing leg
                // into the chain.
                let gather_deposit_tile = gather_target_tile.and_then(|t| {
                    let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                    storage_tile_map.nearest_for_faction_reachable(
                        member.faction_id,
                        t,
                        (t.0, t.1, tz),
                        &chunk_map,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_connectivity,
                    )
                });
                let scavenge_deposit_tile = scavenge_target_tile.and_then(|t| {
                    let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                    storage_tile_map.nearest_for_faction_reachable(
                        member.faction_id,
                        t,
                        (t.0, t.1, tz),
                        &chunk_map,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_connectivity,
                    )
                });
                // Time-of-day + fatigue scoring is enabled for wood and
                // stone gathering (the user-requested resources). Other
                // Stockpile resources (Cloth, Skin, Tools, …) keep the
                // legacy geometric penalty so this PR doesn't shift their
                // ranking.
                let scope_for_branch = if target_rid == crate::economy::core_ids::wood()
                    || target_rid == crate::economy::core_ids::stone()
                {
                    context_aware_scope(&calendar, needs)
                } else {
                    ScoringScope::Geometric
                };
                let ctx = PlannerCtx {
                    scope: scope_for_branch,
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
                    claimed_blueprint_tile: None,
                    gather_target_tile,
                    scavenge_target_entity,
                    scavenge_target_tile,
                    scavenge_food_good: None,
                    gather_deposit_tile,
                    scavenge_deposit_tile,
                    forage_food_good: None,
                    butcher_site_tile: None,
                    prey_target_entity: None,
                    prey_target_tile: None,
                    fresh_corpse_entity: None,
                    fresh_corpse_tile: None,
                    hunt_hearth_tile: None,
                    hunt_area_tile: None,
                    hunt_party_deployed: false,
                    hunt_party_stale: false,
                    target_craft_order: None,
                    craft_output_resource: None,
                    play_partner_entity: None,
                    play_solo_eligible: false,
                    play_stone_storage_tile: None,
                    play_toy_storage_tile: None,
                    play_toy_resource: None,
                    play_grain_seed_storage_tile: None,
                    play_berry_seed_storage_tile: None,
                    play_plant_destination_tile: None,
                    personal_bp_resource: None,
                    agent_has_weapon: false,
                    deposit_target_faction_override: None,
                };

                let abstract_task = AbstractTask::AcquireGood {
                    resource_id: target_rid,
                };
                let methods = method_registry.methods_for(AbstractTaskKind::AcquireGood);
                let chosen = methods
                    .iter()
                    .filter(|m| m.precondition(abstract_task, &ctx))
                    .max_by(|a, b| {
                        let ua = score_method_with_history(
                            a.as_ref(),
                            abstract_task,
                            &ctx,
                            &history,
                            now,
                        );
                        let ub = score_method_with_history(
                            b.as_ref(),
                            abstract_task,
                            &ctx,
                            &history,
                            now,
                        );
                        ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                    });
                let Some(method) = chosen else { continue };
                let chosen_id = method.id();
                // Phase 6b-ii: stamp active method for chain-completion
                // success recording; failure paths clear it explicitly.
                ai.active_method = Some(chosen_id);
                let mut tasks = method.expand(abstract_task, &ctx);
                if tasks.is_empty() {
                    ai.active_method = None;
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
                            ai.active_method = None;
                            history.push(chosen_id, MethodOutcome::FailedRouting, now);
                            continue;
                        }
                        gather_claims.add(
                            gather_tile,
                            memory_kind,
                            actor,
                            suggested_expiry(now, (cur_tx, cur_ty), gather_tile),
                        );
                        ai.active_gather_claim = Some((gather_tile, memory_kind));
                        aq.dispatch(Task::Gather { tile: gather_tile });
                    }
                    Task::Scavenge { target } => {
                        let Some(scav_tile) = scavenge_target_tile else {
                            ai.active_method = None;
                            history.push(chosen_id, MethodOutcome::FailedTarget, now);
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
                            ai.active_method = None;
                            history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                            (cur_tx, cur_ty, ai.current_z),
                            &chunk_map,
                            &chunk_graph,
                            &chunk_connectivity,
                        ) else {
                            ai.active_method = None;
                            history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                            ai.active_method = None;
                            history.push(chosen_id, MethodOutcome::FailedRouting, now);
                            continue;
                        }
                        aq.dispatch(Task::Explore { kind });
                    }
                    _ => {
                        // No registered AcquireGood method returns a
                        // non-Gather, non-Scavenge, non-Explore head under
                        // the gather branch today. Defensive fallthrough.
                        ai.active_method = None;
                        continue;
                    }
                }

                // Push the trailing `Task::DepositToFactionStorage { good, target_faction_id: None }`
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
        let (Some(resource_id), Some(blueprint)) = (target.resource_id(), target.blueprint) else {
            continue;
        };

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Fix 3a: in-hand fast-path. If the agent is already carrying the
        // resource (in-hand or in-inventory) and the bp's slot still needs
        // ≥1 of it, skip the storage round-trip and walk straight to the bp
        // to deposit. Scoped strictly to dispatchers that already hold a
        // JobClaim::Haul — never flows back into posting creation. This
        // avoids the redundant Withdraw→walk→Withdraw cycle when an agent
        // ends up with material in hand from a prior interrupted chain.
        let in_hand = carrier
            .quantity_of_resource(resource_id)
            .saturating_add(agent_econ.quantity_of_resource(resource_id));
        if in_hand > 0 {
            let bp_needs_more = bp_query
                .get(blueprint)
                .map(|bp| !bp.slot_satisfied(resource_id))
                .unwrap_or(false);
            if let (Ok(bp), true) = (bp_query.get(blueprint), bp_needs_more) {
                let bp_tile = bp.worker_target_tile();
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    bp_tile,
                    TaskKind::HaulMaterials,
                    Some(blueprint),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if dispatched {
                    aq.dispatch(Task::HaulToBlueprint { blueprint });
                    // No active method to record — direct dispatch bypasses
                    // the Method/MethodHistory machinery.
                    ai.active_method = None;
                    continue;
                }
                // Routing failed — fall through to the standard withdraw
                // chain so the agent can re-route via storage if reachable.
            }
        }

        // Step 5: Market-haul direct dispatch. When the claim's snapshotted
        // `HaulSource` is `Market`, the worker buys at the faction's market
        // node instead of withdrawing from (empty) storage. Mirrors the
        // in-hand fast-path: direct `aq.dispatch` bypassing the Method
        // registry (avoids 55-site PlannerCtx churn for a special case).
        if let Some(crate::simulation::jobs::HaulSource::Market { .. }) =
            claim_target_opt.and_then(|t| t.haul_source)
        {
            let market = faction_registry
                .factions
                .get(&member.faction_id)
                .and_then(|f| f.procurement_market);
            if let Some((node, market_tile)) = market {
                let bp_needs_more = bp_query
                    .get(blueprint)
                    .map(|bp| !bp.slot_satisfied(resource_id))
                    .unwrap_or(false);
                if bp_needs_more {
                    let dispatched = assign_task_with_routing(
                        &mut ai,
                        (cur_tx, cur_ty),
                        cur_chunk,
                        market_tile,
                        TaskKind::BuyMaterialAtMarket,
                        None,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if dispatched {
                        aq.dispatch(Task::BuyMaterialAtMarket {
                            resource_id,
                            qty: 1,
                            node,
                        });
                        let _ = aq.enqueue(Task::HaulToBlueprint { blueprint });
                        ai.active_method = None;
                        continue;
                    }
                }
            }
            // Market claim but node unresolved / routing failed / bp already
            // satisfied: skip this tick (chronic-failure release eventually
            // frees the claim if it never resolves). No storage fallback —
            // Market hauls are posted precisely because storage is empty.
            continue;
        }

        // Faction-level stock check — mirrors `WithdrawAndHaulToBlueprintMethod`'s
        // precondition gate. Skipping early when the faction has no stock at
        // all avoids touching `SpatialIndex` for every tile on a dry larder.
        let stock = faction_registry
            .factions
            .get(&member.faction_id)
            .and_then(|f| f.storage.totals.get(&resource_id).copied())
            .unwrap_or(0);
        if stock == 0 {
            continue;
        }

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
                    if gi.item.resource_id == resource_id && gi.qty > 0 {
                        tile_stock = tile_stock.saturating_add(gi.qty);
                    }
                }
            }
            let reserved = storage_reservations.get((tx, ty), resource_id);
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
            scope: ScoringScope::Geometric,
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
            // Phase 5c-ii-d-vii: feed the blueprint tile into ctx so
            // `WithdrawAndHaulToBlueprintMethod`'s utility can discount on the
            // *full* storage→blueprint trip rather than just the agent→storage
            // hop. A despawned blueprint silently degrades to `None` (the
            // method falls back to its prior storage-only signal).
            claimed_blueprint_tile: bp_query.get(blueprint).ok().map(|bp| bp.tile),
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::AcquireGood { resource_id };
        let methods = method_registry.methods_for(AbstractTaskKind::AcquireGood);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else { continue };
        let chosen_id = method.id();
        // Phase 6b-ii: stamp active method for chain-completion success
        // recording; failure paths clear it explicitly.
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);

        match head {
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
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
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                // Reserve the qty against the chosen tile so a parallel
                // dispatch in the same tick sees a smaller effective stock.
                // Mirrors `plan_execution_system`'s WithdrawMaterial dispatch
                // site (`plan/mod.rs:2724`).
                let reserved_tile = (storage_tile.0, storage_tile.1);
                storage_reservations.add(reserved_tile, head_resource, qty as u32);
                ai.reserved_tile = reserved_tile;
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            _ => {
                // No registered AcquireGood method returns a non-WithdrawMaterial
                // head today. Defensive fallthrough.
                ai.active_method = None;
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

/// Phase 5c-ii-d-vi dispatcher. Owns `AgentGoal::GatherFood` end-to-end via
/// the `StockpileFood` abstract task — the chief-driven counterpart to
/// `htn_acquire_food_dispatch_system`'s Survive case. Replaces the
/// `AgentGoal::GatherFood` half of the legacy `ScavengeFood` (PlanId 6) and
/// `ExploreForFood` (PlanId 35) plans, both retired by this PR.
///
/// The shape mirrors `htn_acquire_food_dispatch_system` minus the hunger-gate
/// pre-filter and the food-on-hand split: chiefs want the chain to fire
/// regardless of the worker's personal hunger or larder. For each
/// non-Drafted, non-PlayerOrder, non-SOLO `AgentGoal::GatherFood` agent
/// without an `ActivePlan` and idle task slot:
///
/// 1. Scan `SpatialIndex` within `VIEW_RADIUS=15` for a visible loose edible
///    `GroundItem` (excluding faction storage tiles), recording the nearest's
///    entity, tile, and good. Same scan pattern as
///    `htn_acquire_food_dispatch_system`'s 5c-ii-d-iii-ii branch but the
///    picked good threads through to the trailing deposit.
/// 2. Build a `PlannerCtx { scavenge_target_entity, scavenge_target_tile,
///    scavenge_food_good, .. }` and argmax over `methods_for(StockpileFood)`.
///    Today: `ScavengeFoodForStorageMethod` (1.5) wins on visibility;
///    `ExploreForFoodForStorageMethod` (0.3) is the fallback.
/// 3. Route the head `Task::Scavenge { target }` (or `Task::Explore { kind }`)
///    via `assign_task_with_routing` and `aq.dispatch` it. Push the trailing
///    `Task::DepositToFactionStorage { good, target_faction_id: None }` (if any) onto the prefetch
///    ring.
///
/// The chain handoff is shared with the AcquireGood scavenge branch:
/// `item_pickup_system::finish_scavenge` already routes
/// `Task::DepositToFactionStorage` to the nearest faction storage tile and
/// primes `TaskKind::DepositResource`. The `drop_items_at_destination_system`
/// executor already handles edible inventory deposit above `CAMP_KEEP`, so
/// food landing in either hands or inventory flows correctly.
pub fn htn_stockpile_food_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    plant_map: Res<PlantMap>,
    gather_claims: Res<GatherClaims>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    item_query: Query<&crate::simulation::items::GroundItem>,
    plant_query: Query<&Plant>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::jobs::ClaimTarget>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            &crate::simulation::memory::CurrentVision,
            &Profession,
        ),
        (
            Without<Drafted>,
            // Subsistence stockpile is the lowest-priority autonomous
            // work an agent can have — never preempt specialised mid-
            // task state. Corpse carriers, in-flight nomad migrators,
            // and active traders all have their own dispatchers that
            // should fire instead. Hunters mid-hunt are gated by goal
            // and the specialised dispatchers; bureaucrats are filtered
            // by profession inside the closure (no `Without<value>` in
            // Bevy queries).
            Without<crate::simulation::corpse::Carrying>,
            Without<crate::simulation::nomad::MigrationTarget>,
            Without<crate::simulation::person::TraderPlan>,
        ),
    >,
) {
    use crate::simulation::jobs::JobKind;

    let now = clock.tick;
    query.par_iter_mut().for_each(
        |(
            actor,
            mut ai,
            mut aq,
            mut history,
            goal,
            transform,
            member,
            lod,
            job_claim_opt,
            claim_target_opt,
            household_member,
            current_vision,
            profession,
        )| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::GatherFood) {
                return;
            }
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                return;
            }
            if member.faction_id == SOLO {
                return;
            }
            // Specialised professions have their own dispatchers
            // (bureaucrat_admin, trader_route, hunter pipeline) that
            // route them to non-subsistence work. Don't divert them
            // into autonomous food gathering.
            if matches!(*profession, Profession::Bureaucrat | Profession::Trader) {
                return;
            }

            // `AgentGoal::GatherFood` covers two regimes:
            //   1. **Subsistence reflex** — `goal_update_system`'s autonomous
            //      fallback assigns this when `prioritize_food` (faction food
            //      ratio < 1.0). The worker is stockpiling for their own
            //      household / village, not fulfilling an economic contract.
            //      No `JobClaim` involved; the trailing
            //      `DepositToFactionStorage` lands the haul in their own
            //      storage (household first via Phase 2 routing, else village).
            //   2. **Chief / household / self-post coordination** — agent
            //      holds a `JobClaim::Stockpile{food}` (mapped to
            //      `GatherFood` by `job_goal_lock_system` via `posting_goal`).
            // When a claim *is* present, validate its shape — wrong-kind /
            // non-food claims belong on the AcquireGood path. When absent,
            // proceed: this is subsistence work, not a category error.
            if let Some(claim) = job_claim_opt {
                if claim.kind != JobKind::Stockpile {
                    return;
                }
                let claim_is_food = claim_target_opt.map(|t| t.is_food()).unwrap_or(false);
                if !claim_is_food {
                    return;
                }
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            // Vision-first scavenge target: nearest visible loose edible
            // GroundItem within the agent's current vision (LOS-checked by
            // `vision_system`), excluding storage tiles. Resolve the good's
            // resource id from `item_query` so the trailing deposit carries
            // the right payload.
            // Phase 2a: tile-reachability closure for vision pickers.
            let reach_from_agent = |t: (i32, i32)| -> bool {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                chunk_connectivity.tile_reachable(
                    &chunk_graph,
                    (cur_tx, cur_ty, ai.current_z),
                    (t.0, t.1, tz),
                )
            };
            let detour_est =
                crate::pathfinding::detour::DetourEstimator::new(&chunk_router, &chunk_graph);
            let detour_dist = |t: (i32, i32)| -> i32 {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                detour_est.tiles((cur_tx, cur_ty), ai.current_z, t, tz)
            };
            let scavenge = current_vision.nearest_scavenge_target(
                MemoryKind::AnyEdible,
                detour_dist,
                |t| storage_tile_map.tiles.contains_key(&t),
                reach_from_agent,
            );
            let (scavenge_target_entity, scavenge_target_tile, scavenge_food_good) =
                if let Some((entity, tile)) = scavenge {
                    let good = item_query.get(entity).ok().map(|gi| gi.item.resource_id);
                    (Some(entity), Some(tile), good)
                } else {
                    (None, None, None)
                };

            // Subsistence deposit routing: when this dispatch is autonomous
            // (no claim) and the agent belongs to a household sub-faction
            // that owns its own storage tile (Market preset's
            // `seed_market_households`), prefer that storage — the worker is
            // filling their own larder, not the village granary. Chief- /
            // household-claimed work falls through to `member.faction_id`,
            // matching the existing posting semantics (the posting's faction
            // is whoever owns the granary the contract is filling).
            let deposit_faction_id = {
                let hid_opt = household_member.map(|h| h.household_id);
                if job_claim_opt.is_none() {
                    match hid_opt {
                        Some(hid) if storage_tile_map.by_faction.contains_key(&hid) => hid,
                        _ => member.faction_id,
                    }
                } else {
                    member.faction_id
                }
            };

            // Phase 2a: reachability-aware deposit pick — `t` is the scavenge
            // tile the agent will be on after pickup; from there, find the
            // nearest *reachable* storage tile.
            let scavenge_deposit_tile = scavenge_target_tile.and_then(|t| {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                storage_tile_map.nearest_for_faction_reachable(
                    deposit_faction_id,
                    t,
                    (t.0, t.1, tz),
                    &chunk_map,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_connectivity,
                )
            });

            // P6a: live `PlantMap` fast path — see AcquireFood
            // dispatcher comment. Same rationale: catches plants the
            // agent walked onto this tick that vision /
            // SharedKnowledge haven't yet reported.
            let viewer_household = household_member.map(|h| h.household_id);
            let viewer_settlement = gk.settlement_map.first_for_faction(member.faction_id);
            let underfoot = nearest_mature_plant_under_agent(
                &plant_map,
                &plant_query,
                |k| matches!(k, PlantKind::Grain | PlantKind::BerryBush),
                (cur_tx, cur_ty),
                2,
            )
            .filter(|(t, _)| gather_claims.pressure(*t, now, actor) == 0)
            .and_then(|(tile, entity)| {
                let plant = plant_query.get(entity).ok()?;
                let (id, _) = plant.kind.harvest_yield(false);
                Some((tile, id))
            });
            // Vision-first forage candidate: nearest visible mature edible
            // plant. Vision_system only writes mature-stage plant sightings,
            // so the stage filter is implicit. Falls back to SharedKnowledge
            // when vision shows nothing. Threads the plant's harvest resource
            // through `forage_food_good` so the trailing
            // `Task::DepositToFactionStorage` carries the right payload.
            let visible_forage = current_vision
                .nearest_gather_target(
                    MemoryKind::AnyEdible,
                    detour_dist,
                    actor,
                    viewer_household,
                    viewer_settlement,
                    member.faction_id,
                    |t| gather_claims.pressure(t, now, actor) * 4,
                    reach_from_agent,
                )
                .and_then(|tile| {
                    let entity = plant_map.0.get(&tile).copied()?;
                    let plant = plant_query.get(entity).ok()?;
                    let (id, _) = plant.kind.harvest_yield(false);
                    Some((tile, id))
                });
            let forage_candidate = underfoot.or(visible_forage).or_else(|| {
                gk.nearest_target_tile(
                    actor,
                    member.faction_id,
                    viewer_household,
                    MemoryKind::AnyEdible,
                    (cur_tx, cur_ty),
                    ai.current_z,
                    now,
                )
                .and_then(|tile| {
                    let entity = plant_map.0.get(&tile).copied()?;
                    let plant = plant_query.get(entity).ok()?;
                    if plant.stage != GrowthStage::Mature {
                        return None;
                    }
                    let (id, _) = plant.kind.harvest_yield(false);
                    Some((tile, id))
                })
            });
            let gather_target_tile = forage_candidate.map(|(tile, _)| tile);
            let forage_food_good = forage_candidate.map(|(_, id)| id);
            // Phase 2a: reachability-aware deposit pick. Uses the
            // household-aware `deposit_faction_id` computed above so
            // subsistence-mode harvests land in the worker's own larder.
            let gather_deposit_tile = gather_target_tile.and_then(|t| {
                let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
                storage_tile_map.nearest_for_faction_reachable(
                    deposit_faction_id,
                    t,
                    (t.0, t.1, tz),
                    &chunk_map,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_connectivity,
                )
            });

            let ctx = PlannerCtx {
                scope: ScoringScope::Geometric,
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
                claimed_blueprint_tile: None,
                gather_target_tile,
                scavenge_target_entity,
                scavenge_target_tile,
                scavenge_food_good,
                gather_deposit_tile,
                scavenge_deposit_tile,
                forage_food_good,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
            };

            let abstract_task = AbstractTask::StockpileFood;
            let methods = method_registry.methods_for(AbstractTaskKind::StockpileFood);
            let chosen = methods
                .iter()
                .filter(|m| m.precondition(abstract_task, &ctx))
                .max_by(|a, b| {
                    let ua =
                        score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                    let ub =
                        score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                    ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
                });

            let Some(method) = chosen else {
                // Phase 3 terminal Explore fallback (StockpileFood): every
                // method's precondition failed (no scavengeable, no known
                // forage tile). Walk somewhere new; the next sighting fed
                // by `vision_system` will give the next dispatch tick a
                // concrete target.
                let home = faction_registry
                    .home_tile(member.faction_id)
                    .unwrap_or((cur_tx, cur_ty));
                if let Some(dest) = pick_explore_tile(
                    home,
                    (cur_tx, cur_ty, ai.current_z),
                    &chunk_map,
                    &chunk_graph,
                    &chunk_connectivity,
                ) {
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
                    if dispatched {
                        ai.active_method = Some(MethodId::TERMINAL_EXPLORE);
                        aq.dispatch(Task::Explore {
                            kind: MemoryKind::AnyEdible,
                        });
                    } else {
                        history.push(
                            MethodId::TERMINAL_EXPLORE,
                            MethodOutcome::FailedRouting,
                            now,
                        );
                    }
                }
                return;
            };
            let chosen_id = method.id();
            // Phase 6b-ii: stamp active method for chain-completion success
            // recording; failure paths clear it explicitly.
            ai.active_method = Some(chosen_id);
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                ai.active_method = None;
                return;
            }
            let head = tasks.remove(0);

            match head {
                Task::Scavenge { target } => {
                    let Some(scav_tile) = scavenge_target_tile else {
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedTarget, now);
                        return;
                    };
                    // Pass `target_entity = Some(target)` so
                    // `goal_update_system`'s Scavenge target validation
                    // (`goals.rs:286-293`) doesn't flag the task invalid.
                    // GatherFood has no JobClaim bypass like Stockpile/Wood, so
                    // `goal_update_system` runs the validation arm. Mirrors the
                    // `htn_acquire_food_dispatch_system` Scavenge branch's
                    // target_entity passthrough.
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    aq.dispatch(Task::Scavenge { target });
                }
                Task::Explore { kind } => {
                    let home = faction_registry
                        .home_tile(member.faction_id)
                        .unwrap_or((cur_tx, cur_ty));
                    let Some(dest) = pick_explore_tile(
                        home,
                        (cur_tx, cur_ty, ai.current_z),
                        &chunk_map,
                        &chunk_graph,
                        &chunk_connectivity,
                    ) else {
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    aq.dispatch(Task::Explore { kind });
                }
                Task::Gather { tile: gather_tile } => {
                    // Forage dispatch under StockpileFood. The trailing leg
                    // is `Task::DepositToFactionStorage { good, target_faction_id: None }`; the existing
                    // `finish_gather` exit handoff routes to the nearest
                    // faction storage tile and primes
                    // `TaskKind::DepositResource`.
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    let kind = MemoryKind::AnyEdible;
                    gather_claims.add(
                        gather_tile,
                        kind,
                        actor,
                        suggested_expiry(now, (cur_tx, cur_ty), gather_tile),
                    );
                    ai.active_gather_claim = Some((gather_tile, kind));
                    aq.dispatch(Task::Gather { tile: gather_tile });
                }
                _ => {
                    // No registered StockpileFood method returns a non-Scavenge,
                    // non-Explore, non-Gather head today. Defensive fallthrough.
                    ai.active_method = None;
                    return;
                }
            }

            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// Hunter-only spear-arming dispatcher. Replaces the legacy
/// `AcquireHuntingSpear` plan (PlanId 64). Runs *before* `htn_eat_dispatch_system`
/// so an unarmed hunter prefers fetching their spear over eating — mirrors
/// the legacy plan's bias 5.0 outranking the hunger-driven `EatFromInventory`
/// (bias ≈ 1.0) under shared `Survive` / `GatherFood` arenas. Once the chain
/// is dispatched, the (Idle, UNEMPLOYED) gate on every later HTN dispatcher
/// keeps them from preempting it. `MF_UNINTERRUPTIBLE` semantics for the
/// chain-survives-goal-flip part live on the goal-dispatch reset arms
/// (`tasks::goal_dispatch_system` Survive/GatherFood + WithdrawMaterial / Equip).
///
/// Gates: `Profession::Hunter` + Learned `HUNTING_SPEAR` + agent has no
/// Weapon anywhere (inventory / hands / equipped) + faction has Weapon stock
/// in some storage tile (effective stock after reservations > 0). SOLO
/// agents are skipped (no faction storage).
pub fn htn_equip_hunting_spear_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<SimClock>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Profession,
            &EconomicAgent,
            &Carrier,
            Option<&crate::simulation::items::Equipment>,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
        ),
        Without<Drafted>,
    >,
) {
    let weapon_id = crate::economy::core_ids::weapon();
    let now = clock.tick;
    for (
        mut ai,
        mut aq,
        mut history,
        _goal,
        profession,
        agent,
        carrier,
        equipment_opt,
        transform,
        member,
        lod,
        knowledge_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        // Goal-agnostic: a Hunter under a faction `HuntOrder::Hunt` may have
        // any goal (Lead / Defend / Socialize / Survive / etc.). All that
        // matters is they're an unarmed Hunter with stock available.
        // Profession + tech + idle + weapon-absence gates suffice.
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        if *profession != Profession::Hunter {
            continue;
        }
        let has_spear_tech = knowledge_opt
            .map(|k| k.has_learned(crate::simulation::technology::HUNTING_SPEAR))
            .unwrap_or(false);
        if !has_spear_tech {
            continue;
        }
        // Already-armed check: any Weapon in inventory, hands, or an
        // equipment slot self-deselects the chain. Mirrors the legacy
        // `StepPreconditions::forbids_resource(weapon)` gate plus the
        // plan-level forbids_good check.
        if agent.quantity_of_resource(weapon_id) > 0 || carrier.quantity_of_resource(weapon_id) > 0
        {
            continue;
        }
        if let Some(eq) = equipment_opt {
            if eq.has_resource(weapon_id) {
                continue;
            }
        }

        // Faction-level stock check — short-circuits before the per-tile
        // SpatialIndex walk on a dry armoury.
        let stock = faction_registry
            .factions
            .get(&member.faction_id)
            .and_then(|f| f.storage.totals.get(&weapon_id).copied())
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

        // Walk faction storage tiles to find the nearest one with a Weapon
        // in stock (effective after reservations). Mirrors the per-tile scan
        // in the AcquireGood Haul branch.
        let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) else {
            continue;
        };
        let mut best_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        let mut best_tile_stock: u32 = 0;
        for &(tx, ty) in tiles {
            let mut tile_stock: u32 = 0;
            for &gi_entity in spatial.get(tx, ty) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.item.resource_id == weapon_id && gi.qty > 0 {
                        tile_stock = tile_stock.saturating_add(gi.qty);
                    }
                }
            }
            let reserved = storage_reservations.get((tx, ty), weapon_id);
            let effective = tile_stock.saturating_sub(reserved);
            if effective == 0 {
                continue;
            }
            let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
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
            scope: ScoringScope::Geometric,
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
            claimed_blueprint: None,
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::EquipHuntingSpear;
        let methods = method_registry.methods_for(AbstractTaskKind::EquipHuntingSpear);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
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
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                let reserved_tile = storage_tile;
                storage_reservations.add(reserved_tile, head_resource, qty as u32);
                ai.reserved_tile = reserved_tile;
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Hunter-only scout dispatcher. Replaces the legacy `ScoutForPrey` plan
/// (PlanId 65, single-step `WanderForPrey`). Fires when the agent's
/// `Profession == Hunter`, has Learned `HUNTING_SPEAR`, and the faction's
/// chief has flipped `HuntOrder::Scout` (the chief switches back to `Hunt`
/// the moment any hunter records prey memory, naturally ending the scout).
///
/// Single-method registry — `ScoutForPreyMethod` always wins. The dispatcher
/// expands to a head `Task::Explore { kind: MemoryKind::Prey }`, picks a
/// random reachable tile near faction home (mirrors the legacy
/// `StepTarget::ScoutForPrey` resolver), routes via
/// `assign_task_with_routing(... TaskKind::Explore ...)`, and `aq.dispatch`s
/// the typed task. The `vision_system` writes `MemoryKind::Prey` whenever a
/// hunter sees Wolf/Deer along the way; the chief's next decision cycle
/// picks that up and posts a `Hunt` order, naturally peeling the hunter
/// off scouting via `chief_hunt_order_system`.
///
/// Goal arena: `Survive | GatherFood` — same as the legacy
/// `SURVIVE_AND_GATHER_FOOD_GOALS` arrays. The scout dispatcher runs after
/// `htn_acquire_food_dispatch_system` and `htn_stockpile_food_dispatch_system`
/// so a hunter mid-Scout doesn't preempt their own food-gathering chain.
/// In practice the (Idle, UNEMPLOYED) gate makes ordering moot — the food
/// dispatchers above leave the agent in a non-Idle state once they fire.
pub fn htn_scout_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Profession,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::faction::HuntOrder;
    let now = clock.tick;
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, mut history, goal, profession, transform, member, lod, knowledge_opt)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::Survive | AgentGoal::GatherFood) {
                return;
            }
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                return;
            }
            if *profession != Profession::Hunter {
                return;
            }
            // Tech gate: a hunter without HUNTING_SPEAR Learned shouldn't
            // scout. Mirrors PlanDef::tech_gate on the legacy plan.
            let has_spear = knowledge_opt
                .map(|k| k.has_learned(crate::simulation::technology::HUNTING_SPEAR))
                .unwrap_or(false);
            if !has_spear {
                return;
            }
            // Faction must hold a Scout order. Mirrors the candidate filter
            // gate on `PlanId::SCOUT_FOR_PREY` in `plan_execution_system`.
            let has_scout = matches!(
                faction_registry
                    .factions
                    .get(&member.faction_id)
                    .and_then(|f| f.hunt_order.as_ref()),
                Some(HuntOrder::Scout { .. })
            );
            if !has_scout {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            let ctx = PlannerCtx {
                scope: ScoringScope::Geometric,
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
                claimed_blueprint_tile: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
                scavenge_food_good: None,
                gather_deposit_tile: None,
                scavenge_deposit_tile: None,
                forage_food_good: None,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
            };

            let abstract_task = AbstractTask::Scout;
            let Some(pick) =
                dispatch_for_goal(&method_registry, abstract_task, &ctx, &history, now, None)
            else {
                return;
            };
            let method = pick.method;
            let chosen_id = pick.method_id;
            ai.active_method = Some(chosen_id);
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                ai.active_method = None;
                return;
            }
            let head = tasks.remove(0);
            match head {
                Task::Explore { kind } => {
                    let home = faction_registry
                        .home_tile(member.faction_id)
                        .unwrap_or((cur_tx, cur_ty));
                    let Some(dest) = pick_explore_tile(
                        home,
                        (cur_tx, cur_ty, ai.current_z),
                        &chunk_map,
                        &chunk_graph,
                        &chunk_connectivity,
                    ) else {
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
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
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    aq.dispatch(Task::Explore { kind });
                }
                _ => {
                    // No registered Scout method returns a non-Explore head.
                    ai.active_method = None;
                    return;
                }
            }
            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// `AgentGoal::ReturnCamp` dispatcher. The agent is carrying surplus food
/// from a foraging trip; walk back to the nearest faction storage tile and
/// dump everything in hands + inventory. Single-method registry —
/// `DepositSurplusAtStorageMethod` always wins.
///
/// Mirrors the Haul / hunter-arm dispatchers' shape: scan a faction-side
/// resource (here, the nearest storage tile), find a candidate food good in
/// the agent's hands or inventory to thread through the deposit chain's
/// payload, build a `PlannerCtx` snapshot, argmax over the registered
/// methods, and route the head via `assign_task_with_routing(...
/// TaskKind::DepositResource ...)`. Replaces the legacy `ReturnSurplusFood`
/// plan (PlanId 24) and its single step (StepId 12 DepositGoods).
///
/// SOLO agents are skipped (no faction storage). The chain executes via the
/// existing `drop_items_at_destination_system` (Economy, after movement) so
/// no chain handoff is needed — the deposit is the entire chain.
pub fn htn_return_surplus_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &EconomicAgent,
            &Carrier,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, mut history, goal, agent, carrier, transform, member, lod)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !matches!(*goal, AgentGoal::ReturnCamp) {
                return;
            }
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                return;
            }
            if member.faction_id == SOLO {
                return;
            }

            // Pick any edible the agent is currently carrying — the deposit
            // executor dumps everything regardless, but we thread the actual
            // good through `Task::DepositToFactionStorage` for chain
            // inspectability (mirrors `ScavengeFoodForStorageMethod`).
            let mut surplus_food: Option<crate::economy::resource_catalog::ResourceId> = None;
            if let Some(s) = carrier.left {
                if s.qty > 0 && s.item.resource_id.is_edible() {
                    surplus_food = Some(s.item.resource_id);
                }
            }
            if surplus_food.is_none() {
                if let Some(s) = carrier.right {
                    if s.qty > 0 && s.item.resource_id.is_edible() {
                        surplus_food = Some(s.item.resource_id);
                    }
                }
            }
            if surplus_food.is_none() {
                for (it, q) in agent.inventory.iter() {
                    if *q > 0 && it.resource_id.is_edible() {
                        surplus_food = Some(it.resource_id);
                        break;
                    }
                }
            }
            // No food on agent — `goal_update_system` will flip the goal next
            // tick. Skip so the dispatcher doesn't strand the agent.
            let Some(food_id) = surplus_food else {
                return;
            };

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            let nearest_storage_tile =
                storage_tile_map.nearest_for_faction(member.faction_id, (cur_tx, cur_ty));
            let Some(storage_tile) = nearest_storage_tile else {
                return;
            };

            let ctx = PlannerCtx {
                scope: ScoringScope::Geometric,
                tile: (cur_tx, cur_ty),
                faction_id: member.faction_id,
                faction_home: faction_registry.home_tile(member.faction_id),
                home_bed: None,
                home_bed_tile: None,
                edible_count: 0,
                hunger: 0.0,
                nearest_storage_tile: Some(storage_tile),
                faction_food_stock: 0,
                material_storage_tile: None,
                material_stock_for_target: 0,
                claimed_blueprint: None,
                claimed_blueprint_tile: None,
                gather_target_tile: None,
                scavenge_target_entity: None,
                scavenge_target_tile: None,
                // Reuses `scavenge_food_good` as the "food being deposited"
                // payload — same role as in `ScavengeFoodForStorageMethod`.
                scavenge_food_good: Some(food_id),
                gather_deposit_tile: None,
                scavenge_deposit_tile: None,
                forage_food_good: None,
                butcher_site_tile: None,
                prey_target_entity: None,
                prey_target_tile: None,
                fresh_corpse_entity: None,
                fresh_corpse_tile: None,
                hunt_hearth_tile: None,
                hunt_area_tile: None,
                hunt_party_deployed: false,
                hunt_party_stale: false,
                target_craft_order: None,
                craft_output_resource: None,
                play_partner_entity: None,
                play_solo_eligible: false,
                play_stone_storage_tile: None,
                play_toy_storage_tile: None,
                play_toy_resource: None,
                play_grain_seed_storage_tile: None,
                play_berry_seed_storage_tile: None,
                play_plant_destination_tile: None,
                personal_bp_resource: None,
                agent_has_weapon: false,
                deposit_target_faction_override: None,
            };

            let abstract_task = AbstractTask::ReturnSurplus;
            let Some(pick) =
                dispatch_for_goal(&method_registry, abstract_task, &ctx, &history, now, None)
            else {
                return;
            };
            let method = pick.method;
            let chosen_id = pick.method_id;
            ai.active_method = Some(chosen_id);
            let mut tasks = method.expand(abstract_task, &ctx);
            if tasks.is_empty() {
                ai.active_method = None;
                return;
            }
            let head = tasks.remove(0);
            match head {
                Task::DepositToFactionStorage {
                    resource_id,
                    target_faction_id,
                } => {
                    let dispatched = assign_task_with_routing(
                        &mut ai,
                        (cur_tx, cur_ty),
                        cur_chunk,
                        storage_tile,
                        TaskKind::DepositResource,
                        None,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if !dispatched {
                        ai.active_method = None;
                        history.push(chosen_id, MethodOutcome::FailedRouting, now);
                        return;
                    }
                    aq.dispatch(Task::DepositToFactionStorage {
                        resource_id,
                        target_faction_id,
                    });
                }
                _ => {
                    ai.active_method = None;
                    return;
                }
            }
            for task in tasks {
                let _ = aq.enqueue(task);
            }
        },
    );
}

/// `AgentGoal::TameAnimal` dispatcher. Single-method registry —
/// `TameWildAnimalMethod` always wins. Faction tech-gated per-species at
/// scan time:
///   Horse → `HORSE_TAMING`
///   Cow / Pig → `ANIMAL_HUSBANDRY`
///   Cat → `DOG_DOMESTICATION`
/// Wolves are **not** auto-tamed via this path; "dog-from-wolf" is a deliberate
/// player/chief command in v1.
///
/// Scans `SpatialIndex` within `VIEW_RADIUS=15` for the nearest live untamed
/// candidate of any species the faction is Aware of, snapshots `(entity, tile)`
/// into the shared `scavenge_target_entity`/`scavenge_target_tile` ctx slots,
/// and routes via `assign_task_with_routing(... TaskKind::TameAnimal, ...)`.
/// The executor (`tame_task_system`) re-validates the per-species tech at every
/// tick.
pub fn htn_tame_animal_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<SimClock>,
    wild_horse_q: Query<
        (),
        (
            With<crate::simulation::animals::Horse>,
            Without<crate::simulation::animals::Tamed>,
        ),
    >,
    wild_cow_q: Query<
        (),
        (
            With<crate::simulation::animals::Cow>,
            Without<crate::simulation::animals::Tamed>,
        ),
    >,
    wild_pig_q: Query<
        (),
        (
            With<crate::simulation::animals::Pig>,
            Without<crate::simulation::animals::Tamed>,
        ),
    >,
    wild_cat_q: Query<
        (),
        (
            With<crate::simulation::animals::Cat>,
            Without<crate::simulation::animals::Tamed>,
        ),
    >,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    const VIEW_RADIUS: i32 = 15;
    let now = clock.tick;
    for (mut ai, mut aq, mut history, goal, transform, member, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::TameAnimal) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        // Per-species tech awareness. Skip species the faction can't tame so
        // the candidate scan doesn't waste cycles on doomed targets.
        let (can_horse, can_cattle_pig, can_cat) = faction_registry
            .factions
            .get(&member.faction_id)
            .map(|f| {
                (
                    f.techs.has(crate::simulation::technology::HORSE_TAMING),
                    f.techs.has(crate::simulation::technology::ANIMAL_HUSBANDRY),
                    f.techs
                        .has(crate::simulation::technology::DOG_DOMESTICATION),
                )
            })
            .unwrap_or((false, false, false));
        if !(can_horse || can_cattle_pig || can_cat) {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Scan SpatialIndex for the nearest wild candidate matching any tech the
        // faction has. Chebyshev within VIEW_RADIUS, manhattan tiebreak (the same
        // ranking the legacy horse-only dispatcher used).
        let mut best_target: Option<(Entity, (i32, i32))> = None;
        let mut best_dist = i32::MAX;
        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                let tx = cur_tx + dx;
                let ty = cur_ty + dy;
                for &candidate in spatial.get(tx, ty) {
                    let matches = (can_horse && wild_horse_q.get(candidate).is_ok())
                        || (can_cattle_pig
                            && (wild_cow_q.get(candidate).is_ok()
                                || wild_pig_q.get(candidate).is_ok()))
                        || (can_cat && wild_cat_q.get(candidate).is_ok());
                    if matches {
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best_target = Some((candidate, (tx, ty)));
                        }
                    }
                }
            }
        }
        let Some((horse_entity, horse_tile)) = best_target else {
            continue;
        };

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            // The `scavenge_target_*` ctx slots double as "any entity to walk
            // to and interact with" — the TameWildAnimalMethod reads them.
            scavenge_target_entity: Some(horse_entity),
            scavenge_target_tile: Some(horse_tile),
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::TameWildAnimal;
        let Some(pick) =
            dispatch_for_goal(&method_registry, abstract_task, &ctx, &history, now, None)
        else {
            continue;
        };
        let method = pick.method;
        let chosen_id = pick.method_id;
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::TameAnimal { target } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    horse_tile,
                    TaskKind::TameAnimal,
                    Some(target),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::TameAnimal { target });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Farm-goal dispatcher. Replaces the dead legacy `PlantFromStorage` (PlanId 4,
/// Grain) and `PlantBerryFromStorage` (PlanId 66, BerrySeed) plans, neither of
/// which was ever seeded into any agent's `KnownPlans` — chiefs posting
/// `JobKind::Farm` could only drive harvesting via `FarmFood` (PlanId 1). This
/// dispatcher restores the planting half of the Farm goal end-to-end:
///
/// 1. Walk `PlantKind::ALL` to find a plantable seed; among those whose
///    catalog id is stocked in faction storage, pick the one with highest
///    stock — mirrors the legacy "highest `SI_STORAGE_*_SEED` weight wins"
///    selection.
/// 2. Find the nearest faction storage tile that holds the chosen seed
///    (effective stock after `StorageReservations`).
/// 3. Find the nearest unplanted Farmland tile within `VIEW_RADIUS=15` of the
///    agent.
/// 4. Build `AbstractTask::PlantFromStorage { resource_id }` with both tiles
///    snapshotted into ctx; `WithdrawAndPlantSeedMethod` expands to
///    `[WithdrawMaterial { seed, 1 }, Planter { tile }]` with
///    `MF_UNINTERRUPTIBLE`.
/// 5. Routes the head WithdrawMaterial leg via
///    `assign_task_with_routing(... TaskKind::WithdrawMaterial ...)` and
///    reserves the seed at the storage tile; the trailing Planter leg lives in
///    the prefetch ring and is promoted by `production::finish_withdraw_material`'s
///    Planter arm, which routes via `TaskKind::Planter` to the destination
///    farmland tile carried in `Task::Planter { tile }`.
///
/// Goal arena: `Farm` only. Faction-tech-gated on `CROP_CULTIVATION` (the
/// method's `tech_gate`). The dispatcher runs after `htn_tame_animal_dispatch_system`
/// in ParallelB; it doesn't compete with food/haul/scout/spear/return-surplus
/// dispatchers because Farm is a distinct goal arena.
/// Bundle of farm-planner inputs read by `htn_plant_from_storage_dispatch_system`
/// and `htn_harvest_plant_dispatch_system` to keep the outer signature under
/// Bevy's 16-param ceiling. Drives the shared `FarmScope` resolver below.
#[derive(bevy::ecs::system::SystemParam)]
pub struct FarmScopeParams<'w, 's> {
    pub board: Res<'w, crate::simulation::jobs::JobBoard>,
    pub plot_index: Res<'w, crate::simulation::land::PlotIndex>,
    pub plot_q: Query<'w, 's, &'static crate::simulation::land::Plot>,
}

/// Where a `AgentGoal::Farm` worker should source seeds / harvest crops, and
/// where deposits should land. Resolved once per dispatch by
/// [`resolve_farm_scope`] and consumed by both farm dispatchers.
#[derive(Clone, Copy, Debug)]
pub enum FarmScope {
    /// Chief-assigned communal plot. Seeds withdraw from village storage,
    /// harvest deposits to village storage. `source_faction_id ==
    /// member.faction_id`. `deposit_override == None`.
    Communal {
        plot_rect: crate::simulation::settlement::TileRect,
        source_faction_id: u32,
    },
    /// Private worker (Farmer or any household adult, per §4) tending their
    /// household's Agricultural plot. Seeds withdraw from household storage
    /// first (`source_faction_id == household_id`); if empty, the planting
    /// dispatcher falls back to `fallback_source_faction_id` (parent
    /// village) so a freshly-housed kitchen-garden household isn't
    /// deadlocked waiting for harvest. Harvest still deposits to household
    /// storage via `deposit_override == Some(household_id)`.
    Private {
        plot_rect: crate::simulation::settlement::TileRect,
        source_faction_id: u32,
        /// Parent village faction id — seed lookup falls back here when
        /// the household's own storage has none.
        fallback_source_faction_id: u32,
    },
    /// No qualifying plot. Planting falls back to the legacy radius-15
    /// farmland search; harvest falls back to the legacy `MemoryKind::AnyEdible`
    /// search; both route through `member.faction_id` storage. Covers the
    /// pre-carving bootstrap window and any non-`Farmer` worker that ends up
    /// on `AgentGoal::Farm`.
    Bootstrap { source_faction_id: u32 },
}

impl FarmScope {
    pub fn source_faction_id(&self) -> u32 {
        match self {
            FarmScope::Communal {
                source_faction_id, ..
            }
            | FarmScope::Private {
                source_faction_id, ..
            }
            | FarmScope::Bootstrap { source_faction_id } => *source_faction_id,
        }
    }

    pub fn plot_rect(&self) -> Option<crate::simulation::settlement::TileRect> {
        match self {
            FarmScope::Communal { plot_rect, .. } | FarmScope::Private { plot_rect, .. } => {
                Some(*plot_rect)
            }
            FarmScope::Bootstrap { .. } => None,
        }
    }

    pub fn deposit_override(&self) -> Option<u32> {
        match self {
            FarmScope::Private {
                source_faction_id, ..
            } => Some(*source_faction_id),
            FarmScope::Communal { .. } | FarmScope::Bootstrap { .. } => None,
        }
    }

    /// Ordered seed-source candidates: primary first, then any fallback.
    /// §5: a Private scope tries household storage first, then the parent
    /// village so a freshly-housed kitchen-garden household isn't
    /// deadlocked on its empty private storage. Returns `[primary,
    /// fallback_or_none]`; the dispatcher walks both, skipping `None`.
    pub fn seed_source_candidates(&self) -> [Option<u32>; 2] {
        match self {
            FarmScope::Communal {
                source_faction_id, ..
            }
            | FarmScope::Bootstrap { source_faction_id } => [Some(*source_faction_id), None],
            FarmScope::Private {
                source_faction_id,
                fallback_source_faction_id,
                ..
            } => {
                let fallback = if fallback_source_faction_id != source_faction_id {
                    Some(*fallback_source_faction_id)
                } else {
                    None
                };
                [Some(*source_faction_id), fallback]
            }
        }
    }
}

/// Resolver for [`FarmScope`]. Mirrors the ownership cascade in
/// `production.rs` plant-stamping: chief Farm claim wins over household
/// tenure wins over Person/Bootstrap.
///
/// 1. **Communal** — the worker holds `JobClaim::Farm` whose posting carries
///    `JobProgress::Planting { plot_id: Some(_), assigned_farmer: Some(self), .. }`.
///    Resolves to the plot's live `rect` (falling back to the posting's
///    `area` snapshot if the plot vanished).
/// 2. **Private** — no qualifying communal claim, but the worker is
///    `Profession::Farmer` + `HouseholdMember` and the household holds a
///    `ZoneKind::Agricultural` plot. Scans `plot_index.by_id` (small N) for
///    `TenureHolder::Household { faction_id: household_id }`.
/// 3. **Bootstrap** — everything else.
pub fn resolve_farm_scope(
    actor: Entity,
    member_faction_id: u32,
    claim_opt: Option<&crate::simulation::jobs::JobClaim>,
    household_member_opt: Option<&crate::simulation::reproduction::HouseholdMember>,
    profession_opt: Option<&crate::simulation::person::Profession>,
    params: &FarmScopeParams,
) -> FarmScope {
    use crate::simulation::jobs::{JobKind, JobProgress};
    use crate::simulation::land::TenureHolder;
    use crate::simulation::settlement::{TileRect, ZoneKind};

    // Path 1: communal chief-assigned plot.
    if let Some(claim) = claim_opt {
        if matches!(claim.kind, JobKind::Farm) {
            if let Some(posting) = params
                .board
                .faction_postings(claim.faction_id)
                .iter()
                .find(|p| p.id == claim.job_id)
            {
                if let JobProgress::Planting {
                    plot_id,
                    assigned_farmer,
                    area,
                    ..
                } = posting.progress
                {
                    let mine = assigned_farmer.map_or(true, |a| a == actor);
                    if mine {
                        if let Some(pid) = plot_id {
                            if let Some(&ent) = params.plot_index.by_id.get(&pid) {
                                if let Ok(plot) = params.plot_q.get(ent) {
                                    return FarmScope::Communal {
                                        plot_rect: plot.rect,
                                        source_faction_id: member_faction_id,
                                    };
                                }
                            }
                            // Plot vanished — fall back to posting's snapshot.
                            let w = (area.max.0 - area.min.0 + 1).max(1) as u16;
                            let h = (area.max.1 - area.min.1 + 1).max(1) as u16;
                            return FarmScope::Communal {
                                plot_rect: TileRect::new(area.min.0, area.min.1, w, h),
                                source_faction_id: member_faction_id,
                            };
                        }
                        // Bootstrap chief posting (no plot_id). Fall through.
                    }
                }
            }
        }
    }

    // Path 2: private household tending its own Agricultural plot.
    // §4: the Farmer-only gate is dropped — any household adult can tend the
    // household plot. The dispatcher's chain (`Profession::Farmer` lift on
    // EV + skill XP) still preferentially routes Farmers, but a Mason in a
    // kitchen-garden household isn't blocked.
    let _ = profession_opt;
    if let Some(hm) = household_member_opt {
        let hh = hm.household_id;
        for (_, &ent) in params.plot_index.by_id.iter() {
            if let Ok(plot) = params.plot_q.get(ent) {
                if plot.zone_kind == ZoneKind::Agricultural
                    && matches!(
                        plot.holder,
                        TenureHolder::Household { faction_id } if faction_id == hh
                    )
                {
                    return FarmScope::Private {
                        plot_rect: plot.rect,
                        source_faction_id: hh,
                        // §5: parent village backstops seed lookup so a
                        // fresh kitchen-garden household with no harvest
                        // yet can still plant from `STARTING_GRAIN_SEEDS`.
                        fallback_source_faction_id: member_faction_id,
                    };
                }
            }
        }
    }

    FarmScope::Bootstrap {
        source_faction_id: member_faction_id,
    }
}

pub fn htn_plant_from_storage_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<SimClock>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    farm_plot_params: FarmScopeParams,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            Option<&crate::simulation::person::Profession>,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::plants::PlantKind;
    use crate::simulation::tasks::{
        find_nearest_unplanted_farmland, find_nearest_unplanted_in_rect,
    };
    const VIEW_RADIUS: i32 = 15;
    let now = clock.tick;
    for (
        actor,
        mut ai,
        mut aq,
        mut history,
        goal,
        transform,
        member,
        lod,
        knowledge_opt,
        claim_opt,
        household_member_opt,
        profession_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Farm) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        // Per-person tech gate. Mirrors the legacy plan candidate filter
        // (`p.tech_gate.map_or(true, |tid| knowledge.has_learned(tid))`).
        let has_tech = knowledge_opt
            .map(|k| k.has_learned(crate::simulation::technology::CROP_CULTIVATION))
            .unwrap_or(false);
        if !has_tech {
            continue;
        }

        // Resolve which storage / plot the worker is bound to. Communal/
        // Private use the assigned plot's rect for planting search and pull
        // seeds from the matching faction (village vs. household); Bootstrap
        // falls back to the village + radius-15 farmland search.
        let scope = resolve_farm_scope(
            actor,
            member.faction_id,
            claim_opt,
            household_member_opt,
            profession_opt,
            &farm_plot_params,
        );

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // §5: Probe each candidate seed source in order (Private →
        // household first, parent village as fallback). Pick the first
        // (seed_id, storage_tile) pair that resolves to a non-empty tile.
        // Walks PlantKind::ALL for each source so a household with millet
        // seeds and no grain still finds something to plant; adding a new
        // plantable seed = new PlantKind::ALL entry + arm in
        // `PlantKind::seed_resource()`.
        let mut resolved: Option<(
            crate::economy::resource_catalog::ResourceId,
            (i32, i32),
            u32,
        )> = None;
        'sources: for src_opt in scope.seed_source_candidates() {
            let Some(source_fid) = src_opt else { continue };
            let Some(faction) = faction_registry.factions.get(&source_fid) else {
                continue;
            };
            let mut best_seed: Option<(crate::economy::resource_catalog::ResourceId, u32)> = None;
            for kind in PlantKind::ALL.iter().copied() {
                let Some(seed_id) = kind.seed_resource() else {
                    continue;
                };
                let stock = faction.storage.totals.get(&seed_id).copied().unwrap_or(0);
                if stock == 0 {
                    continue;
                }
                if best_seed.map_or(true, |(_, b)| stock > b) {
                    best_seed = Some((seed_id, stock));
                }
            }
            let Some((seed_id, _)) = best_seed else {
                continue;
            };

            let Some(tiles) = storage_tile_map.by_faction.get(&source_fid) else {
                continue;
            };
            let mut best_tile: Option<(i32, i32)> = None;
            let mut best_dist = i32::MAX;
            let mut best_tile_stock: u32 = 0;
            for &(tx, ty) in tiles {
                let mut tile_stock: u32 = 0;
                for &gi_entity in spatial.get(tx, ty) {
                    if let Ok(gi) = item_query.get(gi_entity) {
                        if gi.item.resource_id == seed_id && gi.qty > 0 {
                            tile_stock = tile_stock.saturating_add(gi.qty);
                        }
                    }
                }
                let reserved = storage_reservations.get((tx, ty), seed_id);
                let effective = tile_stock.saturating_sub(reserved);
                if effective == 0 {
                    continue;
                }
                let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_tile = Some((tx, ty));
                    best_tile_stock = effective;
                }
            }
            if let Some(storage_tile) = best_tile {
                resolved = Some((seed_id, storage_tile, best_tile_stock));
                break 'sources;
            }
        }
        let Some((seed_id, storage_tile, best_tile_stock)) = resolved else {
            continue;
        };

        // Farm-planner §11: Communal/Private restrict the planting search
        // to the assigned plot's rect; Bootstrap keeps the radius search.
        let plant_tile = if let Some(rect) = scope.plot_rect() {
            let rmin = (rect.x0, rect.y0);
            let rmax = (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1);
            find_nearest_unplanted_in_rect(&chunk_map, &plant_map, (cur_tx, cur_ty), rmin, rmax)
        } else {
            find_nearest_unplanted_farmland(&chunk_map, &plant_map, (cur_tx, cur_ty), VIEW_RADIUS)
        };
        let Some(plant_tile) = plant_tile else {
            continue;
        };

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint: None,
            claimed_blueprint_tile: None,
            // Reuse `gather_target_tile` for the destination farmland tile —
            // semantically "go work at this tile" matches the slot's existing
            // role in gather/forage chains.
            gather_target_tile: Some(plant_tile),
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::PlantFromStorage {
            resource_id: seed_id,
        };
        let methods = method_registry.methods_for(AbstractTaskKind::PlantFromStorage);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
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
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                storage_reservations.add(storage_tile, head_resource, qty as u32);
                ai.reserved_tile = storage_tile;
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Build-goal dispatcher. Owns `AgentGoal::Build` end-to-end via two paths
/// sharing the same `AbstractTaskKind::ConstructBlueprint` registry:
///
/// **Path A — JobClaim::Build (chief-driven)**: held `JobClaim::Build` +
/// companion `ClaimTarget.blueprint = Some(_)` + `bp.is_satisfied()` (every
/// deposit slot full). Replaces the legacy `ClaimedBuild` plan (PlanId 34) +
/// its `BuildClaimedBlueprint` step (StepId 43). The chief-job pipeline
/// supplies the agent with a Build claim only after deposits are filled (chief
/// posts `JobKind::Haul` until then), so the satisfied gate is cheap and
/// `BuildClaimedBlueprintMethod` always wins.
///
/// **Path B — Personal blueprint (`bp.personal_owner == Some(self)`)**: phase
/// 5e-xiii-a, replaces the storage-fed legacy `HaulFromStorageAndBuild` plan
/// (PlanId 29) for the personal-blueprint flow. Personal blueprints are
/// auto-placed (e.g. `BedBlueprint` for bedless agents in
/// `construction.rs::HOMING`) and player-commissioned via the inspector;
/// chief postings explicitly skip them (`jobs.rs:386`), so they have no
/// JobClaim companion. This dispatcher path scans `BlueprintMap` for the
/// agent's personal blueprint, walks the bp's deposit slots to find the
/// most-deficient resource, and (when storage holds at least one unit of that
/// resource) populates `personal_bp_resource` + `material_storage_tile` so
/// `WithdrawAndHaulToPersonalBlueprintMethod` can fire its
/// `[WithdrawMaterial, HaulToBlueprint]` chain. When the personal bp's
/// deposits are *already* satisfied, `personal_bp_resource` stays `None` and
/// the existing `BuildClaimedBlueprintMethod` fires its single-task
/// `[Construct]` expansion — same shape as Path A's terminal leg.
///
/// Path A and Path B share one ctx-build site and one method argmax. The
/// dispatch tail handles both `Task::Construct` and `Task::WithdrawMaterial`
/// heads. `aq.advance()` on Construct completion lives in
/// `construction_system`'s pass-3 cleanup; the WithdrawMaterial→HaulToBlueprint
/// chain handoff lives in `production::finish_withdraw_material`'s existing
/// `Task::HaulToBlueprint { blueprint }` arm. After a single haul, the agent
/// returns to Idle; the next dispatch tick re-evaluates (next deficit slot or
/// terminal Construct).
pub fn htn_build_claimed_blueprint_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    clock: Res<SimClock>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::jobs::ClaimTarget>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            &crate::simulation::carry::Carrier,
            &crate::economy::agent::EconomicAgent,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::jobs::JobKind;
    let now = clock.tick;
    for (
        agent_entity,
        mut ai,
        mut aq,
        mut history,
        goal,
        transform,
        member,
        lod,
        job_claim_opt,
        claim_target_opt,
        household_member,
        carrier,
        agent_econ,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Build) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        // Path A — JobClaim::Build with satisfied bp. Mirrors the legacy
        // `StepTarget::BuildClaimBlueprint` resolver's claim_target.blueprint
        // lookup + the `if !bp.is_satisfied() { return None; }` gate.
        let path_a: Option<Entity> = match (job_claim_opt, claim_target_opt) {
            (Some(claim), Some(target)) if claim.kind == JobKind::Build => {
                target.blueprint.filter(|&bp_e| {
                    bp_query
                        .get(bp_e)
                        .map(|bp| bp.is_satisfied())
                        .unwrap_or(false)
                })
            }
            _ => None,
        };

        // Path B — personal-owner blueprint. Personal bps bypass the chief
        // job pipeline; the agent's only signal is `bp.personal_owner ==
        // Some(self)`. Pick the first matching live entry (in practice the
        // agent owns at most one personal bp at a time — auto-bed
        // `construction.rs::HOMING` checks `has_personal_bp` before placing).
        let path_b: Option<Entity> = if path_a.is_some() {
            None
        } else {
            bp_map.0.values().copied().find(|&bp_e| {
                bp_query
                    .get(bp_e)
                    .map(|bp| bp.personal_owner == Some(agent_entity))
                    .unwrap_or(false)
            })
        };

        let Some(bp_entity) = path_a.or(path_b) else {
            continue;
        };
        let Ok(bp) = bp_query.get(bp_entity) else {
            continue;
        };
        let bp_tile = bp.worker_target_tile();

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Fix 3b: in-hand fast-path for Path B (personal bp, unsatisfied
        // deposits). If the agent already carries enough of an unmet slot's
        // resource, dispatch HaulMaterials directly to the bp tile. Skips a
        // redundant storage round-trip when a prior interrupted chain left
        // material in their hands. Path A's bp is already satisfied by gate,
        // so this only fires for Path B. Scoped to dispatcher only — never
        // affects posting creation or chief candidate scoring.
        if path_b.is_some() && !bp.is_satisfied() {
            let mut hauled = false;
            for i in 0..bp.deposit_count as usize {
                let still = bp.deposits[i]
                    .needed
                    .saturating_sub(bp.deposits[i].deposited) as u32;
                if still == 0 {
                    continue;
                }
                let rid = bp.deposits[i].resource_id;
                let in_hand = carrier
                    .quantity_of_resource(rid)
                    .saturating_add(agent_econ.quantity_of_resource(rid));
                if in_hand == 0 {
                    continue;
                }
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    bp_tile,
                    TaskKind::HaulMaterials,
                    Some(bp_entity),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if dispatched {
                    aq.dispatch(Task::HaulToBlueprint {
                        blueprint: bp_entity,
                    });
                    ai.active_method = None;
                    hauled = true;
                }
                break;
            }
            if hauled {
                continue;
            }
        }

        // For Path B with deposits unmet, resolve the most-deficient resource
        // + nearest faction storage tile holding it. Mirrors the legacy
        // `StepTarget::WithdrawForFactionNeed { Blueprint, MostDeficient }`
        // resolver collapsed to the personal-bp slot list. Path A always has
        // `bp.is_satisfied()`, so we skip the lookup there.
        let (personal_bp_resource, material_storage_tile, material_stock_for_target) =
            if path_b.is_some() && !bp.is_satisfied() {
                // Walk deposit slots in order, picking the first slot whose
                // (still-needed > 0) AND (storage holds it). Most-deficient
                // tiebreak: prefer the slot with largest still-needed, then
                // stable by ResourceId. Single-deposit recipes (Bed = wood
                // only) collapse to the only choice.
                let mut best_resource = None;
                let mut best_storage_tile: Option<(i32, i32)> = None;
                let mut best_storage_stock: u32 = 0;
                let mut best_still_needed: u32 = 0;

                let storage_tiles = storage_tile_map.by_faction.get(&member.faction_id);
                for i in 0..bp.deposit_count as usize {
                    let still = bp.deposits[i]
                        .needed
                        .saturating_sub(bp.deposits[i].deposited)
                        as u32;
                    if still == 0 {
                        continue;
                    }
                    let rid = bp.deposits[i].resource_id;
                    let Some(tiles) = storage_tiles else {
                        continue;
                    };
                    // Find the nearest storage tile holding this resource
                    // (effective stock after reservations > 0).
                    let mut tile_pick: Option<(i32, i32)> = None;
                    let mut tile_pick_dist = i32::MAX;
                    let mut tile_pick_stock: u32 = 0;
                    for &(tx, ty) in tiles {
                        let mut tile_stock: u32 = 0;
                        for &gi_entity in spatial.get(tx, ty) {
                            if let Ok(gi) = item_query.get(gi_entity) {
                                if gi.item.resource_id == rid && gi.qty > 0 {
                                    tile_stock = tile_stock.saturating_add(gi.qty);
                                }
                            }
                        }
                        let reserved = storage_reservations.get((tx, ty), rid);
                        let effective = tile_stock.saturating_sub(reserved);
                        if effective == 0 {
                            continue;
                        }
                        let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
                        if dist < tile_pick_dist {
                            tile_pick_dist = dist;
                            tile_pick = Some((tx, ty));
                            tile_pick_stock = effective;
                        }
                    }
                    let Some(tile) = tile_pick else {
                        continue;
                    };
                    // Argmax across slots: largest still_needed wins; stable
                    // tiebreak by ResourceId.0.
                    if still > best_still_needed
                        || (still == best_still_needed
                            && best_resource
                                .map(|r: ResourceId| rid.0 < r.0)
                                .unwrap_or(true))
                    {
                        best_still_needed = still;
                        best_resource = Some(rid);
                        best_storage_tile = Some(tile);
                        best_storage_stock = tile_pick_stock;
                    }
                }
                (best_resource, best_storage_tile, best_storage_stock)
            } else {
                (None, None, 0)
            };

        // Phase 5e-xiii-b: when the bp is unsatisfied, also probe the agent's
        // memory for a gather source matching the most-deficient resource.
        // We take the resource the storage scan settled on (when storage held
        // it) OR walk the deposit slots once more to find a still-needed one
        // even when storage is dry. For Bed (only personal bp today) the
        // deposit is wood; the legacy `BuildBlueprint` plan keyed off
        // `MemoryKind::wood()` exclusively. Generalising to any
        // `personal_bp_resource` lets future personal-bp recipes flow through
        // automatically.
        let gather_resource: Option<ResourceId> = if path_b.is_some() && !bp.is_satisfied() {
            personal_bp_resource.or_else(|| {
                for i in 0..bp.deposit_count as usize {
                    let still = bp.deposits[i]
                        .needed
                        .saturating_sub(bp.deposits[i].deposited);
                    if still > 0 {
                        return Some(bp.deposits[i].resource_id);
                    }
                }
                None
            })
        } else {
            None
        };
        let gather_target_tile = gather_resource.and_then(|rid| {
            gk.nearest_target_tile(
                agent_entity,
                member.faction_id,
                household_member.map(|h| h.household_id),
                MemoryKind::Resource(rid),
                (cur_tx, cur_ty),
                ai.current_z,
                now,
            )
        });
        // Surface `gather_resource` through `personal_bp_resource` so the
        // gather method's expand can carry it (the withdraw method already
        // had the field set when storage matched). Without storage stock,
        // the storage-fed scan returned None for `personal_bp_resource`;
        // the gather-fed branch needs the slot info regardless.
        let personal_bp_resource = personal_bp_resource.or(gather_resource);

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
            tile: (cur_tx, cur_ty),
            faction_id: member.faction_id,
            faction_home: faction_registry.home_tile(member.faction_id),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile,
            material_stock_for_target,
            claimed_blueprint: Some(bp_entity),
            claimed_blueprint_tile: Some((bp_tile.0, bp_tile.1)),
            gather_target_tile,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::ConstructBlueprint;
        let methods = method_registry.methods_for(AbstractTaskKind::ConstructBlueprint);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Construct { blueprint } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    (bp_tile.0, bp_tile.1),
                    TaskKind::Construct,
                    Some(blueprint),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Construct { blueprint });
            }
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
                // Path B haul leg. Route to the resolved storage tile, reserve
                // the qty against `StorageReservations`, dispatch the head.
                // Mirrors the AcquireGood haul branch's reservation
                // bookkeeping (`htn_acquire_good_dispatch_system`).
                let Some(storage_tile) = material_storage_tile else {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedTarget, now);
                    continue;
                };
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
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                storage_reservations.add(storage_tile, head_resource, qty as u32);
                ai.reserved_tile = storage_tile;
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            Task::Gather { tile } => {
                // Phase 5e-xiii-b gather leg. Route to the memory-known
                // gather tile via TaskKind::Gather; the chain's trailing
                // HaulToBlueprint is handled by `gather::finish_gather`'s
                // new HaulToBlueprint arm.
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    tile,
                    TaskKind::Gather,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Gather { tile });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-viii-a `DeliverHuntKill` dispatcher. Fires when an agent holds a
/// `Carrying` component (set by `pickup_corpse_task_system` after a hunter
/// picks up a fresh corpse) and is otherwise Idle without an `ActivePlan` —
/// i.e. the legacy `HuntFood` plan has just completed at StepId(53) PickUp,
/// leaving the carrier free for HTN to take over the haul→butcher tail.
///
/// Resolves the butcher site by scanning `CampfireMap` for the nearest
/// hearth (mirrors `StepTarget::NearestButcherSite`); falls back to the
/// faction's `home_tile`. SOLO agents have no home and no campfires, so the
/// dispatcher silently skips them — the corpse decays in place.
///
/// Routes the head `Task::HaulCorpse { dest }` via
/// `assign_task_with_routing(... TaskKind::HaulCorpse, None ...)` and prefetches
/// the trailing `Task::Butcher`. Chain handoff lives in
/// `corpse::haul_corpse_task_system`'s arrival exit (Phase 5e-vii-ii): when
/// the queued head is `Task::Butcher`, prime `task_id = TaskKind::Butcher` +
/// `state = Working` so `butcher_task_system` picks up next tick (Butcher is
/// in-place — no routing).
pub fn htn_deliver_hunt_kill_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    campfire_map: Res<crate::simulation::construction::CampfireMap>,
    clock: Res<SimClock>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &Transform,
            &FactionMember,
            &LodLevel,
            &crate::simulation::corpse::Carrying,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (mut ai, mut aq, mut history, transform, member, lod, _carrying) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Resolve butcher site: nearest campfire, falling back to faction home.
        // Mirrors `StepTarget::NearestButcherSite`.
        let mut best: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for (&tile, _e) in campfire_map.0.iter() {
            let dist = (tile.0 - cur_tx).abs() + (tile.1 - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best = Some(tile);
            }
        }
        let butcher_site_tile = best.or_else(|| faction_registry.home_tile(member.faction_id));
        let Some(dest) = butcher_site_tile else {
            // SOLO / unsettled: no destination; corpse decays in place.
            continue;
        };

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: Some(dest),
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::DeliverHuntKill;
        let methods = method_registry.methods_for(AbstractTaskKind::DeliverHuntKill);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::HaulCorpse { dest } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    dest,
                    TaskKind::HaulCorpse,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::HaulCorpse { dest });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-viii-b `EngagePrey` dispatcher. Fires when a hunter at the chief's
/// hunt area finds a live prey entity (vision or memory) or a fresh corpse to
/// pick up. Two methods compete via argmax: `HuntPreyMethod` emits
/// `[Task::Hunt { prey }]` when prey is targetable; `PickUpFreshCorpseMethod`
/// emits `[Task::PickUpCorpse { corpse }]` when a fresh kill is on the
/// ground. World-state transitions (prey alive → prey dead → corpse) drive
/// method selection between dispatch ticks — there's no chain handoff
/// because each method emits a single task.
///
/// Gating: hunter profession + `HUNTING_SPEAR` learned + faction holds
/// `HuntOrder::Hunt` + agent carries no `Carrying` (delivery phase belongs
/// to `htn_deliver_hunt_kill_dispatch_system`) + no `ActivePlan`
/// (truncated `HuntFood` plan owns `[Muster, Travel]` then completes —
/// HTN takes over here). Replaces the middle two steps of the legacy
/// `HuntFood` plan (PlanId 5): `[StepId(5) Hunt, StepId(53) PickUpCorpse]`.
pub fn htn_engage_prey_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    corpse_map: Res<crate::simulation::corpse::CorpseMap>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    prey_query: Query<
        (&Transform, &crate::simulation::combat::Health),
        Or<(
            With<crate::simulation::animals::Wolf>,
            With<crate::simulation::animals::Deer>,
        )>,
    >,
    corpse_query: Query<&crate::simulation::corpse::Corpse>,
    knowledge_query: Query<&crate::simulation::knowledge::PersonKnowledge>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &mut crate::simulation::combat::CombatTarget,
            &Transform,
            &FactionMember,
            &Profession,
            &LodLevel,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            Option<&crate::simulation::corpse::Carrying>,
            &EconomicAgent,
            &crate::simulation::carry::Carrier,
            Option<&crate::simulation::items::Equipment>,
            Option<&crate::simulation::goal_scorers::Disposition>,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::faction::HuntOrder;
    use crate::simulation::technology::HUNTING_SPEAR;
    const VIEW_RADIUS: i32 = 15;
    let weapon_id = crate::economy::core_ids::weapon();
    let now = clock.tick;
    for (
        agent,
        mut ai,
        mut aq,
        mut history,
        mut combat_target,
        transform,
        member,
        profession,
        lod,
        household_member,
        carrying_opt,
        agent_econ,
        carrier,
        equipment_opt,
        disposition_opt,
    ) in query.iter_mut()
    {
        let disposition = disposition_opt.copied().unwrap_or_default();
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*profession, Profession::Hunter) {
            continue;
        }
        if carrying_opt.is_some() {
            // Delivery phase belongs to `htn_deliver_hunt_kill_dispatch_system`.
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        // Per-person tech gate: matches the legacy plan's tech_gate +
        // `faction_hunter_assignment_system`'s personal Learned check.
        let Ok(knowledge) = knowledge_query.get(agent) else {
            continue;
        };
        if !knowledge.has_learned(HUNTING_SPEAR) {
            continue;
        }
        // Faction must be in Hunt phase (not Scout). Without a Hunt order
        // there's no "hunt area" semantics, so the dispatcher has nothing
        // to do.
        let Some(faction) = faction_registry.factions.get(&member.faction_id) else {
            continue;
        };
        let Some(hunt_order) = faction.hunt_order.as_ref() else {
            continue;
        };
        if !matches!(hunt_order, HuntOrder::Hunt { .. }) {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_z = ai.current_z;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Scan for prey within VIEW_RADIUS (LOS-checked). Mirrors
        // `StepTarget::HuntPrey` resolver — vision first, memory fallback.
        let mut prey: Option<(Entity, (i32, i32))> = None;
        let mut prey_dist = i32::MAX;
        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dy * dy > VIEW_RADIUS * VIEW_RADIUS {
                    continue;
                }
                let tx = cur_tx + dx;
                let ty = cur_ty + dy;
                let to_z = chunk_map.surface_z_at(tx, ty) as i8;
                if !crate::simulation::line_of_sight::has_los(
                    &chunk_map,
                    &door_map,
                    (cur_tx, cur_ty, cur_z),
                    (tx, ty, to_z),
                ) {
                    continue;
                }
                for &candidate in spatial.get(tx, ty) {
                    if let Ok((_t, health)) = prey_query.get(candidate) {
                        if !health.is_dead() {
                            let dist = dx.abs() + dy.abs();
                            if dist < prey_dist {
                                prey_dist = dist;
                                prey = Some((candidate, (tx, ty)));
                            }
                        }
                    }
                }
            }
        }
        if prey.is_none() {
            // Fallback: look up the nearest accessible Prey cluster in
            // SharedKnowledge, then scan the spatial index at the cluster's
            // representative tile for a live prey entity. The migration from
            // `AgentMemory.best_entity_for_dist_weighted` loses the entity
            // binding (clusters are tile-keyed), but the rep tile + spatial
            // re-scan recovers it for entities that are still alive. Prey
            // that wandered off get re-discovered when the hunter arrives.
            if let Some(tile) = gk.nearest_target_tile(
                agent,
                member.faction_id,
                household_member.map(|h| h.household_id),
                MemoryKind::Prey,
                (cur_tx, cur_ty),
                cur_z,
                now,
            ) {
                for &candidate in spatial.get(tile.0, tile.1) {
                    if let Ok((_, health)) = prey_query.get(candidate) {
                        if !health.is_dead() {
                            prey = Some((candidate, tile));
                            break;
                        }
                    }
                }
            }
        }

        // Scan CorpseMap for the nearest fresh corpse within VIEW_RADIUS.
        // Mirrors `StepTarget::NearestFreshCorpse` resolver.
        let mut corpse: Option<(Entity, (i32, i32))> = None;
        let mut corpse_dist = i32::MAX;
        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dy * dy > VIEW_RADIUS * VIEW_RADIUS {
                    continue;
                }
                let tx = cur_tx + dx;
                let ty = cur_ty + dy;
                if let Some(entities) = corpse_map.0.get(&(tx, ty)) {
                    for &e in entities {
                        if corpse_query.get(e).is_err() {
                            continue;
                        }
                        let dist = dx.abs() + dy.abs();
                        if dist < corpse_dist {
                            corpse_dist = dist;
                            corpse = Some((e, (tx, ty)));
                        }
                    }
                }
            }
        }

        // Cheap reject: if neither prey nor corpse is in range, no method's
        // precondition can fire.
        if prey.is_none() && corpse.is_none() {
            continue;
        }

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: prey.map(|(e, _)| e),
            prey_target_tile: prey.map(|(_, t)| t),
            fresh_corpse_entity: corpse.map(|(e, _)| e),
            fresh_corpse_tile: corpse.map(|(_, t)| t),
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: agent_econ.quantity_of_resource(weapon_id) > 0
                || carrier.quantity_of_resource(weapon_id) > 0
                || equipment_opt
                    .map(|eq| eq.has_resource(weapon_id))
                    .unwrap_or(false),
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::EngagePrey;
        let methods = method_registry.methods_for(AbstractTaskKind::EngagePrey);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history_and_disposition(
                    a.as_ref(),
                    abstract_task,
                    &ctx,
                    disposition,
                    &history,
                    now,
                );
                let ub = score_method_with_history_and_disposition(
                    b.as_ref(),
                    abstract_task,
                    &ctx,
                    disposition,
                    &history,
                    now,
                );
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Hunt { prey } => {
                let Some((_, prey_tile)) = ctx.prey_target_entity.zip(ctx.prey_target_tile) else {
                    ai.active_method = None;
                    continue;
                };
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    prey_tile,
                    TaskKind::Hunter,
                    Some(prey),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                combat_target.0 = Some(prey);
                aq.dispatch(Task::Hunt { prey });
            }
            Task::PickUpCorpse { corpse } => {
                let Some(corpse_tile) = ctx.fresh_corpse_tile else {
                    ai.active_method = None;
                    continue;
                };
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    corpse_tile,
                    TaskKind::PickUpCorpse,
                    Some(corpse),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::PickUpCorpse { corpse });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-viii-c `JoinHuntParty` dispatcher. Fires for any hunter with
/// Learned `HUNTING_SPEAR` while the chief holds `HuntOrder::Hunt` and the
/// agent doesn't yet carry a corpse. Two methods compete via argmax based
/// on the order's `deployed_tick` state: `MusterAtHearthMethod` walks to
/// the muster hearth while the party hasn't yet deployed (and isn't
/// stale); `TravelToHuntAreaMethod` walks to the chief's `area_tile` once
/// deployed (or stale). Replaces the leading two steps of the legacy
/// `HuntFood` plan (PlanId 5): `[StepId(57) HuntPartyMuster, StepId(58)
/// TravelToHuntArea]`.
///
/// On the Travel leg, routing destination is the area_tile but the typed
/// task is `Task::Explore { kind: Prey }` — the agent IS scanning for prey
/// memory along the path (which `vision_system` records), and on arrival
/// `goal_dispatch_system`'s catch-all flips Idle, freeing the next dispatch
/// tick for `htn_engage_prey_dispatch_system` to take over.
pub fn htn_join_hunt_party_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    campfire_map: Res<crate::simulation::construction::CampfireMap>,
    clock: Res<SimClock>,
    knowledge_query: Query<&crate::simulation::knowledge::PersonKnowledge>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &Transform,
            &FactionMember,
            &Profession,
            &LodLevel,
            Option<&crate::simulation::corpse::Carrying>,
            &EconomicAgent,
            &crate::simulation::carry::Carrier,
            Option<&crate::simulation::items::Equipment>,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::faction::{HuntOrder, HUNT_PARTY_TIMEOUT};
    use crate::simulation::technology::HUNTING_SPEAR;
    let weapon_id = crate::economy::core_ids::weapon();
    let now = clock.tick;
    for (
        agent,
        mut ai,
        mut aq,
        mut history,
        transform,
        member,
        profession,
        lod,
        carrying_opt,
        agent_econ,
        carrier,
        equipment_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*profession, Profession::Hunter) {
            continue;
        }
        if carrying_opt.is_some() {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let Ok(knowledge) = knowledge_query.get(agent) else {
            continue;
        };
        if !knowledge.has_learned(HUNTING_SPEAR) {
            continue;
        }
        let Some(faction) = faction_registry.factions.get(&member.faction_id) else {
            continue;
        };
        let Some(hunt_order) = faction.hunt_order.as_ref() else {
            continue;
        };
        let HuntOrder::Hunt {
            area_tile,
            deployed_tick,
            posted_tick,
            ..
        } = hunt_order
        else {
            continue;
        };
        let area_tile = *area_tile;
        let deployed = deployed_tick.is_some();
        let stale = now.saturating_sub(*posted_tick) > HUNT_PARTY_TIMEOUT;

        // Resolve hearth: nearest campfire to area_tile, falling back to
        // faction home_tile. Mirrors `StepTarget::HearthForHunt`.
        let mut best: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for (&tile, _e) in campfire_map.0.iter() {
            let dist = (tile.0 - area_tile.0).abs() + (tile.1 - area_tile.1).abs();
            if dist < best_dist {
                best_dist = dist;
                best = Some(tile);
            }
        }
        let hearth = best.or_else(|| faction_registry.home_tile(member.faction_id));

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: hearth,
            hunt_area_tile: Some(area_tile),
            hunt_party_deployed: deployed,
            hunt_party_stale: stale,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: agent_econ.quantity_of_resource(weapon_id) > 0
                || carrier.quantity_of_resource(weapon_id) > 0
                || equipment_opt
                    .map(|eq| eq.has_resource(weapon_id))
                    .unwrap_or(false),
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::JoinHuntParty;
        let methods = method_registry.methods_for(AbstractTaskKind::JoinHuntParty);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::HuntPartyMuster { hearth } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    hearth,
                    TaskKind::HuntPartyMuster,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::HuntPartyMuster { hearth });
            }
            Task::Explore { kind } => {
                // Travel: route destination is the chief's area_tile, even
                // though the typed variant is the generic Explore (which
                // semantically also wants memory of `kind` along the way —
                // `vision_system` records as the agent walks).
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    area_tile,
                    TaskKind::Explore,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Explore { kind });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// `AgentGoal::Socialize` dispatcher. The agent walks to the nearest other
/// Person within 12 tiles and sits adjacent to converse. Single-method
/// registry — `SocializeWithPartnerMethod` always wins. Replaces the legacy
/// `Socialize` plan (PlanId 60) and its single step (StepId 48
/// NearestPlayPartner).
///
/// Filtering: scans `SpatialIndex` and rejects entities that aren't a
/// `Person` (blueprints, ground items, animals all lack the marker), and
/// excludes the agent itself. The legacy resolver double-checked via
/// `prey_query` / `wild_horse_q` etc; with the explicit `Person` filter
/// here the rejection list collapses to one component check.
///
/// Goal-agnostic about lifecycle: there's no chain handoff (single-leg),
/// `task_drops_hand_load` already drops carried items at task entry, and
/// the agent stays in `TaskKind::Socialize` until `goal_update_system`
/// flips them off `AgentGoal::Socialize` (typically when `needs.social`
/// has dropped enough to defuse the trigger). The
/// `goal_dispatch_system` stale-reset arm preserves the task across
/// dispatch ticks while the goal stays Socialize.
pub fn htn_socialize_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    person_query: Query<(), With<crate::simulation::person::Person>>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::goal_scorers::Disposition>,
        ),
        Without<Drafted>,
    >,
) {
    const PARTNER_RADIUS: i32 = 12;
    let now = clock.tick;
    for (agent, mut ai, mut aq, mut history, goal, transform, member, lod, disposition_opt) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Socialize) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Scan SpatialIndex within PARTNER_RADIUS for the nearest other Person.
        // Mirrors the legacy `NearestPlayPartner` resolver; the explicit
        // `Person` marker filter collapses the resolver's blueprint / item /
        // animal rejection list into a single component check.
        let mut best: Option<(Entity, (i32, i32))> = None;
        let mut best_dist = i32::MAX;
        for dy in -PARTNER_RADIUS..=PARTNER_RADIUS {
            for dx in -PARTNER_RADIUS..=PARTNER_RADIUS {
                let tx = cur_tx + dx;
                let ty = cur_ty + dy;
                for &candidate in spatial.get(tx, ty) {
                    if candidate == agent {
                        continue;
                    }
                    if person_query.get(candidate).is_err() {
                        continue;
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((candidate, (tx, ty)));
                    }
                }
            }
        }

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: best.map(|(e, _)| e),
            scavenge_target_tile: best.map(|(_, t)| t),
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let disposition = disposition_opt.copied().unwrap_or_default();
        let abstract_task = AbstractTask::Socialize;
        let Some(pick) = dispatch_for_goal(
            &method_registry,
            abstract_task,
            &ctx,
            &history,
            now,
            Some(disposition),
        ) else {
            continue;
        };
        let method = pick.method;
        let chosen_id = pick.method_id;
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Socialize { partner } => {
                let Some((_, partner_tile)) = best else {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedTarget, now);
                    continue;
                };
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    partner_tile,
                    TaskKind::Socialize,
                    Some(partner),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Socialize { partner });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// `AgentGoal::{Raid, Defend, Lead, Rescue}` dispatcher (Phase 5e-x).
/// Single system covers the four single-step combat/faction goals because
/// they share a near-identical shape: gate on goal, resolve a destination
/// tile from `FactionRegistry` (or the agent's `RescueTarget`), expand a
/// sole-method registry, route via `assign_task_with_routing`, dispatch
/// the typed variant. Rescue alone writes `CombatTarget` for engagement.
///
/// Replaces legacy plans `RescueAlly` (PlanId 23 + StepId 27 EngageRescue),
/// `Raid` (PlanId 61 + StepId 49 FactionRaidTarget), `Defend` (PlanId 62
/// + StepId 50 FactionCamp), `Lead` (PlanId 63 + StepId 51 FactionCamp).
pub fn htn_combat_faction_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    clock: Res<SimClock>,
    chief_query: Query<(), With<crate::simulation::faction::FactionChief>>,
    rescue_query: Query<&crate::simulation::goals::RescueTarget>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &mut crate::simulation::combat::CombatTarget,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (agent, mut ai, mut aq, mut history, mut combat_target, goal, transform, member, lod) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        // Per-goal target resolution.
        let (abstract_task, abstract_kind, dest, target_entity, task_kind) = match *goal {
            AgentGoal::Raid => {
                let Some(target_faction) = faction_registry.raid_target(member.faction_id) else {
                    continue;
                };
                let Some(dest) = faction_registry.home_tile(target_faction) else {
                    continue;
                };
                (
                    AbstractTask::Raid,
                    AbstractTaskKind::Raid,
                    dest,
                    None,
                    TaskKind::Raid,
                )
            }
            AgentGoal::Defend => {
                let Some(dest) = faction_registry.home_tile(member.faction_id) else {
                    continue;
                };
                (
                    AbstractTask::Defend,
                    AbstractTaskKind::Defend,
                    dest,
                    None,
                    TaskKind::Defend,
                )
            }
            AgentGoal::Lead => {
                if chief_query.get(agent).is_err() {
                    continue;
                }
                let Some(dest) = faction_registry.home_tile(member.faction_id) else {
                    continue;
                };
                (
                    AbstractTask::Lead,
                    AbstractTaskKind::Lead,
                    dest,
                    None,
                    TaskKind::Lead,
                )
            }
            AgentGoal::Rescue => {
                let Ok(rt) = rescue_query.get(agent) else {
                    continue;
                };
                (
                    AbstractTask::RescueAlly,
                    AbstractTaskKind::RescueAlly,
                    rt.attacker_tile,
                    Some(rt.attacker),
                    // RescueAlly's legacy step used TaskKind::Defend so the
                    // existing stale-reset / hands-checks behave the same;
                    // the typed variant is the discriminator for executor
                    // logic that needs to differentiate.
                    TaskKind::Defend,
                )
            }
            _ => continue,
        };

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            // Raid / Defend / Lead use the gather_target_tile slot for the
            // single destination tile.
            gather_target_tile: if matches!(*goal, AgentGoal::Rescue) {
                None
            } else {
                Some(dest)
            },
            // RescueAlly carries (attacker_entity, attacker_tile) via the
            // scavenge slots — semantically "any entity to walk to".
            scavenge_target_entity: target_entity,
            scavenge_target_tile: if matches!(*goal, AgentGoal::Rescue) {
                Some(dest)
            } else {
                None
            },
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let methods = method_registry.methods_for(abstract_kind);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        // For Rescue, set CombatTarget so combat_system engages on adjacency.
        if matches!(*goal, AgentGoal::Rescue) {
            combat_target.0 = target_entity;
        }
        let route_target_entity = if matches!(*goal, AgentGoal::Rescue) {
            target_entity
        } else {
            None
        };
        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            dest,
            task_kind,
            route_target_entity,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !dispatched {
            ai.active_method = None;
            history.push(chosen_id, MethodOutcome::FailedRouting, now);
            if matches!(*goal, AgentGoal::Rescue) {
                combat_target.0 = None;
            }
            continue;
        }
        aq.dispatch(head);
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Pluralist Economy R5 follow-on: idle bureaucrats walk to their
/// faction's town hall (= the first settlement's `market_tile`) and
/// stand there. Reuses `Task::Lead` (a no-op-on-arrival task) — no
/// new TaskKind / Task variant. The hard guardrail holds.
///
/// **Gate**: `Profession::Bureaucrat` + `task_id == UNEMPLOYED` +
/// `aq.current == Idle` + `state == Idle` + non-Dormant LOD. The
/// last three together mean the bureaucrat has no other obligation;
/// when goal_update_system flips them onto Survive (hungry) or any
/// other need-driven goal, that goal's normal task chain
/// preempts via `aq.cancel()` semantics in the goal-dispatch
/// pipeline.
///
/// **No HTN method registration**. Bureaucrat behaviour is
/// deterministic (single tile, single task) — there's no decision
/// space for a Method to score. Direct dispatch keeps the
/// implementation a single focused system rather than an abstract
/// task + method + dispatcher trio for what amounts to "stand
/// here."
pub fn bureaucrat_admin_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlements: Query<&crate::simulation::settlement::Settlement>,
    mut query: Query<
        (
            &crate::simulation::person::Profession,
            &mut PersonAI,
            &mut ActionQueue,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    for (prof, mut ai, mut aq, transform, member, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if *prof != crate::simulation::person::Profession::Bureaucrat {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if ai.state != AiState::Idle {
            continue;
        }
        if !matches!(aq.current, Task::Idle) {
            continue;
        }

        // Find the agent's faction's town hall = first settlement's
        // market_tile.
        let Some(sid) = settlement_map.first_for_faction(member.faction_id) else {
            continue;
        };
        let Some(&entity) = settlement_map.by_id.get(&sid) else {
            continue;
        };
        let Ok(settlement) = settlements.get(entity) else {
            continue;
        };
        let dest = settlement.market_tile;

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Already at the town hall? Skip routing — just dispatch
        // the typed task so the executor / preempt system sees a
        // bureaucrat who is "at desk".
        if cur_tx == dest.0 && cur_ty == dest.1 {
            aq.dispatch(Task::Lead { dest });
            continue;
        }

        // Route via the standard pathfinding pipeline.
        let routed = crate::simulation::tasks::assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            dest,
            crate::simulation::tasks::TaskKind::Lead,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if routed {
            aq.dispatch(Task::Lead { dest });
        }
    }
}

/// Phase 5e-xi-a method: withdraw one unit of `resource_id` from faction
/// storage and haul it into a `CraftOrder` whose deposit slots still need it.
/// Replaces the legacy `DeliverFromStorageToCraftOrder` plan (PlanId 15) +
/// `[StepId(40) FetchCraftOrderMaterialFromStorage, StepId(38) HaulToCraftOrder]`.
///
/// Mirrors `WithdrawAndHaulToBlueprintMethod`'s shape — the only difference is
/// the trailing leg's destination (a `CraftOrder` anchor tile vs. a
/// `Blueprint` tile). Both legs together survive a goal flip via
/// `MF_UNINTERRUPTIBLE` (mirrors the legacy plan's `PF_UNINTERRUPTIBLE`) so a
/// transient hunger spike doesn't strand the storage reservation mid-fetch.
///
/// The dispatcher (`htn_deliver_material_to_craft_order_dispatch_system`)
/// populates `material_storage_tile` (where we withdraw from) and
/// `target_craft_order` (where the trailing leg delivers).
pub struct WithdrawAndHaulToCraftOrderMethod;

impl Method for WithdrawAndHaulToCraftOrderMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(
            abstract_task,
            AbstractTask::DeliverMaterialToCraftOrder { .. }
        ) {
            return false;
        }
        ctx.material_stock_for_target > 0
            && ctx.material_storage_tile.is_some()
            && ctx.target_craft_order.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Single-method registry under DeliverMaterialToCraftOrder. Use the
        // baseline tier — there's no sibling to outrank, and the chain only
        // fires when both ctx pre-reqs are populated, which mirrors the
        // legacy plan's "open craft order + storage stock" gate.
        UTIL_BASELINE
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let AbstractTask::DeliverMaterialToCraftOrder { resource_id } = abstract_task else {
            return Vec::new();
        };
        let Some(order) = ctx.target_craft_order else {
            return Vec::new();
        };
        if ctx.material_storage_tile.is_none() {
            return Vec::new();
        }
        vec![
            Task::WithdrawMaterial {
                resource_id,
                qty: 1,
            },
            Task::HaulToCraftOrder { order },
        ]
    }

    fn name(&self) -> &'static str {
        "WithdrawAndHaulToCraftOrder"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_HAUL_TO_CRAFT_ORDER
    }
}

/// Phase 5e-xi-a dispatcher. Owns the `AgentGoal::Craft` deliver-from-storage
/// case via the `DeliverMaterialToCraftOrder` abstract task. Replaces the
/// legacy `DeliverFromStorageToCraftOrder` plan (PlanId 15) — the
/// `[FetchCraftOrderMaterialFromStorage → HaulToCraftOrder]` chain.
///
/// Gates on:
/// - `AgentGoal::Craft` (set by `should_craft` in `goal_update_system` or by a
///   `JobClaim::Craft` companion via `job_goal_lock_system`).
/// - No `ActivePlan` and Idle (the legacy `WorkOnCraft` plan, PlanId 16, still
///   runs through `plan_execution_system` for the labor leg of a satisfied
///   order, so we stay out of its way until it completes).
/// - Non-SOLO faction.
///
/// Resolution mirrors `resolve_withdraw_for_faction_need`'s `MaterialNeed::CraftOrder`
/// branch:
/// 1. Aggregate per-resource still-needed demand across the faction's open
///    `CraftOrder`s.
/// 2. Walk faction storage tiles to find the nearest one whose ground items
///    cover any demanded resource (effective stock after `StorageReservations`).
/// 3. On that tile, pick the most-deficient resource (the legacy
///    `GoodSelector::MostDeficient` behaviour). Stable tiebreak by `ResourceId.0`.
/// 4. Pick the nearest `CraftOrder` whose deposits still need the chosen
///    resource. Carry that order entity through the chain so the trailing
///    `Task::HaulToCraftOrder { order }` knows where to deliver.
///
/// The chain handoff lives in `production::finish_withdraw_material`'s
/// `Task::HaulToCraftOrder` arm, which routes to `order.anchor_tile` via
/// `assign_task_with_routing(... TaskKind::HaulToCraftOrder, Some(order) ...)`.
/// `craft_order_system`'s hauler branch already deposits-on-arrival; this PR
/// teaches it to also `aq.advance()` so the typed channel drains on completion.
pub fn htn_deliver_material_to_craft_order_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    co_map: Res<crate::simulation::crafting::CraftOrderMap>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    clock: Res<SimClock>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (mut ai, mut aq, mut history, goal, member, transform, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Craft) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        // 1. Aggregate per-resource demand across the faction's open orders.
        let mut still_need: AHashMap<ResourceId, u32> = AHashMap::new();
        for (_, &order_entity) in &co_map.0 {
            let Ok(order) = co_query.get(order_entity) else {
                continue;
            };
            if order.faction_id != member.faction_id {
                continue;
            }
            for i in 0..order.deposit_count as usize {
                let still = order.deposits[i]
                    .needed
                    .saturating_sub(order.deposits[i].deposited);
                if still > 0 {
                    let rid = order.deposits[i].resource_id;
                    let slot = still_need.entry(rid).or_insert(0);
                    *slot = slot.saturating_add(still as u32);
                }
            }
        }
        if still_need.is_empty() {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // 2. Find the nearest faction storage tile holding at least one
        //    demanded resource (intersection of demand & effective stock).
        let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) else {
            continue;
        };
        let effective_stock = |tx: i32, ty: i32, rid: ResourceId| -> u32 {
            let mut stock = 0u32;
            for &gi_entity in spatial.get(tx, ty) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.item.resource_id == rid {
                        stock = stock.saturating_add(gi.qty);
                    }
                }
            }
            stock.saturating_sub(storage_reservations.get((tx, ty), rid))
        };
        let mut best_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for &(tx, ty) in tiles {
            let mut has_useful = false;
            for &gi_entity in spatial.get(tx, ty) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.qty == 0 {
                        continue;
                    }
                    let rid = gi.item.resource_id;
                    if still_need.get(&rid).copied().unwrap_or(0) == 0 {
                        continue;
                    }
                    if effective_stock(tx, ty, rid) > 0 {
                        has_useful = true;
                        break;
                    }
                }
            }
            if !has_useful {
                continue;
            }
            let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best_tile = Some((tx, ty));
            }
        }
        let Some((stx, sty)) = best_tile else {
            continue;
        };

        // 3. Pick the most-deficient resource on the chosen tile (legacy
        //    `GoodSelector::MostDeficient` behaviour). Stable tiebreak by
        //    `ResourceId.0` for deterministic dispatch.
        let mut chosen: Option<(ResourceId, u32, u32)> = None; // (rid, deficit, stock)
        for &gi_entity in spatial.get(stx, sty) {
            if let Ok(gi) = item_query.get(gi_entity) {
                if gi.qty == 0 {
                    continue;
                }
                let rid = gi.item.resource_id;
                let deficit = still_need.get(&rid).copied().unwrap_or(0);
                if deficit == 0 {
                    continue;
                }
                let stock = effective_stock(stx, sty, rid);
                if stock == 0 {
                    continue;
                }
                chosen = Some(match chosen {
                    None => (rid, deficit, stock),
                    Some(prev) => {
                        if deficit > prev.1 || (deficit == prev.1 && rid.0 < prev.0 .0) {
                            (rid, deficit, stock)
                        } else {
                            prev
                        }
                    }
                });
            }
        }
        let Some((target_rid, _deficit, tile_stock)) = chosen else {
            continue;
        };

        // 4. Pick the nearest `CraftOrder` of the agent's faction whose
        //    deposits still need `target_rid`.
        let mut order_pick: Option<(Entity, i32)> = None;
        for (_, &order_entity) in &co_map.0 {
            let Ok(order) = co_query.get(order_entity) else {
                continue;
            };
            if order.faction_id != member.faction_id {
                continue;
            }
            let mut needs_it = false;
            for i in 0..order.deposit_count as usize {
                if order.deposits[i].resource_id == target_rid
                    && order.deposits[i].needed > order.deposits[i].deposited
                {
                    needs_it = true;
                    break;
                }
            }
            if !needs_it {
                continue;
            }
            let dist = (order.anchor_tile.0 - cur_tx).abs() + (order.anchor_tile.1 - cur_ty).abs();
            order_pick = Some(match order_pick {
                None => (order_entity, dist),
                Some((_, prev_dist)) if dist < prev_dist => (order_entity, dist),
                Some(prev) => prev,
            });
        }
        let Some((order_entity, _)) = order_pick else {
            continue;
        };

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
            tile: (cur_tx, cur_ty),
            faction_id: member.faction_id,
            faction_home: faction_registry.home_tile(member.faction_id),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: Some((stx, sty)),
            material_stock_for_target: tile_stock,
            claimed_blueprint: None,
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: Some(order_entity),
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::DeliverMaterialToCraftOrder {
            resource_id: target_rid,
        };
        let methods = method_registry.methods_for(AbstractTaskKind::DeliverMaterialToCraftOrder);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    (stx, sty),
                    TaskKind::WithdrawMaterial,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                // Reserve the qty against the chosen tile so a parallel
                // dispatch in the same tick sees a smaller effective stock.
                storage_reservations.add((stx, sty), head_resource, qty as u32);
                ai.reserved_tile = (stx, sty);
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-xi-b method: walk adjacent to a satisfied faction `CraftOrder`
/// and labor at it until completion. Replaces the legacy `WorkOnCraft` plan
/// (PlanId 16) + `[StepId(39) WorkOnCraftOrder, StepId(12) DepositGoods]`.
///
/// Single-method registry under `AbstractTaskKind::WorkOnCraftOrder` —
/// dispatcher fires only when at least one satisfied order exists, so there
/// are no siblings to outrank. Expansion is `[Task::WorkOnCraftOrder { order
/// }, Task::DepositToFactionStorage { resource_id: output, target_faction_id: None }]`. The trailing
/// deposit chain handoff lives in `craft_order_system`'s completion path —
/// after `aq.advance()` promotes the queued deposit, the system routes the
/// agent to the nearest faction storage tile and primes
/// `task_id = TaskKind::DepositResource`. `drop_items_at_destination_system`
/// (Economy) already deposits crafted output goods (Tools / Weapon / Armor /
/// Shield / Cloth / Luxury) from inventory at the destination tile.
///
/// `MF_UNINTERRUPTIBLE` so a goal flip mid-labor doesn't drop the worker —
/// mirrors the legacy plan's `PF_UNINTERRUPTIBLE`.
pub struct WorkOnSatisfiedCraftOrderMethod;

impl Method for WorkOnSatisfiedCraftOrderMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::WorkOnCraftOrder)
            && ctx.target_craft_order.is_some()
            && ctx.craft_output_resource.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(order) = ctx.target_craft_order else {
            return Vec::new();
        };
        let Some(output) = ctx.craft_output_resource else {
            return Vec::new();
        };
        vec![
            Task::WorkOnCraftOrder { order },
            Task::DepositToFactionStorage {
                resource_id: output,
                target_faction_id: None,
            },
        ]
    }

    fn name(&self) -> &'static str {
        "WorkOnSatisfiedCraftOrder"
    }

    fn id(&self) -> MethodId {
        MethodId::WORK_ON_SATISFIED_CRAFT_ORDER
    }
}

/// Phase 5e-xi-b dispatcher. Owns the `AgentGoal::Craft` work-on-satisfied-
/// order case via the `WorkOnCraftOrder` abstract task. Replaces the legacy
/// `WorkOnCraft` plan (PlanId 16).
///
/// Gates on `AgentGoal::Craft` + no `ActivePlan` + Idle + non-SOLO. Scans
/// `CraftOrderMap` for the nearest faction-owned order whose `is_satisfied()`
/// is true (deposits filled). Snapshots the order entity and its recipe's
/// `output_resource` into ctx.
///
/// Routes via `assign_task_with_routing(... TaskKind::WorkOnCraftOrder,
/// Some(order) ...)` to the order's `anchor_tile` and dispatches the head
/// `Task::WorkOnCraftOrder { order }`; the trailing
/// `Task::DepositToFactionStorage { resource_id: output, target_faction_id: None }` rides the prefetch
/// ring. The chain handoff lives in `craft_order_system`'s completion path:
/// after producing the output and calling `aq.advance()`, if the new
/// `current` is a `DepositToFactionStorage`, route to the nearest faction
/// storage tile and prime `task_id = TaskKind::DepositResource`.
pub fn htn_work_on_craft_order_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    co_map: Res<crate::simulation::crafting::CraftOrderMap>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    clock: Res<SimClock>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (mut ai, mut aq, mut history, goal, member, transform, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Craft) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Scan for the nearest faction-owned satisfied order. Mirrors the
        // legacy `StepTarget::NearestSatisfiedCraftOrder` resolver.
        let mut best: Option<(Entity, (i32, i32), u8)> = None; // (entity, anchor, recipe_id)
        let mut best_dist = i32::MAX;
        for (&_anchor_tile, &order_entity) in &co_map.0 {
            let Ok(order) = co_query.get(order_entity) else {
                continue;
            };
            if order.faction_id != member.faction_id {
                continue;
            }
            if !order.is_satisfied() {
                continue;
            }
            let dist = (order.anchor_tile.0 - cur_tx).abs() + (order.anchor_tile.1 - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best = Some((order_entity, order.anchor_tile, order.recipe_id));
            }
        }
        let Some((order_entity, anchor, recipe_id)) = best else {
            continue;
        };

        let recipes = crate::simulation::crafting::craft_recipes();
        let Some(recipe) = recipes.get(recipe_id as usize) else {
            continue;
        };
        let output_resource = recipe.output_resource;

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: Some(order_entity),
            craft_output_resource: Some(output_resource),
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::WorkOnCraftOrder;
        let methods = method_registry.methods_for(AbstractTaskKind::WorkOnCraftOrder);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::WorkOnCraftOrder { order } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    anchor,
                    TaskKind::WorkOnCraftOrder,
                    Some(order),
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::WorkOnCraftOrder { order });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-xi-c method: harvest a mature grain plant in memory and haul
/// the harvested grain to a faction `CraftOrder` whose deposits still need
/// it. Replaces the legacy `DeliverGrainToCraftOrder` plan (PlanId 14) +
/// `[StepId(1) FarmFood, StepId(38) HaulToCraftOrder]`.
///
/// Single-method registry under `AbstractTaskKind::HarvestGrainForCraftOrder`.
/// Expansion: `[Task::Gather { tile }, Task::HaulToCraftOrder { order }]`.
/// The trailing haul's chain handoff lives in `gather::finish_gather`'s
/// `Task::HaulToCraftOrder` arm — looks up `CraftOrder.anchor_tile` and
/// routes via `assign_task_with_routing(... TaskKind::HaulToCraftOrder,
/// Some(order) ...)`. `craft_order_system`'s hauler branch consumes the
/// typed task on arrival and `aq.advance()`s on completion.
///
/// `MF_UNINTERRUPTIBLE` so a goal flip mid-harvest doesn't drop the chain.
pub struct HarvestAndHaulGrainToCraftOrderMethod;

impl Method for HarvestAndHaulGrainToCraftOrderMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::HarvestGrainForCraftOrder)
            && ctx.gather_target_tile.is_some()
            && ctx.target_craft_order.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        let Some(order) = ctx.target_craft_order else {
            return Vec::new();
        };
        vec![Task::Gather { tile }, Task::HaulToCraftOrder { order }]
    }

    fn name(&self) -> &'static str {
        "HarvestAndHaulGrainToCraftOrder"
    }

    fn id(&self) -> MethodId {
        MethodId::HARVEST_AND_HAUL_GRAIN_TO_CRAFT_ORDER
    }
}

/// Phase 5e-xi-c dispatcher. Owns the harvest-grain-for-craft case under
/// `AgentGoal::Craft`. Replaces the legacy `DeliverGrainToCraftOrder` plan
/// (PlanId 14).
///
/// Gates on `AgentGoal::Craft` + no `ActivePlan` + Idle + non-SOLO + at least
/// one open faction `CraftOrder` whose deposits still need Grain. Walks
/// `AgentMemory::best_for(MemoryKind::AnyEdible)` paired with `PlantMap` to
/// find a mature Grain plant tile. Picks the nearest such craft order.
/// Snapshots `(gather_target_tile, target_craft_order)` into ctx and dispatches
/// the head `Task::Gather { tile }` via `assign_task_with_routing(...
/// TaskKind::Gather, None ...)`. The trailing `Task::HaulToCraftOrder { order }`
/// rides the prefetch ring; `gather::finish_gather` routes it on harvest
/// completion.
pub fn htn_harvest_grain_for_craft_order_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    co_map: Res<crate::simulation::crafting::CraftOrderMap>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    clock: Res<SimClock>,
    gather_claims: Res<GatherClaims>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            &crate::simulation::memory::CurrentVision,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    let grain_id = crate::economy::core_ids::grain();
    for (
        actor,
        mut ai,
        mut aq,
        mut history,
        goal,
        member,
        transform,
        lod,
        household_member,
        current_vision,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Craft) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        // Find the nearest faction-owned CraftOrder whose deposits still need
        // grain. Mirrors `MaterialNeed::CraftOrder` resolver but filtered to
        // a specific resource (grain).
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let mut order_pick: Option<(Entity, i32)> = None;
        for (_, &order_entity) in &co_map.0 {
            let Ok(order) = co_query.get(order_entity) else {
                continue;
            };
            if order.faction_id != member.faction_id {
                continue;
            }
            let mut needs_grain = false;
            for i in 0..order.deposit_count as usize {
                if order.deposits[i].resource_id == grain_id
                    && order.deposits[i].needed > order.deposits[i].deposited
                {
                    needs_grain = true;
                    break;
                }
            }
            if !needs_grain {
                continue;
            }
            let dist = (order.anchor_tile.0 - cur_tx).abs() + (order.anchor_tile.1 - cur_ty).abs();
            order_pick = Some(match order_pick {
                None => (order_entity, dist),
                Some((_, prev_dist)) if dist < prev_dist => (order_entity, dist),
                Some(prev) => prev,
            });
        }
        let Some((order_entity, _)) = order_pick else {
            continue;
        };

        // Vision-first: prefer a visible mature Grain plant the agent can
        // see right now over a remembered one. SharedKnowledge is consulted
        // only when vision shows none.
        let viewer_household = household_member.map(|h| h.household_id);
        let viewer_settlement = gk.settlement_map.first_for_faction(member.faction_id);
        // Phase 2a: tile-reachability closure for the visible-grain pick.
        let reach_from_agent = |t: (i32, i32)| -> bool {
            let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
            chunk_connectivity.tile_reachable(
                &chunk_graph,
                (cur_tx, cur_ty, ai.current_z),
                (t.0, t.1, tz),
            )
        };
        let detour_est =
            crate::pathfinding::detour::DetourEstimator::new(&chunk_router, &chunk_graph);
        let detour_dist = |t: (i32, i32)| -> i32 {
            let tz = chunk_map.nearest_standable_z(t.0, t.1, ai.current_z as i32) as i8;
            detour_est.tiles((cur_tx, cur_ty), ai.current_z, t, tz)
        };
        let visible_grain = current_vision
            .nearest_gather_target(
                MemoryKind::AnyEdible,
                detour_dist,
                actor,
                viewer_household,
                viewer_settlement,
                member.faction_id,
                |t| gather_claims.pressure(t, now, actor) * 4,
                reach_from_agent,
            )
            .filter(|tile| {
                plant_map
                    .0
                    .get(tile)
                    .and_then(|e| plant_query.get(*e).ok())
                    .map(|p| p.kind == crate::simulation::plants::PlantKind::Grain)
                    .unwrap_or(false)
            });
        let gather_target_tile = visible_grain.or_else(|| {
            gk.nearest_target_tile(
                actor,
                member.faction_id,
                viewer_household,
                MemoryKind::AnyEdible,
                (cur_tx, cur_ty),
                ai.current_z,
                now,
            )
            .and_then(|tile| {
                let entity = plant_map.0.get(&tile).copied()?;
                let plant = plant_query.get(entity).ok()?;
                if plant.kind == crate::simulation::plants::PlantKind::Grain
                    && plant.stage == GrowthStage::Mature
                {
                    Some(tile)
                } else {
                    None
                }
            })
        });
        let Some(grain_tile) = gather_target_tile else {
            continue;
        };

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: Some(grain_tile),
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: Some(order_entity),
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::HarvestGrainForCraftOrder;
        let methods = method_registry.methods_for(AbstractTaskKind::HarvestGrainForCraftOrder);
        let chosen = methods
            .iter()
            .filter(|m| m.precondition(abstract_task, &ctx))
            .max_by(|a, b| {
                let ua = score_method_with_history(a.as_ref(), abstract_task, &ctx, &history, now);
                let ub = score_method_with_history(b.as_ref(), abstract_task, &ctx, &history, now);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(method) = chosen else {
            continue;
        };
        let chosen_id = method.id();
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Gather { tile } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    tile,
                    TaskKind::Gather,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                let kind = MemoryKind::Resource(grain_id);
                gather_claims.add(
                    tile,
                    kind,
                    actor,
                    suggested_expiry(now, (cur_tx, cur_ty), tile),
                );
                ai.active_gather_claim = Some((tile, kind));
                aq.dispatch(Task::Gather { tile });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Method for `AbstractTask::HarvestPlant`: harvest a remembered mature edible
/// plant under `AgentGoal::Farm` and deposit at faction storage. Replaces the
/// legacy `FarmFood` plan (PlanId 1) — the last live legacy plan.
///
/// `forage_food_good` carries the plant's primary harvest yield so the trailing
/// `Task::DepositToFactionStorage { resource_id, target_faction_id: None }` reflects what's about to land
/// in the agent's hands (informational — the deposit executor itself dumps
/// everything in inventory regardless). Utility is `UTIL_BASELINE` (1.0) with a
/// full-trip distance discount across agent → plant → storage when both ctx tiles
/// are populated.
pub struct HarvestMaturePlantForStorageMethod;

impl Method for HarvestMaturePlantForStorageMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        if !matches!(abstract_task, AbstractTask::HarvestPlant) {
            return false;
        }
        ctx.gather_target_tile.is_some() && ctx.forage_food_good.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE - full_trip_penalty(ctx, ctx.gather_target_tile, ctx.gather_deposit_tile)
    }

    fn expand(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        if !matches!(abstract_task, AbstractTask::HarvestPlant) {
            return Vec::new();
        }
        let Some(tile) = ctx.gather_target_tile else {
            return Vec::new();
        };
        let Some(resource_id) = ctx.forage_food_good else {
            return Vec::new();
        };
        vec![
            Task::Gather { tile },
            Task::DepositToFactionStorage {
                resource_id,
                target_faction_id: ctx.deposit_target_faction_override,
            },
        ]
    }

    fn tech_gate(&self) -> Option<TechId> {
        Some(crate::simulation::technology::CROP_CULTIVATION)
    }

    fn name(&self) -> &'static str {
        "HarvestMaturePlantForStorage"
    }

    fn id(&self) -> MethodId {
        MethodId::HARVEST_MATURE_PLANT_FOR_STORAGE
    }
}

/// Farm-goal harvest dispatcher. Owns the harvest half of `AgentGoal::Farm`
/// (the planting half lives in `htn_plant_from_storage_dispatch_system`).
/// Replaces the last live legacy plan, `FarmFood` (PlanId 1) +
/// `[StepId(1) FarmFarmland, StepId(12) DepositGoods]`.
///
/// Gates on `AgentGoal::Farm` + Learned `CROP_CULTIVATION` + non-SOLO + no
/// `ActivePlan` + Idle. Resolves [`FarmScope`] for the worker:
///
/// - **Communal / Private**: walks the assigned plot's rect via `PlantMap`,
///   picking the nearest live mature plant (Chebyshev). Private routes the
///   trailing `Task::DepositToFactionStorage` to household storage via
///   `deposit_target_faction_override = Some(household_id)`.
/// - **Bootstrap**: falls back to the legacy
///   `GatherKnowledge::nearest_target_tile(MemoryKind::AnyEdible)` lookup so
///   pre-carving villages keep farming.
///
/// Reads the chosen plant's `harvest_yield(false).0` to thread the resource
/// through to the trailing deposit. Snapshots `gather_target_tile` +
/// `forage_food_good` + `gather_deposit_tile` (+ override) into ctx and
/// dispatches the head `Task::Gather { tile }`.
pub fn htn_harvest_plant_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    clock: Res<SimClock>,
    gk: crate::simulation::shared_knowledge::GatherKnowledge,
    farm_plot_params: FarmScopeParams,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &LodLevel,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::person::Profession>,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (
        actor,
        mut ai,
        mut aq,
        mut history,
        goal,
        transform,
        member,
        lod,
        knowledge_opt,
        household_member,
        claim_opt,
        profession_opt,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Farm) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        let has_tech = knowledge_opt
            .map(|k| k.has_learned(crate::simulation::technology::CROP_CULTIVATION))
            .unwrap_or(false);
        if !has_tech {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let scope = resolve_farm_scope(
            actor,
            member.faction_id,
            claim_opt,
            household_member,
            profession_opt,
            &farm_plot_params,
        );

        // Communal / Private restrict the mature-plant search to the
        // assigned plot's rect (no more wandering off to harvest wild
        // berries or a neighbour's bush). Bootstrap keeps the legacy
        // AnyEdible memory search.
        let harvest_candidate: Option<((i32, i32), crate::economy::resource_catalog::ResourceId)> =
            if let Some(rect) = scope.plot_rect() {
                let x0 = rect.x0;
                let y0 = rect.y0;
                let x1 = rect.x0 + rect.w as i32;
                let y1 = rect.y0 + rect.h as i32;
                let mut best: Option<(
                    i32,
                    (i32, i32),
                    crate::economy::resource_catalog::ResourceId,
                )> = None;
                for ty in y0..y1 {
                    for tx in x0..x1 {
                        let Some(entity) = plant_map.0.get(&(tx, ty)).copied() else {
                            continue;
                        };
                        let Ok(plant) = plant_query.get(entity) else {
                            continue;
                        };
                        if plant.stage != GrowthStage::Mature {
                            continue;
                        }
                        let (id, _) = plant.kind.harvest_yield(false);
                        let dist = (tx - cur_tx).abs().max((ty - cur_ty).abs());
                        if best.map_or(true, |(d, _, _)| dist < d) {
                            best = Some((dist, (tx, ty), id));
                        }
                    }
                }
                best.map(|(_, tile, id)| (tile, id))
            } else {
                gk.nearest_target_tile(
                    actor,
                    member.faction_id,
                    household_member.map(|h| h.household_id),
                    MemoryKind::AnyEdible,
                    (cur_tx, cur_ty),
                    ai.current_z,
                    now,
                )
                .and_then(|tile| {
                    let entity = plant_map.0.get(&tile).copied()?;
                    let plant = plant_query.get(entity).ok()?;
                    if plant.stage != GrowthStage::Mature {
                        return None;
                    }
                    let (id, _) = plant.kind.harvest_yield(false);
                    Some((tile, id))
                })
            };
        let Some((plant_tile, harvest_id)) = harvest_candidate else {
            continue;
        };
        let deposit_fid = scope.source_faction_id();
        let deposit_tile = storage_tile_map.nearest_for_faction(deposit_fid, plant_tile);

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: Some(plant_tile),
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: deposit_tile,
            scavenge_deposit_tile: None,
            forage_food_good: Some(harvest_id),
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: scope.deposit_override(),
        };

        let abstract_task = AbstractTask::HarvestPlant;
        let Some(pick) =
            dispatch_for_goal(&method_registry, abstract_task, &ctx, &history, now, None)
        else {
            continue;
        };
        let method = pick.method;
        let chosen_id = pick.method_id;
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Gather { tile } => {
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    tile,
                    TaskKind::Gather,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Gather { tile });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 5e-xii-a method: agent under `AgentGoal::Play` plays with another
/// Person within play radius. Single-leg expansion `[Task::Play { partner:
/// Some(e) }]`. The dispatcher routes the agent adjacent to the partner via
/// `assign_task_with_routing(... TaskKind::Play, Some(partner) ...)`.
/// `play_system` reads the partner from `ai.target_entity` and accumulates
/// willpower / social need fill on adjacency. Replaces the legacy `PlaySocial`
/// plan (PlanId 26).
pub struct PlayWithPartnerMethod;

impl Method for PlayWithPartnerMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play) && ctx.play_partner_entity.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Social play is the higher-value Play branch (the legacy plan
        // weighted PlaySocial reward_scale 1.0 vs PlaySolo 0.4); use the
        // visible-ground tier so it outranks the solo fallback.
        UTIL_VISIBLE_GROUND
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(partner) = ctx.play_partner_entity else {
            return Vec::new();
        };
        vec![Task::Play {
            partner: Some(partner),
        }]
    }

    fn name(&self) -> &'static str {
        "PlayWithPartner"
    }

    fn id(&self) -> MethodId {
        MethodId::PLAY_WITH_PARTNER
    }

    /// Gregarious agents pick partner play (vs solo play with toy /
    /// stones / etc.) more eagerly. Lift capped at 1.3 (greg=255) so
    /// `UTIL_VISIBLE_GROUND` stays under `UTIL_CLAIMED_HAUL=2.0`.
    fn disposition_lift(&self, d: crate::simulation::goal_scorers::Disposition) -> f32 {
        crate::simulation::utility_curves::disposition_lift(d.gregariousness, 0.3)
    }
}

/// Phase 5e-xii-a method: agent under `AgentGoal::Play` plays solo with a
/// held or adjacent entertainment item. Single-leg expansion
/// `[Task::Play { partner: None }]`. The dispatcher routes the agent in place
/// (or to an adjacent entertainment ground item) via
/// `assign_task_with_routing(... TaskKind::Play, None ...)`. `play_system`
/// detects the absence of a partner and falls back to the solo branch.
/// Replaces the legacy `PlaySolo` plan (PlanId 27).
pub struct PlaySoloMethod;

impl Method for PlaySoloMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play) && ctx.play_solo_eligible
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        vec![Task::Play { partner: None }]
    }

    fn name(&self) -> &'static str {
        "PlaySolo"
    }

    fn id(&self) -> MethodId {
        MethodId::PLAY_SOLO_WITH_ITEM
    }
}

/// Phase 5e-xii-b method: agent under `AgentGoal::Play` withdraws one Stone
/// from faction storage and throws it as recreation. Two-leg expansion
/// `[WithdrawMaterial { stone, 1 }, PlayThrow]`. The chain handoff in
/// `production::finish_withdraw_material` primes `task_id = TaskKind::PlayThrow`
/// once the stone is in hand (in-place — no routing); `production_system`'s
/// PlayThrow branch then consumes one stone, awards Combat XP +
/// `ActivityKind::Combat`, and bursts willpower.
///
/// Replaces the legacy `PlayByThrowingRocks` plan (PlanId 31) +
/// `[StepId(34) WithdrawStone, StepId(37) ThrowRocksAsPlay]`.
///
/// `MF_UNINTERRUPTIBLE` so a goal flip mid-fetch (e.g. willpower partially
/// recovered before the agent reaches storage) doesn't strand them with a
/// reservation. The chain ends naturally on completion or via
/// `goal_dispatch_system`'s no-plan stale-reset reseeding the same plan.
pub struct WithdrawAndThrowStonesAsPlayMethod;

impl Method for WithdrawAndThrowStonesAsPlayMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play) && ctx.play_stone_storage_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Throwing rocks is a baseline Play option — outranked by social play
        // (1.5) but on par with solo-with-item (1.0). The legacy plan had
        // bias 0.0 with state weights centred on willpower distress + combat
        // skill; mapping that to UTIL_BASELINE preserves the rough rank.
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> Vec<Task> {
        let stone_id = crate::economy::core_ids::stone();
        vec![
            Task::WithdrawMaterial {
                resource_id: stone_id,
                qty: 1,
            },
            Task::PlayThrow,
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "WithdrawAndThrowStonesAsPlay"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_THROW_STONES_AS_PLAY
    }
}

/// Phase 5e-xii-c method: agent under `AgentGoal::Play` withdraws an
/// entertainment-valued item (luxury / cloth / tools / book / toy) from
/// faction storage and plays with it solo. Two-leg expansion
/// `[WithdrawMaterial { toy, 1 }, Play { partner: None }]`. The chain handoff
/// in `production::finish_withdraw_material` primes `task_id = TaskKind::Play`
/// once the item is in hand (in-place — no routing); `play_system`'s solo
/// branch then accumulates willpower per-tick scaled by the toy's
/// `entertainment_value`.
///
/// Replaces the legacy `PlayWithStoredToy` plan (PlanId 32) +
/// `[StepId(35) WithdrawPlayItem, StepId(30) PlayWithItem]`.
///
/// `MF_UNINTERRUPTIBLE` so a goal flip mid-fetch doesn't strand the agent
/// with a reservation.
pub struct WithdrawAndPlayWithToyMethod;

impl Method for WithdrawAndPlayWithToyMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play)
            && ctx.play_toy_storage_tile.is_some()
            && ctx.play_toy_resource.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        // Solo play with a stored toy is on par with held / adjacent solo
        // play (both 1.0). Social play (1.5) wins when a partner is around;
        // otherwise this method ties with the throw-rocks fallback (1.0) and
        // is selected by registry-insertion order — toy beats throw, which
        // matches the legacy plan ranking (the toy's entertainment-scaled
        // willpower fill outproduces a one-shot throw burst).
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(rid) = ctx.play_toy_resource else {
            return Vec::new();
        };
        vec![
            Task::WithdrawMaterial {
                resource_id: rid,
                qty: 1,
            },
            Task::Play { partner: None },
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "WithdrawAndPlayWithToy"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_PLAY_WITH_TOY
    }
}

/// Phase 5e-xii-d method: agent under `AgentGoal::Play` withdraws one
/// Grain seed from faction storage and plants it on the nearest unplanted
/// grass tile as recreation. Two-leg expansion
/// `[WithdrawMaterial { grain_seed, 1 }, PlayPlant { tile }]`. The chain
/// handoff in `production::finish_withdraw_material` routes via
/// `TaskKind::PlayPlant` to the destination tile carried by the typed
/// variant once the seed is in hand. `production_system`'s Planter branch
/// (shared with `Task::Planter`) handles the actual plant on the destination
/// tile, awarding Farming XP + `ActivityKind::Farming` plus a one-shot
/// willpower burst (because `is_play = true`).
///
/// Replaces the legacy `PlayByPlanting` plan (PlanId 30) +
/// `[StepId(33) WithdrawGrainSeed, StepId(36) PlantGrainSeedAsPlay]`.
///
/// `MF_UNINTERRUPTIBLE` so a goal flip mid-fetch doesn't strand the agent
/// with a reservation.
pub struct WithdrawAndPlantGrainSeedAsPlayMethod;

impl Method for WithdrawAndPlantGrainSeedAsPlayMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play)
            && ctx.play_grain_seed_storage_tile.is_some()
            && ctx.play_plant_destination_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(tile) = ctx.play_plant_destination_tile else {
            return Vec::new();
        };
        vec![
            Task::WithdrawMaterial {
                resource_id: crate::economy::core_ids::grain_seed(),
                qty: 1,
            },
            Task::PlayPlant { tile },
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "WithdrawAndPlantGrainSeedAsPlay"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_PLANT_GRAIN_SEED_AS_PLAY
    }
}

/// Phase 5e-xii-d method: same shape as
/// `WithdrawAndPlantGrainSeedAsPlayMethod` but for Berry seeds. Replaces the
/// legacy `PlayByPlantingBerry` plan (PlanId 67) +
/// `[StepId(60) WithdrawBerrySeed, StepId(62) PlantBerrySeedAsPlay]`.
pub struct WithdrawAndPlantBerrySeedAsPlayMethod;

impl Method for WithdrawAndPlantBerrySeedAsPlayMethod {
    fn precondition(&self, abstract_task: AbstractTask, ctx: &PlannerCtx) -> bool {
        matches!(abstract_task, AbstractTask::Play)
            && ctx.play_berry_seed_storage_tile.is_some()
            && ctx.play_plant_destination_tile.is_some()
    }

    fn utility(&self, _abstract_task: AbstractTask, _ctx: &PlannerCtx) -> f32 {
        UTIL_BASELINE
    }

    fn expand(&self, _abstract_task: AbstractTask, ctx: &PlannerCtx) -> Vec<Task> {
        let Some(tile) = ctx.play_plant_destination_tile else {
            return Vec::new();
        };
        vec![
            Task::WithdrawMaterial {
                resource_id: crate::economy::core_ids::berry_seed(),
                qty: 1,
            },
            Task::PlayPlant { tile },
        ]
    }

    fn flags(&self) -> MethodFlags {
        MF_UNINTERRUPTIBLE
    }

    fn name(&self) -> &'static str {
        "WithdrawAndPlantBerrySeedAsPlay"
    }

    fn id(&self) -> MethodId {
        MethodId::WITHDRAW_AND_PLANT_BERRY_SEED_AS_PLAY
    }
}

/// Phase 5e-xii-a dispatcher. Owns `AgentGoal::Play` end-to-end via the `Play`
/// abstract task. Replaces the legacy `PlaySocial` (PlanId 26) and `PlaySolo`
/// (PlanId 27) plans.
///
/// Gates on `AgentGoal::Play` + no `ActivePlan` + Idle. Scans `SpatialIndex`
/// within `PLAY_RADIUS=12` for the nearest other Person (mirrors the legacy
/// `StepTarget::NearestPlayPartner` resolver — filters out blueprints, items,
/// animals via component checks). Checks the agent's hands for any held
/// entertainment good and within `ITEM_RADIUS=8` for visible entertainment
/// ground items (mirrors `StepTarget::NearestPlayItem`).
///
/// Argmax: `PlayWithPartnerMethod` (1.5) wins when a partner is in range;
/// `PlaySoloMethod` (1.0) is the fallback when only an entertainment item is
/// available. Both methods emit a single `Task::Play` task; routing is to the
/// partner entity (or in-place for solo). `play_system` is unchanged — it
/// reads `ai.target_entity` for partner adjacency.
pub fn htn_play_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<crate::simulation::faction::StorageReservations>,
    faction_registry: Res<FactionRegistry>,
    method_registry: Res<MethodRegistry>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    person_query: Query<(), With<crate::simulation::person::Person>>,
    bp_query: Query<(), With<crate::simulation::construction::Blueprint>>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    animal_query: Query<
        (),
        Or<(
            With<crate::simulation::animals::Wolf>,
            With<crate::simulation::animals::Deer>,
            With<crate::simulation::animals::Horse>,
        )>,
    >,
    clock: Res<SimClock>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut MethodHistory,
            &AgentGoal,
            &Transform,
            &Carrier,
            &LodLevel,
            Option<&FactionMember>,
            Option<&crate::simulation::goal_scorers::Disposition>,
        ),
        Without<Drafted>,
    >,
) {
    const PLAY_RADIUS: i32 = 12;
    const ITEM_RADIUS: i32 = 8;

    let now = clock.tick;
    for (
        agent,
        mut ai,
        mut aq,
        mut history,
        goal,
        transform,
        carrier,
        lod,
        member_opt,
        disposition_opt,
    ) in query.iter_mut()
    {
        let disposition = disposition_opt.copied().unwrap_or_default();
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Play) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Scan for nearest play partner.
        let mut play_partner_entity: Option<Entity> = None;
        let mut play_partner_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for dy in -PLAY_RADIUS..=PLAY_RADIUS {
            for dx in -PLAY_RADIUS..=PLAY_RADIUS {
                let tx = cur_tx + dx;
                let ty = cur_ty + dy;
                for &other in spatial.get(tx, ty) {
                    if other == agent {
                        continue;
                    }
                    if person_query.get(other).is_err() {
                        continue;
                    }
                    if bp_query.get(other).is_ok()
                        || item_query.get(other).is_ok()
                        || animal_query.get(other).is_ok()
                    {
                        continue;
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        play_partner_entity = Some(other);
                        play_partner_tile = Some((tx, ty));
                    }
                }
            }
        }

        // Check hands for entertainment items, then scan for adjacent ground
        // entertainment items. Mirrors the legacy `NearestPlayItem` resolver.
        let held_l = carrier
            .left
            .map(|s| s.item.resource_id.entertainment_value())
            .unwrap_or(0);
        let held_r = carrier
            .right
            .map(|s| s.item.resource_id.entertainment_value())
            .unwrap_or(0);
        let mut play_solo_eligible = held_l > 0 || held_r > 0;
        if !play_solo_eligible {
            'item_scan: for dy in -ITEM_RADIUS..=ITEM_RADIUS {
                for dx in -ITEM_RADIUS..=ITEM_RADIUS {
                    let tx = cur_tx + dx;
                    let ty = cur_ty + dy;
                    for &e in spatial.get(tx, ty) {
                        if let Ok(item) = item_query.get(e) {
                            if item.item.resource_id.entertainment_value() > 0 && item.qty > 0 {
                                play_solo_eligible = true;
                                break 'item_scan;
                            }
                        }
                    }
                }
            }
        }

        // Phase 5e-xii-b: scan faction storage for Stone so
        // `WithdrawAndThrowStonesAsPlayMethod` becomes selectable. Mirrors the
        // per-tile scan in `htn_plant_from_storage_dispatch_system` — same
        // `effective = stock - reservations` filter so two agents can't commit
        // to the same one-unit stack. SOLO agents skip (no faction storage).
        //
        // Phase 5e-xii-c: in the same pass also scan for any
        // entertainment-valued resource so `WithdrawAndPlayWithToyMethod`
        // becomes selectable. The argmax over toys picks the highest
        // `entertainment_value` (tie-break by nearest tile, stable by
        // `ResourceId.0`); ties between Stone and a toy at the same tile are
        // resolved at method-utility time (both 1.0 today; the throw method
        // wins on insertion order, mirroring `register_builtin_methods`).
        let mut play_stone_storage_tile: Option<(i32, i32)> = None;
        let mut play_toy_storage_tile: Option<(i32, i32)> = None;
        let mut play_toy_resource: Option<ResourceId> = None;
        let mut play_grain_seed_storage_tile: Option<(i32, i32)> = None;
        let mut play_berry_seed_storage_tile: Option<(i32, i32)> = None;
        if let Some(member) = member_opt {
            if member.faction_id != SOLO {
                let stone_id = crate::economy::core_ids::stone();
                let grain_seed_id = crate::economy::core_ids::grain_seed();
                let berry_seed_id = crate::economy::core_ids::berry_seed();
                if let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) {
                    let mut best_stone_dist = i32::MAX;
                    let mut best_toy_value: u8 = 0;
                    let mut best_toy_dist = i32::MAX;
                    let mut best_toy_rid: Option<ResourceId> = None;
                    let mut best_grain_dist = i32::MAX;
                    let mut best_berry_dist = i32::MAX;
                    for &(tx, ty) in tiles {
                        let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
                        // Aggregate per-tile stocks: stone (one resource),
                        // grain seed, berry seed, and toy (any non-stone
                        // resource with entertainment_value > 0, pick the
                        // highest-valued one on this tile).
                        let mut tile_stone_stock: u32 = 0;
                        let mut tile_grain_stock: u32 = 0;
                        let mut tile_berry_stock: u32 = 0;
                        let mut tile_best_toy: Option<(ResourceId, u8)> = None;
                        for &gi_entity in spatial.get(tx, ty) {
                            let Ok(gi) = item_query.get(gi_entity) else {
                                continue;
                            };
                            if gi.qty == 0 {
                                continue;
                            }
                            if gi.item.resource_id == stone_id {
                                tile_stone_stock = tile_stone_stock.saturating_add(gi.qty);
                            }
                            if gi.item.resource_id == grain_seed_id {
                                tile_grain_stock = tile_grain_stock.saturating_add(gi.qty);
                            }
                            if gi.item.resource_id == berry_seed_id {
                                tile_berry_stock = tile_berry_stock.saturating_add(gi.qty);
                            }
                            // Stone has entertainment_value > 0 (rock-throwing
                            // is recreation), but it's specifically the throw
                            // method's domain. Exclude it from the toy scan
                            // so the two methods don't double-count the same
                            // resource — `WithdrawAndPlayWithToyMethod` is for
                            // luxuries / cloth / tools / books / etc.
                            if gi.item.resource_id == stone_id {
                                continue;
                            }
                            let ent = gi.item.resource_id.entertainment_value();
                            if ent > 0 {
                                let reserved =
                                    storage_reservations.get((tx, ty), gi.item.resource_id);
                                if gi.qty.saturating_sub(reserved) == 0 {
                                    continue;
                                }
                                if tile_best_toy.map_or(true, |(_, v)| ent > v) {
                                    tile_best_toy = Some((gi.item.resource_id, ent));
                                }
                            }
                        }
                        let stone_reserved = storage_reservations.get((tx, ty), stone_id);
                        let stone_effective = tile_stone_stock.saturating_sub(stone_reserved);
                        if stone_effective > 0 && dist < best_stone_dist {
                            best_stone_dist = dist;
                            play_stone_storage_tile = Some((tx, ty));
                        }
                        let grain_reserved = storage_reservations.get((tx, ty), grain_seed_id);
                        let grain_effective = tile_grain_stock.saturating_sub(grain_reserved);
                        if grain_effective > 0 && dist < best_grain_dist {
                            best_grain_dist = dist;
                            play_grain_seed_storage_tile = Some((tx, ty));
                        }
                        let berry_reserved = storage_reservations.get((tx, ty), berry_seed_id);
                        let berry_effective = tile_berry_stock.saturating_sub(berry_reserved);
                        if berry_effective > 0 && dist < best_berry_dist {
                            best_berry_dist = dist;
                            play_berry_seed_storage_tile = Some((tx, ty));
                        }
                        if let Some((rid, value)) = tile_best_toy {
                            // Argmax: highest entertainment_value first, then
                            // nearest tile.
                            let better = value > best_toy_value
                                || (value == best_toy_value && dist < best_toy_dist);
                            if better {
                                best_toy_value = value;
                                best_toy_dist = dist;
                                best_toy_rid = Some(rid);
                                play_toy_storage_tile = Some((tx, ty));
                            }
                        }
                    }
                    play_toy_resource = best_toy_rid;
                }
            }
        }

        // Phase 5e-xii-d: nearest unplanted Grass tile destination for the
        // PlayPlant chain. The legacy plans used `StepTarget::NearestTile(GRASS_TILES)`
        // which scanned 15 tiles around the agent for any Grass tile; the
        // production_system Planter branch silently bailed if the tile was
        // already planted. Inline the unplanted-grass scan here so the
        // dispatcher only commits when an actually-plantable destination
        // exists.
        let mut play_plant_destination_tile: Option<(i32, i32)> = None;
        if play_grain_seed_storage_tile.is_some() || play_berry_seed_storage_tile.is_some() {
            const GRASS_RADIUS: i32 = 15;
            let mut best_dist = i32::MAX;
            for dy in -GRASS_RADIUS..=GRASS_RADIUS {
                for dx in -GRASS_RADIUS..=GRASS_RADIUS {
                    let tx = cur_tx + dx;
                    let ty = cur_ty + dy;
                    if plant_map.0.contains_key(&(tx, ty)) {
                        continue;
                    }
                    if chunk_map.tile_kind_at(tx, ty) != Some(crate::world::tile::TileKind::Grass) {
                        continue;
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        play_plant_destination_tile = Some((tx, ty));
                    }
                }
            }
        }

        if play_partner_entity.is_none()
            && !play_solo_eligible
            && play_stone_storage_tile.is_none()
            && play_toy_storage_tile.is_none()
            && play_plant_destination_tile.is_none()
        {
            continue;
        }

        let ctx = PlannerCtx {
            scope: ScoringScope::Geometric,
            tile: (cur_tx, cur_ty),
            faction_id: member_opt.map(|m| m.faction_id).unwrap_or(SOLO),
            faction_home: member_opt.and_then(|m| faction_registry.home_tile(m.faction_id)),
            home_bed: None,
            home_bed_tile: None,
            edible_count: 0,
            hunger: 0.0,
            nearest_storage_tile: None,
            faction_food_stock: 0,
            material_storage_tile: None,
            material_stock_for_target: 0,
            claimed_blueprint: None,
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity,
            play_solo_eligible,
            play_stone_storage_tile,
            play_toy_storage_tile,
            play_toy_resource,
            play_grain_seed_storage_tile,
            play_berry_seed_storage_tile,
            play_plant_destination_tile,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        };

        let abstract_task = AbstractTask::Play;
        let Some(pick) = dispatch_for_goal(
            &method_registry,
            abstract_task,
            &ctx,
            &history,
            now,
            Some(disposition),
        ) else {
            continue;
        };
        let method = pick.method;
        let chosen_id = pick.method_id;
        ai.active_method = Some(chosen_id);
        let mut tasks = method.expand(abstract_task, &ctx);
        if tasks.is_empty() {
            ai.active_method = None;
            continue;
        }
        let head = tasks.remove(0);
        match head {
            Task::Play { partner } => {
                let dest = match (partner, play_partner_tile) {
                    (Some(_), Some(tile)) => tile,
                    // Solo play: route to current tile (in-place).
                    _ => (cur_tx, cur_ty),
                };
                let dispatched = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    dest,
                    TaskKind::Play,
                    partner,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if !dispatched {
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                aq.dispatch(Task::Play { partner });
            }
            Task::WithdrawMaterial {
                resource_id: head_resource,
                qty,
            } => {
                // Phase 5e-xii-b/c: storage-fed Play chains. Routes the agent
                // to the storage tile picked at decision time, reserves the qty
                // so concurrent dispatchers can't commit to the same stack,
                // and dispatches the typed head. The chain handoff in
                // `production::finish_withdraw_material`'s
                // `Task::PlayThrow` / `Task::Play { partner: None }` arms
                // primes the legacy channel for the in-place play action once
                // the resource is in hand. Dispatcher selects the storage tile
                // by matching the head resource: stone → throw-rocks tile;
                // anything else → toy tile.
                let storage_tile_opt = if head_resource == crate::economy::core_ids::stone() {
                    play_stone_storage_tile
                } else if head_resource == crate::economy::core_ids::grain_seed() {
                    play_grain_seed_storage_tile
                } else if head_resource == crate::economy::core_ids::berry_seed() {
                    play_berry_seed_storage_tile
                } else {
                    play_toy_storage_tile
                };
                let Some(storage_tile) = storage_tile_opt else {
                    ai.active_method = None;
                    continue;
                };
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
                    ai.active_method = None;
                    history.push(chosen_id, MethodOutcome::FailedRouting, now);
                    continue;
                }
                storage_reservations.add(storage_tile, head_resource, qty as u32);
                ai.reserved_tile = storage_tile;
                ai.reserved_resource = Some(head_resource);
                ai.reserved_qty = qty;
                aq.dispatch(Task::WithdrawMaterial {
                    resource_id: head_resource,
                    qty,
                });
            }
            _ => {
                ai.active_method = None;
                continue;
            }
        }
        for task in tasks {
            let _ = aq.enqueue(task);
        }
    }
}

/// Phase 6b-ii chain-completion. When an HTN-dispatched chain drains to
/// `Task::Idle` with an empty prefetch ring and the agent still carries an
/// `active_method` stamp, record `MethodOutcome::Success` against that method
/// and clear the stamp. Together with the per-dispatcher failure-recording
/// paths (which clear `active_method` before pushing `FailedRouting` /
/// `FailedTarget`), this completes the symmetric outcome model for
/// `MethodHistory` — failures bias `score_method_with_history` away from
/// repeated misses, successes leave the history slot ready for the next
/// dispatch decision.
///
/// Runs in `SimulationSet::Economy` after `drop_items_at_destination_system`
/// so it observes both Sequential-finishing chains (Eat / Withdraw / Gather /
/// Scavenge — those executors call `aq.advance()` in Sequential) and
/// Economy-finishing chains (DepositResource — finalised by
/// `drop_items_at_destination_system`). External preempts via `aq.cancel()`
/// at non-instrumented sites still produce a noisy Success entry; the plan's
/// failure-only bias remains the load-bearing case (per
/// `feedback_plan_history_design.md`), so the residual noise from cancel
/// paths is acceptable until success-rate weighting actually consumes it.
pub fn htn_method_completion_system(
    mut metrics: ResMut<crate::simulation::goal_scorers::DecisionMetrics>,
    mut q: Query<(
        &mut crate::simulation::person::PersonAI,
        &mut MethodHistory,
        &ActionQueue,
    )>,
    clock: Res<crate::simulation::schedule::SimClock>,
) {
    let now = clock.tick;
    for (mut ai, mut history, aq) in q.iter_mut() {
        if let Some(method_id) = ai.active_method {
            if aq.current == Task::Idle && aq.queued_is_empty() {
                history.push(method_id, MethodOutcome::Success, now);
                metrics.htn_method_successes = metrics.htn_method_successes.saturating_add(1);
                ai.active_method = None;
            }
        }
    }
}

/// Dispatcher for `Task::ClearObstacle`. An idle Build-goal agent whose
/// claimed/owned blueprint has a non-empty `pending_clear` gets routed
/// to the first listed obstacle. Runs in `ParallelB` after the build
/// dispatcher so the build dispatcher is the primary path; this system
/// catches builds whose footprint still has plants standing on it.
///
/// Two paths mirror `htn_build_claimed_blueprint_dispatch_system`:
/// - **Path A**: the agent holds a `JobClaim::Build` whose target
///   blueprint has obstacles.
/// - **Path B**: the agent owns a personal blueprint with obstacles.
///
/// Without a claim, idle members of a faction with stale obstacle-only
/// blueprints stay idle — the formal posting layer (`E2`, deferred) is
/// the long-term fix.
pub fn htn_clear_obstacle_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    obstacle_query: Query<&crate::world::spatial::Indexed>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Transform,
            Option<&crate::simulation::jobs::JobClaim>,
            Option<&crate::simulation::jobs::ClaimTarget>,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    use crate::simulation::jobs::JobKind;
    for (agent_entity, mut ai, mut aq, goal, transform, job_claim_opt, claim_target_opt, lod) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::Build) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        // Path A — JobClaim::Build target with non-empty pending_clear.
        let path_a: Option<Entity> = match (job_claim_opt, claim_target_opt) {
            (Some(claim), Some(target)) if claim.kind == JobKind::Build => {
                target.blueprint.filter(|&bp_e| {
                    bp_query
                        .get(bp_e)
                        .map(|bp| !bp.pending_clear.is_empty())
                        .unwrap_or(false)
                })
            }
            _ => None,
        };

        // Path B — personal blueprint with non-empty pending_clear.
        let path_b: Option<Entity> = if path_a.is_some() {
            None
        } else {
            bp_map.0.values().copied().find(|&bp_e| {
                bp_query
                    .get(bp_e)
                    .map(|bp| {
                        bp.personal_owner == Some(agent_entity) && !bp.pending_clear.is_empty()
                    })
                    .unwrap_or(false)
            })
        };

        let Some(bp_entity) = path_a.or(path_b) else {
            continue;
        };
        let Ok(bp) = bp_query.get(bp_entity) else {
            continue;
        };
        let Some(&obstacle_entity) = bp.pending_clear.first() else {
            continue;
        };
        let Ok(obs_indexed) = obstacle_query.get(obstacle_entity) else {
            continue;
        };
        let tile = obs_indexed.tile;

        let agent_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        let cur_chunk = ChunkCoord(
            agent_tile.0.div_euclid(CHUNK_SIZE as i32),
            agent_tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        let dispatched = assign_task_with_routing(
            &mut ai,
            agent_tile,
            cur_chunk,
            tile,
            TaskKind::ClearObstacle,
            Some(obstacle_entity),
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if dispatched {
            aq.dispatch(Task::ClearObstacle {
                entity: obstacle_entity,
                blueprint: bp_entity,
            });
            ai.active_method = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MethodHistory (Phase 6a scaffolding) ──────────────────────────────

    #[test]
    fn method_history_default_empty() {
        let h = MethodHistory::default();
        assert_eq!(h.recently_failed_count(MethodId::SLEEP, 0), 0);
        assert_eq!(
            h.recently_failed_count(MethodId::GATHER_FROM_KNOWN, 1000),
            0
        );
    }

    #[test]
    fn method_history_counts_recent_failure() {
        let mut h = MethodHistory::default();
        h.push(
            MethodId::GATHER_FROM_KNOWN,
            MethodOutcome::FailedRouting,
            50,
        );
        assert_eq!(h.recently_failed_count(MethodId::GATHER_FROM_KNOWN, 100), 1);
        // Different method — no penalty.
        assert_eq!(h.recently_failed_count(MethodId::SLEEP, 100), 0);
    }

    #[test]
    fn method_history_success_does_not_count() {
        let mut h = MethodHistory::default();
        h.push(MethodId::SLEEP, MethodOutcome::Success, 50);
        assert_eq!(h.recently_failed_count(MethodId::SLEEP, 100), 0);
    }

    #[test]
    fn method_history_expires_by_ttl() {
        let mut h = MethodHistory::default();
        h.push(MethodId::SLEEP, MethodOutcome::FailedTarget, 0);
        // Inside TTL.
        assert_eq!(
            h.recently_failed_count(MethodId::SLEEP, METHOD_HISTORY_TTL_TICKS),
            1
        );
        // Past TTL.
        assert_eq!(
            h.recently_failed_count(MethodId::SLEEP, METHOD_HISTORY_TTL_TICKS + 1),
            0
        );
    }

    #[test]
    fn method_history_ring_overwrites_oldest() {
        let mut h = MethodHistory::default();
        // Push METHOD_HISTORY_LEN+1 entries; the oldest gets evicted so the
        // count saturates at the ring length.
        for i in 0..(METHOD_HISTORY_LEN + 1) {
            h.push(MethodId::SLEEP, MethodOutcome::FailedTarget, 10 + i as u64);
        }
        assert_eq!(
            h.recently_failed_count(MethodId::SLEEP, 10 + METHOD_HISTORY_LEN as u64 + 5),
            METHOD_HISTORY_LEN as u32
        );
    }

    #[test]
    fn score_helper_subtracts_failure_penalty() {
        // Use the registered Sleep method as a stand-in: its raw utility is
        // a constant 1.0, so the helper's output is exactly
        // `1.0 - failures * METHOD_FAILURE_PENALTY`.
        let m = SleepMethod;
        let ctx = ctx_solo_in_place();
        let mut h = MethodHistory::default();

        let raw = score_method_with_history(&m, AbstractTask::Sleep, &ctx, &h, 0);
        assert!((raw - 1.0).abs() < 1e-6);

        h.push(MethodId::SLEEP, MethodOutcome::FailedRouting, 0);
        let one_failure = score_method_with_history(&m, AbstractTask::Sleep, &ctx, &h, 50);
        assert!((one_failure - (1.0 - METHOD_FAILURE_PENALTY)).abs() < 1e-6);

        h.push(MethodId::SLEEP, MethodOutcome::FailedTarget, 50);
        let two_failures = score_method_with_history(&m, AbstractTask::Sleep, &ctx, &h, 60);
        assert!((two_failures - (1.0 - 2.0 * METHOD_FAILURE_PENALTY)).abs() < 1e-6);
    }

    #[test]
    fn score_helper_ignores_expired_failures() {
        let m = SleepMethod;
        let ctx = ctx_solo_in_place();
        let mut h = MethodHistory::default();
        h.push(MethodId::SLEEP, MethodOutcome::FailedRouting, 0);
        // Past TTL — penalty should be zero.
        let s = score_method_with_history(
            &m,
            AbstractTask::Sleep,
            &ctx,
            &h,
            METHOD_HISTORY_TTL_TICKS + 1,
        );
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn score_helper_ignores_other_methods_failures() {
        // SLEEP failures should not penalise EatFromInventory's score.
        let m = EatFromInventoryMethod;
        let mut ctx = ctx_solo_in_place();
        ctx.edible_count = 1;
        ctx.hunger = 200.0;
        let mut h = MethodHistory::default();
        h.push(MethodId::SLEEP, MethodOutcome::FailedRouting, 0);
        let s = score_method_with_history(&m, AbstractTask::Eat, &ctx, &h, 50);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn registered_method_ids_are_unique() {
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        let mut ids = std::collections::HashSet::new();
        for kind in [
            AbstractTaskKind::Sleep,
            AbstractTaskKind::Eat,
            AbstractTaskKind::AcquireFood,
            AbstractTaskKind::AcquireGood,
            AbstractTaskKind::StockpileFood,
        ] {
            for m in reg.methods_for(kind) {
                assert!(ids.insert(m.id()), "duplicate MethodId for {}", m.name());
            }
        }
    }

    fn ctx_solo_in_place() -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    fn ctx_with_bed(bed: Entity, bed_tile: (i32, i32)) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    fn ctx_with_food(edible_count: u32, hunger: f32) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    fn ctx_with_storage(
        storage_tile: Option<(i32, i32)>,
        food_stock: u32,
        hunger: f32,
    ) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    fn ctx_with_material_storage(
        storage_tile: Option<(i32, i32)>,
        material_stock: u32,
    ) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    fn ctx_with_haul_claim(
        storage_tile: Option<(i32, i32)>,
        material_stock: u32,
        blueprint: Option<Entity>,
    ) -> PlannerCtx {
        ctx_with_haul_claim_at(storage_tile, material_stock, blueprint, None)
    }

    fn ctx_with_haul_claim_at(
        storage_tile: Option<(i32, i32)>,
        material_stock: u32,
        blueprint: Option<Entity>,
        blueprint_tile: Option<(i32, i32)>,
    ) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: blueprint_tile,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
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
    fn registry_reports_four_acquire_food_methods() {
        // Forage migration: `ForageFromKnownMethod` (utility 1.0) joins
        // `WithdrawFromStorageMethod` (1.0), `ScavengeFoodFromGroundMethod`
        // (1.5), and `ExploreForFoodMethod` (0.3) under AcquireFood.
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::AcquireFood), 4);
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
        assert_eq!(tasks, vec![Task::WithdrawFood { tile: (4, 7) }, Task::Eat]);
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
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            }
            .kind(),
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
        assert!(m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn withdraw_material_precondition_false_without_storage_tile() {
        let m = WithdrawMaterialFromStorageMethod;
        // Stock recorded but no reachable tile.
        let ctx = ctx_with_material_storage(None, 5);
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn withdraw_material_precondition_false_without_stock() {
        let m = WithdrawMaterialFromStorageMethod;
        let ctx = ctx_with_material_storage(Some((1, 1)), 0);
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone()
            },
            &ctx
        ));
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
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone(),
            },
            &ctx,
        );
        // qty: 1 — the single-unit acquisition contract; larger needs come
        // from chained calls or a future `FulfillClaim` abstract task.
        assert_eq!(
            tasks,
            vec![Task::WithdrawMaterial {
                resource_id: crate::economy::core_ids::stone(),
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
        let wood = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let iron = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::iron(),
            },
            &ctx,
        );
        assert_eq!(
            wood,
            vec![Task::WithdrawMaterial {
                resource_id: crate::economy::core_ids::wood(),
                qty: 1
            }]
        );
        assert_eq!(
            iron,
            vec![Task::WithdrawMaterial {
                resource_id: crate::economy::core_ids::iron(),
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
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
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
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: tile,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    #[test]
    fn gather_from_known_precondition_true_when_target_tile_known() {
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((4, 7)));
        assert!(m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn gather_from_known_precondition_false_without_target_tile() {
        let m = GatherFromKnownMethod;
        // No memory of trees / stone tiles for this agent — falls back to
        // the bare-withdraw method or `ExploreFor*`.
        let ctx = ctx_with_gather_target(None);
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
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
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        // Two-task chain: gather at the known tile, then deposit at faction
        // storage. The deposit's `good` mirrors the abstract-task payload so
        // chain integrity can be inspected at runtime.
        assert_eq!(
            tasks,
            vec![
                Task::Gather { tile: (6, 9) },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::wood(),
                    target_faction_id: None,
                },
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
        let wood = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let stone = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone(),
            },
            &ctx,
        );
        assert_eq!(
            wood,
            vec![
                Task::Gather { tile: (0, 0) },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::wood(),
                    target_faction_id: None,
                },
            ]
        );
        assert_eq!(
            stone,
            vec![
                Task::Gather { tile: (0, 0) },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::stone(),
                    target_faction_id: None,
                },
            ]
        );
    }

    #[test]
    fn gather_from_known_expand_returns_empty_without_tile() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(None);
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!(tasks.is_empty());
    }

    #[test]
    fn gather_from_known_expand_returns_empty_for_wrong_abstract_task() {
        let m = GatherFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((1, 1)));
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
    }

    fn ctx_with_scavenge_target(target: Option<Entity>, tile: Option<(i32, i32)>) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: target,
            scavenge_target_tile: tile,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    #[test]
    fn scavenge_from_ground_precondition_true_when_target_known() {
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(11)), Some((4, 7)));
        assert!(m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn scavenge_from_ground_precondition_false_without_entity() {
        let m = ScavengeFromGroundMethod;
        // Tile populated but no live ground-item entity — falls back to the
        // gather / bare-withdraw / explore methods.
        let ctx = ctx_with_scavenge_target(None, Some((4, 7)));
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn scavenge_from_ground_precondition_false_without_tile() {
        let m = ScavengeFromGroundMethod;
        // Entity recorded but no tile — the dispatcher couldn't route the
        // agent there, so the method must opt out cleanly.
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(11)), None);
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
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
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        // Two-task chain: pick up the loose item, then deposit at faction
        // storage. The deposit's `good` mirrors the abstract-task payload so
        // chain integrity can be inspected at runtime.
        assert_eq!(
            tasks,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::wood(),
                    target_faction_id: None,
                },
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
        let wood = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let stone = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone(),
            },
            &ctx,
        );
        assert_eq!(
            wood,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::wood(),
                    target_faction_id: None,
                },
            ]
        );
        assert_eq!(
            stone,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::stone(),
                    target_faction_id: None,
                },
            ]
        );
    }

    #[test]
    fn scavenge_from_ground_expand_returns_empty_without_target() {
        // Defensive: a caller that skips the precondition still gets a sane
        // empty-vec answer rather than a panic.
        let m = ScavengeFromGroundMethod;
        let ctx = ctx_with_scavenge_target(None, Some((1, 1)));
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!(tasks.is_empty());

        // Also defensive: target entity present but tile missing.
        let ctx = ctx_with_scavenge_target(Some(Entity::from_raw(7)), None);
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
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
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: target,
            scavenge_target_tile: tile,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
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
        let ctx = ctx_with_food_scavenge_target(Some(Entity::from_raw(11)), Some((1, 1)), 220.0);
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
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
        let ctx = ctx_with_food_scavenge_target(Some(Entity::from_raw(7)), Some((1, 1)), 220.0);
        let tasks = m.expand(AbstractTask::Eat, &ctx);
        assert!(tasks.is_empty());
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
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
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
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
        assert!(
            u < 1.0,
            "ExploreForFood utility {} should be below WithdrawFromStorage's 1.0",
            u
        );
        assert!(
            u < 1.5,
            "ExploreForFood utility {} should be below ScavengeFoodFromGround's 1.5",
            u
        );
        assert!(
            u > 0.0,
            "ExploreForFood utility {} should be positive (the fallback still beats no method)",
            u
        );
    }

    #[test]
    fn explore_for_food_expands_to_single_explore_task_for_food() {
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        assert_eq!(
            tasks,
            vec![Task::Explore {
                kind: MemoryKind::AnyEdible
            }]
        );
    }

    #[test]
    fn explore_for_food_expand_returns_empty_for_wrong_abstract_task() {
        let m = ExploreForFoodMethod;
        let ctx = ctx_with_storage(None, 0, 220.0);
        assert!(m
            .expand(
                AbstractTask::AcquireGood {
                    resource_id: crate::economy::core_ids::wood()
                },
                &ctx
            )
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
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: None,
            scavenge_target_tile: None,
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    #[test]
    fn explore_for_material_precondition_true_for_wood() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
    }

    #[test]
    fn explore_for_material_precondition_true_for_stone() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone()
            },
            &ctx
        ));
    }

    #[test]
    fn explore_for_material_precondition_false_for_unsupported_good() {
        // Iron / Fruit / etc. don't have a corresponding gather goal in the
        // legacy registry, so there's no `MemoryKind` mapping and Explore
        // doesn't apply. The method opts out cleanly rather than expanding
        // with a default kind.
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::iron()
            },
            &ctx
        ));
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::fruit()
            },
            &ctx
        ));
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
        let u = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!(
            u < 1.0,
            "ExploreForMaterial utility {} should be below 1.0",
            u
        );
        assert!(
            u > 0.0,
            "ExploreForMaterial utility {} should be positive",
            u
        );
    }

    #[test]
    fn explore_for_material_expands_to_single_explore_task_for_wood() {
        let m = ExploreForMaterialMethod;
        let ctx = ctx_empty();
        let tasks = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert_eq!(
            tasks,
            vec![Task::Explore {
                kind: MemoryKind::wood()
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
        let wood = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let stone = m.expand(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::stone(),
            },
            &ctx,
        );
        assert_eq!(
            wood,
            vec![Task::Explore {
                kind: MemoryKind::wood()
            }]
        );
        assert_eq!(
            stone,
            vec![Task::Explore {
                kind: MemoryKind::stone()
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
            .expand(
                AbstractTask::AcquireGood {
                    resource_id: crate::economy::core_ids::iron()
                },
                &ctx
            )
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
        let p = dist_penalty_raw((0, 0), Some((30, 0)));
        assert!((p - MAX_DIST_PENALTY).abs() < 1e-6);
    }

    #[test]
    fn dist_penalty_zero_for_no_target() {
        // ctx fields default to None when the dispatcher hasn't populated
        // them — methods read at base utility in that case.
        assert_eq!(dist_penalty_raw((0, 0), None), 0.0);
    }

    // ── Time-of-day + fatigue weighted distance penalty ─────────────────
    //
    // When `ctx.scope == ContextAware`, `dist_penalty(ctx, target)` scales
    // the geometric penalty by a time-of-day multiplier and a fatigue
    // multiplier. Day + fatigue=0 must match the geometric baseline so
    // existing daytime ranking is unchanged. Dusk ramps with daylight
    // remaining. Night raises the cap to `MAX_DIST_PENALTY_NIGHT (1.50)`
    // so a 1.5-utility scavenge method drops below the 0.3 explore floor.
    // Fatigue=1 doubles the effective penalty.

    fn ctx_with_scope(scope: ScoringScope) -> PlannerCtx {
        let mut c = ctx_solo_in_place();
        c.scope = scope;
        c
    }

    #[test]
    fn weighted_dist_penalty_baseline_matches_geometric() {
        // Day phase + zero fatigue must equal the legacy geometric value
        // for any distance — this is the no-regression guarantee for
        // existing daytime ranking.
        let target = Some((10, 0));
        let raw = dist_penalty_raw((0, 0), target);
        let ctx = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Day,
            dusk_remaining: 1.0,
            fatigue: 0.0,
        });
        let weighted = dist_penalty(&ctx, target);
        assert!((raw - weighted).abs() < 1e-6);
    }

    #[test]
    fn weighted_dist_penalty_dusk_ramps_with_remaining_light() {
        // Same distance, lower daylight remaining → larger penalty (more
        // hesitation about long walks as evening sets in).
        let target = Some((8, 0));
        let early_dusk = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Dusk,
            dusk_remaining: 0.9,
            fatigue: 0.0,
        });
        let late_dusk = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Dusk,
            dusk_remaining: 0.1,
            fatigue: 0.0,
        });
        let p_early = dist_penalty(&early_dusk, target);
        let p_late = dist_penalty(&late_dusk, target);
        assert!(
            p_late > p_early,
            "late dusk penalty {} should exceed early dusk {}",
            p_late,
            p_early
        );
    }

    #[test]
    fn weighted_dist_penalty_night_drops_method_below_explore() {
        // At night, a 16-tile scavenge target's penalty must exceed
        // (UTIL_VISIBLE_GROUND - UTIL_EXPLORE_FALLBACK) = 1.2 so the
        // weighted score drops below the 0.3 explore fallback.
        // 16 * 0.02 * 4.0 = 1.28 > 1.20.
        let target = Some((16, 0));
        let night = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Night,
            dusk_remaining: 1.0,
            fatigue: 0.0,
        });
        let p = dist_penalty(&night, target);
        let needed_drop = UTIL_VISIBLE_GROUND - UTIL_EXPLORE_FALLBACK;
        assert!(
            p > needed_drop,
            "night penalty {} should exceed scavenge→explore margin {}",
            p,
            needed_drop
        );
        // Capped at MAX_DIST_PENALTY_NIGHT, not the daytime cap.
        assert!(p <= MAX_DIST_PENALTY_NIGHT + 1e-6);
        assert!(p > MAX_DIST_PENALTY);
    }

    #[test]
    fn weighted_dist_penalty_fatigue_doubles_at_full_drain() {
        // fatigue=1.0 → fatigue_mul = 2.0 → penalty doubles for the same
        // distance + phase, as long as we stay under the cap.
        let target = Some((4, 0)); // 4 tiles → 0.08 baseline, room under cap
        let rested = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Day,
            dusk_remaining: 1.0,
            fatigue: 0.0,
        });
        let exhausted = ctx_with_scope(ScoringScope::ContextAware {
            time_phase: TimePhase::Day,
            dusk_remaining: 1.0,
            fatigue: 1.0,
        });
        let p_rested = dist_penalty(&rested, target);
        let p_exhausted = dist_penalty(&exhausted, target);
        assert!((p_exhausted - 2.0 * p_rested).abs() < 1e-6);
    }

    #[test]
    fn calendar_time_phase_buckets_match_constants() {
        // Spot-check the day-cycle phase cuts. Calendar at default
        // ticks_per_day=3600 — phases per PHASE_*_START constants
        // (Dawn 0.0–0.05, Day 0.05–0.65, Dusk 0.65–0.85, Night 0.85–1.0).
        let mut cal = crate::world::seasons::Calendar::default();
        cal.ticks_this_day = 100; // ~0.028 → Dawn
        assert_eq!(cal.time_phase(), TimePhase::Dawn);
        cal.ticks_this_day = 800; // ~0.222 → Day
        assert_eq!(cal.time_phase(), TimePhase::Day);
        cal.ticks_this_day = 1800; // 0.5 → Day
        assert_eq!(cal.time_phase(), TimePhase::Day);
        cal.ticks_this_day = 2500; // ~0.694 → Dusk
        assert_eq!(cal.time_phase(), TimePhase::Dusk);
        cal.ticks_this_day = 3300; // ~0.917 → Night
        assert_eq!(cal.time_phase(), TimePhase::Night);
    }

    #[test]
    fn withdraw_from_storage_utility_decreases_with_distance() {
        let m = WithdrawFromStorageMethod;
        let near = ctx_with_storage(Some((1, 0)), 5, 220.0);
        let far = ctx_with_storage(Some((10, 0)), 5, 220.0);
        let u_near = m.utility(AbstractTask::AcquireFood, &near);
        let u_far = m.utility(AbstractTask::AcquireFood, &far);
        assert!(
            u_near > u_far,
            "near {} should outscore far {}",
            u_near,
            u_far
        );
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
        assert!(
            u_scav > u_wd,
            "scavenge {} should still beat withdraw {}",
            u_scav,
            u_wd
        );
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
        let u_near = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &near,
        );
        let u_far = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &far,
        );
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
        let u_haul = haul.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let u_bare = bare.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &bare_ctx,
        );
        assert!(
            u_haul > u_bare,
            "haul {} should beat bare-withdraw {}",
            u_haul,
            u_bare
        );
    }

    // ── Full-trip distance discount (Phase 5c-ii-d-vii) ──────────────────
    //
    // `WithdrawAndHaulToBlueprintMethod` discounts on agent→storage *plus*
    // storage→blueprint when both tiles are in ctx. Tests pin (a) closer
    // blueprints outscore farther ones for the same storage; (b) the cap
    // still preserves the haul-vs-bare-withdraw inter-method ranking; (c) a
    // missing blueprint tile silently falls back to the storage-only signal
    // (same numeric output as the 5c-ii-d-v shape).

    #[test]
    fn haul_closer_blueprint_outscores_farther_blueprint_same_storage() {
        // Same agent + same storage; only the blueprint tile differs.
        // Agent at (0,0), storage at (5,0): agent→storage = 5 tiles.
        // Near blueprint at (10,0): storage→bp = 5 tiles, total 10 → 0.20.
        // Far  blueprint at (20,0): storage→bp = 15 tiles, total 20 → 0.30 cap.
        let m = WithdrawAndHaulToBlueprintMethod;
        let bp = Entity::from_raw(99);
        let near = ctx_with_haul_claim_at(Some((5, 0)), 5, Some(bp), Some((10, 0)));
        let far = ctx_with_haul_claim_at(Some((5, 0)), 5, Some(bp), Some((20, 0)));
        let u_near = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &near,
        );
        let u_far = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &far,
        );
        assert!(
            u_near > u_far,
            "near-bp {} should outscore far-bp {} when storage is identical",
            u_near,
            u_far
        );
    }

    #[test]
    fn haul_full_trip_falls_back_to_storage_when_blueprint_tile_missing() {
        // `claimed_blueprint = Some` but `claimed_blueprint_tile = None`
        // (e.g. the blueprint despawned between dispatch and ctx-build).
        // Method must fall back to the 5c-ii-d-v shape (storage-only
        // distance) rather than skipping the discount entirely.
        let m = WithdrawAndHaulToBlueprintMethod;
        let bp = Entity::from_raw(7);
        // storage at chebyshev=10 from agent. Storage-only path: 2.0 - 0.20.
        let ctx = ctx_with_haul_claim(Some((10, 0)), 5, Some(bp));
        let u = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!((u - (2.0 - 0.20)).abs() < 1e-6, "expected 1.80, got {}", u);
    }

    #[test]
    fn haul_full_trip_capped_at_max_penalty() {
        // Agent at (0,0), storage at (20,0), blueprint at (40,0):
        // total chebyshev = 20 + 20 = 40 tiles, raw penalty 0.80, capped at
        // MAX_DIST_PENALTY = 0.30. Utility = 2.0 - 0.30 = 1.70.
        let m = WithdrawAndHaulToBlueprintMethod;
        let bp = Entity::from_raw(7);
        let ctx = ctx_with_haul_claim_at(Some((20, 0)), 5, Some(bp), Some((40, 0)));
        let u = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!(
            (u - (2.0 - MAX_DIST_PENALTY)).abs() < 1e-6,
            "expected {}, got {}",
            2.0 - MAX_DIST_PENALTY,
            u
        );
    }

    #[test]
    fn haul_full_trip_cap_preserves_ranking_over_bare_withdraw() {
        // Cap-preserves-ranking after the full-trip switch: even with both
        // legs at max distance (40-tile total clamped to 0.30), the haul
        // method still beats a zero-distance bare withdraw.
        let haul = WithdrawAndHaulToBlueprintMethod;
        let bp = Entity::from_raw(99);
        let ctx = ctx_with_haul_claim_at(Some((20, 0)), 5, Some(bp), Some((40, 0)));
        let bare = WithdrawMaterialFromStorageMethod;
        let bare_ctx = ctx_with_material_storage(Some((0, 0)), 5);
        let u_haul = haul.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let u_bare = bare.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &bare_ctx,
        );
        assert!(
            u_haul > u_bare,
            "full-trip haul {} should still beat bare-withdraw {}",
            u_haul,
            u_bare
        );
    }

    #[test]
    fn gather_from_known_utility_decreases_with_distance() {
        let m = GatherFromKnownMethod;
        let near = ctx_with_gather_target(Some((2, 0)));
        let far = ctx_with_gather_target(Some((12, 0)));
        let u_near = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &near,
        );
        let u_far = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &far,
        );
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
        let u_scav = scav.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        let u_gath = gath.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &ctx,
        );
        assert!(u_scav > u_gath);
    }

    #[test]
    fn scavenge_from_ground_closer_target_wins_over_farther() {
        let m = ScavengeFromGroundMethod;
        let near = ctx_with_scavenge_target(Some(Entity::from_raw(1)), Some((2, 0)));
        let far = ctx_with_scavenge_target(Some(Entity::from_raw(2)), Some((12, 0)));
        let u_near = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &near,
        );
        let u_far = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &far,
        );
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

    // ── ScavengeFoodForStorageMethod (Phase 5c-ii-d-vi) ───────────────────
    //
    // Sibling of `ScavengeFoodFromGroundMethod` under `StockpileFood`. Same
    // ctx fields plus `scavenge_food_good`; expansion ends in
    // `DepositToFactionStorage` rather than `Eat`. No hunger gate — chief
    // -driven storage-fill fires regardless of personal hunger.

    fn ctx_with_food_scavenge_for_storage(
        target: Option<Entity>,
        tile: Option<(i32, i32)>,
        good: Option<crate::economy::resource_catalog::ResourceId>,
    ) -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: target,
            scavenge_target_tile: tile,
            scavenge_food_good: good,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: None,
            prey_target_tile: None,
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: None,
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: false,
            deposit_target_faction_override: None,
        }
    }

    #[test]
    fn registry_reports_three_stockpile_food_methods() {
        // Forage migration: `ForageFromKnownForStorageMethod` (utility 1.0)
        // joins `ScavengeFoodForStorageMethod` (1.5) and
        // `ExploreForFoodForStorageMethod` (0.3) under StockpileFood.
        let mut reg = MethodRegistry::default();
        register_builtin_methods(&mut reg);
        assert_eq!(reg.method_count(AbstractTaskKind::StockpileFood), 3);
    }

    #[test]
    fn scavenge_food_for_storage_precondition_true_when_target_and_good_known() {
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(11)),
            Some((4, 7)),
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_precondition_false_without_entity() {
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            None,
            Some((4, 7)),
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(!m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_precondition_false_without_tile() {
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(11)),
            None,
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(!m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_precondition_false_without_good() {
        // The good payload is the deposit's parameter — without it the chain
        // can't know what to deposit, so the method opts out cleanly even
        // though entity + tile are populated.
        let m = ScavengeFoodForStorageMethod;
        let ctx =
            ctx_with_food_scavenge_for_storage(Some(Entity::from_raw(11)), Some((4, 7)), None);
        assert!(!m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_precondition_false_for_wrong_abstract_task() {
        // Defensive: AcquireFood / AcquireGood / Sleep / Eat all rejected
        // even when every storage ctx field is populated.
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(11)),
            Some((1, 1)),
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_no_hunger_gate() {
        // The whole point of the StockpileFood split: the chief-driven case
        // fires even when the worker isn't hungry. Hunger 0 + populated
        // scavenge fields → precondition true.
        let m = ScavengeFoodForStorageMethod;
        let mut ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(11)),
            Some((4, 7)),
            Some(crate::economy::core_ids::fruit()),
        );
        ctx.hunger = 0.0;
        assert!(m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn scavenge_food_for_storage_expands_to_scavenge_then_deposit() {
        let m = ScavengeFoodForStorageMethod;
        let target = Entity::from_raw(13);
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(target),
            Some((6, 9)),
            Some(crate::economy::core_ids::fruit()),
        );
        let tasks = m.expand(AbstractTask::StockpileFood, &ctx);
        assert_eq!(
            tasks,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::fruit(),
                    target_faction_id: None,
                },
            ]
        );
    }

    #[test]
    fn scavenge_food_for_storage_threads_good_through_to_deposit() {
        // Mirrors the cross-good parameterisation contract from
        // `scavenge_from_ground_threads_good_through_to_deposit`. Round-trip
        // Fruit + Meat in the same test to prove the good payload threads
        // through rather than being short-circuited on a hardcoded value.
        let m = ScavengeFoodForStorageMethod;
        let target = Entity::from_raw(21);
        let fruit_ctx = ctx_with_food_scavenge_for_storage(
            Some(target),
            Some((0, 0)),
            Some(crate::economy::core_ids::fruit()),
        );
        let meat_ctx = ctx_with_food_scavenge_for_storage(
            Some(target),
            Some((0, 0)),
            Some(crate::economy::core_ids::meat()),
        );
        let fruit = m.expand(AbstractTask::StockpileFood, &fruit_ctx);
        let meat = m.expand(AbstractTask::StockpileFood, &meat_ctx);
        assert_eq!(
            fruit,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::fruit(),
                    target_faction_id: None,
                },
            ]
        );
        assert_eq!(
            meat,
            vec![
                Task::Scavenge { target },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::meat(),
                    target_faction_id: None,
                },
            ]
        );
    }

    #[test]
    fn scavenge_food_for_storage_expand_returns_empty_without_target_or_good() {
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            None,
            Some((1, 1)),
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(m.expand(AbstractTask::StockpileFood, &ctx).is_empty());
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(7)),
            None,
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(m.expand(AbstractTask::StockpileFood, &ctx).is_empty());
        let ctx = ctx_with_food_scavenge_for_storage(Some(Entity::from_raw(7)), Some((1, 1)), None);
        assert!(m.expand(AbstractTask::StockpileFood, &ctx).is_empty());
    }

    #[test]
    fn scavenge_food_for_storage_expand_returns_empty_for_wrong_abstract_task() {
        let m = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(7)),
            Some((1, 1)),
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(m.expand(AbstractTask::AcquireFood, &ctx).is_empty());
        assert!(m
            .expand(
                AbstractTask::AcquireGood {
                    resource_id: crate::economy::core_ids::wood()
                },
                &ctx
            )
            .is_empty());
    }

    // ── ExploreForFoodForStorageMethod (Phase 5c-ii-d-vi) ─────────────────
    //
    // Mirrors `ExploreForFoodMethod` but under `StockpileFood` and with no
    // hunger gate. Utility 0.3 (loses to the concrete scavenge method).

    #[test]
    fn explore_for_food_for_storage_precondition_true_for_stockpile_food() {
        let m = ExploreForFoodForStorageMethod;
        let ctx = ctx_empty();
        assert!(m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn explore_for_food_for_storage_precondition_false_for_wrong_abstract_task() {
        let m = ExploreForFoodForStorageMethod;
        let ctx = ctx_empty();
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn explore_for_food_for_storage_no_hunger_gate() {
        let m = ExploreForFoodForStorageMethod;
        let mut ctx = ctx_empty();
        ctx.hunger = 0.0;
        // Storage-fill fires regardless of hunger — that's the whole point of
        // the StockpileFood split.
        assert!(m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn explore_for_food_for_storage_utility_below_concrete_method() {
        // Pin the 0.3 < 1.5 (scavenge) ordering so a future tuning PR can't
        // silently invert the fallback ranking.
        let exp = ExploreForFoodForStorageMethod;
        let scav = ScavengeFoodForStorageMethod;
        let ctx = ctx_with_food_scavenge_for_storage(
            Some(Entity::from_raw(1)),
            Some((30, 30)), // max-penalty distance
            Some(crate::economy::core_ids::fruit()),
        );
        let u_exp = exp.utility(AbstractTask::StockpileFood, &ctx);
        let u_scav = scav.utility(AbstractTask::StockpileFood, &ctx);
        assert!(
            u_exp < u_scav,
            "explore {} should lose to scavenge {}",
            u_exp,
            u_scav
        );
    }

    #[test]
    fn explore_for_food_for_storage_expands_to_explore_food() {
        let m = ExploreForFoodForStorageMethod;
        let ctx = ctx_empty();
        let tasks = m.expand(AbstractTask::StockpileFood, &ctx);
        assert_eq!(
            tasks,
            vec![Task::Explore {
                kind: MemoryKind::AnyEdible
            }]
        );
    }

    #[test]
    fn explore_for_food_for_storage_expand_returns_empty_for_wrong_abstract_task() {
        let m = ExploreForFoodForStorageMethod;
        let ctx = ctx_empty();
        assert!(m.expand(AbstractTask::AcquireFood, &ctx).is_empty());
        assert!(m
            .expand(
                AbstractTask::AcquireGood {
                    resource_id: crate::economy::core_ids::wood()
                },
                &ctx
            )
            .is_empty());
        assert!(m.expand(AbstractTask::Sleep, &ctx).is_empty());
        assert!(m.expand(AbstractTask::Eat, &ctx).is_empty());
    }

    #[test]
    fn abstract_task_kind_round_trips_for_stockpile_food() {
        assert_eq!(
            AbstractTask::StockpileFood.kind(),
            AbstractTaskKind::StockpileFood
        );
    }

    // ── Forage methods (Phase 5d-i) ────────────────────────────────────────

    #[test]
    fn forage_from_known_precondition_true_when_target_tile_known() {
        let m = ForageFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((4, 7)));
        assert!(m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn forage_from_known_precondition_false_without_target_tile() {
        let m = ForageFromKnownMethod;
        let ctx = ctx_with_gather_target(None);
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
    }

    #[test]
    fn forage_from_known_precondition_false_for_wrong_abstract_task() {
        let m = ForageFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((1, 1)));
        // Defensive: only AcquireFood drives this method. AcquireGood would
        // double-fire alongside `GatherFromKnownMethod` if this gate slipped.
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
        assert!(!m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn forage_from_known_expands_to_gather_then_eat_chain() {
        let m = ForageFromKnownMethod;
        let ctx = ctx_with_gather_target(Some((6, 9)));
        let tasks = m.expand(AbstractTask::AcquireFood, &ctx);
        // Two-task chain: gather at the known plant, then eat in place.
        // The trailing `Eat` is what makes this method differ from
        // `ForageFromKnownForStorageMethod` (whose chain ends in
        // `DepositToFactionStorage`).
        assert_eq!(tasks, vec![Task::Gather { tile: (6, 9) }, Task::Eat]);
    }

    #[test]
    fn forage_from_known_utility_at_baseline_with_zero_distance() {
        let m = ForageFromKnownMethod;
        // Same tile as agent → zero distance → no penalty → exactly baseline.
        let ctx = ctx_with_gather_target(Some((0, 0)));
        let u = m.utility(AbstractTask::AcquireFood, &ctx);
        assert!((u - UTIL_BASELINE).abs() < 1e-6);
    }

    #[test]
    fn forage_from_known_closer_target_outscores_farther() {
        let m = ForageFromKnownMethod;
        let near = ctx_with_gather_target(Some((1, 0)));
        let far = ctx_with_gather_target(Some((20, 0)));
        let u_near = m.utility(AbstractTask::AcquireFood, &near);
        let u_far = m.utility(AbstractTask::AcquireFood, &far);
        assert!(u_near > u_far, "near {} should beat far {}", u_near, u_far);
    }

    fn ctx_with_forage_for_storage(
        gather: Option<(i32, i32)>,
        deposit: Option<(i32, i32)>,
        good: Option<crate::economy::resource_catalog::ResourceId>,
    ) -> PlannerCtx {
        let mut ctx = ctx_with_gather_target(gather);
        ctx.gather_deposit_tile = deposit;
        ctx.forage_food_good = good;
        ctx
    }

    #[test]
    fn forage_from_known_for_storage_precondition_true_with_target_and_good() {
        let m = ForageFromKnownForStorageMethod;
        let ctx = ctx_with_forage_for_storage(
            Some((4, 7)),
            None,
            Some(crate::economy::core_ids::fruit()),
        );
        assert!(m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn forage_from_known_for_storage_precondition_false_without_good() {
        let m = ForageFromKnownForStorageMethod;
        // Tile populated but plant kind couldn't be resolved (e.g. dispatcher
        // saw an immature plant) — fail the precondition rather than emit a
        // chain with no deposit good.
        let ctx = ctx_with_forage_for_storage(Some((4, 7)), None, None);
        assert!(!m.precondition(AbstractTask::StockpileFood, &ctx));
    }

    #[test]
    fn forage_from_known_for_storage_precondition_false_for_wrong_abstract_task() {
        let m = ForageFromKnownForStorageMethod;
        let ctx = ctx_with_forage_for_storage(
            Some((1, 1)),
            None,
            Some(crate::economy::core_ids::grain()),
        );
        assert!(!m.precondition(AbstractTask::AcquireFood, &ctx));
        assert!(!m.precondition(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood()
            },
            &ctx
        ));
        assert!(!m.precondition(AbstractTask::Sleep, &ctx));
        assert!(!m.precondition(AbstractTask::Eat, &ctx));
    }

    #[test]
    fn forage_from_known_for_storage_expands_to_gather_then_deposit_chain() {
        let m = ForageFromKnownForStorageMethod;
        let ctx = ctx_with_forage_for_storage(
            Some((6, 9)),
            Some((0, 0)),
            Some(crate::economy::core_ids::fruit()),
        );
        let tasks = m.expand(AbstractTask::StockpileFood, &ctx);
        assert_eq!(
            tasks,
            vec![
                Task::Gather { tile: (6, 9) },
                Task::DepositToFactionStorage {
                    resource_id: crate::economy::core_ids::fruit(),
                    target_faction_id: None,
                },
            ]
        );
    }

    #[test]
    fn forage_from_known_for_storage_threads_good_through_to_deposit() {
        // The good payload from ctx flows through to the trailing deposit —
        // this is the Forage analogue of `gather_from_known_threads_good_through`,
        // except the good comes from `ctx.forage_food_good` (resolved at
        // dispatch from the plant kind) instead of the abstract task.
        let m = ForageFromKnownForStorageMethod;
        let grain_ctx = ctx_with_forage_for_storage(
            Some((1, 1)),
            None,
            Some(crate::economy::core_ids::grain()),
        );
        let fruit_ctx = ctx_with_forage_for_storage(
            Some((1, 1)),
            None,
            Some(crate::economy::core_ids::fruit()),
        );
        assert_eq!(
            m.expand(AbstractTask::StockpileFood, &grain_ctx).last(),
            Some(&Task::DepositToFactionStorage {
                resource_id: crate::economy::core_ids::grain(),
                target_faction_id: None,
            })
        );
        assert_eq!(
            m.expand(AbstractTask::StockpileFood, &fruit_ctx).last(),
            Some(&Task::DepositToFactionStorage {
                resource_id: crate::economy::core_ids::fruit(),
                target_faction_id: None,
            })
        );
    }

    #[test]
    fn forage_from_known_for_storage_full_trip_capped_preserves_ranking_over_explore() {
        // 1.0 base, 40-tile total → 0.30 cap → 0.70 effective. Still
        // outranks `ExploreForFoodForStorageMethod` (0.3 flat) so the
        // tier-preserving invariant holds for forage→deposit chains too.
        let m = ForageFromKnownForStorageMethod;
        let ctx = ctx_with_forage_for_storage(
            Some((20, 0)),
            Some((40, 0)),
            Some(crate::economy::core_ids::fruit()),
        );
        let u = m.utility(AbstractTask::StockpileFood, &ctx);
        assert!(
            u >= UTIL_EXPLORE_FALLBACK,
            "forage {} should remain above explore fallback {}",
            u,
            UTIL_EXPLORE_FALLBACK,
        );
    }

    // ── Cross-leg distance discount for gather/scavenge chains ────────────

    fn ctx_with_gather_full_trip(
        gather: Option<(i32, i32)>,
        deposit: Option<(i32, i32)>,
    ) -> PlannerCtx {
        let mut ctx = ctx_with_gather_target(gather);
        ctx.gather_deposit_tile = deposit;
        ctx
    }

    fn ctx_with_scavenge_full_trip(
        target: Option<Entity>,
        tile: Option<(i32, i32)>,
        deposit: Option<(i32, i32)>,
    ) -> PlannerCtx {
        let mut ctx = ctx_with_scavenge_target(target, tile);
        ctx.scavenge_deposit_tile = deposit;
        ctx
    }

    #[test]
    fn gather_closer_deposit_outscores_farther_deposit_same_target() {
        let m = GatherFromKnownMethod;
        let near = ctx_with_gather_full_trip(Some((5, 0)), Some((6, 0)));
        let far = ctx_with_gather_full_trip(Some((5, 0)), Some((20, 0)));
        let u_near = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &near,
        );
        let u_far = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &far,
        );
        assert!(u_near > u_far, "near {} should beat far {}", u_near, u_far);
    }

    #[test]
    fn gather_full_trip_falls_back_to_target_only_when_deposit_missing() {
        let m = GatherFromKnownMethod;
        let with_dep = ctx_with_gather_full_trip(Some((5, 0)), Some((5, 0))); // 0-cost second leg
        let no_dep = ctx_with_gather_full_trip(Some((5, 0)), None);
        let u_a = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &with_dep,
        );
        let u_b = m.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &no_dep,
        );
        assert!((u_a - u_b).abs() < 1e-6, "{} vs {}", u_a, u_b);
    }

    #[test]
    fn scavenge_full_trip_capped_preserves_ranking_over_gather() {
        // 1.5 base, 40-tile total → raw 0.80 capped at 0.30 → 1.20.
        // Still > 1.0 - 0 = 1.0 (zero-distance gather) so the cap-preserves-
        // ranking invariant survives the full-trip switch.
        let scav = ScavengeFromGroundMethod;
        let target = Entity::from_raw(1);
        let scav_ctx = ctx_with_scavenge_full_trip(Some(target), Some((20, 0)), Some((40, 0)));
        let gather_ctx = ctx_with_gather_target(Some((0, 0)));
        let u_scav = scav.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &scav_ctx,
        );
        let u_gat = GatherFromKnownMethod.utility(
            AbstractTask::AcquireGood {
                resource_id: crate::economy::core_ids::wood(),
            },
            &gather_ctx,
        );
        assert!(
            u_scav > u_gat,
            "scav {} should beat gather {}",
            u_scav,
            u_gat
        );
    }

    #[test]
    fn full_trip_penalty_helper_caps_and_falls_back() {
        assert!((full_trip_penalty_raw((0, 0), Some((5, 0)), Some((5, 0))) - 0.10).abs() < 1e-6);
        assert!(
            (full_trip_penalty_raw((0, 0), Some((20, 0)), Some((40, 0))) - MAX_DIST_PENALTY).abs()
                < 1e-6
        );
        // Fallback: no deposit → single-leg agent→target.
        assert!((full_trip_penalty_raw((0, 0), Some((10, 0)), None) - 0.20).abs() < 1e-6);
        // No target → 0.
        assert_eq!(full_trip_penalty_raw((0, 0), None, Some((5, 0))), 0.0);
    }

    // ─── Phase E: disposition-aware method scoring ────────────────

    use crate::simulation::goal_scorers::Disposition;

    fn neutral_ctx() -> PlannerCtx {
        PlannerCtx {
            scope: ScoringScope::Geometric,
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
            claimed_blueprint_tile: None,
            gather_target_tile: None,
            scavenge_target_entity: Some(Entity::from_raw(1)),
            scavenge_target_tile: Some((0, 0)),
            scavenge_food_good: None,
            gather_deposit_tile: None,
            scavenge_deposit_tile: None,
            forage_food_good: None,
            butcher_site_tile: None,
            prey_target_entity: Some(Entity::from_raw(2)),
            prey_target_tile: Some((0, 0)),
            fresh_corpse_entity: None,
            fresh_corpse_tile: None,
            hunt_hearth_tile: None,
            hunt_area_tile: None,
            hunt_party_deployed: false,
            hunt_party_stale: false,
            target_craft_order: None,
            craft_output_resource: None,
            play_partner_entity: Some(Entity::from_raw(3)),
            play_solo_eligible: false,
            play_stone_storage_tile: None,
            play_toy_storage_tile: None,
            play_toy_resource: None,
            play_grain_seed_storage_tile: None,
            play_berry_seed_storage_tile: None,
            play_plant_destination_tile: None,
            personal_bp_resource: None,
            agent_has_weapon: true,
            deposit_target_faction_override: None,
        }
    }

    fn loner() -> Disposition {
        Disposition {
            gregariousness: 10,
            martial: 10,
            ..Disposition::default()
        }
    }

    fn gregarious() -> Disposition {
        Disposition {
            gregariousness: 240,
            martial: 10,
            ..Disposition::default()
        }
    }

    fn warrior() -> Disposition {
        Disposition {
            gregariousness: 10,
            martial: 240,
            ..Disposition::default()
        }
    }

    #[test]
    fn socialize_disposition_lift_diverges_by_gregariousness() {
        let ctx = neutral_ctx();
        let h = MethodHistory::default();
        let loner_score = score_method_with_history_and_disposition(
            &SocializeWithPartnerMethod,
            AbstractTask::Socialize,
            &ctx,
            loner(),
            &h,
            0,
        );
        let greg_score = score_method_with_history_and_disposition(
            &SocializeWithPartnerMethod,
            AbstractTask::Socialize,
            &ctx,
            gregarious(),
            &h,
            0,
        );
        assert!(
            greg_score > loner_score,
            "gregarious socialize {greg_score} must outscore loner {loner_score}"
        );
        // Lift is capped sub-tier: a max-greg agent's score is at
        // most 1.3× a neutral baseline, so it stays below
        // `UTIL_VISIBLE_GROUND = 1.5` (the next tier up).
        assert!(greg_score < UTIL_VISIBLE_GROUND);
    }

    #[test]
    fn hunt_prey_disposition_lift_diverges_by_martial() {
        let ctx = neutral_ctx();
        let h = MethodHistory::default();
        let docile_score = score_method_with_history_and_disposition(
            &HuntPreyMethod,
            AbstractTask::EngagePrey,
            &ctx,
            loner(), // martial=10
            &h,
            0,
        );
        let warrior_score = score_method_with_history_and_disposition(
            &HuntPreyMethod,
            AbstractTask::EngagePrey,
            &ctx,
            warrior(), // martial=240
            &h,
            0,
        );
        assert!(
            warrior_score > docile_score,
            "martial agent's HuntPrey {warrior_score} must outscore docile {docile_score}"
        );
        // Stays under `UTIL_VISIBLE_GROUND` so PickUpFreshCorpse's
        // tier ranking against HuntPrey is preserved.
        assert!(warrior_score < UTIL_VISIBLE_GROUND);
    }

    #[test]
    fn play_with_partner_disposition_lift_diverges_by_gregariousness() {
        let ctx = neutral_ctx();
        let h = MethodHistory::default();
        let loner_score = score_method_with_history_and_disposition(
            &PlayWithPartnerMethod,
            AbstractTask::Play,
            &ctx,
            loner(),
            &h,
            0,
        );
        let greg_score = score_method_with_history_and_disposition(
            &PlayWithPartnerMethod,
            AbstractTask::Play,
            &ctx,
            gregarious(),
            &h,
            0,
        );
        assert!(greg_score > loner_score);
        // PlayWithPartner sits at UTIL_VISIBLE_GROUND (1.5); max
        // greg lift (1.3×) → 1.95. Stays under UTIL_CLAIMED_HAUL (2.0).
        assert!(greg_score < UTIL_CLAIMED_HAUL);
    }

    /// Other methods (no override) return 1.0× lift — `score_method_with_history`
    /// and `score_method_with_history_and_disposition` agree for any
    /// `disposition` value when the method uses the trait default.
    #[test]
    fn unoverridden_methods_ignore_disposition() {
        let mut ctx = neutral_ctx();
        ctx.home_bed_tile = Some((0, 0));
        ctx.faction_home = Some((0, 0));
        let h = MethodHistory::default();
        let baseline = score_method_with_history(&SleepMethod, AbstractTask::Sleep, &ctx, &h, 0);
        let with_warrior = score_method_with_history_and_disposition(
            &SleepMethod,
            AbstractTask::Sleep,
            &ctx,
            warrior(),
            &h,
            0,
        );
        let with_greg = score_method_with_history_and_disposition(
            &SleepMethod,
            AbstractTask::Sleep,
            &ctx,
            gregarious(),
            &h,
            0,
        );
        assert!((baseline - with_warrior).abs() < 1e-6);
        assert!((baseline - with_greg).abs() < 1e-6);
    }
}
