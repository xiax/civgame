use super::animals::{Deer, Horse, Tamed, Wolf};
use super::carry::Carrier;
use super::combat::{CombatTarget, Health};
use super::construction::{Blueprint, BlueprintMap, BuildSiteKind};
use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
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
use super::needs::{Needs, EAT_TRIGGER_HUNGER};
/// Dimensionality of the agent state vector built by `build_state_vec`.
/// See that function for the layout — each `PlanDef::state_weights` is indexed
/// the same way.
pub const STATE_DIM: usize = 38;

/// ε-greedy exploration rate applied during plan selection: with this
/// probability the agent picks a random candidate instead of the highest
/// scorer. Keeps behavior from collapsing onto a single plan.
const PLAN_EPSILON: f32 = 0.10;

// ── State-vector indices (mirror `build_state_vec`) ───────────────────────────
const SI_HUNGER: usize = 0;
const SI_SLEEP: usize = 1;
const SI_SHELTER: usize = 2;
const SI_SAFETY: usize = 3;
const SI_SOCIAL: usize = 4;
const SI_REPRO: usize = 5;
const SI_HAS_FOOD: usize = 6;
const SI_HAS_WOOD: usize = 7;
const SI_HAS_STONE: usize = 8;
const SI_HAS_SEED: usize = 9;
#[allow(dead_code)]
const SI_HAS_COAL: usize = 10;
#[allow(dead_code)]
const SI_SKILL_FARMING: usize = 11;
#[allow(dead_code)]
const SI_SKILL_MINING: usize = 12;
const SI_SKILL_BUILDING: usize = 13;
#[allow(dead_code)]
const SI_SKILL_TRADING: usize = 14;
const SI_SKILL_COMBAT: usize = 15;
const SI_SKILL_CRAFTING: usize = 16;
#[allow(dead_code)]
const SI_SKILL_SOCIAL: usize = 17;
#[allow(dead_code)]
const SI_SKILL_MEDICINE: usize = 18;
const SI_SEASON_FOOD: usize = 19;
const SI_IN_FACTION: usize = 20;
const SI_MEM_FOOD: usize = 21;
const SI_MEM_WOOD: usize = 22;
const SI_MEM_STONE: usize = 23;
const SI_WILLPOWER_DISTRESS: usize = 24;
// 25-34: plan history (2 floats per slot × PLAN_HISTORY_LEN). See build_state_vec.
// Visibility: harvestable resources within VISIBILITY_RADIUS of the agent's
// current tile, normalised by VISIBILITY_SATURATE. Lets gather plans beat
// Explore when food/wood/stone is physically nearby but not yet remembered.
const SI_VIS_FOOD: usize = 35;
const SI_VIS_WOOD: usize = 36;
const SI_VIS_STONE: usize = 37;

const VISIBILITY_RADIUS: i32 = 8;
const VISIBILITY_SATURATE: u8 = 4;
use super::person::{AiState, Drafted, Person, PersonAI, PlayerOrder};
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;
use super::technology::TechId;

pub type StepId = u8;
pub type PlanId = u16;

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
    /// Nearest faction storage tile that contains at least one good currently
    /// needed by an unsatisfied blueprint of the same faction. Used by the
    /// FetchMaterialFromStorage step so haulers can refill their hands from
    /// communal stockpiles instead of relying on a fresh gather wave.
    NearestFactionStorageWithBlueprintMaterial,
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
}

#[derive(Clone, Debug)]
pub struct StepPreconditions {
    pub requires_good: Option<(Good, u32)>,
    pub requires_any_edible: bool,
    pub min_hunger: Option<u8>,
}

impl StepPreconditions {
    pub fn none() -> Self {
        Self {
            requires_good: None,
            requires_any_edible: false,
            min_hunger: None,
        }
    }
    pub fn needs_good(good: Good, qty: u32) -> Self {
        Self {
            requires_good: Some((good, qty)),
            requires_any_edible: false,
            min_hunger: None,
        }
    }
    /// Eat-style preconditions: agent must hold at least one edible and be at
    /// or above the given hunger threshold.
    pub fn eat_when_hungry(min_hunger: u8) -> Self {
        Self {
            requires_good: None,
            requires_any_edible: true,
            min_hunger: Some(min_hunger),
        }
    }

    /// Returns true if the agent currently satisfies these preconditions.
    pub fn is_satisfied(&self, agent: &EconomicAgent, hunger: f32) -> bool {
        if let Some((good, qty)) = self.requires_good {
            if agent.quantity_of(good) < qty {
                return false;
            }
        }
        if self.requires_any_edible && agent.total_food() == 0 {
            return false;
        }
        if let Some(min) = self.min_hunger {
            if hunger < min as f32 {
                return false;
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
}

/// Build a `state_weights` array from a sparse list of (index, weight) pairs.
/// All other entries are zero.
fn mk_weights(pairs: &[(usize, f32)]) -> [f32; STATE_DIM] {
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

pub const RETURN_SURPLUS_FOOD_PLAN_ID: PlanId = 24;
pub const EXPLORE_PLAN_ID: PlanId = 28;

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

pub const PLAN_HISTORY_LEN: usize = 5;

/// Per-agent ring buffer of the last few plan outcomes. Written at every
/// teardown site (success, precondition fail, resolve fail, goal change,
/// interruption). Read by `build_state_vec` so the NN gets feedback on
/// recent plan failures.
#[derive(Component, Default)]
pub struct PlanHistory {
    pub entries: [Option<(PlanId, PlanOutcome)>; PLAN_HISTORY_LEN],
    pub head: u8,
}

impl PlanHistory {
    pub fn push(&mut self, plan_id: PlanId, outcome: PlanOutcome) {
        let i = (self.head as usize) % PLAN_HISTORY_LEN;
        self.entries[i] = Some((plan_id, outcome));
        self.head = ((self.head as usize + 1) % PLAN_HISTORY_LEN) as u8;
    }

    /// True if `plan_id` appears anywhere in the buffer with a failure outcome.
    pub fn recently_failed(&self, plan_id: PlanId) -> bool {
        self.entries.iter().any(
            |slot| matches!(slot, Some((id, outcome)) if *id == plan_id && outcome.is_failure()),
        )
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

// ── Built-in step and plan definitions ───────────────────────────────────────

static GRASS_TILES: &[TileKind] = &[TileKind::Grass];
static FARMLAND_TILES: &[TileKind] = &[TileKind::Farmland];
static FOREST_TILES: &[TileKind] = &[TileKind::Forest];
static STONE_TILES: &[TileKind] = &[TileKind::Stone];

// Step 9 = Eat, Step 10 = WithdrawFood (defined in register_builtin_steps)
static PLAN_STEPS_0: &[StepId] = &[0, 9]; // ForageFood → Eat
static PLAN_STEPS_1: &[StepId] = &[1, 9]; // FarmFood → Eat
static PLAN_STEPS_2: &[StepId] = &[2]; // GatherWood
static PLAN_STEPS_3: &[StepId] = &[3]; // GatherStone
static PLAN_STEPS_4: &[StepId] = &[4, 1, 9]; // PlantAndFarm → Eat
static PLAN_STEPS_5: &[StepId] = &[5, 6, 9]; // HuntFood → Eat
static PLAN_STEPS_6: &[StepId] = &[6, 9]; // ScavengeFood → Eat
static PLAN_STEPS_7: &[StepId] = &[2, 28, 25]; // GatherWood, HaulToBlueprint, BuildAnyBlueprint
static PLAN_STEPS_29: &[StepId] = &[32, 28, 25]; // FetchMaterialFromStorage, HaulToBlueprint, BuildAnyBlueprint
static PLAN_STEPS_9: &[StepId] = &[10, 9]; // WithdrawAndEat: WithdrawFood → Eat
static PLAN_STEPS_10: &[StepId] = &[11]; // TameHorse: TameAnimal

static SURVIVE_GOALS: &[AgentGoal] = &[AgentGoal::Survive];
static GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherFood];
static TAME_HORSE_GOALS: &[AgentGoal] = &[AgentGoal::TameHorse];
static GATHER_WOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherWood];
static GATHER_STONE_GOALS: &[AgentGoal] = &[AgentGoal::GatherStone];
static SURVIVE_AND_GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::Survive, AgentGoal::GatherFood];
static FARM_AND_GATHER_FOOD_GOALS: &[AgentGoal] = &[
    AgentGoal::Survive,
    AgentGoal::GatherFood,
    AgentGoal::Farm,
];
static BUILD_GOALS: &[AgentGoal] = &[AgentGoal::Build];
static CRAFT_GOALS: &[AgentGoal] = &[AgentGoal::Craft];
static RESCUE_GOALS: &[AgentGoal] = &[AgentGoal::Rescue];

static PLAN_STEPS_23: &[StepId] = &[27]; // RescueAlly: EngageRescue
static PLAN_STEPS_24: &[StepId] = &[12]; // ReturnSurplusFood: DepositGoods at faction storage
static PLAN_STEPS_25: &[StepId] = &[9]; // EatFromInventory: Eat (gated by hunger + edible-on-hand)
static PLAN_STEPS_26: &[StepId] = &[29]; // PlaySocial: PlayWithPartner (resolves partner inline)
static PLAN_STEPS_27: &[StepId] = &[30]; // PlaySolo: PlayWithItem (resolves item inline)
static PLAN_STEPS_28: &[StepId] = &[31]; // Explore: walk to a random reachable tile near home
static PLAN_STEPS_30: &[StepId] = &[33, 36]; // PlayByPlanting: WithdrawSeed → PlantSeedAsPlay
static PLAN_STEPS_31: &[StepId] = &[34, 37]; // PlayByThrowingRocks: WithdrawStone → ThrowRocksAsPlay
static PLAN_STEPS_32: &[StepId] = &[35, 30]; // PlayWithStoredToy: WithdrawPlayItem → PlayWithItem (step 30, plays in place when held)

static EXPLORE_GOALS: &[AgentGoal] = &[
    AgentGoal::Survive,
    AgentGoal::GatherFood,
    AgentGoal::GatherWood,
    AgentGoal::GatherStone,
];

static PLAY_GOALS: &[AgentGoal] = &[AgentGoal::Play];

static RETURN_CAMP_GOALS: &[AgentGoal] = &[AgentGoal::ReturnCamp];

// Craft plan step sequences (step IDs match register_builtin_steps above)
// 12=DepositGoods, 13=CollectSkin
// 14=CraftStoneTools, 15=CraftSpear, 16=CraftTorch, 17=CraftBow, 18=CraftCloth
// 19=CraftPottery, 20=CraftShield, 21=CraftLeatherArmor, 22=CraftIronTools, 23=CraftSword
static PLAN_STEPS_11: &[StepId] = &[2, 3, 14, 12]; // MakeStoneTools
static PLAN_STEPS_12: &[StepId] = &[2, 3, 15, 12]; // MakeSpear
static PLAN_STEPS_13: &[StepId] = &[2, 16, 12]; // MakeTorch
static PLAN_STEPS_14: &[StepId] = &[2, 5, 13, 17, 12]; // MakeBow
static PLAN_STEPS_15: &[StepId] = &[1, 1, 18, 12]; // MakeCloth
static PLAN_STEPS_16: &[StepId] = &[3, 2, 19, 12]; // MakePottery
static PLAN_STEPS_17: &[StepId] = &[2, 2, 20, 12]; // MakeShield
static PLAN_STEPS_18: &[StepId] = &[5, 13, 5, 13, 21, 12]; // MakeLeatherArmor
static PLAN_STEPS_19: &[StepId] = &[3, 3, 22, 12]; // MakeIronTools
static PLAN_STEPS_20: &[StepId] = &[3, 2, 23, 12]; // MakeSword

pub fn register_builtin_steps(registry: &mut StepRegistry) {
    registry.0 = vec![
        StepDef {
            // 0: ForageGrass — targets BerryBushes, falls back via memory
            id: 0,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::BerryBush),
            extra: 0,
        },
        StepDef {
            // 1: FarmFarmland — targets Grain, falls back via memory
            id: 1,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::Grain),
            extra: 0,
        },
        StepDef {
            // 2: ChopForest
            id: 2,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Wood),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 3: MineStone
            id: 3,
            task: TaskKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 4: PlantSeed (requires Seed in inventory)
            id: 4,
            task: TaskKind::Planter,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::Seed, 1),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 5: Hunt
            id: 5,
            task: TaskKind::Hunter,
            target: StepTarget::HuntPrey,
            preconditions: StepPreconditions::needs_good(Good::Weapon, 1),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 6: CollectFood
            id: 6,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestEdible,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 7: (unused — reserved for future use)
            id: 7,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 8: (unused — reserved for future use)
            id: 8,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 9: Eat — consume edibles from inventory in place. Gated on hunger
            // so plans don't waste food when the agent is already sated.
            id: 9,
            task: TaskKind::Eat,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::eat_when_hungry(EAT_TRIGGER_HUNGER),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 10: WithdrawFood — pull one edible from a faction storage tile
            id: 10,
            task: TaskKind::WithdrawFood,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 11: TameAnimal — work adjacent to a wild horse for ~100 ticks
            id: 11,
            task: TaskKind::TameAnimal,
            target: StepTarget::NearestWildHorse,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            extra: 0,
        },
        // ── Crafting steps ────────────────────────────────────────────────────
        StepDef {
            // 12: DepositGoods — deposit crafted items at faction storage
            id: 12,
            task: TaskKind::DepositResource,
            target: StepTarget::NearestFactionStorage,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.1,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 13: CollectSkin — pick up Skin from ground (after hunting)
            id: 13,
            task: TaskKind::Scavenge,
            target: StepTarget::NearestItem(Good::Skin),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 14: CraftStoneTools (recipe 0)
            id: 14,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Stone, 2),
            reward_scale: 0.8,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 15: CraftSpear (recipe 1)
            id: 15,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Stone, 1),
            reward_scale: 0.8,
            plant_filter: None,
            extra: 1,
        },
        StepDef {
            // 16: CraftTorch (recipe 2)
            id: 16,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Wood, 2),
            reward_scale: 0.5,
            plant_filter: None,
            extra: 2,
        },
        StepDef {
            // 17: CraftBow (recipe 3)
            id: 17,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Skin, 1),
            reward_scale: 0.9,
            plant_filter: None,
            extra: 3,
        },
        StepDef {
            // 18: CraftCloth (recipe 4)
            id: 18,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Grain, 3),
            reward_scale: 0.7,
            plant_filter: None,
            extra: 4,
        },
        StepDef {
            // 19: CraftPottery (recipe 5)
            id: 19,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Stone, 2),
            reward_scale: 0.6,
            plant_filter: None,
            extra: 5,
        },
        StepDef {
            // 20: CraftShield (recipe 6)
            id: 20,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Wood, 3),
            reward_scale: 0.8,
            plant_filter: None,
            extra: 6,
        },
        StepDef {
            // 21: CraftLeatherArmor (recipe 7)
            id: 21,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Skin, 2),
            reward_scale: 0.9,
            plant_filter: None,
            extra: 7,
        },
        StepDef {
            // 22: CraftIronTools (recipe 8)
            id: 22,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Iron, 2),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 8,
        },
        StepDef {
            // 23: CraftSword (recipe 9)
            id: 23,
            task: TaskKind::Craft,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Iron, 2),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 9,
        },
        StepDef {
            // 24: (unused — reserved for future use)
            id: 24,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 25: BuildAnyBlueprint — finds the nearest accessible blueprint of any kind
            // and contributes wood + labor. Requirements come from the blueprint itself.
            id: 25,
            task: TaskKind::Construct,
            target: StepTarget::NearestAnyBlueprint,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.2,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 26: (unused — formerly FindMate; reproduction is now passive via co-sleeping)
            id: 26,
            task: TaskKind::Idle,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 27: EngageRescue — route to the attacker stored on RescueTarget and engage.
            // CombatTarget is already set by respond_to_distress_system; combat_system
            // takes over as soon as the responder is adjacent.
            id: 27,
            task: TaskKind::Defend,
            target: StepTarget::RescueAttacker,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.5,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 28: HaulToBlueprint — carry currently-held materials to the nearest
            // blueprint that still needs them and drop them in. Excess stays in
            // the hauler's inventory; the step ends as soon as the drop is applied.
            id: 28,
            task: TaskKind::HaulMaterials,
            target: StepTarget::NearestBlueprintNeedingHeldMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 29: PlayWithPartner — route to the nearest play partner and
            // recreate together. play_system handles tick-by-tick willpower
            // refill, social fill, and bilateral affinity.
            id: 29,
            task: TaskKind::Play,
            target: StepTarget::NearestPlayPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 30: PlayWithItem — solo play. Resolves to the agent's tile if
            // they already hold an entertaining good, else the nearest ground
            // item with non-zero entertainment_value.
            id: 30,
            task: TaskKind::Play,
            target: StepTarget::NearestPlayItem,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 31: Explore — walk to a random reachable tile near home.
            // Used by the Explore plan as the NN's "no good options right now"
            // choice; reward_scale is intentionally low so the network reaches
            // for it only when other plans score worse.
            id: 31,
            task: TaskKind::Explore,
            target: StepTarget::ExploreTile,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.05,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 32: FetchMaterialFromStorage — route to the nearest faction
            // storage tile that holds a good currently needed by an unsatisfied
            // blueprint, and pull one unit into the agent's inventory. Pairs
            // with step 28 (HaulToBlueprint) so wood/stone already deposited
            // in granaries can be ferried to in-progress build sites without
            // requiring a fresh gather wave.
            id: 32,
            task: TaskKind::WithdrawMaterial,
            target: StepTarget::NearestFactionStorageWithBlueprintMaterial,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 33: WithdrawSeed — pull one Seed from a faction storage tile so
            // the agent can plant it as recreation in step 36.
            id: 33,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Seed),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: Good::Seed as u8,
        },
        StepDef {
            // 34: WithdrawStone — pull one Stone from a faction storage tile so
            // the agent can throw it as recreation in step 37.
            id: 34,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithGood(Good::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: Good::Stone as u8,
        },
        StepDef {
            // 35: WithdrawPlayItem — pull one entertainment-valued good from a
            // faction storage tile (sentinel 255 in craft_recipe_id signals
            // "first item with entertainment_value > 0").
            id: 35,
            task: TaskKind::WithdrawGood,
            target: StepTarget::NearestFactionStorageWithEntertainment,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.2,
            plant_filter: None,
            extra: 255,
        },
        StepDef {
            // 36: PlantSeedAsPlay — plant a held Seed on a grass tile as
            // recreation. Same effect as Planter (spawns Grain, Farming XP +
            // activity), plus a one-shot willpower burst on completion.
            id: 36,
            task: TaskKind::PlayPlant,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::Seed, 1),
            reward_scale: 0.6,
            plant_filter: None,
            extra: 0,
        },
        StepDef {
            // 37: ThrowRocksAsPlay — throw a held Stone as recreation. Consumes
            // one Stone, awards Combat XP + ActivityKind::Combat, bursts
            // willpower. Resolves to the agent's current tile (they throw in
            // place; the rock is consumed).
            id: 37,
            task: TaskKind::PlayThrow,
            target: StepTarget::SelfPosition,
            preconditions: StepPreconditions::needs_good(Good::Stone, 1),
            reward_scale: 0.6,
            plant_filter: None,
            extra: 0,
        },
    ];
}

pub fn register_builtin_plans(registry: &mut PlanRegistry) {
    // Hand-tuned linear weights on `build_state_vec` features (see `STATE_DIM`
    // and `SI_*` constants). Score = dot(state, state_weights) + bias + manual
    // bonuses (persistence, ally, distance) applied at selection time.
    registry.0 = vec![
        PlanDef {
            id: 0,
            name: "ForageFood",
            steps: PLAN_STEPS_0,
            state_weights: mk_weights(&[
                (SI_HUNGER, 1.5),
                (SI_HAS_FOOD, -0.3),
                (SI_MEM_FOOD, 0.2),
                (SI_VIS_FOOD, 0.8),
            ]),
            bias: 0.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 1,
            name: "FarmFood",
            steps: PLAN_STEPS_1,
            state_weights: mk_weights(&[
                (SI_HUNGER, 1.2),
                (SI_HAS_FOOD, -0.3),
                (SI_SKILL_FARMING, 0.3),
                (SI_SEASON_FOOD, 0.5),
            ]),
            bias: 0.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 2,
            name: "GatherWood",
            steps: PLAN_STEPS_2,
            state_weights: mk_weights(&[
                (SI_MEM_WOOD, 0.2),
                (SI_HAS_WOOD, -0.1),
                (SI_VIS_WOOD, 0.6),
            ]),
            bias: 0.1,
            serves_goals: GATHER_WOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 3,
            name: "GatherStone",
            steps: PLAN_STEPS_3,
            state_weights: mk_weights(&[
                (SI_MEM_STONE, 0.2),
                (SI_HAS_STONE, -0.1),
                (SI_SKILL_MINING, 0.2),
                (SI_VIS_STONE, 0.6),
            ]),
            bias: 0.1,
            serves_goals: GATHER_STONE_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 4,
            name: "PlantAndFarm",
            steps: PLAN_STEPS_4,
            state_weights: mk_weights(&[
                (SI_HUNGER, 0.8),
                (SI_HAS_SEED, 0.4),
                (SI_SKILL_FARMING, 0.4),
                (SI_SEASON_FOOD, 0.5),
            ]),
            bias: 0.0,
            serves_goals: FARM_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 5,
            name: "HuntFood",
            steps: PLAN_STEPS_5,
            state_weights: mk_weights(&[
                (SI_HUNGER, 1.0),
                (SI_HAS_FOOD, -0.2),
                (SI_SKILL_COMBAT, 0.5),
            ]),
            bias: 0.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 6,
            name: "ScavengeFood",
            steps: PLAN_STEPS_6,
            state_weights: mk_weights(&[(SI_HUNGER, 1.0), (SI_HAS_FOOD, -0.3), (SI_VIS_FOOD, 0.4)]),
            bias: 0.0,
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 7,
            name: "BuildBlueprint",
            steps: PLAN_STEPS_7,
            state_weights: mk_weights(&[
                (SI_SHELTER, 0.8),
                (SI_SAFETY, 0.3),
                (SI_SKILL_BUILDING, 0.4),
            ]),
            bias: 0.2,
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 8,
            name: "BuildBed",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0, // unused — never selected
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 9,
            name: "WithdrawAndEat",
            steps: PLAN_STEPS_9,
            state_weights: mk_weights(&[(SI_HUNGER, 1.5), (SI_IN_FACTION, 0.5)]),
            bias: 0.0,
            serves_goals: SURVIVE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 10,
            name: "TameHorse",
            steps: PLAN_STEPS_10,
            state_weights: mk_weights(&[]),
            bias: 0.1,
            serves_goals: TAME_HORSE_GOALS,
            tech_gate: Some(super::technology::HORSE_TAMING),
            memory_target_kind: None,
        },
        // ── Crafting plans ────────────────────────────────────────────────────
        PlanDef {
            id: 11,
            name: "MakeStoneTools",
            steps: PLAN_STEPS_11,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.4), (SI_HAS_STONE, 0.3)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FLINT_KNAPPING),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 12,
            name: "MakeSpear",
            steps: PLAN_STEPS_12,
            state_weights: mk_weights(&[
                (SI_SKILL_CRAFTING, 0.4),
                (SI_SAFETY, 0.3),
                (SI_HAS_STONE, 0.3),
            ]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::HUNTING_SPEAR),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 13,
            name: "MakeTorch",
            steps: PLAN_STEPS_13,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.3), (SI_HAS_WOOD, 0.2)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FIRE_MAKING),
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 14,
            name: "MakeBow",
            steps: PLAN_STEPS_14,
            state_weights: mk_weights(&[
                (SI_SKILL_CRAFTING, 0.4),
                (SI_SAFETY, 0.4),
                (SI_HAS_WOOD, 0.3),
            ]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::BOW_AND_ARROW),
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 15,
            name: "MakeCloth",
            steps: PLAN_STEPS_15,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.3)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::LOOM_WEAVING),
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 16,
            name: "MakePottery",
            steps: PLAN_STEPS_16,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.3), (SI_HAS_STONE, 0.2)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FIRED_POTTERY),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 17,
            name: "MakeShield",
            steps: PLAN_STEPS_17,
            state_weights: mk_weights(&[
                (SI_SKILL_CRAFTING, 0.5),
                (SI_SAFETY, 0.5),
                (SI_HAS_WOOD, 0.3),
            ]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 18,
            name: "MakeLeatherArmor",
            steps: PLAN_STEPS_18,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.5), (SI_SAFETY, 0.6)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 19,
            name: "MakeIronTools",
            steps: PLAN_STEPS_19,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.5), (SI_HAS_STONE, 0.3)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::COPPER_TOOLS),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 20,
            name: "MakeSword",
            steps: PLAN_STEPS_20,
            state_weights: mk_weights(&[(SI_SKILL_CRAFTING, 0.6), (SI_SAFETY, 0.7)]),
            bias: 0.0,
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::BRONZE_WEAPONS),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 21,
            name: "BuildCampfire",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0, // unused
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 22,
            name: "(unused)",
            steps: &[],
            state_weights: mk_weights(&[]),
            bias: -10.0,
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 23,
            name: "RescueAlly",
            steps: PLAN_STEPS_23,
            state_weights: mk_weights(&[(SI_SAFETY, 0.5), (SI_SOCIAL, 0.3)]),
            bias: 0.5, // bias up so allies tend to respond when this goal fires
            serves_goals: RESCUE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 24,
            name: "ReturnSurplusFood",
            steps: PLAN_STEPS_24,
            state_weights: mk_weights(&[(SI_HAS_FOOD, 0.3), (SI_IN_FACTION, 0.3)]),
            bias: 0.0,
            serves_goals: RETURN_CAMP_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 25,
            name: "EatFromInventory",
            steps: PLAN_STEPS_25,
            state_weights: mk_weights(&[(SI_HUNGER, 1.5), (SI_HAS_FOOD, 0.5)]),
            bias: 0.0,
            serves_goals: SURVIVE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 26,
            name: "PlaySocial",
            steps: PLAN_STEPS_26,
            state_weights: mk_weights(&[(SI_SOCIAL, 1.5), (SI_WILLPOWER_DISTRESS, 0.5)]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 27,
            name: "PlaySolo",
            steps: PLAN_STEPS_27,
            state_weights: mk_weights(&[(SI_SOCIAL, 0.6), (SI_WILLPOWER_DISTRESS, 0.7)]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Explore is the fallback for gathering goals. It carries no
            // need-driven weight; the candidate filter drops Food/Wood/Stone
            // gather plans when the agent has neither memory nor visibility of
            // the resource, so Explore wins by default in that case.
            id: 28,
            name: "Explore",
            steps: PLAN_STEPS_28,
            state_weights: mk_weights(&[]),
            bias: 0.0,
            serves_goals: EXPLORE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Sibling of BuildBlueprint that pulls materials out of communal
            // storage instead of gathering fresh from the world. Keeps in-progress
            // build sites moving once the initial gather wave has dropped its
            // surplus into granaries — without this plan, blueprints stall at
            // the "haulers can only deliver what they happen to be carrying"
            // step (NearestBlueprintNeedingHeldMaterial).
            id: 29,
            name: "HaulFromStorageAndBuild",
            steps: PLAN_STEPS_29,
            state_weights: mk_weights(&[
                (SI_SHELTER, 0.8),
                (SI_SAFETY, 0.3),
                (SI_SKILL_BUILDING, 0.4),
            ]),
            bias: 0.2,
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Take a Seed from faction storage and plant it as recreation.
            // Doubles as low-effort farming progress: each completion spawns a
            // Grain plant and feeds Farming activity for tech discovery.
            id: 30,
            name: "PlayByPlanting",
            steps: PLAN_STEPS_30,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.6),
                (SI_SKILL_FARMING, 0.4),
                (SI_SEASON_FOOD, 0.3),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Take a Stone from faction storage and throw it as recreation.
            // Each completion increments ActivityKind::Combat (driving combat
            // tech discovery) and grants a small Combat XP bump.
            id: 31,
            name: "PlayByThrowingRocks",
            steps: PLAN_STEPS_31,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.6),
                (SI_SKILL_COMBAT, 0.4),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Pull an entertainment-valued good (luxury, cloth, tools, …) from
            // faction storage and play with it in place. Chains into PlaySolo's
            // PlayWithItem step so the willpower-per-tick refill scales by the
            // toy's `entertainment_value`.
            id: 32,
            name: "PlayWithStoredToy",
            steps: PLAN_STEPS_32,
            state_weights: mk_weights(&[
                (SI_WILLPOWER_DISTRESS, 0.7),
                (SI_SOCIAL, 0.3),
            ]),
            bias: 0.0,
            serves_goals: PLAY_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
    ];
}

// ── State vector ──────────────────────────────────────────────────────────────

pub fn build_state_vec(
    needs: &Needs,
    agent: &EconomicAgent,
    skills: &Skills,
    member: &FactionMember,
    memory: Option<&AgentMemory>,
    calendar: &Calendar,
    plan_history: Option<&PlanHistory>,
    vis_food: u8,
    vis_wood: u8,
    vis_stone: u8,
) -> [f32; STATE_DIM] {
    let mut s = [0.0f32; STATE_DIM];

    // 0-5: needs
    s[0] = needs.hunger as f32 / 255.0;
    s[1] = needs.sleep as f32 / 255.0;
    s[2] = needs.shelter as f32 / 255.0;
    s[3] = needs.safety as f32 / 255.0;
    s[4] = needs.social as f32 / 255.0;
    s[5] = needs.reproduction as f32 / 255.0;

    // 6-10: inventory has (Food, Wood, Stone, Seed, Coal)
    s[6] = if agent.total_food() > 0 { 1.0 } else { 0.0 };
    s[7] = if agent.quantity_of(Good::Wood) > 0 {
        1.0
    } else {
        0.0
    };
    s[8] = if agent.quantity_of(Good::Stone) > 0 {
        1.0
    } else {
        0.0
    };
    s[9] = if agent.quantity_of(Good::Seed) > 0 {
        1.0
    } else {
        0.0
    };
    s[10] = if agent.quantity_of(Good::Coal) > 0 {
        1.0
    } else {
        0.0
    };

    // 11-18: all 8 skills
    for k in 0..8usize {
        s[11 + k] = (skills.0[k].min(255) as f32) / 255.0;
    }

    // 19: season multiplier
    s[19] = (calendar.food_yield_multiplier() / 1.3).clamp(0.0, 1.0);

    // 20: in faction
    s[20] = if member.faction_id != SOLO { 1.0 } else { 0.0 };

    // 21-23: memory availability
    if let Some(mem) = memory {
        s[21] = if mem.best_for(MemoryKind::Food).is_some() {
            1.0
        } else {
            0.0
        };
        s[22] = if mem.best_for(MemoryKind::Wood).is_some() {
            1.0
        } else {
            0.0
        };
        s[23] = if mem.best_for(MemoryKind::Stone).is_some() {
            1.0
        } else {
            0.0
        };
    }

    // 24: willpower distress (1.0 = drained, 0.0 = full vigor) — inverted to
    // match the convention used by the other six need slots.
    s[24] = ((255.0 - needs.willpower) / 255.0).clamp(0.0, 1.0);

    // 25-34: last PLAN_HISTORY_LEN plan outcomes (2 floats per slot).
    // For each slot: (plan_id_norm, failure_flag). Lets the NN learn to
    // deprioritize plans that just failed and reach for Explore when
    // recent attempts have all flunked.
    if let Some(history) = plan_history {
        for i in 0..PLAN_HISTORY_LEN {
            let base = 25 + i * 2;
            match history.entries[i] {
                Some((plan_id, outcome)) => {
                    s[base] = (plan_id as f32 + 1.0) / 32.0;
                    s[base + 1] = if outcome.is_failure() { 1.0 } else { 0.0 };
                }
                None => {
                    s[base] = 0.0;
                    s[base + 1] = 0.0;
                }
            }
        }
    }

    // 35-37: visible harvestable resources within VISIBILITY_RADIUS, normalised
    // to [0, 1] at VISIBILITY_SATURATE matches.
    let sat = VISIBILITY_SATURATE as f32;
    s[SI_VIS_FOOD] = (vis_food as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_WOOD] = (vis_wood as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_STONE] = (vis_stone as f32 / sat).clamp(0.0, 1.0);

    s
}

/// Counts mature edible plants and edible ground items within
/// `VISIBILITY_RADIUS` of the agent's tile, saturating at
/// `VISIBILITY_SATURATE`. Cheap because plan selection is bucketed (~1 Hz per
/// agent) and scanning early-exits once saturated.
pub(crate) fn count_visible_food(
    tx: i32,
    ty: i32,
    plant_map: &PlantMap,
    plants: &Query<&Plant>,
    spatial: &SpatialIndex,
    items: &Query<&GroundItem>,
) -> u8 {
    let r = VISIBILITY_RADIUS;
    let r2 = r * r;
    let mut n: u8 = 0;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let pt = (tx + dx, ty + dy);
            if let Some(&e) = plant_map.0.get(&pt) {
                if let Ok(p) = plants.get(e) {
                    if p.stage == GrowthStage::Mature
                        && matches!(p.kind, PlantKind::Grain | PlantKind::BerryBush)
                    {
                        n = n.saturating_add(1);
                        if n >= VISIBILITY_SATURATE {
                            return n;
                        }
                    }
                }
            }
            for &e in spatial.get(pt.0, pt.1) {
                if let Ok(item) = items.get(e) {
                    if item.item.good.is_edible() {
                        n = n.saturating_add(1);
                        if n >= VISIBILITY_SATURATE {
                            return n;
                        }
                    }
                }
            }
        }
    }
    n
}

pub(crate) fn count_visible_wood(
    tx: i32,
    ty: i32,
    plant_map: &PlantMap,
    plants: &Query<&Plant>,
    spatial: &SpatialIndex,
    items: &Query<&GroundItem>,
) -> u8 {
    let r = VISIBILITY_RADIUS;
    let r2 = r * r;
    let mut n: u8 = 0;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let pt = (tx + dx, ty + dy);
            if let Some(&e) = plant_map.0.get(&pt) {
                if let Ok(p) = plants.get(e) {
                    if p.stage == GrowthStage::Mature && p.kind == PlantKind::Tree {
                        n = n.saturating_add(1);
                        if n >= VISIBILITY_SATURATE {
                            return n;
                        }
                    }
                }
            }
            for &e in spatial.get(pt.0, pt.1) {
                if let Ok(item) = items.get(e) {
                    if item.item.good == Good::Wood {
                        n = n.saturating_add(1);
                        if n >= VISIBILITY_SATURATE {
                            return n;
                        }
                    }
                }
            }
        }
    }
    n
}

pub(crate) fn count_visible_stone(
    tx: i32,
    ty: i32,
    chunk_map: &ChunkMap,
    spatial: &SpatialIndex,
    items: &Query<&GroundItem>,
) -> u8 {
    let r = VISIBILITY_RADIUS;
    let r2 = r * r;
    let mut n: u8 = 0;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let (sx, sy) = (tx + dx, ty + dy);
            if chunk_map.tile_kind_at(sx, sy) == Some(TileKind::Stone) {
                n = n.saturating_add(1);
                if n >= VISIBILITY_SATURATE {
                    return n;
                }
            }
            for &e in spatial.get(sx, sy) {
                if let Ok(item) = items.get(e) {
                    if item.item.good == Good::Stone {
                        n = n.saturating_add(1);
                        if n >= VISIBILITY_SATURATE {
                            return n;
                        }
                    }
                }
            }
        }
    }
    n
}

// ── Target resolution ─────────────────────────────────────────────────────────

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
    goal: &AgentGoal,
    memory: Option<&AgentMemory>,
    item_query: &Query<&GroundItem>,
    prey_query: &Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    wild_horse_q: &Query<Entity, (With<Horse>, Without<Tamed>)>,
    combat_target: &mut CombatTarget,
    target_item: &mut TargetItem,
    bp_map: &BlueprintMap,
    bp_query: &Query<&Blueprint>,
    rescue_target: Option<&RescueTarget>,
    agent: &EconomicAgent,
    carrier: &Carrier,
) -> Option<(Option<Entity>, i16, i16)> {
    const VIEW_RADIUS: i32 = 15;

    let is_gathering = matches!(
        goal,
        AgentGoal::GatherFood | AgentGoal::GatherWood | AgentGoal::GatherStone
    );

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
                is_gathering,
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
            // 2. Check memory
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
                if let Some((entity, tx, ty)) = mem.best_entity_for_dist_weighted(mkind, pos) {
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
                is_gathering,
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
            // 2. Check memory — only accept GroundItem entities; mature plants
            //    (berry bushes, grain) are also recorded under MemoryKind::Food
            //    but are harvested via ForageFood (TaskKind::Gather), not Scavenge.
            if let Some(mem) = memory {
                if let Some((entity, tx, ty)) =
                    mem.best_entity_for_dist_weighted_filtered(MemoryKind::Food, pos, |e| {
                        item_query.get(e).is_ok()
                    })
                {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::FactionCamp => faction_registry
            .home_tile(faction_id)
            .map(|(tx, ty)| (None, tx, ty)),
        StepTarget::SelfPosition => Some((None, pos.0 as i16, pos.1 as i16)),
        StepTarget::NearestFactionStorage => storage_tile_map
            .nearest_for_faction(faction_id, pos)
            .map(|(tx, ty)| (None, tx, ty)),
        StepTarget::NearestFactionStorageWithBlueprintMaterial => {
            // Build the set of goods currently needed by some unsatisfied
            // blueprint of this faction (including any blueprint personally
            // owned by this agent). If nothing is wanted, no target.
            let mut needed: [bool; crate::economy::goods::GOOD_COUNT] =
                [false; crate::economy::goods::GOOD_COUNT];
            let mut any_needed = false;
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
                        needed[bp.deposits[i].good as usize] = true;
                        any_needed = true;
                    }
                }
            }
            if !any_needed {
                return None;
            }

            // Find the nearest faction storage tile that holds at least one
            // unit of any needed good.
            let tiles = storage_tile_map.by_faction.get(&faction_id)?;
            let mut best: Option<(i16, i16)> = None;
            let mut best_dist = i32::MAX;
            for &(tx, ty) in tiles {
                let mut has_useful = false;
                for &gi_entity in spatial.get(tx as i32, ty as i32) {
                    if let Ok(gi) = item_query.get(gi_entity) {
                        if gi.qty > 0 && needed[gi.item.good as usize] {
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
            best.map(|(tx, ty)| (None, tx, ty))
        }
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
    pub door_map: Res<'w, crate::simulation::construction::DoorMap>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub drop_food_events: EventWriter<'w, DropAbandonedFoodEvent>,
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
);

type OptionalQuery<'a> = (
    Option<&'a AgentMemory>,
    Option<&'a KnownPlans>,
    Option<&'a PlanScoringMethod>,
    Option<&'a mut ActivePlan>,
    Option<&'a RelationshipMemory>,
    Option<&'a RescueTarget>,
    Option<&'a mut PlanHistory>,
);

pub fn plan_execution_system(
    par_commands: ParallelCommands,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    registries: PlanRegistries,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
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
            ),
            (
                memory_opt,
                known_plans_opt,
                scoring_opt,
                mut active_plan_opt,
                _rel_opt,
                rescue_target_opt,
                mut plan_history_opt,
            ),
        )| {
            if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
                return;
            }

            // Only handle plan-driven goals
            if !matches!(
                goal,
                AgentGoal::Survive
                    | AgentGoal::GatherFood
                    | AgentGoal::GatherWood
                    | AgentGoal::GatherStone
                    | AgentGoal::Build
                    | AgentGoal::TameHorse
                    | AgentGoal::Rescue
                    | AgentGoal::ReturnCamp
                    | AgentGoal::Play
            ) {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = chunk_coord(cur_tx, cur_ty);

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
                // scoring share the same numbers. Cheap (bucketed plan-selection,
                // saturating early-exit at 4 hits per kind).
                let vis_food = count_visible_food(
                    cur_tx,
                    cur_ty,
                    &plant_map,
                    &plant_query,
                    &spatial,
                    &item_check,
                );
                let vis_wood = count_visible_wood(
                    cur_tx,
                    cur_ty,
                    &plant_map,
                    &plant_query,
                    &spatial,
                    &item_check,
                );
                let vis_stone =
                    count_visible_stone(cur_tx, cur_ty, &chunk_map, &spatial, &item_check);

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
                    // Bug 3 fix: skip plans whose first step has unmet preconditions so we
                    // don't enter a tight pick-then-immediately-abandon loop.
                    .filter(|p| {
                        p.steps
                            .first()
                            .and_then(|&sid| step_registry.0.iter().find(|s| s.id == sid))
                            .map_or(true, |s| s.preconditions.is_satisfied(&agent, needs.hunger))
                    })
                    // Drop gather plans that have no idea where to go: no memory of
                    // the resource and nothing visible nearby. Otherwise the agent
                    // picks a blind ForageFood/GatherWood/GatherStone, fails to
                    // resolve step 0, and thrashes back through plan selection.
                    // Explore (memory_target_kind: None) is unaffected and wins by
                    // default in this case.
                    .filter(|p| match p.memory_target_kind {
                        Some(MemoryKind::Food) => {
                            vis_food > 0
                                || memory_opt
                                    .and_then(|m| m.best_for(MemoryKind::Food))
                                    .is_some()
                        }
                        Some(MemoryKind::Wood) => {
                            vis_wood > 0
                                || memory_opt
                                    .and_then(|m| m.best_for(MemoryKind::Wood))
                                    .is_some()
                        }
                        Some(MemoryKind::Stone) => {
                            vis_stone > 0
                                || memory_opt
                                    .and_then(|m| m.best_for(MemoryKind::Stone))
                                    .is_some()
                        }
                        _ => true,
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
                        let state = build_state_vec(
                            needs,
                            agent,
                            skills,
                            member,
                            memory_opt,
                            &calendar,
                            plan_history_opt.as_deref(),
                            vis_food,
                            vis_wood,
                            vis_stone,
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
                            } else if plan_def.memory_target_kind.is_some() {
                                // Plan has a target kind but no memory hit. The
                                // candidate filter only lets Food/Wood/Stone gather
                                // plans through here when a visible resource exists,
                                // so this is the visible-but-unmemorised case (or a
                                // Prey/Seed plan running blind). Gentle penalty.
                                *score -= 0.1;
                            }
                            // Plans without a memory_target_kind (Explore, etc.)
                            // get no penalty — they're targetless by design.
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
            let plan_still_valid = plan_registry
                .0
                .iter()
                .find(|p| p.id == active_plan.plan_id)
                .map(|p| p.serves_goals.contains(&goal))
                .unwrap_or(false);
            if !plan_still_valid {
                if let Some(history) = plan_history_opt.as_deref_mut() {
                    history.push(active_plan.plan_id, PlanOutcome::Aborted);
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
                    history.push(active_plan.plan_id, PlanOutcome::Interrupted);
                }
                // ReturnSurplusFood timeout means the agent couldn't reach storage
                // for 5000 ticks. Drop the surplus on the ground so it isn't
                // permanently bottled in inventory and can be picked up by allies.
                if active_plan.plan_id == RETURN_SURPLUS_FOOD_PLAN_ID {
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
                    history.push(active_plan.plan_id, PlanOutcome::Success);
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
                if !step_def.preconditions.is_satisfied(&agent, needs.hunger) {
                    if let Some(history) = plan_history_opt.as_deref_mut() {
                        history.push(active_plan.plan_id, PlanOutcome::FailedPrecondition);
                    }
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<ActivePlan>();
                    });
                    combat_target.0 = None;
                    return;
                }

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
                    goal,
                    memory_opt,
                    &item_check,
                    &prey_query,
                    &wild_horse_q,
                    &mut combat_target,
                    &mut target_item,
                    &bp_map,
                    &bp_query,
                    rescue_target_opt,
                    &agent,
                    &carrier,
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
                    active_plan.dispatched = true;
                    active_plan.reward_scale = step_def.reward_scale;
                } else {
                    // No valid target — record the failure and drop the plan so
                    // the next tick re-enters selection. The NN sees this plan in
                    // recent-failure history and (after training) scores it lower.
                    // Underground recovery is handled inside the Explore plan's
                    // step resolver, which the NN can pick like any other plan.
                    if let Some(history) = plan_history_opt.as_deref_mut() {
                        history.push(active_plan.plan_id, PlanOutcome::FailedNoTarget);
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
