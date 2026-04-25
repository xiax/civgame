use ahash::AHashMap;
use bevy::prelude::*;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::Calendar;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use super::construction::{BedMap, BuildSiteKind, find_wall_build_site, find_bed_build_site};
use super::faction::{FactionMember, FactionRegistry, SOLO};
use super::goals::AgentGoal;
use super::items::{GroundItem, TargetItem};
use super::combat::{CombatTarget, Health};
use super::animals::{Wolf, Deer};
use super::jobs::{JobKind, assign_job_with_routing, find_nearest_tile, find_nearest_item, find_nearest_unplanted_farmland, find_nearest_plant};
use super::lod::LodLevel;
use super::memory::{AgentMemory, MemoryKind};
use super::needs::Needs;
use super::neural::{UtilityNet, STATE_DIM, PLAN_FEAT_DIM};
use super::person::{AiState, PersonAI, PlayerOrder};
use super::plants::{PlantMap, PlantKind, Plant};
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;
use super::technology::TechId;

pub type StepId = u8;
pub type PlanId = u16;

#[derive(Clone, Debug)]
pub enum StepTarget {
    NearestTile(&'static [TileKind]),
    NearestItem(Good),
    FactionCamp,
    FromMemory(MemoryKind),
    HuntPrey,
    NearestBuildSite(BuildSiteKind),
}

#[derive(Clone, Debug)]
pub struct StepPreconditions {
    pub requires_good: Option<(Good, u8)>,
}

impl StepPreconditions {
    pub fn none() -> Self { Self { requires_good: None } }
    pub fn needs_good(good: Good, qty: u8) -> Self { Self { requires_good: Some((good, qty)) } }
}

#[derive(Clone)]
pub struct StepDef {
    pub id:            StepId,
    pub job:           JobKind,
    pub target:        StepTarget,
    pub preconditions: StepPreconditions,
    pub reward_scale:  f32,
    /// When falling back from memory to a plant search (Food memory kind),
    /// restricts which plant kind is targeted. None = any mature plant.
    pub plant_filter:  Option<PlantKind>,
}

#[derive(Clone)]
pub struct PlanDef {
    pub id:           PlanId,
    pub name:         &'static str,
    pub steps:        &'static [StepId],
    pub feature_vec:  [f32; PLAN_FEAT_DIM],
    pub serves_goals: &'static [AgentGoal],
    /// Faction must have unlocked this tech for the plan to be selectable.
    pub tech_gate:    Option<TechId>,
    /// Memory kind used to compute distance penalties during plan scoring.
    pub memory_target_kind: Option<MemoryKind>,
}

#[derive(Resource, Default)]
pub struct StepRegistry(pub Vec<StepDef>);

#[derive(Resource, Default)]
pub struct PlanRegistry(pub Vec<PlanDef>);

#[derive(Component)]
pub struct ActivePlan {
    pub plan_id:      PlanId,
    pub current_step: u8,
    pub started_tick: u64,
    pub max_ticks:    u64,
    pub reward_acc:   f32,
    pub reward_scale: f32,
    pub dispatched:   bool,
}

#[derive(Clone)]
pub struct KnownPlanEntry {
    pub plan_id:  PlanId,
    pub freshness: u8,
    pub innate:   bool,
}

#[derive(Component, Clone)]
pub struct KnownPlans {
    pub entries: Vec<KnownPlanEntry>,
}

impl KnownPlans {
    pub fn with_innate(ids: &[PlanId]) -> Self {
        Self {
            entries: ids.iter().map(|&id| KnownPlanEntry {
                plan_id: id, freshness: 255, innate: true,
            }).collect(),
        }
    }

    pub fn knows(&self, id: PlanId) -> bool {
        self.entries.iter().any(|e| e.plan_id == id)
    }

    pub fn add(&mut self, id: PlanId, freshness: u8) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.plan_id == id) {
            if freshness > e.freshness { e.freshness = freshness; }
        } else {
            self.entries.push(KnownPlanEntry { plan_id: id, freshness, innate: false });
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
        let mut sorted: Vec<(PlanId, u8)> = self.entries.iter()
            .map(|e| (e.plan_id, e.freshness))
            .collect();
        sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(n);
        sorted
    }

    pub fn receive_gossip(&mut self, id: PlanId, freshness: u8) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.plan_id == id) {
            if freshness > e.freshness { e.freshness = freshness; }
        } else {
            self.entries.push(KnownPlanEntry { plan_id: id, freshness, innate: false });
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

static GRASS_TILES:    &[TileKind] = &[TileKind::Grass];
static FARMLAND_TILES: &[TileKind] = &[TileKind::Farmland];
static FOREST_TILES:   &[TileKind] = &[TileKind::Forest];
static STONE_TILES:    &[TileKind] = &[TileKind::Stone];

static PLAN_STEPS_0: &[StepId] = &[0];    // ForageFood
static PLAN_STEPS_1: &[StepId] = &[1];    // FarmFood
static PLAN_STEPS_2: &[StepId] = &[2];    // GatherWood
static PLAN_STEPS_3: &[StepId] = &[3];    // GatherStone
static PLAN_STEPS_4: &[StepId] = &[4, 1]; // PlantAndFarm
static PLAN_STEPS_5: &[StepId] = &[5, 6]; // HuntFood
static PLAN_STEPS_6: &[StepId] = &[6];    // ScavengeFood
static PLAN_STEPS_7: &[StepId] = &[2, 7]; // GatherWood, BuildWoodWall
static PLAN_STEPS_8: &[StepId] = &[2, 8]; // GatherWood, BuildBed

static SURVIVE_GOALS:             &[AgentGoal] = &[AgentGoal::Survive];
static GATHER_GOALS:              &[AgentGoal] = &[AgentGoal::Gather];
static SURVIVE_AND_GATHER_GOALS:  &[AgentGoal] = &[AgentGoal::Survive, AgentGoal::Gather];
static BUILD_GOALS:               &[AgentGoal] = &[AgentGoal::Build];

pub fn register_builtin_steps(registry: &mut StepRegistry) {
    registry.0 = vec![
        StepDef { // 0: ForageGrass — targets FruitBushes, falls back via memory
            id: 0, job: JobKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::FruitBush),
        },
        StepDef { // 1: FarmFarmland — targets Grain, falls back via memory
            id: 1, job: JobKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 1.0,
            plant_filter: Some(PlantKind::Grain),
        },
        StepDef { // 2: ChopForest
            id: 2, job: JobKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Wood),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
        },
        StepDef { // 3: MineStone
            id: 3, job: JobKind::Gather,
            target: StepTarget::FromMemory(MemoryKind::Stone),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.3,
            plant_filter: None,
        },
        StepDef { // 4: PlantSeed (requires Seed in inventory)
            id: 4, job: JobKind::Planter,
            target: StepTarget::NearestTile(GRASS_TILES),
            preconditions: StepPreconditions::needs_good(Good::Seed, 1),
            reward_scale: 0.2,
            plant_filter: None,
        },
        StepDef { // 5: Hunt
            id: 5, job: JobKind::Hunter,
            target: StepTarget::HuntPrey,
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
        },
        StepDef { // 6: CollectFood
            id: 6, job: JobKind::Scavenge,
            target: StepTarget::NearestItem(Good::Food),
            preconditions: StepPreconditions::none(),
            reward_scale: 0.4,
            plant_filter: None,
        },
        StepDef { // 7: BuildWall — go to a wall site and build a wall (costs 2 Wood)
            id: 7, job: JobKind::Construct,
            target: StepTarget::NearestBuildSite(BuildSiteKind::Wall),
            preconditions: StepPreconditions::needs_good(Good::Wood, 2),
            reward_scale: 0.8,
            plant_filter: None,
        },
        StepDef { // 8: BuildBed — place a bed inside an enclosed area (costs 3 Wood)
            id: 8, job: JobKind::ConstructBed,
            target: StepTarget::NearestBuildSite(BuildSiteKind::Bed),
            preconditions: StepPreconditions::needs_good(Good::Wood, 3),
            reward_scale: 1.0,
            plant_filter: None,
        },
    ];
}

pub fn register_builtin_plans(registry: &mut PlanRegistry) {
    // feature_vec: [produces_food, produces_wood, produces_stone,
    //               addresses_hunger, addresses_safety, addresses_social,
    //               step_count_norm, risk]
    registry.0 = vec![
        PlanDef { id: 0, name: "ForageFood",
            steps: PLAN_STEPS_0,
            feature_vec: [1.0, 0.0, 0.0,  1.0, 0.0, 0.0,  0.1, 0.0],
            serves_goals: SURVIVE_AND_GATHER_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef { id: 1, name: "FarmFood",
            steps: PLAN_STEPS_1,
            feature_vec: [1.0, 0.0, 0.0,  1.0, 0.0, 0.0,  0.1, 0.0],
            serves_goals: SURVIVE_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef { id: 2, name: "GatherWood",
            steps: PLAN_STEPS_2,
            feature_vec: [0.0, 1.0, 0.0,  0.0, 0.0, 0.0,  0.1, 0.1],
            serves_goals: GATHER_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Wood),
        },
        PlanDef { id: 3, name: "GatherStone",
            steps: PLAN_STEPS_3,
            feature_vec: [0.0, 0.0, 1.0,  0.0, 0.0, 0.0,  0.1, 0.1],
            serves_goals: GATHER_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Stone),
        },
        PlanDef { id: 4, name: "PlantAndFarm",
            steps: PLAN_STEPS_4,
            feature_vec: [1.0, 0.0, 0.0,  1.0, 0.0, 0.0,  0.2, 0.0],
            serves_goals: SURVIVE_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef { id: 5, name: "HuntFood",
            steps: PLAN_STEPS_5,
            feature_vec: [1.0, 0.0, 0.0,  1.0, 0.0, 0.0,  0.2, 1.0],
            serves_goals: SURVIVE_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Prey),
        },
        PlanDef { id: 6, name: "ScavengeFood",
            steps: PLAN_STEPS_6,
            feature_vec: [1.0, 0.0, 0.0,  1.0, 0.0, 0.0,  0.1, 0.0],
            serves_goals: SURVIVE_GOALS, tech_gate: None,
            memory_target_kind: Some(MemoryKind::Food),
        },
        PlanDef { id: 7, name: "BuildWoodWall",
            steps: PLAN_STEPS_7,
            feature_vec: [0.0, 0.0, 0.0,  0.0, 1.0, 0.0,  0.1, 0.0],
            serves_goals: BUILD_GOALS, tech_gate: None,
            memory_target_kind: None,
        },
        PlanDef { id: 8, name: "BuildBed",
            steps: PLAN_STEPS_8,
            feature_vec: [0.0, 0.0, 0.0,  0.0, 0.0, 0.0,  0.1, 0.0],
            serves_goals: BUILD_GOALS,
            tech_gate: Some(super::technology::PERM_SETTLEMENT),
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
    s[0] = needs.hunger       as f32 / 255.0;
    s[1] = needs.sleep        as f32 / 255.0;
    s[2] = needs.shelter      as f32 / 255.0;
    s[3] = needs.safety       as f32 / 255.0;
    s[4] = needs.social       as f32 / 255.0;
    s[5] = needs.reproduction as f32 / 255.0;

    // 6-10: inventory has (Food, Wood, Stone, Seed, Coal)
    s[6]  = if agent.quantity_of(Good::Food)  > 0 { 1.0 } else { 0.0 };
    s[7]  = if agent.quantity_of(Good::Wood)  > 0 { 1.0 } else { 0.0 };
    s[8]  = if agent.quantity_of(Good::Stone) > 0 { 1.0 } else { 0.0 };
    s[9]  = if agent.quantity_of(Good::Seed)  > 0 { 1.0 } else { 0.0 };
    s[10] = if agent.quantity_of(Good::Coal)  > 0 { 1.0 } else { 0.0 };

    // 11-18: all 8 skills
    for k in 0..8usize {
        s[11 + k] = skills.0[k] as f32 / 255.0;
    }

    // 19: season multiplier
    s[19] = (calendar.food_yield_multiplier() / 1.3).clamp(0.0, 1.0);

    // 20: in faction
    s[20] = if member.faction_id != SOLO { 1.0 } else { 0.0 };

    // 21-23: memory availability
    if let Some(mem) = memory {
        s[21] = if mem.best_for(MemoryKind::Food).is_some()  { 1.0 } else { 0.0 };
        s[22] = if mem.best_for(MemoryKind::Wood).is_some()  { 1.0 } else { 0.0 };
        s[23] = if mem.best_for(MemoryKind::Stone).is_some() { 1.0 } else { 0.0 };
    }

    s
}

// ── Target resolution ─────────────────────────────────────────────────────────

fn resolve_target(
    step: &StepDef,
    pos: (i32, i32),
    chunk_map: &ChunkMap,
    spatial: &SpatialIndex,
    plant_map: &PlantMap,
    plant_query: &Query<&Plant>,
    faction_registry: &FactionRegistry,
    faction_id: u32,
    memory: Option<&AgentMemory>,
    // Bug 2 fix: read GroundItem data so find_nearest_item can filter by good type.
    item_query: &Query<&GroundItem>,
    prey_query: &Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    combat_target: &mut CombatTarget,
    target_item: &mut TargetItem,
    bed_map: &BedMap,
) -> Option<(Option<Entity>, i16, i16)> {
    const VIEW_RADIUS: i32 = 15;

    match &step.target {
        StepTarget::HuntPrey => {
            // 1. Check vision
            let mut best_v: Option<(Entity, i16, i16)> = None;
            let mut best_dist_v = i32::MAX;
            for dy in -VIEW_RADIUS..=VIEW_RADIUS {
                for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                    if dx*dx + dy*dy > VIEW_RADIUS*VIEW_RADIUS { continue; }
                    let tx = pos.0 + dx;
                    let ty = pos.1 + dy;
                    if !super::line_of_sight::has_los(chunk_map, pos, (tx, ty)) { continue; }
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
                if let Some((entity, tx, ty)) = mem.best_entity_for_dist_weighted(MemoryKind::Prey, pos) {
                    combat_target.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::FromMemory(kind) => {
            // 1. Check vision
            let vision_target: Option<(Option<Entity>, i16, i16)> = match kind {
                MemoryKind::Food => find_nearest_plant(plant_map, pos, VIEW_RADIUS, plant_query, true, step.plant_filter).map(|(e, tx, ty)| (Some(e), tx, ty)),
                MemoryKind::Wood => find_nearest_plant(plant_map, pos, VIEW_RADIUS, plant_query, true, Some(PlantKind::Tree)).map(|(e, tx, ty)| (Some(e), tx, ty)),
                MemoryKind::Stone => find_nearest_tile(chunk_map, pos, VIEW_RADIUS, STONE_TILES).map(|(tx, ty)| (None, tx, ty)),
                _ => None,
            };
            if let Some((ent, tx, ty)) = vision_target {
                if super::line_of_sight::has_los(chunk_map, pos, (tx as i32, ty as i32)) {
                    return Some((ent, tx, ty));
                }
            }

            // 2. Check memory
            if let Some(mem) = memory {
                if let Some((ent, tx, ty)) = mem.best_entity_for_dist_weighted(*kind, pos) {
                    return Some((Some(ent), tx, ty));
                }
                if let Some((tx, ty)) = mem.best_for_dist_weighted(*kind, pos) {
                    return Some((None, tx, ty));
                }
            }

            None
        }
        StepTarget::NearestTile(_tiles) => {
            if step.job == JobKind::Planter {
                find_nearest_unplanted_farmland(chunk_map, plant_map, pos, VIEW_RADIUS).map(|(tx, ty)| (None, tx, ty))
            } else {
                find_nearest_tile(chunk_map, pos, VIEW_RADIUS, _tiles).map(|(tx, ty)| (None, tx, ty))
            }
        }
        StepTarget::NearestItem(good) => {
            // 1. Check vision — Bug 2 fix: pass good so only matching items are targeted.
            if let Some((entity, tx, ty)) = find_nearest_item(spatial, pos, VIEW_RADIUS, *good, item_query) {
                if super::line_of_sight::has_los(chunk_map, pos, (tx as i32, ty as i32)) {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            // 2. Check memory
            if let Some(mem) = memory {
                let mkind = match good {
                    Good::Food => MemoryKind::Food,
                    Good::Wood => MemoryKind::Wood,
                    Good::Stone => MemoryKind::Stone,
                    Good::Seed => MemoryKind::Seed,
                    _ => return None,
                };
                if let Some((entity, tx, ty)) = mem.best_entity_for_dist_weighted(mkind, pos) {
                    target_item.0 = Some(entity);
                    return Some((Some(entity), tx, ty));
                }
            }
            None
        }
        StepTarget::FactionCamp => {
            faction_registry.home_tile(faction_id).map(|(tx, ty)| (None, tx, ty))
        }
        StepTarget::NearestBuildSite(kind) => {
            let Some(home) = faction_registry.home_tile(faction_id) else { return None };
            match kind {
                BuildSiteKind::Wall => find_wall_build_site(chunk_map, home, 20).map(|(tx, ty)| (None, tx, ty)),
                BuildSiteKind::Bed  => find_bed_build_site(chunk_map, bed_map, home, 15).map(|(tx, ty)| (None, tx, ty)),
            }
        }
    }
}

fn chunk_coord(tx: i32, ty: i32) -> ChunkCoord {
    ChunkCoord(
        tx.div_euclid(CHUNK_SIZE as i32),
        ty.div_euclid(CHUNK_SIZE as i32),
    )
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
);

type OptionalQuery<'a> = (
    Option<&'a AgentMemory>,
    Option<&'a mut UtilityNet>,
    Option<&'a KnownPlans>,
    Option<&'a PlanScoringMethod>,
    Option<&'a mut ActivePlan>,
);

pub fn plan_execution_system(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial: Res<SpatialIndex>,
    plant_map: Res<PlantMap>,
    plant_query: Query<&Plant>,
    plan_registry: Res<PlanRegistry>,
    step_registry: Res<StepRegistry>,
    faction_registry: Res<FactionRegistry>,
    bed_map: Res<BedMap>,
    calendar: Res<Calendar>,
    clock: Res<SimClock>,
    // Bug 2 fix: read GroundItem data to allow good-type filtering in find_nearest_item.
    item_check: Query<&GroundItem>,
    prey_query: Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    mut query: Query<(AgentQuery, OptionalQuery), Without<PlayerOrder>>,
) {
    for (
        (
            entity, mut ai, agent, member, goal, needs, skills,
            transform, lod, slot, mut combat_target, mut target_item,
        ),
        (
            memory_opt, mut net_opt, known_plans_opt, scoring_opt, mut active_plan_opt,
        )
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }

        // Only handle plan-driven goals
        if !matches!(goal, AgentGoal::Survive | AgentGoal::Gather | AgentGoal::Build) { continue; }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = chunk_coord(cur_tx, cur_ty);

        if active_plan_opt.is_none() {
            // ── Select and start a new plan ───────────────────────────────────
            if ai.state != AiState::Idle || ai.job_id != PersonAI::UNEMPLOYED { continue; }

            let Some(known_plans) = known_plans_opt else { continue };
            let Some(scoring) = scoring_opt else { continue };
            if plan_registry.0.is_empty() { continue; }

            let candidates: Vec<&PlanDef> = plan_registry.0.iter()
                .filter(|p| p.serves_goals.contains(&goal) && known_plans.knows(p.id))
                .filter(|p| p.tech_gate.map_or(true, |tid| {
                    faction_registry.factions.get(&member.faction_id)
                        .map(|f| f.techs.has(tid))
                        .unwrap_or(false)
                }))
                // Bug 3 fix: skip plans whose first step has unmet preconditions so we
                // don't enter a tight pick-then-immediately-abandon loop.
                .filter(|p| {
                    p.steps.first()
                        .and_then(|&sid| step_registry.0.iter().find(|s| s.id == sid))
                        .and_then(|s| s.preconditions.requires_good)
                        .map_or(true, |(good, qty)| agent.quantity_of(good) >= qty)
                })
                .collect();

            if candidates.is_empty() {
                // FALLBACK: Explore toward a random tile within 3 chunks of home
                let home = faction_registry.home_tile(member.faction_id).unwrap_or((cur_tx as i16, cur_ty as i16));
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let target_tx = (home.0 as i32 + dx).max(0) as i16;
                let target_ty = (home.1 as i32 + dy).max(0) as i16;

                assign_job_with_routing(&mut ai, cur_chunk, (target_tx, target_ty), JobKind::Explore, None, &chunk_graph, &chunk_map);
                continue;
            }

            let plan_def = match scoring {
                PlanScoringMethod::UtilityNN => {
                    if let Some(ref mut net) = net_opt {
                        let state = build_state_vec(needs, agent, skills, member, memory_opt, &calendar);
                        let mut scores: Vec<(u16, f32)> = candidates.iter()
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

                            let target_tile = plan_def.memory_target_kind.and_then(|k| {
                                memory_opt.and_then(|m| m.best_for_dist_weighted(k, (cur_tx, cur_ty)))
                            });

                            if let Some(target) = target_tile {
                                let dist_agent = (target.0 as i32 - cur_tx).abs() + (target.1 as i32 - cur_ty).abs();
                                let dist_camp = camp_pos.map_or(0, |c| (target.0 as i32 - c.0 as i32).abs() + (target.1 as i32 - c.1 as i32).abs());

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
                PlanScoringMethod::Random => {
                    candidates[fastrand::usize(..candidates.len())]
                }
            };

            commands.entity(entity).insert(ActivePlan {
                plan_id:      plan_def.id,
                current_step: 0,
                started_tick: clock.tick,
                max_ticks:    5000,
                reward_acc:   0.0,
                reward_scale: 0.0,
                dispatched:   false,
            });
            continue;
        }

        let active_plan = active_plan_opt.as_deref_mut().unwrap();

        // ── Abandon if goal changed (no longer served by this plan) ──────────
        let plan_still_valid = plan_registry.0.iter()
            .find(|p| p.id == active_plan.plan_id)
            .map(|p| p.serves_goals.contains(&goal))
            .unwrap_or(false);
        if !plan_still_valid {
            commands.entity(entity).remove::<ActivePlan>();
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            combat_target.0 = None;
            continue;
        }

        // ── Timeout check ─────────────────────────────────────────────────────
        if clock.tick.saturating_sub(active_plan.started_tick) > active_plan.max_ticks {
            if let Some(ref mut net) = net_opt {
                net.learn(-0.1);
            }
            commands.entity(entity).remove::<ActivePlan>();
            ai.state = AiState::Idle;
            ai.job_id = PersonAI::UNEMPLOYED;
            combat_target.0 = None;
            continue;
        }

        // ── Fetch plan and current step ───────────────────────────────────────
        let plan_def = match plan_registry.0.iter().find(|p| p.id == active_plan.plan_id) {
            Some(p) => p, None => { commands.entity(entity).remove::<ActivePlan>(); combat_target.0 = None; continue; }
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

        let step_id = plan_def.steps[active_plan.current_step as usize];
        let step_def = match step_registry.0.iter().find(|s| s.id == step_id) {
            Some(s) => s, None => { commands.entity(entity).remove::<ActivePlan>(); combat_target.0 = None; continue; }
        };

        // ── Step completion: dispatched + agent went back to Idle+UNEMPLOYED ──
        if active_plan.dispatched && ai.state == AiState::Idle && ai.job_id == PersonAI::UNEMPLOYED {
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
            }
            continue;
        }

        // ── Dispatch current step if not yet dispatched ───────────────────────
        if !active_plan.dispatched {
            if ai.state != AiState::Idle || ai.job_id != PersonAI::UNEMPLOYED { continue; }

            // Check preconditions
            if let Some((good, qty)) = step_def.preconditions.requires_good {
                if agent.quantity_of(good) < qty {
                    commands.entity(entity).remove::<ActivePlan>();
                    combat_target.0 = None;
                    continue;
                }
            }

            if let Some((ent, target_tx, target_ty)) = resolve_target(
                step_def, (cur_tx, cur_ty),
                &chunk_map, &spatial, &plant_map, &plant_query,
                &faction_registry, member.faction_id,
                memory_opt, &item_check,
                &prey_query, &mut combat_target,
                &mut target_item, &bed_map,
            ) {
                assign_job_with_routing(&mut ai, cur_chunk, (target_tx, target_ty), step_def.job, ent, &chunk_graph, &chunk_map);
                active_plan.dispatched = true;
                active_plan.reward_scale = step_def.reward_scale;
            } else {
                // No valid target — explore instead of just failing
                let home = faction_registry.home_tile(member.faction_id).unwrap_or((cur_tx as i16, cur_ty as i16));
                let dx = fastrand::i32(-96..=96);
                let dy = fastrand::i32(-96..=96);
                let target_tx = (home.0 as i32 + dx).max(0) as i16;
                let target_ty = (home.1 as i32 + dy).max(0) as i16;

                assign_job_with_routing(&mut ai, cur_chunk, (target_tx, target_ty), JobKind::Explore, None, &chunk_graph, &chunk_map);
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
                                assign_job_with_routing(&mut ai, cur_chunk, (ptx, pty), step_def.job, Some(target_ent), &chunk_graph, &chunk_map);
                            }
                        }
                    } else {
                        // Target lost
                        ai.state = AiState::Idle;
                        ai.job_id = PersonAI::UNEMPLOYED;
                        ai.target_entity = None;
                        combat_target.0 = None;
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
    let snapshots: AHashMap<Entity, Vec<(PlanId, u8)>> = query.iter()
        .filter(|(_, goal, _, _, lod)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .map(|(e, _, _, plans, _)| (e, plans.top_entries(8)))
        .collect();

    if snapshots.is_empty() { return; }

    // Pass 2: apply gossip to Socialize agents within 3 tiles
    for (entity, goal, transform, mut known_plans, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !matches!(goal, AgentGoal::Socialize) { continue; }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity { continue; }
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

pub fn plan_decay_system(
    clock: Res<SimClock>,
    mut query: Query<&mut KnownPlans>,
) {
    if clock.tick % 120 != 0 { return; }
    for mut plans in &mut query {
        plans.decay();
    }
}
