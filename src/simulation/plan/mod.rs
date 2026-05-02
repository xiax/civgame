use super::animals::{Deer, Horse, Tamed, Wolf};
use super::carry::Carrier;
use super::combat::{CombatTarget, Health};
use super::construction::{Blueprint, BlueprintMap, BuildSiteKind, CampfireMap};
use super::corpse::{Corpse, CorpseMap};
use super::crafting::{CraftOrder, CraftOrderMap};
use super::faction::{
    release_reservation, FactionMember, FactionRegistry, StorageReservations, StorageTileMap,
};
use super::goals::{AgentGoal, RescueTarget};
use super::items::{GroundItem, TargetItem};
use super::tasks::{
    assign_task_with_routing, find_nearest_edible, find_nearest_item, find_nearest_plant,
    find_nearest_tile, find_nearest_unplanted_farmland, nearest_reachable_higher_tile, TaskKind,
};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::Calendar;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use ahash::AHashMap;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use super::lod::LodLevel;
use super::memory::{AgentMemory, MemoryKind, RelationshipMemory};
use super::needs::Needs;
/// Dimensionality of the agent state vector built by `build_state_vec`.
/// See that function for the layout — each `PlanDef::state_weights` is indexed
/// the same way.
pub const STATE_DIM: usize = 41;

/// ε-greedy exploration rate applied during plan selection: with this
/// probability the agent picks a random candidate instead of the highest
/// scorer. Keeps behavior from collapsing onto a single plan.
pub(super) const PLAN_EPSILON: f32 = 0.10;

// ── State-vector indices (mirror `build_state_vec`) ───────────────────────────
// The need slots (0-5) are populated by `build_state_vec` so the state vector
// itself is complete, but no plan currently weights them: under a need-driven
// goal the triggering need is constant across all candidate plans, so weighting
// it again would just re-amplify the goal trigger. Kept addressable for future
// plans that might legitimately discriminate on a need that is *not* the goal
// trigger (e.g. SI_SLEEP on a Survive food plan).
#[allow(dead_code)]
pub(super) const SI_HUNGER: usize = 0;
#[allow(dead_code)]
pub(super) const SI_SLEEP: usize = 1;
#[allow(dead_code)]
pub(super) const SI_SHELTER: usize = 2;
#[allow(dead_code)]
pub(super) const SI_SAFETY: usize = 3;
pub(super) const SI_SOCIAL: usize = 4;
#[allow(dead_code)]
pub(super) const SI_REPRO: usize = 5;
pub(super) const SI_HAS_FOOD: usize = 6;
pub(super) const SI_HAS_WOOD: usize = 7;
pub(super) const SI_HAS_STONE: usize = 8;
pub(super) const SI_HAS_SEED: usize = 9;
#[allow(dead_code)]
pub(super) const SI_HAS_COAL: usize = 10;
#[allow(dead_code)]
pub(super) const SI_SKILL_FARMING: usize = 11;
#[allow(dead_code)]
pub(super) const SI_SKILL_MINING: usize = 12;
pub(super) const SI_SKILL_BUILDING: usize = 13;
#[allow(dead_code)]
pub(super) const SI_SKILL_TRADING: usize = 14;
pub(super) const SI_SKILL_COMBAT: usize = 15;
pub(super) const SI_SKILL_CRAFTING: usize = 16;
#[allow(dead_code)]
pub(super) const SI_SKILL_SOCIAL: usize = 17;
#[allow(dead_code)]
pub(super) const SI_SKILL_MEDICINE: usize = 18;
pub(super) const SI_SEASON_FOOD: usize = 19;
pub(super) const SI_IN_FACTION: usize = 20;
pub(super) const SI_MEM_FOOD: usize = 21;
pub(super) const SI_MEM_WOOD: usize = 22;
pub(super) const SI_MEM_STONE: usize = 23;
pub(super) const SI_WILLPOWER_DISTRESS: usize = 24;
// 25-28: plan history (2 floats per slot × PLAN_HISTORY_LEN). See build_state_vec.
// 29-32: faction storage stocks (per-good total in the agent's faction storage,
// saturated at STORAGE_SATURATE units). Lets withdraw/haul plans score on
// whether storage actually has the goods this plan wants to use, and lets
// producer plans (gather/forage/farm) self-throttle when storage is full.
pub(super) const SI_STORAGE_FOOD: usize = 29;
pub(super) const SI_STORAGE_WOOD: usize = 30;
pub(super) const SI_STORAGE_STONE: usize = 31;
pub(super) const SI_STORAGE_SEED: usize = 32;
// 33: any open craft order for this faction has unmet material deposits.
pub(super) const SI_CRAFT_ORDER_NEEDS_MATERIAL: usize = 33;
// 34 reserved.
// Source-only visibility: counts the harvestable *source* of each resource
// within VISIBILITY_RADIUS — mature edible plants, mature trees, stone tiles.
// Drives Forage/Gather/Deliver*ToCraftOrder plans that can only act on a
// source. Loose `GroundItem`s of the same good live on the ground-only slots
// below, so source and good never share a signal.
pub(super) const SI_VIS_PLANT_FOOD: usize = 35;
pub(super) const SI_VIS_TREE: usize = 36;
pub(super) const SI_VIS_STONE_TILE: usize = 37;
// Ground-item-only visibility: counts loose `GroundItem`s (food/wood/stone
// left by `harvest_ground_drops`, prior spills, combat). Drives the
// `Scavenge*` plans, which can only pick up ground items.
pub(super) const SI_VIS_GROUND_WOOD: usize = 38;
pub(super) const SI_VIS_GROUND_STONE: usize = 39;
pub(super) const SI_VIS_GROUND_FOOD: usize = 40;

pub(super) const VISIBILITY_RADIUS: i32 = 8;
pub(super) const VISIBILITY_SATURATE: u8 = 4;
use super::person::{AiState, Drafted, Person, PersonAI, PlayerOrder, Profession};
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;
use super::technology::TechId;

pub type StepId = u8;
pub type PlanId = u16;

/// Bitfield on `PlanDef::flags` describing how the candidate filter and
/// post-execution hooks should treat a plan, replacing the old per-plan-id
/// match arms scattered through `plan_execution_system` and
/// `explore_satisfaction_system`. Compose with `|` (e.g. an Explore-for-Wood
/// plan carries `PF_EXPLORE | PF_TARGETS_WOOD`).
pub type PlanFlags = u32;
pub const PF_NONE: PlanFlags = 0;
/// Inverts the visibility/memory gate: plan is selectable only when the agent
/// has neither memory nor visibility of the targeted resource. Used by the
/// `ExploreFor*` plans. Must be paired with one of `PF_TARGETS_*`.
pub const PF_EXPLORE: PlanFlags = 1 << 0;
/// Plan acts on loose `GroundItem`s, not on a source. Gates on the matching
/// `SI_VIS_GROUND_*` counter being non-zero. Must be paired with one of
/// `PF_TARGETS_*`. Used by `Scavenge*` plans.
pub const PF_SCAVENGE: PlanFlags = 1 << 1;
/// Resource selectors used together with `PF_EXPLORE` / `PF_SCAVENGE` to pick
/// the right vis counter / memory kind without per-plan-id branching.
pub const PF_TARGETS_FOOD: PlanFlags = 1 << 2;
pub const PF_TARGETS_WOOD: PlanFlags = 1 << 3;
pub const PF_TARGETS_STONE: PlanFlags = 1 << 4;
/// On timeout, drop the agent's surplus food at their feet rather than
/// silently abandoning the plan. Used by the `ReturnSurplusFood` plan.
pub const PF_DROP_FOOD_ON_TIMEOUT: PlanFlags = 1 << 5;
/// Skip the "goal changed" preemption check for this plan. The plan can still
/// be dropped by timeout, target invalidation, precondition failure, or normal
/// completion — but a transient survival-need spike won't peel a worker off a
/// committed faction task. Used by claimed-job, blueprint-haul, and
/// craft-order plans.
pub const PF_UNINTERRUPTIBLE: PlanFlags = 1 << 6;

/// Where the demand for a `WithdrawForFactionNeed` step comes from. The
/// resolver builds a `[u32; GOOD_COUNT]` "still-needed" vector from this
/// source, then looks for storage tiles that hold any of those goods.
#[derive(Copy, Clone, Debug)]
pub enum MaterialNeed {
    /// Faction-shared blueprints plus blueprints owned by this agent.
    Blueprint,
    /// Open faction `CraftOrder`s.
    CraftOrder,
    /// The good named in the agent's active `JobClaim::Haul`.
    HaulClaim,
}

/// How the resolver picks which good to commit to once it has chosen a tile.
#[derive(Copy, Clone, Debug)]
pub enum GoodSelector {
    /// Pick the good with the largest still-needed quantity that the chosen
    /// tile actually holds. Stable tiebreak by `Good as u8`. Lets the faction
    /// drive selection (most-deficient material wins) without per-good plans.
    MostDeficient,
    /// Resolver must commit to exactly this good — used internally for
    /// `MaterialNeed::HaulClaim` after the claim's good is read.
    Specific(Good),
}

#[derive(Clone, Debug)]
pub enum StepTarget {
    NearestTile(&'static [TileKind]),
    NearestItem(Good),
    NearestEdible,
    FactionCamp,
    FromMemory(MemoryKind),
    HuntPrey,
    NearestBuildSite(BuildSiteKind),
    /// Targets the nearest active Blueprint entity for this agent's faction.
    NearestBlueprint(BuildSiteKind),
    /// Targets the nearest active Blueprint entity of any kind for this agent's faction.
    NearestAnyBlueprint,
    /// Targets the nearest active Blueprint that the agent can usefully deposit
    /// into right now: at least one deposit slot still needs material AND the
    /// agent currently carries some of that good. Used by the HaulMaterials step.
    NearestBlueprintNeedingHeldMaterial,
    /// In-place: target resolves to the agent's current tile.
    SelfPosition,
    /// Nearest faction storage tile (for withdrawing food from communal stock).
    NearestFactionStorage,
    /// Unified withdraw target. Replaces the per-need / per-good variants
    /// (`*WithBlueprintMaterial`, `*WithCraftOrderMaterial`,
    /// `*ContainingForCraftOrder`, `StorageWithHaulClaimGood`). The resolver
    /// reads `MaterialNeed` to build a per-good demand vector, picks the
    /// nearest tile that holds something useful (after subtracting
    /// `StorageReservations`), uses `GoodSelector` to choose which good, and
    /// commits a `(good, qty)` intent on the agent so
    /// `withdraw_material_task_system` knows what to take.
    WithdrawForFactionNeed {
        need: MaterialNeed,
        selector: GoodSelector,
    },
    /// Nearest faction storage tile holding ≥1 of the given Good. Used by play
    /// plans that fetch a specific good (Seed, Stone) before recreating.
    NearestFactionStorageWithGood(Good),
    /// Nearest faction storage tile holding ≥1 of any good with non-zero
    /// `entertainment_value`. Used by the PlayWithStoredToy plan so an agent
    /// can grab a luxury/cloth/tool from the stockpile and play with it.
    NearestFactionStorageWithEntertainment,
    /// Nearest wild (untamed) horse entity.
    NearestWildHorse,
    /// Resolves to the attacker carried by the agent's `RescueTarget` component
    /// (set by `sound::respond_to_distress_system`). Routes the responder to the
    /// attacker's current tile and assigns the attacker as `target_entity`.
    RescueAttacker,
    /// Nearest other Person within ~12 tiles, scored by affinity then distance.
    /// Skips agents in incompatible states (Sleeping, in combat).
    NearestPlayPartner,
    /// First fallback: agent already holds an item with `entertainment_value > 0`
    /// — resolves to SelfPosition. Second fallback: nearest ground item with
    /// non-zero entertainment value within 8 tiles.
    NearestPlayItem,
    /// Resolves to a random reachable tile within ±96 of the agent's faction
    /// home (or the agent's tile if homeless). When the agent is below z=0
    /// and no random tile is reachable, falls back to the nearest reachable
    /// higher-Z tile so a stuck-underground agent climbs out.
    ExploreTile,
    /// Nearest active CraftOrder for this faction whose deposit slots still
    /// need a good the agent currently carries (in hand or in inventory).
    /// Mirrors `NearestBlueprintNeedingHeldMaterial` for the order pipeline.
    NearestCraftOrderNeedingHeldMaterial,
    /// Nearest active CraftOrder for this faction that has all deposits
    /// satisfied — i.e. the work step can begin.
    NearestSatisfiedCraftOrder,
    /// The specific blueprint named in the agent's active `JobClaim::Haul`
    /// (the destination for hauled materials). Resolves to None if the agent
    /// has no Haul claim or the blueprint despawned.
    HaulClaimBlueprint,
    /// The specific blueprint named in the agent's active `JobClaim::Build`.
    /// Resolves to None if the agent has no Build claim or the blueprint
    /// despawned.
    BuildClaimBlueprint,
    /// Home tile of the faction this agent's faction is raiding (resolved via
    /// `FactionRegistry::raid_target` then `home_tile`). Used by the Raid
    /// plan; resolves to None for solo agents or when no raid is queued.
    FactionRaidTarget,
    /// Nearest fresh `Corpse` entity within VIEW_RADIUS — used by the hunter
    /// PickUpCorpse step. Resolves to the corpse entity + its tile so the
    /// agent walks adjacent and the executor sets `carried_corpse`.
    NearestFreshCorpse,
    /// Nearest butcher site for this agent's faction. Tries `CampfireMap`
    /// (any tier — open/ringed/lined hearth) first; falls back to the
    /// faction's home tile. Resolves to the tile only — no entity target.
    NearestButcherSite,
    /// In-place equip: agent transfers one unit of `good` from inventory or
    /// hands into `Equipment.items[slot]`. Resolves to `SelfPosition`; the
    /// dispatcher writes `slot` and `good` to PersonAI fields so
    /// `equip_task_system` knows what to wield.
    EquipItem {
        slot: crate::simulation::items::EquipmentSlot,
        good: Good,
    },
    /// Hearth tile to muster at for a chief's `HuntOrder::Hunt`. Falls back
    /// to faction `home_tile`. Resolves to None when no hunt order is active.
    /// The wait-for-party state itself lives in
    /// `wait_for_party_task_system` once the agent arrives — the target only
    /// has to route them to the muster tile.
    HearthForHunt,
    /// The chief's chosen hunting-area tile (centroid of detected prey).
    /// Resolves to None when no `Hunt` order is active.
    HuntArea,
    /// Random reachable tile near `home_tile` used by the `ScoutForPrey`
    /// plan. Mirrors `ExploreTile` resolver but selects only when no `Scout`
    /// order is active. We reuse ExploreTile's resolver via this variant so
    /// scout movement matches explore movement.
    ScoutForPrey,
}

#[derive(Clone, Debug)]
pub struct StepPreconditions {
    pub requires_good: Option<(Good, u32)>,
    pub requires_any_edible: bool,
    pub min_hunger: Option<u8>,
    pub requires_carry_anything: bool,
    /// When set, this precondition is *unsatisfied* if the agent already has
    /// any quantity of the named good (in inventory or hands). Used by
    /// `AcquireHuntingSpear` so the plan is filtered out the moment the
    /// hunter is already armed — no need to fetch another spear.
    pub forbids_good: Option<Good>,
}

impl StepPreconditions {
    pub fn none() -> Self {
        Self {
            requires_good: None,
            requires_any_edible: false,
            min_hunger: None,
            requires_carry_anything: false,
            forbids_good: None,
        }
    }
    pub fn needs_good(good: Good, qty: u32) -> Self {
        Self {
            requires_good: Some((good, qty)),
            requires_any_edible: false,
            min_hunger: None,
            requires_carry_anything: false,
            forbids_good: None,
        }
    }
    /// Eat-style preconditions: agent must hold at least one edible and be at
    /// or above the given hunger threshold.
    pub fn eat_when_hungry(min_hunger: u8) -> Self {
        Self {
            requires_good: None,
            requires_any_edible: true,
            min_hunger: Some(min_hunger),
            requires_carry_anything: false,
            forbids_good: None,
        }
    }
    /// Deposit-style preconditions: agent must be carrying *something* across
    /// inventory or hands. Prevents empty-handed walks to faction storage when
    /// a prior gather step yielded nothing (target despawned, capacity full at
    /// pickup, etc.).
    pub fn carry_anything() -> Self {
        Self {
            requires_good: None,
            requires_any_edible: false,
            min_hunger: None,
            requires_carry_anything: true,
            forbids_good: None,
        }
    }
    /// Plan-gate variant: precondition fails if the agent already has any
    /// quantity of `good` across inventory + hands. Used to make
    /// `AcquireHuntingSpear` self-deselect once a hunter is armed.
    pub fn forbids(good: Good) -> Self {
        Self {
            requires_good: None,
            requires_any_edible: false,
            min_hunger: None,
            requires_carry_anything: false,
            forbids_good: Some(good),
        }
    }

    /// Returns true if the agent currently satisfies these preconditions.
    /// `equipment` is optional because non-Person agents (animals, solo
    /// non-Persons) don't carry the component; treat None as "nothing equipped."
    pub fn is_satisfied(
        &self,
        agent: &EconomicAgent,
        carrier: &Carrier,
        equipment: Option<&crate::simulation::items::Equipment>,
        hunger: f32,
    ) -> bool {
        if let Some((good, qty)) = self.requires_good {
            if agent.quantity_of(good) < qty {
                return false;
            }
        }
        if self.requires_any_edible && super::production::total_edible(agent, carrier) == 0 {
            return false;
        }
        if let Some(min) = self.min_hunger {
            if hunger < min as f32 {
                return false;
            }
        }
        if self.requires_carry_anything
            && agent.current_weight_g() == 0
            && carrier.is_empty()
        {
            return false;
        }
        if let Some(good) = self.forbids_good {
            if agent.quantity_of(good) > 0 || carrier.quantity_of_good(good) > 0 {
                return false;
            }
            // A wielded weapon/armor counts as "already armed" so the
            // AcquireHuntingSpear plan self-deselects after the equip step
            // moves the spear out of inventory and into the MainHand slot.
            if let Some(eq) = equipment {
                if eq.has_good(good) {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Clone)]
pub struct StepDef {
    pub id: StepId,
    pub task: TaskKind,
    pub target: StepTarget,
    pub preconditions: StepPreconditions,
    pub reward_scale: f32,
    /// When falling back from memory to a plant search (Food memory kind),
    /// restricts which plant kind is targeted. None = any mature plant.
    pub plant_filter: Option<PlantKind>,
    /// For Craft steps: the recipe ID in CRAFT_RECIPES to execute.
    /// Encoded into ai.target_z at dispatch time.
    pub extra: u8,
}

#[derive(Clone)]
pub struct PlanDef {
    pub id: PlanId,
    pub name: &'static str,
    pub steps: &'static [StepId],
    /// Linear weights over the state vector built by `build_state_vec`.
    /// Score contribution = dot(state, state_weights). See `SI_*` constants.
    pub state_weights: [f32; STATE_DIM],
    /// Constant baseline added to the linear score.
    pub bias: f32,
    pub serves_goals: &'static [AgentGoal],
    /// Faction must have unlocked this tech for the plan to be selectable.
    pub tech_gate: Option<TechId>,
    /// Memory kind used to compute distance penalties during plan scoring.
    pub memory_target_kind: Option<MemoryKind>,
    /// `PF_*` flags describing how the candidate filter / timeout handling
    /// should treat this plan. Default is `PF_NONE`.
    pub flags: PlanFlags,
    /// When set, candidate filter requires the agent to have this profession.
    /// Used to gate the hunter-only HuntFood and AcquireHuntingSpear plans
    /// without spreading profession checks through the scorer.
    pub requires_profession: Option<Profession>,
}

/// Build a `state_weights` array from a sparse list of (index, weight) pairs.
/// All other entries are zero.
pub(super) fn mk_weights(pairs: &[(usize, f32)]) -> [f32; STATE_DIM] {
    let mut w = [0.0f32; STATE_DIM];
    for &(i, v) in pairs {
        w[i] = v;
    }
    w
}

/// Linear plan score: dot(state, state_weights) + bias.
pub fn score_weighted(state: &[f32; STATE_DIM], plan: &PlanDef) -> f32 {
    let mut s = plan.bias;
    for i in 0..STATE_DIM {
        s += state[i] * plan.state_weights[i];
    }
    s
}

/// ε-greedy plan selection from a slice of (plan_id, score) pairs.
/// Returns the index into the slice (not the plan_id directly).
pub fn select_plan_idx(scores: &[(u16, f32)]) -> usize {
    if scores.is_empty() {
        return 0;
    }
    if fastrand::f32() < PLAN_EPSILON {
        fastrand::usize(..scores.len())
    } else {
        scores
            .iter()
            .enumerate()
            .max_by(|(_, (_, a)), (_, (_, b))| {
                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

#[derive(Resource, Default)]
pub struct StepRegistry(pub Vec<StepDef>);

#[derive(Resource, Default)]
pub struct PlanRegistry(pub Vec<PlanDef>);

/// Maps each agent entity to the plan_id currently running by their most-liked ally.
/// Written each frame by `rel_influence_system`; consumed by `plan_execution_system`.
#[derive(Resource, Default)]
pub struct RelInfluence(pub AHashMap<Entity, u16>);

/// Emitted by `plan_execution_system` when an agent's `ReturnSurplusFood` plan
/// times out (storage tile unreachable). `drop_abandoned_food_system` consumes
/// it and dumps the agent's food inventory at their feet so the surplus isn't
/// permanently bottled up.
#[derive(Event)]
pub struct DropAbandonedFoodEvent(pub Entity);

#[derive(Component)]
pub struct ActivePlan {
    pub plan_id: PlanId,
    pub current_step: u8,
    pub started_tick: u64,
    pub max_ticks: u64,
    pub reward_acc: f32,
    pub reward_scale: f32,
    pub dispatched: bool,
}

/// Outcome of a plan once it stops running. Pushed onto `PlanHistory` so the
/// NN can learn to avoid plans that just failed and reach for Explore when
/// recent attempts have all flunked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlanOutcome {
    Success,
    FailedNoTarget,
    FailedPrecondition,
    Aborted,
    Interrupted,
}

impl PlanOutcome {
    pub const fn is_failure(self) -> bool {
        !matches!(self, PlanOutcome::Success)
    }
}

pub const PLAN_HISTORY_LEN: usize = 2;

/// How long a `PlanHistory` entry remains relevant for the recent-failure
/// score penalty. Past this age the entry still occupies its ring slot but
/// is treated as absent by `recently_failed_count` (which gates the scorer
/// nudge in `plan_execution_system`). 100 ticks ≈ 5 seconds at 20 Hz.
pub const PLAN_HISTORY_TTL_TICKS: u64 = 100;

/// Per-agent ring buffer of the last few plan outcomes. Written at every
/// teardown site (success, precondition fail, resolve fail, goal change,
/// interruption). Each entry is timestamped with the SimClock tick so a
/// stale failure does not penalise a plan forever; `recently_failed_count`
/// returns only entries within `PLAN_HISTORY_TTL_TICKS` of the query tick.
#[derive(Component, Default)]
pub struct PlanHistory {
    pub entries: [Option<(PlanId, PlanOutcome, u64)>; PLAN_HISTORY_LEN],
    pub head: u8,
}

impl PlanHistory {
    pub fn push(&mut self, plan_id: PlanId, outcome: PlanOutcome, tick: u64) {
        let i = (self.head as usize) % PLAN_HISTORY_LEN;
        self.entries[i] = Some((plan_id, outcome, tick));
        self.head = ((self.head as usize + 1) % PLAN_HISTORY_LEN) as u8;
    }

    /// Number of non-expired failure entries for `plan_id`. Used as a soft
    /// score penalty in plan selection — recent failures bias against
    /// re-picking a plan, but never eliminate it from contention.
    pub fn recently_failed_count(&self, plan_id: PlanId, now: u64) -> u32 {
        self.entries
            .iter()
            .filter(|slot| {
                matches!(
                    slot,
                    Some((id, outcome, tick))
                        if *id == plan_id
                            && outcome.is_failure()
                            && now.saturating_sub(*tick) <= PLAN_HISTORY_TTL_TICKS
                )
            })
            .count() as u32
    }
}

#[derive(Clone)]
pub struct KnownPlanEntry {
    pub plan_id: PlanId,
    pub freshness: u8,
    pub innate: bool,
}

#[derive(Component, Clone)]
pub struct KnownPlans {
    pub entries: Vec<KnownPlanEntry>,
}

impl KnownPlans {
    pub fn with_innate(ids: &[PlanId]) -> Self {
        Self {
            entries: ids
                .iter()
                .map(|&id| KnownPlanEntry {
                    plan_id: id,
                    freshness: 255,
                    innate: true,
                })
                .collect(),
        }
    }

    pub fn knows(&self, id: PlanId) -> bool {
        self.entries.iter().any(|e| e.plan_id == id)
    }

    pub fn add(&mut self, id: PlanId, freshness: u8) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.plan_id == id) {
            if freshness > e.freshness {
                e.freshness = freshness;
            }
        } else {
            self.entries.push(KnownPlanEntry {
                plan_id: id,
                freshness,
                innate: false,
            });
        }
    }

    pub fn decay(&mut self) {
        for e in &mut self.entries {
            if !e.innate {
                e.freshness = e.freshness.saturating_sub(1);
            }
        }
        self.entries.retain(|e| e.innate || e.freshness > 0);
    }

    pub fn top_entries(&self, n: usize) -> Vec<(PlanId, u8)> {
        let mut sorted: Vec<(PlanId, u8)> = self
            .entries
            .iter()
            .map(|e| (e.plan_id, e.freshness))
            .collect();
        sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(n);
        sorted
    }

    pub fn receive_gossip(&mut self, id: PlanId, freshness: u8) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.plan_id == id) {
            if freshness > e.freshness {
                e.freshness = freshness;
            }
        } else {
            self.entries.push(KnownPlanEntry {
                plan_id: id,
                freshness,
                innate: false,
            });
        }
    }
}

#[derive(Component)]
pub enum PlanScoringMethod {
    /// Linear scoring: dot(state, plan.state_weights) + plan.bias + manual bonuses.
    Weighted,
    /// Pick uniformly at random from candidates.
    Random,
}

// ── Built-in step and plan definitions ────────────────────────────────────────
mod registry;
pub use registry::{register_builtin_plans, register_builtin_steps};

/// Tile kinds resolve_target walks for the Stone memory fallback. Lives in
/// `mod.rs` (not `registry.rs`) because resolve_target also reads it.
pub(super) static STONE_TILES: &[TileKind] = &[TileKind::Stone];


// ── State vector ─────────────────────────────────────────────────────────────
mod state;
pub use state::{
    build_state_vec, count_visible_ground_food, count_visible_ground_stone,
    count_visible_ground_wood, count_visible_plant_food, count_visible_stone_tiles,
    count_visible_trees,
};


// ── Target resolution ─────────────────────────────────────────────────────────

/// Unified resolver for `StepTarget::WithdrawForFactionNeed`. Builds a
/// per-good demand vector from `need`, picks the nearest faction storage tile
/// holding a useful good (after subtracting in-flight reservations), then
/// commits a `(good, qty)` withdrawal intent. Replaces four hand-rolled
/// per-target arms (blueprint / craft-order / per-good / haul-claim).
fn resolve_withdraw_for_faction_need(
    need: MaterialNeed,
    selector: GoodSelector,
    pos: (i32, i32),
    spatial: &SpatialIndex,
    storage_tile_map: &StorageTileMap,
    reservations: &StorageReservations,
    faction_id: u32,
    agent_entity: Entity,
    item_query: &Query<&GroundItem>,
    bp_map: &BlueprintMap,
    bp_query: &Query<&Blueprint>,
    co_map: &CraftOrderMap,
    co_query: &Query<&CraftOrder>,
    claim_target: Option<&crate::simulation::jobs::ClaimTarget>,
    agent: &EconomicAgent,
    carrier: &crate::simulation::carry::Carrier,
    withdraw_intent_out: &mut Option<(Good, u8)>,
) -> Option<(Option<Entity>, i16, i16)> {
    // 1. Build per-good still-needed demand from `need`. The HaulClaim path
    //    bypasses the demand vector since the claim already names the good.
    let mut still_need_by_good = [0u32; crate::economy::goods::GOOD_COUNT];
    let mut any_needed = false;
    let mut effective_selector = selector;
    match need {
        MaterialNeed::Blueprint => {
            for (_, &bp_entity) in &bp_map.0 {
                let Ok(bp) = bp_query.get(bp_entity) else {
                    continue;
                };
                let allowed = match bp.personal_owner {
                    Some(owner) => owner == agent_entity,
                    None => bp.faction_id == faction_id,
                };
                if !allowed {
                    continue;
                }
                for i in 0..bp.deposit_count as usize {
                    let still = bp.deposits[i]
                        .needed
                        .saturating_sub(bp.deposits[i].deposited);
                    if still > 0 {
                        let g = bp.deposits[i].good as usize;
                        still_need_by_good[g] = still_need_by_good[g].saturating_add(still as u32);
                        any_needed = true;
                    }
                }
            }
        }
        MaterialNeed::CraftOrder => {
            for (_, &order_entity) in &co_map.0 {
                let Ok(order) = co_query.get(order_entity) else {
                    continue;
                };
                if order.faction_id != faction_id {
                    continue;
                }
                for i in 0..order.deposit_count as usize {
                    let still = order.deposits[i]
                        .needed
                        .saturating_sub(order.deposits[i].deposited);
                    if still > 0 {
                        let g = order.deposits[i].good as usize;
                        still_need_by_good[g] = still_need_by_good[g].saturating_add(still as u32);
                        any_needed = true;
                    }
                }
            }
        }
        MaterialNeed::HaulClaim => {
            // Claim names the exact good and (implicitly) the qty we want;
            // no aggregation required. Override the selector so downstream
            // logic treats this as a Specific lookup.
            let claim = claim_target.and_then(|c| c.good)?;
            still_need_by_good[claim as usize] = u32::MAX / 2;
            effective_selector = GoodSelector::Specific(claim);
            any_needed = true;
        }
    }
    if !any_needed {
        return None;
    }

    // For Specific selectors, ignore demand on other goods entirely so we
    // never accidentally pick a tile because some unrelated good is needed.
    if let GoodSelector::Specific(g) = effective_selector {
        let saved = still_need_by_good[g as usize];
        still_need_by_good = [0u32; crate::economy::goods::GOOD_COUNT];
        still_need_by_good[g as usize] = saved;
        if saved == 0 {
            return None;
        }
    }

    // 2. Helper: per-good effective stock on a tile (ground qty minus
    //    reservations). The reservation map tracks promises that haven't
    //    been collected yet, so two agents never commit to the same unit.
    let effective_stock = |tx: i16, ty: i16, good: Good| -> u32 {
        let mut stock = 0u32;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(gi) = item_query.get(gi_entity) {
                if gi.item.good == good {
                    stock = stock.saturating_add(gi.qty);
                }
            }
        }
        stock.saturating_sub(reservations.get((tx, ty), good))
    };

    // 3. Find the nearest faction storage tile that holds at least one
    //    useful good (intersection of demand & effective stock).
    let tiles = storage_tile_map.by_faction.get(&faction_id)?;
    let mut best: Option<(i16, i16)> = None;
    let mut best_dist = i32::MAX;
    for &(tx, ty) in tiles {
        let mut has_useful = false;
        for &gi_entity in spatial.get(tx as i32, ty as i32) {
            if let Ok(gi) = item_query.get(gi_entity) {
                if gi.qty == 0 {
                    continue;
                }
                if still_need_by_good[gi.item.good as usize] == 0 {
                    continue;
                }
                if effective_stock(tx, ty, gi.item.good) > 0 {
                    has_useful = true;
                    break;
                }
            }
        }
        if !has_useful {
            continue;
        }
        let dist = (tx as i32 - pos.0).abs() + (ty as i32 - pos.1).abs();
        if dist < best_dist {
            best_dist = dist;
            best = Some((tx, ty));
        }
    }
    let (tx, ty) = best?;

    // 4. Pick the good to commit to.
    let chosen: Option<(Good, u32)> = match effective_selector {
        GoodSelector::Specific(g) => {
            let stock = effective_stock(tx, ty, g);
            if stock > 0 && still_need_by_good[g as usize] > 0 {
                Some((g, stock.min(still_need_by_good[g as usize])))
            } else {
                None
            }
        }
        GoodSelector::MostDeficient => {
            // Walk goods present on the tile and keep the one with the
            // largest still-needed value. Stable tiebreak by Good as u8.
            let mut best_pick: Option<(Good, u32, u32)> = None; // (good, deficit, take_qty)
            for &gi_entity in spatial.get(tx as i32, ty as i32) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.qty == 0 {
                        continue;
                    }
                    let deficit = still_need_by_good[gi.item.good as usize];
                    if deficit == 0 {
                        continue;
                    }
                    let stock = effective_stock(tx, ty, gi.item.good);
                    if stock == 0 {
                        continue;
                    }
                    let take = stock.min(deficit);
                    let candidate = (gi.item.good, deficit, take);
                    best_pick = Some(match best_pick {
                        None => candidate,
                        Some(prev) => {
                            if deficit > prev.1
                                || (deficit == prev.1
                                    && (gi.item.good as u8) < (prev.0 as u8))
                            {
                                candidate
                            } else {
                                prev
                            }
                        }
                    });
                }
            }
            best_pick.map(|(g, _, q)| (g, q))
        }
    };

    // 5. Cap the commit by what the agent can actually carry home. The
    //    executor routes pickups into `Carrier` first (hands have a large
    //    weight cap, especially for TwoHand bulk) and falls back to
    //    `EconomicAgent.inventory` for residual. Sum the two — and don't
    //    floor to 1 — so we never promise a unit that has no destination.
    //    Stone weighs exactly the inventory cap, so an inventory-only floor
    //    would commit units that vanish on pickup.
    let (good, mut qty) = chosen?;
    let item = crate::economy::item::Item::new_commodity(good);
    let unit_w = item.unit_weight_g().max(1);
    let hand_cap = carrier.pickup_capacity(item);
    let inv_room = agent.capacity_g().saturating_sub(agent.current_weight_g());
    let inv_cap = inv_room / unit_w;
    let total_cap = hand_cap.saturating_add(inv_cap);
    if total_cap == 0 {
        return None;
    }
    qty = qty.min(total_cap);
    let qty_u8 = qty.min(u8::MAX as u32) as u8;
    if qty_u8 == 0 {
        return None;
    }
    *withdraw_intent_out = Some((good, qty_u8));
    Some((None, tx, ty))
}

fn resolve_target(
    step: &StepDef,
    pos: (i32, i32),
    pos_z: i8,
    chunk_map: &ChunkMap,
    door_map: &crate::simulation::construction::DoorMap,
    spatial: &SpatialIndex,
    plant_map: &PlantMap,
    plant_query: &Query<&Plant>,
    faction_registry: &FactionRegistry,
    storage_tile_map: &StorageTileMap,
    chunk_connectivity: &ChunkConnectivity,
    faction_id: u32,
    agent_entity: Entity,
    memory: Option<&AgentMemory>,
    item_query: &Query<&GroundItem>,
    prey_query: &Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    wild_horse_q: &Query<Entity, (With<Horse>, Without<Tamed>)>,
    combat_target: &mut CombatTarget,
    target_item: &mut TargetItem,
    bp_map: &BlueprintMap,
    bp_query: &Query<&Blueprint>,
    co_map: &CraftOrderMap,
    co_query: &Query<&CraftOrder>,
    corpse_map: &CorpseMap,
    corpse_query: &Query<&Corpse>,
    campfire_map: &CampfireMap,
    rescue_target: Option<&RescueTarget>,
    agent: &EconomicAgent,
    carrier: &Carrier,
    claim_target: Option<&crate::simulation::jobs::ClaimTarget>,
    reservations: &StorageReservations,
    withdraw_intent_out: &mut Option<(Good, u8)>,
) -> Option<(Option<Entity>, i16, i16)> {
    const VIEW_RADIUS: i32 = 15;

    match &step.target {
        StepTarget::HuntPrey => {
            // 1. Check vision
            let mut best_v: Option<(Entity, i16, i16)> = None;
            let mut best_dist_v = i32::MAX;
            for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    if dx * dx + dy * dy > VIEW_RADIUS * VIEW_RADIUS {
                        continue;
                    }
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    let to_z = chunk_map.surface_z_at(tx, ty) as i8;
                    if !super::line_of_sight::has_los(
                        chunk_map,
                        door_map,
                        (pos.0, pos.1, pos_z),
                        (tx, ty, to_z),
                    ) {
                        continue;
                    }
                    for &candidate in spatial.get(tx, ty) {
                        if let Ok((_transform, health)) = prey_query.get(candidate) {
                            if !health.is_dead() {
                                let dist = dx.abs() + dy.abs();
                                if dist < best_dist_v {
                                    best_dist_v = dist;
                                    best_v = Some((candidate, tx as i16, ty as i16));
                                }
                            }
                        }
                    }
                }
            }
            if let Some((entity, tx, ty)) = best_v {
                combat_target.0 = Some(entity);
                return Some((Some(entity), tx, ty));
            }

            // 2. Check memory
            if let Some(mem) = memory {
                if let Some((entity, tx, ty)) =
                    mem.best_entity_for_dist_weighted(MemoryKind::Prey, pos)
                {
                    combat_target.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::FromMemory(kind) => {
            // 1. Check vision
            let vision_target: Option<(Option<Entity>, i16, i16)> = match kind {
                MemoryKind::Food => find_nearest_plant(
                    plant_map,
                    pos,
                    VIEW_RADIUS,
                    plant_query,
                    true,
                    step.plant_filter,
                )
                .map(|(e, tx, ty)| (Some(e), tx, ty)),
                MemoryKind::Wood => find_nearest_plant(
                    plant_map,
                    pos,
                    VIEW_RADIUS,
                    plant_query,
                    true,
                    Some(PlantKind::Tree),
                )
                .map(|(e, tx, ty)| (Some(e), tx, ty)),
                MemoryKind::Stone => find_nearest_tile(chunk_map, pos, VIEW_RADIUS, STONE_TILES)
                    .map(|(tx, ty)| (None, tx, ty)),
                _ => None,
            };
            if let Some((ent, tx, ty)) = vision_target {
                let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                if super::line_of_sight::has_los(
                    chunk_map,
                    door_map,
                    (pos.0, pos.1, pos_z),
                    (tx as i32, ty as i32, to_z),
                ) {
                    return Some((ent, tx, ty));
                }
            }

            // 2. Check memory
            if let Some(mem) = memory {
                // Entity memories: skip plants that are no longer Mature (stale after harvest).
                let best_valid_entity = mem
                    .entries
                    .iter()
                    .filter_map(|slot| slot.as_ref())
                    .filter(|e| e.kind == *kind)
                    .filter_map(|e| e.entity.map(|ent| (ent, e.tile, e.freshness)))
                    .filter(|(ent, _, _)| {
                        matches!(plant_query.get(*ent), Ok(p) if p.stage == GrowthStage::Mature)
                    })
                    .max_by(|a, b| {
                        let score = |e: &(Entity, (i16, i16), u8)| {
                            e.2 as f32
                                / ((e.1.0 as i32 - pos.0).abs() + (e.1.1 as i32 - pos.1).abs())
                                    .max(1) as f32
                        };
                        score(a)
                            .partial_cmp(&score(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });

                if let Some((ent, tile, _)) = best_valid_entity {
                    return Some((Some(ent), tile.0, tile.1));
                }

                // Tile-based fallback: for plant resources require a Mature plant at the tile.
                if let Some((tx, ty)) = mem.best_for_dist_weighted(*kind, pos) {
                    let mut found_ent = None;
                    let plant_ok = match plant_map.0.get(&(tx as i32, ty as i32)) {
                        Some(&tile_ent) => {
                            if matches!(plant_query.get(tile_ent), Ok(p) if p.stage == GrowthStage::Mature)
                            {
                                found_ent = Some(tile_ent);
                                true
                            } else {
                                false
                            }
                        }
                        // No plant at tile: valid for Stone, stale for Food/Wood.
                        None => !matches!(kind, MemoryKind::Food | MemoryKind::Wood),
                    };
                    if plant_ok {
                        return Some((found_ent, tx, ty));
                    }
                }
            }

            None
        }
        StepTarget::NearestTile(_tiles) => {
            if step.task == TaskKind::Planter {
                find_nearest_unplanted_farmland(chunk_map, plant_map, pos, VIEW_RADIUS)
                    .map(|(tx, ty)| (None, tx, ty))
            } else {
                find_nearest_tile(chunk_map, pos, VIEW_RADIUS, _tiles)
                    .map(|(tx, ty)| (None, tx, ty))
            }
        }
        StepTarget::NearestItem(good) => {
            // 1. Check vision — Bug 2 fix: pass good so only matching items are targeted.
            if let Some((entity, tx, ty)) = find_nearest_item(
                spatial,
                pos,
                VIEW_RADIUS,
                *good,
                item_query,
                storage_tile_map,
            ) {
                let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                if super::line_of_sight::has_los(
                    chunk_map,
                    door_map,
                    (pos.0, pos.1, pos_z),
                    (tx as i32, ty as i32, to_z),
                ) {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            // 2. Check memory — exclude faction storage tiles to mirror the
            //    vision-path filter; otherwise hungry/gathering agents would
            //    walk to food remembered on a stockpile.
            if let Some(mem) = memory {
                let mkind = match good {
                    Good::Wood => MemoryKind::Wood,
                    Good::Stone => MemoryKind::Stone,
                    Good::Seed => MemoryKind::Seed,
                    _ => {
                        if good.is_edible() {
                            MemoryKind::Food
                        } else {
                            return None;
                        }
                    }
                };
                if let Some((entity, tx, ty)) =
                    mem.best_entity_for_dist_weighted_filtered(mkind, pos, |_, tile| {
                        !storage_tile_map.tiles.contains_key(&tile)
                    })
                {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::NearestEdible => {
            // 1. Check vision
            if let Some((entity, tx, ty)) = find_nearest_edible(
                spatial,
                pos,
                VIEW_RADIUS,
                item_query,
                storage_tile_map,
            ) {
                let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                if super::line_of_sight::has_los(
                    chunk_map,
                    door_map,
                    (pos.0, pos.1, pos_z),
                    (tx as i32, ty as i32, to_z),
                ) {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            // 2. Check memory — only accept GroundItem entities (mature plants
            //    are harvested via ForageFood, not Scavenge), and skip food
            //    sitting on faction storage tiles.
            if let Some(mem) = memory {
                if let Some((entity, tx, ty)) = mem.best_entity_for_dist_weighted_filtered(
                    MemoryKind::Food,
                    pos,
                    |e, tile| {
                        item_query.get(e).is_ok()
                            && !storage_tile_map.tiles.contains_key(&tile)
                    },
                ) {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::FactionCamp => faction_registry
            .home_tile(faction_id)
            .map(|(tx, ty)| (None, tx, ty)),
        StepTarget::FactionRaidTarget => faction_registry
            .raid_target(faction_id)
            .and_then(|target_id| faction_registry.home_tile(target_id))
            .map(|(tx, ty)| (None, tx, ty)),
        StepTarget::SelfPosition => Some((None, pos.0 as i16, pos.1 as i16)),
        StepTarget::NearestFactionStorage => storage_tile_map
            .nearest_for_faction(faction_id, pos)
            .map(|(tx, ty)| (None, tx, ty)),
        StepTarget::WithdrawForFactionNeed { need, selector } => resolve_withdraw_for_faction_need(
            *need,
            *selector,
            pos,
            spatial,
            storage_tile_map,
            reservations,
            faction_id,
            agent_entity,
            item_query,
            bp_map,
            bp_query,
            co_map,
            co_query,
            claim_target,
            agent,
            carrier,
            withdraw_intent_out,
        ),
        StepTarget::NearestFactionStorageWithGood(target_good) => {
            let tiles = storage_tile_map.by_faction.get(&faction_id)?;
            let mut best: Option<(i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for &(tx, ty) in tiles {
                let mut has = false;
                for &gi_entity in spatial.get(tx as i32, ty as i32) {
                    if let Ok(gi) = item_query.get(gi_entity) {
                        if gi.qty > 0 && gi.item.good == *target_good {
                            has = true;
                            break;
                        }
                    }
                }
                if !has {
                    continue;
                }
                let dist = (tx as i32 - pos.0).abs() + (ty as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx, ty));
                }
            }
            best.map(|(tx, ty)| (None, tx, ty))
        }
        StepTarget::NearestFactionStorageWithEntertainment => {
            let tiles = storage_tile_map.by_faction.get(&faction_id)?;
            let mut best: Option<(i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for &(tx, ty) in tiles {
                let mut has = false;
                for &gi_entity in spatial.get(tx as i32, ty as i32) {
                    if let Ok(gi) = item_query.get(gi_entity) {
                        if gi.qty > 0 && gi.item.good.entertainment_value() > 0 {
                            has = true;
                            break;
                        }
                    }
                }
                if !has {
                    continue;
                }
                let dist = (tx as i32 - pos.0).abs() + (ty as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx, ty));
                }
            }
            best.map(|(tx, ty)| (None, tx, ty))
        }
        StepTarget::NearestWildHorse => {
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    for &candidate in spatial.get(pos.0 + dx, pos.1 + dy) {
                        if wild_horse_q.get(candidate).is_ok() {
                            let dist = dx.abs() + dy.abs();
                            if dist < best_dist {
                                best_dist = dist;
                                best = Some((candidate, (pos.0 + dx) as i16, (pos.1 + dy) as i16));
                            }
                        }
                    }
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::HaulClaimBlueprint => {
            // Route directly to the blueprint named in the agent's Haul claim.
            let bp_entity = claim_target.and_then(|c| c.blueprint)?;
            let bp = bp_query.get(bp_entity).ok()?;
            // Skip if the blueprint is already satisfied (no more deposits
            // needed) — the haul is moot, let the plan abandon and re-pick.
            if bp.is_satisfied() {
                return None;
            }
            Some((Some(bp_entity), bp.tile.0, bp.tile.1))
        }
        StepTarget::BuildClaimBlueprint => {
            // Route directly to the blueprint named in the agent's Build claim,
            // but only when its deposits are all in (so the build_progress
            // counter actually advances when the agent works).
            let bp_entity = claim_target.and_then(|c| c.blueprint)?;
            let bp = bp_query.get(bp_entity).ok()?;
            if !bp.is_satisfied() {
                return None;
            }
            Some((Some(bp_entity), bp.tile.0, bp.tile.1))
        }
        StepTarget::NearestBuildSite(_) => None, // Legacy; construction is handled via faction_blueprint_system
        StepTarget::NearestBlueprint(kind) => {
            // Find the nearest active Blueprint this agent is allowed to build.
            // Personal blueprints are matched by agent_entity; faction blueprints by faction_id.
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, &bp_entity) in &bp_map.0 {
                let Ok(bp) = bp_query.get(bp_entity) else {
                    continue;
                };
                let allowed = match bp.personal_owner {
                    Some(owner) => owner == agent_entity,
                    None => bp.faction_id == faction_id,
                };
                if !allowed {
                    continue;
                }
                if &bp.kind != kind {
                    continue;
                }
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((bp_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::RescueAttacker => {
            // Read the attacker + their last-known tile from the agent's RescueTarget.
            // sound::respond_to_distress_system snapshots this each time the victim
            // re-emits a distress event (~every second), so even if the attacker
            // moves, the responder will be redirected as fresh distress arrives.
            let rt = rescue_target?;
            combat_target.0 = Some(rt.attacker);
            Some((Some(rt.attacker), rt.attacker_tile.0, rt.attacker_tile.1))
        }
        StepTarget::NearestAnyBlueprint => {
            // Find the nearest active Blueprint of any kind this agent is
            // allowed to build *that already has all its materials deposited*.
            // Workers committing to an unfunded site is the bug behind the
            // "255/40 stuck" report — there's no point standing on a site
            // whose materials haven't arrived. If no satisfied blueprint is
            // reachable, return None so the plan abandons and the agent
            // picks a different one (likely a haul-related plan) next round.
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, &bp_entity) in &bp_map.0 {
                let Ok(bp) = bp_query.get(bp_entity) else {
                    continue;
                };
                let allowed = match bp.personal_owner {
                    Some(owner) => owner == agent_entity,
                    None => bp.faction_id == faction_id,
                };
                if !allowed {
                    continue;
                }
                if !bp.is_satisfied() {
                    continue;
                }
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((bp_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestPlayPartner => {
            // Spatial scan within ~12 tiles. Filter out non-agent entities
            // (blueprints, ground items, animals) so we don't try to play with
            // a wolf. play_system also defensively re-checks via Person query.
            const PLAY_RADIUS: i32 = 12;
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for dy in -PLAY_RADIUS..=PLAY_RADIUS {
                for dx in -PLAY_RADIUS..=PLAY_RADIUS {
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    for &other in spatial.get(tx, ty) {
                        if other == agent_entity {
                            continue;
                        }
                        if bp_query.get(other).is_ok()
                            || item_query.get(other).is_ok()
                            || prey_query.get(other).is_ok()
                            || wild_horse_q.get(other).is_ok()
                        {
                            continue;
                        }
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best = Some((other, tx as i16, ty as i16));
                        }
                    }
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestPlayItem => {
            // Already holding something fun? Play in place.
            let held_l = carrier
                .left
                .map(|s| s.item.good.entertainment_value())
                .unwrap_or(0);
            let held_r = carrier
                .right
                .map(|s| s.item.good.entertainment_value())
                .unwrap_or(0);
            if held_l > 0 || held_r > 0 {
                return Some((None, pos.0 as i16, pos.1 as i16));
            }
            // Otherwise scan for the most-entertaining ground item nearby.
            const ITEM_RADIUS: i32 = 8;
            let mut best: Option<(Entity, i16, i16, i32)> = None;
            for dy in -ITEM_RADIUS..=ITEM_RADIUS {
                for dx in -ITEM_RADIUS..=ITEM_RADIUS {
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    for &e in spatial.get(tx, ty) {
                        if let Ok(item) = item_query.get(e) {
                            let v = item.item.good.entertainment_value() as i32;
                            if v == 0 {
                                continue;
                            }
                            let dist = dx.abs() + dy.abs();
                            let score = v * 4 - dist;
                            match best {
                                Some((_, _, _, s)) if s >= score => {}
                                _ => best = Some((e, tx as i16, ty as i16, score)),
                            }
                        }
                    }
                }
            }
            best.map(|(e, tx, ty, _)| (Some(e), tx, ty))
        }
        StepTarget::NearestBlueprintNeedingHeldMaterial => {
            // Nearest blueprint with at least one unmet deposit slot whose good
            // is currently in the agent's inventory.
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, &bp_entity) in &bp_map.0 {
                let Ok(bp) = bp_query.get(bp_entity) else {
                    continue;
                };
                let allowed = match bp.personal_owner {
                    Some(owner) => owner == agent_entity,
                    None => bp.faction_id == faction_id,
                };
                if !allowed {
                    continue;
                }
                let mut useful = false;
                for i in 0..bp.deposit_count as usize {
                    let still = bp.deposits[i]
                        .needed
                        .saturating_sub(bp.deposits[i].deposited);
                    let held = carrier
                        .quantity_of_good(bp.deposits[i].good)
                        .saturating_add(agent.quantity_of(bp.deposits[i].good));
                    if still > 0 && held > 0 {
                        useful = true;
                        break;
                    }
                }
                if !useful {
                    continue;
                }
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((bp_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestCraftOrderNeedingHeldMaterial => {
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, &order_entity) in &co_map.0 {
                let Ok(order) = co_query.get(order_entity) else {
                    continue;
                };
                if order.faction_id != faction_id {
                    continue;
                }
                let mut useful = false;
                for i in 0..order.deposit_count as usize {
                    let still = order.deposits[i]
                        .needed
                        .saturating_sub(order.deposits[i].deposited);
                    let held = carrier
                        .quantity_of_good(order.deposits[i].good)
                        .saturating_add(agent.quantity_of(order.deposits[i].good));
                    if still > 0 && held > 0 {
                        useful = true;
                        break;
                    }
                }
                if !useful {
                    continue;
                }
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((order_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestSatisfiedCraftOrder => {
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, &order_entity) in &co_map.0 {
                let Ok(order) = co_query.get(order_entity) else {
                    continue;
                };
                if order.faction_id != faction_id {
                    continue;
                }
                if !order.is_satisfied() {
                    continue;
                }
                // Station-bound recipes require the worker to stand adjacent
                // to the workbench/loom, which is the anchor tile.
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((order_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::ExploreTile => {
            let home = faction_registry
                .home_tile(faction_id)
                .unwrap_or((pos.0 as i16, pos.1 as i16));
            let cur_chunk = chunk_coord(pos.0, pos.1);
            for _ in 0..8 {
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let tx = (home.0 as i32 + dx).max(0) as i16;
                let ty = (home.1 as i32 + dy).max(0) as i16;
                let to_chunk = chunk_coord(tx as i32, ty as i32);
                let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                if chunk_connectivity.is_reachable((cur_chunk, pos_z), (to_chunk, to_z)) {
                    return Some((None, tx, ty));
                }
            }
            // Underground recovery: if no random tile is reachable and the
            // agent is below the surface, head for the nearest reachable
            // higher-Z tile.
            if pos_z < 0 {
                if let Some((tx, ty)) = nearest_reachable_higher_tile(
                    chunk_map,
                    chunk_connectivity,
                    (pos.0 as i16, pos.1 as i16),
                    (cur_chunk, pos_z),
                    96,
                ) {
                    return Some((None, tx, ty));
                }
            }
            None
        }
        StepTarget::NearestFreshCorpse => {
            // Nearest Corpse entity within VIEW_RADIUS, biased to closer tiles.
            // We rely on `corpse_map` for an O(tiles_in_radius) lookup rather
            // than scanning the spatial index.
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    if dx * dx + dy * dy > VIEW_RADIUS * VIEW_RADIUS {
                        continue;
                    }
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    if let Some(entities) = corpse_map.0.get(&(tx as i16, ty as i16)) {
                        for &e in entities {
                            if corpse_query.get(e).is_err() {
                                continue;
                            }
                            let dist = dx.abs() + dy.abs();
                            if dist < best_dist {
                                best_dist = dist;
                                best = Some((e, tx as i16, ty as i16));
                            }
                        }
                    }
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestButcherSite => {
            // Nearest hearth tile owned by the agent's faction, falling back
            // to the faction home tile. We don't enforce a faction filter on
            // CampfireMap entries since hearths are placed by faction
            // blueprint logic and can't appear unowned in practice.
            let mut best: Option<(i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, _e) in campfire_map.0.iter() {
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some(tile);
                }
            }
            if let Some((tx, ty)) = best {
                return Some((None, tx, ty));
            }
            faction_registry
                .home_tile(faction_id)
                .map(|(tx, ty)| (None, tx, ty))
        }
        StepTarget::EquipItem { .. } => {
            // Equip is in-place — the executor reads slot/good off PersonAI
            // (set during dispatch in `plan_execution_system`).
            Some((None, pos.0 as i16, pos.1 as i16))
        }
        StepTarget::HearthForHunt => {
            // Only meaningful when the faction has an active Hunt order.
            // Pick the campfire tile closest to the chief's chosen
            // hunt area (so the muster point is on the side of camp the
            // party will depart from); fall back to home_tile.
            let order = faction_registry
                .factions
                .get(&faction_id)
                .and_then(|f| f.hunt_order.as_ref())?;
            let area_tile = match order {
                crate::simulation::faction::HuntOrder::Hunt { area_tile, .. } => *area_tile,
                _ => return None,
            };
            let ax = area_tile.0 as i32;
            let ay = area_tile.1 as i32;
            let mut best: Option<(i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for (&tile, _e) in campfire_map.0.iter() {
                let dist = (tile.0 as i32 - ax).abs() + (tile.1 as i32 - ay).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some(tile);
                }
            }
            if let Some((tx, ty)) = best {
                return Some((None, tx, ty));
            }
            faction_registry
                .home_tile(faction_id)
                .map(|(tx, ty)| (None, tx, ty))
        }
        StepTarget::HuntArea => {
            let order = faction_registry
                .factions
                .get(&faction_id)
                .and_then(|f| f.hunt_order.as_ref())?;
            match order {
                crate::simulation::faction::HuntOrder::Hunt { area_tile, .. } => {
                    Some((None, area_tile.0, area_tile.1))
                }
                _ => None,
            }
        }
        StepTarget::ScoutForPrey => {
            // Reuse the ExploreTile random-reachable-tile logic so scouts
            // wander outward and dump memory along the way; `vision_system`
            // writes `MemoryKind::Prey` whenever a hunter sees Wolf/Deer,
            // and the candidate filter removes ScoutForPrey from contention
            // the moment that memory is populated.
            let has_scout = matches!(
                faction_registry
                    .factions
                    .get(&faction_id)
                    .and_then(|f| f.hunt_order.as_ref()),
                Some(crate::simulation::faction::HuntOrder::Scout { .. })
            );
            if !has_scout {
                return None;
            }
            let home = faction_registry
                .home_tile(faction_id)
                .unwrap_or((pos.0 as i16, pos.1 as i16));
            let cur_chunk = chunk_coord(pos.0, pos.1);
            for _ in 0..8 {
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let tx = (home.0 as i32 + dx).max(0) as i16;
                let ty = (home.1 as i32 + dy).max(0) as i16;
                let to_chunk = chunk_coord(tx as i32, ty as i32);
                let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                if chunk_connectivity.is_reachable((cur_chunk, pos_z), (to_chunk, to_z)) {
                    return Some((None, tx, ty));
                }
            }
            None
        }
    }
}

fn chunk_coord(tx: i32, ty: i32) -> ChunkCoord {
    ChunkCoord(
        tx.div_euclid(CHUNK_SIZE as i32),
        ty.div_euclid(CHUNK_SIZE as i32),
    )
}

// Bundles registries needed by plan_execution_system; Bevy caps a system at
// 16 top-level params, and these would put us over the limit.
#[derive(SystemParam)]
pub struct PlanRegistries<'w> {
    pub plan_registry: Res<'w, PlanRegistry>,
    pub step_registry: Res<'w, StepRegistry>,
    pub faction_registry: Res<'w, FactionRegistry>,
    pub storage_tile_map: Res<'w, StorageTileMap>,
    pub storage_reservations: Res<'w, StorageReservations>,
    pub door_map: Res<'w, crate::simulation::construction::DoorMap>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub drop_food_events: EventWriter<'w, DropAbandonedFoodEvent>,
}

#[derive(SystemParam)]
pub struct SiteRegistries<'w, 's> {
    pub bp_map: Res<'w, BlueprintMap>,
    pub bp_query: Query<'w, 's, &'static Blueprint>,
    pub co_map: Res<'w, CraftOrderMap>,
    pub co_query: Query<'w, 's, &'static CraftOrder>,
    pub corpse_map: Res<'w, CorpseMap>,
    pub corpse_query: Query<'w, 's, &'static Corpse>,
    pub campfire_map: Res<'w, CampfireMap>,
}

// ── Plan execution system ─────────────────────────────────────────────────────
// Runs in Sequential set after production_system.
// Handles: plan selection for idle agents, step dispatching,
// step completion detection, plan completion + NN learning, timeouts.

type AgentQuery<'a> = (
    Entity,
    &'a mut PersonAI,
    &'a EconomicAgent,
    &'a FactionMember,
    &'a AgentGoal,
    &'a Needs,
    &'a Skills,
    &'a Transform,
    &'a LodLevel,
    &'a BucketSlot,
    &'a mut CombatTarget,
    &'a mut TargetItem,
    &'a Carrier,
    &'a mut crate::pathfinding::path_request::PathFollow,
    &'a Profession,
);

type OptionalQuery<'a> = (
    Option<&'a AgentMemory>,
    Option<&'a KnownPlans>,
    Option<&'a PlanScoringMethod>,
    Option<&'a mut ActivePlan>,
    Option<&'a RelationshipMemory>,
    Option<&'a RescueTarget>,
    Option<&'a mut PlanHistory>,
    Option<&'a crate::simulation::jobs::ClaimTarget>,
    Option<&'a crate::simulation::items::Equipment>,
);

/// Aborts an `ExploreFor*` plan as soon as the agent has memory of the
/// resource kind it was searching for. Runs after `vision_system` so that
/// any sighting recorded this tick is visible here. Removing `ActivePlan`
/// lets the next selection cycle pick the matching gather plan now that
/// memory is populated. Logs `PlanOutcome::Success` — the explore objective
/// was "find one," and we just did.
pub fn explore_satisfaction_system(
    mut commands: Commands,
    plan_registry: Res<PlanRegistry>,
    clock: Res<SimClock>,
    mut query: Query<(
        Entity,
        &ActivePlan,
        &AgentMemory,
        &BucketSlot,
        &LodLevel,
        Option<&mut PlanHistory>,
    )>,
) {
    for (entity, active_plan, memory, slot, lod, mut history) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Some(plan_def) = plan_registry
            .0
            .iter()
            .find(|p| p.id == active_plan.plan_id)
        else {
            continue;
        };
        if plan_def.flags & PF_EXPLORE == 0 {
            continue;
        }
        let target = if plan_def.flags & PF_TARGETS_FOOD != 0 {
            MemoryKind::Food
        } else if plan_def.flags & PF_TARGETS_WOOD != 0 {
            MemoryKind::Wood
        } else if plan_def.flags & PF_TARGETS_STONE != 0 {
            MemoryKind::Stone
        } else {
            continue;
        };
        if memory.best_for(target).is_none() {
            continue;
        }
        if let Some(history) = history.as_deref_mut() {
            history.push(active_plan.plan_id, PlanOutcome::Success, clock.tick);
        }
        commands.entity(entity).remove::<ActivePlan>();
    }
}

pub fn plan_execution_system(
    par_commands: ParallelCommands,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    registries: PlanRegistries,
    sites: SiteRegistries,
    calendar: Res<Calendar>,
    clock: Res<SimClock>,
    item_check: Query<&GroundItem>,
    prey_query: Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    wild_horse_q: Query<Entity, (With<Horse>, Without<Tamed>)>,
    rel_influence: Res<RelInfluence>,
    mut query: Query<(AgentQuery, OptionalQuery), (Without<PlayerOrder>, Without<Drafted>)>,
) {
    let PlanRegistries {
        plan_registry,
        step_registry,
        faction_registry,
        storage_tile_map,
        storage_reservations,
        door_map,
        chunk_router,
        chunk_connectivity,
        mut drop_food_events,
    } = registries;
    let dropped_food: std::sync::Mutex<Vec<Entity>> = std::sync::Mutex::new(Vec::new());
    query.par_iter_mut().for_each(
        |(
            (
                entity,
                mut ai,
                agent,
                member,
                goal,
                needs,
                skills,
                transform,
                lod,
                slot,
                mut combat_target,
                mut target_item,
                carrier,
                _path_follow,
                profession,
            ),
            (
                memory_opt,
                known_plans_opt,
                scoring_opt,
                mut active_plan_opt,
                _rel_opt,
                rescue_target_opt,
                mut plan_history_opt,
                claim_target_opt,
                equipment_opt,
            ),
        )| {
            if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
                return;
            }

            // Only handle plan-driven goals. Socialize/Raid/Defend/Lead were
            // migrated out of `goal_dispatch_system` and now run as plans
            // 60-63 (see `registry.rs`). Sleep is the lone holdout: its
            // bed/camp fallback chain still lives in `goal_dispatch_system`.
            if !matches!(
                goal,
                AgentGoal::Survive
                    | AgentGoal::GatherFood
                    | AgentGoal::GatherWood
                    | AgentGoal::GatherStone
                    | AgentGoal::Build
                    | AgentGoal::Haul
                    | AgentGoal::TameHorse
                    | AgentGoal::Rescue
                    | AgentGoal::ReturnCamp
                    | AgentGoal::Play
                    | AgentGoal::Farm
                    | AgentGoal::Craft
                    | AgentGoal::Socialize
                    | AgentGoal::Raid
                    | AgentGoal::Defend
                    | AgentGoal::Lead
            ) {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = chunk_coord(cur_tx, cur_ty);

            // Defensive: if a stale storage reservation lingers because the
            // withdraw task didn't run its normal exit (plan preempted
            // mid-walk, drafted, etc.), release it now. The withdraw
            // executor handles the common path; this catches preemption.
            if ai.reserved_good.is_some()
                && ai.task_id != TaskKind::WithdrawMaterial as u16
            {
                release_reservation(&storage_reservations, &mut ai);
            }

            if active_plan_opt.is_none() {
                // ── Select and start a new plan ───────────────────────────────────
                if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                    return;
                }

                let Some(known_plans) = known_plans_opt else {
                    return;
                };
                let Some(scoring) = scoring_opt else { return };
                if plan_registry.0.is_empty() {
                    return;
                }

                // Compute visibility once up front so the candidate filter and
                // scoring share the same numbers. Each slot answers one yes/no
                // question (a tree is not a piece of wood; a stone tile is not a
                // ground stone; an edible plant is not a ground corpse) so plans
                // only score on signals they can actually act on. Cheap because
                // plan selection is bucketed and counters early-exit at 4 hits.
                let vis_plant_food =
                    count_visible_plant_food(cur_tx, cur_ty, &plant_map, &plant_query);
                let vis_trees = count_visible_trees(cur_tx, cur_ty, &plant_map, &plant_query);
                let vis_stone_tiles = count_visible_stone_tiles(cur_tx, cur_ty, &chunk_map);
                let vis_ground_wood =
                    count_visible_ground_wood(cur_tx, cur_ty, &spatial, &item_check);
                let vis_ground_stone =
                    count_visible_ground_stone(cur_tx, cur_ty, &spatial, &item_check);
                let vis_ground_food =
                    count_visible_ground_food(cur_tx, cur_ty, &spatial, &item_check);

                let candidates: Vec<&PlanDef> = plan_registry
                    .0
                    .iter()
                    .filter(|p| p.serves_goals.contains(&goal) && known_plans.knows(p.id))
                    .filter(|p| {
                        p.tech_gate.map_or(true, |tid| {
                            faction_registry
                                .factions
                                .get(&member.faction_id)
                                .map(|f| f.techs.has(tid))
                                .unwrap_or(false)
                        })
                    })
                    .filter(|p| {
                        // Profession gate: hunter-only / farmer-only plans are
                        // self-deselecting for everyone else. Plans without a
                        // requires_profession are open to all professions.
                        match p.requires_profession {
                            None => true,
                            Some(required) => *profession == required,
                        }
                    })
                    .filter(|p| {
                        // Hunt-order gate: HuntFood (5) requires a Hunt order;
                        // ScoutForPrey (65) requires a Scout order. The chief
                        // sets the order; without it, hunters fall through to
                        // their normal plan competition (gather, haul, etc.)
                        // and stay flexible labour. AcquireHuntingSpear (64)
                        // is uncoupled from the order so a hunter who picks
                        // up the role mid-day can arm before the next muster.
                        match p.id {
                            5 => matches!(
                                faction_registry
                                    .factions
                                    .get(&member.faction_id)
                                    .and_then(|f| f.hunt_order.as_ref()),
                                Some(crate::simulation::faction::HuntOrder::Hunt { .. })
                            ),
                            65 => matches!(
                                faction_registry
                                    .factions
                                    .get(&member.faction_id)
                                    .and_then(|f| f.hunt_order.as_ref()),
                                Some(crate::simulation::faction::HuntOrder::Scout { .. })
                            ),
                            _ => true,
                        }
                    })
                    // Bug 3 fix: skip plans whose first step has unmet preconditions so we
                    // don't enter a tight pick-then-immediately-abandon loop.
                    .filter(|p| {
                        p.steps
                            .first()
                            .and_then(|&sid| step_registry.0.iter().find(|s| s.id == sid))
                            .map_or(true, |s| {
                                s.preconditions.is_satisfied(
                                    &agent,
                                    &carrier,
                                    equipment_opt,
                                    needs.hunger,
                                )
                            })
                    })
                    // Drop gather plans that have no idea where to go: no memory of
                    // the resource and nothing visible nearby. Otherwise the agent
                    // picks a blind ForageFood/GatherWood/GatherStone, fails to
                    // resolve step 0, and thrashes back through plan selection.
                    // The ExploreFor* plans invert this gate: they're available
                    // only when memory and visibility are both empty for their
                    // target kind, so they cover the "no idea where to go" case.
                    .filter(|p| {
                        // Resolve which resource the plan targets from its
                        // PF_TARGETS_* flag. Returns None for plans that don't
                        // gate on a single resource (build, play, deposit, …).
                        let target_kind = if p.flags & PF_TARGETS_FOOD != 0 {
                            Some(MemoryKind::Food)
                        } else if p.flags & PF_TARGETS_WOOD != 0 {
                            Some(MemoryKind::Wood)
                        } else if p.flags & PF_TARGETS_STONE != 0 {
                            Some(MemoryKind::Stone)
                        } else {
                            None
                        };

                        // Scavenge plans only act on loose ground items: gate
                        // on the matching ground-vis slot.
                        if p.flags & PF_SCAVENGE != 0 {
                            return match target_kind {
                                Some(MemoryKind::Food) => vis_ground_food > 0,
                                Some(MemoryKind::Wood) => vis_ground_wood > 0,
                                Some(MemoryKind::Stone) => vis_ground_stone > 0,
                                _ => true,
                            };
                        }

                        // Inverted gate for ExploreFor* — available only when
                        // the agent has no visibility of the source, no
                        // visibility of loose goods, and no memory. If a
                        // worker can see a pile of wood there's nothing left
                        // to explore for.
                        if p.flags & PF_EXPLORE != 0 {
                            return match target_kind {
                                Some(MemoryKind::Food) => {
                                    vis_plant_food == 0
                                        && vis_ground_food == 0
                                        && memory_opt
                                            .and_then(|m| m.best_for(MemoryKind::Food))
                                            .is_none()
                                }
                                Some(MemoryKind::Wood) => {
                                    vis_trees == 0
                                        && vis_ground_wood == 0
                                        && memory_opt
                                            .and_then(|m| m.best_for(MemoryKind::Wood))
                                            .is_none()
                                }
                                Some(MemoryKind::Stone) => {
                                    vis_stone_tiles == 0
                                        && vis_ground_stone == 0
                                        && memory_opt
                                            .and_then(|m| m.best_for(MemoryKind::Stone))
                                            .is_none()
                                }
                                _ => true,
                            };
                        }

                        // Source-only gate for gather/farm/deliver plans: each
                        // can only resolve a target through `FromMemory`
                        // (filtered to the matching source) or by walking to a
                        // visible source. Loose ground items don't help these
                        // plans.
                        match p.memory_target_kind {
                            Some(MemoryKind::Food) => {
                                vis_plant_food > 0
                                    || memory_opt
                                        .and_then(|m| m.best_for(MemoryKind::Food))
                                        .is_some()
                            }
                            Some(MemoryKind::Wood) => {
                                vis_trees > 0
                                    || memory_opt
                                        .and_then(|m| m.best_for(MemoryKind::Wood))
                                        .is_some()
                            }
                            Some(MemoryKind::Stone) => {
                                vis_stone_tiles > 0
                                    || memory_opt
                                        .and_then(|m| m.best_for(MemoryKind::Stone))
                                        .is_some()
                            }
                            _ => true,
                        }
                    })
                    .collect();

                if candidates.is_empty() {
                    // No plan serves this goal right now (every candidate filtered
                    // out by tech / known / preconditions). The Explore plan is in
                    // the innate set for the food/wood/stone goals, so this only
                    // fires for goals where no plan path exists yet — let the next
                    // tick try again rather than dispatch a hardcoded fallback.
                    return;
                }

                let plan_def = match scoring {
                    PlanScoringMethod::Weighted => {
                        let storage_opt = faction_registry
                            .factions
                            .get(&member.faction_id)
                            .map(|f| &f.storage);
                        let craft_order_needs_material =
                            sites.co_map.0.values().any(|&oe| {
                                sites.co_query.get(oe).ok().map_or(false, |o| {
                                    o.faction_id == member.faction_id
                                        && (0..o.deposit_count as usize).any(|i| {
                                            o.deposits[i].deposited < o.deposits[i].needed
                                        })
                                })
                            });
                        let state = build_state_vec(
                            needs,
                            agent,
                            skills,
                            member,
                            memory_opt,
                            &calendar,
                            plan_history_opt.as_deref(),
                            storage_opt,
                            vis_plant_food,
                            vis_trees,
                            vis_stone_tiles,
                            vis_ground_wood,
                            vis_ground_stone,
                            vis_ground_food,
                            craft_order_needs_material,
                        );
                        let mut scores: Vec<(u16, f32)> = candidates
                            .iter()
                            .map(|p| (p.id, score_weighted(&state, p)))
                            .collect();

                        let camp_pos = faction_registry.home_tile(member.faction_id);
                        for ((_, score), plan_def) in scores.iter_mut().zip(candidates.iter()) {
                            // Persistence bonus reduces plan-switching jitter.
                            if plan_def.id == ai.last_plan_id {
                                *score += 0.2;
                            }

                            // Mild bias toward what a liked ally is already doing.
                            if rel_influence.0.get(&entity) == Some(&plan_def.id) {
                                *score += 0.15;
                            }

                            // Soft penalty for plans that recently failed. Kept small
                            // enough that a strongly-motivated plan (high bias +
                            // visible target) still wins over the per-resource
                            // Explore fallback. Entries expire after
                            // `PLAN_HISTORY_TTL_TICKS`, so a stale failure does not
                            // suppress a plan forever.
                            if let Some(history) = plan_history_opt.as_deref() {
                                let n =
                                    history.recently_failed_count(plan_def.id, clock.tick);
                                if n > 0 {
                                    *score -= 0.3 * n as f32;
                                }
                            }

                            let target_tile = plan_def.memory_target_kind.and_then(|k| {
                                memory_opt
                                    .and_then(|m| m.best_for_dist_weighted(k, (cur_tx, cur_ty)))
                            });

                            if let Some(target) = target_tile {
                                let dist_agent = (target.0 as i32 - cur_tx).abs()
                                    + (target.1 as i32 - cur_ty).abs();
                                let dist_camp = camp_pos.map_or(0, |c| {
                                    (target.0 as i32 - c.0 as i32).abs()
                                        + (target.1 as i32 - c.1 as i32).abs()
                                });
                                *score -= (dist_agent + dist_camp) as f32 * 0.002;
                            } else if plan_def.memory_target_kind.is_some()
                                && plan_def.flags & PF_EXPLORE == 0
                            {
                                // Plan has a target kind but no memory hit. The
                                // candidate filter only lets Food/Wood/Stone gather
                                // plans through here when a visible resource exists,
                                // so this is the visible-but-unmemorised case (or a
                                // Prey/Seed plan running blind). Gentle penalty.
                                // ExploreFor* plans intentionally have no memory
                                // hit (that's why they were picked) — exempting
                                // them keeps the penalty meaningful for blind
                                // gather plans only.
                                *score -= 0.1;
                            }
                        }

                        let idx = select_plan_idx(&scores);
                        let selected = candidates[idx];
                        ai.last_plan_id = selected.id;
                        selected
                    }
                    PlanScoringMethod::Random => candidates[fastrand::usize(..candidates.len())],
                };

                let new_plan = ActivePlan {
                    plan_id: plan_def.id,
                    current_step: 0,
                    started_tick: clock.tick,
                    max_ticks: 5000,
                    reward_acc: 0.0,
                    reward_scale: 0.0,
                    dispatched: false,
                };
                par_commands.command_scope(|mut commands| {
                    commands.entity(entity).insert(new_plan);
                });
                return;
            }

            let active_plan = active_plan_opt.as_deref_mut().unwrap();

            // ── Abandon if goal changed (no longer served by this plan) ──────────
            // Plans flagged with `PF_UNINTERRUPTIBLE` (claimed jobs, blueprint
            // hauls, craft-order pipeline) ignore goal changes — survival
            // goals wait until the task succeeds, fails, or times out.
            let (plan_still_valid, uninterruptible) = plan_registry
                .0
                .iter()
                .find(|p| p.id == active_plan.plan_id)
                .map(|p| {
                    (
                        p.serves_goals.contains(&goal),
                        p.flags & PF_UNINTERRUPTIBLE != 0,
                    )
                })
                .unwrap_or((false, false));
            if !plan_still_valid && !uninterruptible {
                if let Some(history) = plan_history_opt.as_deref_mut() {
                    history.push(active_plan.plan_id, PlanOutcome::Aborted, clock.tick);
                }
                par_commands.command_scope(|mut commands| {
                    commands.entity(entity).remove::<ActivePlan>();
                });
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_tile = (cur_tx as i16, cur_ty as i16);
                ai.dest_tile = ai.target_tile;
                ai.target_entity = None;
                combat_target.0 = None;
                return;
            }

            // ── Timeout check ─────────────────────────────────────────────────────
            if clock.tick.saturating_sub(active_plan.started_tick) > active_plan.max_ticks {
                if let Some(history) = plan_history_opt.as_deref_mut() {
                    history.push(active_plan.plan_id, PlanOutcome::Interrupted, clock.tick);
                }
                // Plans flagged with `PF_DROP_FOOD_ON_TIMEOUT` (currently just
                // ReturnSurplusFood) drop the agent's surplus at their feet
                // when the plan times out — otherwise the surplus would stay
                // bottled in inventory if storage is unreachable.
                let drop_food = plan_registry
                    .0
                    .iter()
                    .find(|p| p.id == active_plan.plan_id)
                    .map_or(false, |p| p.flags & PF_DROP_FOOD_ON_TIMEOUT != 0);
                if drop_food {
                    dropped_food.lock().unwrap().push(entity);
                }
                par_commands.command_scope(|mut commands| {
                    commands.entity(entity).remove::<ActivePlan>();
                });
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_tile = (cur_tx as i16, cur_ty as i16);
                ai.dest_tile = ai.target_tile;
                ai.target_entity = None;
                combat_target.0 = None;
                return;
            }

            // ── Fetch plan and current step ───────────────────────────────────────
            let plan_def = match plan_registry.0.iter().find(|p| p.id == active_plan.plan_id) {
                Some(p) => p,
                None => {
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                    return;
                }
            };

            if active_plan.current_step as usize >= plan_def.steps.len() {
                if let Some(history) = plan_history_opt.as_deref_mut() {
                    history.push(active_plan.plan_id, PlanOutcome::Success, clock.tick);
                }
                par_commands.command_scope(|mut commands| {
                    commands.entity(entity).remove::<ActivePlan>();
                });
                combat_target.0 = None;
                return;
            }

            // ── Step completion: advance step when agent returned Idle+UNEMPLOYED ──
            // Intentionally falls through (no `continue`) so the next step is dispatched
            // in the same tick, eliminating the 1-tick UNEMPLOYED gap that lets
            // goal_update_system flip the goal between Gather and Eat.
            if active_plan.dispatched
                && ai.state == AiState::Idle
                && ai.task_id == PersonAI::UNEMPLOYED
            {
                active_plan.current_step += 1;
                active_plan.dispatched = false;

                if active_plan.current_step as usize >= plan_def.steps.len() {
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                    return;
                }
                // Plan has more steps — fall through to dispatch the next one immediately.
            }

            // Fetch step for current_step (may have just been advanced above).
            let step_id = plan_def.steps[active_plan.current_step as usize];
            let step_def = match step_registry.0.iter().find(|s| s.id == step_id) {
                Some(s) => s,
                None => {
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                    return;
                }
            };

            // ── Dispatch current step if not yet dispatched ───────────────────────
            if !active_plan.dispatched {
                if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                    return;
                }

                // Check preconditions
                if !step_def.preconditions.is_satisfied(
                    &agent,
                    &carrier,
                    equipment_opt,
                    needs.hunger,
                ) {
                    if let Some(history) = plan_history_opt.as_deref_mut() {
                        history.push(
                            active_plan.plan_id,
                            PlanOutcome::FailedPrecondition,
                            clock.tick,
                        );
                    }
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                    return;
                }

                let mut withdraw_intent: Option<(Good, u8)> = None;
                let resolved = resolve_target(
                    step_def,
                    (cur_tx, cur_ty),
                    ai.current_z,
                    &chunk_map,
                    &door_map,
                    &spatial,
                    &plant_map,
                    &plant_query,
                    &faction_registry,
                    &storage_tile_map,
                    &chunk_connectivity,
                    member.faction_id,
                    entity,
                    memory_opt,
                    &item_check,
                    &prey_query,
                    &wild_horse_q,
                    &mut combat_target,
                    &mut target_item,
                    &sites.bp_map,
                    &sites.bp_query,
                    &sites.co_map,
                    &sites.co_query,
                    &sites.corpse_map,
                    &sites.corpse_query,
                    &sites.campfire_map,
                    rescue_target_opt,
                    &agent,
                    &carrier,
                    claim_target_opt,
                    &storage_reservations,
                    &mut withdraw_intent,
                );

                // Discard targets that aren't in the agent's connectivity component.
                // Without this, an agent at z=-10 keeps re-targeting surface gather
                // tiles, hitting the connectivity early-reject on every dispatch
                // and accumulating UnreachableConnectivity failures forever.
                let resolved = resolved.and_then(|(ent, tx, ty)| {
                    let to_chunk = chunk_coord(tx as i32, ty as i32);
                    let to_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                    if chunk_connectivity.is_reachable((cur_chunk, ai.current_z), (to_chunk, to_z))
                    {
                        Some((ent, tx, ty))
                    } else {
                        None
                    }
                });

                if let Some((ent, target_tx, target_ty)) = resolved {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        (target_tx, target_ty),
                        step_def.task,
                        ent,
                        &chunk_graph,
                        &chunk_router,
                        &chunk_map,
                        &chunk_connectivity,
                    );
                    if step_def.task == TaskKind::Craft
                        || step_def.task == TaskKind::WithdrawGood
                    {
                        ai.craft_recipe_id = step_def.extra as u8;
                    }
                    // Equip dispatch: stash slot + good on PersonAI so the
                    // executor knows what to wield. Both are read out by
                    // `equip_task_system`, which clears them on completion.
                    if let StepTarget::EquipItem { slot, good } = step_def.target {
                        ai.equip_slot = slot as u8;
                        ai.craft_recipe_id = good as u8;
                    }
                    // Commit the resolver-chosen withdraw intent (or clear any
                    // stale intent from a prior dispatch). `withdraw_material_task_system`
                    // reads this to know which good and how many to take.
                    ai.withdraw_good = withdraw_intent.map(|(g, _)| g);
                    ai.withdraw_qty = withdraw_intent.map(|(_, q)| q).unwrap_or(0);
                    // Reserve that qty against the chosen storage tile so a
                    // second agent in the same tick sees a smaller effective
                    // stock and either picks another tile/good or aborts.
                    if let Some((good, qty)) = withdraw_intent {
                        if step_def.task == TaskKind::WithdrawMaterial && qty > 0 {
                            // Withdraw is "interacts from adjacent" — the
                            // chosen storage tile is `target_tile`, not
                            // `dest_tile`. assign_task_with_routing has
                            // already populated `target_tile` for the
                            // resolved tile, so use it as the reservation
                            // key.
                            let reserved_tile = (target_tx, target_ty);
                            storage_reservations.add(reserved_tile, good, qty as u32);
                            ai.reserved_tile = reserved_tile;
                            ai.reserved_good = Some(good);
                            ai.reserved_qty = qty;
                        }
                    }
                    active_plan.dispatched = true;
                    active_plan.reward_scale = step_def.reward_scale;
                } else {
                    // No valid target — record the failure and drop the plan so
                    // the next tick re-enters selection. The NN sees this plan in
                    // recent-failure history and (after training) scores it lower.
                    // Underground recovery is handled inside the Explore plan's
                    // step resolver, which the NN can pick like any other plan.
                    if let Some(history) = plan_history_opt.as_deref_mut() {
                        history.push(
                            active_plan.plan_id,
                            PlanOutcome::FailedNoTarget,
                            clock.tick,
                        );
                    }
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                }
            } else {
                // Update target if hunting
                if matches!(step_def.target, StepTarget::HuntPrey) {
                    if let Some(target_ent) = combat_target.0 {
                        if let Ok((target_t, health)) = prey_query.get(target_ent) {
                            if health.is_dead() {
                                // Target is dead, step will complete via Idle+UNEMPLOYED in combat_system or similar
                            } else {
                                // Update target tile to prey's current position if it moved
                                let ptx = (target_t.translation.x / TILE_SIZE).floor() as i16;
                                let pty = (target_t.translation.y / TILE_SIZE).floor() as i16;
                                if ai.dest_tile != (ptx, pty) {
                                    assign_task_with_routing(
                                        &mut ai,
                                        (cur_tx as i16, cur_ty as i16),
                                        cur_chunk,
                                        (ptx, pty),
                                        step_def.task,
                                        Some(target_ent),
                                        &chunk_graph,
                                        &chunk_router,
                                        &chunk_map,
                                        &chunk_connectivity,
                                    );
                                }
                            }
                        } else {
                            // Target lost
                            ai.state = AiState::Idle;
                            ai.task_id = PersonAI::UNEMPLOYED;
                            ai.target_entity = None;
                            combat_target.0 = None;
                        }
                    }
                }
            }
        },
    );
    for entity in dropped_food.into_inner().unwrap() {
        drop_food_events.send(DropAbandonedFoodEvent(entity));
    }
}

// ── Plan gossip system ────────────────────────────────────────────────────────
// Runs in Economy set, after conversation_memory_system.

pub fn plan_gossip_system(
    spatial: Res<SpatialIndex>,
    mut query: Query<(Entity, &AgentGoal, &Transform, &mut KnownPlans, &LodLevel)>,
) {
    // Pass 1: snapshot known plans from Socialize agents
    let snapshots: AHashMap<Entity, Vec<(PlanId, u8)>> = query
        .iter()
        .filter(|(_, goal, _, _, lod)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .map(|(e, _, _, plans, _)| (e, plans.top_entries(8)))
        .collect();

    if snapshots.is_empty() {
        return;
    }

    // Pass 2: apply gossip to Socialize agents within 3 tiles
    for (entity, goal, transform, mut known_plans, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !matches!(goal, AgentGoal::Socialize) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if let Some(entries) = snapshots.get(&other) {
                        for &(plan_id, freshness) in entries {
                            known_plans.receive_gossip(plan_id, freshness / 2);
                        }
                    }
                }
            }
        }
    }
}

// ── Plan decay system ─────────────────────────────────────────────────────────
// Runs in Economy set every 120 ticks.

pub fn plan_decay_system(clock: Res<SimClock>, mut query: Query<&mut KnownPlans>) {
    if clock.tick % 120 != 0 {
        return;
    }
    for mut plans in &mut query {
        plans.decay();
    }
}

/// Builds the RelInfluence map: for each agent, record which plan_id their most-liked
/// ally is currently running. Runs before plan_execution_system to avoid mutable aliasing
/// on ActivePlan.
pub fn rel_influence_system(
    mut influence: ResMut<RelInfluence>,
    query: Query<(Entity, &RelationshipMemory, Option<&ActivePlan>), With<Person>>,
) {
    influence.0.clear();

    // Collect which plan each agent is running.
    let running: AHashMap<Entity, u16> = query
        .iter()
        .filter_map(|(e, _, ap)| ap.map(|p| (e, p.plan_id)))
        .collect();

    // For each agent, find the highest-affinity ally that has an active plan.
    for (entity, rel, _) in query.iter() {
        let mut best_plan: Option<u16> = None;
        let mut best_aff: i8 = 0;
        for slot in &rel.entries {
            if let Some(entry) = slot {
                if entry.affinity > best_aff {
                    if let Some(&plan_id) = running.get(&entry.entity) {
                        best_aff = entry.affinity;
                        best_plan = Some(plan_id);
                    }
                }
            }
        }
        if let Some(plan_id) = best_plan {
            influence.0.insert(entity, plan_id);
        }
    }
}

/// Consumes `DropAbandonedFoodEvent` and dumps the agent's edible inventory at
/// their current tile. Runs in the Economy set after the existing
/// `drop_items_at_destination_system` so they share the spatial/ground-item
/// queries cleanly.
pub fn drop_abandoned_food_system(
    mut commands: Commands,
    mut events: EventReader<DropAbandonedFoodEvent>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut agents: Query<(&Transform, &mut EconomicAgent)>,
) {
    for DropAbandonedFoodEvent(entity) in events.read() {
        let Ok((transform, mut agent)) = agents.get_mut(*entity) else {
            continue;
        };
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut drops: Vec<(Good, u32)> = Vec::new();
        for (it, q) in agent.inventory.iter_mut() {
            if it.good.is_edible() && *q > 0 {
                drops.push((it.good, *q));
                *q = 0;
            }
        }
        for (good, qty) in drops {
            crate::simulation::items::spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                tx,
                ty,
                good,
                qty,
            );
        }
    }
}
