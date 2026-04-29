use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::{Item, ItemMaterial, ItemQuality};
use crate::simulation::construction::{LoomMap, WorkbenchMap};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::TechId;
use crate::world::terrain::TILE_SIZE;
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
    BOW_AND_ARROW, BRONZE_WEAPONS, COPPER_TOOLS, FIRE_MAKING, FIRED_POTTERY, FLINT_KNAPPING,
    HUNTING_SPEAR, LOOM_WEAVING,
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
];

fn quality_for_skill(crafting_xp: u32) -> ItemQuality {
    match crafting_xp {
        0..=9 => ItemQuality::Poor,
        10..=49 => ItemQuality::Normal,
        50..=149 => ItemQuality::Fine,
        _ => ItemQuality::Masterwork,
    }
}

pub fn craft_system(
    clock: Res<SimClock>,
    faction_registry: Res<FactionRegistry>,
    workbench_map: Res<WorkbenchMap>,
    loom_map: Res<LoomMap>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
        &Transform,
    )>,
) {
    for (mut ai, mut agent, mut skills, member, slot, lod, transform) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::Craft as u16 || ai.state != AiState::Working {
            continue;
        }

        let recipe_id = ai.target_z as usize;
        let Some(recipe) = CRAFT_RECIPES.get(recipe_id) else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };

        // Tech gate check
        if let Some(tech) = recipe.tech_gate {
            if member.faction_id != crate::simulation::faction::SOLO {
                if let Some(faction) = faction_registry.factions.get(&member.faction_id) {
                    if !faction.techs.has(tech) {
                        ai.state = AiState::Idle;
                        ai.task_id = PersonAI::UNEMPLOYED;
                        continue;
                    }
                }
            }
        }

        // Station gate: agent must stand within 1 tile of a station of the
        // required kind. Faction is not enforced — any station of the right
        // type works.
        if let Some(station) = recipe.requires_station {
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let mut found = false;
            'find: for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    let pos = ((tx + dx) as i16, (ty + dy) as i16);
                    let hit = match station {
                        StationKind::Workbench => workbench_map.0.contains_key(&pos),
                        StationKind::Loom => loom_map.0.contains_key(&pos),
                    };
                    if hit {
                        found = true;
                        break 'find;
                    }
                }
            }
            if !found {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                continue;
            }
        }

        if ai.work_progress < recipe.work_ticks {
            continue;
        }

        // Verify all inputs are available
        let mut can_craft = true;
        for &(good, qty) in recipe.inputs {
            if agent.quantity_of(good) < qty {
                can_craft = false;
                break;
            }
        }

        if !can_craft {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        // Consume inputs
        for &(good, qty) in recipe.inputs {
            agent.remove_good(good, qty);
        }

        // Produce output
        let quality = quality_for_skill(skills.get(SkillKind::Crafting));
        let output_item = if let Some(mat) = recipe.output_material {
            Item::new_manufactured(recipe.output_good, mat, quality)
        } else {
            Item::new_commodity(recipe.output_good)
        };
        agent.add_item(output_item, recipe.output_qty);

        // Gain XP
        skills.gain_xp(SkillKind::Crafting, recipe.crafting_xp);

        ai.work_progress = 0;
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}
