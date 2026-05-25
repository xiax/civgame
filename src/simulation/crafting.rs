use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;

use crate::economy::item::{Item, ItemMaterial, ItemQuality};
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::construction::{GoodNeed, LoomMap, WorkbenchMap, MAX_BUILD_INPUTS};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::{yield_for_maintenance_boundary, MAINTENANCE_WORK_SLICE_TICKS};
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
use std::sync::OnceLock;

/// A crafting station required for certain recipes. Some recipes (e.g. Bow,
/// Pottery) work in the open and have `requires_station: None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StationKind {
    Workbench,
    Loom,
}

pub struct CraftRecipe {
    pub name: &'static str,
    /// All ingredients that must be consumed. Agent's inventory is
    /// checked for each. Phase 2d: keyed on `ResourceId` so adding a
    /// new resource to the catalog automatically widens the recipe-input
    /// space without changes to recipe consumers.
    pub inputs: Vec<(ResourceId, u32)>,
    pub output_resource: ResourceId,
    pub output_qty: u32,
    /// None means the output item has no material tag (e.g. Luxury goods).
    pub output_material: Option<ItemMaterial>,
    pub work_ticks: u8,
    pub crafting_xp: u32,
    /// None means no tech required.
    pub tech_gate: Option<TechId>,
    /// If Some, the agent must be within 1 tile of an entity of this kind to craft.
    pub requires_station: Option<StationKind>,
    /// Realistic Tool Overhaul: tools a worker must hold (in their `ToolKit`)
    /// before `WorkOnCraftOrder` can advance. Empty for bootstrap-safe recipes.
    pub tool_requirements: Vec<crate::simulation::tools::ToolRequirement>,
    /// Minimum craft quality the output is guaranteed — microlithic / polished
    /// recipes floor at `Fine` so a low-skill crafter still produces the tier.
    pub quality_floor: Option<ItemQuality>,
    /// Minimum `WorkbenchTier` the station must be (copper/bronze recipes need
    /// a copper/bronze workbench). `None` = any workbench (when station-bound).
    pub min_station_tier: Option<crate::simulation::construction::WorkbenchTier>,
}

impl CraftRecipe {
    /// Effective output quality given a crafter's skill-derived quality —
    /// never below the recipe's `quality_floor`.
    pub fn effective_quality(&self, skill_quality: ItemQuality) -> ItemQuality {
        match self.quality_floor {
            Some(floor) if quality_rank(floor) > quality_rank(skill_quality) => floor,
            _ => skill_quality,
        }
    }
}

/// Ordinal rank of an `ItemQuality` (Poor < Normal < Fine < Masterwork).
pub fn quality_rank(q: ItemQuality) -> u8 {
    match q {
        ItemQuality::Poor => 0,
        ItemQuality::Normal => 1,
        ItemQuality::Fine => 2,
        ItemQuality::Masterwork => 3,
    }
}

use crate::simulation::technology::{
    ANIMAL_HUSBANDRY, ARD_PLOW, BONE_TOOLS, BOW_AND_ARROW, BRONZE_CASTING, BRONZE_TOOLS,
    BRONZE_WEAPONS, COPPER_TOOLS, CROP_CULTIVATION, CUNEIFORM_WRITING, FIRED_POTTERY, FIRE_MAKING,
    FISHING, FLINT_KNAPPING, FOOD_SMOKING, HUNTING_SPEAR, LOOM_WEAVING, MICROLITHIC_TOOLS, OX_CART,
};
use crate::simulation::construction::WorkbenchTier;
use crate::simulation::tools::{
    item_tool_tier, work_speed_mult, ToolKit, ToolRequirement, ToolTier, ToolUseKind,
};

/// Average work-speed multiplier the worker's carried tools grant for a
/// recipe. Recipes with no tool requirement run at the `1.0` baseline.
fn craft_tool_speed(recipe: &CraftRecipe, kit: Option<&ToolKit>) -> f32 {
    if recipe.tool_requirements.is_empty() {
        return 1.0;
    }
    let Some(kit) = kit else {
        return 1.0;
    };
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for req in &recipe.tool_requirements {
        if let Some(item) = kit.best_for(req) {
            sum += work_speed_mult(item_tool_tier(item));
            n += 1.0;
        }
    }
    if n > 0.0 {
        sum / n
    } else {
        1.0
    }
}

/// Lazily-built recipe table. Phase 2d migrated `CraftRecipe` from a
/// `static &[...]` to a `OnceLock<Vec<...>>` so the inputs/outputs can
/// be `ResourceId`s — those need to be resolved against the runtime
/// catalog at first read, which a `const` array can't express.
///
/// Production callers reach this via [`craft_recipes`]; they never
/// notice the lazy-init since `core_ids::catalog()` (which feeds the
/// resolution) is also lazy. First call out of any test or system
/// triggers both inits in the right order.
/// Terse constructor for the Realistic Tool Overhaul tool recipes — all
/// produce one unit and are Workbench-bound.
#[allow(clippy::too_many_arguments)]
fn tool_recipe(
    name: &'static str,
    inputs: Vec<(ResourceId, u32)>,
    output_resource: ResourceId,
    output_material: ItemMaterial,
    work_ticks: u8,
    crafting_xp: u32,
    tech_gate: TechId,
    tool_requirements: Vec<ToolRequirement>,
    quality_floor: Option<ItemQuality>,
    min_station_tier: Option<WorkbenchTier>,
) -> CraftRecipe {
    CraftRecipe {
        name,
        inputs,
        output_resource,
        output_qty: 1,
        output_material: Some(output_material),
        work_ticks,
        crafting_xp,
        tech_gate: Some(tech_gate),
        requires_station: Some(StationKind::Workbench),
        tool_requirements,
        quality_floor,
        min_station_tier,
    }
}

fn build_craft_recipes() -> Vec<CraftRecipe> {
    // Force catalog/core_ids init so the `*.get().copied().unwrap()` calls
    // below are safe.
    let _ = core_ids::catalog();

    let stone = core_ids::stone();
    let wood = core_ids::wood();
    let iron = core_ids::iron();
    let coal = core_ids::coal();
    let grain = core_ids::grain();
    let skin = core_ids::skin();
    let cloth = core_ids::cloth();
    let tools = core_ids::tools();
    let weapon = core_ids::weapon();
    let bow = core_ids::bow();
    let sling = core_ids::sling();
    let shield = core_ids::shield();
    let armor = core_ids::armor();
    let luxury = core_ids::luxury();
    let clay_tablet = core_ids::clay_tablet();
    let book = core_ids::book();
    let meat = core_ids::meat();
    let preserved_meat = core_ids::preserved_meat();
    let fish = core_ids::fish();
    let preserved_fish = core_ids::preserved_fish();
    let raw_water = core_ids::raw_water();
    let clean_water = core_ids::clean_water();
    let ard_plow = core_ids::ard_plow();
    let cart_frame_small = core_ids::cart_frame_small();
    let cart_frame_medium = core_ids::cart_frame_medium();
    let cart_wheel_wood = core_ids::cart_wheel_wood();
    let cart_wheel_ironrim = core_ids::cart_wheel_ironrim();
    let copper = core_ids::copper();
    let tin = core_ids::tin();
    let bone = core_ids::bone();
    let knife = core_ids::knife();
    let axe = core_ids::axe();
    let pick = core_ids::pick();
    let hammer = core_ids::hammer();
    let sickle = core_ids::sickle();
    let awl = core_ids::awl();
    let fishing_kit = core_ids::fishing_kit();
    let limestone = core_ids::limestone();
    let lime = core_ids::lime();
    // Realistic Tool Overhaul tool-requirement shorthands.
    let cut = ToolRequirement::any(ToolUseKind::Cut);
    let chop = ToolRequirement::any(ToolUseKind::Chop);
    let shape = ToolRequirement::any(ToolUseKind::Shape);
    let smith = ToolRequirement::any(ToolUseKind::Smith);
    let stitch = ToolRequirement::any(ToolUseKind::Stitch);
    let hammer_stone = ToolRequirement::at_least(ToolUseKind::Smith, ToolTier::Stone);
    let hammer_copper = ToolRequirement::at_least(ToolUseKind::Smith, ToolTier::Copper);

    vec![
        // 0
        CraftRecipe {
            name: "Stone Tools",
            inputs: vec![(stone, 2), (wood, 1)],
            output_resource: tools,
            output_qty: 1,
            output_material: Some(ItemMaterial::Stone),
            work_ticks: 30,
            crafting_xp: 5,
            tech_gate: Some(FLINT_KNAPPING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 1
        CraftRecipe {
            name: "Spear",
            inputs: vec![(wood, 2), (stone, 1)],
            output_resource: weapon,
            output_qty: 1,
            output_material: Some(ItemMaterial::Stone),
            work_ticks: 40,
            crafting_xp: 5,
            tech_gate: Some(HUNTING_SPEAR),
            requires_station: None,
            tool_requirements: vec![cut],
            quality_floor: None,
            min_station_tier: None,
        },
        // 2
        CraftRecipe {
            name: "Torch",
            inputs: vec![(wood, 2)],
            output_resource: luxury,
            output_qty: 2,
            output_material: None,
            work_ticks: 20,
            crafting_xp: 3,
            tech_gate: Some(FIRE_MAKING),
            requires_station: None,
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 3 — ranged weapon (range 5 via `compute_combat_stats` on the `bow`
        // resource). Was previously misrouted to the generic melee `weapon`.
        CraftRecipe {
            name: "Bow",
            inputs: vec![(wood, 2), (skin, 1)],
            output_resource: bow,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 50,
            crafting_xp: 6,
            tech_gate: Some(BOW_AND_ARROW),
            requires_station: None,
            tool_requirements: vec![cut],
            quality_floor: None,
            min_station_tier: None,
        },
        // 4
        CraftRecipe {
            name: "Woven Cloth",
            inputs: vec![(grain, 3)],
            output_resource: cloth,
            output_qty: 1,
            output_material: None,
            work_ticks: 60,
            crafting_xp: 6,
            tech_gate: Some(LOOM_WEAVING),
            requires_station: Some(StationKind::Loom),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 5
        CraftRecipe {
            name: "Pottery",
            inputs: vec![(stone, 2), (wood, 1)],
            output_resource: luxury,
            output_qty: 2,
            output_material: None,
            work_ticks: 60,
            crafting_xp: 5,
            tech_gate: Some(FIRED_POTTERY),
            requires_station: None,
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 6
        CraftRecipe {
            name: "Wooden Shield",
            inputs: vec![(wood, 3)],
            output_resource: shield,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 40,
            crafting_xp: 4,
            tech_gate: None,
            requires_station: None,
            tool_requirements: vec![shape],
            quality_floor: None,
            min_station_tier: None,
        },
        // 7
        CraftRecipe {
            name: "Leather Armor",
            inputs: vec![(skin, 2)],
            output_resource: armor,
            output_qty: 1,
            output_material: Some(ItemMaterial::Leather),
            work_ticks: 50,
            crafting_xp: 6,
            tech_gate: None,
            requires_station: None,
            tool_requirements: vec![stitch],
            quality_floor: None,
            min_station_tier: None,
        },
        // 8
        CraftRecipe {
            name: "Iron Tools",
            inputs: vec![(iron, 2), (coal, 1)],
            output_resource: tools,
            output_qty: 1,
            output_material: Some(ItemMaterial::Iron),
            work_ticks: 60,
            crafting_xp: 8,
            tech_gate: Some(COPPER_TOOLS),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 9
        CraftRecipe {
            name: "Iron Sword",
            inputs: vec![(iron, 2), (wood, 1)],
            output_resource: weapon,
            output_qty: 1,
            output_material: Some(ItemMaterial::Iron),
            work_ticks: 80,
            crafting_xp: 10,
            tech_gate: Some(BRONZE_WEAPONS),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![smith],
            quality_floor: None,
            min_station_tier: None,
        },
        // 10
        CraftRecipe {
            name: "Clay Tablet",
            inputs: vec![(stone, 1), (wood, 1)],
            output_resource: clay_tablet,
            output_qty: 1,
            output_material: None,
            work_ticks: 90,
            crafting_xp: 8,
            tech_gate: Some(CUNEIFORM_WRITING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 11
        CraftRecipe {
            name: "Book",
            inputs: vec![(cloth, 2), (skin, 1)],
            output_resource: book,
            output_qty: 1,
            output_material: None,
            work_ticks: 180,
            crafting_xp: 12,
            tech_gate: Some(CUNEIFORM_WRITING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![stitch],
            quality_floor: None,
            min_station_tier: None,
        },
        // 12 — P7: Smoke 2 fresh meat into 3 preserved (lighter, banked
        // for migration). Workbench gating keeps this off the wild trail
        // until the band has set up a hearth + work station.
        CraftRecipe {
            name: "Preserved Meat",
            inputs: vec![(meat, 2), (wood, 1)],
            output_resource: preserved_meat,
            output_qty: 3,
            output_material: None,
            work_ticks: 60,
            crafting_xp: 4,
            tech_gate: Some(FOOD_SMOKING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![cut],
            quality_floor: None,
            min_station_tier: None,
        },
        // 13 — Boiling raw water into clean drinking water at a hearth /
        // workbench. Requires `FIRE_MAKING`; consumes 2 raw_water and a
        // unit of wood for fuel. Output is 1 clean_water — the loss models
        // evaporation. The recipe slots into the existing craft pipeline
        // (no bespoke `BoilWater` task) so it inherits workshop, skill,
        // and wage-aware labor wiring for free.
        CraftRecipe {
            name: "Boil Water",
            inputs: vec![(raw_water, 2), (wood, 1)],
            output_resource: clean_water,
            output_qty: 1,
            output_material: None,
            work_ticks: 30,
            crafting_xp: 2,
            tech_gate: Some(FIRE_MAKING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // 14 — Draftwork v2: wooden ard plow. Gated on `ARD_PLOW` tech;
        // 3 wood + 1 tools at a Workbench. The plow is durable — the
        // `chief_plow_dispatch_system` gates on `faction.storage.stock_of(
        // ard_plow) > 0` but the plow_task_system does NOT decrement on
        // completion (one plow serves many seasons; durability/decay is a
        // v2.1 follow-up).
        CraftRecipe {
            name: "Ard Plow",
            inputs: vec![(wood, 4)],
            output_resource: ard_plow,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 70,
            crafting_xp: 6,
            tech_gate: Some(ARD_PLOW),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![shape],
            quality_floor: None,
            min_station_tier: None,
        },
        // 15 — Cart Wheel (Wood). Legacy composable cart part — retained as a
        // catalog resource; vehicle assembly now uses the `design_bill` path.
        CraftRecipe {
            name: "Cart Wheel (Wood)",
            inputs: vec![(wood, 4)],
            output_resource: cart_wheel_wood,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 50,
            crafting_xp: 5,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![shape],
            quality_floor: None,
            min_station_tier: None,
        },
        // 16 — Cart Wheel (Iron-Rimmed). An iron tyre removes the wooden
        // wheel's drag penalty; gated on metallurgy.
        CraftRecipe {
            name: "Cart Wheel (Iron-Rimmed)",
            inputs: vec![(wood, 4), (iron, 1)],
            output_resource: cart_wheel_ironrim,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 80,
            crafting_xp: 8,
            tech_gate: Some(BRONZE_CASTING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![smith],
            quality_floor: None,
            min_station_tier: None,
        },
        // 17 — Cart Frame (Small). Light frame → Handcart on assembly.
        CraftRecipe {
            name: "Cart Frame (Small)",
            inputs: vec![(wood, 6)],
            output_resource: cart_frame_small,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 70,
            crafting_xp: 6,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![shape],
            quality_floor: None,
            min_station_tier: None,
        },
        // 18 — Cart Frame (Medium). Heavy frame → OxCart on assembly;
        // gated on `OX_CART` tech.
        CraftRecipe {
            name: "Cart Frame (Medium)",
            inputs: vec![(wood, 12), (skin, 2)],
            output_resource: cart_frame_medium,
            output_qty: 1,
            output_material: Some(ItemMaterial::Wood),
            work_ticks: 120,
            crafting_xp: 10,
            tech_gate: Some(OX_CART),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![shape],
            quality_floor: None,
            min_station_tier: None,
        },
        // 19 — Smoke/dried fish. Mirrors Preserved Meat (recipe 12): the
        // ration doesn't perish and weighs less per food-unit, so bands
        // bank it for migration. `FOOD_SMOKING`-gated, Workbench-bound.
        CraftRecipe {
            name: "Preserved Fish",
            inputs: vec![(fish, 2), (wood, 1)],
            output_resource: preserved_fish,
            output_qty: 3,
            output_material: None,
            work_ticks: 60,
            crafting_xp: 4,
            tech_gate: Some(FOOD_SMOKING),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![cut],
            quality_floor: None,
            min_station_tier: None,
        },
        // Sling — cheap ranged weapon (range 4). Appended last so existing
        // recipe indices stay stable.
        CraftRecipe {
            name: "Sling",
            inputs: vec![(skin, 1), (wood, 1)],
            output_resource: sling,
            output_qty: 1,
            output_material: Some(ItemMaterial::Leather),
            work_ticks: 30,
            crafting_xp: 4,
            tech_gate: Some(BOW_AND_ARROW),
            requires_station: None,
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
        // ── Realistic Tool Overhaul: functional tool recipes (21+) ────
        // Paleolithic — bootstrap-safe stone tools (loose stone + wood,
        // no tool requirement). FLINT_KNAPPING is a Paleolithic personal
        // tech every founder is seeded Aware of.
        // 21
        tool_recipe("Stone Knife", vec![(stone, 1), (wood, 1)], knife,
            ItemMaterial::Stone, 30, 4, FLINT_KNAPPING, vec![], None, None),
        // 22
        tool_recipe("Stone Axe", vec![(stone, 2), (wood, 1)], axe,
            ItemMaterial::Stone, 40, 5, FLINT_KNAPPING, vec![], None, None),
        // 23
        tool_recipe("Stone Pick", vec![(stone, 2), (wood, 1)], pick,
            ItemMaterial::Stone, 45, 5, FLINT_KNAPPING, vec![], None, None),
        // 24
        tool_recipe("Stone Hammer", vec![(stone, 2), (wood, 1)], hammer,
            ItemMaterial::Stone, 40, 5, FLINT_KNAPPING, vec![], None, None),
        // 25 — Bone Awl: needs a Knife to work the bone, gated BONE_TOOLS.
        tool_recipe("Bone Awl", vec![(bone, 1), (stone, 1)], awl,
            ItemMaterial::Bone, 25, 4, BONE_TOOLS, vec![cut], None, None),
        // Mesolithic — microlithic stone (Fine quality floor).
        // 26
        tool_recipe("Microlithic Knife", vec![(stone, 1), (wood, 1)], knife,
            ItemMaterial::Stone, 35, 6, MICROLITHIC_TOOLS, vec![], Some(ItemQuality::Fine), None),
        // 27
        tool_recipe("Microlithic Axe", vec![(stone, 2), (wood, 1)], axe,
            ItemMaterial::Stone, 45, 7, MICROLITHIC_TOOLS, vec![], Some(ItemQuality::Fine), None),
        // 28
        tool_recipe("Microlithic Pick", vec![(stone, 2), (wood, 1)], pick,
            ItemMaterial::Stone, 50, 7, MICROLITHIC_TOOLS, vec![], Some(ItemQuality::Fine), None),
        // 29 — Bone Fishing Kit: needs a Knife, gated FISHING.
        tool_recipe("Bone Fishing Kit", vec![(bone, 1), (skin, 1)], fishing_kit,
            ItemMaterial::Bone, 45, 5, FISHING, vec![cut], None, None),
        // Neolithic — polished stone + the first sickle.
        // 30
        tool_recipe("Polished Stone Axe", vec![(stone, 2), (wood, 1), (skin, 1)], axe,
            ItemMaterial::Stone, 50, 8, CROP_CULTIVATION, vec![smith], Some(ItemQuality::Fine), None),
        // 31
        tool_recipe("Polished Stone Hammer", vec![(stone, 2), (wood, 1)], hammer,
            ItemMaterial::Stone, 50, 8, CROP_CULTIVATION, vec![smith], Some(ItemQuality::Fine), None),
        // 32
        tool_recipe("Stone Sickle", vec![(stone, 2), (wood, 1), (skin, 1)], sickle,
            ItemMaterial::Stone, 45, 7, CROP_CULTIVATION, vec![cut], None, None),
        // Chalcolithic — copper working set (needs a stone+ hammer + a
        // copper-tier workbench).
        // 33
        tool_recipe("Copper Knife", vec![(copper, 1), (wood, 1)], knife,
            ItemMaterial::Copper, 40, 8, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // 34
        tool_recipe("Copper Axe", vec![(copper, 2), (wood, 1)], axe,
            ItemMaterial::Copper, 50, 9, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // 35
        tool_recipe("Copper Pick", vec![(copper, 2), (wood, 1)], pick,
            ItemMaterial::Copper, 55, 9, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // 36
        tool_recipe("Copper Hammer", vec![(copper, 2), (wood, 1)], hammer,
            ItemMaterial::Copper, 50, 9, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // 37
        tool_recipe("Copper Sickle", vec![(copper, 1), (wood, 1), (skin, 1)], sickle,
            ItemMaterial::Copper, 48, 8, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // 38
        tool_recipe("Copper Awl", vec![(copper, 1), (skin, 1)], awl,
            ItemMaterial::Copper, 35, 7, COPPER_TOOLS, vec![hammer_stone], None,
            Some(WorkbenchTier::Copper)),
        // Bronze Age — bronze working set (copper + tin, copper+ hammer,
        // a bronze-tier workbench).
        // 39
        tool_recipe("Bronze Knife", vec![(copper, 1), (tin, 1), (wood, 1)], knife,
            ItemMaterial::Bronze, 45, 10, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 40
        tool_recipe("Bronze Axe", vec![(copper, 2), (tin, 1), (wood, 1)], axe,
            ItemMaterial::Bronze, 55, 11, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 41
        tool_recipe("Bronze Pick", vec![(copper, 2), (tin, 1), (wood, 1)], pick,
            ItemMaterial::Bronze, 60, 11, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 42
        tool_recipe("Bronze Hammer", vec![(copper, 2), (tin, 1), (wood, 1)], hammer,
            ItemMaterial::Bronze, 55, 11, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 43
        tool_recipe("Bronze Sickle", vec![(copper, 1), (tin, 1), (wood, 1), (skin, 1)], sickle,
            ItemMaterial::Bronze, 52, 10, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 44
        tool_recipe("Bronze Awl", vec![(copper, 1), (tin, 1), (skin, 1)], awl,
            ItemMaterial::Bronze, 40, 9, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 45
        tool_recipe("Bronze Fishing Kit", vec![(copper, 1), (tin, 1), (skin, 1)], fishing_kit,
            ItemMaterial::Bronze, 50, 10, BRONZE_TOOLS, vec![hammer_copper], None,
            Some(WorkbenchTier::Bronze)),
        // 46 — Phase F: Burn quarried limestone into lime in a wood-fired
        // kiln. `FIRED_POTTERY`-gated (the same kiln Pottery uses); Workbench-
        // bound at the cadence of pottery cycles. 2 limestone + 1 wood (the
        // wood is fuel, not structural) → 1 lime. Recipe is the gateway to
        // every mortar-class technique (Cut Stone Masonry, Hydraulic Masonry,
        // Lime Plaster).
        CraftRecipe {
            name: "Burn Lime",
            inputs: vec![(limestone, 2), (wood, 1)],
            output_resource: lime,
            output_qty: 1,
            output_material: None,
            work_ticks: 60,
            crafting_xp: 4,
            tech_gate: Some(FIRED_POTTERY),
            requires_station: Some(StationKind::Workbench),
            tool_requirements: vec![],
            quality_floor: None,
            min_station_tier: None,
        },
    ]
}

/// Borrow the recipe table. Lazy-initialised on first call (the
/// `OnceLock` ensures the heavy `Vec` allocation happens once per
/// process). Recipe ids are stable and match the indices in
/// `build_craft_recipes` — preserving the legacy `recipe_id: u8`
/// numbering used by `CraftOrder` and the chief job-posting system.
pub fn craft_recipes() -> &'static [CraftRecipe] {
    static RECIPES: OnceLock<Vec<CraftRecipe>> = OnceLock::new();
    RECIPES.get_or_init(build_craft_recipes).as_slice()
}

/// Recipe ids for the two written-knowledge artefacts. Used by chief-posting
/// and player-encode paths to know which crafts need a `tech_payload` set.
pub const RECIPE_CLAY_TABLET: u8 = 10;
pub const RECIPE_BOOK: u8 = 11;
#[inline]
pub fn recipe_encodes_knowledge(recipe_id: u8) -> bool {
    recipe_id == RECIPE_CLAY_TABLET || recipe_id == RECIPE_BOOK
}

/// Maximum distinct ingredient types per craft recipe. Three is plenty for
/// every recipe in `craft_recipes()`.
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
        let recipe = craft_recipes().get(recipe_id as usize)?;
        let mut deposits = [GoodNeed::default(); MAX_CRAFT_INPUTS];
        let count = recipe.inputs.len().min(MAX_CRAFT_INPUTS);
        for (i, &(id, qty)) in recipe.inputs.iter().take(count).enumerate() {
            deposits[i] = GoodNeed {
                resource_id: id,
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
    workbench_query: Query<&crate::simulation::construction::Workbench>,
    agent_query: Query<(&FactionMember, &EconomicAgent, &LodLevel)>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    // Timeout sweep: despawn orders older than `CRAFT_ORDER_TIMEOUT_TICKS`
    // (stuck on materials / station / no worker) and prune map entries whose
    // entity has already gone away. Workers attached to a despawned order will
    // self-clear next tick — `craft_order_system` resets `aq.current_task_kind()` when
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

        // Sum faction inventory across living members. Phase 2d:
        // ResourceId-keyed to match `storage.totals` so the union check
        // below can drop its reverse-resolve.
        let mut faction_inv: AHashMap<ResourceId, u32> = AHashMap::new();
        for (member, agent, lod) in agent_query.iter() {
            if *lod == LodLevel::Dormant || member.faction_id != faction_id {
                continue;
            }
            for (item, qty) in agent.inventory.iter() {
                if *qty > 0 {
                    let id = item.resource_id;
                    *faction_inv.entry(id).or_insert(0) =
                        faction_inv.entry(id).or_insert(0).saturating_add(*qty);
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
            let Some(recipe_def) = craft_recipes().get(recipe as usize) else {
                continue;
            };
            // Tech gate.
            if let Some(tech) = recipe_def.tech_gate {
                if !faction.techs.has(tech) {
                    continue;
                }
            }
            // Union-availability for every ingredient. Phase 2d: both
            // faction_inv and storage.totals are ResourceId-keyed, so
            // the recipe's input id indexes them directly.
            let mut ok = true;
            for &(id, qty) in recipe_def.inputs.iter() {
                let in_inv = faction_inv.get(&id).copied().unwrap_or(0);
                let in_store = faction.storage.totals.get(&id).copied().unwrap_or(0);
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
                        // Realistic Tool Overhaul: copper/bronze recipes need
                        // a workbench of at least that tier.
                        .filter(|(_, &e)| match recipe_def.min_station_tier {
                            Some(min) => workbench_query
                                .get(e)
                                .map(|wb| wb.tier.rank() >= min.rank())
                                .unwrap_or(true),
                            None => true,
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
            let Some(order) = CraftOrder::new(
                faction_id,
                recipe,
                workbench,
                anchor,
                clock.tick,
                tech_payload,
            ) else {
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
    storage_tile_map: Res<crate::simulation::faction::StorageTileMap>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_connectivity: Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    mut order_query: Query<&mut CraftOrder>,
    member_query: Query<&FactionMember>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &mut Skills,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
        &Transform,
        Option<&JobClaim>,
        Option<&crate::simulation::apprenticeship::ApprenticeOf>,
        Option<&crate::simulation::tools::ToolKit>,
    )>,
) {
    let mut order_haulers: AHashMap<Entity, Vec<(Entity, [u32; MAX_CRAFT_INPUTS])>> =
        AHashMap::new();
    // Realistic Tool Overhaul: each worker carries a per-tick work-speed
    // multiplier derived from the tier of the tools they hold.
    let mut order_workers: AHashMap<Entity, Vec<(Entity, f32)>> = AHashMap::new();

    for (
        entity,
        mut ai,
        mut aq,
        agent,
        carrier,
        _skills,
        _member,
        slot,
        lod,
        _transform,
        _claim,
        _apprentice,
        toolkit,
    ) in agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        let task = aq.current_task_kind();
        let is_hauler = task == TaskKind::HaulToCraftOrder as u16;
        let is_worker = task == TaskKind::WorkOnCraftOrder as u16;
        if !is_hauler && !is_worker {
            continue;
        }
        let Some(order_entity) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            // Phase 5e-xi-a: drain typed channel for HTN-driven HaulToCraftOrder
            // chains. Legacy plan-driven flow leaves `aq.current = Idle`, so
            // this is a benign no-op there.
            aq.advance();
            continue;
        };
        let Ok(order) = order_query.get(order_entity) else {
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            ai.target_entity = None;
            aq.advance();
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
                    let id = order.deposits[i].resource_id;
                    let in_hand = carrier.quantity_of_resource(id);
                    let in_inv = agent.quantity_of_resource(id);
                    snap[i] = in_hand.saturating_add(in_inv);
                    if snap[i] > 0 {
                        useful = true;
                    }
                }
            }
            if !useful {
                // Hauler has nothing the order still needs — drop the chain.
                // Mirrors the paired `aq.advance()` at lines 649/656 above.
                aq.cancel_chain(&mut ai);
                ai.target_entity = None;
                continue;
            }
            order_haulers
                .entry(order_entity)
                .or_default()
                .push((entity, snap));
        } else {
            // Realistic Tool Overhaul: a worker must hold every tool the
            // recipe requires (in their `ToolKit`) before they can advance
            // the order. A stale plan that lost its tool cancels here so it
            // can't silently spin.
            let speed = if let Some(recipe) = craft_recipes().get(order.recipe_id as usize) {
                if !recipe.tool_requirements.is_empty() {
                    // A worker with no `ToolKit` at all predates the tool
                    // system (or is a fixture agent) — treat as satisfied so
                    // the gate degrades gracefully; an *empty* kit blocks.
                    let satisfied = recipe
                        .tool_requirements
                        .iter()
                        .all(|req| toolkit.map(|tk| tk.satisfies(req)).unwrap_or(true));
                    if !satisfied {
                        aq.cancel_chain(&mut ai);
                        ai.target_entity = None;
                        continue;
                    }
                }
                craft_tool_speed(recipe, toolkit)
            } else {
                1.0
            };
            order_workers
                .entry(order_entity)
                .or_default()
                .push((entity, speed));
        }
    }

    if order_haulers.is_empty() && order_workers.is_empty() {
        return;
    }

    let mut completed_agents: Vec<Entity> = Vec::new();
    let mut hauler_done: Vec<Entity> = Vec::new();
    let mut orphaned_agents: Vec<Entity> = Vec::new();
    let mut xp_grants: Vec<Entity> = Vec::new();
    // Workers who advanced an unfinished order. Pass 3 yields them after a
    // bounded slice so maintenance can run without falsely promoting the
    // queued deposit leg.
    let mut slice_candidates: Vec<Entity> = Vec::new();
    // (agent_entity, resource_id, qty_to_remove)
    let mut good_removals: Vec<(Entity, crate::economy::resource_catalog::ResourceId, u32)> =
        Vec::new();
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
                orphaned_agents.extend(workers.iter().map(|(e, _)| *e));
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
                    good_removals.push((agent_e, need.resource_id, take));
                    order.deposits[i].deposited =
                        order.deposits[i].deposited.saturating_add(take as u8);
                }
                hauler_done.push(agent_e);
            }
        }

        // 2. Advance work once satisfied.
        let Some(recipe) = craft_recipes().get(order.recipe_id as usize) else {
            // Unknown recipe — clean up.
            if let Some(workers) = order_workers.get(&order_entity) {
                orphaned_agents.extend(workers.iter().map(|(e, _)| *e));
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
                // Realistic Tool Overhaul: each worker advances the order by
                // their tool-tier work-speed multiplier (≥1 tick total so a
                // crew never stalls).
                let advance = workers
                    .iter()
                    .map(|(_, s)| *s)
                    .sum::<f32>()
                    .round()
                    .max(1.0) as u8;
                order.work_progress = order
                    .work_progress
                    .saturating_add(advance)
                    .min(recipe.work_ticks);
                xp_grants.extend(workers.iter().map(|(e, _)| *e));
                if order.work_progress < recipe.work_ticks {
                    slice_candidates.extend(workers.iter().map(|(e, _)| *e));
                }
            }
        }

        if order.work_progress >= recipe.work_ticks && order.is_satisfied() {
            // Pick a "lead" worker to receive the output. The first registered
            // worker is fine — output is intentionally portable, not anchored
            // to a specific tile.
            let lead = order_workers
                .get(&order_entity)
                .and_then(|v| v.first().map(|(e, _)| *e));
            if let Some(lead_e) = lead {
                output_grants.push((lead_e, order.recipe_id, order.tech_payload));
                order_completion_credits.push((lead_e, order.recipe_id, recipe.output_qty));
                let faction_id = member_query.get(lead_e).map(|m| m.faction_id).unwrap_or(0);
                activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                    tick: clock.tick,
                    actor: lead_e,
                    faction_id,
                    kind: crate::ui::activity_log::ActivityEntryKind::Crafted { name: recipe.name },
                });
            }

            order_map.0.remove(&order.anchor_tile);
            commands.entity(order_entity).despawn_recursive();

            if let Some(workers) = order_workers.get(&order_entity) {
                completed_agents.extend(workers.iter().map(|(e, _)| *e));
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

    for (
        entity,
        mut ai,
        mut aq,
        mut agent,
        mut carrier,
        mut skills,
        member,
        _slot,
        _lod,
        transform,
        claim,
        apprentice,
        _toolkit,
    ) in agent_query.iter_mut()
    {
        for &(ae, id, qty) in &good_removals {
            if ae == entity {
                let from_hand = carrier.remove_resource(id, qty);
                let still = qty - from_hand;
                if still > 0 {
                    agent.remove_resource(id, still);
                }
            }
        }

        if xp_grants.contains(&entity) {
            // Crafting XP is granted at recipe-completion (per craft); per-tick
            // workers also get a small Crafting XP nudge so labor is rewarded.
            // Phase 5b: apprentices earn at `APPRENTICE_XP_MULT` rate while a
            // mentor is supervising — deliberate-practice multiplier.
            let xp = crate::simulation::apprenticeship::xp_with_apprentice_bonus(1, apprentice);
            skills.gain_xp(SkillKind::Crafting, xp);
        }

        // Output payout & job credit for the lead worker on completed orders.
        for &(ae, recipe_id, tech_payload) in &output_grants {
            if ae != entity {
                continue;
            }
            let Some(recipe) = craft_recipes().get(recipe_id as usize) else {
                continue;
            };
            // Realistic Tool Overhaul: microlithic / polished recipes floor
            // the output quality so a low-skill crafter still hits the tier.
            let quality =
                recipe.effective_quality(quality_for_skill(skills.get(SkillKind::Crafting)));
            let mut output_item = if let Some(mat) = recipe.output_material {
                Item::new_manufactured(recipe.output_resource, mat, quality)
            } else {
                Item::new_commodity(recipe.output_resource)
            };
            output_item.display_name = Some(recipe.name);
            // Stamp tech payload onto Clay Tablet / Book outputs. Equality
            // partitions tablets-of-tech-A from tablets-of-tech-B in
            // inventories and ground-item piles.
            output_item.tech_payload = tech_payload;
            agent.add_item(output_item, recipe.output_qty);
            // Phase 5b: deliberate-practice multiplier for apprentices.
            let xp = crate::simulation::apprenticeship::xp_with_apprentice_bonus(
                recipe.crafting_xp,
                apprentice,
            );
            skills.gain_xp(SkillKind::Crafting, xp);
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
                    .map(|p| {
                        matches!(
                            p.progress,
                            JobProgress::Crafting { recipe, .. } if recipe == recipe_id
                        )
                    })
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
            ai.state = AiState::Idle;
            ai.target_entity = None;
            ai.work_progress = 0;
            // Phase 5e-xi-a: drain the typed channel so HTN-driven
            // HaulToCraftOrder chains complete cleanly. Legacy plan-driven
            // flows leave `aq.current = Idle`, so this is a benign no-op there.
            aq.advance();

            // Phase 5e-xi-b: chain handoff for the WorkOnCraftOrder method.
            // After `aq.advance()` promotes the queued
            // `Task::DepositToFactionStorage`, route the lead worker to the
            // nearest faction storage tile and prime
            // `task_id = TaskKind::DepositResource` so
            // `drop_items_at_destination_system` (Economy) deposits the
            // crafted output goods from inventory on arrival. Hauler chains
            // (5e-xi-a) end at HaulToCraftOrder with no trailing deposit, so
            // this branch only fires for completed workers.
            if matches!(
                aq.current,
                crate::simulation::typed_task::Task::DepositToFactionStorage { .. }
            ) {
                use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
                use crate::world::terrain::TILE_SIZE;
                let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                let cur_chunk = ChunkCoord(
                    cur_tx.div_euclid(CHUNK_SIZE as i32),
                    cur_ty.div_euclid(CHUNK_SIZE as i32),
                );
                if let Some(storage_tile) =
                    storage_tile_map.nearest_for_faction(member.faction_id, (cur_tx, cur_ty))
                {
                    let dispatched = crate::simulation::tasks::assign_task_with_routing(
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
                        aq.cancel();
                    }
                } else {
                    // No faction storage — drop the chain. Output stays in
                    // inventory until something else evicts it (matches the
                    // legacy plan's silent degradation when storage is
                    // unreachable).
                    aq.cancel();
                }
            }
        } else if slice_candidates.contains(&entity)
            && ai.work_progress >= MAINTENANCE_WORK_SLICE_TICKS
        {
            yield_for_maintenance_boundary(&mut ai, &mut aq);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::core_ids;

    /// Pin: the recipe table resolves to the expected `ResourceId`s for
    /// every input and output. Catches drift between `core.ron` keys
    /// and the recipe definitions in `build_craft_recipes`.
    #[test]
    fn craft_recipe_inputs_resolve_to_known_resources() {
        let recipes = craft_recipes();
        assert_eq!(
            recipes.len(),
            47,
            "expected 47 recipes (21 legacy + 25 Realistic Tool Overhaul + 1 Phase F Burn Lime); \
             counts feed CraftOrder.recipe_id wire format"
        );

        // Stone Tools (recipe 0): Stone×2 + Wood×1 → Tools×1
        let stone_tools = &recipes[0];
        assert_eq!(stone_tools.name, "Stone Tools");
        assert_eq!(
            stone_tools.inputs,
            vec![(core_ids::stone(), 2), (core_ids::wood(), 1),]
        );
        assert_eq!(stone_tools.output_resource, core_ids::tools());
        assert_eq!(stone_tools.output_qty, 1);

        // Book (recipe 11): Cloth×2 + Skin×1 → Book×1, gated on writing.
        let book = &recipes[11];
        assert_eq!(book.name, "Book");
        assert_eq!(
            book.inputs,
            vec![(core_ids::cloth(), 2), (core_ids::skin(), 1),]
        );
        assert_eq!(book.output_resource, core_ids::book());

        // Ard Plow (recipe 14): Wood×4 → ard_plow×1, gated on ARD_PLOW.
        // Realistic Tool Overhaul dropped the `tools` ingredient in favour
        // of a held-tool requirement (`Shape`).
        let ard_plow = &recipes[14];
        assert_eq!(ard_plow.name, "Ard Plow");
        assert_eq!(ard_plow.inputs, vec![(core_ids::wood(), 4)]);
        assert!(!ard_plow.tool_requirements.is_empty());
        assert_eq!(ard_plow.output_resource, core_ids::ard_plow());
        assert_eq!(ard_plow.output_qty, 1);
        assert_eq!(
            ard_plow.tech_gate,
            Some(crate::simulation::technology::ARD_PLOW)
        );

        // Cart parts (recipes 15-18): each resolves to its catalog resource.
        assert_eq!(recipes[15].name, "Cart Wheel (Wood)");
        assert_eq!(recipes[15].output_resource, core_ids::cart_wheel_wood());
        assert_eq!(recipes[16].name, "Cart Wheel (Iron-Rimmed)");
        assert_eq!(recipes[16].output_resource, core_ids::cart_wheel_ironrim());
        assert_eq!(recipes[16].inputs.len(), 2);
        assert_eq!(recipes[17].name, "Cart Frame (Small)");
        assert_eq!(recipes[17].output_resource, core_ids::cart_frame_small());
        assert_eq!(recipes[18].name, "Cart Frame (Medium)");
        assert_eq!(recipes[18].output_resource, core_ids::cart_frame_medium());
        assert_eq!(
            recipes[18].tech_gate,
            Some(crate::simulation::technology::OX_CART)
        );
    }

    /// Realistic Tool Overhaul: the 25 appended tool recipes resolve to the
    /// tool catalog resources, gate on the right tech, and the bronze set
    /// carries a copper-hammer requirement + bronze-workbench tier.
    #[test]
    fn tool_recipes_are_well_formed() {
        use crate::economy::core_ids;
        use crate::simulation::tools::{ToolForm, ToolTier};
        let recipes = craft_recipes();

        // Stone Knife (21) — bootstrap-safe: no tool requirement.
        let stone_knife = &recipes[21];
        assert_eq!(stone_knife.name, "Stone Knife");
        assert_eq!(stone_knife.output_resource, core_ids::knife());
        assert!(stone_knife.tool_requirements.is_empty());

        // Microlithic Knife (26) — Fine quality floor.
        let micro = &recipes[26];
        assert_eq!(micro.name, "Microlithic Knife");
        assert_eq!(micro.quality_floor, Some(ItemQuality::Fine));

        // Bronze Axe (40) — copper+tin output, copper-hammer floor,
        // bronze-tier workbench.
        let bronze_axe = &recipes[40];
        assert_eq!(bronze_axe.name, "Bronze Axe");
        assert_eq!(bronze_axe.output_resource, core_ids::axe());
        assert_eq!(bronze_axe.output_material, Some(ItemMaterial::Bronze));
        assert_eq!(bronze_axe.min_station_tier, Some(WorkbenchTier::Bronze));
        assert_eq!(bronze_axe.tool_requirements.len(), 1);
        assert_eq!(bronze_axe.tool_requirements[0].min_tier, ToolTier::Copper);

        // Every appended tool recipe (21..46) outputs a known tool form.
        // Recipes after 45 are Phase F construction-material entries (Burn
        // Lime, etc.) — not tools, skip.
        for r in &recipes[21..46] {
            assert!(
                ToolForm::from_resource_id(r.output_resource).is_some(),
                "recipe {} should output a tool form",
                r.name
            );
        }
    }

    /// Phase F (knowledge-system overhaul): Burn Lime resolves to recipe 46
    /// with the expected inputs/output/gate. Locking this contract because
    /// future construction-technique recipes will compose against it.
    #[test]
    fn burn_lime_recipe_phase_f() {
        let recipes = craft_recipes();
        let burn = &recipes[46];
        assert_eq!(burn.name, "Burn Lime");
        assert_eq!(
            burn.inputs,
            vec![(core_ids::limestone(), 2), (core_ids::wood(), 1)]
        );
        assert_eq!(burn.output_resource, core_ids::lime());
        assert_eq!(burn.output_qty, 1);
        assert_eq!(
            burn.tech_gate,
            Some(crate::simulation::technology::FIRED_POTTERY)
        );
        assert!(matches!(
            burn.requires_station,
            Some(crate::simulation::crafting::StationKind::Workbench)
        ));
    }

    /// Phase F: grain harvest co-yields a thatch bundle alongside grain seeds.
    /// Locking the contract so Phase E's Thatch Roofing technique recipe has
    /// a reliable upstream supply.
    #[test]
    fn grain_harvest_yields_thatch_byproduct_phase_f() {
        use crate::simulation::plants::PlantKind;
        let extras = PlantKind::Grain.harvest_extra_yields();
        let thatch = core_ids::thatch();
        let grain_seed = core_ids::grain_seed();
        assert!(
            extras.iter().any(|(rid, _)| *rid == thatch),
            "Grain.harvest_extra_yields should include thatch"
        );
        assert!(
            extras.iter().any(|(rid, _)| *rid == grain_seed),
            "Grain.harvest_extra_yields should still include grain_seed"
        );
    }

    /// `effective_quality` lifts a low skill result up to the recipe floor
    /// but never lowers a better one.
    #[test]
    fn quality_floor_lifts_low_skill_output() {
        let recipes = craft_recipes();
        let micro = &recipes[26]; // floor = Fine
        assert_eq!(micro.effective_quality(ItemQuality::Poor), ItemQuality::Fine);
        assert_eq!(
            micro.effective_quality(ItemQuality::Masterwork),
            ItemQuality::Masterwork
        );
        let stone_knife = &recipes[21]; // no floor
        assert_eq!(
            stone_knife.effective_quality(ItemQuality::Poor),
            ItemQuality::Poor
        );
    }
}
