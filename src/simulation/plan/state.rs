//! State-vector construction and visibility counters used by plan scoring.
//!
//! Split out of `plan/mod.rs` so changes to the linear-scoring feature layout
//! don't force a re-read of the registry or the execution loop. The
//! `SI_*` indices and `STATE_DIM` live in `mod.rs` because the registry
//! literals reference them and Rust can't make sibling-module constants
//! visible to a `pub use` parent.

use super::{
    PlanHistory, PLAN_HISTORY_LEN, STATE_DIM, SI_STORAGE_FOOD, SI_STORAGE_SEED, SI_STORAGE_STONE,
    SI_STORAGE_WOOD, SI_VIS_GROUND_FOOD, SI_VIS_GROUND_STONE, SI_VIS_GROUND_WOOD,
    SI_VIS_PLANT_FOOD, SI_VIS_STONE_TILE, SI_VIS_TREE, VISIBILITY_RADIUS, VISIBILITY_SATURATE,
};
use bevy::prelude::*;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::faction::{FactionMember, FactionStorage, SOLO};
use crate::simulation::items::GroundItem;
use crate::simulation::memory::{AgentMemory, MemoryKind};
use crate::simulation::needs::Needs;
use crate::simulation::plants::{GrowthStage, Plant, PlantKind, PlantMap};
use crate::simulation::skills::Skills;
use crate::world::chunk::ChunkMap;
use crate::world::seasons::Calendar;
use crate::world::spatial::SpatialIndex;
use crate::world::tile::TileKind;

// ── State vector ──────────────────────────────────────────────────────────────

/// Saturation cap for faction-storage state slots (`SI_STORAGE_*`). Storage
/// totals divided by this and clamped to [0,1] — same shape as visibility
/// saturation so the slots compose cleanly with the rest of the state vector.
pub const STORAGE_SATURATE: f32 = 20.0;

pub fn build_state_vec(
    needs: &Needs,
    agent: &EconomicAgent,
    skills: &Skills,
    member: &FactionMember,
    memory: Option<&AgentMemory>,
    calendar: &Calendar,
    plan_history: Option<&PlanHistory>,
    storage: Option<&FactionStorage>,
    vis_plant_food: u8,
    vis_trees: u8,
    vis_stone_tiles: u8,
    vis_ground_wood: u8,
    vis_ground_stone: u8,
    vis_ground_food: u8,
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

    // 25-28: last PLAN_HISTORY_LEN plan outcomes (2 floats per slot).
    // For each slot: (plan_id_norm, failure_flag). Lets the scorer factor
    // recent failures into plan selection. Tick timestamps in the history
    // entries are not surfaced here — the soft penalty in
    // `plan_execution_system` consumes them via `recently_failed_count`.
    if let Some(history) = plan_history {
        for i in 0..PLAN_HISTORY_LEN {
            let base = 25 + i * 2;
            match history.entries[i] {
                Some((plan_id, outcome, _tick)) => {
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

    // 29-32: faction storage stocks. SOLO members and unfounded factions pass
    // None → all storage slots stay 0, which is the correct signal for plans
    // that read storage availability (WithdrawAndEat, HaulFromStorageAndBuild,
    // DeliverFromStorageToCraftOrder).
    if let Some(st) = storage {
        s[SI_STORAGE_FOOD] = (st.food_total() / STORAGE_SATURATE).clamp(0.0, 1.0);
        s[SI_STORAGE_WOOD] = (st.totals.get(&Good::Wood).copied().unwrap_or(0) as f32
            / STORAGE_SATURATE)
            .clamp(0.0, 1.0);
        s[SI_STORAGE_STONE] = (st.totals.get(&Good::Stone).copied().unwrap_or(0) as f32
            / STORAGE_SATURATE)
            .clamp(0.0, 1.0);
        s[SI_STORAGE_SEED] = (st.seed_total() as f32 / STORAGE_SATURATE).clamp(0.0, 1.0);
    }

    // 35-37: source-only visibility — mature edible plants, mature trees, stone
    // tiles within VISIBILITY_RADIUS, normalised to [0, 1] at VISIBILITY_SATURATE.
    // Feeds Forage/Gather/Deliver*ToCraftOrder plans, which can only act on
    // sources. Loose ground items live on slots 38-40 so source and good never
    // share a signal.
    let sat = VISIBILITY_SATURATE as f32;
    s[SI_VIS_PLANT_FOOD] = (vis_plant_food as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_TREE] = (vis_trees as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_STONE_TILE] = (vis_stone_tiles as f32 / sat).clamp(0.0, 1.0);

    // 38-40: ground-only visibility for loose food/wood/stone GroundItems.
    // Feeds the Scavenge* plans, which can only pick up loose items.
    s[SI_VIS_GROUND_WOOD] = (vis_ground_wood as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_GROUND_STONE] = (vis_ground_stone as f32 / sat).clamp(0.0, 1.0);
    s[SI_VIS_GROUND_FOOD] = (vis_ground_food as f32 / sat).clamp(0.0, 1.0);

    s
}

/// Counts mature edible *plants* (sources) within `VISIBILITY_RADIUS` of the
/// agent's tile, saturating at `VISIBILITY_SATURATE`. Drives `SI_VIS_PLANT_FOOD`,
/// which feeds plans that harvest plants (`ForageFood`). Loose food on the
/// ground is counted separately by `count_visible_ground_food` so source and
/// good never share a signal. Cheap because plan selection is bucketed
/// (~1 Hz per agent) and scanning early-exits once saturated.
pub fn count_visible_plant_food(
    tx: i32,
    ty: i32,
    plant_map: &PlantMap,
    plants: &Query<&Plant>,
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
        }
    }
    n
}

/// Counts mature *trees* (sources) within `VISIBILITY_RADIUS`. Drives
/// `SI_VIS_TREE`, which feeds plans that chop trees (`GatherWood`,
/// `DeliverWoodToCraftOrder`). Loose wood on the ground is counted by
/// `count_visible_ground_wood`.
pub fn count_visible_trees(
    tx: i32,
    ty: i32,
    plant_map: &PlantMap,
    plants: &Query<&Plant>,
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
        }
    }
    n
}

/// Counts visible *Stone tiles* (sources) within `VISIBILITY_RADIUS`. Drives
/// `SI_VIS_STONE_TILE`, which feeds plans that mine stone (`GatherStone`,
/// `DeliverStoneToCraftOrder`). Loose stone on the ground is counted by
/// `count_visible_ground_stone`.
pub fn count_visible_stone_tiles(
    tx: i32,
    ty: i32,
    chunk_map: &ChunkMap,
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
        }
    }
    n
}

/// Counts visible loose edible `GroundItem`s within `VISIBILITY_RADIUS`. Drives
/// `SI_VIS_GROUND_FOOD`, which feeds `ScavengeFood`. Mature edible plants are
/// counted by `count_visible_plant_food` so the two plans never share a signal.
pub fn count_visible_ground_food(
    tx: i32,
    ty: i32,
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
            for &e in spatial.get(tx + dx, ty + dy) {
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

/// Count visible loose Wood `GroundItem`s only (excludes standing trees).
/// Drives `SI_VIS_GROUND_WOOD` so `ScavengeWood` scores above `GatherWood` only
/// when there's actual ground litter — without this split the two plans share
/// the same visibility signal and `ScavengeWood` would fire spuriously next to
/// untouched forest.
pub fn count_visible_ground_wood(
    tx: i32,
    ty: i32,
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
            for &e in spatial.get(tx + dx, ty + dy) {
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

/// Count visible loose Stone `GroundItem`s only (excludes Stone tiles). See
/// `count_visible_ground_wood`.
pub fn count_visible_ground_stone(
    tx: i32,
    ty: i32,
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
            for &e in spatial.get(tx + dx, ty + dy) {
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
