use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::faction::{
    FactionChief, FactionData, FactionMember, FactionRegistry, FactionTechs, StorageTileMap, SOLO,
};
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::{
    TechId, BRONZE_CASTING, BRONZE_TOOLS, CITY_STATE_ORG, COPPER_TOOLS, COPPER_WORKING,
    FIRED_POTTERY, FIRE_MAKING, FLINT_KNAPPING, GRANARY, LONG_DIST_TRADE, LOOM_WEAVING,
    MONUMENTAL_BUILDING, PERM_SETTLEMENT, PROFESSIONAL_ARMY, SACRED_RITUAL,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::world::tile::{TileData, TileKind};
use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

/// Safety cap: prevents the blueprint queue growing unbounded due to bugs.
pub const MAX_BLUEPRINTS_SAFETY_CAP: usize = 20;
pub const TICKS_DECONSTRUCT_BED: u8 = 60;

/// Global toggle: when false, agents skip the Build goal entirely.
#[derive(Resource)]
pub struct AutonomousBuildingToggle(pub bool);

/// Maps tile positions to bed entities placed there.
#[derive(Resource, Default)]
pub struct BedMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to wall entities placed there.
#[derive(Resource, Default)]
pub struct WallMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to campfire entities placed there.
#[derive(Resource, Default)]
pub struct CampfireMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to active Blueprint entities (faction build reservations).
#[derive(Resource, Default)]
pub struct BlueprintMap(pub AHashMap<(i16, i16), Entity>);

/// Queue of (faction_id, building_tile, home_tile) tuples populated by
/// `construction_system` when a structure finalises and by the planner when a
/// new road spine is laid out. `road_carve_system` drains it each tick and
/// runs Bresenham from the building tile back to the home tile, marking each
/// passable, non-Wall tile as `TileKind::Road`.
#[derive(Resource, Default)]
pub struct RoadCarveQueue(pub Vec<(u32, (i16, i16), (i16, i16))>);

/// Per-door tracking: stores the door entity and its current open state so
/// `has_los` can query door state by tile without joining a Bevy query.
#[derive(Clone, Copy)]
pub struct DoorEntry {
    pub entity: Entity,
    pub open: bool,
}

/// Maps tile positions to door entries placed there.
#[derive(Resource, Default)]
pub struct DoorMap(pub AHashMap<(i16, i16), DoorEntry>);

/// Maps tile positions to workbench entities placed there.
#[derive(Resource, Default)]
pub struct WorkbenchMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to loom entities placed there.
#[derive(Resource, Default)]
pub struct LoomMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to table entities placed there.
#[derive(Resource, Default)]
pub struct TableMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to chair entities placed there.
#[derive(Resource, Default)]
pub struct ChairMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to granary entities placed there.
#[derive(Resource, Default)]
pub struct GranaryMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to shrine entities placed there.
#[derive(Resource, Default)]
pub struct ShrineMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to market entities placed there.
#[derive(Resource, Default)]
pub struct MarketMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to barracks entities placed there.
#[derive(Resource, Default)]
pub struct BarracksMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to monument entities placed there.
#[derive(Resource, Default)]
pub struct MonumentMap(pub AHashMap<(i16, i16), Entity>);

/// Bundle of furniture/structure maps used by `construction_system`. Bevy caps
/// systems at 16 top-level params; bundling these stays under that limit.
#[derive(bevy::ecs::system::SystemParam)]
pub struct FurnitureMaps<'w> {
    pub bed_map: ResMut<'w, BedMap>,
    pub wall_map: ResMut<'w, WallMap>,
    pub campfire_map: ResMut<'w, CampfireMap>,
    pub door_map: ResMut<'w, DoorMap>,
    pub workbench_map: ResMut<'w, WorkbenchMap>,
    pub loom_map: ResMut<'w, LoomMap>,
    pub table_map: ResMut<'w, TableMap>,
    pub chair_map: ResMut<'w, ChairMap>,
    pub granary_map: ResMut<'w, GranaryMap>,
    pub shrine_map: ResMut<'w, ShrineMap>,
    pub market_map: ResMut<'w, MarketMap>,
    pub barracks_map: ResMut<'w, BarracksMap>,
    pub monument_map: ResMut<'w, MonumentMap>,
}

/// Bed construction tier. Tracks how the bed was built so the upgrade pipeline
/// can replace older tiers when the faction unlocks better tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BedTier {
    /// Pile of leaves / hide. No tech required.
    #[default]
    Crude,
    /// Wood frame, basic carpentry. Gated by `FLINT_KNAPPING`.
    Framed,
    /// Carved wood + textile. Gated by `COPPER_TOOLS`.
    Carved,
}

impl BedTier {
    pub fn label(self) -> &'static str {
        match self {
            BedTier::Crude => "Crude Bed",
            BedTier::Framed => "Framed Bed",
            BedTier::Carved => "Carved Bed",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            BedTier::Crude => 0,
            BedTier::Framed => 1,
            BedTier::Carved => 2,
        }
    }
}

/// Placed on completed bed entities. `owner` is the person who has claimed
/// this bed as theirs; cleared when the owner dies (`death_system`) and
/// reassigned by `assign_beds_system`.
#[derive(Component, Default)]
pub struct Bed {
    pub owner: Option<Entity>,
    pub tier: BedTier,
}

/// Persistent bed claim on a person. Inserted/updated by `assign_beds_system`.
/// `None` means the person has no claim (e.g. faction has no beds yet).
#[derive(Component, Default, Clone, Copy)]
pub struct HomeBed(pub Option<Entity>);

/// Wall construction material. Each tier requires a tech and resource mix;
/// see `BUILD_RECIPES`. All variants render as a `Wall` entity that overwrites
/// the underlying tile with `TileKind::Wall`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallMaterial {
    Palisade,
    WattleDaub,
    Stone,
    Mudbrick,
    CutStone,
}

impl WallMaterial {
    pub const ALL: [WallMaterial; 5] = [
        WallMaterial::Palisade,
        WallMaterial::WattleDaub,
        WallMaterial::Stone,
        WallMaterial::Mudbrick,
        WallMaterial::CutStone,
    ];

    pub fn label(self) -> &'static str {
        match self {
            WallMaterial::Palisade => "Palisade",
            WallMaterial::WattleDaub => "Wattle & Daub",
            WallMaterial::Stone => "Stone Wall",
            WallMaterial::Mudbrick => "Mudbrick",
            WallMaterial::CutStone => "Cut Stone",
        }
    }
}

/// Marker placed on completed wall entities.
#[derive(Component)]
pub struct Wall {
    pub material: WallMaterial,
}

/// Hearth tier. Open campfire vs. stone-lined hearth (better food yield once
/// fired pottery is known).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HearthTier {
    /// Bare campfire. Default.
    #[default]
    Open,
    /// Stone-ringed hearth. Gated by `FLINT_KNAPPING`.
    Ringed,
    /// Lined with fired clay. Gated by `FIRED_POTTERY`.
    Lined,
}

impl HearthTier {
    pub fn label(self) -> &'static str {
        match self {
            HearthTier::Open => "Campfire",
            HearthTier::Ringed => "Ringed Hearth",
            HearthTier::Lined => "Lined Hearth",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            HearthTier::Open => 0,
            HearthTier::Ringed => 1,
            HearthTier::Lined => 2,
        }
    }
}

/// Marker placed on completed campfire entities.
#[derive(Component, Default)]
pub struct Campfire {
    pub tier: HearthTier,
}

/// Door tier. Wooden plank → reinforced for the citadel-style cultures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DoorTier {
    /// Lashed wood. Default.
    #[default]
    Wood,
    /// Plank door — needs `FLINT_KNAPPING`.
    Plank,
    /// Reinforced with metal — needs `BRONZE_TOOLS`.
    Reinforced,
}

impl DoorTier {
    pub fn label(self) -> &'static str {
        match self {
            DoorTier::Wood => "Wood Door",
            DoorTier::Plank => "Plank Door",
            DoorTier::Reinforced => "Reinforced Door",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            DoorTier::Wood => 0,
            DoorTier::Plank => 1,
            DoorTier::Reinforced => 2,
        }
    }
}

/// A door — passable to all agents (faction-gating is TODO), and blocks line of
/// sight when `open == false`. The `faction_id` is recorded for future use
/// when pathfinding gains faction context.
#[derive(Component)]
pub struct Door {
    pub faction_id: u32,
    pub open: bool,
    pub tier: DoorTier,
}

/// Workbench tier. Stone tools → Copper smithing → Bronze casting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkbenchTier {
    /// Flint / stone tools. Default.
    #[default]
    Stone,
    /// Copper smithing — needs `COPPER_WORKING`.
    Copper,
    /// Bronze casting — needs `BRONZE_CASTING`.
    Bronze,
}

impl WorkbenchTier {
    pub fn label(self) -> &'static str {
        match self {
            WorkbenchTier::Stone => "Stone Workbench",
            WorkbenchTier::Copper => "Copper Workbench",
            WorkbenchTier::Bronze => "Bronze Workbench",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            WorkbenchTier::Stone => 0,
            WorkbenchTier::Copper => 1,
            WorkbenchTier::Bronze => 2,
        }
    }
}

/// Workbench — crafting station required for Stone Tools, Iron Tools, Iron Sword.
#[derive(Component)]
pub struct Workbench {
    pub faction_id: u32,
    pub tier: WorkbenchTier,
}

/// Loom — crafting station required for Woven Cloth.
#[derive(Component)]
pub struct Loom {
    pub faction_id: u32,
}

/// Table — boosts social need recovery when an agent is socializing nearby.
#[derive(Component)]
pub struct Table;

/// Chair — pairs with a Table to give the social bonus.
#[derive(Component)]
pub struct Chair;

/// Granary — slows grain decay and boosts effective food storage. Gated by
/// `GRANARY` tech.
#[derive(Component)]
pub struct Granary {
    pub faction_id: u32,
}

/// Shrine — radiates a small mood/social bonus to nearby agents and serves as
/// a focal point for rituals. Gated by `SACRED_RITUAL` tech.
#[derive(Component)]
pub struct Shrine {
    pub faction_id: u32,
}

/// Market — hub for long-distance trade. Gated by `LONG_DIST_TRADE`.
#[derive(Component)]
pub struct Market {
    pub faction_id: u32,
}

/// Barracks — boosts adjacent agents' Combat XP. Gated by `PROFESSIONAL_ARMY`.
#[derive(Component)]
pub struct Barracks {
    pub faction_id: u32,
}

/// Monument — a focal point for rituals and a prestige marker. Gated by
/// `MONUMENTAL_BUILDING`.
#[derive(Component)]
pub struct Monument {
    pub faction_id: u32,
}

/// What kind of structure is being built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSiteKind {
    Wall(WallMaterial),
    Door,
    Bed,
    Campfire,
    Workbench,
    Loom,
    Table,
    Chair,
    Granary,
    Shrine,
    Market,
    Barracks,
    Monument,
}

/// A single ingredient slot inside a `Blueprint`. `needed` is fixed at spawn
/// from the recipe; `deposited` advances as workers contribute the matching good.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoodNeed {
    pub good: Good,
    pub needed: u8,
    pub deposited: u8,
}

/// Maximum distinct ingredient types per build recipe. Three is plenty for
/// every recipe in `BUILD_RECIPES` and avoids heap allocation per blueprint.
pub const MAX_BUILD_INPUTS: usize = 3;

/// A construction site. Agents converge on Blueprint entities to deposit
/// ingredients (wood, stone, grain, …) and contribute build progress.
/// Despawned when construction completes.
/// `personal_owner`: if Some, only that agent builds this (personal commission);
/// if None, any faction member with matching `faction_id` may contribute.
#[derive(Component)]
pub struct Blueprint {
    pub faction_id: u32,
    pub personal_owner: Option<Entity>,
    pub kind: BuildSiteKind,
    pub tile: (i16, i16),
    /// Z-level at which the placed structure should sit. All blueprints
    /// belonging to one building share this value so the walls form a
    /// coherent floor instead of scattering across per-tile surface_z.
    pub target_z: i8,
    pub deposits: [GoodNeed; MAX_BUILD_INPUTS],
    pub deposit_count: u8,
    pub build_progress: u8,
}

impl Blueprint {
    /// Build a blueprint pre-filled from `recipe_for(kind)`.
    pub fn new(
        faction_id: u32,
        personal_owner: Option<Entity>,
        kind: BuildSiteKind,
        tile: (i16, i16),
        target_z: i8,
    ) -> Self {
        let recipe = recipe_for(kind);
        let mut deposits = [GoodNeed::default(); MAX_BUILD_INPUTS];
        let count = recipe.inputs.len().min(MAX_BUILD_INPUTS);
        for (i, &(good, qty)) in recipe.inputs.iter().take(count).enumerate() {
            deposits[i] = GoodNeed {
                good,
                needed: qty,
                deposited: 0,
            };
        }
        Self {
            faction_id,
            personal_owner,
            kind,
            tile,
            target_z,
            deposits,
            deposit_count: count as u8,
            build_progress: 0,
        }
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

// ── Build recipes ─────────────────────────────────────────────────────────────

/// Static description of how to build a single structure kind: ingredients,
/// labour ticks, optional tech gate, and what is refunded on deconstruction.
pub struct BuildRecipe {
    pub name: &'static str,
    pub inputs: &'static [(Good, u8)],
    pub work_ticks: u8,
    pub tech_gate: Option<TechId>,
    pub deconstruct_refund: &'static [(Good, u8)],
}

const RECIPE_PALISADE: BuildRecipe = BuildRecipe {
    name: "Palisade Wall",
    inputs: &[(Good::Wood, 2)],
    work_ticks: 60,
    tech_gate: None,
    deconstruct_refund: &[(Good::Wood, 1)],
};
const RECIPE_WATTLE_DAUB: BuildRecipe = BuildRecipe {
    name: "Wattle & Daub Wall",
    inputs: &[(Good::Wood, 2), (Good::Grain, 1)],
    work_ticks: 70,
    tech_gate: Some(PERM_SETTLEMENT),
    deconstruct_refund: &[(Good::Wood, 1)],
};
const RECIPE_STONE_WALL: BuildRecipe = BuildRecipe {
    name: "Stone Wall",
    inputs: &[(Good::Stone, 3)],
    work_ticks: 90,
    tech_gate: Some(FLINT_KNAPPING),
    deconstruct_refund: &[(Good::Stone, 2)],
};
const RECIPE_MUDBRICK: BuildRecipe = BuildRecipe {
    name: "Mudbrick Wall",
    inputs: &[(Good::Stone, 2), (Good::Wood, 1)],
    work_ticks: 80,
    tech_gate: Some(FIRED_POTTERY),
    deconstruct_refund: &[(Good::Stone, 1)],
};
const RECIPE_CUT_STONE: BuildRecipe = BuildRecipe {
    name: "Cut Stone Wall",
    inputs: &[(Good::Stone, 4)],
    work_ticks: 120,
    tech_gate: Some(MONUMENTAL_BUILDING),
    deconstruct_refund: &[(Good::Stone, 3)],
};
const RECIPE_WORKBENCH: BuildRecipe = BuildRecipe {
    name: "Workbench",
    inputs: &[(Good::Wood, 3), (Good::Stone, 1)],
    work_ticks: 60,
    tech_gate: Some(FLINT_KNAPPING),
    deconstruct_refund: &[(Good::Wood, 2)],
};
const RECIPE_LOOM: BuildRecipe = BuildRecipe {
    name: "Loom",
    inputs: &[(Good::Wood, 4)],
    work_ticks: 70,
    tech_gate: Some(LOOM_WEAVING),
    deconstruct_refund: &[(Good::Wood, 2)],
};
const RECIPE_TABLE: BuildRecipe = BuildRecipe {
    name: "Table",
    inputs: &[(Good::Wood, 3)],
    work_ticks: 50,
    tech_gate: None,
    deconstruct_refund: &[(Good::Wood, 2)],
};
const RECIPE_CHAIR: BuildRecipe = BuildRecipe {
    name: "Chair",
    inputs: &[(Good::Wood, 2)],
    work_ticks: 40,
    tech_gate: None,
    deconstruct_refund: &[(Good::Wood, 1)],
};
const RECIPE_DOOR: BuildRecipe = BuildRecipe {
    name: "Door",
    inputs: &[(Good::Wood, 2)],
    work_ticks: 50,
    tech_gate: None,
    deconstruct_refund: &[(Good::Wood, 1)],
};
const RECIPE_BED: BuildRecipe = BuildRecipe {
    name: "Bed",
    inputs: &[(Good::Wood, 3)],
    work_ticks: 80,
    tech_gate: None,
    deconstruct_refund: &[(Good::Wood, 2)],
};
const RECIPE_CAMPFIRE: BuildRecipe = BuildRecipe {
    name: "Campfire",
    inputs: &[(Good::Wood, 2)],
    work_ticks: 40,
    tech_gate: Some(FIRE_MAKING),
    deconstruct_refund: &[(Good::Wood, 1)],
};
const RECIPE_GRANARY: BuildRecipe = BuildRecipe {
    name: "Granary",
    inputs: &[(Good::Wood, 4), (Good::Stone, 2)],
    work_ticks: 120,
    tech_gate: Some(GRANARY),
    deconstruct_refund: &[(Good::Wood, 2), (Good::Stone, 1)],
};
const RECIPE_SHRINE: BuildRecipe = BuildRecipe {
    name: "Shrine",
    inputs: &[(Good::Stone, 3), (Good::Wood, 2)],
    work_ticks: 140,
    tech_gate: Some(SACRED_RITUAL),
    deconstruct_refund: &[(Good::Stone, 1), (Good::Wood, 1)],
};
const RECIPE_MARKET: BuildRecipe = BuildRecipe {
    name: "Market",
    inputs: &[(Good::Wood, 5), (Good::Stone, 2)],
    work_ticks: 160,
    tech_gate: Some(LONG_DIST_TRADE),
    deconstruct_refund: &[(Good::Wood, 2), (Good::Stone, 1)],
};
const RECIPE_BARRACKS: BuildRecipe = BuildRecipe {
    name: "Barracks",
    inputs: &[(Good::Wood, 4), (Good::Stone, 3)],
    work_ticks: 180,
    tech_gate: Some(PROFESSIONAL_ARMY),
    deconstruct_refund: &[(Good::Wood, 2), (Good::Stone, 1)],
};
const RECIPE_MONUMENT: BuildRecipe = BuildRecipe {
    name: "Monument",
    inputs: &[(Good::Stone, 6), (Good::Wood, 2)],
    work_ticks: 220,
    tech_gate: Some(MONUMENTAL_BUILDING),
    deconstruct_refund: &[(Good::Stone, 3), (Good::Wood, 1)],
};

/// Most advanced wall material a faction's tech bitset allows. Used by the
/// chief to upgrade defensive walls automatically; the player may still pick
/// any unlocked material via the right-click menu.
pub fn best_wall_material(techs: &FactionTechs) -> WallMaterial {
    if techs.has(MONUMENTAL_BUILDING) {
        WallMaterial::CutStone
    } else if techs.has(FIRED_POTTERY) {
        WallMaterial::Mudbrick
    } else if techs.has(FLINT_KNAPPING) {
        WallMaterial::Stone
    } else if techs.has(PERM_SETTLEMENT) {
        WallMaterial::WattleDaub
    } else {
        WallMaterial::Palisade
    }
}

/// Best bed tier the faction's tech bitset allows.
pub fn best_bed_for(techs: &FactionTechs) -> BedTier {
    if techs.has(COPPER_TOOLS) {
        BedTier::Carved
    } else if techs.has(FLINT_KNAPPING) {
        BedTier::Framed
    } else {
        BedTier::Crude
    }
}

/// Best workbench tier the faction's tech bitset allows.
pub fn best_workbench_for(techs: &FactionTechs) -> WorkbenchTier {
    if techs.has(BRONZE_CASTING) {
        WorkbenchTier::Bronze
    } else if techs.has(COPPER_WORKING) {
        WorkbenchTier::Copper
    } else {
        WorkbenchTier::Stone
    }
}

/// Best door tier the faction's tech bitset allows.
pub fn best_door_for(techs: &FactionTechs) -> DoorTier {
    if techs.has(BRONZE_TOOLS) {
        DoorTier::Reinforced
    } else if techs.has(FLINT_KNAPPING) {
        DoorTier::Plank
    } else {
        DoorTier::Wood
    }
}

/// Best hearth/campfire tier the faction's tech bitset allows.
pub fn best_hearth_for(techs: &FactionTechs) -> HearthTier {
    if techs.has(FIRED_POTTERY) {
        HearthTier::Lined
    } else if techs.has(FLINT_KNAPPING) {
        HearthTier::Ringed
    } else {
        HearthTier::Open
    }
}

/// True if the faction has the tech needed for this wall material.
pub fn faction_can_build(kind: BuildSiteKind, techs: &FactionTechs) -> bool {
    match recipe_for(kind).tech_gate {
        Some(t) => techs.has(t),
        None => true,
    }
}

/// Returns the static recipe for a given build site kind.
pub fn recipe_for(kind: BuildSiteKind) -> &'static BuildRecipe {
    match kind {
        BuildSiteKind::Wall(WallMaterial::Palisade) => &RECIPE_PALISADE,
        BuildSiteKind::Wall(WallMaterial::WattleDaub) => &RECIPE_WATTLE_DAUB,
        BuildSiteKind::Wall(WallMaterial::Stone) => &RECIPE_STONE_WALL,
        BuildSiteKind::Wall(WallMaterial::Mudbrick) => &RECIPE_MUDBRICK,
        BuildSiteKind::Wall(WallMaterial::CutStone) => &RECIPE_CUT_STONE,
        BuildSiteKind::Door => &RECIPE_DOOR,
        BuildSiteKind::Bed => &RECIPE_BED,
        BuildSiteKind::Campfire => &RECIPE_CAMPFIRE,
        BuildSiteKind::Workbench => &RECIPE_WORKBENCH,
        BuildSiteKind::Loom => &RECIPE_LOOM,
        BuildSiteKind::Table => &RECIPE_TABLE,
        BuildSiteKind::Chair => &RECIPE_CHAIR,
        BuildSiteKind::Granary => &RECIPE_GRANARY,
        BuildSiteKind::Shrine => &RECIPE_SHRINE,
        BuildSiteKind::Market => &RECIPE_MARKET,
        BuildSiteKind::Barracks => &RECIPE_BARRACKS,
        BuildSiteKind::Monument => &RECIPE_MONUMENT,
    }
}

/// Count how many of the 4 cardinal directions have a wall (or higher-z terrain)
/// within 3 tiles. Score range: 0–4.
pub fn enclosure_score(chunk_map: &ChunkMap, tx: i32, ty: i32) -> u8 {
    let agent_z = chunk_map.surface_z_at(tx, ty);
    let mut score = 0u8;
    for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)] {
        for step in 1..=3i32 {
            let nx = tx + dx * step;
            let ny = ty + dy * step;
            let kind_wall = chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall);
            let z_higher = chunk_map.surface_z_at(nx, ny) > agent_z;
            if kind_wall || z_higher {
                score += 1;
                break;
            }
        }
    }
    score
}

// ── Placement helpers ─────────────────────────────────────────────────────────

fn count_beds_near(bed_map: &BedMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    bed_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_campfires_near(campfire_map: &CampfireMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    campfire_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_walls_near(wall_map: &WallMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    wall_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_workbenches_near(map: &WorkbenchMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_granaries_near(map: &GranaryMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_shrines_near(map: &ShrineMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_markets_near(map: &MarketMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_barracks_near(map: &BarracksMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_monuments_near(map: &MonumentMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

/// Find a clear (2·hw+1) × (2·hh+1) footprint inside the first matching zone
/// of `plan`. Returns the centre. Falls back to `find_building_origin` (radial
/// search around home) when no matching zone exists or the zone is full.
fn find_footprint_in_zone(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    kind: crate::simulation::settlement::ZoneKind,
    home: (i16, i16),
    half_w: i32,
    half_h: i32,
    fallback_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (home.0 as i32, home.1 as i32);

    if let Some(plan) = plan {
        if let Some(rect) = plan.zones.iter().find(|z| z.kind == kind).map(|z| z.rect) {
            // Rank candidates by (spread asc, distance asc). Flat ground is
            // strongly preferred; uneven sites that exceed MAX_TERRAFORM_SPREAD
            // are rejected outright so we don't queue megaprojects.
            let mut best: Option<(u8, i32, (i16, i16))> = None;
            let cx_min = rect.x0 as i32 + half_w;
            let cy_min = rect.y0 as i32 + half_h;
            let cx_max = rect.x0 as i32 + rect.w as i32 - half_w - 1;
            let cy_max = rect.y0 as i32 + rect.h as i32 - half_h - 1;
            for cy in cy_min..=cy_max {
                for cx in cx_min..=cx_max {
                    if !is_clear_footprint(chunk_map, bed_map, bp_map, cx, cy, half_w, half_h) {
                        continue;
                    }
                    if blocks_cardinal_corridor(cx, cy, half_w, half_h, home) {
                        continue;
                    }
                    let (_, spread) = footprint_z_stats(chunk_map, cx, cy, half_w, half_h);
                    if spread > MAX_TERRAFORM_SPREAD {
                        continue;
                    }
                    let d = (cx - hx).abs() + (cy - hy).abs();
                    let cand = (spread, d, (cx as i16, cy as i16));
                    if best.map(|b| (cand.0, cand.1) < (b.0, b.1)).unwrap_or(true) {
                        best = Some(cand);
                    }
                }
            }
            if let Some((_, _, p)) = best {
                return Some(p);
            }
        }
    }

    // Fallback: organic radial search.
    find_building_origin(
        chunk_map,
        bed_map,
        bp_map,
        home,
        half_w,
        half_h,
        fallback_radius,
    )
}

/// Pick a clear single-tile site inside the first zone of the given kind in
/// `plan`. Returns the tile closest to `home`. Falls back to a radial search
/// around `home` when no zone of the requested kind exists or no clear tile
/// is found within it.
fn find_clear_tile_in_zone(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    kind: crate::simulation::settlement::ZoneKind,
    home: (i16, i16),
    fallback_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (home.0 as i32, home.1 as i32);

    if let Some(plan) = plan {
        if let Some(rect) = plan.zones.iter().find(|z| z.kind == kind).map(|z| z.rect) {
            let mut best: Option<(i32, (i16, i16))> = None;
            for dy in 0..rect.h as i32 {
                for dx in 0..rect.w as i32 {
                    let tx = rect.x0 as i32 + dx;
                    let ty = rect.y0 as i32 + dy;
                    let pos = (tx as i16, ty as i16);
                    if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                        continue;
                    }
                    let Some(k) = chunk_map.tile_kind_at(tx, ty) else {
                        continue;
                    };
                    if !k.is_passable() || k == TileKind::Wall {
                        continue;
                    }
                    let d = (tx - hx).abs() + (ty - hy).abs();
                    if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                        best = Some((d, pos));
                    }
                }
            }
            if let Some((_, p)) = best {
                return Some(p);
            }
        }
    }

    // Fallback radial search.
    for d in 1..=fallback_radius {
        for dy in -d..=d {
            for dx in -d..=d {
                if dx.abs().max(dy.abs()) != d {
                    continue;
                }
                let pos = ((hx + dx) as i16, (hy + dy) as i16);
                if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(hx + dx, hy + dy) else {
                    continue;
                };
                if k.is_passable() && k != TileKind::Wall {
                    return Some(pos);
                }
            }
        }
    }
    None
}

/// Find a passable, unreserved tile in the first Civic zone of `plan` whose
/// rect doesn't already contain a campfire. Lets multi-hearth Paleolithic
/// camps place each new fire at a fresh Civic anchor instead of clustering
/// the second hearth on top of the first. Falls back to a 6-tile radial
/// search around `home` when no plan / no eligible zone is available.
fn find_unfilled_civic_zone_tile(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    campfire_map: &CampfireMap,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    home: (i16, i16),
) -> Option<(i16, i16)> {
    use crate::simulation::settlement::ZoneKind;
    if let Some(plan) = plan {
        for zone in plan.zones.iter().filter(|z| z.kind == ZoneKind::Civic) {
            let rect = zone.rect;
            let occupied = campfire_map.0.keys().any(|&(x, y)| rect.contains(x, y));
            if occupied {
                continue;
            }
            let mut best: Option<(i32, (i16, i16))> = None;
            let zcx = (rect.x0 as i32 + rect.w as i32 / 2);
            let zcy = (rect.y0 as i32 + rect.h as i32 / 2);
            for dy in 0..rect.h as i32 {
                for dx in 0..rect.w as i32 {
                    let tx = rect.x0 as i32 + dx;
                    let ty = rect.y0 as i32 + dy;
                    let pos = (tx as i16, ty as i16);
                    if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                        continue;
                    }
                    let Some(k) = chunk_map.tile_kind_at(tx, ty) else {
                        continue;
                    };
                    if !k.is_passable() || k == TileKind::Wall {
                        continue;
                    }
                    let d = (tx - zcx).abs() + (ty - zcy).abs();
                    if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                        best = Some((d, pos));
                    }
                }
            }
            if let Some((_, p)) = best {
                return Some(p);
            }
        }
    }
    // No plan / no eligible Civic zone — fall back to a tight radial search
    // so the very first hearth still gets placed before the planner runs.
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    for d in 1i32..=6 {
        for dy in -d..=d {
            for dx in -d..=d {
                if dx.abs().max(dy.abs()) != d {
                    continue;
                }
                let pos = ((hx + dx) as i16, (hy + dy) as i16);
                if bp_map.0.contains_key(&pos)
                    || bed_map.0.contains_key(&pos)
                    || campfire_map.0.contains_key(&pos)
                {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(hx + dx, hy + dy) else {
                    continue;
                };
                if k.is_passable() && k != TileKind::Wall {
                    return Some(pos);
                }
            }
        }
    }
    None
}

/// Pick a bed tile inside one of two opposing crescents around a hearth.
/// The crescent axis is perpendicular to the home→hearth direction so beds
/// flank the approach path on either side, leaving the home-facing and
/// far-facing corridors clear. Diagonal corners (≥45° off-axis) are
/// excluded; the chosen tile balances bed counts across the two crescents.
fn find_bed_tile_around_hearth(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    hearths: &[(i16, i16)],
    home: (i16, i16),
    inner_r: i32,
    outer_r: i32,
) -> Option<(i16, i16)> {
    if hearths.is_empty() {
        return None;
    }
    // cos(~44°) — accept tiles within ~44° of either crescent pole. Just
    // tight enough to reject 45° diagonals so the cluster doesn't square off.
    const ALIGNMENT_THRESHOLD: f32 = 0.72;

    // Per-hearth crescent axis (unit vector), perpendicular to the
    // home→hearth direction.
    let bed_axes: Vec<(f32, f32)> = hearths
        .iter()
        .map(|&(hx, hy)| {
            let approach_dx = home.0 as f32 - hx as f32;
            let approach_dy = home.1 as f32 - hy as f32;
            let approach = approach_dy.atan2(approach_dx);
            let axis = approach + std::f32::consts::FRAC_PI_2;
            (axis.cos(), axis.sin())
        })
        .collect();

    // Per-hearth occupancy of the two crescents (positive vs negative pole).
    let crescents_per_hearth: Vec<[u8; 2]> = hearths
        .iter()
        .enumerate()
        .map(|(hi, &(hx, hy))| {
            let (ax, ay) = bed_axes[hi];
            let mut s = [0u8; 2];
            for &(bx, by) in bed_map.0.keys() {
                let dx = bx as i32 - hx as i32;
                let dy = by as i32 - hy as i32;
                let dist2 = dx * dx + dy * dy;
                if dist2 < inner_r * inner_r || dist2 > outer_r * outer_r {
                    continue;
                }
                let r = (dist2 as f32).sqrt();
                let alignment = (dx as f32 * ax + dy as f32 * ay) / r;
                if alignment.abs() < ALIGNMENT_THRESHOLD {
                    continue;
                }
                s[(alignment < 0.0) as usize] += 1;
            }
            s
        })
        .collect();

    let mut best: Option<(i32, (i16, i16))> = None;
    for (hi, &(hx, hy)) in hearths.iter().enumerate() {
        let (ax, ay) = bed_axes[hi];
        let crescents = &crescents_per_hearth[hi];
        let min_side = (*crescents.iter().min().unwrap_or(&0)) as i32;
        for dy in -outer_r..=outer_r {
            for dx in -outer_r..=outer_r {
                let dist2 = dx * dx + dy * dy;
                if dist2 < inner_r * inner_r || dist2 > outer_r * outer_r {
                    continue;
                }
                let tx = hx as i32 + dx;
                let ty = hy as i32 + dy;
                let pos = (tx as i16, ty as i16);
                if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(tx, ty) else {
                    continue;
                };
                if !k.is_passable() || k == TileKind::Wall {
                    continue;
                }
                let r = (dist2 as f32).sqrt();
                let alignment = (dx as f32 * ax + dy as f32 * ay) / r;
                if alignment.abs() < ALIGNMENT_THRESHOLD {
                    continue;
                }
                let side = (alignment < 0.0) as usize;
                let arc_pressure = (crescents[side] as i32 - min_side) * 24;
                let radial_pressure = (dist2 - inner_r * inner_r).max(0);
                let hearth_pressure = (hi as i32) * 4;
                let cost = arc_pressure + radial_pressure + hearth_pressure;
                if best.map(|(c, _)| cost < c).unwrap_or(true) {
                    best = Some((cost, pos));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Maximum surface_z spread (max - min) we are willing to terraform for a
/// single building footprint. Sites above this are rejected entirely so the
/// AI doesn't sink an entire faction into a 10-block earthworks project.
const MAX_TERRAFORM_SPREAD: u8 = 4;

/// Compute (target_z, spread) for the rectangular footprint centred at
/// (cx, cy). target_z is the rounded mean of surface_z across the
/// footprint — every wall/floor of the building will be placed at this
/// height. spread = max - min, used by site selection to penalise hilly
/// candidates.
fn footprint_z_stats(chunk_map: &ChunkMap, cx: i32, cy: i32, half_w: i32, half_h: i32) -> (i8, u8) {
    let mut sum: i32 = 0;
    let mut count: i32 = 0;
    let mut min_z = i32::MAX;
    let mut max_z = i32::MIN;
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let z = chunk_map.surface_z_at(cx + dx, cy + dy);
            sum += z;
            count += 1;
            min_z = min_z.min(z);
            max_z = max_z.max(z);
        }
    }
    let mean = if count > 0 { sum / count } else { 0 };
    let spread = (max_z - min_z).max(0).min(255) as u8;
    (mean.clamp(i8::MIN as i32, i8::MAX as i32) as i8, spread)
}

/// Returns true if every tile in the half_w × half_h footprint centred at (cx,cy)
/// is passable, not a wall, and not reserved by an existing bed or blueprint.
fn is_clear_footprint(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let pos = ((cx + dx) as i16, (cy + dy) as i16);
            if bp_map.0.contains_key(&pos) {
                return false;
            }
            if bed_map.0.contains_key(&pos) {
                return false;
            }
            let Some(kind) = chunk_map.tile_kind_at(cx + dx, cy + dy) else {
                return false;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                return false;
            }
        }
    }
    true
}

/// Returns true if any wall tile or bed exists within `radius` tiles of the
/// expanded bounding box of the footprint — i.e. there is something to attach to.
fn has_nearby_structure(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    radius: i32,
) -> bool {
    let outer_w = half_w + radius;
    let outer_h = half_h + radius;
    for dy in -outer_h..=outer_h {
        for dx in -outer_w..=outer_w {
            if dy.abs() <= half_h && dx.abs() <= half_w {
                continue;
            } // skip own footprint
            let nx = cx + dx;
            let ny = cy + dy;
            if bed_map.0.contains_key(&(nx as i16, ny as i16)) {
                return true;
            }
            if chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall) {
                return true;
            }
        }
    }
    false
}

/// Returns true if the footprint would straddle either cardinal corridor from the
/// faction center — i.e. any tile in the footprint lies on `x = home.x` (N-S axis)
/// or `y = home.y` (E-W axis). These corridors are reserved as future access roads.
fn blocks_cardinal_corridor(cx: i32, cy: i32, half_w: i32, half_h: i32, home: (i16, i16)) -> bool {
    let hx = home.0 as i32;
    let hy = home.1 as i32;
    let blocks_ns = cx - half_w <= hx && hx <= cx + half_w;
    let blocks_ew = cy - half_h <= hy && hy <= cy + half_h;
    blocks_ns || blocks_ew
}

/// Phase 1/2: find the center of a clear (2·half_w+1) × (2·half_h+1) footprint.
/// Returns a site near the camp center for the first few buildings, then expands
/// outward organically (adjacent to existing structures).
fn find_building_origin(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    camp_home: (i16, i16),
    half_w: i32,
    half_h: i32,
    max_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    let min_ring = half_w.max(half_h) + 1;
    let early_ring = min_ring + 3; // within this ring: always accept a clear footprint

    for ring in min_ring..=max_radius {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let cx = hx + dx;
                let cy = hy + dy;
                if blocks_cardinal_corridor(cx, cy, half_w, half_h, camp_home) {
                    continue;
                }
                if !is_clear_footprint(chunk_map, bed_map, bp_map, cx, cy, half_w, half_h) {
                    continue;
                }
                if ring <= early_ring {
                    return Some((cx as i16, cy as i16));
                }
                // Beyond the seeding zone, grow organically: require adjacency.
                if has_nearby_structure(chunk_map, bed_map, cx, cy, half_w, half_h, 2) {
                    return Some((cx as i16, cy as i16));
                }
            }
        }
    }
    None
}

/// Phase 1/2: plan all wall and bed blueprints for a single rectangular building.
/// The perimeter wall tile closest to camp_home becomes the entrance (left open).
/// `wall_material` controls which wall recipe is used for every perimeter tile.
fn plan_building(
    commands: &mut Commands,
    bp_map: &mut BlueprintMap,
    terraform_map: &mut crate::simulation::terraform::TerraformMap,
    pending_footprints: &mut crate::simulation::terraform::PendingFootprints,
    chunk_map: &ChunkMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    faction_id: u32,
    camp_home: (i16, i16),
    interior_beds: &[(i32, i32)],
    wall_material: WallMaterial,
) {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);

    // The perimeter tile whose world-position is closest to the camp center becomes entrance.
    let entrance: (i32, i32) = {
        let mut best = (0i32, half_h);
        let mut best_dist = i64::MAX;
        for dy in -half_h..=half_h {
            for dx in -half_w..=half_w {
                if dx.abs() < half_w && dy.abs() < half_h {
                    continue;
                } // interior
                if dx.abs() == half_w && dy.abs() == half_h {
                    continue;
                } // corner — entrance must be on a flat side
                let d = ((cx + dx - hx) as i64).pow(2) + ((cy + dy - hy) as i64).pow(2);
                if d < best_dist {
                    best_dist = d;
                    best = (dx, dy);
                }
            }
        }
        best
    };

    let (target_z, _spread) = footprint_z_stats(chunk_map, cx, cy, half_w, half_h);

    // Build the wall+bed plan. We always compute it first so the deferred
    // path (footprint_completion_system) can spawn the same blueprints once
    // terraform completes.
    let mut wall_plan: Vec<(BuildSiteKind, (i16, i16))> = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            if dx.abs() < half_w && dy.abs() < half_h {
                continue;
            } // interior — beds go here
            let tile = ((cx + dx) as i16, (cy + dy) as i16);
            let kind = if (dx, dy) == entrance {
                BuildSiteKind::Door
            } else {
                BuildSiteKind::Wall(wall_material)
            };
            wall_plan.push((kind, tile));
        }
    }
    for &(bdx, bdy) in interior_beds {
        let tile = ((cx + bdx) as i16, (cy + bdy) as i16);
        wall_plan.push((BuildSiteKind::Bed, tile));
    }

    // Collect the tiles that need terraforming (footprint covers walls AND
    // interior — every tile under the building must sit at target_z so the
    // floor is level).
    let mut terraform_tiles: Vec<(i16, i16)> = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let tx = cx + dx;
            let ty = cy + dy;
            let surf = chunk_map.surface_z_at(tx, ty);
            if surf as i8 != target_z {
                terraform_tiles.push((tx as i16, ty as i16));
            }
        }
    }

    if terraform_tiles.is_empty() {
        // Flat ground: spawn wall blueprints immediately.
        for (kind, tile) in &wall_plan {
            if bp_map.0.contains_key(tile) {
                continue;
            }
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let e = commands
                .spawn((
                    Blueprint::new(faction_id, None, *kind, *tile, target_z),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(*tile, e);
        }
        return;
    }

    // Uneven ground: spawn TerraformSites and defer wall placement.
    use crate::simulation::terraform::{PendingFootprint, TerraformSite};
    for &tile in &terraform_tiles {
        if terraform_map.0.contains_key(&tile) || bp_map.0.contains_key(&tile) {
            continue;
        }
        let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
        let e = commands
            .spawn((
                TerraformSite {
                    faction_id,
                    target_z,
                },
                Transform::from_xyz(wp.x, wp.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        terraform_map.0.insert(tile, e);
    }
    pending_footprints.queue.push(PendingFootprint {
        faction_id,
        target_z,
        terraform_tiles,
        wall_plan,
    });
}

/// Phase 1/2: find a single open slot on the rectangular palisade that wraps the
/// settlement's bed bounding box plus a buffer. Returns None when the palisade is
/// complete or no beds exist near camp.
fn find_palisade_site(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    camp_home: (i16, i16),
    buffer: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    let search = 25i32;

    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for &pos in bed_map.0.keys() {
        let (bx, by) = (pos.0 as i32, pos.1 as i32);
        if (bx - hx).abs() > search || (by - hy).abs() > search {
            continue;
        }
        min_x = min_x.min(bx);
        max_x = max_x.max(bx);
        min_y = min_y.min(by);
        max_y = max_y.max(by);
    }
    if min_x == i32::MAX {
        return None;
    }

    min_x -= buffer;
    max_x += buffer;
    min_y -= buffer;
    max_y += buffer;

    // Top and bottom rows — leave one-tile gateway at x=hx on each row.
    for x in min_x..=max_x {
        for &y in &[min_y, max_y] {
            if x == hx {
                continue; // N or S gateway: keep open for cardinal access
            }
            let tile = (x as i16, y as i16);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                continue;
            }
            return Some(tile);
        }
    }
    // Left and right columns (excluding corners) — leave one-tile gateway at y=hy.
    for y in (min_y + 1)..max_y {
        for &x in &[min_x, max_x] {
            if y == hy {
                continue; // W or E gateway: keep open for cardinal access
            }
            let tile = (x as i16, y as i16);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                continue;
            }
            return Some(tile);
        }
    }
    None
}

// ── Blueprint planning system ─────────────────────────────────────────────────

/// Read-only bundle of building maps that the chief uses to decide what to
/// build next. Bundled as a `SystemParam` so `chief_directive_system` stays
/// under Bevy's per-system parameter limit.
#[derive(bevy::ecs::system::SystemParam)]
pub struct BuildingMapsRO<'w> {
    pub bed_map: Res<'w, BedMap>,
    pub wall_map: Res<'w, WallMap>,
    pub campfire_map: Res<'w, CampfireMap>,
    pub workbench_map: Res<'w, WorkbenchMap>,
    pub granary_map: Res<'w, GranaryMap>,
    pub shrine_map: Res<'w, ShrineMap>,
    pub market_map: Res<'w, MarketMap>,
    pub barracks_map: Res<'w, BarracksMap>,
    pub monument_map: Res<'w, MonumentMap>,
}

/// One thing the chief is considering building this tick. The selector
/// generates several candidates and picks the one with the highest score.
struct BuildCandidate {
    intent: BuildIntent,
    /// Centre tile for the placement (single-tile target or footprint centre).
    tile: (i16, i16),
    score: f32,
}

#[derive(Clone, Copy)]
enum BuildIntent {
    /// Single-tile blueprint.
    Single(BuildSiteKind),
    /// 1×1 walled hut: 4 wall tiles + 1 door + 1 interior bed.
    Hut(WallMaterial),
    /// 2×1 walled longhouse: 8 perimeter tiles + 1 door + 2 interior beds.
    Longhouse(WallMaterial),
    /// One palisade segment along the settlement perimeter.
    PalisadeSegment(WallMaterial, i32 /*buffer*/),
}

/// Maintains the faction build queue every 60 ticks. One project at a time per
/// faction. The selector scores candidates from several pressure sources
/// (residential demand, defense weakness, hunger, crafting, civic priorities)
/// and picks the highest, modulated by the faction's culture traits.
pub fn chief_directive_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    auto_build: Res<AutonomousBuildingToggle>,
    chunk_map: Res<ChunkMap>,
    faction_registry: Res<FactionRegistry>,
    maps: BuildingMapsRO,
    mut bp_map: ResMut<BlueprintMap>,
    mut terraform_map: ResMut<crate::simulation::terraform::TerraformMap>,
    mut pending_footprints: ResMut<crate::simulation::terraform::PendingFootprints>,
    bp_query: Query<&Blueprint>,
    plans: Res<crate::simulation::settlement::SettlementPlans>,
    chief_query: Query<(&FactionMember, &AgentGoal), With<FactionChief>>,
) {
    if clock.tick % 60 != 0 || !auto_build.0 {
        return;
    }

    let leading_factions: AHashSet<u32> = chief_query
        .iter()
        .filter(|(_, goal)| **goal == AgentGoal::Lead)
        .map(|(m, _)| m.faction_id)
        .collect();

    let mut faction_bp_count: AHashMap<u32, usize> = AHashMap::new();
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            *faction_bp_count.entry(bp.faction_id).or_insert(0) += 1;
        }
    }
    // Pending footprints (mid-terraform, no walls yet) also count as
    // in-flight projects so the chief doesn't queue a second building on
    // top of an unfinished levelling job.
    for pending in &pending_footprints.queue {
        *faction_bp_count.entry(pending.faction_id).or_insert(0) += 1;
    }

    for (&faction_id, faction) in faction_registry.factions.iter() {
        if faction_id == SOLO || faction.member_count == 0 {
            continue;
        }
        if !leading_factions.contains(&faction_id) {
            continue;
        }
        let count = faction_bp_count.get(&faction_id).copied().unwrap_or(0);
        if count >= MAX_BLUEPRINTS_SAFETY_CAP {
            continue;
        }
        // One project at a time per faction.
        if count > 0 {
            continue;
        }

        let plan = plans.0.get(&faction_id);
        let candidates = generate_candidates(faction_id, faction, plan, &chunk_map, &maps, &bp_map);
        let Some(best) = candidates.into_iter().max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            continue;
        };

        spawn_intent(
            &mut commands,
            &mut bp_map,
            &mut terraform_map,
            &mut pending_footprints,
            &chunk_map,
            faction_id,
            faction.home_tile,
            best.intent,
            best.tile,
        );
    }
}

/// Build the per-faction candidate list. Each pressure source contributes at
/// most one candidate; cheap to compute since each calls O(zone) helpers.
fn generate_candidates(
    faction_id: u32,
    faction: &FactionData,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    chunk_map: &ChunkMap,
    maps: &BuildingMapsRO,
    bp_map: &BlueprintMap,
) -> Vec<BuildCandidate> {
    use crate::simulation::settlement::ZoneKind;

    let mut out: Vec<BuildCandidate> = Vec::with_capacity(8);
    let home = faction.home_tile;
    let members = faction.member_count;
    let techs = &faction.techs;
    let culture = &faction.culture;
    let wall_mat = best_wall_material(techs);

    // 0. Upgrade rebuild — if a structure has been deconstructed for upgrade
    //    and the slot is now empty, prioritise refilling it with the upgraded
    //    material. Bypass other candidates so the upgrade cycle resolves
    //    promptly and no agent picks up the slot for an unrelated build.
    if let Some(up_tile) = faction.active_upgrade {
        let still_a_wall = maps.wall_map.0.contains_key(&up_tile);
        if !still_a_wall {
            // Slot is vacant. Verify the tile is passable and unreserved.
            let tx = up_tile.0 as i32;
            let ty = up_tile.1 as i32;
            let blocked = bp_map.0.contains_key(&up_tile) || maps.bed_map.0.contains_key(&up_tile);
            let passable = chunk_map
                .tile_kind_at(tx, ty)
                .map(|k| k.is_passable() && k != TileKind::Wall)
                .unwrap_or(false);
            if !blocked && passable {
                out.push(BuildCandidate {
                    intent: BuildIntent::PalisadeSegment(wall_mat, 2),
                    tile: up_tile,
                    score: 5000.0,
                });
                return out; // skip everything else this tick
            }
        }
    }

    // 1. Hearths — band camps (pre-PERM_SETTLEMENT) target one fire per
    //    ~6 members, capped at 3, so a growing band naturally splits into
    //    multiple hearth-clusters rather than piling everyone around a
    //    single fire. Sedentary cultures keep the single civic-zone hearth.
    let desired_hearths: u32 = if !techs.has(PERM_SETTLEMENT) {
        crate::simulation::settlement::paleolithic_hearth_count(members)
    } else {
        1
    };
    let existing_hearths = count_campfires_near(&maps.campfire_map, home, 30) as u32;
    // Pacing: don't pre-emptively start a second hearth while the first is
    // still acquiring its bed crescents. Require ~4 beds per existing hearth
    // before opening a new one. Only applies once at least one fire is up.
    let total_beds_so_far = count_beds_near(&maps.bed_map, home, 30) as u32;
    let bed_pacing_ok =
        existing_hearths == 0 || total_beds_so_far >= existing_hearths.saturating_mul(4);
    let effective_desired = if bed_pacing_ok {
        desired_hearths
    } else {
        existing_hearths
    };
    if techs.has(FIRE_MAKING) && existing_hearths < effective_desired {
        let mut tile_opt = find_unfilled_civic_zone_tile(
            chunk_map,
            &maps.bed_map,
            bp_map,
            &maps.campfire_map,
            plan,
            home,
        );
        // Bootstrap: a freshly-created Paleolithic faction may not have its
        // first plan yet (planner is staggered ~60 ticks). Use the
        // deterministic hearth offsets directly so even the very first fire
        // lands at the proper distance from home rather than adjacent to it.
        if tile_opt.is_none() && !techs.has(PERM_SETTLEMENT) && plan.is_none() {
            let hearths = crate::simulation::settlement::paleolithic_hearth_positions(
                faction_id, home, members,
            );
            'outer: for (hx, hy) in hearths {
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let tx = hx + dx;
                        let ty = hy + dy;
                        let pos = (tx as i16, ty as i16);
                        if bp_map.0.contains_key(&pos)
                            || maps.bed_map.0.contains_key(&pos)
                            || maps.campfire_map.0.contains_key(&pos)
                        {
                            continue;
                        }
                        let Some(k) = chunk_map.tile_kind_at(tx, ty) else {
                            continue;
                        };
                        if k.is_passable() && k != TileKind::Wall {
                            tile_opt = Some(pos);
                            break 'outer;
                        }
                    }
                }
            }
        }
        if let Some(tile) = tile_opt {
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Campfire),
                tile,
                score: 1000.0,
            });
        }
    }

    let bed_count = count_beds_near(&maps.bed_map, home, 30) as i32;
    let bed_deficit = (members as i32 - bed_count).max(0) as f32;

    // 2. Residential — the principal growth axis. Pre-settlement: simple beds.
    //    Post-settlement: walled huts; with CITY_STATE_ORG, longhouses preferred.
    if bed_deficit > 0.0 {
        if !techs.has(PERM_SETTLEMENT) {
            // Paleolithic: cluster beds in a crescent annulus around the
            // nearest hearth. Defer if no fire exists yet — the campfire
            // candidate above (score 1000) will resolve first, matching the
            // historical ordering of fire-then-shelter.
            let hearths: Vec<(i16, i16)> = maps
                .campfire_map
                .0
                .keys()
                .copied()
                .filter(|&(cx, cy)| {
                    let dx = cx as i32 - home.0 as i32;
                    let dy = cy as i32 - home.1 as i32;
                    dx.abs() <= 25 && dy.abs() <= 25
                })
                .collect();
            if !hearths.is_empty() {
                if let Some(tile) = find_bed_tile_around_hearth(
                    chunk_map,
                    &maps.bed_map,
                    bp_map,
                    &hearths,
                    home,
                    2,
                    6,
                ) {
                    out.push(BuildCandidate {
                        intent: BuildIntent::Single(BuildSiteKind::Bed),
                        tile,
                        score: 200.0 + bed_deficit * 30.0,
                    });
                }
            }
        } else if techs.has(CITY_STATE_ORG) && bed_deficit >= 2.0 {
            if let Some(origin) = find_footprint_in_zone(
                chunk_map,
                &maps.bed_map,
                bp_map,
                plan,
                ZoneKind::Residential,
                home,
                2,
                1,
                20,
            ) {
                out.push(BuildCandidate {
                    intent: BuildIntent::Longhouse(wall_mat),
                    tile: origin,
                    score: 260.0 + bed_deficit * 25.0,
                });
            } else if let Some(origin) = find_footprint_in_zone(
                chunk_map,
                &maps.bed_map,
                bp_map,
                plan,
                ZoneKind::Residential,
                home,
                1,
                1,
                18,
            ) {
                out.push(BuildCandidate {
                    intent: BuildIntent::Hut(wall_mat),
                    tile: origin,
                    score: 230.0 + bed_deficit * 25.0,
                });
            }
        } else {
            if let Some(origin) = find_footprint_in_zone(
                chunk_map,
                &maps.bed_map,
                bp_map,
                plan,
                ZoneKind::Residential,
                home,
                1,
                1,
                18,
            ) {
                out.push(BuildCandidate {
                    intent: BuildIntent::Hut(wall_mat),
                    tile: origin,
                    score: 230.0 + bed_deficit * 25.0,
                });
            }
        }
    }

    // 3. Defense — palisade reinforcement. Driven by culture.defensive.
    let walls_count = count_walls_near(&maps.wall_map, home, 25) as i32;
    let target_walls = (members as i32 * 2 + 8).min(48);
    let defense_deficit = (target_walls - walls_count).max(0) as f32;
    if defense_deficit > 0.0 && bed_count > 0 {
        if let Some(tile) = find_palisade_site(chunk_map, &maps.bed_map, bp_map, home, 2) {
            let def_mult = 0.4 + (culture.defensive as f32 / 255.0) * 1.6;
            out.push(BuildCandidate {
                intent: BuildIntent::PalisadeSegment(wall_mat, 2),
                tile,
                score: 70.0 * def_mult + defense_deficit * 1.5,
            });
        }
    }

    // 4. Crafting — Workbench when FLINT_KNAPPING and we have somewhere to live.
    if techs.has(FLINT_KNAPPING)
        && count_workbenches_near(&maps.workbench_map, home, 25) == 0
        && bed_count >= 2
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            plan,
            ZoneKind::Crafting,
            home,
            10,
        ) {
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Workbench),
                tile,
                score: 150.0,
            });
        }
    }

    // 5. Granary — gated by GRANARY tech. Mercantile cultures prioritise.
    if techs.has(GRANARY)
        && count_granaries_near(&maps.granary_map, home, 25) == 0
        && bed_count >= 2
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            plan,
            ZoneKind::Storage,
            home,
            10,
        ) {
            let mer_mult = 0.7 + (culture.mercantile as f32 / 255.0) * 0.8;
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Granary),
                tile,
                score: 180.0 * mer_mult,
            });
        }
    }

    // 6. Shrine — gated by SACRED_RITUAL. Ceremonial cultures push hard.
    if techs.has(SACRED_RITUAL)
        && count_shrines_near(&maps.shrine_map, home, 25) == 0
        && bed_count >= 2
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            plan,
            ZoneKind::Sacred,
            home,
            10,
        ) {
            let cer_mult = 0.4 + (culture.ceremonial as f32 / 255.0) * 2.0;
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Shrine),
                tile,
                score: 110.0 * cer_mult,
            });
        }
    }

    // 7. Market — LONG_DIST_TRADE. Mercantile cultures aim for two; others one.
    if techs.has(LONG_DIST_TRADE) && bed_count >= 3 {
        let target_count = if culture.mercantile > 180 { 2 } else { 1 };
        if count_markets_near(&maps.market_map, home, 25) < target_count {
            if let Some(tile) = find_clear_tile_in_zone(
                chunk_map,
                &maps.bed_map,
                bp_map,
                plan,
                ZoneKind::Market,
                home,
                12,
            ) {
                let mer_mult = 0.6 + (culture.mercantile as f32 / 255.0) * 1.6;
                out.push(BuildCandidate {
                    intent: BuildIntent::Single(BuildSiteKind::Market),
                    tile,
                    score: 130.0 * mer_mult,
                });
            }
        }
    }

    // 8. Barracks — PROFESSIONAL_ARMY. Martial cultures prioritise.
    if techs.has(PROFESSIONAL_ARMY)
        && count_barracks_near(&maps.barracks_map, home, 25) == 0
        && bed_count >= 3
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            plan,
            ZoneKind::Defense,
            home,
            12,
        ) {
            let mar_mult = 0.5 + (culture.martial as f32 / 255.0) * 1.8;
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Barracks),
                tile,
                score: 140.0 * mar_mult,
            });
        }
    }

    // 9. Monument — MONUMENTAL_BUILDING. Ceremonial cultures invest heavily.
    if techs.has(MONUMENTAL_BUILDING)
        && count_monuments_near(&maps.monument_map, home, 30) == 0
        && bed_count >= 4
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            plan,
            ZoneKind::Sacred,
            home,
            12,
        ) {
            let cer_mult = 0.5 + (culture.ceremonial as f32 / 255.0) * 2.2;
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Monument),
                tile,
                score: 95.0 * cer_mult,
            });
        }
    }

    out
}

/// Spawn the blueprint(s) that realise a chosen intent at the given tile.
fn spawn_intent(
    commands: &mut Commands,
    bp_map: &mut BlueprintMap,
    terraform_map: &mut crate::simulation::terraform::TerraformMap,
    pending_footprints: &mut crate::simulation::terraform::PendingFootprints,
    chunk_map: &ChunkMap,
    faction_id: u32,
    home: (i16, i16),
    intent: BuildIntent,
    tile: (i16, i16),
) {
    match intent {
        BuildIntent::Single(kind) => {
            let target_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32) as i8;
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let e = commands
                .spawn((
                    Blueprint::new(faction_id, None, kind, tile, target_z),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
        }
        BuildIntent::Hut(wall_mat) => {
            plan_building(
                commands,
                bp_map,
                terraform_map,
                pending_footprints,
                chunk_map,
                tile.0 as i32,
                tile.1 as i32,
                1,
                1,
                faction_id,
                home,
                &[(0, 0)],
                wall_mat,
            );
        }
        BuildIntent::Longhouse(wall_mat) => {
            plan_building(
                commands,
                bp_map,
                terraform_map,
                pending_footprints,
                chunk_map,
                tile.0 as i32,
                tile.1 as i32,
                2,
                1,
                faction_id,
                home,
                &[(-1, 0), (1, 0)],
                wall_mat,
            );
        }
        BuildIntent::PalisadeSegment(wall_mat, _) => {
            let target_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32) as i8;
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let e = commands
                .spawn((
                    Blueprint::new(
                        faction_id,
                        None,
                        BuildSiteKind::Wall(wall_mat),
                        tile,
                        target_z,
                    ),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
        }
    }
}

// ── Ritual system ─────────────────────────────────────────────────────────────

/// Single ritual event record. Most recent N entries are kept on
/// `RitualState.recent_events` and surfaced in the Debug panel.
#[derive(Clone, Debug)]
pub struct RitualEvent {
    pub faction_id: u32,
    pub season: Season,
    pub focal: (i16, i16),
    pub uses_monument: bool,
    pub members_affected: u32,
    pub pulse: u8,
}

/// Tracks the last season we processed a ritual pulse for. Compared each tick
/// to the live `Calendar`; on transition the pulse fires once for every
/// faction with a Shrine or Monument. The `recent_events` ring buffer keeps
/// the last 16 ritual fires for player observation.
#[derive(Resource)]
pub struct RitualState {
    pub last_season: Season,
    pub recent_events: Vec<RitualEvent>,
}

impl RitualState {
    const MAX_EVENTS: usize = 16;

    pub fn record(&mut self, ev: RitualEvent) {
        if self.recent_events.len() >= Self::MAX_EVENTS {
            self.recent_events.remove(0);
        }
        self.recent_events.push(ev);
    }
}

impl Default for RitualState {
    fn default() -> Self {
        Self {
            last_season: Season::Spring,
            recent_events: Vec::new(),
        }
    }
}

/// On season transition, every faction that owns a Shrine or Monument runs a
/// short ritual: faction members within radius 12 of the focal structure get
/// their `social` need reduced by a pulse (mood follows from distress in
/// `derive_mood_system`). Pulse magnitude scales with `culture.ceremonial`.
pub fn ritual_system(
    calendar: Res<Calendar>,
    mut ritual_state: ResMut<RitualState>,
    registry: Res<FactionRegistry>,
    shrine_map: Res<ShrineMap>,
    monument_map: Res<MonumentMap>,
    mut agent_query: Query<(&FactionMember, &mut Needs, &Transform, &LodLevel)>,
) {
    if calendar.season == ritual_state.last_season {
        return;
    }
    ritual_state.last_season = calendar.season;

    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO || faction.member_count == 0 {
            continue;
        }
        let home = faction.home_tile;
        let (hx, hy) = (home.0 as i32, home.1 as i32);

        // Pick a ritual focal point: monument first (more impressive), shrine fallback.
        let monument_focal = monument_map
            .0
            .keys()
            .find(|&&p| (p.0 as i32 - hx).abs() <= 30 && (p.1 as i32 - hy).abs() <= 30)
            .copied();
        let shrine_focal = shrine_map
            .0
            .keys()
            .find(|&&p| (p.0 as i32 - hx).abs() <= 30 && (p.1 as i32 - hy).abs() <= 30)
            .copied();
        let (focal, uses_monument) = match (monument_focal, shrine_focal) {
            (Some(p), _) => (p, true),
            (None, Some(p)) => (p, false),
            _ => continue,
        };

        // Pulse magnitude: 15 baseline + up to 35 from ceremonial trait.
        let pulse_f = 15.0 + (faction.culture.ceremonial as f32 / 255.0) * 35.0;
        let (fx, fy) = (focal.0 as i32, focal.1 as i32);

        let mut affected: u32 = 0;
        for (member, mut needs, transform, lod) in agent_query.iter_mut() {
            if member.faction_id != faction_id {
                continue;
            }
            if *lod == LodLevel::Dormant {
                continue;
            }
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            if (tx - fx).abs() > 12 || (ty - fy).abs() > 12 {
                continue;
            }
            needs.social = (needs.social - pulse_f).max(0.0);
            affected += 1;
        }

        ritual_state.record(RitualEvent {
            faction_id,
            season: calendar.season,
            focal,
            uses_monument,
            members_affected: affected,
            pulse: pulse_f.round().clamp(0.0, 255.0) as u8,
        });
    }
}

// ── Road carving ──────────────────────────────────────────────────────────────

/// Drains `RoadCarveQueue`. For each pending (faction_id, building_tile, home)
/// triple, walks a Bresenham line from the building back to the home tile and
/// converts each passable, non-Wall tile into `TileKind::Road`. Skips tiles
/// already road, blueprint, bed, or wall. Emits `TileChangedEvent` for each
/// converted tile so the renderer refreshes.
pub fn road_carve_system(
    mut queue: ResMut<RoadCarveQueue>,
    mut chunk_map: ResMut<ChunkMap>,
    bp_map: Res<BlueprintMap>,
    bed_map: Res<BedMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    if queue.0.is_empty() {
        return;
    }
    // Drain — re-allocate a fresh empty Vec to release the lock on `queue`.
    let drained: Vec<(u32, (i16, i16), (i16, i16))> = std::mem::take(&mut queue.0);

    for (_faction_id, from, to) in drained {
        let mut x0 = from.0 as i32;
        let mut y0 = from.1 as i32;
        let x1 = to.0 as i32;
        let y1 = to.1 as i32;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            // Skip the endpoint tiles (the building itself, the home tile).
            let is_endpoint =
                (x0 == from.0 as i32 && y0 == from.1 as i32) || (x0 == x1 && y0 == y1);
            if !is_endpoint {
                let tile = (x0 as i16, y0 as i16);
                if !bp_map.0.contains_key(&tile) && !bed_map.0.contains_key(&tile) {
                    let surf_z = chunk_map.surface_z_at(x0, y0);
                    let cur = chunk_map.tile_kind_at(x0, y0);
                    let writable = matches!(
                        cur,
                        Some(TileKind::Grass) | Some(TileKind::Dirt) | Some(TileKind::Farmland)
                    );
                    if writable {
                        chunk_map.set_tile(
                            x0,
                            y0,
                            surf_z as i32,
                            TileData {
                                kind: TileKind::Road,
                                ..Default::default()
                            },
                        );
                        tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                            tx: tile.0,
                            ty: tile.1,
                        });
                    }
                }
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }
}

// ── Upgrade pipeline ──────────────────────────────────────────────────────────

/// Cadence for the upgrade scan. 240 ticks @ 20 Hz = 12 s per faction pass.
const UPGRADE_INTERVAL_TICKS: u64 = 240;

/// Identify and start one structure upgrade per faction. Triggers when:
/// - The faction has unlocked a higher wall material than some existing wall.
/// - No upgrade is already in flight (`active_upgrade.is_none()`).
/// - The faction has surplus stock for the rebuild recipe (2× inputs).
/// - The faction is not under raid (don't tear walls down mid-attack).
///
/// On match: assigns a `Deconstruct` task to the nearest idle faction member,
/// sets `active_upgrade = Some(tile)`. The chief's selector then sees the
/// vacated slot and queues a fresh blueprint at the upgraded material tier.
pub fn building_upgrade_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    chunk_map: Res<ChunkMap>,
    wall_map: Res<WallMap>,
    wall_query: Query<&Wall>,
    mut agent_query: Query<(Entity, &mut PersonAI, &FactionMember, &Transform, &LodLevel)>,
) {
    if clock.tick % UPGRADE_INTERVAL_TICKS != 0 {
        return;
    }

    // Snapshot all faction state we need (we'll mutate `active_upgrade` later).
    let faction_state: Vec<(
        u32,
        (i16, i16),
        FactionTechs,
        bool,
        bool,
        AHashMap<Good, u32>,
    )> = registry
        .factions
        .iter()
        .filter(|(&id, _)| id != SOLO)
        .map(|(&id, f)| {
            (
                id,
                f.home_tile,
                f.techs.clone(),
                f.active_upgrade.is_some(),
                f.under_raid,
                f.storage.totals.clone(),
            )
        })
        .collect();

    for (faction_id, home, techs, has_active, under_raid, storage) in faction_state {
        if has_active || under_raid {
            continue;
        }

        let target_mat = best_wall_material(&techs);
        let target_rank = target_mat as u8;

        // Find one outdated wall within radius 25 of home.
        let (hx, hy) = (home.0 as i32, home.1 as i32);
        let mut outdated: Option<(i16, i16)> = None;
        for (&pos, &wall_e) in wall_map.0.iter() {
            if (pos.0 as i32 - hx).abs() > 25 || (pos.1 as i32 - hy).abs() > 25 {
                continue;
            }
            let Ok(wall) = wall_query.get(wall_e) else {
                continue;
            };
            // Lower-rank wall material gets upgraded.
            if (wall.material as u8) < target_rank {
                outdated = Some(pos);
                break;
            }
        }
        let Some(tile) = outdated else { continue };

        // Surplus stock check: 2× the rebuild recipe inputs.
        let recipe = recipe_for(BuildSiteKind::Wall(target_mat));
        let has_stock = recipe
            .inputs
            .iter()
            .all(|&(good, qty)| storage.get(&good).copied().unwrap_or(0) >= (qty as u32) * 2);
        if !has_stock {
            continue;
        }

        // Find the closest idle, non-dormant faction member.
        let mut nearest: Option<(Entity, i32, (i16, i16))> = None;
        for (e, ai, member, transform, lod) in agent_query.iter() {
            if member.faction_id != faction_id {
                continue;
            }
            if *lod == LodLevel::Dormant {
                continue;
            }
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                continue;
            }
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let d = (tx - tile.0 as i32).abs() + (ty - tile.1 as i32).abs();
            if nearest.map(|(_, nd, _)| d < nd).unwrap_or(true) {
                nearest = Some((e, d, (tx as i16, ty as i16)));
            }
        }
        let Some((agent_e, _, cur_tile)) = nearest else {
            continue;
        };

        // Assign the Deconstruct task at the wall's tile.
        if let Ok((_, mut ai, _, _, _)) = agent_query.get_mut(agent_e) {
            let cur_chunk = ChunkCoord(
                (cur_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
                (cur_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
            );
            assign_task_with_routing(
                &mut ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Deconstruct,
                None,
                &chunk_graph,
                &chunk_router,
                &chunk_map,
                &chunk_connectivity,
            );
            // Mark the slot as in-flight so the chief and selector know.
            if let Some(faction) = registry.factions.get_mut(&faction_id) {
                faction.active_upgrade = Some(tile);
            }
        }
    }
}

/// Handles agents at Blueprint entities. Two roles run in parallel:
///   • `TaskKind::HaulMaterials` — drops matching goods into the blueprint's
///     deposit slots and returns to Idle the same tick (excess stays in the
///     hauler's inventory).
///   • `TaskKind::Construct` / `ConstructBed` — advances `bp.build_progress`
///     by one each tick the worker is on-site and earns Building XP. Workers
///     stay on the task until the structure completes (no longer kicked off
///     just because they're empty-handed).
/// Construction completes when both `build_progress >= recipe.work_ticks` AND
/// `bp.is_satisfied()`.
/// Runs in Sequential set after gather_system.
pub fn construction_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut maps: FurnitureMaps,
    mut bp_map: ResMut<BlueprintMap>,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut road_carve_queue: ResMut<RoadCarveQueue>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    mut bp_query: Query<&mut Blueprint>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&mut ActivePlan>,
    )>,
) {
    // Pass 1: collect pending contributions from Working agents, classified by
    // role.
    //   bp_haulers: bp_entity → Vec<(agent, inventory snapshot per deposit slot)>
    //   bp_workers: bp_entity → Vec<agent>
    let mut bp_haulers: AHashMap<Entity, Vec<(Entity, [u32; MAX_BUILD_INPUTS])>> = AHashMap::new();
    let mut bp_workers: AHashMap<Entity, Vec<Entity>> = AHashMap::new();

    for (entity, mut ai, agent, carrier, _skills, slot, lod, _) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        let task = ai.task_id;
        let is_hauler = task == TaskKind::HaulMaterials as u16;
        let is_worker = task == TaskKind::Construct as u16 || task == TaskKind::ConstructBed as u16;
        if !is_hauler && !is_worker {
            continue;
        }

        let Some(bp_entity) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };

        // Peek at the blueprint's deposit list (immutable) so we can snapshot
        // hauler inventories and validate that the bp still exists.
        let bp_info = bp_query
            .get(bp_entity)
            .ok()
            .map(|bp| (bp.deposits, bp.deposit_count));
        let Some((deposits, count)) = bp_info else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            ai.target_entity = None;
            continue;
        };

        if is_hauler {
            // Snapshot only the goods the bp still needs.
            // Haulers usually carry materials in hand; also count personal inventory
            // in case they happen to have a useful good stashed.
            let mut snap = [0u32; MAX_BUILD_INPUTS];
            let mut useful = false;
            for i in 0..count as usize {
                let still = deposits[i].needed.saturating_sub(deposits[i].deposited) as u32;
                if still > 0 {
                    let in_hand = carrier.quantity_of_good(deposits[i].good);
                    let in_inv = agent.quantity_of(deposits[i].good);
                    snap[i] = in_hand.saturating_add(in_inv);
                    if snap[i] > 0 {
                        useful = true;
                    }
                }
            }
            if !useful {
                // Nothing to drop here — release back to plan so it can re-route.
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.work_progress = 0;
                ai.target_entity = None;
                continue;
            }
            bp_haulers
                .entry(bp_entity)
                .or_default()
                .push((entity, snap));
        } else {
            // Worker: register on-site. XP and progress are awarded later,
            // once we know the blueprint has all its materials deposited
            // (see the is_satisfied gate in pass 2).
            bp_workers.entry(bp_entity).or_default().push(entity);
        }
    }

    if bp_haulers.is_empty() && bp_workers.is_empty() {
        return;
    }

    let mut completed_agents: Vec<Entity> = Vec::new();
    let mut hauler_done: Vec<Entity> = Vec::new();
    let mut orphaned_agents: Vec<Entity> = Vec::new();
    // Workers who actually advanced progress this tick (i.e. on-site at a
    // satisfied blueprint). Building XP is granted in pass 3.
    let mut xp_grants: Vec<Entity> = Vec::new();
    // (agent_entity, good, qty_to_remove)
    let mut good_removals: Vec<(Entity, Good, u32)> = Vec::new();

    // Pass 2: deposit hauler goods, advance worker progress, check completion.
    let mut bp_entities: Vec<Entity> = bp_haulers
        .keys()
        .copied()
        .chain(bp_workers.keys().copied())
        .collect();
    bp_entities.sort_unstable();
    bp_entities.dedup();

    for bp_entity in bp_entities {
        let Ok(mut bp) = bp_query.get_mut(bp_entity) else {
            if let Some(haulers) = bp_haulers.get(&bp_entity) {
                orphaned_agents.extend(haulers.iter().map(|(e, _)| *e));
            }
            if let Some(workers) = bp_workers.get(&bp_entity) {
                orphaned_agents.extend(workers.iter().copied());
            }
            continue;
        };

        // Deposit hauler goods first.
        if let Some(haulers) = bp_haulers.get(&bp_entity) {
            for &(agent_e, snap) in haulers {
                for i in 0..bp.deposit_count as usize {
                    let need = bp.deposits[i];
                    let still = need.needed.saturating_sub(need.deposited) as u32;
                    if still == 0 || snap[i] == 0 {
                        continue;
                    }
                    let take = still.min(snap[i]);
                    good_removals.push((agent_e, need.good, take));
                    bp.deposits[i].deposited = bp.deposits[i].deposited.saturating_add(take as u8);
                }
                hauler_done.push(agent_e);
            }
        }

        // Advance work by one tick per on-site worker — but only once all
        // materials have been deposited. Gating on `is_satisfied()` here
        // (a) prevents `build_progress` from saturating past `work_ticks`
        // while haulers are still en route, and (b) keeps Building XP
        // honest by only awarding it for real labour.
        let recipe = recipe_for(bp.kind);
        if bp.is_satisfied() {
            if let Some(workers) = bp_workers.get(&bp_entity) {
                bp.build_progress = bp
                    .build_progress
                    .saturating_add(workers.len() as u8)
                    .min(recipe.work_ticks);
                xp_grants.extend(workers.iter().copied());
            }
        }

        if bp.build_progress >= recipe.work_ticks && bp.is_satisfied() {
            let tile = bp.tile;
            let (tx, ty) = (tile.0 as i32, tile.1 as i32);

            let world_pos = tile_to_world(tx, ty);
            match bp.kind {
                BuildSiteKind::Wall(material) => {
                    let surf_z = bp.target_z as i32;
                    // Defer placement if any agent currently stands on the
                    // target column at the soon-to-be-trapped foot z. Writing
                    // a Wall at surf_z+1 invalidates passable_at(tx, ty,
                    // surf_z) (no headspace), and the recovery system would
                    // then need to teleport them. Cleaner to wait one tick
                    // for them to vacate.
                    if spatial.agent_occupied(tx, ty, surf_z)
                        || spatial.agent_occupied(tx, ty, surf_z + 1)
                    {
                        continue;
                    }
                    chunk_map.set_tile(
                        tx,
                        ty,
                        surf_z + 1,
                        TileData {
                            kind: TileKind::Wall,
                            elevation: 0,
                            fertility: 0,
                            flags: 0b0001,
                        },
                    );

                    let wall_entity = commands
                        .spawn((
                            Wall { material },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.wall_map.0.insert(tile, wall_entity);
                }
                BuildSiteKind::Bed => {
                    let bed_entity = commands
                        .spawn((
                            Bed::default(),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.bed_map.0.insert(tile, bed_entity);
                }
                BuildSiteKind::Campfire => {
                    let campfire_entity = commands
                        .spawn((
                            Campfire::default(),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.campfire_map.0.insert(tile, campfire_entity);
                }
                BuildSiteKind::Door => {
                    // A door does NOT write a Wall tile — the underlying
                    // terrain stays passable. The Door entity carries the
                    // open/closed state consulted by line_of_sight.
                    let door_entity = commands
                        .spawn((
                            Door {
                                faction_id: bp.faction_id,
                                open: false,
                                tier: DoorTier::default(),
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.door_map.0.insert(
                        tile,
                        DoorEntry {
                            entity: door_entity,
                            open: false,
                        },
                    );
                }
                BuildSiteKind::Workbench => {
                    let e = commands
                        .spawn((
                            Workbench {
                                faction_id: bp.faction_id,
                                tier: WorkbenchTier::default(),
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.workbench_map.0.insert(tile, e);
                }
                BuildSiteKind::Loom => {
                    let e = commands
                        .spawn((
                            Loom {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.loom_map.0.insert(tile, e);
                }
                BuildSiteKind::Table => {
                    let e = commands
                        .spawn((
                            Table,
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.table_map.0.insert(tile, e);
                }
                BuildSiteKind::Chair => {
                    let e = commands
                        .spawn((
                            Chair,
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.chair_map.0.insert(tile, e);
                }
                BuildSiteKind::Granary => {
                    let e = commands
                        .spawn((
                            Granary {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.granary_map.0.insert(tile, e);
                }
                BuildSiteKind::Shrine => {
                    let e = commands
                        .spawn((
                            Shrine {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.shrine_map.0.insert(tile, e);
                }
                BuildSiteKind::Market => {
                    let e = commands
                        .spawn((
                            Market {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.market_map.0.insert(tile, e);
                }
                BuildSiteKind::Barracks => {
                    let e = commands
                        .spawn((
                            Barracks {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.barracks_map.0.insert(tile, e);
                }
                BuildSiteKind::Monument => {
                    let e = commands
                        .spawn((
                            Monument {
                                faction_id: bp.faction_id,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.45),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.monument_map.0.insert(tile, e);
                }
            }

            // Emit a TileChangedEvent so pathfinding caches (flow fields,
            // chunk graph) see the new wall/furniture and re-route.
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                tx: tile.0,
                ty: tile.1,
            });

            bp_map.0.remove(&tile);
            commands.entity(bp_entity).despawn_recursive();

            // Clear `active_upgrade` if the rebuild slot has just been filled.
            if let Some(faction) = registry.factions.get_mut(&bp.faction_id) {
                if faction.active_upgrade == Some(tile) {
                    faction.active_upgrade = None;
                }
                // Queue a road carve from the new building back to home_tile so
                // the settlement grows a connective road network organically.
                road_carve_queue
                    .0
                    .push((bp.faction_id, tile, faction.home_tile));
            }

            if let Some(workers) = bp_workers.get(&bp_entity) {
                completed_agents.extend(workers.iter().copied());
            }
            if let Some(haulers) = bp_haulers.get(&bp_entity) {
                completed_agents.extend(haulers.iter().map(|(e, _)| *e));
            }
        }
    }

    if good_removals.is_empty()
        && completed_agents.is_empty()
        && hauler_done.is_empty()
        && orphaned_agents.is_empty()
        && xp_grants.is_empty()
    {
        return;
    }

    // Pass 3: remove deposited goods from agents, grant Building XP to workers
    // whose labour actually advanced progress, and reset completed/hauler/orphaned agents.
    for (entity, mut ai, mut agent, mut carrier, mut skills, _, _, mut plan_opt) in
        agent_query.iter_mut()
    {
        for &(ae, good, qty) in &good_removals {
            if ae == entity {
                // Consume from hands first (where haulers typically carry the load),
                // fall back to personal inventory.
                let from_hand = carrier.remove_good(good, qty);
                let still = qty - from_hand;
                if still > 0 {
                    agent.remove_good(good, still);
                }
            }
        }

        if xp_grants.contains(&entity) {
            skills.gain_xp(SkillKind::Building, 1);
        }

        let is_completed = completed_agents.contains(&entity);
        let is_hauler_done = hauler_done.contains(&entity);
        let is_orphaned = orphaned_agents.contains(&entity);

        if is_completed || is_hauler_done || is_orphaned {
            if is_completed {
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += if ai.task_id == TaskKind::ConstructBed as u16 {
                        2.0
                    } else if ai.task_id == TaskKind::HaulMaterials as u16 {
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

/// Affinity threshold above which a homeless woman gets a "discount" toward
/// beds close to her partner's bed.
const PARTNER_AFFINITY_THRESHOLD: i8 = 60;
/// Higher bar for an *already-housed* woman to abandon her bed for one closer
/// to her partner (hysteresis vs PARTNER_AFFINITY_THRESHOLD prevents flapping).
const REASSIGN_AFFINITY_THRESHOLD: i8 = 80;
/// Manhattan radius around partner's bed considered "close enough" — also the
/// search radius for an unclaimed alternative.
const PARTNER_PROXIMITY_RADIUS: i32 = 3;
/// Distance "discount" applied to scoring when a candidate bed is within
/// `PARTNER_PROXIMITY_RADIUS` of an affinity-≥`PARTNER_AFFINITY_THRESHOLD` partner's bed.
const PARTNER_PROXIMITY_BONUS: i32 = 50;

/// Assigns each agent a personal bed (`HomeBed`) so they consistently sleep in
/// the same place. Faction members are paired with the nearest unclaimed bed
/// inside their faction territory. Solo agents first try to claim any unclaimed
/// bed within 30 tiles; failing that, they place a personal bed blueprint at
/// their current position so they can build one themselves.
///
/// Affinity overlay: women's bed scoring is biased toward beds near their
/// highest-affinity opposite-sex faction-mate. An already-housed woman whose
/// affinity to a man exceeds `REASSIGN_AFFINITY_THRESHOLD` will migrate to an
/// unclaimed bed within `PARTNER_PROXIMITY_RADIUS` of his bed (or queue a new
/// bed blueprint adjacent to his bed if none exists).
pub fn assign_beds_system(
    mut commands: Commands,
    mut bed_query: Query<&mut Bed>,
    person_query: Query<
        (
            Entity,
            &FactionMember,
            &Transform,
            Option<&HomeBed>,
            Option<&crate::simulation::memory::RelationshipMemory>,
            Option<&crate::simulation::reproduction::BiologicalSex>,
        ),
        With<Person>,
    >,
    bed_map: Res<BedMap>,
    mut bp_map: ResMut<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    faction_registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
) {
    use crate::simulation::reproduction::BiologicalSex;

    if clock.tick % 30 != 0 {
        return;
    }

    let mut claimed_this_pass: AHashSet<Entity> = AHashSet::new();

    // Reverse lookup: bed entity → tile position. BedMap is sparse so the
    // collect is cheap.
    let bed_pos_by_entity: AHashMap<Entity, (i16, i16)> =
        bed_map.0.iter().map(|(&pos, &e)| (e, pos)).collect();

    // Snapshot every person's HomeBed entity, sex, and faction so Pass A can
    // resolve partner data without re-querying.
    struct PartnerInfo {
        sex: Option<BiologicalSex>,
        home_bed: Option<Entity>,
        faction_id: u32,
    }
    let partner_info: AHashMap<Entity, PartnerInfo> = person_query
        .iter()
        .map(|(e, fm, _, hb, _, sex)| {
            (
                e,
                PartnerInfo {
                    sex: sex.copied(),
                    home_bed: hb.and_then(|h| h.0),
                    faction_id: fm.faction_id,
                },
            )
        })
        .collect();

    // Resolves the highest-affinity opposite-sex same-faction partner that
    // also has a HomeBed. Returns the partner's bed tile.
    let best_partner_bed_for = |entity: Entity,
                                rel: &crate::simulation::memory::RelationshipMemory,
                                my_sex: BiologicalSex,
                                my_faction: u32,
                                min_aff: i8|
     -> Option<(i16, i16)> {
        let mut best: Option<((i16, i16), i8)> = None;
        for slot in &rel.entries {
            let Some(entry) = slot else { continue };
            if entry.affinity < min_aff {
                continue;
            }
            if entry.entity == entity {
                continue;
            }
            let Some(info) = partner_info.get(&entry.entity) else {
                continue;
            };
            if info.faction_id != my_faction {
                continue;
            }
            let Some(p_sex) = info.sex else { continue };
            if p_sex == my_sex {
                continue;
            }
            let Some(p_bed_e) = info.home_bed else {
                continue;
            };
            let Some(&p_bed_pos) = bed_pos_by_entity.get(&p_bed_e) else {
                continue;
            };
            if best.map(|(_, a)| entry.affinity > a).unwrap_or(true) {
                best = Some((p_bed_pos, entry.affinity));
            }
        }
        best.map(|(pos, _)| pos)
    };

    // ── Pass A: re-evaluate already-housed women whose partner lives far away.
    for (person, member, _transform, home_bed_opt, rel_opt, sex_opt) in &person_query {
        if member.faction_id == SOLO {
            continue;
        }
        let Some(my_bed) = home_bed_opt.and_then(|h| h.0) else {
            continue;
        };
        let my_bed_owner = bed_query.get(my_bed).ok().and_then(|b| b.owner);
        if my_bed_owner != Some(person) {
            continue;
        }
        let Some(&my_bed_pos) = bed_pos_by_entity.get(&my_bed) else {
            continue;
        };
        let Some(rel) = rel_opt else { continue };
        let Some(my_sex) = sex_opt.copied() else {
            continue;
        };
        if my_sex != BiologicalSex::Female {
            continue;
        }

        let Some(partner_bed_pos) = best_partner_bed_for(
            person,
            rel,
            my_sex,
            member.faction_id,
            REASSIGN_AFFINITY_THRESHOLD,
        ) else {
            continue;
        };
        let cur_dist = (my_bed_pos.0 as i32 - partner_bed_pos.0 as i32).abs()
            + (my_bed_pos.1 as i32 - partner_bed_pos.1 as i32).abs();
        if cur_dist <= PARTNER_PROXIMITY_RADIUS {
            continue;
        }

        // Look for an unclaimed bed within the proximity radius of partner.
        let mut candidate: Option<(Entity, (i16, i16))> = None;
        for (&pos, &bed_e) in &bed_map.0 {
            if bed_e == my_bed || claimed_this_pass.contains(&bed_e) {
                continue;
            }
            let d = (pos.0 as i32 - partner_bed_pos.0 as i32).abs()
                + (pos.1 as i32 - partner_bed_pos.1 as i32).abs();
            if d > PARTNER_PROXIMITY_RADIUS {
                continue;
            }
            match bed_query.get(bed_e) {
                Ok(b) if b.owner.is_none() => {
                    candidate = Some((bed_e, pos));
                    break;
                }
                _ => {}
            }
        }

        if let Some((new_bed, _)) = candidate {
            if let Ok(mut old) = bed_query.get_mut(my_bed) {
                old.owner = None;
            }
            if let Ok(mut new) = bed_query.get_mut(new_bed) {
                new.owner = Some(person);
            }
            commands.entity(person).insert(HomeBed(Some(new_bed)));
            claimed_this_pass.insert(new_bed);
            continue;
        }

        // Fallback: queue a blueprint adjacent to partner's bed.
        let neighbors = [
            (0, 1),
            (1, 0),
            (0, -1),
            (-1, 0),
            (1, 1),
            (-1, 1),
            (1, -1),
            (-1, -1),
        ];
        for (dx, dy) in neighbors {
            let tx_i32 = partner_bed_pos.0 as i32 + dx;
            let ty_i32 = partner_bed_pos.1 as i32 + dy;
            let tx = tx_i32 as i16;
            let ty = ty_i32 as i16;
            if bp_map.0.contains_key(&(tx, ty)) || bed_map.0.contains_key(&(tx, ty)) {
                continue;
            }
            let target_z = chunk_map.surface_z_at(tx_i32, ty_i32) as i8;
            if !chunk_map.passable_at(tx_i32, ty_i32, target_z as i32) {
                continue;
            }
            let wp = tile_to_world(tx_i32, ty_i32);
            let bp_e = commands
                .spawn((
                    Blueprint::new(
                        member.faction_id,
                        None,
                        BuildSiteKind::Bed,
                        (tx, ty),
                        target_z,
                    ),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert((tx, ty), bp_e);
            break;
        }
    }

    // ── Faction pass ─────────────────────────────────────────────────────────
    struct Homeless {
        person: Entity,
        pos: (i32, i32),
        partner_bed: Option<(i16, i16)>,
    }
    let mut homeless_by_faction: AHashMap<u32, Vec<Homeless>> = AHashMap::new();
    for (person, member, transform, home_bed, rel_opt, sex_opt) in &person_query {
        if member.faction_id == SOLO {
            continue;
        }
        let stale = match home_bed.and_then(|h| h.0) {
            Some(bed_entity) => bed_query.get(bed_entity).is_err(),
            None => true,
        };
        if !stale {
            continue;
        }
        let x = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let y = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
        let partner_bed = match (rel_opt, sex_opt.copied()) {
            (Some(rel), Some(sex)) if sex == BiologicalSex::Female => best_partner_bed_for(
                person,
                rel,
                sex,
                member.faction_id,
                PARTNER_AFFINITY_THRESHOLD,
            ),
            _ => None,
        };
        homeless_by_faction
            .entry(member.faction_id)
            .or_default()
            .push(Homeless {
                person,
                pos: (x, y),
                partner_bed,
            });
    }

    for (faction_id, homeless) in homeless_by_faction {
        let Some(fd) = faction_registry.factions.get(&faction_id) else {
            continue;
        };
        let home = fd.home_tile;
        let mut available: Vec<(Entity, (i16, i16))> = bed_map
            .0
            .iter()
            .filter(|(pos, _)| {
                (pos.0 as i32 - home.0 as i32).abs() <= 30
                    && (pos.1 as i32 - home.1 as i32).abs() <= 30
            })
            .filter_map(|(pos, &bed_entity)| {
                if claimed_this_pass.contains(&bed_entity) {
                    return None;
                }
                match bed_query.get(bed_entity) {
                    Ok(b) if b.owner.is_none() => Some((bed_entity, *pos)),
                    _ => None,
                }
            })
            .collect();

        for h in homeless {
            if available.is_empty() {
                break;
            }
            let mut best_idx = 0;
            let mut best_score = i32::MAX;
            for (i, (_, bpos)) in available.iter().enumerate() {
                let manhattan_to_person =
                    (bpos.0 as i32 - h.pos.0).abs() + (bpos.1 as i32 - h.pos.1).abs();
                let partner_bonus = match h.partner_bed {
                    Some(pbed) => {
                        let d = (bpos.0 as i32 - pbed.0 as i32).abs()
                            + (bpos.1 as i32 - pbed.1 as i32).abs();
                        if d <= PARTNER_PROXIMITY_RADIUS {
                            PARTNER_PROXIMITY_BONUS
                        } else {
                            0
                        }
                    }
                    None => 0,
                };
                let score = manhattan_to_person - partner_bonus;
                if score < best_score {
                    best_score = score;
                    best_idx = i;
                }
            }
            let (bed_e, _) = available.swap_remove(best_idx);
            claimed_this_pass.insert(bed_e);
            commands.entity(h.person).insert(HomeBed(Some(bed_e)));
            if let Ok(mut bed_comp) = bed_query.get_mut(bed_e) {
                bed_comp.owner = Some(h.person);
            }
        }
    }

    // ── Solo pass ─────────────────────────────────────────────────────────────
    // Solo agents claim any nearby unclaimed bed, or place a personal blueprint.
    for (person, member, transform, home_bed, _, _) in &person_query {
        if member.faction_id != SOLO {
            continue;
        }
        let stale = match home_bed.and_then(|h| h.0) {
            Some(bed_entity) => bed_query.get(bed_entity).is_err(),
            None => true,
        };
        if !stale {
            continue;
        }
        let tx = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i16;
        let ty = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i16;

        // Try to claim the nearest unclaimed bed within 30 tiles.
        let mut best_bed: Option<(Entity, i32)> = None;
        for (&bpos, &bed_entity) in &bed_map.0 {
            if claimed_this_pass.contains(&bed_entity) {
                continue;
            }
            if bed_query
                .get(bed_entity)
                .map(|b| b.owner.is_some())
                .unwrap_or(true)
            {
                continue;
            }
            let d = (bpos.0 as i32 - tx as i32).abs() + (bpos.1 as i32 - ty as i32).abs();
            if d <= 30 && best_bed.map(|(_, bd)| d < bd).unwrap_or(true) {
                best_bed = Some((bed_entity, d));
            }
        }

        if let Some((bed_e, _)) = best_bed {
            claimed_this_pass.insert(bed_e);
            commands.entity(person).insert(HomeBed(Some(bed_e)));
            if let Ok(mut bed_comp) = bed_query.get_mut(bed_e) {
                bed_comp.owner = Some(person);
            }
        } else {
            // No bed nearby — place a personal blueprint if none already exists.
            let has_personal_bp = bp_map.0.iter().any(|(_, &bp_e)| {
                bp_query
                    .get(bp_e)
                    .map(|bp| bp.personal_owner == Some(person))
                    .unwrap_or(false)
            });
            if !has_personal_bp && !bp_map.0.contains_key(&(tx, ty)) {
                let target_z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
                let wp = tile_to_world(tx as i32, ty as i32);
                let bp_e = commands
                    .spawn((
                        Blueprint::new(SOLO, Some(person), BuildSiteKind::Bed, (tx, ty), target_z),
                        Transform::from_xyz(wp.x, wp.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                bp_map.0.insert((tx, ty), bp_e);
            }
        }
    }
}

/// Handles deconstruction (`TaskKind::Deconstruct`). When an agent finishes
/// dismantling, removes the entity from whichever map holds it (Bed/Door/
/// Table/Chair/Workbench/Loom/Campfire/Granary/Shrine/Wall), refunds the
/// recipe's `deconstruct_refund`, and chains into a `DepositResource` task to
/// carry the recovered goods to storage.
///
/// Walls are also handled here (used by the upgrade pipeline): the wall entity
/// is despawned, the chunk_map tile reverts to Grass, and a TileChangedEvent
/// is emitted so the renderer refreshes.
pub fn deconstruct_system(
    mut commands: Commands,
    mut maps: FurnitureMaps,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &FactionMember,
        &Transform,
    )>,
    person_home_query: Query<(Entity, &HomeBed)>,
    wall_query: Query<&Wall>,
    storage_tile_map: Res<StorageTileMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    // Collect agents that just finished deconstruction.
    let mut to_complete: Vec<(Entity, (i16, i16), u32, (i32, i32))> = Vec::new();

    for (entity, mut ai, _, _, member, transform) in agent_query.iter_mut() {
        if ai.state != AiState::Working || ai.task_id != TaskKind::Deconstruct as u16 {
            continue;
        }
        ai.work_progress = ai.work_progress.saturating_add(1);
        if ai.work_progress >= TICKS_DECONSTRUCT_BED {
            ai.work_progress = 0;
            let cur_x = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_y = (transform.translation.y / TILE_SIZE).floor() as i32;
            to_complete.push((entity, ai.target_tile, member.faction_id, (cur_x, cur_y)));
        }
    }

    for (agent_entity, tile, faction_id, (cur_x, cur_y)) in to_complete {
        // Try each furniture map in turn and pick the first that holds this tile.
        let mut removed: Option<(Entity, BuildSiteKind, bool /*was_bed*/)> = None;
        if let Some(e) = maps.bed_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Bed, true));
        } else if let Some(e) = maps.campfire_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Campfire, false));
        } else if let Some(entry) = maps.door_map.0.remove(&tile) {
            removed = Some((entry.entity, BuildSiteKind::Door, false));
        } else if let Some(e) = maps.workbench_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Workbench, false));
        } else if let Some(e) = maps.loom_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Loom, false));
        } else if let Some(e) = maps.table_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Table, false));
        } else if let Some(e) = maps.chair_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Chair, false));
        } else if let Some(e) = maps.granary_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Granary, false));
        } else if let Some(e) = maps.shrine_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Shrine, false));
        } else if let Some(e) = maps.market_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Market, false));
        } else if let Some(e) = maps.barracks_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Barracks, false));
        } else if let Some(e) = maps.monument_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Monument, false));
        } else if let Some(e) = maps.wall_map.0.remove(&tile) {
            // Walls: revert chunk_map to Grass + emit TileChangedEvent so the
            // sprite refreshes. The recipe-determined refund is given via the
            // BuildSiteKind::Wall(material) path.
            let mat = wall_query
                .get(e)
                .map(|w| w.material)
                .unwrap_or(WallMaterial::Palisade);
            let surf_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32);
            chunk_map.set_tile(
                tile.0 as i32,
                tile.1 as i32,
                surf_z as i32,
                TileData {
                    kind: TileKind::Grass,
                    ..Default::default()
                },
            );
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                tx: tile.0,
                ty: tile.1,
            });
            removed = Some((e, BuildSiteKind::Wall(mat), false));
        }

        // Note: `active_upgrade` is intentionally NOT cleared here. The chief
        // needs to see it set to know the slot is awaiting a rebuild. It is
        // cleared by `construction_system` once the rebuild blueprint
        // finalises at the same tile (or after a stuck-cleanup timeout).

        let Some((target_entity, kind, was_bed)) = removed else {
            // Already gone — just idle the agent.
            if let Ok((_, mut ai, _, _, _, _)) = agent_query.get_mut(agent_entity) {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            continue;
        };

        // Furniture removal can change tile passability/speed; tell pathing
        // caches to invalidate. (The Wall arm above already emits one — a
        // duplicate event is harmless, the invalidator dedupes by chunk.)
        if !matches!(kind, BuildSiteKind::Wall(_)) {
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                tx: tile.0,
                ty: tile.1,
            });
        }

        commands.entity(target_entity).despawn_recursive();

        // For beds, clear HomeBed for the previous owner.
        if was_bed {
            for (person_entity, home_bed) in person_home_query.iter() {
                if home_bed.0 == Some(target_entity) {
                    commands.entity(person_entity).insert(HomeBed(None));
                }
            }
        }

        if let Ok((_, mut ai, mut economic_agent, mut carrier, _, _)) =
            agent_query.get_mut(agent_entity)
        {
            // Recovered materials prefer the agent's hands so they can be hauled to
            // storage; fall back to inventory; spill any remainder at the deconstructed
            // tile as a GroundItem.
            for &(good, qty) in recipe_for(kind).deconstruct_refund {
                let qty = qty as u32;
                let item = crate::economy::item::Item::new_commodity(good);
                let after_hand = carrier.try_pick_up(item, qty);
                let after_inv = if after_hand > 0 {
                    economic_agent.add_item(item, after_hand)
                } else {
                    0
                };
                if after_inv > 0 {
                    let pos = tile_to_world(tile.0 as i32, tile.1 as i32);
                    commands.spawn((
                        crate::simulation::items::GroundItem {
                            item,
                            qty: after_inv,
                        },
                        Transform::from_xyz(pos.x, pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ));
                }
            }

            let cur_tile = (cur_x as i16, cur_y as i16);
            let cur_chunk = ChunkCoord(
                cur_x.div_euclid(CHUNK_SIZE as i32),
                cur_y.div_euclid(CHUNK_SIZE as i32),
            );

            if let Some(storage_tile) =
                storage_tile_map.nearest_for_faction(faction_id, (tile.0 as i32, tile.1 as i32))
            {
                assign_task_with_routing(
                    &mut ai,
                    cur_tile,
                    cur_chunk,
                    storage_tile,
                    TaskKind::DepositResource,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
            } else {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
        }
    }
}

/// Toggles each door's `open` state based on whether any agent is currently
/// adjacent (within 1 tile). Open doors stop blocking line of sight; closed
/// doors are treated as opaque by `has_los`. Runs every 5 ticks.
///
/// TODO: faction-gate which agents can trigger a door (currently any agent).
pub fn door_proximity_system(
    clock: Res<SimClock>,
    mut door_map: ResMut<DoorMap>,
    mut door_query: Query<(&mut Door, &Transform)>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
) {
    if clock.tick % 5 != 0 {
        return;
    }
    for (tile, entry) in door_map.0.iter_mut() {
        let Ok((mut door, _xform)) = door_query.get_mut(entry.entity) else {
            continue;
        };
        let (tx, ty) = (tile.0 as i32, tile.1 as i32);
        let mut nearby = false;
        'outer: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if !spatial.get(tx + dx, ty + dy).is_empty() {
                    nearby = true;
                    break 'outer;
                }
            }
        }
        if door.open != nearby {
            door.open = nearby;
            entry.open = nearby;
        }
    }
}
