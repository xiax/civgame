use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::{Item, ItemMaterial, ItemQuality};
use crate::simulation::construction::{GoodNeed, LoomMap, WorkbenchMap, MAX_BUILD_INPUTS};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::jobs::{
    record_progress, JobBoard, JobClaim, JobCompletedEvent, JobKind, JobProgress,
};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::TechId;
use crate::world::terrain::tile_to_world;
use ahash::AHashMap;
use bevy::prelude::*;

/// A crafting station required for certain recipes. Some recipes (e.g. Bow,
/// Pottery) work in the open and have `requires_station: None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StationKind {
    Workbench,
    Loom,
}

pub struct CraftRecipe {
    pub name: &'static str,
    /// All ingredients that must be consumed. Agent's inventory is checked for each.
    pub inputs: &'static [(Good, u32)],
    pub output_good: Good,
    pub output_qty: u32,
    /// None means the output item has no material tag (e.g. Luxury goods).
    pub output_material: Option<ItemMaterial>,
    pub work_ticks: u8,
    pub crafting_xp: u32,
    /// None means no tech required.
    pub tech_gate: Option<TechId>,
    /// If Some, the agent must be within 1 tile of an entity of this kind to craft.
    pub requires_station: Option<StationKind>,
}

use crate::simulation::technology::{
    BOW_AND_ARROW, BRONZE_WEAPONS, COPPER_TOOLS, CUNEIFORM_WRITING, FIRED_POTTERY, FIRE_MAKING,
    FLINT_KNAPPING, HUNTING_SPEAR, LOOM_WEAVING,
};

pub static CRAFT_RECIPES: &[CraftRecipe] = &[
    // 0
    CraftRecipe {
        name: "Stone Tools",
        inputs: &[(Good::Stone, 2), (Good::Wood, 1)],
        output_good: Good::Tools,
        output_qty: 1,
        output_material: Some(ItemMaterial::Stone),
        work_ticks: 30,
        crafting_xp: 5,
        tech_gate: Some(FLINT_KNAPPING),
        requires_station: Some(StationKind::Workbench),
    },
    // 1
    CraftRecipe {
        name: "Spear",
        inputs: &[(Good::Wood, 2), (Good::Stone, 1)],
        output_good: Good::Weapon,
        output_qty: 1,
        output_material: Some(ItemMaterial::Stone),
        work_ticks: 40,
        crafting_xp: 5,
        tech_gate: Some(HUNTING_SPEAR),
        requires_station: None,
    },
    // 2
    CraftRecipe {
        name: "Torch",
        inputs: &[(Good::Wood, 2)],
        output_good: Good::Luxury,
        output_qty: 2,
        output_material: None,
        work_ticks: 20,
        crafting_xp: 3,
        tech_gate: Some(FIRE_MAKING),
        requires_station: None,
    },
    // 3
    CraftRecipe {
        name: "Bow",
        inputs: &[(Good::Wood, 2), (Good::Skin, 1)],
        output_good: Good::Weapon,
        output_qty: 1,
        output_material: Some(ItemMaterial::Wood),
        work_ticks: 50,
        crafting_xp: 6,
        tech_gate: Some(BOW_AND_ARROW),
        requires_station: None,
    },
    // 4
    CraftRecipe {
        name: "Woven Cloth",
        inputs: &[(Good::Grain, 3)],
        output_good: Good::Cloth,
        output_qty: 1,
        output_material: None,
        work_ticks: 60,
        crafting_xp: 6,
        tech_gate: Some(LOOM_WEAVING),
        requires_station: Some(StationKind::Loom),
    },
    // 5
    CraftRecipe {
        name: "Pottery",
        inputs: &[(Good::Stone, 2), (Good::Wood, 1)],
        output_good: Good::Luxury,
        output_qty: 2,
        output_material: None,
        work_ticks: 60,
        crafting_xp: 5,
        tech_gate: Some(FIRED_POTTERY),
        requires_station: None,
    },
    // 6
    CraftRecipe {
        name: "Wooden Shield",
        inputs: &[(Good::Wood, 3)],
        output_good: Good::Shield,
        output_qty: 1,
        output_material: Some(ItemMaterial::Wood),
        work_ticks: 40,
        crafting_xp: 4,
        tech_gate: None,
        requires_station: None,
    },
    // 7
    CraftRecipe {
        name: "Leather Armor",
        inputs: &[(Good::Skin, 2)],
        output_good: Good::Armor,
        output_qty: 1,
        output_material: Some(ItemMaterial::Leather),
        work_ticks: 50,
        crafting_xp: 6,
        tech_gate: None,
        requires_station: None,
    },
    // 8
    CraftRecipe {
        name: "Iron Tools",
        inputs: &[(Good::Iron, 2), (Good::Coal, 1)],
        output_good: Good::Tools,
        output_qty: 1,
        output_material: Some(ItemMaterial::Iron),
        work_ticks: 60,
        crafting_xp: 8,
        tech_gate: Some(COPPER_TOOLS),
        requires_station: Some(StationKind::Workbench),
    },
    // 9
    CraftRecipe {
        name: "Iron Sword",
        inputs: &[(Good::Iron, 2), (Good::Wood, 1)],
        output_good: Good::Weapon,
        output_qty: 1,
        output_material: Some(ItemMaterial::Iron),
        work_ticks: 80,
        crafting_xp: 10,
        tech_gate: Some(BRONZE_WEAPONS),
        requires_station: Some(StationKind::Workbench),
    },
    // 10
    CraftRecipe {
        name: "Clay Tablet",
        inputs: &[(Good::Stone, 1), (Good::Wood, 1)],
        output_good: Good::ClayTablet,
        output_qty: 1,
        output_material: None,
        work_ticks: 90,
        crafting_xp: 8,
        tech_gate: Some(CUNEIFORM_WRITING),
        requires_station: Some(StationKind::Workbench),
    },
    // 11
    CraftRecipe {
        name: "Book",
        inputs: &[(Good::Cloth, 2), (Good::Skin, 1)],
        output_good: Good::Book,
        output_qty: 1,
        output_material: None,
        work_ticks: 180,
        crafting_xp: 12,
        tech_gate: Some(CUNEIFORM_WRITING),
        requires_station: Some(StationKind::Workbench),
    },
];

/// Recipe ids for the two written-knowledge artefacts. Used by chief-posting
/// and player-encode paths to know which crafts need a `tech_payload` set.
pub const RECIPE_CLAY_TABLET: u8 = 10;
pub const RECIPE_BOOK: u8 = 11;
#[inline]
pub fn recipe_encodes_knowledge(recipe_id: u8) -> bool {
    recipe_id == RECIPE_CLAY_TABLET || recipe_id == RECIPE_BOOK
}

/// Maximum distinct ingredient types per craft recipe. Three is plenty for
/// every recipe in `CRAFT_RECIPES`.
pub const MAX_CRAFT_INPUTS: usize = MAX_BUILD_INPUTS;

/// Faction-shared crafting accumulator. Mirrors `Blueprint`: workers haul each
/// ingredient into `deposits` over time; once `is_satisfied()`, an adjacent
/// worker advances `work_progress` until the recipe completes and the order is
/// despawned. Recipes that don't require a station anchor at the faction
/// camp tile.
#[derive(Component)]
pub struct CraftOrder {
    pub faction_id: u32,
    /// `Some` for station-bound recipes; `None` for stationless recipes
    /// (Bow, Pottery, Spear, Torch, Shield, Leather Armor).
    pub workbench_tile: Option<(i32, i32)>,
    /// Tile the agent must work adjacent to. Workbench tile when present,
    /// faction camp tile otherwise.
    pub anchor_tile: (i32, i32),
    pub recipe_id: u8,
    pub deposits: [GoodNeed; MAX_CRAFT_INPUTS],
    pub deposit_count: u8,
    pub work_progress: u8,
    /// `SimClock::tick` at spawn. `faction_craft_order_system` despawns orders
    /// older than `CRAFT_ORDER_TIMEOUT_TICKS` so a stuck order can't waste the
    /// per-faction `CRAFT_ORDERS_PER_FACTION_*` cap forever.
    pub spawn_tick: u64,
    /// For Clay Tablet / Book recipes: the TechId encoded into the produced
    /// item. Stamped onto `output_item.tech_payload` at completion. None for
    /// every other recipe.
    pub tech_payload: Option<TechId>,
}

impl CraftOrder {
    pub fn new(
        faction_id: u32,
        recipe_id: u8,
        workbench_tile: Option<(i32, i32)>,
        anchor_tile: (i32, i32),
        spawn_tick: u64,
        tech_payload: Option<TechId>,
    ) -> Option<Self> {
        let recipe = CRAFT_RECIPES.get(recipe_id as usize)?;
        let mut deposits = [GoodNeed::default(); MAX_CRAFT_INPUTS];
        let count = recipe.inputs.len().min(MAX_CRAFT_INPUTS);
        for (i, &(good, qty)) in recipe.inputs.iter().take(count).enumerate() {
            deposits[i] = GoodNeed {
                good,
                needed: qty.min(u8::MAX as u32) as u8,
                deposited: 0,
            };
        }
        Some(Self {
            faction_id,
            workbench_tile,
            anchor_tile,
            recipe_id,
            deposits,
            deposit_count: count as u8,
            work_progress: 0,
            spawn_tick,
            tech_payload,
        })
    }

    pub fn is_satisfied(&self) -> bool {
        for i in 0..self.deposit_count as usize {
            if self.deposits[i].deposited < self.deposits[i].needed {
                return false;
            }
        }
        true
    }
}

/// Maps anchor tile → CraftOrder entity. Mirrors `BlueprintMap` so resolvers
/// can find the order from a worker's tile cheaply.
#[derive(Resource, Default)]
pub struct CraftOrderMap(pub AHashMap<(i32, i32), Entity>);

/// Per-faction cap on simultaneous orders. Keeps the work board focused.
const CRAFT_ORDERS_PER_FACTION_BASE: u32 = 1;
const CRAFT_ORDERS_PER_FACTION_MAX: u32 = 3;

/// A `CraftOrder` older than this without completing is considered stuck
/// (materials never arrived, station inaccessible, no idle worker, etc.) and
/// gets despawned so the per-faction cap doesn't permanently fill with stale
/// orders. ~30 s at 20 Hz; recipes finish within ≤80 work ticks once
/// satisfied, so this leaves ample slack.
const CRAFT_ORDER_TIMEOUT_TICKS: u64 = 600;

/// Faction craft-order planner. For each faction with an open `Craft` job
/// posting, spawns a `CraftOrder` if all ingredients are union-available and
/// no order for that recipe is already in flight. Mirrors
/// `chief_job_posting_system`'s cadence (60-tick interval).
pub fn faction_craft_order_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    board: Res<JobBoard>,
    workbench_map: Res<WorkbenchMap>,
    loom_map: Res<LoomMap>,
    mut order_map: ResMut<CraftOrderMap>,
    order_query: Query<&CraftOrder>,
    agent_query: Query<(&FactionMember, &EconomicAgent, &LodLevel)>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    // Timeout sweep: despawn orders older than `CRAFT_ORDER_TIMEOUT_TICKS`
    // (stuck on materials / station / no worker) and prune map entries whose
    // entity has already gone away. Workers attached to a despawned order will
    // self-clear next tick — `craft_order_system` resets `ai.task_id` when
    // `order_query.get()` fails, and `plan_execution_system`'s safety net at
    // the top releases any lingering storage reservation.
    {
        let now = clock.tick;
        let mut to_drop: Vec<((i32, i32), Entity)> = Vec::new();
        for (&anchor, &order_entity) in order_map.0.iter() {
            match order_query.get(order_entity) {
                Ok(order) => {
                    if now.saturating_sub(order.spawn_tick) > CRAFT_ORDER_TIMEOUT_TICKS {
                        to_drop.push((anchor, order_entity));
                    }
                }
                Err(_) => to_drop.push((anchor, order_entity)),
            }
        }
        for (anchor, entity) in to_drop {
            order_map.0.remove(&anchor);
            if let Some(ec) = commands.get_entity(entity) {
                ec.despawn_recursive();
            }
        }
    }

    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO {
            continue;
        }
        let cap_base = CRAFT_ORDERS_PER_FACTION_BASE;
        let cap = cap_base
            .saturating_add(faction.member_count / 4)
            .min(CRAFT_ORDERS_PER_FACTION_MAX);

        // Count currently-live orders for this faction (and per-recipe).
        let mut live_total: u32 = 0;
        let mut live_recipes: AHashMap<u8, u32> = AHashMap::new();
        for order in order_query.iter() {
            if order.faction_id == faction_id {
                live_total += 1;
                *live_recipes.entry(order.recipe_id).or_insert(0) += 1;
            }
        }
        if live_total >= cap {
            continue;
        }

        // Sum faction inventory across living members.
        let mut faction_inv: AHashMap<Good, u32> = AHashMap::new();
        for (member, agent, lod) in agent_query.iter() {
            if *lod == LodLevel::Dormant || member.faction_id != faction_id {
                continue;
            }
            for (item, qty) in agent.inventory.iter() {
                if *qty > 0 {
                    *faction_inv.entry(item.good).or_insert(0) =
                        faction_inv.entry(item.good).or_insert(0).saturating_add(*qty);
                }
            }
        }

        // Walk Craft postings and decide per-recipe whether to spawn an order.
        let postings = board.faction_postings(faction_id);
        for posting in postings.iter() {
            if !matches!(posting.kind, JobKind::Craft) {
                continue;
            }
            let JobProgress::Crafting { recipe, .. } = posting.progress else {
                continue;
            };
            if posting.progress.is_complete() {
                continue;
            }
            if live_recipes.get(&recipe).copied().unwrap_or(0) >= 1 {
                continue;
            }
            let Some(recipe_def) = CRAFT_RECIPES.get(recipe as usize) else {
                continue;
            };
            // Tech gate.
            if let Some(tech) = recipe_def.tech_gate {
                if !faction.techs.has(tech) {
                    continue;
                }
            }
            // Union-availability for every ingredient.
            let mut ok = true;
            for &(good, qty) in recipe_def.inputs {
                let in_inv = faction_inv.get(&good).copied().unwrap_or(0);
                let in_store = faction.storage.totals.get(&good).copied().unwrap_or(0);
                if in_inv.saturating_add(in_store) >= qty {
                    continue;
                }
                // Not enough sitting around — but raw resources may be visible
                // or remembered. The strict per-ingredient memory check is too
                // strict to evaluate per-faction here; the existing plan-time
                // filters take care of that. Be lenient and let the order spawn
                // if at least one worker is available.
                ok = false;
                break;
            }
            if !ok {
                continue;
            }

            // Pick anchor tile: workbench/loom for station-bound recipes;
            // faction home for stationless ones.
            let anchor_opt: Option<((i32, i32), Option<(i32, i32)>)> =
                match recipe_def.requires_station {
                    Some(crate::simulation::crafting::StationKind::Workbench) => workbench_map
                        .0
                        .iter()
                        .filter(|((tx, ty), _)| {
                            let dx = (*tx as i32 - faction.home_tile.0 as i32).abs();
                            let dy = (*ty as i32 - faction.home_tile.1 as i32).abs();
                            dx <= 16 && dy <= 16
                        })
                        .map(|(&pos, _)| (pos, Some(pos)))
                        .next(),
                    Some(crate::simulation::crafting::StationKind::Loom) => loom_map
                        .0
                        .iter()
                        .filter(|((tx, ty), _)| {
                            let dx = (*tx as i32 - faction.home_tile.0 as i32).abs();
                            let dy = (*ty as i32 - faction.home_tile.1 as i32).abs();
                            dx <= 16 && dy <= 16
                        })
                        .map(|(&pos, _)| (pos, Some(pos)))
                        .next(),
                    None => Some((faction.home_tile, None)),
                };
            let Some((anchor, workbench)) = anchor_opt else {
                continue;
            };

            // Avoid colliding with an existing CraftOrder at the anchor tile.
            if order_map.0.contains_key(&anchor) {
                continue;
            }

            // Pull tech_payload from the JobBoard posting (chief-posting path
            // sets it for Clay Tablet / Book recipes; everything else is None).
            let tech_payload = match posting.progress {
                JobProgress::Crafting { tech_payload, .. } => tech_payload,
                _ => None,
            };
            let Some(order) =
                CraftOrder::new(faction_id, recipe, workbench, anchor, clock.tick, tech_payload)
            else {
                continue;
            };
            let wp = tile_to_world(anchor.0 as i32, anchor.1 as i32);
            let entity = commands
                .spawn((
                    order,
                    Transform::from_xyz(wp.x, wp.y, 0.32),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            order_map.0.insert(anchor, entity);
            live_total += 1;
            *live_recipes.entry(recipe).or_insert(0) += 1;
            if live_total >= cap {
                break;
            }

        }
    }
}

/// Hauler/worker resolution for `CraftOrder`s. Mirrors `construction_system`:
///   • `TaskKind::HaulToCraftOrder` — drops matching held goods into the
///     order's deposit slots and returns to Idle the same tick.
///   • `TaskKind::WorkOnCraftOrder` — once `is_satisfied()`, advances
///     `work_progress` by one per on-site worker; on completion, produces the
///     output to the worker's inventory and despawns the order.
pub fn craft_order_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut order_map: ResMut<CraftOrderMap>,
    mut board: ResMut<JobBoard>,
    mut job_completed: EventWriter<JobCompletedEvent>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut order_query: Query<&mut CraftOrder>,
    member_query: Query<&FactionMember>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &mut Skills,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
        &Transform,
        Option<&JobClaim>,
        Option<&mut crate::simulation::plan::ActivePlan>,
    )>,
) {
    let mut order_haulers: AHashMap<Entity, Vec<(Entity, [u32; MAX_CRAFT_INPUTS])>> =
        AHashMap::new();
    let mut order_workers: AHashMap<Entity, Vec<Entity>> = AHashMap::new();

    for (entity, mut ai, agent, carrier, _skills, _member, slot, lod, _transform, _claim, _ap) in
        agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        let task = ai.task_id;
        let is_hauler = task == TaskKind::HaulToCraftOrder as u16;
        let is_worker = task == TaskKind::WorkOnCraftOrder as u16;
        if !is_hauler && !is_worker {
            continue;
        }
        let Some(order_entity) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };
        let Ok(order) = order_query.get(order_entity) else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            ai.target_entity = None;
            continue;
        };
        if is_hauler {
            let mut snap = [0u32; MAX_CRAFT_INPUTS];
            let mut useful = false;
            for i in 0..order.deposit_count as usize {
                let still = order.deposits[i]
                    .needed
                    .saturating_sub(order.deposits[i].deposited) as u32;
                if still > 0 {
                    let in_hand = carrier.quantity_of_good(order.deposits[i].good);
                    let in_inv = agent.quantity_of(order.deposits[i].good);
                    snap[i] = in_hand.saturating_add(in_inv);
                    if snap[i] > 0 {
                        useful = true;
                    }
                }
            }
            if !useful {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
                ai.target_entity = None;
                continue;
            }
            order_haulers
                .entry(order_entity)
                .or_default()
                .push((entity, snap));
        } else {
            order_workers.entry(order_entity).or_default().push(entity);
        }
    }

    if order_haulers.is_empty() && order_workers.is_empty() {
        return;
    }

    let mut completed_agents: Vec<Entity> = Vec::new();
    let mut hauler_done: Vec<Entity> = Vec::new();
    let mut orphaned_agents: Vec<Entity> = Vec::new();
    let mut xp_grants: Vec<Entity> = Vec::new();
    // (agent_entity, good, qty_to_remove)
    let mut good_removals: Vec<(Entity, Good, u32)> = Vec::new();
    // (agent_entity, recipe_id, tech_payload) — paid out as inventory at end of pass.
    let mut output_grants: Vec<(Entity, u8, Option<TechId>)> = Vec::new();
    // Job board credits to apply (recipe, qty) per worker entity.
    let mut order_completion_credits: Vec<(Entity, u8, u32)> = Vec::new();

    let mut order_entities: Vec<Entity> = order_haulers
        .keys()
        .copied()
        .chain(order_workers.keys().copied())
        .collect();
    order_entities.sort_unstable();
    order_entities.dedup();

    for order_entity in order_entities {
        let Ok(mut order) = order_query.get_mut(order_entity) else {
            if let Some(haulers) = order_haulers.get(&order_entity) {
                orphaned_agents.extend(haulers.iter().map(|(e, _)| *e));
            }
            if let Some(workers) = order_workers.get(&order_entity) {
                orphaned_agents.extend(workers.iter().copied());
            }
            continue;
        };

        // 1. Apply hauler deposits.
        if let Some(haulers) = order_haulers.get(&order_entity) {
            for &(agent_e, snap) in haulers {
                for i in 0..order.deposit_count as usize {
                    let need = order.deposits[i];
                    let still = need.needed.saturating_sub(need.deposited) as u32;
                    if still == 0 || snap[i] == 0 {
                        continue;
                    }
                    let take = still.min(snap[i]);
                    good_removals.push((agent_e, need.good, take));
                    order.deposits[i].deposited =
                        order.deposits[i].deposited.saturating_add(take as u8);
                }
                hauler_done.push(agent_e);
            }
        }

        // 2. Advance work once satisfied.
        let Some(recipe) = CRAFT_RECIPES.get(order.recipe_id as usize) else {
            // Unknown recipe — clean up.
            if let Some(workers) = order_workers.get(&order_entity) {
                orphaned_agents.extend(workers.iter().copied());
            }
            if let Some(haulers) = order_haulers.get(&order_entity) {
                orphaned_agents.extend(haulers.iter().map(|(e, _)| *e));
            }
            order_map.0.remove(&order.anchor_tile);
            commands.entity(order_entity).despawn_recursive();
            continue;
        };

        if order.is_satisfied() {
            if let Some(workers) = order_workers.get(&order_entity) {
                let advance = workers.len() as u8;
                order.work_progress = order
                    .work_progress
                    .saturating_add(advance)
                    .min(recipe.work_ticks);
                xp_grants.extend(workers.iter().copied());
            }
        }

        if order.work_progress >= recipe.work_ticks && order.is_satisfied() {
            // Pick a "lead" worker to receive the output. The first registered
            // worker is fine — output is intentionally portable, not anchored
            // to a specific tile.
            let lead = order_workers
                .get(&order_entity)
                .and_then(|v| v.first().copied());
            if let Some(lead_e) = lead {
                output_grants.push((lead_e, order.recipe_id, order.tech_payload));
                order_completion_credits.push((lead_e, order.recipe_id, recipe.output_qty));
                let faction_id =
                    member_query.get(lead_e).map(|m| m.faction_id).unwrap_or(0);
                activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                    tick: clock.tick,
                    actor: lead_e,
                    faction_id,
                    kind: crate::ui::activity_log::ActivityEntryKind::Crafted {
                        name: recipe.name,
                    },
                });
            }

            order_map.0.remove(&order.anchor_tile);
            commands.entity(order_entity).despawn_recursive();

            if let Some(workers) = order_workers.get(&order_entity) {
                completed_agents.extend(workers.iter().copied());
            }
            if let Some(haulers) = order_haulers.get(&order_entity) {
                completed_agents.extend(haulers.iter().map(|(e, _)| *e));
            }
        }
    }

    if good_removals.is_empty()
        && completed_agents.is_empty()
        && hauler_done.is_empty()
        && orphaned_agents.is_empty()
        && xp_grants.is_empty()
        && output_grants.is_empty()
    {
        return;
    }

    for (entity, mut ai, mut agent, mut carrier, mut skills, _member, _slot, _lod, _t, claim, mut plan_opt) in
        agent_query.iter_mut()
    {
        for &(ae, good, qty) in &good_removals {
            if ae == entity {
                let from_hand = carrier.remove_good(good, qty);
                let still = qty - from_hand;
                if still > 0 {
                    agent.remove_good(good, still);
                }
            }
        }

        if xp_grants.contains(&entity) {
            // Crafting XP is granted at recipe-completion (per craft); per-tick
            // workers also get a small Crafting XP nudge so labor is rewarded.
            skills.gain_xp(SkillKind::Crafting, 1);
        }

        // Output payout & job credit for the lead worker on completed orders.
        for &(ae, recipe_id, tech_payload) in &output_grants {
            if ae != entity {
                continue;
            }
            let Some(recipe) = CRAFT_RECIPES.get(recipe_id as usize) else {
                continue;
            };
            let quality = quality_for_skill(skills.get(SkillKind::Crafting));
            let mut output_item = if let Some(mat) = recipe.output_material {
                Item::new_manufactured(recipe.output_good, mat, quality)
            } else {
                Item::new_commodity(recipe.output_good)
            };
            output_item.display_name = Some(recipe.name);
            // Stamp tech payload onto Clay Tablet / Book outputs. Equality
            // partitions tablets-of-tech-A from tablets-of-tech-B in
            // inventories and ground-item piles.
            output_item.tech_payload = tech_payload;
            agent.add_item(output_item, recipe.output_qty);
            skills.gain_xp(SkillKind::Crafting, recipe.crafting_xp);
        }
        // Job-board credit: post recipe-completion to the worker's matching
        // Craft posting (if any).
        if let Some(claim) = claim {
            for &(ae, recipe_id, output_qty) in &order_completion_credits {
                if ae != entity {
                    continue;
                }
                let matches_recipe = board
                    .get(claim.job_id)
                    .map(|p| matches!(
                        p.progress,
                        JobProgress::Crafting { recipe, .. } if recipe == recipe_id
                    ))
                    .unwrap_or(false);
                if matches_recipe {
                    record_progress(
                        &mut commands,
                        &mut board,
                        &mut job_completed,
                        claim,
                        JobKind::Craft,
                        output_qty,
                    );
                }
            }
        }

        let is_completed = completed_agents.contains(&entity);
        let is_hauler_done = hauler_done.contains(&entity);
        let is_orphaned = orphaned_agents.contains(&entity);

        if is_completed || is_hauler_done || is_orphaned {
            if is_completed {
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += if ai.task_id == TaskKind::HaulToCraftOrder as u16 {
                        0.4
                    } else {
                        1.0
                    };
                }
            }
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            ai.work_progress = 0;
        }
    }

}

fn quality_for_skill(crafting_xp: u32) -> ItemQuality {
    match crafting_xp {
        0..=9 => ItemQuality::Poor,
        10..=49 => ItemQuality::Normal,
        50..=149 => ItemQuality::Fine,
        _ => ItemQuality::Masterwork,
    }
}

// `craft_system` (the legacy personal-inventory crafter) was removed when the
// CraftOrder pipeline took over. Crafting now flows through
// `faction_craft_order_system` (planner) → `craft_order_system` (haul + work +
// completion). `TaskKind::Craft` is retained as a deprecated enum value so old
// references compile, but no plan dispatches it anymore.
