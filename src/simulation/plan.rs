use super::animals::{Deer, Horse, Tamed, Wolf};
use super::combat::{CombatTarget, Health};
use super::construction::{Blueprint, BlueprintMap, BuildSiteKind};
use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::goals::{AgentGoal, RescueTarget};
use super::items::{GroundItem, TargetItem};
use super::tasks::{
    assign_task_with_routing, find_nearest_edible, find_nearest_item, find_nearest_plant,
    find_nearest_tile, find_nearest_unplanted_farmland, TaskKind,
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
use super::neural::{UtilityNet, PLAN_FEAT_DIM, STATE_DIM};
use super::person::{AiState, Person, PersonAI, PlayerOrder};
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use super::reproduction::BiologicalSex;
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
    /// In-place: target resolves to the agent's current tile.
    SelfPosition,
    /// Nearest faction storage tile (for withdrawing food from communal stock).
    NearestFactionStorage,
    /// Nearest wild (untamed) horse entity.
    NearestWildHorse,
    /// Nearest opposite-sex same-faction person within view radius. Affinity-weighted via RelationshipMemory.
    NearestPartner,
    /// Resolves to the attacker carried by the agent's `RescueTarget` component
    /// (set by `sound::respond_to_distress_system`). Routes the responder to the
    /// attacker's current tile and assigns the attacker as `target_entity`.
    RescueAttacker,
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
    pub feature_vec: [f32; PLAN_FEAT_DIM],
    pub serves_goals: &'static [AgentGoal],
    /// Faction must have unlocked this tech for the plan to be selectable.
    pub tech_gate: Option<TechId>,
    /// Memory kind used to compute distance penalties during plan scoring.
    pub memory_target_kind: Option<MemoryKind>,
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
    UtilityNN,
    Random,
    // Level 3 variant: ModelBased { rollout_depth: u8 },
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
static PLAN_STEPS_7: &[StepId] = &[2, 25]; // GatherWood, BuildAnyBlueprint
static PLAN_STEPS_9: &[StepId] = &[10, 9]; // WithdrawAndEat: WithdrawFood → Eat
static PLAN_STEPS_10: &[StepId] = &[11]; // TameHorse: TameAnimal

static SURVIVE_GOALS: &[AgentGoal] = &[AgentGoal::Survive];
static GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherFood];
static TAME_HORSE_GOALS: &[AgentGoal] = &[AgentGoal::TameHorse];
static GATHER_WOOD_GOALS: &[AgentGoal] = &[AgentGoal::GatherWood];
static GATHER_STONE_GOALS: &[AgentGoal] = &[AgentGoal::GatherStone];
static SURVIVE_AND_GATHER_FOOD_GOALS: &[AgentGoal] = &[AgentGoal::Survive, AgentGoal::GatherFood];
static BUILD_GOALS: &[AgentGoal] = &[AgentGoal::Build];
static CRAFT_GOALS: &[AgentGoal] = &[AgentGoal::Craft];
static REPRODUCE_GOALS: &[AgentGoal] = &[AgentGoal::Reproduce];
static RESCUE_GOALS: &[AgentGoal] = &[AgentGoal::Rescue];

static PLAN_STEPS_22: &[StepId] = &[26]; // FindMate: NearestPartner
static PLAN_STEPS_23: &[StepId] = &[27]; // RescueAlly: EngageRescue
static PLAN_STEPS_24: &[StepId] = &[12]; // ReturnSurplusFood: DepositGoods at faction storage
static PLAN_STEPS_25: &[StepId] = &[9]; // EatFromInventory: Eat (gated by hunger + edible-on-hand)

static RETURN_CAMP_GOALS: &[AgentGoal] = &[AgentGoal::ReturnCamp];

// Craft plan step sequences (step IDs match register_builtin_steps above)
// 12=DepositGoods, 13=CollectSkin
// 14=CraftStoneTools, 15=CraftSpear, 16=CraftTorch, 17=CraftBow, 18=CraftCloth
// 19=CraftPottery, 20=CraftShield, 21=CraftLeatherArmor, 22=CraftIronTools, 23=CraftSword
static PLAN_STEPS_11: &[StepId] = &[2, 3, 14, 12]; // MakeStoneTools
static PLAN_STEPS_12: &[StepId] = &[2, 3, 15, 12]; // MakeSpear
static PLAN_STEPS_13: &[StepId] = &[2, 16, 12];    // MakeTorch
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
            preconditions: StepPreconditions::none(),
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
            // 26: FindMate — locate nearest partner; reproduction_system finalizes the birth
            id: 26,
            task: TaskKind::Reproduce,
            target: StepTarget::NearestPartner,
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
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
    ];
}

pub fn register_builtin_plans(registry: &mut PlanRegistry) {
    // feature_vec: [produces_food, produces_wood, produces_stone,
    //               addresses_hunger, addresses_safety, addresses_social,
    //               step_count_norm, risk]
    registry.0 = vec![
        PlanDef {
            id: 0,
            name: "ForageFood",
            steps: PLAN_STEPS_0,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.1, 0.0],
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 1,
            name: "FarmFood",
            steps: PLAN_STEPS_1,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.1, 0.0],
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 2,
            name: "GatherWood",
            steps: PLAN_STEPS_2,
            feature_vec: [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.1],
            serves_goals: GATHER_WOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 3,
            name: "GatherStone",
            steps: PLAN_STEPS_3,
            feature_vec: [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.1, 0.1],
            serves_goals: GATHER_STONE_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 4,
            name: "PlantAndFarm",
            steps: PLAN_STEPS_4,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.2, 0.0],
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 5,
            name: "HuntFood",
            steps: PLAN_STEPS_5,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.2, 1.0],
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 6,
            name: "ScavengeFood",
            steps: PLAN_STEPS_6,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.1, 0.0],
            serves_goals: SURVIVE_AND_GATHER_FOOD_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 7,
            name: "BuildBlueprint",
            steps: PLAN_STEPS_7,
            feature_vec: [0.0, 0.1, 0.0, 0.0, 0.3, 0.2, 0.1, 0.0],
            serves_goals: BUILD_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 8,
            name: "BuildBed",
            steps: &[],
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 9,
            name: "WithdrawAndEat",
            steps: PLAN_STEPS_9,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.2, 0.0],
            serves_goals: SURVIVE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 10,
            name: "TameHorse",
            steps: PLAN_STEPS_10,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.1],
            serves_goals: TAME_HORSE_GOALS,
            tech_gate: Some(super::technology::HORSE_TAMING),
            memory_target_kind: None,
        },
        // ── Crafting plans ────────────────────────────────────────────────────
        PlanDef {
            id: 11,
            name: "MakeStoneTools",
            steps: PLAN_STEPS_11,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.3, 0.0],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FLINT_KNAPPING),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 12,
            name: "MakeSpear",
            steps: PLAN_STEPS_12,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.8, 0.0, 0.3, 0.1],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::HUNTING_SPEAR),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 13,
            name: "MakeTorch",
            steps: PLAN_STEPS_13,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.2, 0.0],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FIRE_MAKING),
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 14,
            name: "MakeBow",
            steps: PLAN_STEPS_14,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.4, 0.2],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::BOW_AND_ARROW),
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 15,
            name: "MakeCloth",
            steps: PLAN_STEPS_15,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.4, 0.0, 0.3, 0.0],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::LOOM_WEAVING),
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef {
            id: 16,
            name: "MakePottery",
            steps: PLAN_STEPS_16,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.3, 0.0],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::FIRED_POTTERY),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 17,
            name: "MakeShield",
            steps: PLAN_STEPS_17,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.7, 0.0, 0.3, 0.0],
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef {
            id: 18,
            name: "MakeLeatherArmor",
            steps: PLAN_STEPS_18,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.8, 0.0, 0.5, 0.3],
            serves_goals: CRAFT_GOALS,
            tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef {
            id: 19,
            name: "MakeIronTools",
            steps: PLAN_STEPS_19,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.4, 0.1],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::COPPER_TOOLS),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 20,
            name: "MakeSword",
            steps: PLAN_STEPS_20,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.4, 0.2],
            serves_goals: CRAFT_GOALS,
            tech_gate: Some(super::technology::BRONZE_WEAPONS),
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef {
            id: 21,
            name: "BuildCampfire",
            steps: &[],
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            serves_goals: &[],
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 22,
            name: "FindMate",
            steps: PLAN_STEPS_22,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.05, 0.0],
            serves_goals: REPRODUCE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 23,
            name: "RescueAlly",
            steps: PLAN_STEPS_23,
            // safety-leaning: high addresses_safety, mild social signal
            feature_vec: [0.0, 0.0, 0.0, 0.0, 1.0, 0.2, 0.05, 0.5],
            serves_goals: RESCUE_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            id: 24,
            name: "ReturnSurplusFood",
            steps: PLAN_STEPS_24,
            feature_vec: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.05, 0.0],
            serves_goals: RETURN_CAMP_GOALS,
            tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef {
            // Eat what's already in the agent's inventory. The Eat step's
            // preconditions gate this on hunger + having edibles, so it only
            // becomes a candidate when the agent should actually eat.
            id: 25,
            name: "EatFromInventory",
            steps: PLAN_STEPS_25,
            feature_vec: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.05, 0.0],
            serves_goals: SURVIVE_GOALS,
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

    s
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
    partner_query: &Query<(&Transform, &BiologicalSex, &FactionMember), With<Person>>,
    my_sex: BiologicalSex,
    relationships: Option<&RelationshipMemory>,
    rescue_target: Option<&RescueTarget>,
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
                            if matches!(plant_query.get(tile_ent), Ok(p) if p.stage == GrowthStage::Mature) {
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
            if let Some((entity, tx, ty)) =
                find_nearest_item(spatial, pos, VIEW_RADIUS, *good, item_query, storage_tile_map, is_gathering)
            {
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
            if let Some((entity, tx, ty)) =
                find_nearest_edible(spatial, pos, VIEW_RADIUS, item_query, storage_tile_map, is_gathering)
            {
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
                if let Some((entity, tx, ty)) = mem.best_entity_for_dist_weighted_filtered(
                    MemoryKind::Food,
                    pos,
                    |e| item_query.get(e).is_ok(),
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
        StepTarget::SelfPosition => Some((None, pos.0 as i16, pos.1 as i16)),
        StepTarget::NearestFactionStorage => storage_tile_map
            .nearest_for_faction(faction_id, pos)
            .map(|(tx, ty)| (None, tx, ty)),
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
                                best = Some((
                                    candidate,
                                    (pos.0 + dx) as i16,
                                    (pos.1 + dy) as i16,
                                ));
                            }
                        }
                    }
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
        }
        StepTarget::NearestPartner => {
            if faction_id == SOLO {
                return None;
            }
            let mut best: Option<(Entity, i16, i16)> = None;
            let mut best_affinity = i8::MIN;
            for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    for &candidate in spatial.get(tx, ty) {
                        if candidate == agent_entity {
                            continue;
                        }
                        let Ok((_t, other_sex, other_fm)) = partner_query.get(candidate) else {
                            continue;
                        };
                        if *other_sex == my_sex || other_fm.faction_id != faction_id {
                            continue;
                        }
                        let affinity = relationships
                            .map(|r| r.get_affinity(candidate))
                            .unwrap_or(0);
                        if best.is_none() || affinity > best_affinity {
                            best_affinity = affinity;
                            best = Some((candidate, tx as i16, ty as i16));
                        }
                    }
                }
            }
            // Females are the rendezvous anchor (reproduction_system scans the 3x3
            // around females for males); route her to her own tile so males come
            // to her, preventing mutual chase between two reproducing agents.
            best.map(|(e, tx, ty)| {
                if my_sex == BiologicalSex::Female {
                    (Some(e), pos.0 as i16, pos.1 as i16)
                } else {
                    (Some(e), tx, ty)
                }
            })
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
            // Find the nearest active Blueprint of any kind this agent is allowed to build.
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
                let dist = (tile.0 as i32 - pos.0).abs() + (tile.1 as i32 - pos.1).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((bp_entity, tile.0, tile.1));
                }
            }
            best.map(|(e, tx, ty)| (Some(e), tx, ty))
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
pub struct PlanRegistries<'w, 's> {
    pub plan_registry: Res<'w, PlanRegistry>,
    pub step_registry: Res<'w, StepRegistry>,
    pub faction_registry: Res<'w, FactionRegistry>,
    pub storage_tile_map: Res<'w, StorageTileMap>,
    pub door_map: Res<'w, crate::simulation::construction::DoorMap>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub partner_query: Query<'w, 's, (&'static Transform, &'static BiologicalSex, &'static FactionMember), With<Person>>,
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
    &'a BiologicalSex,
);

type OptionalQuery<'a> = (
    Option<&'a AgentMemory>,
    Option<&'a mut UtilityNet>,
    Option<&'a KnownPlans>,
    Option<&'a PlanScoringMethod>,
    Option<&'a mut ActivePlan>,
    Option<&'a RelationshipMemory>,
    Option<&'a RescueTarget>,
);

pub fn plan_execution_system(
    mut commands: Commands,
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
    mut query: Query<(AgentQuery, OptionalQuery), Without<PlayerOrder>>,
) {
    let PlanRegistries {
        plan_registry,
        step_registry,
        faction_registry,
        storage_tile_map,
        door_map,
        chunk_router,
        chunk_connectivity,
        partner_query,
        mut drop_food_events,
    } = registries;
    for (
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
            my_sex,
        ),
        (memory_opt, mut net_opt, known_plans_opt, scoring_opt, mut active_plan_opt, rel_opt, rescue_target_opt),
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
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
                | AgentGoal::Reproduce
                | AgentGoal::Rescue
                | AgentGoal::ReturnCamp
        ) {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = chunk_coord(cur_tx, cur_ty);

        if active_plan_opt.is_none() {
            // ── Select and start a new plan ───────────────────────────────────
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                continue;
            }

            let Some(known_plans) = known_plans_opt else {
                continue;
            };
            let Some(scoring) = scoring_opt else { continue };
            if plan_registry.0.is_empty() {
                continue;
            }

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
                .collect();

            if candidates.is_empty() {
                // FALLBACK: Explore toward a random tile within 3 chunks of home
                let home = faction_registry
                    .home_tile(member.faction_id)
                    .unwrap_or((cur_tx as i16, cur_ty as i16));
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let target_tx = (home.0 as i32 + dx).max(0) as i16;
                let target_ty = (home.1 as i32 + dy).max(0) as i16;

                assign_task_with_routing(
                    &mut ai,
                    (cur_tx as i16, cur_ty as i16),
                    cur_chunk,
                    (target_tx, target_ty),
                    TaskKind::Explore,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                continue;
            }

            let plan_def = match scoring {
                PlanScoringMethod::UtilityNN => {
                    if let Some(ref mut net) = net_opt {
                        let state =
                            build_state_vec(needs, agent, skills, member, memory_opt, &calendar);
                        let mut scores: Vec<(u16, f32)> = candidates
                            .iter()
                            .map(|p| (p.id, net.score_plan(state, p.feature_vec, p.id)))
                            .collect();

                        // Bug 5 fix: use PlanDef.memory_target_kind instead of a hard-coded
                        // plan-ID match, so distance penalties stay correct if plans change.
                        let camp_pos = faction_registry.home_tile(member.faction_id);
                        for ((_, score), plan_def) in scores.iter_mut().zip(candidates.iter()) {
                            // Bonus for last plan to reduce switching jitter
                            if plan_def.id == ai.last_plan_id {
                                *score += 0.2;
                            }

                            // Bonus when a liked ally is already running this plan
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

                                // Penalty: -0.002 per tile of total distance
                                *score -= (dist_agent + dist_camp) as f32 * 0.002;
                            } else {
                                // No known target for this plan — heavy penalty to favor plans with known targets
                                *score -= 0.5;
                            }
                        }

                        let idx = UtilityNet::select_plan_idx(&scores);
                        let selected = candidates[idx];
                        ai.last_plan_id = selected.id;
                        // Re-score selected plan to save correct activations for learning (using unpenalized score for NN internals)
                        net.score_plan(state, selected.feature_vec, selected.id);
                        selected
                    } else {
                        candidates[fastrand::usize(..candidates.len())]
                    }
                }
                PlanScoringMethod::Random => candidates[fastrand::usize(..candidates.len())],
            };

            commands.entity(entity).insert(ActivePlan {
                plan_id: plan_def.id,
                current_step: 0,
                started_tick: clock.tick,
                max_ticks: 5000,
                reward_acc: 0.0,
                reward_scale: 0.0,
                dispatched: false,
            });
            continue;
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
            commands.entity(entity).remove::<ActivePlan>();
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_tile = (cur_tx as i16, cur_ty as i16);
            ai.dest_tile = ai.target_tile;
            ai.target_entity = None;
            combat_target.0 = None;
            continue;
        }

        // ── Timeout check ─────────────────────────────────────────────────────
        if clock.tick.saturating_sub(active_plan.started_tick) > active_plan.max_ticks {
            if let Some(ref mut net) = net_opt {
                net.learn(-0.1);
            }
            // ReturnSurplusFood timeout means the agent couldn't reach storage
            // for 5000 ticks. Drop the surplus on the ground so it isn't
            // permanently bottled in inventory and can be picked up by allies.
            if active_plan.plan_id == RETURN_SURPLUS_FOOD_PLAN_ID {
                drop_food_events.send(DropAbandonedFoodEvent(entity));
            }
            commands.entity(entity).remove::<ActivePlan>();
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_tile = (cur_tx as i16, cur_ty as i16);
            ai.dest_tile = ai.target_tile;
            ai.target_entity = None;
            combat_target.0 = None;
            continue;
        }

        // ── Fetch plan and current step ───────────────────────────────────────
        let plan_def = match plan_registry.0.iter().find(|p| p.id == active_plan.plan_id) {
            Some(p) => p,
            None => {
                commands.entity(entity).remove::<ActivePlan>();
                combat_target.0 = None;
                continue;
            }
        };

        if active_plan.current_step as usize >= plan_def.steps.len() {
            if let Some(ref mut net) = net_opt {
                let ticks = clock.tick.saturating_sub(active_plan.started_tick);
                let time_penalty = (ticks as f32 * 0.0005).min(0.8);
                let decayed_reward = active_plan.reward_acc * (1.0 - time_penalty);
                net.learn(decayed_reward);
            }
            commands.entity(entity).remove::<ActivePlan>();
            combat_target.0 = None;
            continue;
        }

        // ── Step completion: advance step when agent returned Idle+UNEMPLOYED ──
        // Intentionally falls through (no `continue`) so the next step is dispatched
        // in the same tick, eliminating the 1-tick UNEMPLOYED gap that lets
        // goal_update_system flip the goal between Gather and Eat.
        if active_plan.dispatched && ai.state == AiState::Idle && ai.task_id == PersonAI::UNEMPLOYED
        {
            active_plan.current_step += 1;
            active_plan.dispatched = false;

            if active_plan.current_step as usize >= plan_def.steps.len() {
                if let Some(ref mut net) = net_opt {
                    let ticks = clock.tick.saturating_sub(active_plan.started_tick);
                    let time_penalty = (ticks as f32 * 0.0005).min(0.8);
                    let decayed_reward = active_plan.reward_acc * (1.0 - time_penalty);
                    net.learn(decayed_reward);
                }
                commands.entity(entity).remove::<ActivePlan>();
                combat_target.0 = None;
                continue;
            }
            // Plan has more steps — fall through to dispatch the next one immediately.
        }

        // Fetch step for current_step (may have just been advanced above).
        let step_id = plan_def.steps[active_plan.current_step as usize];
        let step_def = match step_registry.0.iter().find(|s| s.id == step_id) {
            Some(s) => s,
            None => {
                commands.entity(entity).remove::<ActivePlan>();
                combat_target.0 = None;
                continue;
            }
        };

        // ── Dispatch current step if not yet dispatched ───────────────────────
        if !active_plan.dispatched {
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                continue;
            }

            // Check preconditions
            if !step_def.preconditions.is_satisfied(&agent, needs.hunger) {
                commands.entity(entity).remove::<ActivePlan>();
                combat_target.0 = None;
                continue;
            }

            if let Some((ent, target_tx, target_ty)) = resolve_target(
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
                &partner_query,
                *my_sex,
                rel_opt,
                rescue_target_opt,
            ) {
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
                // For Craft tasks, encode the recipe ID in target_z (unused for in-place crafting).
                if step_def.task == TaskKind::Craft {
                    ai.target_z = step_def.extra as i8;
                }
                active_plan.dispatched = true;
                active_plan.reward_scale = step_def.reward_scale;
            } else {
                // No valid target — explore instead of just failing
                let home = faction_registry
                    .home_tile(member.faction_id)
                    .unwrap_or((cur_tx as i16, cur_ty as i16));
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let target_tx = (home.0 as i32 + dx).max(0) as i16;
                let target_ty = (home.1 as i32 + dy).max(0) as i16;

                assign_task_with_routing(
                    &mut ai,
                    (cur_tx as i16, cur_ty as i16),
                    cur_chunk,
                    (target_tx, target_ty),
                    TaskKind::Explore,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                commands.entity(entity).remove::<ActivePlan>();
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
            } else if matches!(step_def.target, StepTarget::NearestPartner) {
                // Mirror HuntPrey: re-route to partner's current tile, drop task if invalid.
                // Exception: females stay put on their own tile so males converge on
                // them — without this, both partners chase each other and oscillate.
                match ai.target_entity {
                    Some(target_ent) => match partner_query.get(target_ent) {
                        Ok((target_t, other_sex, other_fm)) => {
                            if *other_sex == *my_sex || other_fm.faction_id != member.faction_id {
                                // Partner no longer eligible (sex/faction mismatch); drop task.
                                ai.state = AiState::Idle;
                                ai.task_id = PersonAI::UNEMPLOYED;
                                ai.target_entity = None;
                            } else if *my_sex == BiologicalSex::Female {
                                let here = (cur_tx as i16, cur_ty as i16);
                                if ai.dest_tile != here {
                                    assign_task_with_routing(
                                        &mut ai,
                                        here,
                                        cur_chunk,
                                        here,
                                        step_def.task,
                                        Some(target_ent),
                                        &chunk_graph,
                                        &chunk_router,
                                        &chunk_map,
                                        &chunk_connectivity,
                                    );
                                }
                            } else {
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
                        }
                        Err(_) => {
                            // Partner despawned; drop task so the step re-acquires next tick.
                            ai.state = AiState::Idle;
                            ai.task_id = PersonAI::UNEMPLOYED;
                            ai.target_entity = None;
                        }
                    },
                    None => {
                        // No target on record; drop task so the step re-acquires.
                        ai.state = AiState::Idle;
                        ai.task_id = PersonAI::UNEMPLOYED;
                    }
                }
            }
        }
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
